//! Route a `HookEnvelope` to a session. Only live sessions receive hooks.
//!
//! Order: `berth_session` (from `BERTH_SESSION_ID`, set in every PTY) when it
//! names a known session — if that session is no longer live the hook is late
//! and dropped, never re-routed to another session; else the unique live
//! session whose agent `external_id` equals the payload's Claude session id /
//! Codex thread id; else the unique live session whose cwd equals the payload
//! cwd. Anything else is dropped (`debug!`), never guessed.

use std::path::{Path, PathBuf};

use berth_core::{AgentSignal, HookEnvelope, SessionId, SessionMeta};

use crate::agent_state::claude_event_name;

/// Where a hook envelope goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    Live(SessionId),
    /// The session the hook names is no longer live (the agent outlived it):
    /// dropped, but worth a `hook:late` event record there.
    Late(SessionId),
    /// No unique live session matches: dropped.
    Unmatched,
}

/// Event name for records: the Claude hook event, the Codex notify type, or
/// `statusline`.
pub fn event_name(signal: &AgentSignal) -> String {
    match signal {
        AgentSignal::Claude(h) => claude_event_name(&h.event).to_owned(),
        AgentSignal::Codex(n) => format!("codex:{}", n.event_type),
        AgentSignal::Statusline(_) => "statusline".into(),
    }
}

fn external_id(signal: &AgentSignal) -> Option<&str> {
    let id = match signal {
        AgentSignal::Claude(h) => h.session_id.as_str(),
        AgentSignal::Codex(n) => n.thread_id.as_deref()?,
        AgentSignal::Statusline(s) => s.session_id.as_str(),
    };
    (!id.is_empty()).then_some(id)
}

fn cwds(signal: &AgentSignal) -> Vec<&Path> {
    let mut out: Vec<&Path> = Vec::new();
    match signal {
        AgentSignal::Claude(h) => out.extend(h.cwd.as_deref()),
        AgentSignal::Codex(n) => out.extend(n.cwd.as_deref()),
        AgentSignal::Statusline(s) => {
            out.extend(s.cwd.as_deref());
            out.extend(s.project_dir.as_deref());
        }
    }
    out
}

fn normalize(p: &Path) -> PathBuf {
    p.components().collect()
}

fn unique<I: Iterator<Item = SessionId>>(mut it: I) -> Option<SessionId> {
    let first = it.next()?;
    it.next().is_none().then_some(first)
}

pub fn resolve_target<'a>(
    envelope: &HookEnvelope,
    sessions: impl Iterator<Item = &'a SessionMeta> + Clone,
) -> Target {
    if let Some(id) = envelope.berth_session {
        match sessions.clone().find(|m| m.id == id) {
            Some(m) if m.is_live() => return Target::Live(id),
            Some(_) => {
                tracing::debug!(session = %id, "hook for a session that is no longer live; dropped");
                return Target::Late(id);
            }
            None => {
                tracing::debug!(session = %id, "hook names an unknown berth session; trying fallbacks");
            }
        }
    }
    let live = sessions.filter(|m| m.is_live());
    if let Some(ext) = external_id(&envelope.signal) {
        let hit = unique(
            live.clone()
                .filter(|m| m.agent.external_id.as_deref() == Some(ext))
                .map(|m| m.id),
        );
        if let Some(id) = hit {
            return Target::Live(id);
        }
    }
    for cwd in cwds(&envelope.signal) {
        let want = normalize(cwd);
        let hit = unique(
            live.clone()
                .filter(|m| normalize(&m.cwd) == want)
                .map(|m| m.id),
        );
        if let Some(id) = hit {
            return Target::Live(id);
        }
    }
    tracing::debug!(
        pid = envelope.pid,
        "hook event matches no unique live session; dropped"
    );
    Target::Unmatched
}

#[cfg(test)]
mod tests {
    use super::*;
    use berth_core::{ClaudeHook, ClaudeHookEvent, CodexNotify, SessionStatus, StatuslineUpdate};

    fn meta(cwd: &str, live: bool, ext: Option<&str>) -> SessionMeta {
        let mut m = SessionMeta {
            id: SessionId::new(),
            cwd: cwd.into(),
            ..Default::default()
        };
        if !live {
            m.status = SessionStatus::Restored;
        }
        m.agent.external_id = ext.map(Into::into);
        m
    }

