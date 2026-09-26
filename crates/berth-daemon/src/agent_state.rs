//! Agent state machine (DESIGN §9).
//!
//! Inputs are `Signal`s from three tiers: hooks (Claude Code / Codex), shell
//! integration (OSC 133) and heuristics (foreground process, output activity,
//! silence). Rules:
//! - a hook signal within the last 30 s wins over heuristics;
//! - heuristics never leave `WaitingPermission` (nor the other "sticky"
//!   states `Done`, `WaitingInput`, `Error`, `Exited`): activity only moves
//!   `Idle → Thinking`, silence only moves busy states `→ Idle`;
//! - only the nine core Claude hook events (and `Notification` types with a
//!   clear meaning) move the state; the other hook events (`Other`) are
//!   recorded but never change it;
//! - every applied signal is reported as `Applied` so the manager can write
//!   an `EventRecord` (kind + short detail, never prompt text).

use berth_core::{
    AgentInfo, AgentKind, AgentState, ClaudeHook, ClaudeHookEvent, CodexNotify, StateSource,
    StatuslineUpdate,
};
use berth_vt::{OscEvent, PromptMark};

/// How long a hook keeps heuristics from changing the state.
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
}

impl AgentMachine {
    pub fn new(info: AgentInfo) -> AgentMachine {
        AgentMachine {
            info,
            last_hook_ms: None,
            subagent_stops: 0,
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
    }

    pub fn subagent_stops(&self) -> u32 {
        self.subagent_stops
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

    fn set_kind(&mut self, kind: AgentKind) -> bool {
        if self.info.kind == kind {
            return false;
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
        match signal {
            Signal::Hook(hook) => Some(self.apply_claude(hook, now_ms)),
            Signal::Codex(n) => Some(self.apply_codex(n, now_ms)),
            Signal::Osc(ev) => self.apply_osc(ev, now_ms),
            Signal::ForegroundProcess(name) => {
                let kind = agent_kind_for_process(name)?;
                if self.hook_recent(now_ms) || !self.set_kind(kind) {
                    return None;
                }
                if self.info.source == StateSource::Heuristic {
                    self.info.confidence = HEURISTIC_CONFIDENCE;
                }
                Some(Applied {
                    kind: "heuristic:foreground".into(),
                    detail: Some(name.clone()),
                    state_changed: false,
                })
            }
            Signal::OutputActivity => {
                if !self.info.kind.is_agent()
                    || self.hook_recent(now_ms)
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
                    || self.hook_recent(now_ms)
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
            Signal::Exited(code) => {
                let state = AgentState::Exited { code: *code };
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
        }
    }

    fn apply_claude(&mut self, hook: &ClaudeHook, now_ms: i64) -> Applied {
        self.last_hook_ms = Some(now_ms);
        self.set_kind(AgentKind::Claude);
        self.set_external_id(Some(&hook.session_id));
        if hook.transcript_path.is_some() {
            self.info.transcript_path = hook.transcript_path.clone();
        }
        let (name, detail, next): (&str, Option<String>, Option<AgentState>) = match &hook.event {
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
                    // The notification does not name the tool, and with
                    // parallel tool calls the last PreToolUse may be another.
                    NotificationClass::Permission => {
                        Some(AgentState::WaitingPermission { tool: None })
                    }
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
                ("PreCompact", trigger.clone(), Some(AgentState::Compacting))
            }
            ClaudeHookEvent::SessionEnd { reason } => (
                "SessionEnd",
                reason.clone(),
                Some(AgentState::Exited { code: None }),
            ),
            // The events beyond the nine core ones (PermissionRequest,
            // StopFailure, PostCompact, ...) are recorded but never move the
            // state.
            ClaudeHookEvent::Other { hook_event_name } => (hook_event_name.as_str(), None, None),
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
            self.info.kind = AgentKind::Claude;
            changed = true;
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
