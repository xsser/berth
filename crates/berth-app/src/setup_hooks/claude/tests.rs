use super::*;

/// A user's settings: env, permissions, a hook with a matcher, an empty
/// event list, a status line, mixed formatting (inline arrays / objects).
pub(crate) const SETTINGS: &str = r#"{
  "$schema": "https://json.schemastore.org/claude-code-settings.json",
  "env": {
    "ANTHROPIC_API_KEY_HELPER_TTL_MS": "3600000",
    "DISABLE_TELEMETRY": "1"
  },
  "permissions": {
    "allow": ["Bash(npm run lint)", "Read(~/.zshrc)"],
    "deny": ["Read(./.env)"]
  },
  "hooks": {
    "PreToolUse": [
      {
        "matcher": "Bash",
        "hooks": [
          { "type": "command", "command": "~/.claude/check.sh", "timeout": 5 }
        ]
      }
    ],
    "Stop": []
  },
  "statusLine": {"type": "command", "command": "~/.claude/statusline.sh", "padding": 0},
  "model": "opus"
}
"#;

fn hook(path: &str) -> Hook {
    Hook { path: path.into() }
}

fn value(text: &str) -> Value {
    serde_json::from_str(text).unwrap()
}

fn berth_commands(text: &str) -> Vec<(String, String)> {
    let v = value(text);
    let mut out = Vec::new();
    for (event, entries) in v["hooks"].as_object().unwrap() {
        for e in entries.as_array().unwrap() {
            for h in e["hooks"].as_array().unwrap() {
                let cmd = h["command"].as_str().unwrap();
                if is_berth_claude(cmd) {
                    out.push((event.clone(), cmd.to_owned()));
                }
            }
        }
    }
    out
}

#[test]
fn install_adds_all_events_and_keeps_everything_else_byte_for_byte() {
    let h = hook("/opt/berth/bin/berth-hook");
    let out = install(SETTINGS, &h, false).unwrap();
    let cmds = berth_commands(&out.text);
    assert_eq!(cmds.len(), EVENTS.len());
    for event in EVENTS {
        assert!(cmds
            .iter()
            .any(|(e, c)| e == event && c == "/opt/berth/bin/berth-hook claude"));
    }
    // Only insertions: every original line is still there, in order (the
    // empty `Stop` list is opened up: `"Stop": [`), and the removal
    // restores the exact bytes.
    let mut rest = out.text.as_str();
    for line in SETTINGS.lines() {
        let line = line
            .strip_suffix("[]")
            .map_or(line.to_owned(), |l| format!("{l}["));
        let at = rest
            .find(&line)
            .unwrap_or_else(|| panic!("lost line {line:?}"));
        rest = &rest[at + line.len()..];
    }
    let back = uninstall(&out.text, Some(&value(SETTINGS))).unwrap();
    assert_eq!(back.text, SETTINGS);
    // The user's own hook still comes first for PreToolUse.
    let v = value(&out.text);
    assert_eq!(v["hooks"]["PreToolUse"][0]["matcher"], "Bash");
    assert_eq!(v["hooks"]["PreToolUse"][1]["hooks"][0]["type"], "command");
    assert!(v["hooks"]["PreToolUse"][1].get("matcher").is_none());
    assert_eq!(v["statusLine"]["command"], "~/.claude/statusline.sh");
}

#[test]
fn install_is_idempotent_and_moves_stale_paths() {
    let a = install(SETTINGS, &hook("/a/berth-hook"), false).unwrap();
    let again = install(&a.text, &hook("/a/berth-hook"), false).unwrap();
    assert_eq!(again.text, a.text);
    assert!(again.notes.is_empty(), "{:?}", again.notes);
    let b = install(&a.text, &hook("/b b/berth-hook"), false).unwrap();
    let cmds = berth_commands(&b.text);
    assert_eq!(cmds.len(), EVENTS.len(), "no duplicates");
    assert!(cmds.iter().all(|(_, c)| c == "'/b b/berth-hook' claude"));
    assert!(b.notes[0].contains("18 个事件"), "{:?}", b.notes);
    // Nothing but the path changed.
    assert_eq!(b.text.lines().count(), a.text.lines().count());
}

