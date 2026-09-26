//! Desktop notifications (integrate.md §4, DESIGN §8.3).
//!
//! [`Policy`] decides (pure, unit-tested): an `AgentChanged` whose state
//! needs attention, is listed in `[notify].on`, differs from the session's
//! previous state (statusline updates repeat the state), for a session that
//! is not being looked at (unfocused, or the window is unfocused), at most
//! once per session per [`REPEAT_WINDOW`]. Programs' own OSC 9 / 777
//! notifications follow the same focus and repeat rules.
//!
//! [`Notifier`] delivers on its own thread (`mac-notification-sys` may block
//! briefly); failures are logged.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use berth_core::{AgentKind, AgentState, SessionId};

pub const REPEAT_WINDOW: Duration = Duration::from_secs(30);
pub const DEFAULT_ON: [&str; 4] = ["waiting_permission", "waiting_input", "done", "error"];

#[derive(Debug)]
pub struct Policy {
    on: Vec<String>,
    state: HashMap<SessionId, &'static str>,
    sent: HashMap<SessionId, Instant>,
}

impl Policy {
    pub fn new(on: Vec<String>) -> Policy {
        Policy {
            on,
            state: HashMap::new(),
            sent: HashMap::new(),
        }
    }

    /// Remember a state without notifying (listing, reconnect).
    pub fn seed(&mut self, sid: SessionId, state: &AgentState) {
        self.state.insert(sid, state.name());
    }

    /// `attended`: the session is focused and the window has focus.
    pub fn agent_changed(
        &mut self,
        sid: SessionId,
        state: &AgentState,
        attended: bool,
        now: Instant,
    ) -> bool {
        let name = state.name();
        let changed = self.state.insert(sid, name) != Some(name);
        changed
            && state.needs_attention()
            && !attended
            && self.on.iter().any(|o| o == name)
            && self.allow(sid, now)
    }

    /// OSC 9 / 777 from the program in the session.
    pub fn program_notify(&mut self, sid: SessionId, attended: bool, now: Instant) -> bool {
        !attended && self.allow(sid, now)
    }

    pub fn forget(&mut self, sid: SessionId) {
        self.state.remove(&sid);
        self.sent.remove(&sid);
    }

    fn allow(&mut self, sid: SessionId, now: Instant) -> bool {
        match self.sent.get(&sid) {
            Some(t) if now.saturating_duration_since(*t) < REPEAT_WINDOW => false,
            _ => {
                self.sent.insert(sid, now);
                true
            }
        }
    }
}

/// Notification body for an agent state.
pub fn body(kind: &AgentKind, state: &AgentState) -> String {
    let who = match kind {
        AgentKind::Shell => "shell".to_string(),
        AgentKind::Claude => "claude".to_string(),
        AgentKind::Codex => "codex".to_string(),
        AgentKind::Other(name) => name.clone(),
    };
    match state {
        AgentState::WaitingPermission { tool: Some(t) } => format!("{who} 等待授权：{t}"),
        AgentState::WaitingPermission { tool: None } => format!("{who} 等待授权"),
        AgentState::WaitingInput => format!("{who} 等待输入"),
        AgentState::Done => format!("{who} 已完成"),
        AgentState::Error { message } if message.is_empty() => format!("{who} 出错"),
        AgentState::Error { message } => format!("{who} 出错：{message}"),
        other => format!("{who} {}", other.name()),
    }
}

/// Background delivery of desktop notifications.
pub struct Notifier {
    tx: crossbeam_channel::Sender<(String, String)>,
}

impl Notifier {
    pub fn start() -> anyhow::Result<Notifier> {
        let (tx, rx) = crossbeam_channel::bounded::<(String, String)>(64);
        std::thread::Builder::new()
            .name("berth-notify".into())
            .spawn(move || {
                for (title, body) in rx {
                    deliver(&title, &body);
                }
            })?;
        Ok(Notifier { tx })
    }

    pub fn send(&self, title: String, body: String) {
        if let Err(e) = self.tx.try_send((title, body)) {
            tracing::warn!("notification dropped: {e}");
        }
    }
}

#[cfg(target_os = "macos")]
fn deliver(title: &str, body: &str) {
    // Unbundled binaries notify under the library's default identity until
    // berth ships as an app bundle.
    match mac_notification_sys::send_notification(title, None, body, None) {
        Ok(_) => tracing::info!(title, body, "desktop notification handed to macOS"),
        Err(e) => tracing::warn!(title, "desktop notification failed: {e}"),
    }
}

#[cfg(not(target_os = "macos"))]
fn deliver(title: &str, body: &str) {
    tracing::info!(
        title,
        body,
        "desktop notification (not implemented on this platform)"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> Policy {
        Policy::new(DEFAULT_ON.iter().map(|s| s.to_string()).collect())
    }

    #[test]
    fn attention_states_notify_once_per_change_and_window() {
        let mut p = policy();
        let s = SessionId::new();
        let t0 = Instant::now();
        p.seed(s, &AgentState::Thinking);
        assert!(
            !p.agent_changed(s, &AgentState::Thinking, false, t0),
            "busy"
        );
        assert!(p.agent_changed(s, &AgentState::WaitingInput, false, t0));
        // Same state again (statusline refresh): no repeat.
        assert!(!p.agent_changed(s, &AgentState::WaitingInput, false, t0));
        // A new attention state within 30 s: suppressed.
        assert!(!p.agent_changed(s, &AgentState::Done, false, t0 + Duration::from_secs(5)));
        // After the window.
        p.agent_changed(
            s,
            &AgentState::Thinking,
            false,
            t0 + Duration::from_secs(40),
        );
        assert!(p.agent_changed(s, &AgentState::Done, false, t0 + Duration::from_secs(41)));
    }

    #[test]
    fn attended_sessions_and_filtered_states_do_not_notify() {
        let mut p = policy();
        let s = SessionId::new();
        let t0 = Instant::now();
        assert!(
            !p.agent_changed(s, &AgentState::Done, true, t0),
            "looking at it"
        );
        let mut only_permission = Policy::new(vec!["waiting_permission".into()]);
        assert!(!only_permission.agent_changed(s, &AgentState::Done, false, t0));
        assert!(only_permission.agent_changed(
            s,
            &AgentState::WaitingPermission { tool: None },
            false,
            t0
        ));
        let mut off = Policy::new(Vec::new());
        assert!(!off.agent_changed(
            s,
            &AgentState::Error {
                message: "x".into()
            },
            false,
            t0
        ));
    }

    #[test]
    fn program_notifications_share_the_repeat_window() {
        let mut p = policy();
        let s = SessionId::new();
        let t0 = Instant::now();
        assert!(!p.program_notify(s, true, t0));
        assert!(p.program_notify(s, false, t0));
        assert!(!p.program_notify(s, false, t0 + Duration::from_secs(10)));
        p.forget(s);
        assert!(p.program_notify(s, false, t0 + Duration::from_secs(11)));
    }

    #[test]
    fn bodies_name_the_agent_and_state() {
        assert_eq!(
            body(
                &AgentKind::Claude,
                &AgentState::WaitingPermission {
                    tool: Some("Bash".into())
                }
            ),
            "claude 等待授权：Bash"
        );
        assert_eq!(body(&AgentKind::Codex, &AgentState::Done), "codex 已完成");
    }
}
