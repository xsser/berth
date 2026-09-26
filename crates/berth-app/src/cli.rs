//! CLI subcommands (integrate.md §5): `berth list`, `berth doctor`, and the
//! hidden `berth debug …` helpers used for scripted end-to-end checks.
//!
//! None of them launches `berthd`. `doctor` only reads: metadata of the
//! data directory and socket, the daemon's status, the login shell's PATH,
//! and whether `~/.claude/settings.json` / `~/.codex/config.toml` mention
//! `berth-hook`. Of those two files it prints event / key names and
//! booleans only, never their contents (they can hold credentials).

use std::collections::HashSet;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context as _, Result};
use berth_core::{
    AgentInfo, AgentKind, AgentState, ClientRole, Dims, Event, Paths, Request, ReviveMode,
    SessionId, SessionMeta, SessionStatus, StateSource, SubscribeMode, Workspace,
};
use unicode_width::UnicodeWidthStr;

use crate::client::{self, LoginEnv, SyncClient};
use crate::config::Config;
use crate::session_view::SessionView;
use crate::sidebar::format_elapsed;

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
}

fn connect(paths: &Paths) -> Result<SyncClient> {
    SyncClient::connect(paths, ClientRole::Cli).map_err(|e| {
        anyhow!("无法连接 berthd：{e:#}\n（CLI 不会自动启动 berthd；启动 berth GUI 会拉起它）")
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

fn json_mentions(v: &serde_json::Value, needle: &str) -> bool {
    match v {
        serde_json::Value::String(s) => s.contains(needle),
        serde_json::Value::Array(a) => a.iter().any(|x| json_mentions(x, needle)),
        serde_json::Value::Object(o) => o.values().any(|x| json_mentions(x, needle)),
        _ => false,
    }
}

fn toml_mentions(v: &toml::Value, needle: &str) -> bool {
    match v {
        toml::Value::String(s) => s.contains(needle),
        toml::Value::Array(a) => a.iter().any(|x| toml_mentions(x, needle)),
        toml::Value::Table(t) => t.values().any(|x| toml_mentions(x, needle)),
        _ => false,
    }
}

/// Which Claude Code hook events call `berth-hook`, and whether the
/// statusline goes through it. `Ok(None)`: the file does not exist.
pub fn claude_hooks(path: &Path) -> Result<Option<(Vec<String>, bool)>> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => bail!("无法读取：{}", e.kind()),
    };
    // serde_json's messages carry no input text; still only keep the position.
    let v: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| anyhow!("不是有效的 JSON（第 {} 行第 {} 列）", e.line(), e.column()))?;
    let mut events: Vec<String> = v
        .get("hooks")
        .and_then(serde_json::Value::as_object)
        .map(|hooks| {
            hooks
                .iter()
                .filter(|(_, entries)| json_mentions(entries, "berth-hook"))
                .map(|(event, _)| clean(event))
                .collect()
        })
        .unwrap_or_default();
    events.sort();
    let statusline = v
        .get("statusLine")
        .is_some_and(|s| json_mentions(s, "berth-hook"));
    Ok(Some((events, statusline)))
}

