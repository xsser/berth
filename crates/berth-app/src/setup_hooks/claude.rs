//! `~/.claude/settings.json`: one matcher-less entry per hook event running
//! `<berth-hook> claude`, optionally the status line through
//! `<berth-hook> statusline -- <command>`. Edits are made in place (see
//! `json_span`), then checked: the result must parse and, berth's entries
//! aside, mean exactly what the original meant.

use serde_json::Value;

use super::json_span::{self, Edits, Layout, Node, J};
use super::shell;

/// The hook events of the Claude Code hooks reference (2026-09-27), in its
/// order. berth-hook never prints a decision, so the ones that could decide
/// (PreToolUse, PermissionRequest, ...) take the normal permission flow.
pub const EVENTS: [&str; 18] = [
    "SessionStart",
    "UserPromptSubmit",
    "PreToolUse",
    "PermissionRequest",
    "PermissionDenied",
    "PostToolUse",
    "PostToolUseFailure",
    "Notification",
    "SubagentStart",
    "SubagentStop",
    "PreCompact",
    "PostCompact",
    "Stop",
    "StopFailure",
    "Elicitation",
    "ElicitationResult",
    "CwdChanged",
    "SessionEnd",
];

/// The berth-hook executable, as written into commands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hook {
    pub path: String,
}

impl Hook {
    /// The hook command (run by `sh -c`).
    pub fn claude_command(&self) -> String {
        format!("{} claude", shell::quote(&self.path))
    }

    fn statusline_prefix(&self) -> String {
        format!("{} statusline --", shell::quote(&self.path))
    }
}

#[derive(Debug)]
pub struct Change {
    pub text: String,
    pub notes: Vec<String>,
}

/// serde_json's view of `text`; errors carry only a position (the message
/// never quotes the file, which may hold secrets).
pub fn check(text: &str) -> Result<Value, String> {
    serde_json::from_str(text)
        .map_err(|e| format!("不是有效的 JSON（第 {} 行第 {} 列）", e.line(), e.column()))
}

/// Whether `command` runs `berth-hook claude`.
pub fn is_berth_claude(command: &str) -> bool {
    shell::split(command).is_some_and(|w| {
        w.words.len() >= 2 && shell::is_berth_hook(&w.words[0].text) && w.words[1].text == "claude"
    })
}

fn is_berth_statusline(command: &str) -> bool {
    shell::split(command).is_some_and(|w| {
        w.words.len() >= 2
            && shell::is_berth_hook(&w.words[0].text)
            && w.words[1].text == "statusline"
    })
}

/// `command` run through berth-hook: as is when it is a plain command
/// (berth-hook runs it as argv), else via `sh -c` (as Claude Code would
/// run it). `None`: nothing to run.
pub fn wrap_statusline(command: &str, hook: &Hook) -> Option<String> {
    let words = shell::split(command)?;
    if words.words.is_empty() {
        return None;
    }
    Some(if shell::is_plain_command(&words) {
        format!("{} {command}", hook.statusline_prefix())
    } else {
        format!(
            "{} sh -c {}",
            hook.statusline_prefix(),
            shell::quote(command)
        )
    })
}

/// The original of a status line command `wrap_statusline` produced (with
/// any berth-hook path). `None`: not in that form.
pub fn unwrap_statusline(command: &str) -> Option<String> {
    let words = shell::split(command)?;
    let w = &words.words;
    if w.len() < 4 || !shell::is_berth_hook(&w[0].text) || w[1].text != "statusline" {
        return None;
    }
    if w[2].text != "--" {
        return None;
    }
    let hook = Hook {
        path: w[0].text.clone(),
    };
    if w.len() == 6 && w[3].text == "sh" && w[4].text == "-c" {
        let inner = w[5].text.clone();
        if wrap_statusline(&inner, &hook).as_deref() == Some(command) {
            return Some(inner);
        }
    }
    command.get(w[2].span.end + 1..).map(str::to_owned)
}