#[test]
fn install_creates_the_file_content_from_nothing() {
    let out = install("{}\n", &hook("/x/berth-hook"), false).unwrap();
    assert!(out.text.starts_with("{\n  \"hooks\": {\n    \"SessionStart\": [\n      {\n        \"hooks\": [\n          {\n            \"type\": \"command\",\n            \"command\": \"/x/berth-hook claude\"\n          }\n        ]\n      }\n    ],\n"), "{}", out.text);
    assert!(out.text.ends_with("}\n"));
    assert_eq!(berth_commands(&out.text).len(), 18);
    // A compact file stays compact.
    let out = install(r#"{"model":"opus"}"#, &hook("/x/berth-hook"), false).unwrap();
    assert_eq!(out.text.lines().count(), 1);
    assert_eq!(berth_commands(&out.text).len(), 18);
}

#[test]
fn install_refuses_what_it_cannot_edit_safely() {
    let h = hook("/x/berth-hook");
    let err = install("{\n  \"a\": 1,\n}", &h, false).unwrap_err();
    assert!(err.contains("第 3 行"), "{err}");
    assert!(install("[]", &h, false).unwrap_err().contains("顶层"));
    assert!(install(r#"{"hooks": []}"#, &h, false)
        .unwrap_err()
        .contains("hooks"));
    assert!(install(r#"{"hooks": {"Stop": {}}}"#, &h, false)
        .unwrap_err()
        .contains("Stop"));
    let dup = r#"{"hooks": {"Stop": []}, "hooks": {}}"#;
    assert!(install(dup, &h, false).unwrap_err().contains("不止一次"));
    let dup = r#"{"hooks": {"Stop": [], "Stop": []}}"#;
    assert!(install(dup, &h, false).unwrap_err().contains("不止一次"));
    // Error messages never quote the file.
    let secret = "{\"env\": {\"TOKEN\": \"sk-secret-value\"},}";
    assert!(!install(secret, &h, false)
        .unwrap_err()
        .contains("sk-secret"));
}

#[test]
fn statusline_is_wrapped_once_and_unwrapped_exactly() {
    let h = hook("/x/berth-hook");
    let out = install(SETTINGS, &h, true).unwrap();
    let v = value(&out.text);
    assert_eq!(
        v["statusLine"]["command"],
        "/x/berth-hook statusline -- ~/.claude/statusline.sh"
    );
    assert_eq!(v["statusLine"]["padding"], 0);
    assert!(statusline_installed(&out.text));
    let again = install(&out.text, &h, true).unwrap();
    assert_eq!(again.text, out.text);
    let moved = install(&out.text, &hook("/y/berth-hook"), true).unwrap();
    assert_eq!(
        value(&moved.text)["statusLine"]["command"],
        "/y/berth-hook statusline -- ~/.claude/statusline.sh"
    );
    let back = uninstall(&out.text, Some(&value(SETTINGS))).unwrap();
    assert_eq!(back.text, SETTINGS);

    // Shell syntax runs through sh -c, like Claude Code would run it.
    let text = r#"{"statusLine": {"type": "command", "command": "cd ~ && ./s.sh | head -1"}}"#;
    let out = install(text, &h, true).unwrap();
    let cmd = value(&out.text)["statusLine"]["command"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(
        cmd,
        "/x/berth-hook statusline -- sh -c 'cd ~ && ./s.sh | head -1'"
    );
    assert_eq!(
        unwrap_statusline(&cmd).as_deref(),
        Some("cd ~ && ./s.sh | head -1")
    );
    assert!(out.notes.iter().any(|n| n.contains("sh -c")));
    // No status line: nothing to wrap, said so.
    let out = install("{}", &h, true).unwrap();
    assert!(out.notes.iter().any(|n| n.contains("没有 statusLine")));
}

#[test]
fn wrap_and_unwrap_are_inverse() {
    let h = hook("/p q/berth-hook");
    for original in [
        "~/.claude/statusline.sh",
        "npx -y ccusage statusline",
        "sh -c 'echo hi'",
        "FOO=1 bar",
        "a | b",
        "printf '%s' \"$(date)\"",
    ] {
        let wrapped = wrap_statusline(original, &h).unwrap();
        assert!(wrapped.starts_with("'/p q/berth-hook' statusline -- "));
        assert_eq!(
            unwrap_statusline(&wrapped).as_deref(),
            Some(original),
            "{wrapped}"
        );
    }
    assert!(wrap_statusline("  ", &h).is_none());
    assert!(unwrap_statusline("/x/berth-hook claude").is_none());
}

#[test]
fn uninstall_keeps_later_changes_and_mixed_groups() {
    let h = hook("/x/berth-hook");
    let installed = install(SETTINGS, &h, false).unwrap().text;
    // The user later adds a handler of their own next to berth's.
    let mut v = value(&installed);
    v["hooks"]["Stop"][0]["hooks"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::json!({"type": "command", "command": "say done"}));
    v["model"] = "sonnet".into();
    let edited = serde_json::to_string_pretty(&v).unwrap();
    let out = uninstall(&edited, Some(&value(SETTINGS))).unwrap();
    let after = value(&out.text);
    assert!(berth_commands(&out.text).is_empty());
    assert_eq!(after["model"], "sonnet");
    assert_eq!(after["hooks"]["Stop"][0]["hooks"][0]["command"], "say done");
    assert!(after["hooks"].get("SessionStart").is_none());
    assert!(out.notes[0].contains("删除 18 个"), "{:?}", out.notes);
    // Nothing of berth's: unchanged.
    let none = uninstall(SETTINGS, None).unwrap();
    assert_eq!(none.text, SETTINGS);
    assert!(none.notes.is_empty());
    // Without a reference, a hooks object left empty goes.
    let fresh = install("{}\n", &h, false).unwrap().text;
    assert_eq!(uninstall(&fresh, None).unwrap().text, "{}\n");
}

#[test]
fn installed_hook_is_found_for_undo() {
    let text = install(SETTINGS, &hook("/p q/berth-hook"), true)
        .unwrap()
        .text;
    assert_eq!(installed_hook(&text), Some(hook("/p q/berth-hook")));
    assert_eq!(installed_hook(SETTINGS), None);
    assert!(!statusline_installed(SETTINGS));
}

/// Review high: only exactly `<berth-hook> claude` is berth's entry. A
/// command of the user's that also runs berth-hook is not installed, not
/// rewritten when the path changes, kept by the removal, and the self-check
/// refuses an edit to it.
#[test]
fn a_users_own_berth_hook_command_is_never_berths_entry() {
    let users = "/old/berth-hook claude && echo user-added-this";
    for other in [
        users,
        "FOO=1 /old/berth-hook claude",
        "/old/berth-hook claude > /dev/null",
        "/old/berth-hook claude extra",
        "/old/berth-hook claude; true",
        // Two words whose first is named berth-hook: only the compound
        // syntax inside it tells these apart from berth's entry.
        "true&&/old/berth-hook claude",
        "$(true)/old/berth-hook claude",
    ] {
        assert!(!is_berth_claude(other), "{other}");
    }
    assert!(is_berth_claude("'/p q/berth-hook' claude"));
    let text = r#"{"hooks": {
  "Stop": [{"hooks": [
    {"type": "command", "command": "USERS"},
    {"type": "command", "command": "/old/berth-hook claude"}
  ]}],
  "SessionEnd": [{"hooks": [{"type": "command", "command": "USERS"}]}]
}}
"#
    .replace("USERS", users);
    let found = installed(&text).unwrap();
    assert!(found.events.contains(&"Stop"), "{found:?}");
    assert!(found.missing.contains(&"SessionEnd"), "{found:?}");
    assert_eq!(found.hooks, ["/old/berth-hook"]);

    let out = install(&text, &hook("/new/berth-hook"), false).unwrap();
    assert_eq!(out.text.matches(users).count(), 2, "{}", out.text);
    let cmds = berth_commands(&out.text);
    assert_eq!(cmds.len(), EVENTS.len());
    assert!(cmds.iter().all(|(_, c)| c == "/new/berth-hook claude"));
    let v = value(&out.text);
    assert_eq!(v["hooks"]["SessionEnd"][0]["hooks"][0]["command"], users);
    assert!(
        out.notes
            .iter()
            .any(|n| n == "Stop：/old/berth-hook claude 改为 /new/berth-hook claude"),
        "{:?}",
        out.notes
    );
    assert!(
        out.notes
            .iter()
            .any(|n| n.starts_with("Stop, SessionEnd 里有用户自己写的 berth-hook 命令")),
        "{:?}",
        out.notes
    );
    let back = uninstall(&out.text, None).unwrap();
    assert_eq!(back.text.matches(users).count(), 2, "{}", back.text);
    assert!(berth_commands(&back.text).is_empty());
    // An edit that took the user's command for berth's is refused.
    let cmd = "/new/berth-hook claude";
    let taken = out.text.replacen(users, cmd, 1);
    assert!(verify_install(&value(&text), &taken, cmd, false).is_err());
}

/// Review high, status line: only the exact wrapper `wrap_statusline`
/// makes is berth's. Anything else running berth-hook is the user's: not
/// installed, never wrapped again or rewritten, kept by the removal.
#[test]
fn a_status_line_the_user_runs_through_berth_hook_is_left_alone() {
    for users in [
        "/x/berth-hook statusline -- ./s.sh && echo user-added-this",
        "FOO=1 /x/berth-hook statusline -- ./s.sh",
    ] {
        let text = format!(
            r#"{{"statusLine": {{"type": "command", "command": {}}}}}"#,
            serde_json::to_string(users).unwrap()
        );
        assert_eq!(unwrap_statusline(users), None, "{users}");
        assert!(!statusline_installed(&text), "{users}");
        let out = install(&text, &hook("/y/berth-hook"), true).unwrap();
        assert_eq!(value(&out.text)["statusLine"]["command"], users);
        assert!(
            out.notes.iter().any(|n| n.contains("不再包装")),
            "{:?}",
            out.notes
        );
        let back = uninstall(&out.text, None).unwrap();
        assert_eq!(value(&back.text)["statusLine"]["command"], users);
    }
}

/// The self-check behind every edit refuses results that change anything
/// but berth's entries.
#[test]
fn verification_refuses_collateral_changes() {
    let h = hook("/x/berth-hook");
    let cmd = h.claude_command();
    let good = install(SETTINGS, &h, false).unwrap().text;
    let before = value(SETTINGS);
    assert!(verify_install(&before, &good, &cmd, false).is_ok());
    let collateral = good.replace("\"opus\"", "\"sonnet\"");
    assert!(verify_install(&before, &collateral, &cmd, false).is_err());
    let lost_event = good.replacen("\"CwdChanged\"", "\"CwdChangedX\"", 1);
    assert!(verify_install(&before, &lost_event, &cmd, false).is_err());
    assert!(verify_install(&before, "{", &cmd, false).is_err());
}