/// Top-level keys of `~/.codex/config.toml` whose value mentions
/// `berth-hook` (e.g. `notify`). `Ok(None)`: the file does not exist.
pub fn codex_hooks(path: &Path) -> Result<Option<Vec<String>>> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => bail!("无法读取：{}", e.kind()),
    };
    // toml's error messages quote the offending line: never show them.
    let v: toml::Table = text
        .parse()
        .map_err(|_| anyhow!("不是有效的 TOML（错误详情含文件内容，不显示）"))?;
    let mut keys: Vec<String> = v
        .iter()
        .filter(|(_, val)| toml_mentions(val, "berth-hook"))
        .map(|(k, _)| clean(k))
        .collect();
    keys.sort();
    Ok(Some(keys))
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
                        "可达：版本 {}，pid {}，已运行 {}，session {} live / {} 共",
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
            if absent {
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

    // Hooks (read-only; installing them is `berth setup-hooks`, M3).
    match input.home {
        Some(home) => {
            let settings = home.join(".claude/settings.json");
            checks.push(match claude_hooks(&settings) {
                Ok(None) => Check::new(
                    Level::Info,
                    "Claude hooks",
                    format!("{} 不存在", settings.display()),
                ),
                Ok(Some((events, statusline))) => {
                    let line = if statusline {
                        "statusline 经 berth-hook"
                    } else {
                        "statusline 未经 berth-hook"
                    };
                    if events.is_empty() {
                        Check::new(
                            Level::Info,
                            "Claude hooks",
                            format!(
                                "未安装（没有事件调用 berth-hook；{line}）；安装属于 M3 `berth setup-hooks`，需显式确认"
                            ),
                        )
                    } else {
                        Check::new(
                            Level::Ok,
                            "Claude hooks",
                            format!("berth-hook 已挂在：{}；{line}", events.join(", ")),
                        )
                    }
                }
                Err(e) => Check::new(
                    Level::Warn,
                    "Claude hooks",
                    format!("{}：{e}", settings.display()),
                ),
            });
            let codex = home.join(".codex/config.toml");
            checks.push(match codex_hooks(&codex) {
                Ok(None) => Check::new(
                    Level::Info,
                    "Codex notify",
                    format!("{} 不存在", codex.display()),
                ),
                Ok(Some(keys)) if keys.is_empty() => Check::new(
                    Level::Info,
                    "Codex notify",
                    "未指向 berth-hook；安装属于 M3 `berth setup-hooks`，需显式确认",
                ),
                Ok(Some(keys)) => Check::new(
                    Level::Ok,
                    "Codex notify",
                    format!("berth-hook 出现在键：{}", keys.join(", ")),
                ),
                Err(e) => Check::new(
                    Level::Warn,
                    "Codex notify",
                    format!("{}：{e}", codex.display()),
                ),
            });
        }
        None => checks.push(Check::new(
            Level::Warn,
            "hooks",
            "HOME 未设置，无法检查 ~/.claude 与 ~/.codex",
        )),
    }

    // Shell integration: berth does not inject OSC 133 yet (M3); report
    // whether any live session's state came from OSC 133 marks.
    checks.push(match &sessions {
        Some(ss) => {
            let live: Vec<&SessionMeta> = ss.iter().filter(|s| s.is_live()).collect();
            let marked = live
                .iter()
                .filter(|s| s.agent.source == StateSource::ShellIntegration)
                .count();
            Check::new(
                if marked > 0 { Level::Ok } else { Level::Info },
                "shell 集成",
                format!(
                    "berth 尚未自动注入 OSC 133（M3）；{} 个 live session 中 {marked} 个的状态来自 OSC 133 标记",
                    live.len()
                ),
            )
        }
        None => Check::new(
            Level::Info,
            "shell 集成",
            "berth 尚未自动注入 OSC 133（M3）；berthd 未连上，无法观察运行中的 session",
        ),
    });

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
    let mut c = connect(paths)?;
    match cmd {
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
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TestDaemon;

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

    #[test]
    fn hook_detection_reports_names_only() {
        let dir = tempfile::tempdir().unwrap();
        let settings = dir.path().join("settings.json");
        std::fs::write(
            &settings,
            r#"{
              "env": {"ANTHROPIC_API_KEY": "sk-not-a-real-key"},
              "hooks": {
                "Stop": [{"hooks": [{"type": "command", "command": "berth-hook claude"}]}],
                "PreToolUse": [{"matcher": "Bash", "hooks": [{"type": "command", "command": "other"}]}]
              },
              "statusLine": {"type": "command", "command": "berth-hook statusline -- sh s.sh"}
            }"#,
        )
        .unwrap();
        let (events, statusline) = claude_hooks(&settings).unwrap().unwrap();
        assert_eq!(events, vec!["Stop".to_string()]);
        assert!(statusline);
        assert!(claude_hooks(&dir.path().join("none.json"))
            .unwrap()
            .is_none());
        std::fs::write(&settings, "{ \"env\": \"sk-secret\" ").unwrap();
        let err = claude_hooks(&settings).unwrap_err().to_string();
        assert!(!err.contains("sk-secret"), "{err}");

        let codex = dir.path().join("config.toml");
        std::fs::write(
            &codex,
            "model = \"x\"\nnotify = [\"berth-hook\", \"codex\", \"--chain\", \"old\"]\n",
        )
        .unwrap();
        assert_eq!(codex_hooks(&codex).unwrap().unwrap(), vec!["notify"]);
        std::fs::write(&codex, "token = \"sk-secret\nbroken").unwrap();
        let err = codex_hooks(&codex).unwrap_err().to_string();
        assert!(!err.contains("sk-secret"), "{err}");
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
        assert_eq!(find("socket").level, Level::Ok, "{text}");
        assert_eq!(find("claude").level, Level::Warn);
        assert_eq!(find("codex").level, Level::Ok);
        assert_eq!(find("Claude hooks").level, Level::Info);
        assert!(find("Codex notify").detail.contains("不存在"));
        assert_eq!(find("berthd 可执行文件").level, Level::Fail);
        assert!(text.contains("shell 集成"));
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
    }
}
