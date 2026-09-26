//! `~/.codex/config.toml`: the top-level `notify` argv goes through
//! berth-hook — `["<berth-hook>", "codex"]`, or with an existing notify
//! program `["<berth-hook>", "codex", "--chain", <original argv...>]`
//! (berth-hook then execs the original with the same arguments). Edited
//! with toml_edit: comments and formatting elsewhere stay as they are.

use toml_edit::{Array, DocumentMut, Item, Value};

use super::claude::Change;
use super::shell;

/// Parse errors show a position only: toml's messages quote the file.
fn parse(text: &str) -> Result<DocumentMut, String> {
    text.parse::<DocumentMut>().map_err(|e| {
        let at = e.span().map_or(0, |s| s.start);
        let line = text[..at.min(text.len())].matches('\n').count() + 1;
        format!("不是有效的 TOML（第 {line} 行附近；详情含文件内容，不显示）")
    })
}

/// The top-level `notify` as strings; `Ok(None)` when absent.
fn notify(doc: &DocumentMut) -> Result<Option<Vec<String>>, String> {
    let Some(item) = doc.get("notify") else {
        return Ok(None);
    };
    let array = item
        .as_value()
        .and_then(Value::as_array)
        .ok_or("\"notify\" 不是数组；berth 只能串接 argv 形式的 notify")?;
    array
        .iter()
        .map(|v| {
            v.as_str()
                .map(str::to_owned)
                .ok_or_else(|| "\"notify\" 里有非字符串元素".to_string())
        })
        .collect::<Result<Vec<_>, _>>()
        .map(Some)
}

fn is_berth(argv: &[String]) -> bool {
    argv.len() >= 2 && shell::is_berth_hook(&argv[0]) && argv[1] == "codex"
}

/// Replace the `notify` value, keeping its key and surrounding comments.
fn set_notify(doc: &mut DocumentMut, argv: &[String]) {
    let mut array = Array::new();
    for a in argv {
        array.push(a.as_str());
    }
    match doc.get_mut("notify").and_then(Item::as_value_mut) {
        Some(old) => {
            let decor = old.decor().clone();
            let mut new = Value::Array(array);
            *new.decor_mut() = decor;
            *old = new;
        }
        None => {
            doc.insert("notify", toml_edit::value(array));
        }
    }
}

/// The berth-hook path of an installed notify, if any.
pub fn installed_hook(text: &str) -> Option<String> {
    let argv = notify(&parse(text).ok()?).ok()??;
    is_berth(&argv).then(|| argv[0].clone())
}

/// `text` with notify going through `hook` (idempotent; an older
/// berth-hook path is replaced).
pub fn install(text: &str, hook: &str) -> Result<Change, String> {
    let mut doc = parse(text)?;
    let mut notes = Vec::new();
    let wanted: Vec<String> = match notify(&doc)? {
        Some(argv) if is_berth(&argv) => {
            if argv[0] == hook {
                return Ok(Change {
                    text: text.to_owned(),
                    notes,
                });
            }
            notes.push("notify 已经过另一个路径的 berth-hook：改为当前路径".into());
            std::iter::once(hook.to_owned())
                .chain(argv[1..].iter().cloned())
                .collect()
        }
        Some(argv) if !argv.is_empty() => {
            notes.push("已有 notify 程序：berth-hook 转发后以 --chain 原样执行它".into());
            [hook, "codex", "--chain"]
                .into_iter()
                .map(str::to_owned)
                .chain(argv)
                .collect()
        }
        _ => vec![hook.to_owned(), "codex".to_owned()],
    };
    set_notify(&mut doc, &wanted);
    let after = doc.to_string();
    verify(text, &after, Some(&wanted))?;
    Ok(Change { text: after, notes })
}

/// `text` with the original notify back (or none, when berth's was the only
/// one).
pub fn uninstall(text: &str) -> Result<Change, String> {
    let mut doc = parse(text)?;
    let Some(argv) = notify(&doc)?.filter(|a| is_berth(a)) else {
        return Ok(Change {
            text: text.to_owned(),
            notes: Vec::new(),
        });
    };
    let original: Vec<String> = match argv.get(2).map(String::as_str) {
        Some("--chain") | Some("--") => argv[3..].to_vec(),
        _ => Vec::new(),
    };
    let note = if original.is_empty() {
        doc.remove("notify");
        "删除 berth 的 notify"
    } else {
        set_notify(&mut doc, &original);
        "notify 恢复为原程序"
    };
    let after = doc.to_string();
    let expect = (!original.is_empty()).then_some(original);
    verify(text, &after, expect.as_ref())?;
    Ok(Change {
        text: after,
        notes: vec![note.into()],
    })
}

