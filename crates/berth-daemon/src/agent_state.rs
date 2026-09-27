//! Agent state machine (DESIGN §9).
//!
//! Inputs are `Signal`s from three tiers: hooks (Claude Code / Codex), shell
//! integration (OSC 133) and heuristics (foreground process, output activity,
//! silence). Rules (see also the `StateSource` contract in berth-core):
//! - a hook-sourced state changes only through hooks, OSC 133, PTY exit,
//!   user input and an agent → non-agent foreground switch; the output
//!   heuristics (activity, silence) never touch it, whatever its age;
//! - the output heuristics never leave the "sticky" states either
//!   (`WaitingPermission`, `WaitingInput`, `Done`, `Error`, `Exited`):
//!   activity only moves `Idle → Thinking`, silence only moves busy states
//!   `→ Idle`;
//! - a foreground agent name sets the kind unless a hook spoke within 30 s;
//! - the agent's `SessionEnd`, OSC 133 marks while an agent owns the session
//!   (the shell is back at its prompt) and a foreground switch from an agent
//!   name to another name are "agent left": kind back to `Shell`, ids and
//!   transcript kept — the shell lives on, so this is never `Exited`;
//! - `Exited` is the PTY's exit and terminal: no signal leaves it (late
//!   hooks are recorded as `hook:late`), only a revive (`reset`) does;
//! - the nine core Claude hook events, `Notification` types with a clear
//!   meaning and `PermissionRequest` / `PermissionDenied` /
//!   `PostToolUseFailure` / `StopFailure` / `PostCompact` / `Elicitation` /
//!   `ElicitationResult` move the state; the other hook events (`Other`:
//!   `SubagentStart`, `CwdChanged` — whose cwd the manager applies —,
//!   unknown names) are recorded but never change it;
//! - compaction ends (`PostCompact` or `SessionStart{compact}`, whichever
//!   comes first — their order is not documented) in `Thinking` when it
//!   began during a turn (auto-compact, the turn goes on) and in `Idle`
//!   otherwise (`/compact` at the prompt: no turn follows, and a hook
//!   `Thinking` would stay until the next prompt);
//! - every applied signal is reported as `Applied` so the manager can write
//!   an `EventRecord` (kind + short detail, never prompt text).

use berth_core::{
    AgentInfo, AgentKind, AgentState, ClaudeHook, ClaudeHookEvent, CodexNotify, StateSource,
    StatuslineUpdate,
};
use berth_vt::{OscEvent, PromptMark};

/// How long a hook keeps the foreground-process heuristic from changing the
/// agent kind.
pub const HOOK_PRIORITY_MS: i64 = 30_000;
/// Output silence after which a busy agent is considered idle.
pub const SILENCE_IDLE_SECS: u64 = 3;
pub const HOOK_CONFIDENCE: f32 = 1.0;
pub const OSC_CONFIDENCE: f32 = 0.9;
pub const HEURISTIC_CONFIDENCE: f32 = 0.5;

#[derive(Clone, Debug, PartialEq)]
pub enum Signal {
    Hook(ClaudeHook),
    Codex(CodexNotify),
    Osc(OscEvent),
    /// Base name of the PTY's foreground process.
    ForegroundProcess(String),
    OutputActivity,
    /// No output for `secs`; `cursor_at_line_start` = cursor in column 0.
    Silence {
        secs: u64,
        cursor_at_line_start: bool,
    },
    /// The user typed into the session.
    UserInput,
    /// Child exited (PTY EOF).
    Exited(Option<i32>),
}

/// A transition worth recording.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Applied {
    /// `EventRecord.kind`, e.g. `hook:PreToolUse`, `osc:133D`.
    pub kind: String,
    pub detail: Option<String>,
    /// False when only metadata (kind, ids) changed.
    pub state_changed: bool,
}

/// What a `Notification` hook means for the state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NotificationClass {
    /// → `WaitingPermission`
    Permission,
    /// → `WaitingInput`
    Input,
    /// → `Thinking` (an MCP elicitation was answered)
    Working,
    /// → `Done`
    Completed,
    /// State unchanged; only the event is recorded.
    Informational,
}

