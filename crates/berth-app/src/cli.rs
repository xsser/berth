//! CLI subcommands (integrate.md §5): `berth list`, `berth doctor`, and the
//! hidden `berth debug …` helpers used for scripted end-to-end checks.
//!
//! None of them launches `berthd`, except `berth debug restart-daemon`
//! (stop the running one, start this build's). `doctor` only reads: metadata of the
//! data directory and socket, the daemon's status, the login shell's PATH,
//! which hook events of `~/.claude/settings.json` and whether the `notify`
//! of `~/.codex/config.toml` go through `berth-hook` (judged as `berth
//! setup-hooks` judges them), and the shell integration setting and files.
//! Of the two agent files it prints event names, berth-hook paths and
//! booleans only, never their values (they can hold credentials).

use std::collections::HashSet;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context as _, Result};
use berth_core::{
    AgentInfo, AgentKind, AgentState, ClientRole, Dims, Event, EventEntry, Paths, Request,
    ReviveMode, SessionId, SessionMeta, SessionStatus, StateSource, SubscribeMode, Workspace,
    PROTOCOL_VERSION,
};
use unicode_width::UnicodeWidthStr;

use crate::client::{self, LoginEnv, SyncClient};
use crate::config::Config;
use crate::session_view::SessionView;
use crate::setup_hooks::{self, Agent};
use crate::sidebar::format_elapsed;
use crate::timefmt::local_time;

const TIMEOUT: Duration = Duration::from_secs(5);
/// Largest `Input` the debug sender puts in one request.
const SEND_CHUNK: usize = 60 * 1024;
/// `FetchLines` page for `debug dump --history` (daemon limit: 5000).
const DUMP_PAGE: u32 = 5000;

/// Hidden helpers for scripted checks (`berth debug …`).
#[derive(clap::Subcommand, Debug)]
pub enum DebugCmd {
    /// Daemon status: version, pid, uptime, session counts.
    Status,
    /// Create a session in the workspace rooted at DIR (created if missing);
    /// prints the session id.
    NewSession {
        /// Working directory and workspace root (default: current directory).
        #[arg(long, value_name = "DIR")]
        dir: Option<PathBuf>,
        #[arg(long)]
        title: Option<String>,
        /// Initial PTY size.
        #[arg(long, value_name = "COLSxROWS", value_parser = crate::parse_cells, default_value = "120x40")]
        size: (u16, u16),
        /// Program and arguments, after `--` (default: the login shell).
        #[arg(last = true, value_name = "CMD")]
        command: Vec<String>,
    },
    /// Send text to a session. Escapes: \n \r \t \e \0 \\ \xNN \u{…} \cX.
    Send { session: String, text: String },
    /// Wait until the session's screen contains TEXT (exit 1 on timeout).
    Wait {
        session: String,
        text: String,
        #[arg(long, value_name = "SECS", default_value_t = 20.0)]
        timeout: f64,
    },
    /// Print the session's screen (history first with --history).
    Dump {
        session: String,
        #[arg(long)]
        history: bool,
    },
    /// Attach at COLSxROWS for SECS seconds: the PTY takes the smallest
    /// attached size, so this resizes it while it runs.
    Hold {
        session: String,
        #[arg(value_parser = crate::parse_cells, value_name = "COLSxROWS")]
        size: (u16, u16),
        #[arg(long, value_name = "SECS", default_value_t = 5.0)]
        secs: f64,
    },
    /// Revive a dormant / restored session (a shell, or the agent's resume
    /// command with --agent).
    Revive {
        session: String,
        #[arg(long)]
        agent: bool,
    },
    /// Kill a live session's process (it becomes dormant).
    Kill { session: String },
    /// The session's agent events (state changes, hooks, OSC 133 marks),
    /// oldest first, in local time.
    Events {
        session: String,
        /// How many of the newest events (the daemon returns at most 200).
        #[arg(long, value_name = "N", default_value_t = 50)]
        limit: u32,
    },
    /// What `revive --agent` would run for the session, and in which
    /// directory (nothing is started).
    Resume { session: String },
    /// What the GUI's 「重启 berthd」 does: stop the running berthd (in its
    /// own protocol version when that differs), wait until it is gone, and
    /// start this build's. Its sessions come back dormant / restored.
    RestartDaemon,
}

/// How to get past a berthd of another protocol version.
fn restart_advice() -> String {
    format!(
        "在 berth 窗口点「重启 berthd」，或运行 `berth debug restart-daemon`；{}。",
        client::RESTART_EFFECT
    )
}

fn connect(paths: &Paths) -> Result<SyncClient> {
    SyncClient::connect(paths, ClientRole::Cli).map_err(|e| match client::incompatible(&e) {
        Some(i) => anyhow!("{i}，版本不一致。{}", restart_advice()),
        None => {
            anyhow!("无法连接 berthd：{e:#}\n（CLI 不会自动启动 berthd；启动 berth GUI 会拉起它）")
        }
    })
}

/// A short name for an event (debug output of `Screen` would be huge).
fn event_kind(e: &Event) -> &'static str {
    match e {
        Event::Hello { .. } => "Hello",
        Event::Incompatible { .. } => "Incompatible",
        Event::Ok => "Ok",
        Event::Error { .. } => "Error",
        Event::Workspaces(_) => "Workspaces",
        Event::WorkspaceUpdated(_) => "WorkspaceUpdated",
        Event::WorkspaceRemoved(_) => "WorkspaceRemoved",
        Event::Sessions(_) => "Sessions",
        Event::SessionUpdated(_) => "SessionUpdated",
        Event::SessionRemoved(_) => "SessionRemoved",
        Event::Screen(_) => "Screen",
        Event::Lines { .. } => "Lines",
        Event::Preview { .. } => "Preview",
        Event::Title { .. } => "Title",
        Event::Cwd { .. } => "Cwd",
        Event::Bell { .. } => "Bell",
        Event::Notify { .. } => "Notify",
        Event::AgentChanged { .. } => "AgentChanged",
        Event::Exited { .. } => "Exited",
        Event::Status(_) => "Status",
        Event::Events { .. } => "Events",
        Event::ResumeCommand { .. } => "ResumeCommand",
    }
}

fn unexpected(what: &str, e: Event) -> anyhow::Error {
    match e {
        Event::Error { message } => anyhow!("{what} 失败：{message}"),
        other => anyhow!("{what} 的意外回答：{}", event_kind(&other)),
    }
}

fn list_all(c: &mut SyncClient) -> Result<(Vec<Workspace>, Vec<SessionMeta>)> {
    let ws = match c.request(Request::ListWorkspaces, TIMEOUT)? {
        Event::Workspaces(v) => v,
        other => return Err(unexpected("ListWorkspaces", other)),
    };
    let ss = match c.request(Request::ListSessions, TIMEOUT)? {
        Event::Sessions(v) => v,
        other => return Err(unexpected("ListSessions", other)),
    };
    Ok((ws, ss))
}

/// Replace control characters (titles come from programs via OSC).
fn clean(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() { '?' } else { c })
        .collect()
}

fn tilde(path: &Path, home: Option<&Path>) -> String {
    if let Some(rest) = home.and_then(|h| path.strip_prefix(h).ok()) {
        return if rest.as_os_str().is_empty() {
            "~".into()
        } else {
            format!("~/{}", clean(&rest.display().to_string()))
        };
    }
    clean(&path.display().to_string())
}

fn kind_name(kind: &AgentKind) -> String {
    match kind {
        AgentKind::Shell => "shell".into(),
        AgentKind::Claude => "claude".into(),
        AgentKind::Codex => "codex".into(),
        AgentKind::Other(name) => clean(name),
    }
}

