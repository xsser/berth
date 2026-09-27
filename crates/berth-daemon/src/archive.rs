//! Automatic archiving (DESIGN §17.1). A scan runs `FIRST_SCAN` after the
//! daemon starts and every `SCAN_EVERY` after that (`run_scanner`); for
//! each session `decide` says whether to archive it — a live one is killed
//! first — or to purge it. `Manager::scan_archive` applies the decisions.
//!
//! "Now" is always injected: `decide` is a pure function of the session,
//! what its foreground poll saw, the time and `[archive]`, and the scanner
//! takes its clock as a parameter, so neither needs days of waiting to be
//! tested.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use berth_core::SessionMeta;
use tokio::sync::watch;
use tokio::time::MissedTickBehavior;

use crate::config::{ArchiveConfig, DAY_MS};
use crate::manager::Manager;
use crate::wait_true;

/// Delay of the first scan after the daemon started (DESIGN §17.1).
pub const FIRST_SCAN: Duration = Duration::from_secs(60);
/// Interval between two scans.
pub const SCAN_EVERY: Duration = Duration::from_secs(10 * 60);

/// What a scan does with a session. `Display` is the reason logged with it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScanAction {
    /// Idle for `idle_ms`, more than `auto_after_days`: archive it, killing
    /// it first when `live` (a plain shell idle at its prompt).
    Archive { idle_ms: i64, live: bool },
    /// Archived `archived_ms` ago, more than `purge_after_days`: purge it
    /// like `Request::Delete`.
    Purge { archived_ms: i64 },
}

impl fmt::Display for ScanAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let days = |ms: i64| ms as f64 / DAY_MS as f64;
        match *self {
            ScanAction::Archive {
                idle_ms,
                live: false,
            } => write!(f, "idle for {:.1} days, not live", days(idle_ms)),
            ScanAction::Archive {
                idle_ms,
                live: true,
            } => write!(
                f,
                "idle for {:.1} days, a shell at its prompt (killed first)",
                days(idle_ms)
            ),
            ScanAction::Purge { archived_ms } => {
                write!(f, "archived {:.1} days ago", days(archived_ms))
            }
        }
    }
}

/// What the scan at `now_ms` does with `meta`, if anything (DESIGN §17.1):
///
/// - archived for more than `purge_after_days` (unless 0): purge;
/// - otherwise, idle (`last_active_ms`) for more than `auto_after_days`
///   (unless 0): archive when it is not live, or when it is live but only
///   a plain shell idles at its prompt — kind not an agent, state not busy,
///   and no foreground command (`shell_in_foreground`).
///
/// `shell_in_foreground` is what the session's last foreground poll found
/// (live sessions): `Some(true)` = the session's own shell is the
/// foreground process, `Some(false)` = something else is, `None` = unknown
/// (not polled yet, not inspectable), which counts as a foreground command.
pub fn decide(
    meta: &SessionMeta,
    shell_in_foreground: Option<bool>,
    now_ms: i64,
    config: &ArchiveConfig,
) -> Option<ScanAction> {
    if let Some(at) = meta.archived_at_ms {
        let after = config.purge_after_ms()?;
        let archived_ms = now_ms.saturating_sub(at);
        // An archived session is never live; one that were is left alone.
        return (archived_ms > after && !meta.is_live())
            .then_some(ScanAction::Purge { archived_ms });
    }
    let after = config.auto_after_ms()?;
    let idle_ms = now_ms.saturating_sub(meta.last_active_ms);
    if idle_ms <= after {
        return None;
    }
    if !meta.is_live() {
        return Some(ScanAction::Archive {
            idle_ms,
            live: false,
        });
    }
    let idle_shell = !meta.agent.kind.is_agent()
        && !meta.agent.state.is_busy()
        && shell_in_foreground == Some(true);
    idle_shell.then_some(ScanAction::Archive {
        idle_ms,
        live: true,
    })
}