    fn claude(berth: Option<SessionId>, session_id: &str, cwd: Option<&str>) -> HookEnvelope {
        HookEnvelope {
            berth_session: berth,
            pid: 1,
            sent_at_ms: 0,
            signal: AgentSignal::Claude(ClaudeHook {
                session_id: session_id.into(),
                cwd: cwd.map(Into::into),
                transcript_path: None,
                permission_mode: None,
                event: ClaudeHookEvent::Stop {
                    stop_hook_active: false,
                },
            }),
        }
    }

    #[test]
    fn berth_session_wins() {
        let a = meta("/w", true, None);
        let b = meta("/w", true, Some("ext"));
        let all = [a.clone(), b.clone()];
        assert_eq!(
            resolve_target(&claude(Some(a.id), "ext", Some("/w")), all.iter()),
            Target::Live(a.id)
        );
    }

    #[test]
    fn external_id_then_unique_live_cwd() {
        let a = meta("/w", true, Some("ext-a"));
        let b = meta("/other/", true, None);
        let dead = meta("/other", false, None);
        let all = [a.clone(), b.clone(), dead];
        // Unknown berth session falls back to the external id.
        assert_eq!(
            resolve_target(&claude(Some(SessionId::new()), "ext-a", None), all.iter()),
            Target::Live(a.id)
        );
        // cwd match ignores non-live sessions and trailing slashes.
        assert_eq!(
            resolve_target(&claude(None, "new", Some("/other")), all.iter()),
            Target::Live(b.id)
        );
        // Nothing matches → dropped.
        assert_eq!(
            resolve_target(&claude(None, "new", Some("/nowhere")), all.iter()),
            Target::Unmatched
        );
        assert_eq!(
            resolve_target(&claude(None, "", None), all.iter()),
            Target::Unmatched
        );
    }

    /// Review #16 / #19: every tier considers live sessions only, and a hook
    /// naming a session that has exited is late — never re-routed.
    #[test]
    fn only_live_sessions_receive_hooks() {
        let dead = meta("/w", false, Some("ext"));
        let live = meta("/w", true, Some("ext"));
        let all = [dead.clone(), live.clone()];
        // berth_session tier: the named session exited → late, not `live`.
        assert_eq!(
            resolve_target(&claude(Some(dead.id), "ext", Some("/w")), all.iter()),
            Target::Late(dead.id)
        );
        // external-id tier: the live one of two sessions sharing the id.
        assert_eq!(
            resolve_target(&claude(None, "ext", None), all.iter()),
            Target::Live(live.id)
        );
        // cwd tier: the live one of two sessions sharing the cwd.
        assert_eq!(
            resolve_target(&claude(None, "other", Some("/w")), all.iter()),
            Target::Live(live.id)
        );
        // Only dead sessions match: nothing.
        let only_dead = [dead.clone()];
        assert_eq!(
            resolve_target(&claude(None, "ext", Some("/w")), only_dead.iter()),
            Target::Unmatched
        );
    }

    #[test]
    fn ambiguous_cwd_is_dropped() {
        let all = [meta("/w", true, None), meta("/w", true, None)];
        assert_eq!(
            resolve_target(&claude(None, "x", Some("/w")), all.iter()),
            Target::Unmatched
        );
    }

    #[test]
    fn codex_and_statusline_keys() {
        let a = meta("/proj", true, Some("thread-9"));
        let b = meta("/proj2", true, None);
        let all = [a.clone(), b.clone()];
        let codex = HookEnvelope {
            berth_session: None,
            pid: 1,
            sent_at_ms: 0,
            signal: AgentSignal::Codex(CodexNotify {
                event_type: "agent-turn-complete".into(),
                thread_id: Some("thread-9".into()),
                cwd: Some("/elsewhere".into()),
                last_message: None,
            }),
        };
        assert_eq!(resolve_target(&codex, all.iter()), Target::Live(a.id));
        let status = HookEnvelope {
            berth_session: None,
            pid: 1,
            sent_at_ms: 0,
            signal: AgentSignal::Statusline(StatuslineUpdate {
                session_id: "unknown".into(),
                cwd: Some("/proj2/sub".into()),
                project_dir: Some("/proj2".into()),
                ..Default::default()
            }),
        };
        assert_eq!(resolve_target(&status, all.iter()), Target::Live(b.id));
    }
}
