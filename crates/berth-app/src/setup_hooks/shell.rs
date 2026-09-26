//! Just enough POSIX `sh` word splitting to recognise berth-hook commands
//! and to decide whether a status line command can be wrapped as is.

use std::ops::Range;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Word {
    /// The word after quote removal.
    pub text: String,
    /// Its bytes in the command string, quotes included.
    pub span: Range<usize>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Words {
    pub words: Vec<Word>,
    /// Something beyond "a program and its arguments": a pipe, list,
    /// redirection, subshell, command substitution, comment or newline.
    pub compound: bool,
}

/// Split `s` like `sh` would split a simple command. `None`: unterminated
/// quote or a trailing backslash.
pub fn split(s: &str) -> Option<Words> {
    let mut words = Vec::new();
    let mut compound = false;
    let mut cur: Option<(String, usize)> = None;
    let mut chars = s.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        match c {
            ' ' | '\t' => {
                if let Some((text, start)) = cur.take() {
                    words.push(Word {
                        text,
                        span: start..i,
                    });
                }
            }
            '\n' | '|' | '&' | ';' | '<' | '>' | '(' | ')' | '`' => {
                compound = true;
                cur.get_or_insert_with(|| (String::new(), i)).0.push(c);
            }
            '#' if cur.is_none() => {
                compound = true;
                cur = Some((c.to_string(), i));
            }
            '$' => {
                if chars.peek().is_some_and(|(_, n)| *n == '(') {
                    compound = true;
                }
                cur.get_or_insert_with(|| (String::new(), i)).0.push(c);
            }
            '\\' => {
                let (_, next) = chars.next()?;
                let word = cur.get_or_insert_with(|| (String::new(), i));
                if next != '\n' {
                    word.0.push(next);
                }
            }
            '\'' => {
                let word = cur.get_or_insert_with(|| (String::new(), i));
                loop {
                    match chars.next()? {
                        (_, '\'') => break,
                        (_, ch) => word.0.push(ch),
                    }
                }
            }
            '"' => {
                let word = cur.get_or_insert_with(|| (String::new(), i));
                loop {
                    match chars.next()? {
                        (_, '"') => break,
                        (_, '\\') => {
                            let (_, next) = chars.next()?;
                            if matches!(next, '$' | '`' | '"' | '\\') {
                                word.0.push(next);
                            } else if next != '\n' {
                                word.0.push('\\');
                                word.0.push(next);
                            }
                        }
                        (_, '`') => {
                            compound = true;
                            word.0.push('`');
                        }
                        (_, '$') => {
                            if chars.peek().is_some_and(|(_, n)| *n == '(') {
                                compound = true;
                            }
                            word.0.push('$');
                        }
                        (_, ch) => word.0.push(ch),
                    }
                }
            }
            _ => cur.get_or_insert_with(|| (String::new(), i)).0.push(c),
        }
    }
    if let Some((text, start)) = cur {
        words.push(Word {
            text,
            span: start..s.len(),
        });
    }
    Some(Words { words, compound })
}

/// `s` as one `sh` word: unchanged when it only has characters no shell
/// treats specially, else single-quoted.
pub fn quote(s: &str) -> String {
    let plain = !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "_./:@%+=,-".contains(c));
    if plain {
        s.to_owned()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

/// Whether `word` names a `berth-hook` executable (any directory).
pub fn is_berth_hook(word: &str) -> bool {
    std::path::Path::new(word)
        .file_name()
        .is_some_and(|n| n == "berth-hook")
}

/// Shell keywords and builtins without an executable of the same name:
/// commands starting with them only work inside a shell.
const SHELL_ONLY: &[&str] = &[
    "!", "{", "}", "[[", "]]", "if", "then", "else", "elif", "fi", "for", "while", "until", "do",
    "done", "case", "esac", "select", "function", "time", "cd", ".", "source", "exec", "eval",
    "export", "set", "unset", "alias", "unalias", "read", "trap", "return", "exit", "shift",
    "local", "declare", "typeset", "readonly", "builtin", "command", "ulimit", "umask", "wait",
    "jobs", "fg", "bg",
];

/// Whether `words` can run without a shell, as argv: no compound syntax, no
/// leading `NAME=value` assignment, no shell keyword or builtin first.
pub fn is_plain_command(words: &Words) -> bool {
    let Some(first) = words.words.first() else {
        return false;
    };
    let assignment = first.text.split_once('=').is_some_and(|(name, _)| {
        !name.is_empty()
            && !name.starts_with(|c: char| c.is_ascii_digit())
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    });
    !words.compound && !assignment && !SHELL_ONLY.contains(&first.text.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(s: &str) -> Vec<String> {
        split(s)
            .unwrap()
            .words
            .into_iter()
            .map(|w| w.text)
            .collect()
    }

    #[test]
    fn splits_like_sh() {
        assert_eq!(texts("a  b\tc"), ["a", "b", "c"]);
        assert_eq!(
            texts(r#"'/x y/berth-hook' claude"#),
            ["/x y/berth-hook", "claude"]
        );
        assert_eq!(texts(r#""a \"b\" \$c \d" e\ f"#), [r#"a "b" $c \d"#, "e f"]);
        assert_eq!(texts("x'y'\"z\""), ["xyz"]);
        let w = split("  ab 'c d'").unwrap();
        assert_eq!(w.words[0].span, 2..4);
        assert_eq!(w.words[1].span, 5..10);
        assert!(split("'open").is_none());
        assert!(split("a\\").is_none());
    }

    #[test]
    fn compound_commands_are_recognised() {
        for s in [
            "a | b",
            "a && b",
            "a; b",
            "a > f",
            "(a)",
            "a $(b)",
            "a `b`",
            "a \"$(b)\"",
            "a # c",
            "a\nb",
        ] {
            assert!(split(s).unwrap().compound, "{s}");
        }
        for s in ["a b", "a '|' \"&&\"", "a $HOME ~/x", "a#b"] {
            assert!(!split(s).unwrap().compound, "{s}");
        }
        let plain = |s: &str| is_plain_command(&split(s).unwrap());
        assert!(plain("~/.claude/statusline.sh --x"));
        assert!(plain("npx -y ccusage statusline"));
        assert!(!plain("FOO=1 cmd"));
        assert!(!plain("cd ~ && ./s"));
        assert!(!plain("source x"));
        assert!(!plain(""));
    }

    #[test]
    fn quoting_round_trips() {
        for s in [
            "/usr/local/bin/berth-hook",
            "/Users/me/My Apps/berth-hook",
            "it's",
            "",
            "$HOME",
            "~x",
        ] {
            let q = quote(s);
            assert_eq!(texts(&q), [s], "{q}");
        }
        assert_eq!(quote("/a/b-c_d.e"), "/a/b-c_d.e");
        assert!(is_berth_hook("/opt/x/berth-hook"));
        assert!(is_berth_hook("berth-hook"));
        assert!(!is_berth_hook("/opt/berth-hook/other"));
    }
}