#[derive(Clone, Debug)]
pub struct AgentMachine {
    info: AgentInfo,
    last_hook_ms: Option<i64>,
    subagent_stops: u32,
    subagent_starts: u32,
    /// Previous `ForegroundProcess` name, to recognise an agent leaving.
    last_fg: Option<String>,
    /// The current compaction began while the agent was busy.
    compact_from_busy: bool,
}

impl AgentMachine {
    pub fn new(info: AgentInfo) -> AgentMachine {
        AgentMachine {
            info,
            last_hook_ms: None,
            subagent_stops: 0,
            subagent_starts: 0,
            last_fg: None,
            compact_from_busy: false,
        }
    }

    pub fn info(&self) -> &AgentInfo {
        &self.info
    }

    /// Replace the info wholesale (revive / restore); forgets hook priority.
    pub fn reset(&mut self, info: AgentInfo) {
        self.info = info;
        self.last_hook_ms = None;
        self.subagent_stops = 0;
        self.subagent_starts = 0;
        self.last_fg = None;
        self.compact_from_busy = false;
    }

    pub fn subagent_stops(&self) -> u32 {
        self.subagent_stops
    }

    /// Where a compaction ends (see the module rules); `None` when none is
    /// under way (the other end-of-compaction event already ended it).
    fn after_compaction(&self) -> Option<AgentState> {
        let end = if self.compact_from_busy {
            AgentState::Thinking
        } else {
            AgentState::Idle
        };
        (self.info.state == AgentState::Compacting).then_some(end)
    }

    fn hook_recent(&self, now_ms: i64) -> bool {
        self.last_hook_ms
            .is_some_and(|t| now_ms - t < HOOK_PRIORITY_MS)
    }

    /// Enter `state`; returns whether anything visible changed.
    fn enter(
        &mut self,
        state: AgentState,
        source: StateSource,
        confidence: f32,
        now_ms: i64,
    ) -> bool {
        let changed = self.info.state != state;
        if changed {
            self.info.state = state;
            self.info.since_ms = now_ms;
        }
        let meta_changed = self.info.source != source || self.info.confidence != confidence;
        self.info.source = source;
        self.info.confidence = confidence;
        changed || meta_changed
    }

    /// The agent process is gone and the shell is back: kind `Shell` and
    /// `state`; the agent becomes `last_agent`, and external id and
    /// transcript stay (for a later resume). Returns whether the state
    /// changed.
    fn agent_left(
        &mut self,
        state: AgentState,
        source: StateSource,
        confidence: f32,
        now_ms: i64,
    ) -> bool {
        if self.info.kind.is_agent() {
            self.info.last_agent = Some(self.info.kind.clone());
        }
        self.info.kind = AgentKind::Shell;
        let before = self.info.state.clone();
        self.enter(state, source, confidence, now_ms);
        // A shell from now on: a new state even when it is `Idle` again
        // (the usual `/exit` from Claude's idle prompt).
        self.info.since_ms = now_ms;
        before != self.info.state
    }

    /// Tool of the running (or permission-blocked) tool call, if known.
    fn current_tool(&self) -> Option<String> {
        match &self.info.state {
            AgentState::ToolRunning { tool } => Some(tool.clone()),
            AgentState::WaitingPermission { tool } => tool.clone(),
            _ => None,
        }
    }

    fn set_kind(&mut self, kind: AgentKind) -> bool {
        if self.info.kind == kind {
            return false;
        }
        // Ids and transcript belong to the agent that reported them (the one
        // `resume_kind` names): another agent resuming them would open the
        // wrong session. It reports its own.
        if kind.is_agent() && self.info.resume_kind() != Some(&kind) {
            self.info.external_id = None;
            self.info.transcript_path = None;
        }
        self.info.kind = kind;
        true
    }

