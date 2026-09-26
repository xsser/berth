//! Route a `HookEnvelope` to a session.
//!
//! Order: `berth_session` (from `BERTH_SESSION_ID`, set in every PTY) when it
//! names a known session; else the session whose agent `external_id` equals
//! the payload's Claude session id / Codex thread id (unique match); else the
//! unique *live* session whose cwd equals the payload cwd. Anything else is
//! dropped (`debug!`), never guessed.

use std::path::{Path, PathBuf};

use berth_core::{AgentSignal, HookEnvelope, SessionId, SessionMeta};

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
) -> Option<SessionId> {
    if let Some(id) = envelope.berth_session {
        if sessions.clone().any(|m| m.id == id) {
            return Some(id);
        }
        tracing::debug!(session = %id, "hook names an unknown berth session; trying fallbacks");
    }
    if let Some(ext) = external_id(&envelope.signal) {
        let hit = unique(
            sessions
                .clone()
                .filter(|m| m.agent.external_id.as_deref() == Some(ext))
                .map(|m| m.id),
        );
        if hit.is_some() {
            return hit;
        }
    }
    for cwd in cwds(&envelope.signal) {
        let want = normalize(cwd);
        let hit = unique(
            sessions
                .clone()
                .filter(|m| m.is_live() && normalize(&m.cwd) == want)
                .map(|m| m.id),
        );
        if hit.is_some() {
            return hit;
        }
    }
    tracing::debug!(
        pid = envelope.pid,
        "hook event matches no unique session; dropped"
    );
    None
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
            Some(a.id)
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
            Some(a.id)
        );
        // cwd match ignores non-live sessions and trailing slashes.
        assert_eq!(
            resolve_target(&claude(None, "new", Some("/other")), all.iter()),
            Some(b.id)
        );
        // Nothing matches → dropped.
        assert_eq!(
            resolve_target(&claude(None, "new", Some("/nowhere")), all.iter()),
            None
        );
        assert_eq!(resolve_target(&claude(None, "", None), all.iter()), None);
    }

    #[test]
    fn ambiguous_cwd_is_dropped() {
        let all = [meta("/w", true, None), meta("/w", true, None)];
        assert_eq!(
            resolve_target(&claude(None, "x", Some("/w")), all.iter()),
            None
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
        assert_eq!(resolve_target(&codex, all.iter()), Some(a.id));
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
        assert_eq!(resolve_target(&status, all.iter()), Some(b.id));
    }
}
