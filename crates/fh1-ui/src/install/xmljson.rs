//! Generic XML -> JSON mapping (port of `convert_ui.py` `xml_obj`/`load_xml`) and a JSON writer that
//! reproduces Python's `json.dumps(obj, indent=1, ensure_ascii=False)` byte for byte (key order kept).
//!
//! Mapping: root `{ "<RootTag>": node }`; node = `@attr` strings (document order), `#text` (trimmed,
//! only the text before the first child, only if non-empty), then one array per child tag (first
//! appearance order). Comments are dropped. Bare `&` are escaped first (same rule as the Python `AMP`).

use crate::{Error, Result};

/// A JSON value with ordered objects. Numbers are kept pre-formatted.
pub enum Json {
    Null,
    Bool(bool),
    Raw(String),
    Str(String),
    Arr(Vec<Json>),
    Obj(Vec<(String, Json)>),
}

impl Json {
    pub fn int(v: i64) -> Json {
        Json::Raw(v.to_string())
    }

    pub fn float(v: f64) -> Json {
        let mut s = format!("{v}");
        if !s.contains(['.', 'e', 'E', 'n', 'i']) {
            s.push_str(".0");
        }
        Json::Raw(s)
    }

    pub fn str(v: &str) -> Json {
        Json::Str(v.to_string())
    }

    /// `json.dumps(obj, indent=1, ensure_ascii=False) + "\n"`.
    pub fn to_pretty(&self) -> String {
        let mut out = String::new();
        self.write(&mut out, 0);
        out.push('\n');
        out
    }

    fn write(&self, out: &mut String, depth: usize) {
        match self {
            Json::Null => out.push_str("null"),
            Json::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            Json::Raw(s) => out.push_str(s),
            Json::Str(s) => escape(out, s),
            Json::Arr(v) if v.is_empty() => out.push_str("[]"),
            Json::Arr(v) => {
                out.push('[');
                for (i, x) in v.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    newline(out, depth + 1);
                    x.write(out, depth + 1);
                }
                newline(out, depth);
                out.push(']');
            }
            Json::Obj(v) if v.is_empty() => out.push_str("{}"),
            Json::Obj(v) => {
                out.push('{');
                for (i, (k, x)) in v.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    newline(out, depth + 1);
                    escape(out, k);
                    out.push_str(": ");
                    x.write(out, depth + 1);
                }
                newline(out, depth);
                out.push('}');
            }
        }
    }
}

fn newline(out: &mut String, depth: usize) {
    out.push('\n');
    for _ in 0..depth {
        out.push(' ');
    }
}

