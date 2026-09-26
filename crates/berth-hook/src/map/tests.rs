use super::*;

fn ctx() -> Context {
    Context {
        berth_session: Some(SessionId::nil()),
        pid: 7,
        now_ms: 1_000,
    }
}

fn claude_hook(json: &str) -> ClaudeHook {
    match claude(json.as_bytes(), &ctx()).expect("mapped").signal {
        AgentSignal::Claude(h) => h,
        other => panic!("unexpected {other:?}"),
    }
}

// Verbatim example from the hooks reference ("Common input fields").
const PRE_TOOL_USE: &str = r#"{
  "session_id": "abc123",
  "prompt_id": "550e8400-e29b-41d4-a716-446655440000",
  "transcript_path": "/home/user/.claude/projects/.../transcript.jsonl",
  "cwd": "/home/user/my-project",
  "scratchpad_dir": "/tmp/claude-1000/-home-user-my-project/abc123/scratchpad",
  "permission_mode": "default",
  "hook_event_name": "PreToolUse",
  "tool_name": "Bash",
  "tool_input": {
    "command": "npm test",
    "description": "Run test suite",
    "timeout": 120000,
    "run_in_background": false
  },
  "tool_use_id": "toolu_01ABC123..."
}"#;

#[test]
fn claude_pre_tool_use_sample() {
    let env = claude(PRE_TOOL_USE.as_bytes(), &ctx()).unwrap();
    assert_eq!(env.berth_session, Some(SessionId::nil()));
    assert_eq!((env.pid, env.sent_at_ms), (7, 1_000));
    let AgentSignal::Claude(h) = env.signal else {
        panic!()
    };
    assert_eq!(
        h,
        ClaudeHook {
            session_id: "abc123".into(),
            cwd: Some("/home/user/my-project".into()),
            transcript_path: Some("/home/user/.claude/projects/.../transcript.jsonl".into()),
            permission_mode: Some("default".into()),
            event: ClaudeHookEvent::PreToolUse {
                tool_name: "Bash".into()
            },
        }
    );
    // tool_input never makes it into the envelope.
    assert!(!format!("{h:?}").contains("npm test"));
}

#[test]
fn claude_notification_sample() {
    // Verbatim "Notification input" example.
    let h = claude_hook(
        r#"{
  "session_id": "abc123",
  "transcript_path": "/Users/.../.claude/projects/.../00893aaf-19fa-41d2-8238-13269b9b3ca0.jsonl",
  "cwd": "/Users/...",
  "hook_event_name": "Notification",
  "message": "Claude needs your permission",
  "title": "Permission needed",
  "notification_type": "permission_prompt"
}"#,
    );
    assert_eq!(
        h.event,
        ClaudeHookEvent::Notification {
            notification_type: Some("permission_prompt".into()),
            message: "Claude needs your permission".into(),
        }
    );
    assert_eq!(h.permission_mode, None);
}

