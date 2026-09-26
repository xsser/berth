//! One test per row of the DESIGN §9 table, plus the priority rules.

use std::path::PathBuf;

use super::*;

const T0: i64 = 1_000_000;

fn hook(event: ClaudeHookEvent) -> Signal {
    Signal::Hook(ClaudeHook {
        session_id: "claude-uuid".into(),
        cwd: Some(PathBuf::from("/w")),
        transcript_path: Some(PathBuf::from("/t.jsonl")),
        permission_mode: None,
        event,
    })
}

fn notification(kind: &str) -> Signal {
    hook(ClaudeHookEvent::Notification {
        notification_type: Some(kind.into()),
        message: String::new(),
    })
}

fn pre(tool: &str) -> Signal {
    hook(ClaudeHookEvent::PreToolUse {
        tool_name: tool.into(),
    })
}

fn machine() -> AgentMachine {
    AgentMachine::new(AgentInfo::default())
}

/// A machine whose agent kind came from the foreground-process heuristic
/// (no hook ever seen).
fn heuristic_claude() -> AgentMachine {
    let mut m = machine();
    m.apply(&Signal::ForegroundProcess("claude".into()), T0)
        .unwrap();
    m
}

#[test]
fn row_session_start_sets_kind_ids_and_idle() {
    let mut m = machine();
    m.apply(&hook(ClaudeHookEvent::UserPromptSubmit), T0);
    let a = m
        .apply(
            &hook(ClaudeHookEvent::SessionStart {
                source: Some("startup".into()),
            }),
            T0 + 1,
        )
        .unwrap();
    assert_eq!(a.kind, "hook:SessionStart");
    assert_eq!(a.detail.as_deref(), Some("startup"));
    assert!(a.state_changed);
    let i = m.info();
    assert_eq!(i.kind, AgentKind::Claude);
    assert_eq!(i.external_id.as_deref(), Some("claude-uuid"));
    assert_eq!(i.transcript_path, Some(PathBuf::from("/t.jsonl")));
    assert_eq!(i.state, AgentState::Idle);
    assert_eq!(
        (i.source, i.confidence, i.since_ms),
        (StateSource::Hook, 1.0, T0 + 1)
    );
}

#[test]
fn row_user_prompt_submit_thinking() {
    let mut m = machine();
    let a = m
        .apply(&hook(ClaudeHookEvent::UserPromptSubmit), T0)
        .unwrap();
    assert_eq!(
        (a.kind.as_str(), a.state_changed),
        ("hook:UserPromptSubmit", true)
    );
    assert_eq!(m.info().state, AgentState::Thinking);
}

#[test]
fn row_pre_and_post_tool_use() {
    let mut m = machine();
    let a = m.apply(&pre("Bash"), T0).unwrap();
    assert_eq!(a.detail.as_deref(), Some("Bash"));
    assert_eq!(
        m.info().state,
        AgentState::ToolRunning {
            tool: "Bash".into()
        }
    );
    let a = m
        .apply(
            &hook(ClaudeHookEvent::PostToolUse {
                tool_name: "Bash".into(),
            }),
            T0 + 5,
        )
        .unwrap();
    assert_eq!(a.kind, "hook:PostToolUse");
    assert_eq!(m.info().state, AgentState::Thinking);
    assert_eq!(m.info().since_ms, T0 + 5);
}

#[test]
fn row_notification_permission_and_idle_prompt() {
    let mut m = machine();
    m.apply(&pre("Bash"), T0);
    let a = m.apply(&notification("permission_prompt"), T0 + 1).unwrap();
    assert_eq!(a.detail.as_deref(), Some("permission_prompt"));
    // The notification does not name the tool.
    assert_eq!(m.info().state, AgentState::WaitingPermission { tool: None });
    m.apply(&notification("idle_prompt"), T0 + 2);
    assert_eq!(m.info().state, AgentState::WaitingInput);
    // Informational notifications do not move the state.
    let a = m.apply(&notification("auth_success"), T0 + 3).unwrap();
    assert!(!a.state_changed);
    assert_eq!(m.info().state, AgentState::WaitingInput);
    // Legacy payloads without notification_type fall back to the message.
    let legacy = hook(ClaudeHookEvent::Notification {
        notification_type: None,
        message: "Claude needs your permission to use Bash".into(),
    });
    m.apply(&legacy, T0 + 4);
    assert!(matches!(
        m.info().state,
        AgentState::WaitingPermission { .. }
    ));
}

