//! Context menus (DESIGN §17.2): which entries a menu offers in the state
//! it is opened in, as plain data the sidebar draws and tests check. The
//! entry chosen comes back to the app as `UiAction::Menu` and is applied by
//! [`apply`]; what only the app can do (clipboard, the rename dialog) is
//! returned as an [`AppEffect`].
//!
//! Archive entries (「归档」, 「归档 session」) come with the archive UI.

use std::time::Instant;

use berth_core::{ReviveMode, SessionId, WorkspaceId};

use crate::controller::{Controller, Outbound};
use crate::panes::SplitDir;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MenuAction {
    /// Sidebar row: show the session in a new pane beside the focused one.
    OpenInSplit(SessionId, SplitDir),
    /// 「重命名…」 of a session: opens the rename dialog.
    RenameSession(SessionId),
    MarkRead(SessionId),
    /// 「恢复运行」 = Revive (a new shell in the session's directory).
    Revive(SessionId),
    NewSessionIn(WorkspaceId),
    RenameWorkspace(WorkspaceId),
    /// Asks first.
    DeleteWorkspace(WorkspaceId),
    /// Terminal area: the selection of the pane (the one right-clicked,
    /// which the click focused).
    Copy,
    Paste,
    /// ⌘D / ⌘⇧D on the focused pane.
    Split(SplitDir),
    /// Close the pane, keep the session.
    RemoveFromSplit(SessionId),
    /// 「关闭 pane」 = ⌘W on the pane's session.
    ClosePane(SessionId),
}

/// What a rename dialog renames.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RenameTarget {
    Session(SessionId),
    Workspace(WorkspaceId),
}

/// Menu results the app handles itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AppEffect {
    Copy,
    Paste,
    Rename(RenameTarget),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MenuItem {
    pub label: &'static str,
    pub action: MenuAction,
    pub enabled: bool,
    /// Why the entry is disabled (hover text).
    pub hint: Option<&'static str>,
}

fn item(label: &'static str, action: MenuAction) -> MenuItem {
    MenuItem {
        label,
        action,
        enabled: true,
        hint: None,
    }
}

fn disabled_unless(mut it: MenuItem, enabled: bool, hint: &'static str) -> MenuItem {
    it.enabled = enabled;
    it.hint = (!enabled).then_some(hint);
    it
}

/// A sidebar session row as its menu needs it.
#[derive(Clone, Copy, Debug)]
pub struct SessionRow {
    pub sid: SessionId,
    /// It has a pane already (one pane per session).
    pub shown: bool,
    pub unread: bool,
    pub live: bool,
}

pub const HINT_SHOWN: &str = "已在分屏中";
pub const HINT_WORKSPACE_NOT_EMPTY: &str = "先移走或彻底删除其中的 session";

pub fn session_row(r: SessionRow) -> Vec<MenuItem> {
    let mut v = vec![
        disabled_unless(
            item(
                "在右侧分屏打开",
                MenuAction::OpenInSplit(r.sid, SplitDir::Right),
            ),
            !r.shown,
            HINT_SHOWN,
        ),
        disabled_unless(
            item(
                "在下方分屏打开",
                MenuAction::OpenInSplit(r.sid, SplitDir::Down),
            ),
            !r.shown,
            HINT_SHOWN,
        ),
        item("重命名…", MenuAction::RenameSession(r.sid)),
        disabled_unless(
            item("标记已读", MenuAction::MarkRead(r.sid)),
            r.unread,
            "没有未读",
        ),
    ];
    if !r.live {
        v.push(item("恢复运行", MenuAction::Revive(r.sid)));
    }
    v
}

/// `has_sessions`: dormant ones count (they would be orphaned).
pub fn workspace_header(id: WorkspaceId, has_sessions: bool) -> Vec<MenuItem> {
    vec![
        item("新建 session", MenuAction::NewSessionIn(id)),
        item("重命名…", MenuAction::RenameWorkspace(id)),
        disabled_unless(
            item("删除 workspace…", MenuAction::DeleteWorkspace(id)),
            !has_sessions,
            HINT_WORKSPACE_NOT_EMPTY,
        ),
    ]
}

/// The terminal area of `sid`'s pane.
#[derive(Clone, Copy, Debug)]
pub struct TerminalArea {
    pub sid: SessionId,
    pub has_selection: bool,
    pub panes: usize,
}

pub fn terminal(t: TerminalArea) -> Vec<MenuItem> {
    let mut v = Vec::new();
    if t.has_selection {
        v.push(item("复制", MenuAction::Copy));
    }
    v.extend([
        item("粘贴", MenuAction::Paste),
        item("向右分屏", MenuAction::Split(SplitDir::Right)),
        item("向下分屏", MenuAction::Split(SplitDir::Down)),
        disabled_unless(
            item("从分屏移除", MenuAction::RemoveFromSplit(t.sid)),
            t.panes > 1,
            "只有一个 pane",
        ),
        item("关闭 pane", MenuAction::ClosePane(t.sid)),
        item("重命名…", MenuAction::RenameSession(t.sid)),
    ]);
    v
}

