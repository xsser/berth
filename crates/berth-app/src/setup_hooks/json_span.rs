//! Editing a JSON document in place: a reader that records where every
//! value is, byte ranges to replace, and helpers that add or remove array
//! items / object members in the document's own layout. Everything outside
//! the edited ranges keeps its exact bytes.
//!
//! Input is expected to be valid JSON already (the caller checks it with
//! serde_json first, which also produces the error position); this reader
//! still refuses anything it does not understand.

use std::ops::Range;

#[derive(Clone, Debug)]
pub struct Node {
    /// The value's bytes, brackets and quotes included.
    pub span: Range<usize>,
    pub kind: Kind,
}

#[derive(Clone, Debug)]
pub enum Kind {
    Object(Vec<Member>),
    Array(Vec<Node>),
    String(String),
    /// Number, `true`, `false`, `null`.
    Scalar,
}

#[derive(Clone, Debug)]
pub struct Member {
    pub key: String,
    /// Where the key's opening quote is.
    pub start: usize,
    pub value: Node,
}

impl Node {
    pub fn members(&self) -> Option<&[Member]> {
        match &self.kind {
            Kind::Object(m) => Some(m),
            _ => None,
        }
    }

    pub fn items(&self) -> Option<&[Node]> {
        match &self.kind {
            Kind::Array(items) => Some(items),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match &self.kind {
            Kind::String(s) => Some(s),
            _ => None,
        }
    }

    /// The member `key` of an object (the last one, as serde_json reads a
    /// duplicate key).
    pub fn get(&self, key: &str) -> Option<&Node> {
        self.members()?
            .iter()
            .rev()
            .find(|m| m.key == key)
            .map(|m| &m.value)
    }

    /// Keys that occur more than once in this object.
    pub fn duplicate_keys(&self) -> Vec<&str> {
        let mut seen = std::collections::HashSet::new();
        let mut dups = Vec::new();
        for m in self.members().unwrap_or_default() {
            if !seen.insert(m.key.as_str()) && !dups.contains(&m.key.as_str()) {
                dups.push(m.key.as_str());
            }
        }
        dups
    }

    /// Inside the brackets.
    fn inner(&self) -> Range<usize> {
        self.span.start + 1..self.span.end - 1
    }
}

const MAX_DEPTH: usize = 128;

pub fn parse(text: &str) -> Result<Node, String> {
    let mut p = Reader {
        text,
        bytes: text.as_bytes(),
        pos: 0,
    };
    p.skip_ws();
    let node = p.value(0)?;
    p.skip_ws();
    if p.pos != p.bytes.len() {
        return Err(p.error("unexpected data after the document"));
    }
    Ok(node)
}

struct Reader<'a> {
    text: &'a str,
    bytes: &'a [u8],
    pos: usize,
}

