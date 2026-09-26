//! Daemon view of `config.toml` (DESIGN §8.4). Only `[terminal]`,
//! `[persist]` and `[agents.*]` matter here; other tables (`[font]`,
//! `[sidebar]`, `[[keybind]]`, ...) belong to the GUI and are ignored.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use berth_core::AgentKind;
use serde::Deserialize;

/// Hard cap on in-memory scrollback (DESIGN §5).
pub const MAX_SCROLLBACK: usize = 100_000;

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct Config {
    pub terminal: TerminalConfig,
    pub persist: PersistConfig,
    pub agents: BTreeMap<String, AgentConfig>,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct TerminalConfig {
    pub scrollback: usize,
    /// `auto` | `none` (shell integration injection is not implemented yet;
    /// parsed so configs stay valid).
    pub shell_integration: String,
    /// Surface OSC 52 clipboard *stores* (DESIGN §11: off unless enabled;
    /// clipboard loads are always refused by berth-vt).
    pub osc52_store: bool,
}

impl Default for TerminalConfig {
    fn default() -> Self {
        Self {
            scrollback: 20_000,
            shell_integration: "auto".into(),
            osc52_store: false,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct PersistConfig {
    pub snapshot_interval_s: u64,
    /// Default journal policy for new sessions.
    pub journal: bool,
    /// Cap on the restored history prefix kept per session.
    pub max_restored_lines: usize,
}

impl Default for PersistConfig {
    fn default() -> Self {
        Self {
            snapshot_interval_s: 5,
            journal: false,
            max_restored_lines: 50_000,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct AgentConfig {
    /// Command typed into a fresh shell for `Revive { ResumeAgent }`;
    /// `{id}` is replaced with the (shell-quoted) external id.
    pub resume_command: Option<String>,
}

impl Config {
    pub fn parse(text: &str) -> Result<Config, toml::de::Error> {
        toml::from_str(text)
    }

    /// Missing file → defaults. A broken file must not keep the daemon from
    /// starting: log it and use defaults.
    pub fn load(path: &Path) -> Config {
        match std::fs::read_to_string(path) {
            Ok(text) => Config::parse(&text).unwrap_or_else(|e| {
                tracing::warn!(path = %path.display(), error = %e, "invalid config; using defaults");
                Config::default()
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Config::default(),
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "cannot read config; using defaults");
                Config::default()
            }
        }
    }

    pub fn scrollback(&self) -> usize {
        self.terminal.scrollback.min(MAX_SCROLLBACK)
    }

    pub fn snapshot_interval(&self) -> Duration {
        Duration::from_secs(self.persist.snapshot_interval_s.max(1))
    }

    pub fn max_restored_lines(&self) -> usize {
        self.persist.max_restored_lines
    }

    /// `agents.<kind>.resume_command`, falling back to the built-in commands
    /// (`claude --resume <session_id>`, `codex resume <SESSION_ID>`; the
    /// latter checked against openai/codex `codex-rs/cli/src/main.rs`).
    pub fn resume_command(&self, kind: &AgentKind) -> Option<String> {
        let key = match kind {
            AgentKind::Shell => return None,
            AgentKind::Claude => "claude",
            AgentKind::Codex => "codex",
            AgentKind::Other(name) => name.as_str(),
        };
        if let Some(cmd) = self.agents.get(key).and_then(|a| a.resume_command.clone()) {
            return Some(cmd);
        }
        match kind {
            AgentKind::Claude => Some("claude --resume {id}".into()),
            AgentKind::Codex => Some("codex resume {id}".into()),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_design() {
        let c = Config::default();
        assert_eq!(c.scrollback(), 20_000);
        assert!(!c.terminal.osc52_store, "OSC 52 stores need an opt-in");
        assert_eq!(c.snapshot_interval(), Duration::from_secs(5));
        assert!(!c.persist.journal);
        assert_eq!(c.max_restored_lines(), 50_000);
        assert_eq!(
            c.resume_command(&AgentKind::Claude).unwrap(),
            "claude --resume {id}"
        );
        assert_eq!(
            c.resume_command(&AgentKind::Codex).unwrap(),
            "codex resume {id}"
        );
        assert_eq!(c.resume_command(&AgentKind::Shell), None);
    }

    #[test]
    fn parses_design_example_and_ignores_gui_tables() {
        let c = Config::parse(
            r#"
[font]
family = "SF Mono"
size = 13
[terminal]
scrollback = 500000
shell_integration = "none"
osc52_store = true
[persist]
snapshot_interval_s = 0
journal = true
max_restored_lines = 100
[sidebar]
width = 280
[agents.claude]
resume_command = "claude -r {id}"
[agents.aider]
resume_command = "aider --restore {id}"
[[keybind]]
key = "cmd+k"
action = "command_palette"
"#,
        )
        .unwrap();
        assert_eq!(c.scrollback(), MAX_SCROLLBACK);
        assert_eq!(c.terminal.shell_integration, "none");
        assert!(c.terminal.osc52_store);
        assert_eq!(c.snapshot_interval(), Duration::from_secs(1));
        assert!(c.persist.journal);
        assert_eq!(c.max_restored_lines(), 100);
        assert_eq!(
            c.resume_command(&AgentKind::Claude).unwrap(),
            "claude -r {id}"
        );
        assert_eq!(
            c.resume_command(&AgentKind::Other("aider".into())).unwrap(),
            "aider --restore {id}"
        );
        assert_eq!(
            c.resume_command(&AgentKind::Codex).unwrap(),
            "codex resume {id}"
        );
    }

    #[test]
    fn load_missing_or_broken_file_falls_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        assert_eq!(Config::load(&path), Config::default());
        std::fs::write(&path, "[persist\nbroken").unwrap();
        assert_eq!(Config::load(&path), Config::default());
        std::fs::write(&path, "[persist]\njournal = true\n").unwrap();
        assert!(Config::load(&path).persist.journal);
    }
}