/// The berth-hook path of the first berth hook entry, if any (to redo an
/// installation exactly when undoing).
pub fn installed_hook(text: &str) -> Option<Hook> {
    let root = json_span::parse(text).ok()?;
    for member in root.get("hooks")?.members()? {
        for entry in member.value.items().unwrap_or_default() {
            for handler in handlers(entry) {
                if let Some(cmd) = handler.get("command").and_then(Node::as_str) {
                    if is_berth_claude(cmd) {
                        let first = shell::split(cmd)?.words.into_iter().next()?;
                        return Some(Hook { path: first.text });
                    }
                }
            }
        }
    }
    None
}

/// Whether the status line command goes through berth-hook.
pub fn statusline_installed(text: &str) -> bool {
    json_span::parse(text).ok().is_some_and(|root| {
        root.get("statusLine")
            .and_then(|s| s.get("command"))
            .and_then(Node::as_str)
            .is_some_and(is_berth_statusline)
    })
}

fn handlers(entry: &Node) -> &[Node] {
    entry.get("hooks").and_then(Node::items).unwrap_or_default()
}

fn entry(command: &str) -> J {
    J::Obj(vec![(
        "hooks".into(),
        J::Arr(vec![J::Obj(vec![
            ("type".into(), J::Str("command".into())),
            ("command".into(), J::Str(command.into())),
        ])]),
    )])
}

fn refuse_duplicates(node: &Node, what: &str, keys: &[&str]) -> Result<(), String> {
    match node.duplicate_keys().iter().find(|k| keys.contains(k)) {
        Some(k) => Err(format!(
            "{what}里 \"{k}\" 出现了不止一次；请先手动合并，berth 不猜测哪一个生效"
        )),
        None => Ok(()),
    }
}

/// `text` with berth's hook entries (and, with `statusline`, the wrapped
/// status line). Idempotent: present entries are kept, entries with another
/// berth-hook path are pointed at `hook`.
pub fn install(text: &str, hook: &Hook, statusline: bool) -> Result<Change, String> {
    let before = check(text)?;
    let root = json_span::parse(text)?;
    if root.members().is_none() {
        return Err("顶层不是 JSON 对象".into());
    }
    refuse_duplicates(&root, "顶层", &["hooks", "statusLine"])?;
    let layout = Layout::of(text, &root);
    let command = hook.claude_command();
    let mut edits = Edits::default();
    let mut notes = Vec::new();

    match root.get("hooks") {
        None => {
            let events = EVENTS
                .iter()
                .map(|e| (e.to_string(), J::Arr(vec![entry(&command)])))
                .collect();
            json_span::append_members(
                &mut edits,
                text,
                &layout,
                &root,
                &[("hooks".into(), J::Obj(events))],
            );
        }
        Some(hooks) if hooks.members().is_none() => {
            return Err("\"hooks\" 不是 JSON 对象".into());
        }
        Some(hooks) => {
            refuse_duplicates(hooks, "\"hooks\" ", &EVENTS)?;
            let mut missing = Vec::new();
            let mut updated = Vec::new();
            for event in EVENTS {
                let Some(list) = hooks.get(event) else {
                    missing.push((event.to_string(), J::Arr(vec![entry(&command)])));
                    continue;
                };
                let Some(entries) = list.items() else {
                    return Err(format!("\"hooks\".\"{event}\" 不是数组"));
                };
                let berth: Vec<&Node> = entries
                    .iter()
                    .flat_map(handlers)
                    .filter_map(|h| h.get("command"))
                    .filter(|c| c.as_str().is_some_and(is_berth_claude))
                    .collect();
                if berth.iter().any(|c| c.as_str() == Some(command.as_str())) {
                    continue;
                }
                match berth.first() {
                    Some(stale) => {
                        json_span::replace_string(&mut edits, stale, &command);
                        updated.push(event);
                    }
                    None => {
                        json_span::append_item(&mut edits, text, &layout, list, &entry(&command))
                    }
                }
            }
            if !updated.is_empty() {
                notes.push(format!(
                    "{} 个事件已有指向其他路径的 berth-hook，改为指向当前路径：{}",
                    updated.len(),
                    updated.join(", ")
                ));
            }
            json_span::append_members(&mut edits, text, &layout, hooks, &missing);
        }
    }

    if statusline {
        statusline_edit(&root, hook, &mut edits, &mut notes)?;
    }
    if edits.is_empty() {
        return Ok(Change {
            text: text.to_owned(),
            notes,
        });
    }
    let after_text = edits.apply(text)?;
    verify_install(&before, &after_text, &command, statusline)?;
    Ok(Change {
        text: after_text,
        notes,
    })
}