#[test]
fn row_stop_done_then_user_input_idle_and_subagent_stop_counts() {
    let mut m = machine();
    m.apply(&hook(ClaudeHookEvent::UserPromptSubmit), T0);
    m.apply(
        &hook(ClaudeHookEvent::Stop {
            stop_hook_active: false,
        }),
        T0 + 1,
    );
    assert_eq!(m.info().state, AgentState::Done);
    let a = m.apply(&Signal::UserInput, T0 + 2).unwrap();
    assert_eq!(a.kind, "input:user");
    assert_eq!(m.info().state, AgentState::Idle);
    // User input outside Done / WaitingInput is not a transition.
    m.apply(&hook(ClaudeHookEvent::UserPromptSubmit), T0 + 3);
    assert_eq!(m.apply(&Signal::UserInput, T0 + 4), None);

    let before = m.info().state.clone();
    let a = m
        .apply(&hook(ClaudeHookEvent::SubagentStop), T0 + 5)
        .unwrap();
    assert_eq!(
        (a.kind.as_str(), a.detail.as_deref(), a.state_changed),
        ("hook:SubagentStop", Some("1"), false)
    );
    m.apply(&hook(ClaudeHookEvent::SubagentStop), T0 + 6);
    assert_eq!(m.subagent_stops(), 2);
    assert_eq!(m.info().state, before);
}

#[test]
fn row_pre_compact_and_session_end() {
    let mut m = machine();
    m.apply(
        &hook(ClaudeHookEvent::PreCompact {
            trigger: Some("auto".into()),
        }),
        T0,
    );
    assert_eq!(m.info().state, AgentState::Compacting);
    // PostCompact is not a core event; SessionStart{compact} (hooks
    // reference: fires after auto or manual compaction) ends the phase.
    m.apply(
        &hook(ClaudeHookEvent::Other {
            hook_event_name: "PostCompact".into(),
        }),
        T0 + 1,
    );
    assert_eq!(m.info().state, AgentState::Compacting);
    m.apply(
        &hook(ClaudeHookEvent::SessionStart {
            source: Some("compact".into()),
        }),
        T0 + 1,
    );
    assert_eq!(m.info().state, AgentState::Idle);
    let a = m
        .apply(
            &hook(ClaudeHookEvent::SessionEnd {
                reason: Some("prompt_input_exit".into()),
            }),
            T0 + 2,
        )
        .unwrap();
    assert_eq!(a.detail.as_deref(), Some("prompt_input_exit"));
    assert_eq!(m.info().state, AgentState::Exited { code: None });
}

#[test]
fn row_codex_agent_turn_complete_done() {
    let mut m = machine();
    let a = m
        .apply(
            &Signal::Codex(CodexNotify {
                event_type: "agent-turn-complete".into(),
                thread_id: Some("thread-1".into()),
                cwd: None,
                last_message: Some("done".into()),
            }),
            T0,
        )
        .unwrap();
    assert_eq!(a.kind, "codex:agent-turn-complete");
    assert_eq!(a.detail, None, "assistant text is never recorded");
    let i = m.info();
    assert_eq!(
        (i.kind.clone(), i.external_id.as_deref()),
        (AgentKind::Codex, Some("thread-1"))
    );
    assert_eq!(
        (i.state.clone(), i.source),
        (AgentState::Done, StateSource::Hook)
    );
}

#[test]
fn row_osc_133_a_c_d() {
    let mut m = machine();
    let osc = |mark| Signal::Osc(OscEvent::Prompt(mark));
    let a = m.apply(&osc(PromptMark::OutputStart), T0).unwrap();
    assert_eq!(a.kind, "osc:133C");
    assert_eq!(m.info().state, AgentState::Thinking);
    assert_eq!(
        (m.info().source, m.info().confidence),
        (StateSource::ShellIntegration, OSC_CONFIDENCE)
    );
    let a = m
        .apply(&osc(PromptMark::CommandEnd { exit_code: Some(2) }), T0 + 1)
        .unwrap();
    assert_eq!(
        (a.kind.as_str(), a.detail.as_deref()),
        ("osc:133D", Some("2"))
    );
    assert_eq!(m.info().state, AgentState::Idle);
    // 133;A while already idle and 133;B are not transitions.
    assert_eq!(m.apply(&osc(PromptMark::PromptStart), T0 + 2), None);
    assert_eq!(m.apply(&osc(PromptMark::CommandStart), T0 + 3), None);
    m.apply(&osc(PromptMark::OutputStart), T0 + 4);
    assert!(
        m.apply(&osc(PromptMark::PromptStart), T0 + 5)
            .unwrap()
            .state_changed
    );
    // cwd / notify OSCs are not state signals.
    assert_eq!(
        m.apply(&Signal::Osc(OscEvent::Cwd("/x".into())), T0 + 6),
        None
    );
}