    /// Only plain tokens become the external id: it ends up in the argv of
    /// `Revive { ResumeAgent }`, and it comes from any local process that
    /// can reach the socket.
    fn set_external_id(&mut self, id: Option<&str>) -> bool {
        let Some(id) = id.filter(|id| !id.is_empty()) else {
            return false;
        };
        if !is_valid_external_id(id) {
            let preview: String = id.chars().take(64).collect();
            tracing::debug!(
                len = id.len(),
                id = %preview.escape_debug(),
                "agent session id rejected: not [A-Za-z0-9._-]{{1,128}}"
            );
            return false;
        }
        if self.info.external_id.as_deref() == Some(id) {
            return false;
        }
        self.info.external_id = Some(id.to_owned());
        true
    }

    pub fn apply(&mut self, signal: &Signal, now_ms: i64) -> Option<Applied> {
        if let Signal::Exited(code) = signal {
            return self.apply_pty_exit(*code, now_ms);
        }
        if matches!(self.info.state, AgentState::Exited { .. }) {
            return self.apply_while_exited(signal);
        }
        match signal {
            Signal::Hook(hook) => Some(self.apply_claude(hook, now_ms)),
            Signal::Codex(n) => Some(self.apply_codex(n, now_ms)),
            Signal::Osc(ev) => self.apply_osc(ev, now_ms),
            Signal::ForegroundProcess(name) => self.apply_foreground(name, now_ms),
            Signal::OutputActivity => {
                if !self.info.kind.is_agent()
                    || self.info.source == StateSource::Hook
                    || self.info.state != AgentState::Idle
                {
                    return None;
                }
                self.enter(
                    AgentState::Thinking,
                    StateSource::Heuristic,
                    HEURISTIC_CONFIDENCE,
                    now_ms,
                );
                Some(Applied {
                    kind: "heuristic:activity".into(),
                    detail: None,
                    state_changed: true,
                })
            }
            Signal::Silence {
                secs,
                cursor_at_line_start,
            } => {
                if !self.info.kind.is_agent()
                    || self.info.source == StateSource::Hook
                    || *secs < SILENCE_IDLE_SECS
                    || !cursor_at_line_start
                    || !self.info.state.is_busy()
                {
                    return None;
                }
                self.enter(
                    AgentState::Idle,
                    StateSource::Heuristic,
                    HEURISTIC_CONFIDENCE,
                    now_ms,
                );
                Some(Applied {
                    kind: "heuristic:idle".into(),
                    detail: Some(format!("{secs}s")),
                    state_changed: true,
                })
            }
            Signal::UserInput => {
                if !matches!(self.info.state, AgentState::Done | AgentState::WaitingInput) {
                    return None;
                }
                let (source, confidence) = (self.info.source, self.info.confidence);
                self.enter(AgentState::Idle, source, confidence, now_ms);
                Some(Applied {
                    kind: "input:user".into(),
                    detail: None,
                    state_changed: true,
                })
            }
            Signal::Exited(_) => unreachable!("handled above"),
        }
    }

    /// PTY EOF.
    fn apply_pty_exit(&mut self, code: Option<i32>, now_ms: i64) -> Option<Applied> {
        let state = AgentState::Exited { code };
        if self.info.state == state {
            return None;
        }
        let source = self.info.source;
        self.enter(state, source, 1.0, now_ms);
        Some(Applied {
            kind: "pty:exit".into(),
            detail: code.map(|c| c.to_string()),
            state_changed: true,
        })
    }

    /// `Exited` is terminal (review #19, DESIGN §9): late hooks are reported
    /// as `hook:late` and change nothing (not even ids or kind); OSC and
    /// heuristics are ignored.
    fn apply_while_exited(&self, signal: &Signal) -> Option<Applied> {
        let late = |name: String| Applied {
            kind: "hook:late".into(),
            detail: Some(name),
            state_changed: false,
        };
        match signal {
            Signal::Hook(hook) => Some(late(claude_event_name(&hook.event).to_owned())),
            Signal::Codex(n) => Some(late(format!("codex:{}", n.event_type))),
            _ => None,
        }
    }

