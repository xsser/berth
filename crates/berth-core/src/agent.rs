//! Agent kinds, states, and the typed hook payloads that drive the state
//! machine. Everything here is `postcard`-serializable (no `serde_json::Value`).

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::ids::SessionId;

#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AgentKind {
    #[default]
    Shell,
    Claude,
    Codex,
    Other(String),
}

impl AgentKind {
    pub fn is_agent(&self) -> bool {
        !matches!(self, AgentKind::Shell)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum AgentState {
    /// Prompt is showing / nothing running.
    #[default]
    Idle,
    /// Model is generating (or a plain shell command is running).
    Thinking,
    ToolRunning {
        tool: String,
    },
    WaitingPermission {
        tool: Option<String>,
    },
    WaitingInput,
    /// Turn finished; cleared to `Idle` when the user interacts again.
    Done,
    Error {
        message: String,
    },
    Compacting,
    /// Child process exited.
    Exited {
        code: Option<i32>,
    },
}

impl AgentState {
    /// States that should raise a notification / badge when unfocused.
    pub fn needs_attention(&self) -> bool {
        matches!(
            self,
            AgentState::WaitingPermission { .. }
                | AgentState::WaitingInput
                | AgentState::Done
                | AgentState::Error { .. }
        )
    }

    pub fn is_busy(&self) -> bool {
        matches!(
            self,
            AgentState::Thinking | AgentState::ToolRunning { .. } | AgentState::Compacting
        )
    }

    /// Stable machine-readable name (config, logs, events table).
    pub fn name(&self) -> &'static str {
        match self {
            AgentState::Idle => "idle",
            AgentState::Thinking => "thinking",
            AgentState::ToolRunning { .. } => "tool_running",
            AgentState::WaitingPermission { .. } => "waiting_permission",
            AgentState::WaitingInput => "waiting_input",
            AgentState::Done => "done",
            AgentState::Error { .. } => "error",
            AgentState::Compacting => "compacting",
            AgentState::Exited { .. } => "exited",
        }
    }
}

/// Where the current state came from. Priority: Hook > ShellIntegration >
/// Heuristic; a lower-priority source never overrides `WaitingPermission`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum StateSource {
    #[default]
    Heuristic,
    ShellIntegration,
    Hook,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct AgentInfo {
    pub kind: AgentKind,
    /// Claude Code session uuid / Codex thread id, used for resume.
    pub external_id: Option<String>,
    pub transcript_path: Option<PathBuf>,
    pub model: Option<String>,
    pub context_pct: Option<f32>,
    pub cost_usd: Option<f64>,
    pub state: AgentState,
    /// When the current state was entered (unix ms).
    pub since_ms: i64,
    pub source: StateSource,
    /// 0.0..=1.0; hooks report 1.0, heuristics less.
    pub confidence: f32,
}

// ---------------------------------------------------------------------------
// Hook payloads (produced by `berth-hook`, consumed by the daemon)
// ---------------------------------------------------------------------------

/// Claude Code hook events. Field names follow the official hook JSON; the
/// hook CLI maps unknown events to `Other`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClaudeHookEvent {
    SessionStart {
        source: Option<String>,
    },
    UserPromptSubmit,
    PreToolUse {
        tool_name: String,
    },
    PostToolUse {
        tool_name: String,
    },
    Notification {
        notification_type: Option<String>,
        message: String,
    },
    Stop {
        stop_hook_active: bool,
    },
    SubagentStop,
    PreCompact {
        trigger: Option<String>,
    },
    SessionEnd {
        reason: Option<String>,
    },
    Other {
        hook_event_name: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaudeHook {
    pub session_id: String,
    pub cwd: Option<PathBuf>,
    pub transcript_path: Option<PathBuf>,
    pub permission_mode: Option<String>,
    pub event: ClaudeHookEvent,
}

/// Codex CLI `notify` payload (e.g. `agent-turn-complete`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodexNotify {
    pub event_type: String,
    pub thread_id: Option<String>,
    pub cwd: Option<PathBuf>,
    pub last_message: Option<String>,
}

/// Subset of the Claude Code statusline JSON forwarded by the optional tee.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct StatuslineUpdate {
    pub session_id: String,
    pub model: Option<String>,
    pub context_pct: Option<f32>,
    pub cost_usd: Option<f64>,
    pub project_dir: Option<PathBuf>,
    pub cwd: Option<PathBuf>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum AgentSignal {
    Claude(ClaudeHook),
    Codex(CodexNotify),
    Statusline(StatuslineUpdate),
}

/// One message from `berth-hook` to the daemon.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HookEnvelope {
    /// From `BERTH_SESSION_ID` in the hook's environment; `None` means the
    /// daemon must fall back to cwd / process-tree matching.
    pub berth_session: Option<SessionId>,
    /// Pid of the hook process (its ancestors identify the agent process).
    pub pid: u32,
    pub sent_at_ms: i64,
    pub signal: AgentSignal,
}