#[test]
fn row_foreground_process_and_activity_heuristics() {
    let mut m = machine();
    // Plain shells never get heuristic states.
    assert_eq!(m.apply(&Signal::OutputActivity, T0), None);
    assert_eq!(m.apply(&Signal::ForegroundProcess("zsh".into()), T0), None);
    assert_eq!(m.apply(&Signal::ForegroundProcess("node".into()), T0), None);

    let a = m
        .apply(&Signal::ForegroundProcess("codex".into()), T0)
        .unwrap();
    assert_eq!(
        (a.kind.as_str(), a.state_changed),
        ("heuristic:foreground", false)
    );
    assert_eq!(m.info().kind, AgentKind::Codex);
    // The native Claude Code build reports `claude.exe`.
    for name in ["claude", "claude.exe", "/Users/me/.local/bin/claude.exe"] {
        assert_eq!(
            agent_kind_for_process(name),
            Some(AgentKind::Claude),
            "{name}"
        );
    }
    assert_eq!(
        agent_kind_for_process("codex-aarch64-apple-darwin"),
        Some(AgentKind::Codex)
    );
    for name in ["node", "zsh", "nvim"] {
        assert_eq!(agent_kind_for_process(name), None, "{name}");
    }
    let mut m = heuristic_claude();
    assert_eq!(m.info().kind, AgentKind::Claude);

    let a = m.apply(&Signal::OutputActivity, T0 + 1).unwrap();
    assert_eq!(a.kind, "heuristic:activity");
    assert_eq!(m.info().state, AgentState::Thinking);
    assert_eq!(
        (m.info().source, m.info().confidence),
        (StateSource::Heuristic, HEURISTIC_CONFIDENCE)
    );

    // Silence needs ≥3 s *and* the cursor in column 0.
    assert_eq!(
        m.apply(
            &Signal::Silence {
                secs: 2,
                cursor_at_line_start: true
            },
            T0 + 2
        ),
        None
    );
    assert_eq!(
        m.apply(
            &Signal::Silence {
                secs: 5,
                cursor_at_line_start: false
            },
            T0 + 3
        ),
        None
    );
    let a = m
        .apply(
            &Signal::Silence {
                secs: 3,
                cursor_at_line_start: true,
            },
            T0 + 4,
        )
        .unwrap();
    assert_eq!(a.kind, "heuristic:idle");
    assert_eq!(m.info().state, AgentState::Idle);
    assert_eq!(m.info().confidence, HEURISTIC_CONFIDENCE);
}

#[test]
fn row_pty_eof_exited() {
    let mut m = machine();
    m.apply(&pre("Bash"), T0);
    let a = m.apply(&Signal::Exited(Some(0)), T0 + 1).unwrap();
    assert_eq!(
        (a.kind.as_str(), a.detail.as_deref()),
        ("pty:exit", Some("0"))
    );
    assert_eq!(m.info().state, AgentState::Exited { code: Some(0) });
    assert_eq!(m.apply(&Signal::Exited(Some(0)), T0 + 2), None);
}

#[test]
fn hook_within_30s_beats_heuristics() {
    let mut m = heuristic_claude();
    m.apply(
        &hook(ClaudeHookEvent::Stop {
            stop_hook_active: false,
        }),
        T0,
    );
    m.apply(&Signal::UserInput, T0 + 1);
    assert_eq!(m.info().state, AgentState::Idle);
    // Idle + output activity would normally be Thinking, but a hook is fresh.
    assert_eq!(m.apply(&Signal::OutputActivity, T0 + 29_999), None);
    assert_eq!(
        m.apply(&Signal::ForegroundProcess("codex".into()), T0 + 10),
        None
    );
    m.apply(&hook(ClaudeHookEvent::UserPromptSubmit), T0 + 20_000);
    assert_eq!(
        m.apply(
            &Signal::Silence {
                secs: 9,
                cursor_at_line_start: true
            },
            T0 + 49_999
        ),
        None
    );
    // After the window heuristics apply again.
    assert!(m
        .apply(
            &Signal::Silence {
                secs: 9,
                cursor_at_line_start: true
            },
            T0 + 50_000
        )
        .is_some());
    assert_eq!(m.info().state, AgentState::Idle);
}