/// Run `Manager::scan_archive` after `first`, then every `every` (non-zero)
/// until `stop` turns true; a scan that takes longer delays the next one.
/// `clock` is "now" in ms since the epoch (`berth_core::now_ms` in the
/// daemon). A scan still running when `stop` turns true is abandoned: the
/// daemon is shutting down.
pub async fn run_scanner<C>(
    mgr: Arc<Manager>,
    mut stop: watch::Receiver<bool>,
    first: Duration,
    every: Duration,
    clock: C,
) where
    C: Fn() -> i64 + Send + Sync + 'static,
{
    let mut ticks = tokio::time::interval_at(tokio::time::Instant::now() + first, every);
    ticks.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = ticks.tick() => {}
            _ = wait_true(&mut stop) => return,
        }
        tokio::select! {
            _ = mgr.scan_archive(&clock) => {}
            _ = wait_true(&mut stop) => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use berth_core::{AgentKind, AgentState, SessionStatus};

    use super::*;

    const NOW: i64 = 1_800_000_000_000;
    const WEEK: i64 = 7 * DAY_MS;

    fn session(live: bool, idle_ms: i64) -> SessionMeta {
        SessionMeta {
            status: if live {
                SessionStatus::Live
            } else {
                SessionStatus::Dormant {
                    exit_code: Some(0),
                    at_ms: NOW - idle_ms,
                }
            },
            last_active_ms: NOW - idle_ms,
            ..SessionMeta::default()
        }
    }

    fn archived(archived_ms: i64) -> SessionMeta {
        SessionMeta {
            status: SessionStatus::Restored,
            archived_at_ms: Some(NOW - archived_ms),
            ..session(false, 60 * DAY_MS)
        }
    }

    fn days(auto: u32, purge: u32) -> ArchiveConfig {
        ArchiveConfig {
            auto_after_days: auto,
            purge_after_days: purge,
        }
    }

    #[test]
    fn a_live_shell_idle_at_its_prompt_is_archived_after_the_idle_days() {
        let cfg = ArchiveConfig::default();
        let idle = session(true, WEEK + 1);
        assert_eq!(
            decide(&idle, Some(true), NOW, &cfg),
            Some(ScanAction::Archive {
                idle_ms: WEEK + 1,
                live: true
            })
        );
        // "More than 7 days": exactly 7 is not enough.
        assert_eq!(decide(&session(true, WEEK), Some(true), NOW, &cfg), None);
        let three = days(3, 0);
        assert!(decide(&session(true, 3 * DAY_MS + 1), Some(true), NOW, &three).is_some());
        // Sticky states that are not busy do not keep a plain shell.
        let mut done = session(true, WEEK + 1);
        done.agent.state = AgentState::Done;
        assert!(decide(&done, Some(true), NOW, &cfg).is_some());
    }

    #[test]
    fn a_live_agent_is_never_archived() {
        let cfg = ArchiveConfig::default();
        for kind in [
            AgentKind::Claude,
            AgentKind::Codex,
            AgentKind::Other("aider".into()),
        ] {
            for state in [AgentState::Idle, AgentState::Done, AgentState::WaitingInput] {
                let mut s = session(true, 30 * DAY_MS);
                s.agent.kind = kind.clone();
                s.agent.state = state.clone();
                assert_eq!(
                    decide(&s, Some(true), NOW, &cfg),
                    None,
                    "{kind:?} {state:?}"
                );
            }
        }
    }

    #[test]
    fn a_live_busy_shell_is_never_archived() {
        let cfg = ArchiveConfig::default();
        for state in [
            AgentState::Thinking,
            AgentState::ToolRunning {
                tool: "Bash".into(),
            },
            AgentState::Compacting,
        ] {
            let mut s = session(true, 30 * DAY_MS);
            s.agent.state = state.clone();
            assert_eq!(decide(&s, Some(true), NOW, &cfg), None, "{state:?}");
        }
    }

    /// A foreground command, or no usable foreground information, keeps a
    /// live session: only a shell known to sit at its prompt is killed.
    #[test]
    fn a_foreground_command_or_an_unknown_foreground_keeps_a_live_session() {
        let cfg = ArchiveConfig::default();
        let s = session(true, 30 * DAY_MS);
        assert_eq!(decide(&s, Some(false), NOW, &cfg), None);
        assert_eq!(decide(&s, None, NOW, &cfg), None);
    }

    #[test]
    fn a_session_that_is_not_live_is_archived_whatever_it_ran() {
        let cfg = ArchiveConfig::default();
        let mut s = session(false, WEEK + 5);
        s.agent.kind = AgentKind::Claude;
        s.agent.state = AgentState::Exited { code: None };
        let archive = Some(ScanAction::Archive {
            idle_ms: WEEK + 5,
            live: false,
        });
        assert_eq!(decide(&s, None, NOW, &cfg), archive);
        s.status = SessionStatus::Restored;
        assert_eq!(decide(&s, Some(false), NOW, &cfg), archive);
        assert_eq!(decide(&session(false, WEEK), None, NOW, &cfg), None);
    }

    #[test]
    fn an_archived_session_is_purged_after_the_purge_days_only() {
        let thirty = days(7, 30);
        assert_eq!(
            decide(&archived(30 * DAY_MS + 1), None, NOW, &thirty),
            Some(ScanAction::Purge {
                archived_ms: 30 * DAY_MS + 1
            })
        );
        assert_eq!(decide(&archived(30 * DAY_MS), None, NOW, &thirty), None);
        // Purging does not depend on automatic archiving.
        assert!(decide(&archived(31 * DAY_MS), None, NOW, &days(0, 30)).is_some());
        // Default: never purged, and an archived session is not archived
        // again however long it is idle.
        let cfg = ArchiveConfig::default();
        assert_eq!(decide(&archived(365 * DAY_MS), None, NOW, &cfg), None);
        // Never a live one (archived sessions are never live).
        let mut odd = archived(365 * DAY_MS);
        odd.status = SessionStatus::Live;
        assert_eq!(decide(&odd, Some(true), NOW, &thirty), None);
    }

    #[test]
    fn zero_days_turn_archiving_off() {
        let off = days(0, 0);
        assert_eq!(decide(&session(false, 365 * DAY_MS), None, NOW, &off), None);
        assert_eq!(
            decide(&session(true, 365 * DAY_MS), Some(true), NOW, &off),
            None
        );
        assert_eq!(decide(&archived(365 * DAY_MS), None, NOW, &off), None);
    }

    /// A clock that went backwards (or times from a machine ahead of this
    /// one) never makes anything look old.
    #[test]
    fn times_in_the_future_are_not_idle() {
        let cfg = days(7, 30);
        assert_eq!(decide(&session(false, -WEEK), None, NOW, &cfg), None);
        assert_eq!(decide(&archived(-60 * DAY_MS), None, NOW, &cfg), None);
        let mut ancient = session(false, 0);
        ancient.last_active_ms = i64::MIN;
        assert!(decide(&ancient, None, NOW, &cfg).is_some(), "no overflow");
    }

    #[test]
    fn reasons_name_the_days() {
        let archive = ScanAction::Archive {
            idle_ms: 8 * DAY_MS,
            live: true,
        };
        assert_eq!(
            archive.to_string(),
            "idle for 8.0 days, a shell at its prompt (killed first)"
        );
        let purge = ScanAction::Purge {
            archived_ms: 31 * DAY_MS + DAY_MS / 2,
        };
        assert_eq!(purge.to_string(), "archived 31.5 days ago");
    }
}