    /// An agent name sets the kind (unless a hook spoke within
    /// `HOOK_PRIORITY_MS`); a switch from an agent name to any other name
    /// means the agent left — also out of a hook-sourced state, but not out
    /// of `Exited`.
    fn apply_foreground(&mut self, name: &str, now_ms: i64) -> Option<Applied> {
        let previous = self.last_fg.replace(name.to_owned());
        if let Some(kind) = agent_kind_for_process(name) {
            if self.hook_recent(now_ms) || !self.set_kind(kind) {
                return None;
            }
            if self.info.source == StateSource::Heuristic {
                self.info.confidence = HEURISTIC_CONFIDENCE;
            }
            return Some(Applied {
                kind: "heuristic:foreground".into(),
                detail: Some(name.to_owned()),
                state_changed: false,
            });
        }
        let agent_was_foreground = previous
            .as_deref()
            .and_then(agent_kind_for_process)
            .is_some();
        if !agent_was_foreground
            || !self.info.kind.is_agent()
            || matches!(self.info.state, AgentState::Exited { .. })
        {
            return None;
        }
        let state_changed = self.agent_left(
            AgentState::Idle,
            StateSource::Heuristic,
            HEURISTIC_CONFIDENCE,
            now_ms,
        );
        Some(Applied {
            kind: "heuristic:agent_left".into(),
            detail: Some(name.to_owned()),
            state_changed,
        })
    }

    fn apply_claude(&mut self, hook: &ClaudeHook, now_ms: i64) -> Applied {
        self.last_hook_ms = Some(now_ms);
        self.set_kind(AgentKind::Claude);
        self.set_external_id(Some(&hook.session_id));
        if hook.transcript_path.is_some() {
            self.info.transcript_path = hook.transcript_path.clone();
        }
        let (name, detail, next): (&str, Option<String>, Option<AgentState>) = match &hook.event {
            // `compact` is the end of a compaction, not a new session.
            ClaudeHookEvent::SessionStart { source } if source.as_deref() == Some("compact") => {
                ("SessionStart", source.clone(), self.after_compaction())
            }
            ClaudeHookEvent::SessionStart { source } => {
                ("SessionStart", source.clone(), Some(AgentState::Idle))
            }
            ClaudeHookEvent::UserPromptSubmit => {
                ("UserPromptSubmit", None, Some(AgentState::Thinking))
            }
            ClaudeHookEvent::PreToolUse { tool_name } => (
                "PreToolUse",
                Some(tool_name.clone()),
                Some(AgentState::ToolRunning {
                    tool: tool_name.clone(),
                }),
            ),
            ClaudeHookEvent::PostToolUse { tool_name } => (
                "PostToolUse",
                Some(tool_name.clone()),
                Some(AgentState::Thinking),
            ),
            ClaudeHookEvent::Notification {
                notification_type,
                message,
            } => {
                let next = match classify_notification(notification_type.as_deref(), message) {
                    // The notification does not name the tool: keep the one
                    // `PermissionRequest` / `PreToolUse` named (with parallel
                    // tool calls that may be another call's tool).
                    NotificationClass::Permission => Some(AgentState::WaitingPermission {
                        tool: self.current_tool(),
                    }),
                    NotificationClass::Input => Some(AgentState::WaitingInput),
                    NotificationClass::Working => Some(AgentState::Thinking),
                    NotificationClass::Completed => Some(AgentState::Done),
                    NotificationClass::Informational => None,
                };
                ("Notification", notification_type.clone(), next)
            }
            ClaudeHookEvent::Stop { .. } => ("Stop", None, Some(AgentState::Done)),
            ClaudeHookEvent::SubagentStop => {
                self.subagent_stops += 1;
                ("SubagentStop", Some(self.subagent_stops.to_string()), None)
            }
            ClaudeHookEvent::PreCompact { trigger } => {
                if self.info.state != AgentState::Compacting {
                    self.compact_from_busy = self.info.state.is_busy();
                }
                ("PreCompact", trigger.clone(), Some(AgentState::Compacting))
            }
            // DESIGN §9: the agent left and the shell lives on (`Exited` is
            // the PTY's). Ids and transcript stay for a later resume.
            ClaudeHookEvent::SessionEnd { reason } => {
                let state_changed =
                    self.agent_left(AgentState::Idle, StateSource::Hook, HOOK_CONFIDENCE, now_ms);
                return Applied {
                    kind: "hook:SessionEnd".into(),
                    detail: reason.clone(),
                    state_changed,
                };
            }
            ClaudeHookEvent::Other { hook_event_name } => {
                let name = hook_event_name.as_str();
                match name {
                    // The dialog is up now (the `permission_prompt`
                    // notification only follows after ~6 s without input).
                    "PermissionRequest" => {
                        let tool = self.current_tool();
                        (
                            name,
                            tool.clone(),
                            Some(AgentState::WaitingPermission { tool }),
                        )
                    }
                    // The tool call is over (refused / failed); the model
                    // carries on.
                    "PermissionDenied" | "PostToolUseFailure" => {
                        (name, None, Some(AgentState::Thinking))
                    }
                    // The turn ended on an API error.
                    "StopFailure" => (
                        name,
                        None,
                        Some(AgentState::Error {
                            message: "turn failed (StopFailure)".into(),
                        }),
                    ),
                    "PostCompact" => (name, None, self.after_compaction()),
                    // An MCP server asks the user for input; answered (by
                    // the user or a hook), the model carries on.
                    "Elicitation" => (name, None, Some(AgentState::WaitingInput)),
                    "ElicitationResult" => (name, None, Some(AgentState::Thinking)),
                    // Counted like `SubagentStop`; the main agent's state
                    // stays.
                    "SubagentStart" => {
                        self.subagent_starts += 1;
                        (name, Some(self.subagent_starts.to_string()), None)
                    }
                    // Everything else (CwdChanged — the manager moves the
                    // cwd —, unknown events) is recorded but never moves
                    // the state.
                    _ => (name, None, None),
                }
            }
        };
        let state_changed = match next {
            Some(state) => {
                let before = self.info.state.clone();
                self.enter(state, StateSource::Hook, HOOK_CONFIDENCE, now_ms);
                before != self.info.state
            }
            None => false,
        };
        Applied {
            kind: format!("hook:{name}"),
            detail,
            state_changed,
        }
    }

