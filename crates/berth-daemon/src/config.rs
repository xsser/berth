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
    /// `agents.<kind>.resume_command`, split into words once by `parse`.
    #[serde(skip)]
    resume_templates: BTreeMap<String, Result<Vec<String>, String>>,
}

/// Placeholder for the agent's session id; replaced only as a whole word.
pub const ID_PLACEHOLDER: &str = "{id}";

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
    /// argv template for `Revive { ResumeAgent }`, e.g. `claude --resume
    /// {id}`. Split once with shell-like quoting (`'…'`, `"…"`, `\`), then
    /// executed directly — never by a shell — with the word `{id}` replaced
    /// by the agent's session id.
    pub resume_command: Option<String>,
}

impl Config {
    pub fn parse(text: &str) -> Result<Config, toml::de::Error> {
        let mut config: Config = toml::from_str(text)?;
        config.resume_templates = config
            .agents
            .iter()
            .filter_map(|(kind, agent)| {
                let command = agent.resume_command.as_deref()?;
                let template = parse_template(command);
                if let Err(e) = &template {
                    tracing::warn!(agent = %kind, error = %e, "invalid resume_command");
                }
                Some((kind.clone(), template))
            })
            .collect();
        Ok(config)
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

    /// argv resuming agent session `id` (which the caller has checked to be
    /// a plain token): `agents.<kind>.resume_command`, else the built-in
    /// `claude --resume {id}` / `codex resume {id}` (the latter checked
    /// against openai/codex `codex-rs/cli/src/main.rs`).
    pub fn resume_argv(&self, kind: &AgentKind, id: &str) -> Result<Vec<String>, String> {
        let key = match kind {
            AgentKind::Shell => return Err("a plain shell has no agent session to resume".into()),
            AgentKind::Claude => "claude",
            AgentKind::Codex => "codex",
            AgentKind::Other(name) => name.as_str(),
        };
        let configured = self
            .agents
            .get(key)
            .and_then(|a| a.resume_command.as_deref());
        let template = match (self.resume_templates.get(key), configured) {
            (Some(parsed), _) => parsed.clone(),
            // Built programmatically rather than by `parse`.
            (None, Some(command)) => parse_template(command),
            (None, None) => match kind {
                AgentKind::Claude => Ok(words(&["claude", "--resume", ID_PLACEHOLDER])),
                AgentKind::Codex => Ok(words(&["codex", "resume", ID_PLACEHOLDER])),
                _ => Err(format!("no agents.{key}.resume_command configured")),
            },
        }
        .map_err(|e| format!("agents.{key}.resume_command: {e}"))?;
        Ok(template
            .into_iter()
            .map(|w| {
                if w == ID_PLACEHOLDER {
                    id.to_owned()
                } else {
                    w
                }
            })
            .collect())
    }
}

fn words(ws: &[&str]) -> Vec<String> {
    ws.iter().map(|w| (*w).to_owned()).collect()
}

/// Split a resume command template and check it: non-empty, no shell
/// syntax (it will not run in a shell), `{id}` present and only as a whole
/// word. A leading `~/` in the program is expanded to `$HOME`.
fn parse_template(command: &str) -> Result<Vec<String>, String> {
    let mut argv = split_words(command)?;
    let Some(program) = argv.first_mut() else {
        return Err("empty command".into());
    };
    if let Some(rest) = program.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            *program = std::path::Path::new(&home)
                .join(rest)
                .to_string_lossy()
                .into_owned();
        }
    }
    if argv
        .iter()
        .any(|w| w != ID_PLACEHOLDER && w.contains(ID_PLACEHOLDER))
    {
        return Err(format!("{ID_PLACEHOLDER} is only replaced as a whole word"));
    }
    if !argv.iter().any(|w| w == ID_PLACEHOLDER) {
        return Err(format!("{ID_PLACEHOLDER} must appear as a separate word"));
    }
    Ok(argv)
}