fn status_text(s: &SessionMeta) -> String {
    let mut t = match &s.status {
        SessionStatus::Live => "live".to_string(),
        SessionStatus::Dormant {
            exit_code: Some(c), ..
        } => format!("dormant(exit {c})"),
        SessionStatus::Dormant { .. } => "dormant".into(),
        SessionStatus::Restored => "restored".into(),
    };
    if s.unread {
        t.push_str(" •未读");
    }
    t
}

fn agent_text(a: &AgentInfo, now_ms: i64) -> String {
    let state = match &a.state {
        AgentState::ToolRunning { tool } => format!("tool_running:{}", clean(tool)),
        AgentState::WaitingPermission { tool: Some(t) } => {
            format!("waiting_permission:{}", clean(t))
        }
        AgentState::Exited { code: Some(c) } => format!("exited:{c}"),
        s => s.name().to_string(),
    };
    let source = match a.source {
        StateSource::Hook => "hook",
        StateSource::ShellIntegration => "osc133",
        StateSource::Heuristic => "推断",
    };
    let since = if a.since_ms > 0 {
        format_elapsed(now_ms.saturating_sub(a.since_ms))
    } else {
        "-".into()
    };
    format!("{} {state} · {since} · {source}", kind_name(&a.kind))
}

/// Columns padded by display width (CJK titles are two cells wide).
fn table(rows: &[Vec<String>]) -> String {
    let n = rows.iter().map(Vec::len).max().unwrap_or(0);
    let widths: Vec<usize> = (0..n)
        .map(|i| {
            rows.iter()
                .filter_map(|r| r.get(i))
                .map(|c| c.width())
                .max()
                .unwrap_or(0)
        })
        .collect();
    let mut out = String::new();
    for r in rows {
        let mut line = String::new();
        for (i, cell) in r.iter().enumerate() {
            line.push_str(cell);
            if i + 1 < r.len() {
                line.extend(std::iter::repeat_n(' ', widths[i] - cell.width() + 2));
            }
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

/// `berth list` output.
pub fn render_list(
    workspaces: &[Workspace],
    sessions: &[SessionMeta],
    now_ms: i64,
    home: Option<&Path>,
) -> String {
    let mut ws: Vec<&Workspace> = workspaces.iter().collect();
    ws.sort_by_key(|w| (w.order, w.created_at_ms));
    let mut ss: Vec<&SessionMeta> = sessions.iter().collect();
    ss.sort_by_key(|s| (s.order, s.created_at_ms));
    let known: HashSet<_> = ws.iter().map(|w| w.id).collect();
    let row = |ws_name: String, s: &SessionMeta| {
        vec![
            ws_name,
            format!("{} {}", s.id.short(), clean(s.title())),
            status_text(s),
            agent_text(&s.agent, now_ms),
            tilde(&s.cwd, home),
        ]
    };
    let mut rows = vec![["WORKSPACE", "SESSION", "STATUS", "AGENT", "CWD"]
        .map(String::from)
        .to_vec()];
    for w in &ws {
        for s in ss.iter().filter(|s| s.workspace == w.id) {
            rows.push(row(clean(&w.name), s));
        }
    }
    for s in ss.iter().filter(|s| !known.contains(&s.workspace)) {
        rows.push(row("?".into(), s));
    }
    let live = sessions.iter().filter(|s| s.is_live()).count();
    let mut out = table(&rows);
    let _ = writeln!(
        out,
        "{} 个 workspace，{} 个 session（{live} 个 live）",
        workspaces.len(),
        sessions.len()
    );
    out
}

pub fn list(paths: &Paths) -> Result<()> {
    let mut c = connect(paths)?;
    let (ws, ss) = list_all(&mut c)?;
    let home = std::env::var_os("HOME").map(PathBuf::from);
    print!(
        "{}",
        render_list(&ws, &ss, berth_core::now_ms(), home.as_deref())
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// doctor
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    Ok,
    Info,
    Warn,
    Fail,
}

#[derive(Clone, Debug)]
pub struct Check {
    pub level: Level,
    pub label: &'static str,
    pub detail: String,
}

impl Check {
    fn new(level: Level, label: &'static str, detail: impl Into<String>) -> Check {
        Check {
            level,
            label,
            detail: detail.into(),
        }
    }
}

pub fn render_checks(checks: &[Check]) -> String {
    let mut out = String::from("berth doctor（只读检查，不写任何文件）\n");
    for c in checks {
        let mark = match c.level {
            Level::Ok => "[ok]",
            Level::Info => "[--]",
            Level::Warn => "[!!]",
            Level::Fail => "[xx]",
        };
        let _ = writeln!(out, "{mark} {}：{}", c.label, c.detail);
    }
    out
}

fn mode_string(mode: u32, kind: char) -> String {
    let mut s = String::with_capacity(10);
    s.push(kind);
    for shift in [6, 3, 0] {
        let bits = (mode >> shift) & 7;
        s.push(if bits & 4 != 0 { 'r' } else { '-' });
        s.push(if bits & 2 != 0 { 'w' } else { '-' });
        s.push(if bits & 1 != 0 { 'x' } else { '-' });
    }
    s
}

fn current_uid() -> u32 {
    // SAFETY: getuid(2) has no preconditions and cannot fail.
    unsafe { libc::getuid() }
}

/// Owner and permission check of the data directory / socket.
fn permission_check(label: &'static str, path: &Path, want_socket: bool, uid: u32) -> Check {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    let meta = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let why = if want_socket {
                "不存在（berthd 未运行）"
            } else {
                "不存在（berthd 首次启动时创建）"
            };
            return Check::new(Level::Info, label, format!("{} {why}", path.display()));
        }
        Err(e) => {
            return Check::new(
                Level::Fail,
                label,
                format!("{} 无法读取元数据：{e}", path.display()),
            )
        }
    };
    let ft = meta.file_type();
    let (kind, right_type) = if ft.is_socket() {
        ('s', want_socket)
    } else if ft.is_dir() {
        ('d', !want_socket)
    } else if ft.is_symlink() {
        ('l', false)
    } else {
        ('-', false)
    };
    let mode = meta.mode() & 0o7777;
    let owner = meta.uid();
    let mut detail = format!(
        "{} {} 属主 uid {owner}",
        path.display(),
        mode_string(mode, kind)
    );
    let level = if !right_type {
        detail.push_str(if want_socket {
            "，不是 socket"
        } else {
            "，不是目录"
        });
        Level::Fail
    } else if owner != uid {
        let _ = write!(detail, "，不是当前用户（uid {uid}）");
        Level::Fail
    } else if mode & 0o077 != 0 {
        detail.push_str("，同组或其他用户有权限");
        Level::Warn
    } else {
        detail.push_str("（仅当前用户可访问）");
        Level::Ok
    };
    Check::new(level, label, detail)
}

/// How `command -v NAME` answered in the login shell. Alias definitions
/// and function bodies are never printed.
pub fn describe_resolution(name: &str, found: &Option<String>) -> Check {
    let label = if name == "claude" { "claude" } else { "codex" };
    match found.as_deref() {
        None => Check::new(
            Level::Warn,
            label,
            format!("登录 shell 的 PATH 下找不到 {name}（ResumeAgent 会失败）"),
        ),
        Some(p) if LoginEnv::is_exec_path(found) => {
            Check::new(Level::Ok, label, format!("{p}（berthd 可按 argv 启动）"))
        }
        Some(p) if p.starts_with("alias ") => Check::new(
            Level::Warn,
            label,
            "是 shell 别名（定义不显示）；berthd 以 argv 启动时用不到别名",
        ),
        Some(p) if Path::new(p).is_absolute() => {
            Check::new(Level::Warn, label, format!("{p} 不是可执行文件"))
        }
        Some(_) => Check::new(
            Level::Warn,
            label,
            "是 shell 函数或内建命令（内容不显示）；berthd 以 argv 启动时用不到",
        ),
    }
}

const SETUP_CLAUDE_HINT: &str = "`berth setup-hooks claude` 预览安装（加 --yes 才写入）";
const SETUP_CODEX_HINT: &str = "`berth setup-hooks codex` 预览安装（加 --yes 才写入）";

/// The berth-hook paths of an installation, each marked when it is not an
/// executable file (or is a bare name looked up in PATH); `true` when one
/// cannot run or they differ (setup-hooks points them all at one path).
fn hook_paths(paths: &[String]) -> (String, bool) {
    use std::os::unix::fs::PermissionsExt as _;
    let mut bad = paths.len() > 1;
    let shown: Vec<String> = paths
        .iter()
        .map(|p| {
            let path = Path::new(p);
            if !path.is_absolute() {
                return format!("{}（按 PATH 查找）", clean(p));
            }
            let runnable = path
                .metadata()
                .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0);
            if runnable {
                clean(p)
            } else {
                bad = true;
                format!("{}（不存在或不可执行）", clean(p))
            }
        })
        .collect();
    let mut text = shown.join(", ");
    if paths.len() > 1 {
        text.push_str("（路径不一致；重新运行 berth setup-hooks 会统一）");
    }
    (text, bad)
}

