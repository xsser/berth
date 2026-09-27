//! Daemon view of `config.toml` (DESIGN §8.4). Only `[terminal]`,
//! `[persist]`, `[archive]` and `[agents.*]` matter here; other tables
//! (`[font]`, `[sidebar]`, `[[keybind]]`, ...) belong to the GUI and are
//! ignored.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use berth_core::AgentKind;
use serde::Deserialize;

/// Hard cap on in-memory scrollback (DESIGN §5).
pub const MAX_SCROLLBACK: usize = 100_000;
/// One day in milliseconds (the unit of `[archive]`).
pub const DAY_MS: i64 = 24 * 60 * 60 * 1000;

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct Config {
    pub terminal: TerminalConfig,
    pub persist: PersistConfig,
    pub archive: ArchiveConfig,
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
    /// `auto` (default) | `zsh`: interactive zsh sessions get the zsh
    /// integration (`shell_integration.rs`); `none` turns it off. Other
    /// values turn it off too (with a warning).
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

/// `[archive]` (DESIGN §17.4): the periodic scan of `archive.rs`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct ArchiveConfig {
    /// Archive sessions idle for more than this many days: any session that
    /// is not live, and a live one only when a plain shell sits idle at its
    /// prompt (it is killed first). 0 = never (DESIGN §17.1).
    pub auto_after_days: Days,
    /// Purge sessions archived for more than this many days, like
    /// `Request::Delete`. 0 = never.
    pub purge_after_days: Days,
}

impl Default for ArchiveConfig {
    fn default() -> Self {
        Self {
            auto_after_days: Days::whole(7),
            purge_after_days: Days::OFF,
        }
    }
}

impl ArchiveConfig {
    /// Idle time (ms) beyond which a session is archived; `None` = never.
    pub fn auto_after_ms(&self) -> Option<i64> {
        self.auto_after_days.ms()
    }

    /// Time archived (ms) beyond which a session is purged; `None` = never.
    pub fn purge_after_ms(&self) -> Option<i64> {
        self.purge_after_days.ms()
    }
}

/// A number of days in `[archive]`: whole or fractional (`0.5` = 12 h,
/// `0.0001` = 8.64 s), never negative; 0 turns the rule off. Held in ms
/// (rounded, at least 1 ms when positive) so `Config` stays `Eq`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Days {
    ms: i64,
}

impl Days {
    /// 0: the rule is off.
    pub const OFF: Days = Days { ms: 0 };

    /// `days` whole days (cannot overflow: `u32::MAX` days are about
    /// 3.7e17 ms).
    pub const fn whole(days: u32) -> Days {
        Days {
            ms: days as i64 * DAY_MS,
        }
    }

    /// `days` days: finite and not negative. Beyond `i64::MAX` ms it
    /// saturates (never reached).
    pub fn new(days: f64) -> Result<Days, String> {
        if !days.is_finite() || days < 0.0 {
            return Err(format!("{days} is not a number of days (0 or more)"));
        }
        let ms = (days * DAY_MS as f64).round() as i64;
        Ok(Days {
            ms: if days > 0.0 { ms.max(1) } else { 0 },
        })
    }

    /// The span in ms; `None` when off.
    pub fn ms(self) -> Option<i64> {
        (self.ms > 0).then_some(self.ms)
    }
}

impl<'de> Deserialize<'de> for Days {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Days, D::Error> {
        // Integers are accepted too (`auto_after_days = 7`).
        Days::new(f64::deserialize(d)?).map_err(serde::de::Error::custom)
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
        if !matches!(
            config.terminal.shell_integration.as_str(),
            "auto" | "zsh" | "none"
        ) {
            tracing::warn!(
                value = %config.terminal.shell_integration,
                "unknown terminal.shell_integration (auto | zsh | none); integration off"
            );
        }
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

    /// Whether interactive zsh sessions get the shell integration.
    pub fn shell_integration(&self) -> bool {
        matches!(self.terminal.shell_integration.as_str(), "auto" | "zsh")
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
        // DESIGN §17.4: archive after a week, never purge.
        assert_eq!(c.archive.auto_after_days, Days::whole(7));
        assert_eq!(c.archive.auto_after_ms(), Some(7 * DAY_MS));
        assert_eq!(c.archive.purge_after_days, Days::OFF);
        assert_eq!(c.archive.purge_after_ms(), None);
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
[archive]
auto_after_days = 3
purge_after_days = 30
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
            c.archive,
            ArchiveConfig {
                auto_after_days: Days::whole(3),
                purge_after_days: Days::whole(30)
            }
        );
        assert_eq!(c.archive.purge_after_ms(), Some(30 * DAY_MS));
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

    /// `[archive]` like the other sections: missing keys keep their
    /// defaults, 0 turns each part off, a value that is not a day count
    /// breaks the file (then `load` falls back to all defaults).
    #[test]
    fn archive_section_defaults_zero_and_bad_values() {
        let c = Config::parse("[archive]\npurge_after_days = 14\n").unwrap();
        assert_eq!(
            c.archive.auto_after_days,
            Days::whole(7),
            "missing key keeps its default"
        );
        assert_eq!(c.archive.purge_after_ms(), Some(14 * DAY_MS));
        let off = Config::parse("[archive]\nauto_after_days = 0\n").unwrap();
        assert_eq!(off.archive.auto_after_ms(), None);
        assert_eq!(off.archive.purge_after_ms(), None);
        assert_eq!(
            Config::parse("[archive]\nauto_after_days = 0.0\n").unwrap(),
            off
        );
        // Fractions of a day (e.g. a threshold of seconds for a check).
        let short =
            Config::parse("[archive]\nauto_after_days = 0.0001\npurge_after_days = 1.5\n").unwrap();
        assert_eq!(short.archive.auto_after_ms(), Some(8_640));
        assert_eq!(short.archive.purge_after_ms(), Some(36 * 60 * 60 * 1000));
        let tiny = Config::parse("[archive]\nauto_after_days = 1e-12\n").unwrap();
        assert_eq!(
            tiny.archive.auto_after_ms(),
            Some(1),
            "positive is never off"
        );
        let huge = Config::parse("[archive]\nauto_after_days = 1e300\n").unwrap();
        assert_eq!(huge.archive.auto_after_ms(), Some(i64::MAX));
        for bad in [
            "[archive]\nauto_after_days = -1\n",
            "[archive]\npurge_after_days = -0.5\n",
            "[archive]\nauto_after_days = inf\n",
            "[archive]\nauto_after_days = nan\n",
            "[archive]\nauto_after_days = \"7\"\n",
        ] {
            assert!(Config::parse(bad).is_err(), "{bad}");
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[archive]\nauto_after_days = -1\n").unwrap();
        assert_eq!(Config::load(&path).archive, ArchiveConfig::default());
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
