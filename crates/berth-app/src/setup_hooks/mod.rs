//! `berth setup-hooks claude|codex`: show, install or undo berth's hook
//! entries in `$HOME/.claude/settings.json` / `$HOME/.codex/config.toml`.
//!
//! Nothing is written without `--yes`; the preview is a unified diff (values
//! of secret-looking keys hidden). `--yes` first copies the file to
//! `<file>.bak.berth-<UTC timestamp>`, then replaces it atomically (same
//! permissions; a symlink's target is written) — unless the file changed
//! since it was read. `--undo` restores the newest backup byte for byte when
//! the file is exactly what setup-hooks wrote; if it changed since, the
//! backup is not used (it would drop the newer changes): only berth's
//! entries are removed. Before an undo writes, the current file is kept as
//! `<file>.bak.berth-undo-<timestamp>`.

mod claude;
mod codex;
mod json_span;
mod shell;

use std::fs;
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{anyhow, bail, Context, Result};

#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Agent {
    /// Claude Code: `~/.claude/settings.json`, 18 hook events.
    Claude,
    /// Codex: `notify` in `~/.codex/config.toml`.
    Codex,
}

#[derive(clap::Args, Debug)]
pub struct Args {
    pub agent: Agent,
    /// Claude only: also route `statusLine.command` through
    /// `berth-hook statusline -- <command>` (model / context / cost).
    #[arg(long)]
    pub statusline: bool,
    /// Write the change (after a backup). Without it only the diff is shown.
    #[arg(long)]
    pub yes: bool,
    /// Undo: restore the newest `<file>.bak.berth-<ts>` byte for byte, or
    /// remove berth's entries when the file changed since (preview unless
    /// `--yes`).
    #[arg(long, conflicts_with_all = ["statusline", "hook_path"])]
    pub undo: bool,
    /// The berth-hook to call (default: `berth-hook` next to this `berth`).
    #[arg(long, value_name = "PATH")]
    pub hook_path: Option<PathBuf>,
}

/// What the command needs from the process, passed in for tests.
pub struct Env {
    pub home: PathBuf,
    /// This `berth` executable.
    pub exe: Option<PathBuf>,
    pub cwd: PathBuf,
    /// `CLAUDE_CONFIG_DIR` / `CODEX_HOME` are set (not followed: warned).
    pub claude_config_dir: bool,
    pub codex_home: bool,
    pub now: SystemTime,
}

impl Env {
    pub fn from_process() -> Result<Env> {
        let home = std::env::var_os("HOME")
            .filter(|h| !h.is_empty())
            .map(PathBuf::from)
            .ok_or_else(|| anyhow!("HOME 未设置：不知道配置文件在哪里，未做任何改动"))?;
        Ok(Env {
            home,
            exe: std::env::current_exe().ok(),
            cwd: std::env::current_dir().context("当前目录不可用")?,
            claude_config_dir: std::env::var_os("CLAUDE_CONFIG_DIR").is_some(),
            codex_home: std::env::var_os("CODEX_HOME").is_some(),
            now: SystemTime::now(),
        })
    }
}

impl Agent {
    fn file(self, home: &Path) -> PathBuf {
        match self {
            Agent::Claude => home.join(".claude/settings.json"),
            Agent::Codex => home.join(".codex/config.toml"),
        }
    }

    fn name(self) -> &'static str {
        match self {
            Agent::Claude => "claude",
            Agent::Codex => "codex",
        }
    }
}

/// Run the command; the report is returned for printing.
pub fn run(args: &Args, env: &Env) -> Result<String> {
    if args.statusline && args.agent == Agent::Codex {
        bail!("--statusline 只用于 claude（Codex 没有 statusLine）");
    }
    let file = args.agent.file(&env.home);
    let mut out = Vec::new();
    match args.agent {
        Agent::Claude if env.claude_config_dir => out.push(
            "注意：CLAUDE_CONFIG_DIR 已设置，Claude Code 读取的是该目录下的 settings.json；本命令只处理下面这个文件".to_string(),
        ),
        Agent::Codex if env.codex_home => out.push(
            "注意：CODEX_HOME 已设置，Codex 读取的是该目录下的 config.toml；本命令只处理下面这个文件".to_string(),
        ),
        _ => {}
    }
    if args.undo {
        undo(args, env, &file, &mut out)?;
    } else {
        install(args, env, &file, &mut out)?;
    }
    Ok(out.join("\n") + "\n")
}