/// `~/.claude/settings.json`: how many of the events setup-hooks installs
/// call berth-hook, which are missing, the paths, the status line.
fn claude_hooks_check(home: &Path) -> Check {
    const LABEL: &str = "Claude hooks";
    let file = setup_hooks::config_file(Agent::Claude, home);
    let installed = match setup_hooks::installed(Agent::Claude, home) {
        Ok(Some(i)) => i,
        Ok(None) => {
            return Check::new(
                Level::Info,
                LABEL,
                format!("{} 不存在；{SETUP_CLAUDE_HINT}", file.display()),
            )
        }
        Err(e) => return Check::new(Level::Warn, LABEL, format!("{e:#}")),
    };
    let line = if installed.statusline {
        "statusline 经 berth-hook"
    } else {
        "statusline 未经 berth-hook"
    };
    if installed.events.is_empty() {
        return Check::new(
            Level::Info,
            LABEL,
            format!("未安装（没有事件调用 berth-hook claude；{line}）；{SETUP_CLAUDE_HINT}"),
        );
    }
    let mut level = Level::Ok;
    let mut detail = format!(
        "berth-hook 已挂在 {}/{} 个事件",
        installed.events.len(),
        setup_hooks::CLAUDE_EVENTS.len()
    );
    if !installed.missing.is_empty() {
        level = Level::Warn;
        let _ = write!(
            detail,
            "；缺 {}（重新运行 `berth setup-hooks claude` 可补齐）",
            installed.missing.join(", ")
        );
    }
    let (paths, bad) = hook_paths(&installed.hooks);
    if bad {
        level = Level::Warn;
    }
    let _ = write!(detail, "；berth-hook：{paths}；{line}");
    Check::new(level, LABEL, detail)
}

/// `~/.codex/config.toml`: whether `notify` goes through berth-hook, and
/// whether the original program is chained.
fn codex_notify_check(home: &Path) -> Check {
    const LABEL: &str = "Codex notify";
    let file = setup_hooks::config_file(Agent::Codex, home);
    let installed = match setup_hooks::installed(Agent::Codex, home) {
        Ok(Some(i)) => i,
        Ok(None) => {
            return Check::new(
                Level::Info,
                LABEL,
                format!("{} 不存在；{SETUP_CODEX_HINT}", file.display()),
            )
        }
        Err(e) => return Check::new(Level::Warn, LABEL, format!("{e:#}")),
    };
    if installed.events.is_empty() {
        return Check::new(
            Level::Info,
            LABEL,
            format!("notify 未经 berth-hook；{SETUP_CODEX_HINT}"),
        );
    }
    let (paths, bad) = hook_paths(&installed.hooks);
    let chain = if installed.chained {
        "，转发后执行原 notify 程序"
    } else {
        ""
    };
    Check::new(
        if bad { Level::Warn } else { Level::Ok },
        LABEL,
        format!("notify 经 berth-hook{chain}；berth-hook：{paths}"),
    )
}

/// Where berthd writes the zsh integration files (`berth_daemon::
/// shell_integration::zsh_dir`; a test keeps the two equal).
pub fn zsh_integration_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("shell-integration").join("zsh")
}

/// The files berthd writes into [`zsh_integration_dir`].
const ZSH_INTEGRATION_FILES: [&str; 2] = [".zshenv", "berth-integration.zsh"];

/// `[terminal] shell_integration` as berthd reads this key (`auto` | `zsh`
/// on, `none` or anything else off; a missing or unreadable file means the
/// default `auto`): whether the zsh integration is on, the setting as
/// shown, and whether it needs attention.
fn shell_integration_setting(config_file: &Path) -> (bool, String, bool) {
    let text = match std::fs::read_to_string(config_file) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return (true, "auto（默认）".into(), false)
        }
        Err(e) => {
            return (
                true,
                format!("配置文件无法读取（{}），berthd 使用默认值 auto", e.kind()),
                true,
            )
        }
    };
    // toml's error messages quote the file: never show them.
    let Ok(table) = text.parse::<toml::Table>() else {
        return (true, "配置文件无效，berthd 使用默认值 auto".into(), true);
    };
    match table
        .get("terminal")
        .and_then(|t| t.get("shell_integration"))
    {
        None => (true, "auto（默认）".into(), false),
        Some(toml::Value::String(v)) => match v.as_str() {
            "auto" | "zsh" => (true, v.clone(), false),
            "none" => (false, "none（已关闭）".into(), false),
            other => (
                false,
                format!(
                    "{:?} 不认识：berthd 按关闭处理（可选 auto | zsh | none）",
                    clean(other)
                ),
                true,
            ),
        },
        Some(_) => (
            true,
            "不是字符串：berthd 忽略整个配置文件、使用默认值 auto".into(),
            true,
        ),
    }
}

/// The zsh integration: setting, login shell, files, and how many live
/// sessions have their state from OSC 133 marks.
fn shell_integration_check(input: &DoctorInput, sessions: Option<&[SessionMeta]>) -> Check {
    let (on, setting, attention) = shell_integration_setting(&input.paths.config_file);
    let zsh = input.login.shell.file_name().is_some_and(|n| n == "zsh");
    let mut detail = format!("[terminal] shell_integration = {setting}");
    if on {
        if !zsh {
            let _ = write!(
                detail,
                "；登录 shell 是 {}：目前只有 zsh 有集成",
                input.login.shell.display()
            );
        }
        let dir = zsh_integration_dir(&input.paths.data_dir);
        if ZSH_INTEGRATION_FILES.iter().all(|f| dir.join(f).is_file()) {
            let _ = write!(detail, "；zsh 垫片已写入 {}", dir.display());
        } else {
            let _ = write!(
                detail,
                "；zsh 垫片尚未写入（berthd 启动 zsh session 时写入并校验 {}）",
                dir.display()
            );
        }
    }
    match sessions {
        Some(ss) => {
            let live: Vec<&SessionMeta> = ss.iter().filter(|s| s.is_live()).collect();
            let marked = live
                .iter()
                .filter(|s| s.agent.source == StateSource::ShellIntegration)
                .count();
            let _ = write!(
                detail,
                "；{} 个 live session 中 {marked} 个的状态来自 OSC 133 标记",
                live.len()
            );
        }
        None => detail.push_str("；berthd 未连上，无法观察运行中的 session"),
    }
    let level = if attention {
        Level::Warn
    } else if on && zsh {
        Level::Ok
    } else {
        Level::Info
    };
    Check::new(level, "shell 集成", detail)
}

/// Everything `berth doctor` checks, with the environment passed in.
pub struct DoctorInput<'a> {
    pub paths: &'a Paths,
    pub home: Option<&'a Path>,
    pub uid: u32,
    pub login: &'a LoginEnv,
    pub berthd: Option<PathBuf>,
}

