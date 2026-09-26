//! Agent state machine (DESIGN §9).
//!
//! Inputs are `Signal`s from three tiers: hooks (Claude Code / Codex), shell
//! integration (OSC 133) and heuristics (foreground process, output activity,
//! silence). Rules:
//! - a hook signal within the last 30 s wins over heuristics;
//! - heuristics never leave `WaitingPermission` (nor the other "sticky"
//!   states `Done`, `WaitingInput`, `Error`, `Exited`): activity only moves
//!   `Idle → Thinking`, silence only moves busy states `→ Idle`;
//! - every applied transition is reported as `Applied` so the manager can
//!   write an `EventRecord` (kind + short detail, never prompt text).

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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NotificationClass {
    Permission,
    Input,
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

    fn current_tool(&self) -> Option<String> {
        match &self.info.state {
            AgentState::ToolRunning { tool } => Some(tool.clone()),
            AgentState::WaitingPermission { tool } => tool.clone(),
            _ => None,
        }
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

    fn set_external_id(&mut self, id: Option<&str>) -> bool {
        match id {
            Some(id) if !id.is_empty() && self.info.external_id.as_deref() != Some(id) => {
                self.info.external_id = Some(id.to_owned());
                true
            }
            _ => false,
        }
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
                    NotificationClass::Permission => Some(AgentState::WaitingPermission {
                        tool: self.current_tool(),
                    }),
                    NotificationClass::Input => Some(AgentState::WaitingInput),
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
            ClaudeHookEvent::Other { hook_event_name } => {
                // Newer events the core enum does not name (hooks reference,
                // 2026-09): map the ones that carry clear state meaning.
                let next = match hook_event_name.as_str() {
                    "PermissionRequest" => Some(AgentState::WaitingPermission {
                        tool: self.current_tool(),
                    }),
                    "PostToolUseFailure" | "PermissionDenied" => Some(AgentState::Thinking),
                    "StopFailure" => Some(AgentState::Error {
                        message: "stop failure".into(),
                    }),
                    "PostCompact" if self.info.state == AgentState::Compacting => {
                        Some(AgentState::Idle)
                    }
                    _ => None,
                };
                (hook_event_name.as_str(), None, next)
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

/// `claude` / `codex` foreground processes set the agent kind; anything
/// else (shells, `node` without argv evidence, editors) leaves it alone.
pub fn agent_kind_for_process(name: &str) -> Option<AgentKind> {
    let base = name.rsplit('/').next().unwrap_or(name).to_ascii_lowercase();
    if base == "claude" || base.starts_with("claude-") {
        Some(AgentKind::Claude)
    } else if base == "codex" || base.starts_with("codex-") {
        Some(AgentKind::Codex)
    } else {
        None
    }
}

fn classify_notification(kind: Option<&str>, message: &str) -> NotificationClass {
    match kind {
        Some("permission_prompt") => NotificationClass::Permission,
        Some(
            "idle_prompt" | "elicitation_dialog" | "elicitation_url_dialog" | "agent_needs_input",
        ) => NotificationClass::Input,
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