    fn apply_codex(&mut self, n: &CodexNotify, now_ms: i64) -> Applied {
        self.last_hook_ms = Some(now_ms);
        self.set_kind(AgentKind::Codex);
        self.set_external_id(n.thread_id.as_deref());
        let state_changed = if n.event_type == "agent-turn-complete" {
            let before = self.info.state.clone();
            self.enter(AgentState::Done, StateSource::Hook, HOOK_CONFIDENCE, now_ms);
            before != self.info.state
        } else {
            false
        };
        Applied {
            kind: format!("codex:{}", n.event_type),
            detail: None,
            state_changed,
        }
    }

    fn apply_osc(&mut self, ev: &OscEvent, now_ms: i64) -> Option<Applied> {
        let OscEvent::Prompt(mark) = ev else {
            return None;
        };
        let (kind, detail, state) = match mark {
            PromptMark::PromptStart => ("osc:133A", None, AgentState::Idle),
            PromptMark::OutputStart => ("osc:133C", None, AgentState::Thinking),
            PromptMark::CommandEnd { exit_code } => (
                "osc:133D",
                exit_code.map(|c| c.to_string()),
                AgentState::Idle,
            ),
            PromptMark::CommandStart => return None,
        };
        // Prompt marks come from the shell, so an agent that still owns the
        // session has left: that ends even a hook-sourced state, as an
        // "agent left" rather than a plain flip (never `Exited`: terminal).
        if self.info.kind.is_agent() {
            let state_changed =
                self.agent_left(state, StateSource::ShellIntegration, OSC_CONFIDENCE, now_ms);
            return Some(Applied {
                kind: kind.into(),
                detail: Some("agent_left".into()),
                state_changed,
            });
        }
        let before = self.info.state.clone();
        self.enter(state, StateSource::ShellIntegration, OSC_CONFIDENCE, now_ms);
        let changed = before != self.info.state;
        // 133;D always carries information (exit code); A/C only when they move.
        if !changed && !matches!(mark, PromptMark::CommandEnd { .. }) {
            return None;
        }
        Some(Applied {
            kind: kind.into(),
            detail,
            state_changed: changed,
        })
    }