fn escape(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct Node {
    pub attrs: Vec<(String, String)>,
    pub text: String,
    pub children: Vec<(String, Vec<Node>)>,
}

impl Node {
    fn to_json(&self) -> Json {
        let mut o = Vec::new();
        for (k, v) in &self.attrs {
            o.push((format!("@{k}"), Json::Str(v.clone())));
        }
        let t = self.text.trim();
        if !t.is_empty() {
            o.push(("#text".to_string(), Json::str(t)));
        }
        for (tag, nodes) in &self.children {
            o.push((tag.clone(), Json::Arr(nodes.iter().map(Node::to_json).collect())));
        }
        Json::Obj(o)
    }

    /// This node plus all descendants.
    #[cfg(test)]
    pub fn count(&self) -> usize {
        1 + self.children.iter().flat_map(|(_, v)| v).map(Node::count).sum::<usize>()
    }
}

pub struct Doc {
    pub root_tag: String,
    pub root: Node,
}

impl Doc {
    pub fn to_json(&self) -> Json {
        Json::Obj(vec![(self.root_tag.clone(), self.root.to_json())])
    }
}

/// Escape `&` that does not start `amp;|lt;|gt;|quot;|apos;|#123;|#x1F;`. Returns (text, repairs).
pub fn repair_amp(s: &str) -> (String, usize) {
    let mut out = String::with_capacity(s.len() + 16);
    let mut n = 0;
    let mut rest = s;
    while let Some(p) = rest.find('&') {
        out.push_str(&rest[..p]);
        let after = &rest[p + 1..];
        if valid_entity(after) {
            out.push('&');
        } else {
            out.push_str("&amp;");
            n += 1;
        }
        rest = after;
    }
    out.push_str(rest);
    (out, n)
}

fn valid_entity(after: &str) -> bool {
    for name in ["amp;", "lt;", "gt;", "quot;", "apos;"] {
        if after.starts_with(name) {
            return true;
        }
    }
    let Some(r) = after.strip_prefix('#') else { return false };
    let Some(end) = r.find(';') else { return false };
    let body = &r[..end];
    if let Some(h) = body.strip_prefix('x') {
        !h.is_empty() && h.bytes().all(|b| b.is_ascii_hexdigit())
    } else {
        !body.is_empty() && body.bytes().all(|b| b.is_ascii_digit())
    }
}

fn decode_entities(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(p) = rest.find('&') {
        out.push_str(&rest[..p]);
        let after = &rest[p + 1..];
        let Some(end) = after.find(';') else {
            out.push('&');
            rest = after;
            continue;
        };
        let name = &after[..end];
        let rep: Option<char> = match name {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            _ => name
                .strip_prefix("#x")
                .and_then(|h| u32::from_str_radix(h, 16).ok())
                .or_else(|| name.strip_prefix('#').and_then(|d| d.parse::<u32>().ok()))
                .and_then(char::from_u32),
        };
        match rep {
            Some(c) => {
                out.push(c);
                rest = &after[end + 1..];
            }
            None => {
                out.push('&');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

struct Parser<'a> {
    s: &'a str,
    i: usize,
}

fn bad(msg: &str, at: usize) -> Error {
    Error::Format(format!("xml: {msg} at byte {at}"))
}

impl Parser<'_> {
    fn rest(&self) -> &str {
        self.s.get(self.i..).unwrap_or("")
    }

    fn starts(&self, pat: &str) -> bool {
        self.rest().starts_with(pat)
    }

    fn skip_ws(&mut self) {
        while self.s.as_bytes().get(self.i).is_some_and(|b| b.is_ascii_whitespace()) {
            self.i += 1;
        }
    }

    /// Skip past `end`, starting at the current position.
    fn skip_to(&mut self, end: &str) -> Result<()> {
        match self.rest().find(end) {
            Some(p) => {
                self.i += p + end.len();
                Ok(())
            }
            None => Err(bad(&format!("missing `{end}`"), self.i)),
        }
    }

    fn name(&mut self) -> Result<String> {
        let start = self.i;
        while let Some(&b) = self.s.as_bytes().get(self.i) {
            if b.is_ascii_whitespace() || matches!(b, b'=' | b'/' | b'>' | b'<' | b'"' | b'\'') {
                break;
            }
            self.i += 1;
        }
        if self.i == start {
            return Err(bad("expected a name", start));
        }
        Ok(self.s[start..self.i].to_string())
    }

    fn element(&mut self, depth: usize) -> Result<(String, Node)> {
        if depth > 256 {
            return Err(bad("nesting too deep", self.i));
        }
        self.i += 1; // '<'
        let tag = self.name()?;
        let mut node = Node::default();
        loop {
            self.skip_ws();
            if self.starts("/>") {
                self.i += 2;
                return Ok((tag, node));
            }
            if self.starts(">") {
                self.i += 1;
                break;
            }
            let k = self.name()?;
            self.skip_ws();
            if !self.starts("=") {
                return Err(bad("expected `=`", self.i));
            }
            self.i += 1;
            self.skip_ws();
            let q = match self.s.as_bytes().get(self.i) {
                Some(&q) if q == b'"' || q == b'\'' => q as char,
                _ => return Err(bad("expected a quote", self.i)),
            };
            self.i += 1;
            let end = self.rest().find(q).ok_or_else(|| bad("unterminated attribute", self.i))?;
            let raw = self.rest()[..end].replace("\r\n", "\n");
            let norm: String = raw.chars().map(|c| if matches!(c, '\n' | '\r' | '\t') { ' ' } else { c }).collect();
            node.attrs.push((k, decode_entities(&norm)));
            self.i += end + 1;
        }
        let mut seen_child = false;
        loop {
            if self.i >= self.s.len() {
                return Err(bad("unexpected end of document", self.i));
            }
            if self.starts("</") {
                self.i += 2;
                let close = self.name()?;
                self.skip_ws();
                if !self.starts(">") || close != tag {
                    return Err(bad(&format!("mismatched `</{close}>` for `<{tag}>`"), self.i));
                }
                self.i += 1;
                return Ok((tag, node));
            } else if self.starts("<!--") {
                self.skip_to("-->")?;
            } else if self.starts("<![CDATA[") {
                self.i += 9;
                let end = self.rest().find("]]>").ok_or_else(|| bad("unterminated CDATA", self.i))?;
                if !seen_child {
                    node.text.push_str(&self.rest()[..end].replace("\r\n", "\n"));
                }
                self.i += end + 3;
            } else if self.starts("<?") {
                self.skip_to("?>")?;
            } else if self.starts("<") {
                let (ctag, child) = self.element(depth + 1)?;
                seen_child = true;
                match node.children.iter_mut().find(|(t, _)| *t == ctag) {
                    Some((_, v)) => v.push(child),
                    None => node.children.push((ctag, vec![child])),
                }
            } else {
                let end = self.rest().find('<').unwrap_or(self.rest().len());
                if !seen_child {
                    let raw = self.rest()[..end].replace("\r\n", "\n").replace('\r', "\n");
                    node.text.push_str(&decode_entities(&raw));
                }
                self.i += end;
            }
        }
    }
}

/// Parse one document (prolog comments/PIs/doctype skipped; trailing content ignored).
pub fn parse(text: &str) -> Result<Doc> {
    let mut p = Parser { s: text, i: 0 };
    loop {
        p.skip_ws();
        if p.starts("<?") {
            p.skip_to("?>")?;
        } else if p.starts("<!--") {
            p.skip_to("-->")?;
        } else if p.starts("<!") {
            p.skip_to(">")?;
        } else if p.starts("<") {
            let (root_tag, root) = p.element(0)?;
            return Ok(Doc { root_tag, root });
        } else {
            return Err(bad("no root element", p.i));
        }
    }
}

/// Source bytes -> (JSON text as written by the Python converter, number of bare `&` repaired).
pub fn convert(bytes: &[u8]) -> Result<(String, usize)> {
    let (doc, n) = parse_bytes(bytes)?;
    Ok((doc.to_json().to_pretty(), n))
}

/// Source bytes -> (parsed document, repairs).
pub fn parse_bytes(bytes: &[u8]) -> Result<(Doc, usize)> {
    let bytes = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF][..]).unwrap_or(bytes);
    let text = std::str::from_utf8(bytes).map_err(|e| Error::Format(format!("xml: not utf-8: {e}")))?;
    let (fixed, n) = repair_amp(text);
    Ok((parse(&fixed)?, n))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn json(xml: &str) -> String {
        convert(xml.as_bytes()).unwrap().0
    }

    #[test]
    fn attrs_text_children() {
        let s = json("<a x=\"1\" y='two'>  hi  <b/><b k=\"v\">t</b><c/></a>");
        let want = "{\n \"a\": {\n  \"@x\": \"1\",\n  \"@y\": \"two\",\n  \"#text\": \"hi\",\n  \"b\": [\n   {},\n   {\n    \"@k\": \"v\",\n    \"#text\": \"t\"\n   }\n  ],\n  \"c\": [\n   {}\n  ]\n }\n}\n";
        assert_eq!(s, want);
    }

    #[test]
    fn key_order_is_document_order() {
        let doc = parse("<r><z/><a/><z/><m/></r>").unwrap();
        let tags: Vec<_> = doc.root.children.iter().map(|(t, v)| (t.as_str(), v.len())).collect();
        assert_eq!(tags, vec![("z", 2), ("a", 1), ("m", 1)]);
        let doc = parse("<r b=\"1\" a=\"2\">x<c/></r>").unwrap();
        let keys: Vec<_> = match doc.to_json() {
            Json::Obj(o) => match &o[0].1 {
                Json::Obj(i) => i.iter().map(|(k, _)| k.clone()).collect(),
                _ => panic!(),
            },
            _ => panic!(),
        };
        assert_eq!(keys, ["@b", "@a", "#text", "c"]);
    }

    #[test]
    fn bare_ampersand_repaired() {
        let (t, n) = repair_amp("A & B &amp; C &#65; &#x41; &nbsp; &lt;");
        assert_eq!(n, 2);
        assert_eq!(t, "A &amp; B &amp; C &#65; &#x41; &amp;nbsp; &lt;");
        let doc = parse(&repair_amp("<l>Art & Design &lt;3 &#65;</l>").0).unwrap();
        assert_eq!(doc.root.text, "Art & Design <3 A");
    }

    #[test]
    fn comments_dropped_and_text_joined() {
        let doc = parse("<?xml version=\"1.0\"?>\n<!-- top --><r>a<!-- c !-->b<k/>tail</r>").unwrap();
        assert_eq!(doc.root.text, "ab");
        assert_eq!(doc.root.count(), 2);
    }

    #[test]
    fn bom_and_escapes() {
        let mut b = vec![0xEF, 0xBB, 0xBF];
        b.extend_from_slice("<r q=\"a\tb\">é\"\\</r>".as_bytes());
        let (s, _) = convert(&b).unwrap();
        assert_eq!(s, "{\n \"r\": {\n  \"@q\": \"a b\",\n  \"#text\": \"é\\\"\\\\\"\n }\n}\n");
    }

    #[test]
    fn errors_not_panics() {
        assert!(parse("<a><b></a>").is_err());
        assert!(parse("<a").is_err());
        assert!(parse("").is_err());
    }

    #[test]
    fn floats_like_python() {
        assert_eq!(Json::float(0.61).to_pretty(), "0.61\n");
        assert_eq!(Json::float(0.0).to_pretty(), "0.0\n");
    }
}