/// The file as text; `None` when it does not exist.
fn read(file: &Path) -> Result<Option<String>> {
    match fs::read(file) {
        Ok(bytes) => String::from_utf8(bytes)
            .map(Some)
            .map_err(|_| anyhow!("{} 不是 UTF-8 文本，未做任何改动", file.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(anyhow!("无法读取 {}：{}", file.display(), e.kind())),
    }
}

fn is_executable(p: &Path) -> bool {
    p.metadata()
        .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// `--hook-path` (made absolute), else `berth-hook` next to this `berth`
/// (the path it was started by first: a symlinked install keeps a stable
/// path; then the resolved one).
fn hook_path(args: &Args, env: &Env) -> Result<String> {
    let path = match &args.hook_path {
        Some(p) => {
            let p = if p.is_absolute() {
                p.clone()
            } else {
                env.cwd.join(p)
            };
            if !is_executable(&p) {
                bail!("--hook-path {}：不是可执行文件，未做任何改动", p.display());
            }
            p
        }
        None => {
            let exe = env.exe.as_deref().ok_or_else(|| {
                anyhow!("找不到 berth 自身的路径；请用 --hook-path 指定 berth-hook")
            })?;
            let mut candidates = vec![exe.with_file_name("berth-hook")];
            if let Ok(real) = fs::canonicalize(exe) {
                candidates.push(real.with_file_name("berth-hook"));
            }
            match candidates.iter().find(|p| is_executable(p)) {
                Some(p) => p.clone(),
                None => bail!(
                    "找不到 berth-hook：{} 不是可执行文件；请用 --hook-path 指定，未做任何改动",
                    candidates[0].display()
                ),
            }
        }
    };
    path.to_str()
        .map(str::to_owned)
        .ok_or_else(|| anyhow!("berth-hook 路径不是 UTF-8：{}", path.display()))
}

fn install(args: &Args, env: &Env, file: &Path, out: &mut Vec<String>) -> Result<()> {
    let hook = hook_path(args, env)?;
    let before = read(file)?;
    let base = before.clone().unwrap_or_else(|| match args.agent {
        Agent::Claude => "{}\n".to_string(),
        Agent::Codex => String::new(),
    });
    let change = match args.agent {
        Agent::Claude => {
            claude::install(&base, &claude::Hook { path: hook.clone() }, args.statusline)
        }
        Agent::Codex => codex::install(&base, &hook),
    }
    .map_err(|e| anyhow!("{}：{e}；未做任何改动", file.display()))?;
    out.push(format!("berth setup-hooks {}", args.agent.name()));
    out.push(format!("文件：{}", file.display()));
    out.push(format!("berth-hook：{hook}"));
    out.extend(change.notes.iter().map(|n| format!("· {n}")));
    if before.as_deref() == Some(change.text.as_str()) {
        out.push("已是最新：无需改动（未写入，未备份）".into());
        return Ok(());
    }
    let backup = before
        .as_ref()
        .map(|_| backup_name(file, env.now, "bak.berth"));
    if before.is_none() {
        out.push("文件不存在：将新建（无需备份）".into());
    }
    out.push(diff(
        before.as_deref().unwrap_or(""),
        &change.text,
        file,
        before.is_none(),
    ));
    if !args.yes {
        match &backup {
            Some(b) => out.push(format!(
                "只是预览，未写入。确认后加 --yes：先备份到 {}，再写入",
                b.display()
            )),
            None => out.push("只是预览，未写入。确认后加 --yes 新建该文件".into()),
        }
        return Ok(());
    }
    if let (Some(original), Some(_)) = (&before, &backup) {
        let saved = write_backup(file, env.now, "bak.berth", original.as_bytes())?;
        out.push(format!("已备份：{}", saved.display()));
    }
    write_file(
        file,
        before.as_deref().map(str::as_bytes),
        change.text.as_bytes(),
    )?;
    out.push(format!("已写入：{}", file.display()));
    out.push(format!(
        "撤销：berth setup-hooks {} --undo（先预览）",
        args.agent.name()
    ));
    Ok(())
}

fn undo(args: &Args, env: &Env, file: &Path, out: &mut Vec<String>) -> Result<()> {
    out.push(format!("berth setup-hooks {} --undo", args.agent.name()));
    out.push(format!("文件：{}", file.display()));
    let current = read(file)?
        .ok_or_else(|| anyhow!("{} 不存在：没有可撤销的内容，未做任何改动", file.display()))?;
    // Newest first; one identical to the file is an earlier undo's result.
    let backup = backups(file)?
        .into_iter()
        .map(|p| fs::read(&p).map(|b| (p, b)))
        .collect::<std::io::Result<Vec<_>>>()
        .context("读取备份失败")?
        .into_iter()
        .find(|(_, bytes)| bytes != current.as_bytes());
    let exact = backup.as_ref().is_some_and(|(_, bytes)| {
        std::str::from_utf8(bytes)
            .is_ok_and(|b| redo(args.agent, b, &current).as_deref() == Some(current.as_str()))
    });
    let (after, how) = match &backup {
        Some((path, bytes)) if exact => (
            String::from_utf8(bytes.clone()).expect("checked above"),
            format!(
                "当前文件正是 setup-hooks 写入的结果：逐字节恢复备份 {}",
                path.display()
            ),
        ),
        _ => {
            let reference = backup
                .as_ref()
                .and_then(|(_, b)| std::str::from_utf8(b).ok());
            let change = match args.agent {
                Agent::Claude => {
                    let reference = reference.and_then(|r| claude::check(r).ok());
                    claude::uninstall(&current, reference.as_ref())
                }
                Agent::Codex => codex::uninstall(&current),
            }
            .map_err(|e| anyhow!("{}：{e}；未做任何改动", file.display()))?;
            if change.text == current {
                out.push("没有 berth 的条目：无需撤销（未写入）".into());
                return Ok(());
            }
            let why = match &backup {
                Some((path, _)) => format!(
                    "当前文件在 setup-hooks 之后又被修改过：不用备份 {} 覆盖（会丢掉这些修改），只删除 berth 的条目",
                    path.display()
                ),
                None => "没有找到备份：只删除 berth 的条目".to_string(),
            };
            out.extend(change.notes.iter().map(|n| format!("· {n}")));
            (change.text, why)
        }
    };
    out.push(how);
    out.push(diff(&current, &after, file, false));
    if !args.yes {
        out.push("只是预览，未写入。确认后加 --yes（当前文件会先另存一份）".into());
        return Ok(());
    }
    let saved = write_backup(file, env.now, "bak.berth-undo", current.as_bytes())?;
    out.push(format!("撤销前的文件已另存：{}", saved.display()));
    write_file(file, Some(current.as_bytes()), after.as_bytes())?;
    out.push(format!("已写入：{}", file.display()));
    Ok(())
}

/// What installing again on `backup` would give, with the options visible
/// in `current` (hook path, status line).
fn redo(agent: Agent, backup: &str, current: &str) -> Option<String> {
    match agent {
        Agent::Claude => {
            let hook = claude::installed_hook(current)?;
            let statusline = claude::statusline_installed(current);
            claude::install(backup, &hook, statusline)
                .ok()
                .map(|c| c.text)
        }
        Agent::Codex => {
            let hook = codex::installed_hook(current)?;
            codex::install(backup, &hook).ok().map(|c| c.text)
        }
    }
}

/// `YYYYmmddTHHMMSSZ` (UTC).
fn timestamp(t: SystemTime) -> String {
    let secs = t
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs()) as i64;
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}{month:02}{day:02}T{:02}{:02}{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// `<file>.<kind>-<ts>` (the first free name gets `-2`, `-3`, ... when
/// taken; `write_backup` makes sure).
fn backup_name(file: &Path, now: SystemTime, kind: &str) -> PathBuf {
    let mut name = file.as_os_str().to_owned();
    name.push(format!(".{kind}-{}", timestamp(now)));
    PathBuf::from(name)
}

/// Create the backup (never overwriting one), `0600`, synced.
fn write_backup(file: &Path, now: SystemTime, kind: &str, bytes: &[u8]) -> Result<PathBuf> {
    let base = backup_name(file, now, kind);
    for n in 1..1000 {
        let path = if n == 1 {
            base.clone()
        } else {
            let mut s = base.as_os_str().to_owned();
            s.push(format!("-{n}"));
            PathBuf::from(s)
        };
        let opened = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path);
        match opened {
            Ok(mut f) => {
                f.write_all(bytes)
                    .and_then(|()| f.sync_all())
                    .with_context(|| format!("写备份 {} 失败，未改动原文件", path.display()))?;
                return Ok(path);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => {
                return Err(anyhow!(
                    "无法创建备份 {}：{}；未改动原文件",
                    path.display(),
                    e.kind()
                ))
            }
        }
    }
    bail!("备份名 {} 都已被占用；未改动原文件", base.display())
}

/// `<file>.bak.berth-<ts>[-n]` backups (not undo copies), newest first.
fn backups(file: &Path) -> Result<Vec<PathBuf>> {
    let dir = file.parent().unwrap_or(Path::new("."));
    let Some(name) = file.file_name().and_then(|n| n.to_str()) else {
        return Ok(Vec::new());
    };
    let prefix = format!("{name}.bak.berth-");
    let mut found: Vec<(String, u32, PathBuf)> = Vec::new();
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(anyhow!("无法列出 {}：{}", dir.display(), e.kind())),
    };
    for entry in entries.flatten() {
        let fname = entry.file_name();
        let Some(rest) = fname.to_str().and_then(|n| n.strip_prefix(&prefix)) else {
            continue;
        };
        let (ts, n) = match rest.split_once('-') {
            Some((ts, n)) => match n.parse::<u32>() {
                Ok(n) => (ts, n),
                Err(_) => continue,
            },
            None => (rest, 1),
        };
        let is_ts = ts.len() == 16
            && ts.as_bytes()[8] == b'T'
            && ts.ends_with('Z')
            && ts[..8]
                .bytes()
                .chain(ts[9..15].bytes())
                .all(|b| b.is_ascii_digit());
        if is_ts {
            found.push((ts.to_owned(), n, entry.path()));
        }
    }
    found.sort_by(|a, b| (&b.0, b.1).cmp(&(&a.0, a.1)));
    Ok(found.into_iter().map(|(_, _, p)| p).collect())
}

/// Replace `file` with `bytes` atomically, unless it no longer holds
/// `expected` (`None`: did not exist). A symlink's target is replaced (the
/// link stays); the mode is kept (`0600` for a new file).
fn write_file(file: &Path, expected: Option<&[u8]>, bytes: &[u8]) -> Result<()> {
    let target = match expected {
        Some(_) => {
            fs::canonicalize(file).with_context(|| format!("无法解析 {}", file.display()))?
        }
        None => file.to_path_buf(),
    };
    let dir = target
        .parent()
        .ok_or_else(|| anyhow!("{} 没有上级目录", target.display()))?;
    if expected.is_none() && !dir.exists() {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .with_context(|| format!("无法创建目录 {}", dir.display()))?;
    }
    let mode = match expected {
        Some(_) => fs::metadata(&target)?.permissions().mode() & 0o7777,
        None => 0o600,
    };
    let name = target
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("file");
    let tmp = dir.join(format!(".{name}.berth-tmp-{}", std::process::id()));
    let _ = fs::remove_file(&tmp);
    let result = (|| -> Result<()> {
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)
            .with_context(|| format!("无法创建临时文件 {}", tmp.display()))?;
        f.write_all(bytes)?;
        f.set_permissions(fs::Permissions::from_mode(mode))?;
        f.sync_all()?;
        // Someone (Claude Code, an editor) may have written meanwhile.
        let now = match fs::read(&target) {
            Ok(b) => Some(b),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(anyhow!("无法重新读取 {}：{}", target.display(), e.kind())),
        };
        if now.as_deref() != expected {
            bail!(
                "{} 在预览之后被其他程序修改：未写入（备份保留），请重新运行",
                file.display()
            );
        }
        fs::rename(&tmp, &target).with_context(|| format!("无法替换 {}", target.display()))?;
        if let Ok(d) = fs::File::open(dir) {
            let _ = d.sync_all();
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// Words that mark a secret-looking key or `NAME=value` word.
const SECRET_WORDS: [&str; 8] = [
    "key",
    "token",
    "secret",
    "password",
    "passwd",
    "auth",
    "cookie",
    "credential",
];

fn secretish(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    !name.is_empty() && SECRET_WORDS.iter().any(|w| lower.contains(w))
}

fn bare(word: &str) -> &str {
    word.trim_matches(|c: char| "\"'{}[],:".contains(c))
}

/// `"***"` plus the closing punctuation of `value` (`,`, `}`, `]`).
fn masked_value(value: &str) -> String {
    let kept = value.len() - value.trim_end_matches([',', '}', ']']).len();
    format!("\"***\"{}", &value[value.len() - kept..])
}

/// A diff line with the values of secret-looking keys (`"api_key": …`,
/// `API_TOKEN = …`, inline `{"X_KEY": …}`, `TOKEN=…` inside a command)
/// replaced by `***`.
fn mask(line: &str) -> String {
    let trimmed = line.trim_start();
    // A whole `"key": value` (JSON) or `key = value` (TOML) line.
    let (key, sep) = match trimmed.strip_prefix('"') {
        Some(rest) => (rest.split_once('"').map(|(k, _)| k), ':'),
        None => (trimmed.split_once('=').map(|(k, _)| k.trim()), '='),
    };
    if key.is_some_and(secretish) {
        if let Some(at) = line.find(sep) {
            let comma = if line.trim_end().ends_with(',') {
                ","
            } else {
                ""
            };
            return format!("{} \"***\"{comma}", &line[..=at]);
        }
    }
    // Pairs inside the line.
    let words: Vec<&str> = line.split(' ').collect();
    let mut out: Vec<String> = Vec::with_capacity(words.len());
    let mut i = 0;
    while i < words.len() {
        let w = words[i];
        if let Some((name, _)) = w.split_once('=') {
            let n = bare(name);
            if secretish(n) && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                out.push(format!("{name}=***"));
                i += 1;
                continue;
            }
        }
        if secretish(bare(w)) {
            if w.ends_with(':') && i + 1 < words.len() {
                out.push(w.to_owned());
                out.push(masked_value(words[i + 1]));
                i += 2;
                continue;
            }
            if words.get(i + 1) == Some(&"=") && i + 2 < words.len() {
                out.push(w.to_owned());
                out.push("=".to_owned());
                out.push(masked_value(words[i + 2]));
                i += 3;
                continue;
            }
        }
        out.push(w.to_owned());
        i += 1;
    }
    out.join(" ")
}

/// Unified diff from `old` to `new`, secret-looking values masked.
fn diff(old: &str, new: &str, file: &Path, created: bool) -> String {
    let from = if created {
        "/dev/null".to_string()
    } else {
        format!("{}（当前）", file.display())
    };
    let mut text = format!("--- {from}\n+++ {}（之后）\n", file.display());
    let d = similar::TextDiff::from_lines(old, new);
    let mut masked = false;
    for hunk in d.unified_diff().context_radius(3).iter_hunks() {
        text.push_str(&format!("{}\n", hunk.header()));
        for change in hunk.iter_changes() {
            let sign = match change.tag() {
                similar::ChangeTag::Delete => '-',
                similar::ChangeTag::Insert => '+',
                similar::ChangeTag::Equal => ' ',
            };
            let line = change.value().trim_end_matches(['\n', '\r']);
            let shown = mask(line);
            masked |= shown != line;
            text.push(sign);
            text.push_str(&shown);
            text.push('\n');
            if change.missing_newline() {
                text.push_str("\\ 文件末尾没有换行\n");
            }
        }
    }
    if masked {
        text.push_str("（疑似凭据的值已隐藏为 ***，文件里不变）\n");
    }
    text.trim_end().to_owned()
}

#[cfg(test)]
mod tests;