impl Reader<'_> {
    fn error(&self, what: &str) -> String {
        format!("{what} at byte {}", self.pos)
    }

    fn skip_ws(&mut self) {
        while matches!(self.bytes.get(self.pos), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.pos += 1;
        }
    }

    fn eat(&mut self, b: u8) -> bool {
        if self.bytes.get(self.pos) == Some(&b) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn expect(&mut self, b: u8) -> Result<(), String> {
        if self.eat(b) {
            Ok(())
        } else {
            Err(self.error(&format!("expected `{}`", b as char)))
        }
    }

    fn value(&mut self, depth: usize) -> Result<Node, String> {
        if depth > MAX_DEPTH {
            return Err(self.error("nested too deeply"));
        }
        let start = self.pos;
        let kind = match self.bytes.get(self.pos) {
            Some(b'{') => {
                self.pos += 1;
                let mut members = Vec::new();
                self.skip_ws();
                if !self.eat(b'}') {
                    loop {
                        self.skip_ws();
                        let key_start = self.pos;
                        let key = self.string()?;
                        self.skip_ws();
                        self.expect(b':')?;
                        self.skip_ws();
                        let value = self.value(depth + 1)?;
                        members.push(Member {
                            key,
                            start: key_start,
                            value,
                        });
                        self.skip_ws();
                        if self.eat(b',') {
                            continue;
                        }
                        self.expect(b'}')?;
                        break;
                    }
                }
                Kind::Object(members)
            }
            Some(b'[') => {
                self.pos += 1;
                let mut items = Vec::new();
                self.skip_ws();
                if !self.eat(b']') {
                    loop {
                        self.skip_ws();
                        items.push(self.value(depth + 1)?);
                        self.skip_ws();
                        if self.eat(b',') {
                            continue;
                        }
                        self.expect(b']')?;
                        break;
                    }
                }
                Kind::Array(items)
            }
            Some(b'"') => Kind::String(self.string()?),
            Some(_) => {
                while let Some(&b) = self.bytes.get(self.pos) {
                    if matches!(b, b',' | b'}' | b']' | b' ' | b'\t' | b'\n' | b'\r') {
                        break;
                    }
                    self.pos += 1;
                }
                if self.pos == start {
                    return Err(self.error("expected a value"));
                }
                Kind::Scalar
            }
            None => return Err(self.error("unexpected end")),
        };
        Ok(Node {
            span: start..self.pos,
            kind,
        })
    }

    fn string(&mut self) -> Result<String, String> {
        self.expect(b'"')?;
        let mut out = String::new();
        loop {
            match self.bytes.get(self.pos) {
                None => return Err(self.error("unterminated string")),
                Some(b'"') => {
                    self.pos += 1;
                    return Ok(out);
                }
                Some(b'\\') => {
                    self.pos += 1;
                    let esc = *self
                        .bytes
                        .get(self.pos)
                        .ok_or_else(|| self.error("unterminated escape"))?;
                    self.pos += 1;
                    match esc {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{8}'),
                        b'f' => out.push('\u{c}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let first = self.hex4()?;
                            let mut units = vec![first];
                            if (0xd800..0xdc00).contains(&first)
                                && self.bytes.get(self.pos..self.pos + 2) == Some(b"\\u")
                            {
                                self.pos += 2;
                                units.push(self.hex4()?);
                            }
                            for c in char::decode_utf16(units) {
                                out.push(c.unwrap_or(char::REPLACEMENT_CHARACTER));
                            }
                        }
                        _ => return Err(self.error("invalid escape")),
                    }
                }
                Some(_) => {
                    let c = self.text[self.pos..]
                        .chars()
                        .next()
                        .ok_or_else(|| self.error("invalid UTF-8"))?;
                    out.push(c);
                    self.pos += c.len_utf8();
                }
            }
        }
    }

    fn hex4(&mut self) -> Result<u16, String> {
        let digits = self
            .text
            .get(self.pos..self.pos + 4)
            .ok_or_else(|| self.error("short \\u escape"))?;
        let v = u16::from_str_radix(digits, 16).map_err(|_| self.error("invalid \\u escape"))?;
        self.pos += 4;
        Ok(v)
    }
}

/// Byte ranges to replace, applied together.
#[derive(Debug, Default)]
pub struct Edits(Vec<(Range<usize>, String)>);

impl Edits {
    pub fn replace(&mut self, range: Range<usize>, text: String) {
        self.0.push((range, text));
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn extend(&mut self, other: Edits) {
        self.0.extend(other.0);
    }

    /// Ranges must not overlap or start at the same byte (the order of two
    /// insertions at one place would be ambiguous).
    pub fn apply(mut self, text: &str) -> Result<String, String> {
        self.0.sort_by_key(|(r, _)| (r.start, r.end));
        for pair in self.0.windows(2) {
            if pair[1].0.start < pair[0].0.end || pair[1].0.start == pair[0].0.start {
                return Err("internal error: overlapping edits".into());
            }
        }
        let mut out = String::with_capacity(text.len() + 1024);
        let mut done = 0;
        for (range, replacement) in &self.0 {
            out.push_str(&text[done..range.start]);
            out.push_str(replacement);
            done = range.end;
        }
        out.push_str(&text[done..]);
        Ok(out)
    }
}

/// How new JSON is laid out: the document's indentation unit, line ending
/// and whether it is pretty-printed at all.
#[derive(Clone, Debug)]
pub struct Layout {
    pub unit: String,
    pub newline: &'static str,
    pub multiline: bool,
}

impl Layout {
    /// From the document: the top object's first member line gives the
    /// unit (default two spaces); a one-line non-empty top object means a
    /// compact document.
    pub fn of(text: &str, root: &Node) -> Layout {
        let newline = if text.contains("\r\n") { "\r\n" } else { "\n" };
        let members = root.members().unwrap_or_default();
        let multiline = members.is_empty() || spans_lines(text, &root.span);
        let unit = members
            .first()
            .map(|m| line_indent(text, m.start))
            .filter(|u| !u.is_empty() && multiline)
            .unwrap_or("  ")
            .to_owned();
        Layout {
            unit,
            newline,
            multiline,
        }
    }
}

/// Leading whitespace of the line that contains byte `pos`.
pub fn line_indent(text: &str, pos: usize) -> &str {
    let line_start = text[..pos].rfind('\n').map_or(0, |i| i + 1);
    let rest = &text[line_start..];
    let len = rest.len() - rest.trim_start_matches([' ', '\t']).len();
    &rest[..len]
}

fn spans_lines(text: &str, range: &Range<usize>) -> bool {
    text[range.clone()].contains('\n')
}

/// Values berth writes, in the order given.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum J {
    Str(String),
    Arr(Vec<J>),
    Obj(Vec<(String, J)>),
}

