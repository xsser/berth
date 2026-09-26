//! JSON payloads → `HookEnvelope`. Field names follow the official docs
//! (checked 2026-09-26):
//! - Claude Code hooks: <https://code.claude.com/docs/en/hooks> — common
//!   `session_id`, `transcript_path`, `cwd`, `permission_mode`,
//!   `hook_event_name`; per event `source` (SessionStart), `tool_name`
//!   (Pre/PostToolUse), `notification_type` + `message` (Notification),
//!   `stop_hook_active` (Stop), `trigger` (PreCompact), `reason` (SessionEnd).
//! - Statusline: <https://code.claude.com/docs/en/statusline> — `session_id`,
//!   `model.id`, `context_window.used_percentage` (may be null),
//!   `cost.total_cost_usd`, `workspace.project_dir`, `cwd`.
//! - Codex `notify` (openai/codex `codex-rs/hooks/src/legacy_notify.rs`):
//!   JSON appended as the last argv, kebab-case keys `type`, `thread-id`,
//!   `turn-id`, `cwd`, `input-messages`, `last-assistant-message`.
//!
//! Privacy: prompt text, `tool_input`, `input-messages` and transcripts are
//! never read into the envelope; free-text fields are truncated.

use std::path::PathBuf;

use berth_core::{
    AgentSignal, ClaudeHook, ClaudeHookEvent, CodexNotify, HookEnvelope, SessionId,
    StatuslineUpdate,
};
use serde_json::Value;

/// Cap for the free-text fields we do forward (notification message, Codex
/// last assistant message).
pub const MAX_TEXT: usize = 256;

/// Process-level context shared by all modes.
#[derive(Clone, Debug)]
pub struct Context {
    pub berth_session: Option<SessionId>,
    pub pid: u32,
    pub now_ms: i64,
}

impl Context {
    pub fn from_env() -> Context {
        Context {
            berth_session: std::env::var("BERTH_SESSION_ID")
                .ok()
                .and_then(|s| s.trim().parse().ok()),
            pid: std::process::id(),
            now_ms: berth_core::now_ms(),
        }
    }

    fn envelope(&self, signal: AgentSignal) -> HookEnvelope {
        HookEnvelope {
            berth_session: self.berth_session,
            pid: self.pid,
            sent_at_ms: self.now_ms,
            signal,
        }
    }
}

fn str_at<'a>(v: &'a Value, path: &[&str]) -> Option<&'a str> {
    let mut cur = v;
    for key in path {
        cur = cur.get(key)?;
    }
    cur.as_str()
}

fn num_at(v: &Value, path: &[&str]) -> Option<f64> {
    let mut cur = v;
    for key in path {
        cur = cur.get(key)?;
    }
    cur.as_f64()
}

fn path_at(v: &Value, path: &[&str]) -> Option<PathBuf> {
    str_at(v, path).filter(|s| !s.is_empty()).map(PathBuf::from)
}

fn owned(v: &Value, key: &str) -> Option<String> {
    str_at(v, &[key]).map(str::to_owned)
}

pub fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_owned();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_owned()
}

