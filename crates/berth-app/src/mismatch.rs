//! A berthd of another protocol version is still running (started by an
//! older or newer berth): what the GUI shows about it, and the state of its
//! 「重启 berthd」 action. The app runs the steps (stop the daemon in its own
//! protocol, wait until it is gone, start this build's berthd through the
//! usual launch path, connect); this type only decides what they lead to,
//! so its transitions are tested without a window.

use berth_core::PROTOCOL_VERSION;

use crate::client::{Incompatible, RESTART_EFFECT, STOP_WAIT};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Mismatch {
    #[default]
    None,
    /// The last connection attempt met this daemon.
    Seen(Incompatible),
    /// Stopping it: its sessions write their snapshots first.
    Stopping(Incompatible),
    /// It is gone; connecting launches this build's berthd.
    Starting(Incompatible),
    /// Stopping it failed, or the berthd started after it differs too.
    Failed { daemon: Incompatible, error: String },
}

/// What the terminal area shows instead of a session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Banner {
    pub title: String,
    pub body: String,
    /// The restart button's label, when it can be pressed.
    pub button: Option<&'static str>,
}

impl Mismatch {
    /// A connection attempt failed; `refused` when berthd answered `Hello`
    /// with another protocol version.
    pub fn connect_failed(&mut self, refused: Option<Incompatible>) {
        *self = match (std::mem::take(self), refused) {
            // Attempts that were under way when the restart began.
            (Mismatch::Stopping(old), _) => Mismatch::Stopping(old),
            (Mismatch::Starting(_), Some(daemon)) => Mismatch::Failed {
                daemon,
                error: "启动的 berthd 仍是这个协议版本：它的可执行文件与本 berth \
                        不是同一次构建（berth doctor 显示用的是哪个）"
                    .into(),
            },
            (Mismatch::Failed { daemon, error }, Some(again)) if again == daemon => {
                Mismatch::Failed { daemon, error }
            }
            (_, Some(daemon)) => Mismatch::Seen(daemon),
            // Nothing of another version listens any more.
            (_, None) => Mismatch::None,
        };
    }

    /// Connected. Returns the notice for a restart that just completed.
    pub fn connected(&mut self) -> Option<String> {
        match std::mem::take(self) {
            Mismatch::Starting(old) => Some(format!(
                "已重启 berthd（协议 v{} → v{PROTOCOL_VERSION}）：原有会话以休眠 / 已恢复状态出现，\
                 历史保留，可 Revive",
                old.daemon
            )),
            _ => None,
        }
    }

    /// The restart button. Returns whether to stop the daemon now.
    pub fn restart(&mut self) -> bool {
        match self {
            Mismatch::Seen(daemon) | Mismatch::Failed { daemon, .. } => {
                *self = Mismatch::Stopping(*daemon);
                true
            }
            _ => false,
        }
    }

    /// The stop finished: connect next (launching this build's berthd), or
    /// show why it failed.
    pub fn stopped(&mut self, result: Result<(), String>) {
        if let Mismatch::Stopping(daemon) = *self {
            *self = match result {
                Ok(()) => Mismatch::Starting(daemon),
                Err(error) => Mismatch::Failed { daemon, error },
            };
        }
    }

    pub fn banner(&self) -> Option<Banner> {
        Some(match self {
            Mismatch::None => return None,
            Mismatch::Seen(daemon) => Banner {
                title: "berthd 版本不一致".into(),
                body: format!(
                    "{daemon}。仍在运行的 berthd 来自另一版本的 berth，这个窗口无法与它通信。\
                     {RESTART_EFFECT}。"
                ),
                button: Some("重启 berthd"),
            },
            Mismatch::Stopping(daemon) => Banner {
                title: "正在停止旧 berthd…".into(),
                body: format!(
                    "{daemon}。它正在保存各会话的快照，最多等 {} s。",
                    STOP_WAIT.as_secs()
                ),
                button: None,
            },
            Mismatch::Starting(daemon) => Banner {
                title: "正在启动 berthd…".into(),
                body: format!("旧 berthd（协议 v{}）已退出。", daemon.daemon),
                button: None,
            },
            Mismatch::Failed { daemon, error } => Banner {
                title: "重启 berthd 失败".into(),
                body: format!("{daemon}。{error}"),
                button: Some("重试"),
            },
        })
    }