#[test]
fn heuristics_never_override_waiting_permission() {
    let mut m = heuristic_claude();
    m.apply(&pre("Edit"), T0);
    m.apply(&notification("permission_prompt"), T0 + 1);
    let later = T0 + HOOK_PRIORITY_MS + 1;
    assert_eq!(m.apply(&Signal::OutputActivity, later), None);
    assert_eq!(
        m.apply(
            &Signal::Silence {
                secs: 60,
                cursor_at_line_start: true
            },
            later
        ),
        None
    );
    assert_eq!(
        m.apply(&Signal::ForegroundProcess("claude".into()), later),
        None
    );
    assert_eq!(m.info().state, AgentState::WaitingPermission { tool: None });
    // Only a real signal leaves it.
    m.apply(
        &hook(ClaudeHookEvent::PostToolUse {
            tool_name: "Edit".into(),
        }),
        later + 1,
    );
    assert_eq!(m.info().state, AgentState::Thinking);
}

#[test]
fn other_claude_events_are_recorded_but_never_change_state() {
    let mut m = machine();
    m.apply(&pre("Bash"), T0);
    let running = AgentState::ToolRunning {
        tool: "Bash".into(),
    };
    assert_eq!(m.info().state, running);
    for (i, name) in [
        "PermissionRequest",
        "PostToolUseFailure",
        "PermissionDenied",
        "StopFailure",
        "PostCompact",
        "CwdChanged",
    ]
    .into_iter()
    .enumerate()
    {
        let a = m
            .apply(
                &hook(ClaudeHookEvent::Other {
                    hook_event_name: name.into(),
                }),
                T0 + 1 + i as i64,
            )
            .unwrap();
        assert_eq!(a.kind, format!("hook:{name}"));
        assert_eq!((a.detail, a.state_changed), (None, false));
        assert_eq!(m.info().state, running, "{name}");
    }
}

#[test]
fn notification_types_follow_hooks_reference() {
    let mut m = machine();
    for (kind, want) in [
        ("elicitation_dialog", AgentState::WaitingInput),
        ("elicitation_response", AgentState::Thinking),
        ("elicitation_url_dialog", AgentState::WaitingInput),
        ("elicitation_complete", AgentState::Thinking),
        ("agent_needs_input", AgentState::WaitingInput),
        ("agent_completed", AgentState::Done),
    ] {
        m.apply(&notification(kind), T0);
        assert_eq!(m.info().state, want, "{kind}");
    }
    // Informational and unknown types are recorded with the type as detail
    // but leave the state alone.
    for kind in ["auth_success", "quota_auto_resume_fired", "brand_new_type"] {
        let a = m.apply(&notification(kind), T0 + 1).unwrap();
        assert_eq!(
            (a.kind.as_str(), a.detail.as_deref(), a.state_changed),
            ("hook:Notification", Some(kind), false)
        );
        assert_eq!(m.info().state, AgentState::Done, "{kind}");
    }
}

#[test]
fn statusline_updates_model_context_cost() {
    let mut m = machine();
    let s = StatuslineUpdate {
        session_id: "sid".into(),
        model: Some("claude-opus-5-5".into()),
        context_pct: Some(42.0),
        cost_usd: Some(1.5),
        project_dir: None,
        cwd: None,
    };
    assert!(m.apply_statusline(&s));
    assert!(!m.apply_statusline(&s));
    let i = m.info();
    assert_eq!(i.kind, AgentKind::Claude);
    assert_eq!(i.external_id.as_deref(), Some("sid"));
    assert_eq!(
        (i.model.as_deref(), i.context_pct, i.cost_usd),
        (Some("claude-opus-5-5"), Some(42.0), Some(1.5))
    );
    // Missing fields keep the previous values.
    assert!(!m.apply_statusline(&StatuslineUpdate {
        session_id: "sid".into(),
        ..Default::default()
    }));
    assert_eq!(m.info().cost_usd, Some(1.5));
}