fn statusline_edit(
    root: &Node,
    hook: &Hook,
    edits: &mut Edits,
    notes: &mut Vec<String>,
) -> Result<(), String> {
    let Some(status) = root.get("statusLine") else {
        notes.push("没有 statusLine：--statusline 无事可做".into());
        return Ok(());
    };
    if status.members().is_none() {
        return Err("\"statusLine\" 不是 JSON 对象".into());
    }
    refuse_duplicates(status, "\"statusLine\" ", &["command", "type"])?;
    if status
        .get("type")
        .and_then(Node::as_str)
        .is_some_and(|t| t != "command")
    {
        notes.push("statusLine 的 type 不是 command：--statusline 跳过".into());
        return Ok(());
    }
    let Some(node) = status.get("command") else {
        notes.push("statusLine 没有 command：--statusline 跳过".into());
        return Ok(());
    };
    let Some(current) = node.as_str() else {
        return Err("\"statusLine\".\"command\" 不是字符串".into());
    };
    let wanted = match unwrap_statusline(current) {
        // Already wrapped: only a different berth-hook path changes.
        Some(original) => wrap_statusline(&original, hook),
        None if is_berth_statusline(current) => {
            notes.push("statusLine 已经经过 berth-hook（非 berth 生成的写法）：保持不变".into());
            return Ok(());
        }
        None => {
            let wrapped = wrap_statusline(current, hook);
            if let Some(w) = &wrapped {
                if w.contains(" sh -c ") && !current.contains(" sh -c ") {
                    notes.push(
                        "statusLine 命令含管道/重定向/内建命令等 shell 语法：以 sh -c 包装，语义不变"
                            .into(),
                    );
                }
            }
            wrapped
        }
    };
    match wanted {
        Some(w) if w != current => json_span::replace_string(edits, node, &w),
        Some(_) => {}
        None => notes.push("statusLine.command 为空或引号不配对：--statusline 跳过".into()),
    }
    Ok(())
}

/// berth's entries removed and wrapped status lines unwrapped: what the
/// settings mean apart from berth. Empty lists and hook groups that only
/// held berth entries go too.
fn without_berth(v: &Value) -> Value {
    let mut v = v.clone();
    if let Some(hooks) = v.get_mut("hooks").and_then(Value::as_object_mut) {
        for list in hooks.values_mut() {
            if let Some(entries) = list.as_array_mut() {
                entries.retain_mut(|entry| {
                    let Some(hs) = entry.get_mut("hooks").and_then(Value::as_array_mut) else {
                        return true;
                    };
                    let before = hs.len();
                    hs.retain(|h| {
                        !h.get("command")
                            .and_then(Value::as_str)
                            .is_some_and(is_berth_claude)
                    });
                    !(before > 0 && hs.is_empty())
                });
            }
        }
        hooks.retain(|_, list| list.as_array().is_none_or(|a| !a.is_empty()));
    }
    if v.get("hooks")
        .and_then(Value::as_object)
        .is_some_and(|h| h.is_empty())
    {
        v.as_object_mut().map(|o| o.remove("hooks"));
    }
    if let Some(cmd) = v.get_mut("statusLine").and_then(|s| s.get_mut("command")) {
        if let Some(original) = cmd.as_str().and_then(unwrap_statusline) {
            *cmd = Value::String(original);
        }
    }
    v
}