/// Carry out a menu entry.
pub fn apply(
    ctl: &mut Controller,
    out: &mut dyn Outbound,
    action: MenuAction,
    now: Instant,
) -> Option<AppEffect> {
    match action {
        MenuAction::OpenInSplit(sid, dir) => ctl.open_in_split(out, sid, dir, now),
        MenuAction::RenameSession(sid) => {
            return Some(AppEffect::Rename(RenameTarget::Session(sid)))
        }
        MenuAction::MarkRead(sid) => ctl.mark_read_now(out, sid),
        MenuAction::Revive(sid) => ctl.revive(out, sid, ReviveMode::Shell, now),
        MenuAction::NewSessionIn(ws) => ctl.new_session_in(out, ws),
        MenuAction::RenameWorkspace(id) => {
            return Some(AppEffect::Rename(RenameTarget::Workspace(id)))
        }
        MenuAction::DeleteWorkspace(id) => ctl.request_delete_workspace(id),
        MenuAction::Copy => return Some(AppEffect::Copy),
        MenuAction::Paste => return Some(AppEffect::Paste),
        MenuAction::Split(dir) => ctl.split(out, dir),
        MenuAction::RemoveFromSplit(sid) => {
            ctl.remove_from_split(out, sid, now);
        }
        MenuAction::ClosePane(sid) => {
            ctl.focus(out, sid, now);
            ctl.request_close(out);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels(items: &[MenuItem]) -> Vec<&'static str> {
        items.iter().map(|i| i.label).collect()
    }

    fn get<'a>(items: &'a [MenuItem], label: &str) -> &'a MenuItem {
        items.iter().find(|i| i.label == label).expect(label)
    }

    #[test]
    fn a_session_row_offers_splits_unless_it_is_shown() {
        let sid = SessionId::new();
        let row = SessionRow {
            sid,
            shown: false,
            unread: true,
            live: true,
        };
        let m = session_row(row);
        assert_eq!(
            labels(&m),
            ["在右侧分屏打开", "在下方分屏打开", "重命名…", "标记已读"]
        );
        assert!(m.iter().all(|i| i.enabled && i.hint.is_none()));
        assert_eq!(
            get(&m, "在下方分屏打开").action,
            MenuAction::OpenInSplit(sid, SplitDir::Down)
        );
        let m = session_row(SessionRow {
            shown: true,
            unread: false,
            live: false,
            ..row
        });
        assert_eq!(labels(&m).last(), Some(&"恢复运行"), "dormant: Revive");
        for label in ["在右侧分屏打开", "在下方分屏打开"] {
            let it = get(&m, label);
            assert!(!it.enabled);
            assert_eq!(it.hint, Some(HINT_SHOWN));
        }
        assert!(!get(&m, "标记已读").enabled, "nothing unread");
        assert!(get(&m, "重命名…").enabled);
    }

    #[test]
    fn a_workspace_with_sessions_cannot_be_deleted() {
        let id = WorkspaceId::new();
        let m = workspace_header(id, true);
        assert_eq!(labels(&m), ["新建 session", "重命名…", "删除 workspace…"]);
        let del = get(&m, "删除 workspace…");
        assert!(!del.enabled);
        assert_eq!(del.hint, Some(HINT_WORKSPACE_NOT_EMPTY));
        let del = workspace_header(id, false).pop().unwrap();
        assert!(del.enabled);
        assert_eq!(del.action, MenuAction::DeleteWorkspace(id));
    }

    #[test]
    fn the_terminal_menu_follows_the_selection_and_the_pane_count() {
        let sid = SessionId::new();
        let t = TerminalArea {
            sid,
            has_selection: false,
            panes: 1,
        };
        let m = terminal(t);
        assert_eq!(
            labels(&m),
            [
                "粘贴",
                "向右分屏",
                "向下分屏",
                "从分屏移除",
                "关闭 pane",
                "重命名…"
            ]
        );
        assert!(!get(&m, "从分屏移除").enabled, "the last pane stays");
        let m = terminal(TerminalArea {
            has_selection: true,
            panes: 2,
            ..t
        });
        assert_eq!(labels(&m)[0], "复制");
        assert!(m.iter().all(|i| i.enabled));
        assert_eq!(
            get(&m, "从分屏移除").action,
            MenuAction::RemoveFromSplit(sid)
        );
        assert_eq!(get(&m, "关闭 pane").action, MenuAction::ClosePane(sid));
    }
}