fn json_string(s: &str) -> String {
    serde_json::to_string(s).expect("a string always serialises")
}

impl J {
    /// This value as text starting on a line indented by `base`.
    pub fn render(&self, layout: &Layout, base: &str) -> String {
        if !layout.multiline {
            return self.compact();
        }
        let nl = layout.newline;
        let inner = format!("{base}{}", layout.unit);
        match self {
            J::Str(s) => json_string(s),
            J::Arr(items) if items.is_empty() => "[]".into(),
            J::Obj(members) if members.is_empty() => "{}".into(),
            J::Arr(items) => {
                let body: Vec<String> = items
                    .iter()
                    .map(|v| format!("{inner}{}", v.render(layout, &inner)))
                    .collect();
                format!("[{nl}{}{nl}{base}]", body.join(&format!(",{nl}")))
            }
            J::Obj(members) => {
                let body: Vec<String> = members
                    .iter()
                    .map(|(k, v)| {
                        format!("{inner}{}: {}", json_string(k), v.render(layout, &inner))
                    })
                    .collect();
                format!("{{{nl}{}{nl}{base}}}", body.join(&format!(",{nl}")))
            }
        }
    }

    fn compact(&self) -> String {
        match self {
            J::Str(s) => json_string(s),
            J::Arr(items) => {
                let body: Vec<String> = items.iter().map(J::compact).collect();
                format!("[{}]", body.join(", "))
            }
            J::Obj(members) => {
                let body: Vec<String> = members
                    .iter()
                    .map(|(k, v)| format!("{}: {}", json_string(k), v.compact()))
                    .collect();
                format!("{{{}}}", body.join(", "))
            }
        }
    }
}

/// Append `item` to array `arr`, laid out like its existing items.
pub fn append_item(edits: &mut Edits, text: &str, layout: &Layout, arr: &Node, item: &J) {
    let items = arr.items().unwrap_or_default();
    match items.last() {
        Some(last) if layout.multiline && spans_lines(text, &arr.span) => {
            let ind = line_indent(text, last.span.start);
            let at = last.span.end;
            edits.replace(
                at..at,
                format!(",{}{ind}{}", layout.newline, item.render(layout, ind)),
            );
        }
        Some(last) => {
            let at = last.span.end;
            edits.replace(at..at, format!(", {}", item.compact()));
        }
        None => fill_empty(
            edits,
            text,
            layout,
            arr,
            vec![item.render_at(layout, text, arr)],
        ),
    }
}

impl J {
    /// Rendered as the first line inside the empty container `parent`.
    fn render_at(&self, layout: &Layout, text: &str, parent: &Node) -> String {
        let inner = format!("{}{}", line_indent(text, parent.span.start), layout.unit);
        self.render(layout, &inner)
    }
}