pub fn doctor_checks(input: &DoctorInput) -> Vec<Check> {
    let paths = input.paths;
    let mut checks = vec![permission_check(
        "数据目录",
        &paths.data_dir,
        false,
        input.uid,
    )];
    if let Some(parent) = paths
        .socket
        .parent()
        .filter(|p| *p != paths.data_dir.as_path())
    {
        checks.push(permission_check(
            "socket 所在目录",
            parent,
            false,
            input.uid,
        ));
    }
    checks.push(permission_check("socket", &paths.socket, true, input.uid));

    // Daemon.
    let mut sessions = None;
    match SyncClient::connect(paths, ClientRole::Cli) {
        Ok(mut c) => {
            match c.request(Request::DaemonStatus, TIMEOUT) {
                Ok(Event::Status(s)) => checks.push(Check::new(
                    Level::Ok,
                    "berthd",
                    format!(
                        "可达：版本 {}，协议 v{PROTOCOL_VERSION}（与客户端一致），pid {}，\
                         已运行 {}，session {} live / {} 共",
                        s.version,
                        s.pid,
                        format_elapsed(s.uptime_ms),
                        s.sessions_live,
                        s.sessions_total
                    ),
                )),
                Ok(other) => checks.push(Check::new(
                    Level::Fail,
                    "berthd",
                    unexpected("DaemonStatus", other).to_string(),
                )),
                Err(e) => checks.push(Check::new(Level::Fail, "berthd", format!("{e:#}"))),
            }
            if let Ok((_, ss)) = list_all(&mut c) {
                sessions = Some(ss);
            }
        }
        Err(e) => {
            let absent = e
                .chain()
                .filter_map(|c| c.downcast_ref::<std::io::Error>())
                .any(client::daemon_absent);
            if let Some(i) = client::incompatible(&e) {
                checks.push(Check::new(
                    Level::Fail,
                    "berthd",
                    format!("可达，但 {i}，版本不一致。{}", restart_advice()),
                ));
            } else if absent {
                checks.push(Check::new(
                    Level::Info,
                    "berthd",
                    "未运行（启动 berth GUI 时自动拉起）",
                ));
            } else {
                checks.push(Check::new(Level::Fail, "berthd", format!("不可达：{e:#}")));
            }
        }
    }
    checks.push(match &input.berthd {
        Some(p) => Check::new(Level::Ok, "berthd 可执行文件", p.display().to_string()),
        None => Check::new(
            Level::Fail,
            "berthd 可执行文件",
            "在 berth 同目录、PATH 与登录 shell PATH 中都找不到",
        ),
    });

    // Login shell environment (what berthd gets when berth launches it).
    let login = input.login;
    match (&login.path, &login.error) {
        (Some(path), _) => checks.push(Check::new(
            Level::Ok,
            "登录 shell PATH",
            format!(
                "{} -lc 取得 {} 项：{path}",
                login.shell.display(),
                std::env::split_paths(path).count()
            ),
        )),
        (None, err) => checks.push(Check::new(
            Level::Warn,
            "登录 shell PATH",
            format!(
                "{} -lc 失败（{}）；berthd 将继承 berth 自己的 PATH",
                login.shell.display(),
                err.as_deref().unwrap_or("无输出")
            ),
        )),
    }
    if login.path.is_some() {
        checks.push(describe_resolution("claude", &login.claude));
        checks.push(describe_resolution("codex", &login.codex));
    }

    // Hooks (read-only; installing them is `berth setup-hooks`).
    match input.home {
        Some(home) => {
            checks.push(claude_hooks_check(home));
            checks.push(codex_notify_check(home));
        }
        None => checks.push(Check::new(
            Level::Warn,
            "hooks",
            "HOME 未设置，无法检查 ~/.claude 与 ~/.codex",
        )),
    }

    checks.push(shell_integration_check(input, sessions.as_deref()));

    // Config file.
    let config = &paths.config_file;
    checks.push(if config.exists() {
        match Config::load_from(config).1 {
            None => Check::new(Level::Ok, "配置文件", config.display().to_string()),
            Some(warning) => Check::new(Level::Warn, "配置文件", warning),
        }
    } else {
        Check::new(
            Level::Info,
            "配置文件",
            format!("{} 不存在（使用默认值）", config.display()),
        )
    });
    checks
}

pub fn doctor(paths: &Paths) -> Result<()> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let login = client::probe_login_env(&client::user_shell(), client::LOGIN_PROBE_TIMEOUT);
    let exe = std::env::current_exe().ok();
    let berthd = client::find_berthd(
        exe.as_deref().and_then(Path::parent),
        std::env::var_os("PATH").as_deref(),
        login.path.as_deref().map(std::ffi::OsStr::new),
    );
    let checks = doctor_checks(&DoctorInput {
        paths,
        home: home.as_deref(),
        uid: current_uid(),
        login: &login,
        berthd,
    });
    print!("{}", render_checks(&checks));
    Ok(())
}

// ---------------------------------------------------------------------------
// debug helpers
// ---------------------------------------------------------------------------

/// Decode `\n \r \t \e \0 \\ \xNN \u{…} \cX` escapes.
pub fn unescape(s: &str) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len());
    let mut chars = s.chars();
    let mut buf = [0u8; 4];
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            continue;
        }
        match chars.next() {
            Some('n') => out.push(b'\n'),
            Some('r') => out.push(b'\r'),
            Some('t') => out.push(b'\t'),
            Some('e') => out.push(0x1b),
            Some('0') => out.push(0),
            Some('\\') => out.push(b'\\'),
            Some('x') => {
                let hex: String = chars.by_ref().take(2).collect();
                let b = u8::from_str_radix(&hex, 16)
                    .ok()
                    .filter(|_| hex.len() == 2)
                    .ok_or_else(|| anyhow!("\\x 后需要两位十六进制：{hex:?}"))?;
                out.push(b);
            }
            Some('u') => {
                if chars.next() != Some('{') {
                    bail!("\\u 的写法是 \\u{{…}}");
                }
                let hex: String = chars.by_ref().take_while(|c| *c != '}').collect();
                let ch = u32::from_str_radix(&hex, 16)
                    .ok()
                    .and_then(char::from_u32)
                    .ok_or_else(|| anyhow!("无效的 \\u{{{hex}}}"))?;
                out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
            }
            Some('c') => {
                let ch = chars.next().ok_or_else(|| anyhow!("\\c 后缺少字符"))?;
                let b =
                    crate::input::ctrl_byte(ch).ok_or_else(|| anyhow!("\\c{ch} 不是控制字符"))?;
                out.push(b);
            }
            Some(other) => bail!("未知转义 \\{other}"),
            None => bail!("末尾是单独的反斜杠"),
        }
    }
    Ok(out)
}

/// A session id or unique prefix (with or without dashes).
pub fn resolve_session(sessions: &[SessionMeta], want: &str) -> Result<SessionMeta> {
    let want = want.to_ascii_lowercase();
    let hits: Vec<&SessionMeta> = sessions
        .iter()
        .filter(|s| {
            let full = s.id.to_string();
            full.starts_with(&want) || full.replace('-', "").starts_with(&want)
        })
        .collect();
    match hits.as_slice() {
        [one] => Ok((*one).clone()),
        [] => bail!("没有 id 以 {want} 开头的 session"),
        _ => bail!("{want} 匹配多个 session，请写更长的前缀"),
    }
}

fn session_arg(c: &mut SyncClient, want: &str) -> Result<SessionMeta> {
    let (_, ss) = list_all(c)?;
    resolve_session(&ss, want)
}