    /// Statusline tee: model / context / cost. Returns whether anything changed.
    pub fn apply_statusline(&mut self, s: &StatuslineUpdate) -> bool {
        let mut changed = false;
        if self.info.kind == AgentKind::Shell {
            changed |= self.set_kind(AgentKind::Claude);
        }
        changed |= self.set_external_id(Some(&s.session_id));
        if s.model.is_some() && self.info.model != s.model {
            self.info.model = s.model.clone();
            changed = true;
        }
        if s.context_pct.is_some() && self.info.context_pct != s.context_pct {
            self.info.context_pct = s.context_pct;
            changed = true;
        }
        if s.cost_usd.is_some() && self.info.cost_usd != s.cost_usd {
            self.info.cost_usd = s.cost_usd;
            changed = true;
        }
        changed
    }
}

/// Hook event name as Claude Code reports it (`hook_event_name`).
pub fn claude_event_name(event: &ClaudeHookEvent) -> &str {
    match event {
        ClaudeHookEvent::SessionStart { .. } => "SessionStart",
        ClaudeHookEvent::UserPromptSubmit => "UserPromptSubmit",
        ClaudeHookEvent::PreToolUse { .. } => "PreToolUse",
        ClaudeHookEvent::PostToolUse { .. } => "PostToolUse",
        ClaudeHookEvent::Notification { .. } => "Notification",
        ClaudeHookEvent::Stop { .. } => "Stop",
        ClaudeHookEvent::SubagentStop => "SubagentStop",
        ClaudeHookEvent::PreCompact { .. } => "PreCompact",
        ClaudeHookEvent::SessionEnd { .. } => "SessionEnd",
        ClaudeHookEvent::Other { hook_event_name } => hook_event_name,
    }
}

/// Agent session ids (Claude `session_id`, Codex `thread-id`) are UUID-like:
/// `[A-Za-z0-9._-]{1,128}`. Anything else is refused.
pub fn is_valid_external_id(id: &str) -> bool {
    (1..=128).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// `claude*` (the native build reports `claude.exe`) and `codex` foreground
/// processes set the agent kind; anything else (shells, `node` without argv
/// evidence, editors) leaves it alone.
pub fn agent_kind_for_process(name: &str) -> Option<AgentKind> {
    let base = name.rsplit('/').next().unwrap_or(name).to_ascii_lowercase();
    if base.starts_with("claude") {
        Some(AgentKind::Claude)
    } else if base == "codex" || base.starts_with("codex-") {
        Some(AgentKind::Codex)
    } else {
        None
    }
}

/// `notification_type` values of the hooks reference (2026-09). Anything
/// else (`auth_success`, `quota_auto_resume_*`, future types) is
/// informational: recorded with the type as detail, state unchanged.
fn classify_notification(kind: Option<&str>, message: &str) -> NotificationClass {
    match kind {
        Some("permission_prompt") => NotificationClass::Permission,
        Some(
            "idle_prompt" | "elicitation_dialog" | "elicitation_url_dialog" | "agent_needs_input",
        ) => NotificationClass::Input,
        Some("elicitation_complete" | "elicitation_response") => NotificationClass::Working,
        Some("agent_completed") => NotificationClass::Completed,
        Some(_) => NotificationClass::Informational,
        // Older Claude Code versions sent no `notification_type`.
        None => {
            let m = message.to_ascii_lowercase();
            if m.contains("permission") {
                NotificationClass::Permission
            } else if m.contains("waiting for your input") {
                NotificationClass::Input
            } else {
                NotificationClass::Informational
            }
        }
    }
}

#[cfg(test)]
mod tests;