/// Shell-like word splitting without any expansion: blanks separate words,
/// `'…'` is literal, `"…"` honours `\"`, `\\`, `\$`, `` \` ``, and a
/// backslash outside quotes escapes the next character. Unquoted shell
/// operators and expansions are rejected instead of being passed on
/// literally.
fn split_words(s: &str) -> Result<Vec<String>, String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        match c {
            ' ' | '\t' | '\n' => {
                if in_word {
                    words.push(std::mem::take(&mut word));
                    in_word = false;
                }
            }
            '\'' => {
                in_word = true;
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some(c) => word.push(c),
                        None => return Err("unterminated single quote".into()),
                    }
                }
            }
            '"' => {
                in_word = true;
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('\\') => match chars.next() {
                            Some(c @ ('"' | '\\' | '$' | '`')) => word.push(c),
                            Some('\n') => {}
                            Some(c) => {
                                word.push('\\');
                                word.push(c);
                            }
                            None => return Err("unterminated double quote".into()),
                        },
                        Some(c @ ('$' | '`')) => {
                            return Err(format!(
                                "shell syntax `{c}` is not supported (the command runs without a shell)"
                            ));
                        }
                        Some(c) => word.push(c),
                        None => return Err("unterminated double quote".into()),
                    }
                }
            }
            '\\' => match chars.next() {
                Some('\n') => {}
                Some(c) => {
                    in_word = true;
                    word.push(c);
                }
                None => return Err("trailing backslash".into()),
            },
            '|' | '&' | ';' | '<' | '>' | '(' | ')' | '$' | '`' => {
                return Err(format!(
                    "shell syntax `{c}` is not supported (the command runs without a shell)"
                ));
            }
            c => {
                in_word = true;
                word.push(c);
            }
        }
    }
    if in_word {
        words.push(word);
    }
    Ok(words)
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
            c.resume_argv(&AgentKind::Claude, "abc-1").unwrap(),
            words(&["claude", "--resume", "abc-1"])
        );
        assert_eq!(
            c.resume_argv(&AgentKind::Codex, "t.2").unwrap(),
            words(&["codex", "resume", "t.2"])
        );
        assert!(c.resume_argv(&AgentKind::Shell, "x").is_err());
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
            c.resume_argv(&AgentKind::Claude, "s").unwrap(),
            words(&["claude", "-r", "s"])
        );
        assert_eq!(
            c.resume_argv(&AgentKind::Other("aider".into()), "s")
                .unwrap(),
            words(&["aider", "--restore", "s"])
        );
        assert_eq!(
            c.resume_argv(&AgentKind::Codex, "s").unwrap(),
            words(&["codex", "resume", "s"])
        );
    }

    /// Review high #1: the template is split once, `{id}` only replaces a
    /// whole word, and nothing is ever handed to a shell.
    #[test]
    fn resume_templates_are_split_once_and_take_the_id_as_a_whole_word() {
        let c = Config::parse(
            r#"
[agents.claude]
resume_command = "'/opt/my tools/claude' --resume {id} --append-system-prompt \"a \\\"b\\\" c\""
[agents.codex]
resume_command = "codex resume --session={id}"
[agents.aider]
resume_command = "aider --restore {id}; rm -rf ~"
"#,
        )
        .unwrap();
        assert_eq!(
            c.resume_argv(&AgentKind::Claude, "s1").unwrap(),
            words(&[
                "/opt/my tools/claude",
                "--resume",
                "s1",
                "--append-system-prompt",
                "a \"b\" c"
            ])
        );
        // Whatever the id contains, it stays one argument (callers also
        // refuse ids that are not plain tokens).
        assert_eq!(
            c.resume_argv(&AgentKind::Claude, "a b;c").unwrap()[2],
            "a b;c"
        );
        let codex = c.resume_argv(&AgentKind::Codex, "s1").unwrap_err();
        assert!(codex.contains("whole word"), "{codex}");
        let aider = c
            .resume_argv(&AgentKind::Other("aider".into()), "s1")
            .unwrap_err();
        assert!(aider.contains("without a shell"), "{aider}");
    }

    #[test]
    fn split_words_quoting() {
        assert_eq!(
            split_words(r#"a 'b c' "d \"e\" \$f" g\ h ''"#).unwrap(),
            words(&["a", "b c", "d \"e\" $f", "g h", ""])
        );
        for bad in [
            "a 'b",
            "a \"b",
            "a \\",
            "a $(b)",
            "a | b",
            "a \"$HOME\"",
            "x `y`",
        ] {
            assert!(split_words(bad).is_err(), "{bad}");
        }
        assert_eq!(split_words("'a | $b'").unwrap(), words(&["a | $b"]));
        assert_eq!(split_words("  ").unwrap(), Vec::<String>::new());
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