/// Append members to object `obj`, laid out like its existing members.
pub fn append_members(
    edits: &mut Edits,
    text: &str,
    layout: &Layout,
    obj: &Node,
    new: &[(String, J)],
) {
    if new.is_empty() {
        return;
    }
    let members = obj.members().unwrap_or_default();
    match members.last() {
        Some(last) if layout.multiline && spans_lines(text, &obj.span) => {
            let ind = line_indent(text, last.start);
            let at = last.value.span.end;
            let body: String = new
                .iter()
                .map(|(k, v)| {
                    format!(
                        ",{}{ind}{}: {}",
                        layout.newline,
                        json_string(k),
                        v.render(layout, ind)
                    )
                })
                .collect();
            edits.replace(at..at, body);
        }
        Some(last) => {
            let at = last.value.span.end;
            let body: String = new
                .iter()
                .map(|(k, v)| format!(", {}: {}", json_string(k), v.compact()))
                .collect();
            edits.replace(at..at, body);
        }
        None => {
            let inner = format!("{}{}", line_indent(text, obj.span.start), layout.unit);
            let lines = new
                .iter()
                .map(|(k, v)| format!("{}: {}", json_string(k), v.render(layout, &inner)))
                .collect();
            fill_empty(edits, text, layout, obj, lines);
        }
    }
}

/// Put `entries` (already rendered for the first inner line) into the empty
/// container `node`.
fn fill_empty(edits: &mut Edits, text: &str, layout: &Layout, node: &Node, entries: Vec<String>) {
    let outer = line_indent(text, node.span.start);
    let body = if layout.multiline {
        let nl = layout.newline;
        let inner = format!("{outer}{}", layout.unit);
        let lines: Vec<String> = entries.iter().map(|e| format!("{inner}{e}")).collect();
        format!("{nl}{}{nl}{outer}", lines.join(&format!(",{nl}")))
    } else {
        entries.join(", ")
    };
    edits.replace(node.inner(), body);
}

/// Remove the children at `remove` (ascending) of the array or object
/// `node`, with the separators that belong to them.
pub fn remove_children(edits: &mut Edits, node: &Node, remove: &[usize]) {
    let spans: Vec<Range<usize>> = match &node.kind {
        Kind::Array(items) => items.iter().map(|n| n.span.clone()).collect(),
        Kind::Object(members) => members.iter().map(|m| m.start..m.value.span.end).collect(),
        _ => return,
    };
    let kept: Vec<usize> = (0..spans.len()).filter(|i| !remove.contains(i)).collect();
    let Some(&first_kept) = kept.first() else {
        if !spans.is_empty() {
            edits.replace(node.inner(), String::new());
        }
        return;
    };
    // Children before the first kept one go with the separator after them.
    if first_kept > 0 {
        edits.replace(spans[0].start..spans[first_kept].start, String::new());
    }
    // Later ones go with the separator before them.
    for &i in remove.iter().filter(|&&i| i > first_kept) {
        edits.replace(spans[i - 1].end..spans[i].end, String::new());
    }
}