#[test]
fn claude_other_events() {
    let base = |name: &str, extra: &str| {
        claude_hook(&format!(
            r#"{{"session_id":"s","cwd":"/w","hook_event_name":"{name}"{extra}}}"#
        ))
        .event
    };
    assert_eq!(
        base(
            "SessionStart",
            r#","source":"resume","model":"claude-opus-5""#
        ),
        ClaudeHookEvent::SessionStart {
            source: Some("resume".into())
        }
    );
    assert_eq!(
        base("UserPromptSubmit", r#","prompt":"secret prompt""#),
        ClaudeHookEvent::UserPromptSubmit
    );
    assert_eq!(
        base(
            "PostToolUse",
            r#","tool_name":"Write","tool_response":{"filePath":"/x"}"#
        ),
        ClaudeHookEvent::PostToolUse {
            tool_name: "Write".into()
        }
    );
    assert_eq!(
        base(
            "Stop",
            r#","stop_hook_active":true,"last_assistant_message":"done""#
        ),
        ClaudeHookEvent::Stop {
            stop_hook_active: true
        }
    );
    assert_eq!(
        base("Stop", ""),
        ClaudeHookEvent::Stop {
            stop_hook_active: false
        }
    );
    assert_eq!(
        base("SubagentStop", r#","agent_id":"def456""#),
        ClaudeHookEvent::SubagentStop
    );
    assert_eq!(
        base(
            "PreCompact",
            r#","trigger":"auto","custom_instructions":null"#
        ),
        ClaudeHookEvent::PreCompact {
            trigger: Some("auto".into())
        }
    );
    assert_eq!(
        base("SessionEnd", r#","reason":"prompt_input_exit""#),
        ClaudeHookEvent::SessionEnd {
            reason: Some("prompt_input_exit".into())
        }
    );
    assert_eq!(
        base("PermissionRequest", r#","tool_name":"Bash""#),
        ClaudeHookEvent::Other {
            hook_event_name: "PermissionRequest".into()
        }
    );
    // The prompt text is never forwarded.
    let env = claude(
        br#"{"session_id":"s","hook_event_name":"UserPromptSubmit","prompt":"top secret"}"#,
        &ctx(),
    )
    .unwrap();
    assert!(!format!("{env:?}").contains("top secret"));
}

#[test]
fn claude_rejects_non_hook_json() {
    assert!(claude(b"not json", &ctx()).is_none());
    assert!(claude(br#"{"session_id":"s"}"#, &ctx()).is_none());
    let long = "x".repeat(10_000);
    let h = claude_hook(&format!(
        r#"{{"session_id":"s","hook_event_name":"Notification","message":"{long}"}}"#
    ));
    let ClaudeHookEvent::Notification {
        message,
        notification_type,
    } = h.event
    else {
        panic!()
    };
    assert_eq!(message.len(), MAX_TEXT);
    assert_eq!(notification_type, None);
}

#[test]
fn codex_notify_sample() {
    // Wire shape asserted by openai/codex legacy_notify.rs tests.
    let json = r#"{"type":"agent-turn-complete","thread-id":"b5f6c1c2-1111-2222-3333-444455556666","turn-id":"12345","cwd":"/Users/example/project","client":"codex-tui","input-messages":["Rename `foo` to `bar` and update the callsites."],"last-assistant-message":"Rename complete and verified `cargo build` succeeds."}"#;
    let env = codex(json, &ctx()).unwrap();
    let AgentSignal::Codex(n) = env.signal else {
        panic!()
    };
    assert_eq!(
        n,
        CodexNotify {
            event_type: "agent-turn-complete".into(),
            thread_id: Some("b5f6c1c2-1111-2222-3333-444455556666".into()),
            cwd: Some("/Users/example/project".into()),
            last_message: Some("Rename complete and verified `cargo build` succeeds.".into()),
        }
    );
    // User prompts (`input-messages`) are dropped.
    assert!(!format!("{n:?}").contains("Rename `foo`"));

    let snake = codex(r#"{"type":"agent-turn-complete","thread_id":"t1"}"#, &ctx()).unwrap();
    let AgentSignal::Codex(n) = snake.signal else {
        panic!()
    };
    assert_eq!(n.thread_id.as_deref(), Some("t1"));
    assert!(codex(r#"{"thread-id":"t1"}"#, &ctx()).is_none());
}

#[test]
fn statusline_sample() {
    // Trimmed from the statusline "Full JSON schema" example.
    let json = br#"{
    "cwd": "/current/working/directory",
    "session_id": "abc123...",
    "transcript_path": "/path/to/transcript.jsonl",
    "model": { "id": "claude-opus-5-5", "display_name": "Opus" },
    "workspace": {
      "current_dir": "/current/working/directory",
      "project_dir": "/original/project/directory",
      "added_dirs": []
    },
    "version": "2.1.90",
    "cost": { "total_cost_usd": 0.01234, "total_duration_ms": 45000 },
    "context_window": { "context_window_size": 200000, "used_percentage": 8, "remaining_percentage": 92 },
    "exceeds_200k_tokens": false
  }"#;
    let env = statusline(json, &ctx()).unwrap();
    let AgentSignal::Statusline(s) = env.signal else {
        panic!()
    };
    assert_eq!(
        s,
        StatuslineUpdate {
            session_id: "abc123...".into(),
            model: Some("claude-opus-5-5".into()),
            context_pct: Some(8.0),
            cost_usd: Some(0.01234),
            project_dir: Some("/original/project/directory".into()),
            cwd: Some("/current/working/directory".into()),
        }
    );
    // used_percentage may be null early in a session.
    let early = statusline(
        br#"{"session_id":"s","context_window":{"used_percentage":null},"workspace":{"current_dir":"/w"}}"#,
        &ctx(),
    )
    .unwrap();
    let AgentSignal::Statusline(s) = early.signal else {
        panic!()
    };
    assert_eq!((s.context_pct, s.cwd), (None, Some("/w".into())));
    assert!(statusline(b"[1,2]", &ctx()).is_none());
}

#[test]
fn codex_and_statusline_argv() {
    let v = |xs: &[&str]| xs.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    assert_eq!(
        parse_codex_args(&v(&[r#"{"type":"x"}"#])),
        CodexArgs {
            json: Some(r#"{"type":"x"}"#.into()),
            chain: vec![]
        }
    );
    assert_eq!(
        parse_codex_args(&v(&[
            "--chain",
            "/usr/bin/notifier",
            "--flag",
            r#"{"type":"x"}"#
        ])),
        CodexArgs {
            json: Some(r#"{"type":"x"}"#.into()),
            chain: v(&["/usr/bin/notifier", "--flag"])
        }
    );
    assert_eq!(
        parse_codex_args(&v(&["--", "notifier", "plain"])),
        CodexArgs {
            json: None,
            chain: v(&["notifier", "plain"])
        }
    );
    assert_eq!(
        parse_codex_args(&[]),
        CodexArgs {
            json: None,
            chain: vec![]
        }
    );
    assert_eq!(
        parse_statusline_args(&v(&["--", "bash", "x.sh"])),
        v(&["bash", "x.sh"])
    );
    assert_eq!(
        parse_statusline_args(&v(&["bash", "x.sh"])),
        v(&["bash", "x.sh"])
    );
}
