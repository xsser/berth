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

fn hook_from(session_id: &str, event: ClaudeHookEvent) -> Signal {
    Signal::Hook(ClaudeHook {
        session_id: session_id.into(),
        cwd: None,
        transcript_path: None,
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
    // The notification does not name the tool; the running call's is used.
    assert_eq!(
        m.info().state,
        AgentState::WaitingPermission {
            tool: Some("Bash".into())
        }
    );
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
    // M3: SubagentStart is counted the same way and changes nothing.
    for n in ["1", "2"] {
        let a = m.apply(&other("SubagentStart"), T0 + 7).unwrap();
        assert_eq!(
            (a.kind.as_str(), a.detail.as_deref(), a.state_changed),
            ("hook:SubagentStart", Some(n), false)
        );
    }
    assert_eq!(m.info().state, before);
}

fn pre_compact(trigger: &str) -> Signal {
    hook(ClaudeHookEvent::PreCompact {
        trigger: Some(trigger.into()),
    })
}

fn session_start(source: &str) -> Signal {
    hook(ClaudeHookEvent::SessionStart {
        source: Some(source.into()),
    })
}

#[test]
fn row_pre_compact_and_session_end() {
    let mut m = machine();
    // `/compact` at the prompt: nothing runs afterwards (a repeated
    // PreCompact does not count the compaction itself as a turn).
    m.apply(&pre_compact("manual"), T0);
    m.apply(&pre_compact("manual"), T0);
    assert_eq!(m.info().state, AgentState::Compacting);
    let a = m.apply(&other("PostCompact"), T0 + 1).unwrap();
    assert_eq!(
        (a.kind.as_str(), a.state_changed),
        ("hook:PostCompact", true)
    );
    assert_eq!(m.info().state, AgentState::Idle);
    // SessionStart{compact} (fires after a compaction too) finds it over.
    let a = m.apply(&session_start("compact"), T0 + 1).unwrap();
    assert!(!a.state_changed);
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
    // DESIGN §9: agent left, not Exited (the shell lives on).
    let info = m.info();
    assert_eq!(
        (info.kind.clone(), info.state.clone(), info.source),
        (AgentKind::Shell, AgentState::Idle, StateSource::Hook)
    );
    assert_eq!(info.external_id.as_deref(), Some("claude-uuid"));
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

/// Review #8: activity and silence never override a hook-sourced state, no
/// matter how old the hook is; only the kind detection keeps the 30 s window.
#[test]
fn output_heuristics_never_override_hook_states() {
    let mut m = heuristic_claude();
    m.apply(
        &hook(ClaudeHookEvent::Stop {
            stop_hook_active: false,
        }),
        T0,
    );
    m.apply(&Signal::UserInput, T0 + 1);
    assert_eq!(
        (m.info().state.clone(), m.info().source),
        (AgentState::Idle, StateSource::Hook)
    );
    let hour = 3_600_000;
    assert_eq!(m.apply(&Signal::OutputActivity, T0 + 2), None);
    assert_eq!(m.apply(&Signal::OutputActivity, T0 + hour), None);
    m.apply(&hook(ClaudeHookEvent::UserPromptSubmit), T0 + hour);
    let silence = Signal::Silence {
        secs: 60,
        cursor_at_line_start: true,
    };
    assert_eq!(m.apply(&silence, T0 + hour + 60_000), None);
    assert_eq!(m.apply(&silence, T0 + 9 * hour), None);
    assert_eq!(m.info().state, AgentState::Thinking);
    // Kind detection still yields to a hook for 30 s.
    assert_eq!(
        m.apply(&Signal::ForegroundProcess("codex".into()), T0 + hour + 10),
        None
    );
    assert_eq!(m.info().kind, AgentKind::Claude);

    // Shell-integration and heuristic states are fair game.
    let mut m = machine();
    m.apply(&Signal::Osc(OscEvent::Prompt(PromptMark::OutputStart)), T0);
    m.apply(&Signal::ForegroundProcess("claude".into()), T0 + 1);
    assert_eq!(
        (m.info().state.clone(), m.info().source),
        (AgentState::Thinking, StateSource::ShellIntegration)
    );
    assert!(m.apply(&silence, T0 + 2).is_some());
    assert_eq!(m.info().state, AgentState::Idle);
    assert!(m.apply(&Signal::OutputActivity, T0 + 3).is_some());
    assert_eq!(m.info().state, AgentState::Thinking);
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
    assert_eq!(
        m.info().state,
        AgentState::WaitingPermission {
            tool: Some("Edit".into())
        }
    );
    // Only a real signal leaves it.
    m.apply(
        &hook(ClaudeHookEvent::PostToolUse {
            tool_name: "Edit".into(),
        }),
        later + 1,
    );
    assert_eq!(m.info().state, AgentState::Thinking);
}

fn other(name: &str) -> Signal {
    hook(ClaudeHookEvent::Other {
        hook_event_name: name.into(),
    })
}

/// Review #10: the permission / failure events beyond the nine core ones.
#[test]
fn permission_and_failure_events_move_the_state() {
    let mut m = machine();
    m.apply(&pre("Bash"), T0);
    let a = m.apply(&other("PermissionRequest"), T0 + 1).unwrap();
    assert_eq!(
        (a.kind.as_str(), a.detail.as_deref(), a.state_changed),
        ("hook:PermissionRequest", Some("Bash"), true)
    );
    let waiting = AgentState::WaitingPermission {
        tool: Some("Bash".into()),
    };
    assert_eq!(m.info().state, waiting);
    // The permission_prompt notification ~6 s later keeps the tool.
    m.apply(&notification("permission_prompt"), T0 + 6_000);
    assert_eq!(m.info().state, waiting);
    m.apply(&other("PermissionDenied"), T0 + 7_000);
    assert_eq!(m.info().state, AgentState::Thinking);

    m.apply(&pre("Edit"), T0 + 8_000);
    m.apply(&other("PostToolUseFailure"), T0 + 8_001);
    assert_eq!(m.info().state, AgentState::Thinking);
    // No tool running: the dialog is for an unknown tool.
    m.apply(&other("PermissionRequest"), T0 + 8_002);
    assert_eq!(m.info().state, AgentState::WaitingPermission { tool: None });

    let a = m.apply(&other("StopFailure"), T0 + 9_000).unwrap();
    assert!(a.state_changed);
    assert!(matches!(m.info().state, AgentState::Error { .. }));
    assert_eq!(m.info().source, StateSource::Hook);
}

#[test]
fn other_claude_events_are_recorded_but_never_change_state() {
    let mut m = machine();
    m.apply(&pre("Bash"), T0);
    let running = AgentState::ToolRunning {
        tool: "Bash".into(),
    };
    for (i, name) in ["CwdChanged", "InstructionsLoaded", "Brand"]
        .into_iter()
        .enumerate()
    {
        let a = m.apply(&other(name), T0 + 1 + i as i64).unwrap();
        assert_eq!(a.kind, format!("hook:{name}"));
        assert_eq!((a.detail, a.state_changed), (None, false));
        assert_eq!(m.info().state, running, "{name}");
    }
}

/// Review #9: the foreground switching from an agent to anything else means
/// the agent left — even out of a hook state, never out of `Exited`.
#[test]
fn foreground_agent_to_non_agent_is_agent_left() {
    let mut m = machine();
    m.apply(&Signal::ForegroundProcess("claude".into()), T0);
    m.apply(&pre("Bash"), T0 + 1);
    let a = m
        .apply(&Signal::ForegroundProcess("zsh".into()), T0 + 2)
        .unwrap();
    assert_eq!(
        (a.kind.as_str(), a.detail.as_deref(), a.state_changed),
        ("heuristic:agent_left", Some("zsh"), true)
    );
    let info = m.info();
    assert_eq!(info.kind, AgentKind::Shell);
    assert_eq!(info.last_agent, Some(AgentKind::Claude));
    assert_eq!(
        (info.state.clone(), info.source, info.confidence),
        (
            AgentState::Idle,
            StateSource::Heuristic,
            HEURISTIC_CONFIDENCE
        )
    );
    assert_eq!(info.external_id.as_deref(), Some("claude-uuid"));
    assert_eq!(info.transcript_path, Some(PathBuf::from("/t.jsonl")));
    // Now a shell: heuristics no longer apply, and the same name twice is
    // not another switch.
    assert_eq!(m.apply(&Signal::OutputActivity, T0 + 3), None);
    assert_eq!(
        m.apply(&Signal::ForegroundProcess("vim".into()), T0 + 4),
        None
    );

    // Not after `Exited` (PTY exit).
    let mut m = machine();
    m.apply(&Signal::ForegroundProcess("claude".into()), T0);
    m.apply(&Signal::Exited(Some(0)), T0 + 1);
    assert_eq!(
        m.apply(&Signal::ForegroundProcess("zsh".into()), T0 + 2),
        None
    );
    assert_eq!(m.info().state, AgentState::Exited { code: Some(0) });

    // Only a switch *from an agent name* counts: `node` hosting Claude.
    let mut m = machine();
    m.apply(&Signal::ForegroundProcess("node".into()), T0);
    m.apply(&hook(ClaudeHookEvent::UserPromptSubmit), T0 + 1);
    assert_eq!(
        m.apply(&Signal::ForegroundProcess("zsh".into()), T0 + 2),
        None
    );
    assert_eq!(m.info().kind, AgentKind::Claude);
    // ...and a revive forgets the previous foreground.
    let mut m = heuristic_claude();
    m.reset(m.info().clone());
    assert_eq!(
        m.apply(&Signal::ForegroundProcess("zsh".into()), T0 + 1),
        None
    );
}

/// Review #11 / `StateSource` contract: OSC 133 marks while an agent owns
/// the session end its state as "agent left" (kind back to `Shell`), even a
/// hook-sourced sticky state; heuristics still never do.
#[test]
fn osc_prompt_marks_end_an_agent_as_agent_left() {
    let osc = |mark| Signal::Osc(OscEvent::Prompt(mark));
    let mut m = machine();
    m.apply(&pre("Edit"), T0);
    m.apply(&other("PermissionRequest"), T0 + 1);
    assert!(matches!(
        m.info().state,
        AgentState::WaitingPermission { .. }
    ));
    let a = m.apply(&osc(PromptMark::PromptStart), T0 + 2).unwrap();
    assert_eq!(
        (a.kind.as_str(), a.detail.as_deref(), a.state_changed),
        ("osc:133A", Some("agent_left"), true)
    );
    let info = m.info();
    assert_eq!(
        (info.kind.clone(), info.state.clone(), info.source),
        (
            AgentKind::Shell,
            AgentState::Idle,
            StateSource::ShellIntegration
        )
    );
    assert_eq!(info.last_agent, Some(AgentKind::Claude));
    assert_eq!(info.external_id.as_deref(), Some("claude-uuid"));
    assert_eq!(info.transcript_path, Some(PathBuf::from("/t.jsonl")));
    // Back to plain shell marks.
    let a = m.apply(&osc(PromptMark::OutputStart), T0 + 3).unwrap();
    assert_eq!((a.kind.as_str(), a.detail), ("osc:133C", None));

    let mut m = machine();
    m.apply(
        &hook(ClaudeHookEvent::Stop {
            stop_hook_active: false,
        }),
        T0,
    );
    let a = m
        .apply(&osc(PromptMark::CommandEnd { exit_code: Some(0) }), T0 + 1)
        .unwrap();
    assert_eq!(
        (a.kind.as_str(), a.detail.as_deref()),
        ("osc:133D", Some("agent_left"))
    );
    assert_eq!(m.info().kind, AgentKind::Shell);
}

/// Review #19: `Exited` is terminal — late hooks are recorded but change
/// nothing, OSC and heuristics are ignored; only a revive resets.
#[test]
fn exited_is_terminal() {
    let mut m = machine();
    m.apply(&pre("Bash"), T0);
    m.apply(&Signal::Exited(Some(0)), T0 + 1);
    let exited = m.info().clone();
    assert_eq!(exited.state, AgentState::Exited { code: Some(0) });
    for (i, event) in [
        ClaudeHookEvent::SessionStart { source: None },
        ClaudeHookEvent::PreToolUse {
            tool_name: "Bash".into(),
        },
        ClaudeHookEvent::Stop {
            stop_hook_active: false,
        },
    ]
    .into_iter()
    .enumerate()
    {
        let name = claude_event_name(&event).to_owned();
        let a = m
            .apply(&hook_from("other-id", event), T0 + 2 + i as i64)
            .unwrap();
        assert_eq!(
            (a.kind.as_str(), a.detail, a.state_changed),
            ("hook:late", Some(name), false)
        );
        assert_eq!(m.info(), &exited);
    }
    let codex = Signal::Codex(CodexNotify {
        event_type: "agent-turn-complete".into(),
        thread_id: Some("t".into()),
        cwd: None,
        last_message: None,
    });
    assert_eq!(m.apply(&codex, T0 + 9).unwrap().kind, "hook:late");
    let silence = Signal::Silence {
        secs: 60,
        cursor_at_line_start: true,
    };
    for signal in [
        Signal::Osc(OscEvent::Prompt(PromptMark::PromptStart)),
        Signal::Osc(OscEvent::Prompt(PromptMark::CommandEnd { exit_code: None })),
        Signal::OutputActivity,
        silence,
        Signal::UserInput,
        Signal::ForegroundProcess("zsh".into()),
    ] {
        assert_eq!(m.apply(&signal, T0 + 10), None, "{signal:?}");
    }
    assert_eq!(m.info(), &exited);
    // Revive.
    let mut info = m.info().clone();
    info.state = AgentState::Idle;
    m.reset(info);
    assert!(m.apply(&pre("Bash"), T0 + 11).unwrap().state_changed);
}

/// DESIGN §9 (e147a89): `SessionEnd` is "agent left", not `Exited` — the
/// shell lives on, drives the state through OSC 133 again, and the next
/// agent run simply starts over. Only the PTY's exit is `Exited`.
#[test]
fn session_end_is_agent_left_not_exited() {
    let mut m = machine();
    m.apply(&pre("Bash"), T0);
    let a = m
        .apply(&hook(ClaudeHookEvent::SessionEnd { reason: None }), T0 + 1)
        .unwrap();
    assert_eq!(
        (a.kind.as_str(), a.state_changed),
        ("hook:SessionEnd", true)
    );
    let info = m.info().clone();
    assert_eq!(
        (info.kind, info.state),
        (AgentKind::Shell, AgentState::Idle)
    );
    assert_eq!(info.last_agent, Some(AgentKind::Claude));
    assert_eq!(info.external_id.as_deref(), Some("claude-uuid"));
    assert_eq!(info.transcript_path, Some(PathBuf::from("/t.jsonl")));
    // The shell's marks move it again.
    let a = m
        .apply(
            &Signal::Osc(OscEvent::Prompt(PromptMark::OutputStart)),
            T0 + 2,
        )
        .unwrap();
    assert_eq!((a.kind.as_str(), a.detail), ("osc:133C", None));
    assert_eq!(m.info().state, AgentState::Thinking);
    // A new run in the same shell.
    let a = m
        .apply(
            &hook_from("new-run", ClaudeHookEvent::SessionStart { source: None }),
            T0 + 3,
        )
        .unwrap();
    assert!(a.state_changed);
    assert_eq!(
        (m.info().kind.clone(), m.info().external_id.as_deref()),
        (AgentKind::Claude, Some("new-run"))
    );
    // The usual exit: Claude idle at its prompt, then `/exit`. Still a new
    // state (a shell now): source Hook, since = now, reason as detail.
    assert_eq!(m.info().state, AgentState::Idle);
    let a = m
        .apply(
            &hook_from(
                "new-run",
                ClaudeHookEvent::SessionEnd {
                    reason: Some("prompt_input_exit".into()),
                },
            ),
            T0 + 9,
        )
        .unwrap();
    assert_eq!(
        (a.kind.as_str(), a.detail.as_deref()),
        ("hook:SessionEnd", Some("prompt_input_exit"))
    );
    let info = m.info();
    assert_eq!(
        (
            info.kind.clone(),
            info.state.clone(),
            info.source,
            info.since_ms
        ),
        (
            AgentKind::Shell,
            AgentState::Idle,
            StateSource::Hook,
            T0 + 9
        )
    );
    assert_eq!(info.external_id.as_deref(), Some("new-run"));
}

/// `last_agent` names the agent the ids belong to: an agent of another
/// kind taking over the session drops them rather than resuming them as
/// its own; the same agent again keeps them.
#[test]
fn ids_stay_with_the_agent_that_reported_them() {
    let mut m = machine();
    m.apply(&hook(ClaudeHookEvent::UserPromptSubmit), T0);
    m.apply(&hook(ClaudeHookEvent::SessionEnd { reason: None }), T0 + 1);
    assert_eq!(m.info().resume_kind(), Some(&AgentKind::Claude));
    let later = T0 + HOOK_PRIORITY_MS + 1; // heuristics count again
                                           // Claude again (heuristic first, before its hooks): same ids.
    let mut again = m.clone();
    again.apply(&Signal::ForegroundProcess("claude".into()), later);
    assert_eq!(again.info().kind, AgentKind::Claude);
    assert_eq!(again.info().external_id.as_deref(), Some("claude-uuid"));
    // Codex: Claude's ids are not its own.
    m.apply(&Signal::ForegroundProcess("codex".into()), later);
    let info = m.info();
    assert_eq!(info.kind, AgentKind::Codex);
    assert_eq!(
        (info.external_id.as_deref(), info.transcript_path.as_deref()),
        (None, None)
    );
    m.apply(&Signal::ForegroundProcess("zsh".into()), later + 1);
    let info = m.info();
    assert_eq!(
        (
            info.kind.clone(),
            info.last_agent.clone(),
            info.external_id.clone()
        ),
        (AgentKind::Shell, Some(AgentKind::Codex), None),
        "nothing to resume rather than codex with Claude's id"
    );
    // A statusline brings Claude back with its own id.
    m.apply_statusline(&StatuslineUpdate {
        session_id: "claude-2".into(),
        ..StatuslineUpdate::default()
    });
    assert_eq!(
        (m.info().kind.clone(), m.info().external_id.as_deref()),
        (AgentKind::Claude, Some("claude-2"))
    );
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

/// Review high #1: ids that are not plain tokens never become the external
/// id, whichever channel they arrive on.
#[test]
fn external_ids_must_be_plain_tokens() {
    let long = "a".repeat(129);
    for bad in [
        "\u{15}touch /tmp/x #",
        "a b",
        "$(id)",
        "x;y",
        "ünï",
        long.as_str(),
    ] {
        let mut m = machine();
        m.apply(&hook_from(bad, ClaudeHookEvent::UserPromptSubmit), T0);
        m.apply(
            &Signal::Codex(CodexNotify {
                event_type: "agent-turn-complete".into(),
                thread_id: Some(bad.into()),
                cwd: None,
                last_message: None,
            }),
            T0,
        );
        m.apply_statusline(&StatuslineUpdate {
            session_id: bad.into(),
            ..Default::default()
        });
        assert_eq!(m.info().external_id, None, "{bad:?}");
    }
    let good = "0f8c2e1a-1111-2222-3333-444455556666";
    let mut m = machine();
    m.apply(&hook_from(good, ClaudeHookEvent::UserPromptSubmit), T0);
    assert_eq!(m.info().external_id.as_deref(), Some(good));
    // A later invalid id does not replace a valid one.
    m.apply(
        &hook_from("\u{15}rm -rf ~", ClaudeHookEvent::UserPromptSubmit),
        T0 + 1,
    );
    assert_eq!(m.info().external_id.as_deref(), Some(good));
    assert!(is_valid_external_id(&"a".repeat(128)));
    assert!(is_valid_external_id("thread_1.v2"));
    assert!(!is_valid_external_id(""));
}

/// M3: an auto-compaction during a turn ends in `Thinking` (the turn goes
/// on), whichever of `PostCompact` / `SessionStart{compact}` comes first;
/// the later one changes nothing.
#[test]
fn compaction_during_a_turn_resumes_thinking_in_either_order() {
    for first_post_compact in [true, false] {
        let mut m = machine();
        m.apply(&hook(ClaudeHookEvent::UserPromptSubmit), T0);
        m.apply(&pre("Bash"), T0 + 1);
        m.apply(&pre_compact("auto"), T0 + 2);
        // A repeated PreCompact does not forget that a turn was running.
        m.apply(&pre_compact("auto"), T0 + 3);
        assert_eq!(m.info().state, AgentState::Compacting);
        let (a, b) = if first_post_compact {
            (other("PostCompact"), session_start("compact"))
        } else {
            (session_start("compact"), other("PostCompact"))
        };
        assert!(m.apply(&a, T0 + 4).unwrap().state_changed);
        assert_eq!(m.info().state, AgentState::Thinking, "{first_post_compact}");
        assert!(!m.apply(&b, T0 + 5).unwrap().state_changed);
        assert_eq!(m.info().state, AgentState::Thinking, "{first_post_compact}");
        assert_eq!(m.info().source, StateSource::Hook);
    }
}

/// M3: without a compaction under way, `PostCompact` and
/// `SessionStart{compact}` leave the state alone (M1 turned the latter into
/// `Idle`); other `SessionStart` sources still start at `Idle`.
#[test]
fn compact_events_outside_a_compaction_change_nothing() {
    let mut m = machine();
    m.apply(
        &hook(ClaudeHookEvent::Stop {
            stop_hook_active: false,
        }),
        T0,
    );
    assert_eq!(m.info().state, AgentState::Done);
    for (i, s) in [other("PostCompact"), session_start("compact")]
        .iter()
        .enumerate()
    {
        let a = m.apply(s, T0 + 1 + i as i64).unwrap();
        assert!(!a.state_changed);
        assert_eq!(m.info().state, AgentState::Done);
    }
    m.apply(&session_start("clear"), T0 + 5);
    assert_eq!(m.info().state, AgentState::Idle);
    // A compaction ends where it began; `reset` forgets a running one.
    m.apply(&hook(ClaudeHookEvent::UserPromptSubmit), T0 + 6);
    m.apply(&pre_compact("auto"), T0 + 7);
    let info = m.info().clone();
    m.reset(info);
    m.apply(&other("PostCompact"), T0 + 8);
    assert_eq!(m.info().state, AgentState::Idle);
}

/// M3: an MCP elicitation waits for the user; its result lets the model
/// carry on.
#[test]
fn elicitation_waits_for_input_and_its_result_resumes() {
    let mut m = machine();
    m.apply(&hook(ClaudeHookEvent::UserPromptSubmit), T0);
    let a = m.apply(&other("Elicitation"), T0 + 1).unwrap();
    assert_eq!(
        (a.kind.as_str(), a.detail.as_deref(), a.state_changed),
        ("hook:Elicitation", None, true)
    );
    assert_eq!(m.info().state, AgentState::WaitingInput);
    assert!(m.info().state.needs_attention());
    let a = m.apply(&other("ElicitationResult"), T0 + 2).unwrap();
    assert!(a.state_changed);
    assert_eq!(m.info().state, AgentState::Thinking);
    // Sticky like every waiting state: output activity does not end it.
    m.apply(&other("Elicitation"), T0 + 3);
    assert_eq!(m.apply(&Signal::OutputActivity, T0 + 4), None);
    assert_eq!(m.info().state, AgentState::WaitingInput);
}