/// Replace the string `node` with `value`.
pub fn replace_string(edits: &mut Edits, node: &Node, value: &str) {
    edits.replace(node.span.clone(), json_string(value));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn same_value(a: &str, b: &str) {
        let va: serde_json::Value = serde_json::from_str(a).unwrap();
        let vb: serde_json::Value = serde_json::from_str(b).unwrap();
        assert_eq!(va, vb, "\n{a}\n---\n{b}");
    }

    #[test]
    fn reader_records_spans_and_decodes_strings() {
        let text = r#" { "a" : [1, "x\"é😀", {"b": null}], "c": "" } "#;
        let root = parse(text).unwrap();
        assert_eq!(&text[root.span.clone()], text.trim());
        let a = root.get("a").unwrap();
        let items = a.items().unwrap();
        assert_eq!(&text[items[0].span.clone()], "1");
        assert_eq!(items[1].as_str(), Some("x\"é😀"));
        assert_eq!(&text[items[2].span.clone()], r#"{"b": null}"#);
        assert_eq!(
            root.members().unwrap()[1].start,
            text.find("\"c\"").unwrap()
        );
        assert!(parse("[1,]").is_err());
        assert!(parse("{} x").is_err());
        let deep = "[".repeat(200) + &"]".repeat(200);
        assert!(parse(&deep).is_err());
        let dup = parse(r#"{"a": 1, "a": 2, "b": 3}"#).unwrap();
        assert_eq!(dup.duplicate_keys(), ["a"]);
    }

    fn edit(text: &str, f: impl FnOnce(&mut Edits, &Layout, &Node)) -> String {
        let root = parse(text).unwrap();
        let layout = Layout::of(text, &root);
        let mut edits = Edits::default();
        f(&mut edits, &layout, &root);
        edits.apply(text).unwrap()
    }

    fn item() -> J {
        J::Obj(vec![("k".into(), J::Arr(vec![J::Str("v".into())]))])
    }

    #[test]
    fn append_follows_the_documents_layout() {
        let text = "{\n    \"a\": [\n        1\n    ],\n    \"e\": []\n}\n";
        let out = edit(text, |e, l, root| {
            append_item(e, text, l, root.get("a").unwrap(), &item());
            append_item(e, text, l, root.get("e").unwrap(), &J::Str("s".into()));
        });
        assert_eq!(
            out,
            "{\n    \"a\": [\n        1,\n        {\n            \"k\": [\n                \"v\"\n            ]\n        }\n    ],\n    \"e\": [\n        \"s\"\n    ]\n}\n"
        );
        // Compact documents stay compact.
        let text = r#"{"a":[1],"o":{}}"#;
        let out = edit(text, |e, l, root| {
            append_item(e, text, l, root.get("a").unwrap(), &J::Str("s".into()));
            append_members(e, text, l, root.get("o").unwrap(), &[("x".into(), item())]);
        });
        assert_eq!(out, r#"{"a":[1, "s"],"o":{"x": {"k": ["v"]}}}"#);
        same_value(&out, r#"{"a":[1,"s"],"o":{"x":{"k":["v"]}}}"#);
    }

    #[test]
    fn members_are_added_after_the_last_one_or_into_an_empty_object() {
        let text = "{\r\n\t\"a\": 1\r\n}";
        let out = edit(text, |e, l, root| {
            append_members(
                e,
                text,
                l,
                root,
                &[("b".into(), item()), ("c".into(), J::Str("z".into()))],
            );
        });
        assert_eq!(
            out,
            "{\r\n\t\"a\": 1,\r\n\t\"b\": {\r\n\t\t\"k\": [\r\n\t\t\t\"v\"\r\n\t\t]\r\n\t},\r\n\t\"c\": \"z\"\r\n}"
        );
        let text = "{}\n";
        let out = edit(text, |e, l, root| {
            append_members(e, text, l, root, &[("b".into(), J::Arr(vec![]))]);
        });
        assert_eq!(out, "{\n  \"b\": []\n}\n");
    }

    #[test]
    fn removal_keeps_separators_right() {
        let text = "[\n  1,\n  2,\n  3,\n  4\n]";
        for (remove, want) in [
            (vec![0], "[\n  2,\n  3,\n  4\n]"),
            (vec![3], "[\n  1,\n  2,\n  3\n]"),
            (vec![0, 1], "[\n  3,\n  4\n]"),
            (vec![1, 2], "[\n  1,\n  4\n]"),
            (vec![0, 2, 3], "[\n  2\n]"),
            (vec![0, 1, 2, 3], "[]"),
        ] {
            let out = edit(text, |e, _, root| remove_children(e, root, &remove));
            assert_eq!(out, want, "{remove:?}");
        }
        let text = r#"{"a": 1, "b": {"x": 2}, "c": 3}"#;
        let out = edit(text, |e, _, root| remove_children(e, root, &[1]));
        assert_eq!(out, r#"{"a": 1, "c": 3}"#);
        let out = edit(text, |e, _, root| remove_children(e, root, &[0, 2]));
        assert_eq!(out, r#"{"b": {"x": 2}}"#);
    }

    #[test]
    fn overlapping_edits_are_refused() {
        let mut e = Edits::default();
        e.replace(0..3, "x".into());
        e.replace(2..4, "y".into());
        assert!(e.apply("abcdef").is_err());
        let mut e = Edits::default();
        e.replace(1..1, "x".into());
        e.replace(1..1, "y".into());
        assert!(e.apply("abc").is_err());
    }
}