/// Claude Code hook stdin JSON. `None` when it is not a hook payload.
pub fn claude(json: &[u8], ctx: &Context) -> Option<HookEnvelope> {
    let v: Value = serde_json::from_slice(json).ok()?;
    let name = str_at(&v, &["hook_event_name"])?;
    let tool = || owned(&v, "tool_name").unwrap_or_default();
    let event = match name {
        "SessionStart" => ClaudeHookEvent::SessionStart {
            source: owned(&v, "source"),
        },
        "UserPromptSubmit" => ClaudeHookEvent::UserPromptSubmit,
        "PreToolUse" => ClaudeHookEvent::PreToolUse { tool_name: tool() },
        "PostToolUse" => ClaudeHookEvent::PostToolUse { tool_name: tool() },
        "Notification" => ClaudeHookEvent::Notification {
            notification_type: owned(&v, "notification_type"),
            message: truncate(str_at(&v, &["message"]).unwrap_or_default(), MAX_TEXT),
        },
        "Stop" => ClaudeHookEvent::Stop {
            stop_hook_active: v
                .get("stop_hook_active")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        },
        "SubagentStop" => ClaudeHookEvent::SubagentStop,
        "PreCompact" => ClaudeHookEvent::PreCompact {
            trigger: owned(&v, "trigger"),
        },
        "SessionEnd" => ClaudeHookEvent::SessionEnd {
            reason: owned(&v, "reason"),
        },
        other => ClaudeHookEvent::Other {
            hook_event_name: truncate(other, 64),
        },
    };
    Some(ctx.envelope(AgentSignal::Claude(ClaudeHook {
        session_id: owned(&v, "session_id").unwrap_or_default(),
        cwd: path_at(&v, &["cwd"]),
        transcript_path: path_at(&v, &["transcript_path"]),
        permission_mode: owned(&v, "permission_mode"),
        event,
    })))
}

/// Codex `notify` JSON (last argv). `None` without a `type`.
pub fn codex(json: &str, ctx: &Context) -> Option<HookEnvelope> {
    let v: Value = serde_json::from_str(json).ok()?;
    let event_type = owned(&v, "type")?;
    let thread_id = owned(&v, "thread-id").or_else(|| owned(&v, "thread_id"));
    let last = str_at(&v, &["last-assistant-message"])
        .or_else(|| str_at(&v, &["last_assistant_message"]))
        .map(|s| truncate(s, MAX_TEXT));
    Some(ctx.envelope(AgentSignal::Codex(CodexNotify {
        event_type,
        thread_id,
        cwd: path_at(&v, &["cwd"]),
        last_message: last,
    })))
}

/// Claude Code statusline stdin JSON.
pub fn statusline(json: &[u8], ctx: &Context) -> Option<HookEnvelope> {
    let v: Value = serde_json::from_slice(json).ok()?;
    if !v.is_object() {
        return None;
    }
    Some(
        ctx.envelope(AgentSignal::Statusline(StatuslineUpdate {
            session_id: owned(&v, "session_id").unwrap_or_default(),
            model: str_at(&v, &["model", "id"])
                .or_else(|| str_at(&v, &["model", "display_name"]))
                .map(str::to_owned),
            context_pct: num_at(&v, &["context_window", "used_percentage"]).map(|p| p as f32),
            cost_usd: num_at(&v, &["cost", "total_cost_usd"]),
            project_dir: path_at(&v, &["workspace", "project_dir"]),
            cwd: path_at(&v, &["cwd"]).or_else(|| path_at(&v, &["workspace", "current_dir"])),
        })),
    )
}

/// `codex` mode argv (after the `codex` word): `[--chain|-- <cmd...>] <json>`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CodexArgs {
    /// Notify payload: always the last argv (Codex appends it).
    pub json: Option<String>,
    /// Original notify command to exec afterwards; it receives the same
    /// payload as its last argument, exactly as Codex would have passed it.
    pub chain: Vec<String>,
}

pub fn parse_codex_args(args: &[String]) -> CodexArgs {
    let json = args
        .last()
        .filter(|a| a.trim_start().starts_with('{'))
        .cloned();
    let chain = match args.iter().position(|a| a == "--chain" || a == "--") {
        Some(i) => {
            let mut rest: Vec<String> = args[i + 1..].to_vec();
            if json.is_some() {
                rest.pop();
            }
            rest
        }
        None => Vec::new(),
    };
    CodexArgs { json, chain }
}

/// `statusline` mode argv (after the word): `[--] <cmd...>`.
pub fn parse_statusline_args(args: &[String]) -> Vec<String> {
    match args.first().map(String::as_str) {
        Some("--") => args[1..].to_vec(),
        _ => args.to_vec(),
    }
}

#[cfg(test)]
mod tests;