/// Round trip after fire-and-forget requests: the daemon handles one
/// connection's requests in order, so errors for them arrive first.
fn barrier(c: &mut SyncClient, ids: &[u32]) -> Result<()> {
    match c.request(Request::DaemonStatus, TIMEOUT)? {
        Event::Status(_) => {}
        other => return Err(unexpected("DaemonStatus", other)),
    }
    if let Some(msg) = c.wait_for(Instant::now(), |m| {
        m.reply_to.is_some_and(|id| ids.contains(&id))
    })? {
        return Err(unexpected("Input", msg.event));
    }
    Ok(())
}

/// Subscribe to full screen updates (no effect on the PTY size) and feed
/// them to a view until `done` says so or `deadline` passes.
fn watch_screen(
    c: &mut SyncClient,
    sid: SessionId,
    deadline: Instant,
    mut done: impl FnMut(&mut SessionView) -> bool,
) -> Result<(SessionView, bool)> {
    let id = c.send(Request::Subscribe {
        session: sid,
        mode: SubscribeMode::Full,
    })?;
    let mut view = SessionView::new(sid);
    let first = c
        .wait_for(deadline, |m| m.reply_to == Some(id))?
        .ok_or_else(|| anyhow!("berthd 没有发来 {sid} 的屏幕"))?;
    match first.event {
        Event::Screen(u) => {
            view.apply_screen(&u, true);
        }
        other => return Err(unexpected("Subscribe", other)),
    }
    if done(&mut view) {
        return Ok((view, true));
    }
    loop {
        let msg = c.wait_for(deadline, |m| {
            matches!(&m.event, Event::Screen(u) if u.session == sid)
                || matches!(&m.event, Event::Error { .. }) && m.reply_to.is_none()
        })?;
        match msg.map(|m| m.event) {
            Some(Event::Screen(u)) => {
                view.apply_screen(&u, false);
                if done(&mut view) {
                    return Ok((view, true));
                }
            }
            Some(other) => return Err(unexpected("屏幕订阅", other)),
            None => return Ok((view, false)),
        }
    }
}

fn screen_text(view: &mut SessionView) -> String {
    let mut out = String::new();
    for line in &view.screen().lines {
        out.push_str(&line.text_trimmed());
        out.push('\n');
    }
    out
}

fn dims_of((cols, rows): (u16, u16)) -> Dims {
    Dims { cols, rows }
}

pub fn debug(paths: &Paths, cmd: DebugCmd) -> Result<()> {
    let mut c = match cmd {
        // Before connecting: the running berthd may speak another protocol.
        DebugCmd::RestartDaemon => {
            print!("{}", restart_daemon(paths, &mut client::launch_berthd)?);
            return Ok(());
        }
        _ => connect(paths)?,
    };
    match cmd {
        DebugCmd::RestartDaemon => {} // Done above, without a connection.
        DebugCmd::Status => match c.request(Request::DaemonStatus, TIMEOUT)? {
            Event::Status(s) => {
                println!(
                    "version {} (hello {}) pid {} uptime_ms {} live {} total {}",
                    s.version,
                    c.daemon_version(),
                    s.pid,
                    s.uptime_ms,
                    s.sessions_live,
                    s.sessions_total
                );
            }
            other => return Err(unexpected("DaemonStatus", other)),
        },
        DebugCmd::NewSession {
            dir,
            title,
            size,
            command,
        } => {
            let dir = match dir {
                Some(d) => d,
                None => std::env::current_dir().context("current directory")?,
            };
            let dir = dir
                .canonicalize()
                .with_context(|| format!("{} 不可用", dir.display()))?;
            let (workspaces, _) = list_all(&mut c)?;
            let ws = match workspaces.iter().find(|w| w.root == dir) {
                Some(w) => w.id,
                None => match c.request(
                    Request::CreateWorkspace {
                        name: crate::controller::workspace_name(&dir),
                        root: dir.clone(),
                    },
                    TIMEOUT,
                )? {
                    Event::WorkspaceUpdated(w) => w.id,
                    other => return Err(unexpected("CreateWorkspace", other)),
                },
            };
            let req = Request::CreateSession {
                workspace: ws,
                cwd: Some(dir),
                command: (!command.is_empty()).then_some(command),
                title,
                dims: dims_of(size),
            };
            match c.request(req, TIMEOUT)? {
                Event::SessionUpdated(meta) => println!("{}", meta.id),
                other => return Err(unexpected("CreateSession", other)),
            }
        }
        DebugCmd::Send { session, text } => {
            let meta = session_arg(&mut c, &session)?;
            let bytes = unescape(&text)?;
            let mut ids = Vec::new();
            for chunk in bytes.chunks(SEND_CHUNK) {
                ids.push(c.send(Request::Input {
                    session: meta.id,
                    data: chunk.to_vec(),
                })?);
                barrier(&mut c, &ids)?;
            }
        }
        DebugCmd::Wait {
            session,
            text,
            timeout,
        } => {
            let meta = session_arg(&mut c, &session)?;
            let deadline = Instant::now() + Duration::from_secs_f64(timeout.clamp(0.1, 600.0));
            let (mut view, found) = watch_screen(&mut c, meta.id, deadline, |v| {
                screen_text(v).contains(&text)
            })?;
            if !found {
                eprint!("{}", screen_text(&mut view));
                bail!("{timeout} s 内屏幕上没有出现 {text:?}（上面是最后的屏幕）");
            }
        }
        DebugCmd::Dump { session, history } => {
            let meta = session_arg(&mut c, &session)?;
            let (mut view, _) = watch_screen(&mut c, meta.id, Instant::now() + TIMEOUT, |_| true)?;
            if history {
                let total = view.history_len();
                println!("# history {total} lines");
                let mut start = 0u64;
                while start < total {
                    let count = (total - start).min(u64::from(DUMP_PAGE)) as u32;
                    let req = Request::FetchLines {
                        session: meta.id,
                        start,
                        count,
                    };
                    match c.request(req, TIMEOUT)? {
                        Event::Lines { lines, .. } => {
                            if lines.is_empty() {
                                break;
                            }
                            for l in &lines {
                                println!("{}", l.text_trimmed());
                            }
                            start += lines.len() as u64;
                        }
                        other => return Err(unexpected("FetchLines", other)),
                    }
                }
            }
            let dims = view.dims();
            let modes = view.modes();
            let cursor = view.screen().cursor;
            println!(
                "# screen {}x{} cursor row {} col {} visible {} alt_screen {} history_len {} status {}",
                dims.cols,
                dims.rows,
                cursor.row,
                cursor.col,
                cursor.visible,
                modes.contains(berth_core::TermModes::ALT_SCREEN),
                view.history_len(),
                status_text(&meta)
            );
            print!("{}", screen_text(&mut view));
        }
        DebugCmd::Hold {
            session,
            size,
            secs,
        } => {
            let meta = session_arg(&mut c, &session)?;
            let id = c.send(Request::Attach {
                session: meta.id,
                dims: dims_of(size),
            })?;
            let first = c
                .wait_for(Instant::now() + TIMEOUT, |m| m.reply_to == Some(id))?
                .ok_or_else(|| anyhow!("Attach 没有回答"))?;
            match first.event {
                Event::Screen(u) => println!(
                    "attached at {}x{}; screen now {}x{}",
                    size.0, size.1, u.dims.cols, u.dims.rows
                ),
                other => return Err(unexpected("Attach", other)),
            }
            // Keep reading (and dropping) updates until the time is up.
            let until = Instant::now() + Duration::from_secs_f64(secs.clamp(0.1, 3600.0));
            c.wait_for(until, |_| false)?;
            match c.request(Request::Detach { session: meta.id }, TIMEOUT)? {
                Event::Ok => println!("detached"),
                other => return Err(unexpected("Detach", other)),
            }
        }
        DebugCmd::Revive { session, agent } => {
            let meta = session_arg(&mut c, &session)?;
            let mode = if agent {
                ReviveMode::ResumeAgent
            } else {
                ReviveMode::Shell
            };
            match c.request(
                Request::Revive {
                    session: meta.id,
                    mode,
                },
                TIMEOUT,
            )? {
                Event::SessionUpdated(m) => println!("{} {}", m.id, status_text(&m)),
                other => return Err(unexpected("Revive", other)),
            }
        }
        DebugCmd::Kill { session } => {
            let meta = session_arg(&mut c, &session)?;
            match c.request(Request::Kill { session: meta.id }, TIMEOUT)? {
                Event::Ok => println!("killed {}", meta.id),
                other => return Err(unexpected("Kill", other)),
            }
        }
        DebugCmd::Events { session, limit } => {
            let meta = session_arg(&mut c, &session)?;
            let request = Request::ListEvents {
                session: meta.id,
                limit,
            };
            match c.request(request, TIMEOUT)? {
                Event::Events { events, .. } => print!("{}", render_events(&events)),
                other => return Err(unexpected("ListEvents", other)),
            }
        }
        DebugCmd::Resume { session } => {
            let meta = session_arg(&mut c, &session)?;
            match c.request(Request::ResumeCommand { session: meta.id }, TIMEOUT)? {
                Event::ResumeCommand { cwd, command, .. } => {
                    println!("cwd: {}", cwd.display());
                    match command {
                        Ok(argv) => println!("command: {}", setup_hooks::command_line(&argv)),
                        Err(e) => bail!("不能恢复 agent：{e}"),
                    }
                }
                other => return Err(unexpected("ResumeCommand", other)),
            }
        }
    }
    Ok(())
}