/// The result parses, has `notify` = `wanted`, and nothing else changed.
fn verify(before: &str, after: &str, wanted: Option<&Vec<String>>) -> Result<(), String> {
    let mut a: toml::Table = before
        .parse()
        .map_err(|_| "internal error: original not TOML".to_string())?;
    let mut b: toml::Table = after
        .parse()
        .map_err(|_| "internal error: result not TOML".to_string())?;
    let got = b.remove("notify");
    a.remove("notify");
    if a != b {
        return Err("internal error: the edit would change more than notify".into());
    }
    let want =
        wanted.map(|w| toml::Value::Array(w.iter().cloned().map(toml::Value::String).collect()));
    if got != want {
        return Err("internal error: notify not as intended".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) const CONFIG: &str = r#"# Codex settings
model = "gpt-5.5"   # default model
approval_policy = "on-request"

# run after each turn
notify = ["terminal-notifier", "-title", "Codex"]  # keep this

[mcp_servers.docs]
command = "npx"
args = ["-y", "docs-mcp"]
env = { DOCS_TOKEN = "not-a-real-token" }

[profiles.fast]
model = "gpt-5.5-mini"
"#;

    #[test]
    fn existing_notify_is_chained_and_everything_else_kept() {
        let out = install(CONFIG, "/opt/berth/bin/berth-hook").unwrap();
        let changed: Vec<(&str, &str)> = CONFIG
            .lines()
            .zip(out.text.lines())
            .filter(|(a, b)| a != b)
            .collect();
        assert_eq!(
            changed,
            [(
                r#"notify = ["terminal-notifier", "-title", "Codex"]  # keep this"#,
                r#"notify = ["/opt/berth/bin/berth-hook", "codex", "--chain", "terminal-notifier", "-title", "Codex"]  # keep this"#
            )]
        );
        assert_eq!(CONFIG.lines().count(), out.text.lines().count());
        assert!(out.notes[0].contains("--chain"));
        assert_eq!(
            install(&out.text, "/opt/berth/bin/berth-hook")
                .unwrap()
                .text,
            out.text
        );
        assert_eq!(uninstall(&out.text).unwrap().text, CONFIG);
        assert_eq!(
            installed_hook(&out.text).as_deref(),
            Some("/opt/berth/bin/berth-hook")
        );
        // Another berth-hook path is replaced, the chain kept.
        let moved = install(&out.text, "/new/berth-hook").unwrap();
        assert!(moved
            .text
            .contains(r#"["/new/berth-hook", "codex", "--chain", "terminal-notifier""#));
    }

    #[test]
    fn missing_notify_is_added_at_top_level() {
        let text = "model = \"m\"\n\n[profiles.x]\nmodel = \"y\"\n";
        let out = install(text, "/x/berth-hook").unwrap();
        let doc: toml::Table = out.text.parse().unwrap();
        assert_eq!(
            doc["notify"],
            toml::Value::Array(vec!["/x/berth-hook".into(), "codex".into()])
        );
        assert!(doc["profiles"]["x"].get("notify").is_none());
        let back = uninstall(&out.text).unwrap();
        assert_eq!(back.text, text);
        // From an empty (new) file.
        let out = install("", "/x/berth-hook").unwrap();
        assert_eq!(out.text, "notify = [\"/x/berth-hook\", \"codex\"]\n");
        assert_eq!(uninstall(&out.text).unwrap().text, "");
    }

    #[test]
    fn verification_refuses_collateral_changes() {
        let want = vec!["/h".to_string(), "codex".to_string()];
        let good = "model = \"m\"\nnotify = [\"/h\", \"codex\"]\n";
        assert!(verify("model = \"m\"\n", good, Some(&want)).is_ok());
        assert!(verify("model = \"x\"\n", good, Some(&want)).is_err());
        assert!(verify("model = \"m\"\n", good, None).is_err());
    }

    #[test]
    fn unusable_notify_is_refused_without_quoting_the_file() {
        assert!(install("notify = \"x\"\n", "/h")
            .unwrap_err()
            .contains("不是数组"));
        assert!(install("notify = [1]\n", "/h")
            .unwrap_err()
            .contains("非字符串"));
        let err = install("token = \"sk-secret\"\n[[\n", "/h").unwrap_err();
        assert!(err.contains("第 2 行"), "{err}");
        assert!(!err.contains("sk-secret"));
        // An empty notify is treated as none.
        let out = install("notify = []\n", "/h").unwrap();
        assert!(out.text.contains("[\"/h\", \"codex\"]"));
    }
}