fn verify_install(
    before: &Value,
    after_text: &str,
    command: &str,
    statusline: bool,
) -> Result<(), String> {
    let after = check(after_text).map_err(|e| format!("internal error: result {e}"))?;
    if without_berth(before) != without_berth(&after) {
        return Err("internal error: the edit would change more than berth's entries".into());
    }
    for event in EVENTS {
        let ok = after["hooks"][event].as_array().is_some_and(|entries| {
            entries.iter().any(|e| {
                e["hooks"]
                    .as_array()
                    .is_some_and(|hs| hs.iter().any(|h| h["command"] == command))
            })
        });
        if !ok {
            return Err(format!("internal error: {event} has no berth entry"));
        }
    }
    if statusline {
        if let Some(cmd) = after["statusLine"]["command"].as_str() {
            if !cmd.is_empty() && !is_berth_statusline(cmd) {
                return Err("internal error: status line not wrapped".into());
            }
        }
    }
    Ok(())
}

/// `text` without berth's hook entries and with the status line unwrapped.
/// An event list or `hooks` object left empty is removed when `reference`
/// (the settings before berth, if known) does not have it.
pub fn uninstall(text: &str, reference: Option<&Value>) -> Result<Change, String> {
    let before = check(text)?;
    let root = json_span::parse(text)?;
    if root.members().is_none() {
        return Err("顶层不是 JSON 对象".into());
    }
    refuse_duplicates(&root, "顶层", &["hooks", "statusLine"])?;
    let ref_hooks = reference.and_then(|r| r.get("hooks"));
    let mut edits = Edits::default();
    let mut removed = 0;
    if let Some(hooks) = root.get("hooks").filter(|h| h.members().is_some()) {
        let members = hooks.members().unwrap_or_default();
        let mut drop_events = Vec::new();
        let mut inner = Edits::default();
        for (mi, member) in members.iter().enumerate() {
            let Some(entries) = member.value.items() else {
                continue;
            };
            let mut drop_entries = Vec::new();
            let mut entry_edits = Edits::default();
            for (ei, e) in entries.iter().enumerate() {
                let hs = handlers(e);
                let berth: Vec<usize> = hs
                    .iter()
                    .enumerate()
                    .filter(|(_, h)| {
                        h.get("command")
                            .and_then(Node::as_str)
                            .is_some_and(is_berth_claude)
                    })
                    .map(|(i, _)| i)
                    .collect();
                removed += berth.len();
                if berth.is_empty() {
                    continue;
                }
                if berth.len() == hs.len() {
                    drop_entries.push(ei);
                } else if let Some(list) = e.get("hooks") {
                    json_span::remove_children(&mut entry_edits, list, &berth);
                }
            }
            let all_gone = !entries.is_empty() && drop_entries.len() == entries.len();
            let keep_key = ref_hooks.is_some_and(|h| h.get(&member.key).is_some());
            if all_gone && !keep_key {
                drop_events.push(mi);
                continue;
            }
            inner.extend(entry_edits);
            json_span::remove_children(&mut inner, &member.value, &drop_entries);
        }
        let hooks_gone = !members.is_empty() && drop_events.len() == members.len();
        let ref_has_hooks = reference.is_some_and(|r| r.get("hooks").is_some());
        if hooks_gone && !ref_has_hooks {
            let index = root
                .members()
                .unwrap_or_default()
                .iter()
                .rposition(|m| m.key == "hooks")
                .expect("hooks is a member");
            json_span::remove_children(&mut edits, &root, &[index]);
        } else {
            edits.extend(inner);
            json_span::remove_children(&mut edits, hooks, &drop_events);
        }
    }
    let mut notes = Vec::new();
    if let Some(node) = root.get("statusLine").and_then(|s| s.get("command")) {
        if let Some(original) = node.as_str().and_then(unwrap_statusline) {
            json_span::replace_string(&mut edits, node, &original);
            notes.push("statusLine 恢复为原命令".into());
        }
    }
    if removed > 0 {
        notes.insert(0, format!("删除 {removed} 个 berth-hook 条目"));
    }
    if edits.is_empty() {
        return Ok(Change {
            text: text.to_owned(),
            notes,
        });
    }
    let after_text = edits.apply(text)?;
    let after = check(&after_text).map_err(|e| format!("internal error: result {e}"))?;
    if without_berth(&before) != without_berth(&after) {
        return Err("internal error: the removal would change more than berth's entries".into());
    }
    Ok(Change {
        text: after_text,
        notes,
    })
}

#[cfg(test)]
pub(super) mod tests;