/// `berth debug restart-daemon`: stop the running berthd, whatever its
/// protocol version, then connect to a new one started by `launch`.
fn restart_daemon(paths: &Paths, launch: &mut dyn FnMut() -> Result<()>) -> Result<String> {
    let mut out = String::new();
    match client::stop_daemon(paths, client::STOP_WAIT)? {
        Some(stopped) => {
            let pid = stopped
                .pid
                .map(|p| format!("，pid {p}"))
                .unwrap_or_default();
            writeln!(
                out,
                "已停止 berthd {}（协议 v{}{pid}）",
                stopped.version, stopped.protocol
            )?;
        }
        None => writeln!(out, "berthd 未在运行")?,
    }
    let (stream, launched) = client::connect_or_spawn(paths, launch)?;
    let c = SyncClient::start(stream, ClientRole::Cli)?;
    let how = if launched {
        "已启动"
    } else {
        "已连接到另一方启动的"
    };
    writeln!(
        out,
        "{how} berthd {}（协议 v{PROTOCOL_VERSION}）",
        c.daemon_version()
    )?;
    Ok(out)
}

/// `berth debug events`: the daemon's list (newest first) as a table,
/// oldest first.
pub fn render_events(events: &[EventEntry]) -> String {
    if events.is_empty() {
        return "（没有事件）\n".into();
    }
    let mut rows = vec![vec![
        "时间".to_string(),
        "事件".to_string(),
        "状态".to_string(),
        "详情".to_string(),
    ]];
    rows.extend(events.iter().rev().map(|e| {
        vec![
            local_time(e.at_ms),
            clean(&e.kind),
            clean(&e.state),
            e.detail.as_deref().map(clean).unwrap_or_default(),
        ]
    }));
    table(&rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{fake_old_daemon, refuse_hellos, TestDaemon};

    fn meta(title: &str, ws: berth_core::WorkspaceId, status: SessionStatus) -> SessionMeta {
        SessionMeta {
            workspace: ws,
            title_user: Some(title.into()),
            cwd: PathBuf::from("/home/u/proj/src"),
            status,
            ..SessionMeta::default()
        }
    }

    #[test]
    fn list_table_aligns_wide_titles_and_groups_by_workspace() {
        let w = Workspace {
            id: berth_core::WorkspaceId::new(),
            name: "proj".into(),
            root: PathBuf::from("/home/u/proj"),
            color: None,
            order: 0,
            created_at_ms: 0,
        };
        let mut a = meta("中文标题", w.id, SessionStatus::Live);
        a.agent = AgentInfo {
            kind: AgentKind::Claude,
            state: AgentState::WaitingPermission {
                tool: Some("Bash".into()),
            },
            since_ms: 1_000,
            source: StateSource::Hook,
            ..AgentInfo::default()
        };
        a.unread = true;
        let mut b = meta("ascii\x1b[31m", w.id, SessionStatus::Restored);
        b.order = 1;
        let orphan = meta(
            "lost",
            berth_core::WorkspaceId::new(),
            SessionStatus::Dormant {
                exit_code: Some(1),
                at_ms: 0,
            },
        );
        let out = render_list(
            &[w],
            &[b.clone(), a.clone(), orphan],
            61_000,
            Some(Path::new("/home/u")),
        );
        let lines: Vec<&str> = out.lines().collect();
        assert!(lines[0].starts_with("WORKSPACE"));
        assert!(lines[1].contains("中文标题") && lines[1].contains("live •未读"));
        assert!(
            lines[1].contains("claude waiting_permission:Bash · 1m00s · hook"),
            "{out}"
        );
        assert!(lines[2].contains("ascii?[31m"), "control chars replaced");
        assert!(lines[3].starts_with('?') && lines[3].contains("dormant(exit 1)"));
        assert!(lines[1].ends_with("~/proj/src"));
        // STATUS starts at the same display column on every row.
        let col = |l: &str| {
            let status = ["STATUS", "live", "restored", "dormant"]
                .iter()
                .filter_map(|s| l.find(s))
                .min()
                .expect("status column");
            l[..status].width()
        };
        assert_eq!(col(lines[0]), col(lines[1]));
        assert_eq!(col(lines[0]), col(lines[2]));
        assert_eq!(lines[4], "1 个 workspace，3 个 session（1 个 live）");
    }

    #[test]
    fn escapes_decode() {
        assert_eq!(unescape("ls -la\\r").unwrap(), b"ls -la\r");
        assert_eq!(
            unescape("\\e[A\\x7f\\cC\\0").unwrap(),
            b"\x1b[A\x7f\x03\x00"
        );
        assert_eq!(
            unescape("只回复：好\\u{1F600}").unwrap(),
            "只回复：好😀".as_bytes()
        );
        assert!(unescape("\\q").is_err());
        assert!(unescape("\\x1").is_err());
        assert!(unescape("tail\\").is_err());
    }

    #[test]
    fn command_resolution_never_prints_alias_or_function_bodies() {
        let alias = describe_resolution(
            "claude",
            &Some("alias claude='SECRET_TOKEN=abc claude-real'".into()),
        );
        assert_eq!(alias.level, Level::Warn);
        assert!(!alias.detail.contains("SECRET"));
        let function = describe_resolution("codex", &Some("codex".into()));
        assert!(function.detail.contains("函数"));
        let missing = describe_resolution("codex", &None);
        assert!(missing.detail.contains("找不到"));
        let sh = describe_resolution("claude", &Some("/bin/sh".into()));
        assert_eq!(sh.level, Level::Ok);
    }

    /// A fake `berth-hook` (executable) in `dir`.
    fn fake_hook(dir: &Path) -> String {
        use std::os::unix::fs::PermissionsExt as _;
        let hook = dir.join("berth-hook");
        std::fs::write(&hook, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
        hook.to_str().unwrap().to_owned()
    }

    fn setup(home: &Path, agent: Agent, hook: &str) {
        let args = setup_hooks::Args {
            agent,
            statusline: false,
            yes: true,
            undo: false,
            hook_path: Some(PathBuf::from(hook)),
        };
        let env = setup_hooks::Env {
            home: home.to_path_buf(),
            exe: None,
            cwd: home.to_path_buf(),
            claude_config_dir: false,
            codex_home: false,
            now: std::time::SystemTime::now(),
        };
        setup_hooks::run(&args, &env).unwrap();
    }

    #[test]
    fn claude_hooks_are_counted_as_setup_hooks_installs_them() {
        let home = tempfile::tempdir().unwrap();
        let home = home.path();
        let check = claude_hooks_check(home);
        assert_eq!(check.level, Level::Info);
        assert!(check.detail.contains("不存在") && check.detail.contains("setup-hooks claude"));

        std::fs::create_dir_all(home.join(".claude")).unwrap();
        let settings = home.join(".claude/settings.json");
        std::fs::write(
            &settings,
            r#"{
              "env": {"ANTHROPIC_API_KEY": "sk-not-a-real-key"},
              "hooks": {
                "Stop": [{"hooks": [{"type": "command", "command": "/gone/berth-hook claude"}]}],
                "PreToolUse": [{"matcher": "Bash", "hooks": [{"type": "command", "command": "echo berth-hook"}]}]
              },
              "statusLine": {"type": "command", "command": "/gone/berth-hook statusline -- sh s.sh"}
            }"#,
        )
        .unwrap();
        let check = claude_hooks_check(home);
        assert_eq!(check.level, Level::Warn, "{}", check.detail);
        // "echo berth-hook" is not a berth hook: only Stop counts.
        assert!(check.detail.contains("1/18"), "{}", check.detail);
        assert!(check.detail.contains("缺 SessionStart"), "{}", check.detail);
        let installed = setup_hooks::installed(Agent::Claude, home)
            .unwrap()
            .unwrap();
        assert_eq!(installed.events, ["Stop"]);
        assert_eq!(installed.missing.len(), 17);
        assert_eq!(installed.hooks, ["/gone/berth-hook"]);
        assert!(installed.statusline);
        assert!(
            check
                .detail
                .contains("/gone/berth-hook（不存在或不可执行）"),
            "{}",
            check.detail
        );
        assert!(check.detail.contains("statusline 经 berth-hook"));
        assert!(!check.detail.contains("sk-not"));

        // After `berth setup-hooks claude --yes`: all events, one path.
        let bin = tempfile::tempdir().unwrap();
        let hook = fake_hook(bin.path());
        std::fs::write(&settings, "{\"env\": {\"TOKEN\": \"sk-secret\"}}\n").unwrap();
        setup(home, Agent::Claude, &hook);
        let check = claude_hooks_check(home);
        assert_eq!(check.level, Level::Ok, "{}", check.detail);
        assert!(check.detail.contains("18/18"), "{}", check.detail);
        assert!(check.detail.contains(&hook) && !check.detail.contains("缺"));
        assert!(check.detail.contains("statusline 未经 berth-hook"));

        std::fs::write(&settings, "{ \"env\": \"sk-secret\" ").unwrap();
        let check = claude_hooks_check(home);
        assert_eq!(check.level, Level::Warn);
        assert!(!check.detail.contains("sk-secret"), "{}", check.detail);
        assert!(check.detail.contains("第 1 行"), "{}", check.detail);
    }

    #[test]
    fn codex_notify_reports_the_chain_and_never_values() {
        let home = tempfile::tempdir().unwrap();
        let home = home.path();
        assert!(codex_notify_check(home).detail.contains("不存在"));
        std::fs::create_dir_all(home.join(".codex")).unwrap();
        let config = home.join(".codex/config.toml");
        std::fs::write(&config, "model = \"x\"\nnotify = [\"notify-send\"]\n").unwrap();
        let check = codex_notify_check(home);
        assert_eq!(check.level, Level::Info);
        assert!(check.detail.contains("未经 berth-hook"), "{}", check.detail);

        let bin = tempfile::tempdir().unwrap();
        let hook = fake_hook(bin.path());
        setup(home, Agent::Codex, &hook);
        let check = codex_notify_check(home);
        assert_eq!(check.level, Level::Ok, "{}", check.detail);
        assert!(
            check.detail.contains("转发后执行原 notify 程序"),
            "{}",
            check.detail
        );
        assert!(!check.detail.contains("notify-send"));

        std::fs::write(&config, "notify = [\"berth-hook\", \"codex\"]\n").unwrap();
        let check = codex_notify_check(home);
        assert!(
            check.detail.contains("berth-hook（按 PATH 查找）"),
            "{}",
            check.detail
        );
        assert!(!check.detail.contains("转发"));

        std::fs::write(&config, "token = \"sk-secret\nbroken").unwrap();
        let check = codex_notify_check(home);
        assert_eq!(check.level, Level::Warn);
        assert!(!check.detail.contains("sk-secret"), "{}", check.detail);
    }

    #[test]
    fn shell_integration_setting_follows_berthd() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("config.toml");
        let setting = |text: Option<&str>| {
            match text {
                Some(t) => std::fs::write(&file, t).unwrap(),
                None => {
                    let _ = std::fs::remove_file(&file);
                }
            }
            shell_integration_setting(&file)
        };
        assert_eq!(setting(None), (true, "auto（默认）".into(), false));
        assert!(setting(Some("[font]\nsize = 12\n")).0);
        assert_eq!(
            setting(Some("[terminal]\nshell_integration = \"zsh\"\n")),
            (true, "zsh".into(), false)
        );
        assert_eq!(
            setting(Some("[terminal]\nshell_integration = \"none\"\n")),
            (false, "none（已关闭）".into(), false)
        );
        let (on, text, attention) = setting(Some("[terminal]\nshell_integration = \"bash\"\n"));
        assert!(!on && attention && text.contains("\"bash\""), "{text}");
        let (on, _, attention) = setting(Some("[terminal]\nshell_integration = false\n"));
        assert!(on && attention);
        let (on, text, attention) = setting(Some("token = \"sk-secret\n["));
        assert!(on && attention && !text.contains("sk-secret"));
    }

    #[test]
    fn zsh_integration_files_are_found_where_berthd_writes_them() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path();
        assert_eq!(
            zsh_integration_dir(data),
            berth_daemon::shell_integration::zsh_dir(data)
        );
        let login = LoginEnv {
            shell: PathBuf::from("/bin/zsh"),
            path: None,
            claude: None,
            codex: None,
            error: None,
        };
        let paths = Paths::in_dir(data);
        let input = DoctorInput {
            paths: &paths,
            home: None,
            uid: current_uid(),
            login: &login,
            berthd: None,
        };
        let check = shell_integration_check(&input, None);
        assert!(check.detail.contains("尚未写入"), "{}", check.detail);
        berth_daemon::shell_integration::install_zsh(data).unwrap();
        let check = shell_integration_check(&input, None);
        assert_eq!(check.level, Level::Ok, "{}", check.detail);
        assert!(check.detail.contains("已写入"), "{}", check.detail);
        assert!(check.detail.contains("berthd 未连上"));
        let bash = LoginEnv {
            shell: PathBuf::from("/bin/bash"),
            path: None,
            claude: None,
            codex: None,
            error: None,
        };
        let check = shell_integration_check(
            &DoctorInput {
                login: &bash,
                ..input
            },
            Some(&[]),
        );
        assert_eq!(check.level, Level::Info);
        assert!(check.detail.contains("只有 zsh"), "{}", check.detail);
        assert!(
            check.detail.contains("0 个 live session"),
            "{}",
            check.detail
        );
    }

    #[test]
    fn events_are_listed_oldest_first_in_local_time() {
        let e = |at_ms, kind: &str, state: &str, detail: Option<&str>| EventEntry {
            at_ms,
            kind: kind.into(),
            state: state.into(),
            detail: detail.map(Into::into),
        };
        // As the daemon sends them: newest first.
        let text = render_events(&[
            e(1_790_000_002_345, "hook:Stop", "done", None),
            e(
                1_790_000_001_000,
                "hook:PreToolUse",
                "thinking",
                Some("Write\u{1b}[2J"),
            ),
        ]);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3, "{text}");
        assert!(lines[0].starts_with("时间"));
        assert!(lines[1].contains("hook:PreToolUse") && lines[1].contains("Write?[2J"));
        assert!(lines[2].contains("hook:Stop") && lines[2].contains(".345"));
        assert_eq!(render_events(&[]), "（没有事件）\n");
    }

    #[test]
    fn doctor_reports_a_running_daemon_and_private_permissions() {
        let d = TestDaemon::start();
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join(".claude")).unwrap();
        std::fs::write(home.path().join(".claude/settings.json"), "{}").unwrap();
        let login = LoginEnv {
            shell: PathBuf::from("/bin/zsh"),
            path: Some("/usr/bin:/bin".into()),
            claude: None,
            codex: Some("/bin/sh".into()),
            error: None,
        };
        let checks = doctor_checks(&DoctorInput {
            paths: &d.paths,
            home: Some(home.path()),
            uid: current_uid(),
            login: &login,
            berthd: None,
        });
        let text = render_checks(&checks);
        let find = |label: &str| {
            checks
                .iter()
                .find(|c| c.label == label)
                .unwrap_or_else(|| panic!("{label} missing:\n{text}"))
        };
        assert_eq!(find("berthd").level, Level::Ok, "{text}");
        let same = format!("协议 v{PROTOCOL_VERSION}（与客户端一致）");
        assert!(find("berthd").detail.contains(&same), "{text}");
        assert_eq!(find("socket").level, Level::Ok, "{text}");
        assert_eq!(find("claude").level, Level::Warn);
        assert_eq!(find("codex").level, Level::Ok);
        assert_eq!(find("Claude hooks").level, Level::Info);
        assert!(find("Codex notify").detail.contains("不存在"));
        assert_eq!(find("berthd 可执行文件").level, Level::Fail);
        assert!(text.contains("shell 集成"));
    }

    #[test]
    fn a_daemon_of_another_protocol_is_named_with_both_versions_and_the_restart() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::in_dir(dir.path());
        let old = PROTOCOL_VERSION - 1;
        let listener = std::os::unix::net::UnixListener::bind(&paths.socket).unwrap();
        let daemon = std::thread::spawn(move || refuse_hellos(&listener, old, 3));
        let want = format!("berthd 协议 v{old}，本客户端协议 v{PROTOCOL_VERSION}，版本不一致");

        let err = format!("{:#}", connect(&paths).err().expect("refused"));
        assert!(err.contains(&want), "{err}");
        assert!(err.contains("berth debug restart-daemon"), "{err}");
        assert!(err.contains(client::RESTART_EFFECT), "{err}");
        assert_eq!(format!("{:#}", list(&paths).unwrap_err()), err);

        let login = LoginEnv {
            shell: PathBuf::from("/bin/zsh"),
            path: Some("/usr/bin:/bin".into()),
            claude: None,
            codex: None,
            error: None,
        };
        let checks = doctor_checks(&DoctorInput {
            paths: &paths,
            home: None,
            uid: current_uid(),
            login: &login,
            berthd: None,
        });
        let check = checks.iter().find(|c| c.label == "berthd").unwrap();
        assert_eq!(check.level, Level::Fail);
        assert!(check.detail.contains(&want), "{}", check.detail);
        assert!(check.detail.contains("「重启 berthd」"), "{}", check.detail);
        assert!(
            check.detail.contains(client::RESTART_EFFECT),
            "{}",
            check.detail
        );

        for hello in daemon.join().unwrap() {
            assert!(
                matches!(hello, Request::Hello { role: ClientRole::Cli, protocol, .. } if protocol == PROTOCOL_VERSION),
                "{hello:?}"
            );
        }
    }

    #[test]
    fn restart_daemon_stops_an_old_berthd_before_starting_this_one() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::in_dir(dir.path());
        let old = PROTOCOL_VERSION - 1;
        let fake = fake_old_daemon(&paths, old);
        let mut started = None;
        let mut launch = || {
            assert!(
                !paths.socket.exists(),
                "launched while the old berthd listened"
            );
            started = Some(TestDaemon::start_at(paths.clone()));
            Ok(())
        };
        let out = restart_daemon(&paths, &mut launch).unwrap();
        let first = format!("已停止 berthd 0.0.1（协议 v{old}）\n");
        assert!(out.starts_with(&first), "{out}");
        let restarted = format!(
            "已启动 berthd {}（协议 v{PROTOCOL_VERSION}）\n",
            env!("CARGO_PKG_VERSION")
        );
        assert!(out.ends_with(&restarted), "{out}");
        assert_eq!(fake.join().unwrap().last(), Some(&Request::Shutdown));
        let mut c = connect(&paths).expect("this build's berthd answers");
        assert!(matches!(
            c.request(Request::DaemonStatus, TIMEOUT).unwrap(),
            Event::Status(_)
        ));
        drop(c);

        // Nothing running: only the start.
        drop(started.take());
        let mut launch = || {
            started = Some(TestDaemon::start_at(paths.clone()));
            Ok(())
        };
        let out = restart_daemon(&paths, &mut launch).unwrap();
        assert!(out.starts_with("berthd 未在运行\n已启动 berthd "), "{out}");
        assert!(started.is_some());
    }

    #[test]
    fn debug_helpers_drive_a_real_daemon() {
        let d = TestDaemon::start();
        let work = tempfile::tempdir().unwrap();
        let mut c = connect(&d.paths).unwrap();
        // Same steps as `debug new-session`.
        let root = work.path().canonicalize().unwrap();
        let ws = match c
            .request(
                Request::CreateWorkspace {
                    name: "w".into(),
                    root: root.clone(),
                },
                TIMEOUT,
            )
            .unwrap()
        {
            Event::WorkspaceUpdated(w) => w.id,
            other => panic!("{other:?}"),
        };
        let sid = match c
            .request(
                Request::CreateSession {
                    workspace: ws,
                    cwd: Some(root),
                    command: Some(vec![
                        "/bin/sh".into(),
                        "-c".into(),
                        "printf 'hello\\n'; exec cat".into(),
                    ]),
                    title: None,
                    dims: Dims { cols: 40, rows: 6 },
                },
                TIMEOUT,
            )
            .unwrap()
        {
            Event::SessionUpdated(m) => m.id,
            other => panic!("{other:?}"),
        };
        let prefix = &sid.to_string()[..8];
        debug(
            &d.paths,
            DebugCmd::Send {
                session: prefix.into(),
                text: "abc\\n".into(),
            },
        )
        .unwrap();
        debug(
            &d.paths,
            DebugCmd::Wait {
                session: prefix.into(),
                text: "abc".into(),
                timeout: 10.0,
            },
        )
        .unwrap();
        let err = debug(
            &d.paths,
            DebugCmd::Wait {
                session: prefix.into(),
                text: "never".into(),
                timeout: 0.3,
            },
        )
        .unwrap_err();
        assert!(err.to_string().contains("没有出现"));
        let (_, ss) = list_all(&mut c).unwrap();
        let text = render_list(&[], &ss, berth_core::now_ms(), None);
        assert!(text.contains(&sid.short()), "{text}");
        debug(
            &d.paths,
            DebugCmd::Kill {
                session: prefix.into(),
            },
        )
        .unwrap();
        assert!(resolve_session(&ss, "zzzz").is_err());
        debug(
            &d.paths,
            DebugCmd::Events {
                session: prefix.into(),
                limit: 10,
            },
        )
        .unwrap();
        // A plain shell has no agent session to resume.
        let err = debug(
            &d.paths,
            DebugCmd::Resume {
                session: prefix.into(),
            },
        )
        .unwrap_err();
        assert!(err.to_string().contains("不能恢复 agent"), "{err:#}");
    }
}