    /// The sidebar's connection line.
    pub fn status(&self) -> Option<&'static str> {
        match self {
            Mismatch::None => None,
            Mismatch::Seen(_) => Some("berthd 版本不一致"),
            Mismatch::Stopping(_) => Some("正在停止旧 berthd…"),
            Mismatch::Starting(_) => Some("正在启动 berthd…"),
            Mismatch::Failed { .. } => Some("重启 berthd 失败"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OLD: Incompatible = Incompatible {
        daemon: PROTOCOL_VERSION - 1,
    };

    fn seen() -> Mismatch {
        let mut m = Mismatch::None;
        m.connect_failed(Some(OLD));
        m
    }

    #[test]
    fn a_refused_hello_shows_both_versions_what_a_restart_does_and_the_button() {
        let m = seen();
        assert_eq!(m, Mismatch::Seen(OLD));
        let b = m.banner().unwrap();
        let versions = format!(
            "berthd 协议 v{}，本客户端协议 v{PROTOCOL_VERSION}",
            PROTOCOL_VERSION - 1
        );
        assert!(b.body.contains(&versions), "{}", b.body);
        assert!(b.body.contains(RESTART_EFFECT), "{}", b.body);
        assert_eq!(b.button, Some("重启 berthd"));
        assert_eq!(m.status(), Some("berthd 版本不一致"));
    }

    #[test]
    fn a_restart_stops_then_starts_then_names_the_restored_sessions() {
        let mut m = seen();
        assert!(m.restart());
        assert_eq!(m, Mismatch::Stopping(OLD));
        assert_eq!(m.banner().unwrap().button, None, "no second press");
        assert!(!m.restart());
        m.connect_failed(Some(OLD)); // an attempt from before the press
        assert_eq!(m, Mismatch::Stopping(OLD));
        m.stopped(Ok(()));
        assert_eq!(m, Mismatch::Starting(OLD));
        assert!(!m.restart());
        let notice = m.connected().expect("the restart is reported");
        assert!(notice.contains(&format!("协议 v{} → v{PROTOCOL_VERSION}", OLD.daemon)));
        assert!(notice.contains("休眠 / 已恢复"), "{notice}");
        assert_eq!(m, Mismatch::None);
        assert_eq!(m.banner(), None);
        assert_eq!(m.connected(), None, "reported once");
    }

    #[test]
    fn a_failed_stop_keeps_its_error_and_offers_a_retry() {
        let mut m = seen();
        assert!(m.restart());
        m.stopped(Err("berthd did not exit within 6 s".into()));
        let b = m.banner().unwrap();
        assert_eq!(b.title, "重启 berthd 失败");
        assert!(b.body.contains("did not exit within 6 s"), "{}", b.body);
        assert_eq!(b.button, Some("重试"));
        m.connect_failed(Some(OLD)); // the periodic retry meets it again
        assert!(matches!(m, Mismatch::Failed { .. }), "{m:?}");
        assert!(m.restart());
        assert_eq!(m, Mismatch::Stopping(OLD));
    }

    #[test]
    fn a_new_berthd_that_still_differs_is_a_failure_not_a_loop() {
        let mut m = seen();
        assert!(m.restart());
        m.stopped(Ok(()));
        m.connect_failed(Some(OLD));
        let b = m.banner().unwrap();
        assert_eq!(b.title, "重启 berthd 失败");
        assert!(b.body.contains("berth doctor"), "{}", b.body);
    }

    #[test]
    fn other_failures_and_connections_end_it() {
        let mut m = seen();
        m.connect_failed(None);
        assert_eq!(m, Mismatch::None);
        let mut m = seen();
        assert_eq!(
            m.connected(),
            None,
            "restarted elsewhere: nothing to report"
        );
        assert_eq!(m, Mismatch::None);
        let mut m = Mismatch::None;
        m.stopped(Ok(()));
        assert_eq!(m, Mismatch::None, "no stop was asked for");
    }
}
