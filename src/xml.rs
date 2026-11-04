//! `xml.etree.ElementTree.fromstring` as 2.3.0 uses it: a namespace-aware, non-validating XML
//! reader producing tags in Clark notation (`{uri}local`), attributes, and `text`. Errors are
//! worded as expat's (`ParseError: <message>: line L, column C`), positions as expat reports
//! them for the common cases.

use std::collections::HashMap;

pub struct Element {
    pub tag: String,
    pub attrs: Vec<(String, String)>,
    pub text: Option<String>,
    pub children: Vec<Element>,
}

impl Element {
    pub fn get(&self, name: &str) -> Option<&str> {
        self.attrs.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }

    /// `elem.iter(tag)`: pre-order, including `self`.
    pub fn iter_tag<'a>(&'a self, tag: &str, out: &mut Vec<&'a Element>) {
        if self.tag == tag {
            out.push(self);
        }
        for c in &self.children {
            c.iter_tag(tag, out);
        }
    }

    /// `elem.find(tag)`: the first direct child with this tag.
    pub fn find(&self, tag: &str) -> Option<&Element> {
        self.children.iter().find(|c| c.tag == tag)
    }
}

const XML_NS: &str = "http://www.w3.org/XML/1998/namespace";
/// Marks undecodable input; U+FFFF is not an XML character, so it fails as an invalid token.
const BAD: char = '\u{FFFF}';

/// An exception from `ET.fromstring`: ParseError, or what the unknown-encoding handler raised.
#[derive(Debug)]
pub struct XmlErr {
    pub class: &'static str,
    pub msg: String,
}

fn parse_error(msg: String) -> XmlErr {
    XmlErr { class: "ParseError", msg }
}

/// `_Py_normalize_encoding` (then `encodings.normalize_encoding`, which leaves it unchanged).
fn normalize_codec(name: &str) -> String {
    let mut out = String::new();
    let mut punct = false;
    for b in name.bytes() {
        if b.is_ascii_alphanumeric() || b == b'.' {
            if punct && !out.is_empty() {
                out.push('_');
            }
            punct = false;
            out.push(b.to_ascii_lowercase() as char);
        } else {
            punct = true;
        }
    }
    out
}

fn table_get<T: Copy>(table: &[(&str, T)], key: &str) -> Option<T> {
    table.binary_search_by(|(k, _)| (*k).cmp(key)).ok().map(|i| table[i].1)
}

/// `encodings.search_function`: alias, then the module of that name.
fn codec_module(name: &str) -> Option<i32> {
    use crate::tables::{CODEC_ALIASES, CODEC_MODULES};
    let norm = normalize_codec(name);
    let alias = table_get(CODEC_ALIASES, &norm).or_else(|| table_get(CODEC_ALIASES, &norm.replace('.', "_")));
    let mut candidates: Vec<&str> = Vec::new();
    if let Some(a) = alias {
        candidates.push(a);
    }
    candidates.push(&norm);
    for m in candidates {
        if m.is_empty() || m.contains('.') {
            continue;
        }
        if let Some(v) = table_get(CODEC_MODULES, m) {
            return Some(v);
        }
    }
    None
}

/// Bytes in an encoding expat does not know, as pyexpat's handler maps them.
fn decode_codec(name: &str, body: &[u8]) -> Result<Vec<char>, XmlErr> {
    use crate::tables::{CODEC_ERRORS, CODEC_MAPS};
    match codec_module(name) {
        None => Err(XmlErr { class: "LookupError", msg: format!("unknown encoding: {}", name) }),
        Some(-2) => Err(XmlErr {
            class: "LookupError",
            msg: format!("'{}' is not a text encoding; use codecs.decode() to handle arbitrary codecs", name),
        }),
        Some(-1) => Err(XmlErr { class: "ValueError", msg: "multi-byte encodings are not supported".into() }),
        Some(i) if i <= -10 => {
            let (class, msg) = CODEC_ERRORS[(-10 - i) as usize];
            Err(XmlErr { class: if class == "UnicodeError" { "UnicodeError" } else { "ValueError" }, msg: msg.into() })
        }
        Some(i) => {
            let map = &CODEC_MAPS[i as usize];
            let mut out = Vec::with_capacity(body.len());
            for &b in body {
                match char::from_u32(map[b as usize] as u32).filter(|_| map[b as usize] >= 0) {
                    Some(c) => out.push(c),
                    None => {
                        out.push(BAD);
                        break;
                    }
                }
            }
            Ok(out)
        }
    }
}

fn decode(data: &[u8]) -> Result<Vec<char>, XmlErr> {
    let mut chars: Vec<char> = Vec::with_capacity(data.len());
    if let Some(rest) = data.strip_prefix(b"\xff\xfe").or_else(|| data.strip_prefix(b"\xfe\xff")) {
        let be = data[0] == 0xFE;
        let units: Vec<u16> = rest.chunks(2).map(|c| if c.len() < 2 { 0xDC00 } else if be { u16::from_be_bytes([c[0], c[1]]) } else { u16::from_le_bytes([c[0], c[1]]) }).collect();
        for r in char::decode_utf16(units) {
            match r {
                Ok(c) => chars.push(c),
                Err(_) => {
                    chars.push(BAD);
                    break;
                }
            }
        }
        return Ok(chars);
    }
    let body = data.strip_prefix(b"\xef\xbb\xbf").unwrap_or(data);
    match std::str::from_utf8(body) {
        Ok(s) => chars.extend(s.chars()),
        Err(e) => {
            // Everything up to the first undecodable byte, then a marker the parser rejects.
            chars.extend(std::str::from_utf8(&body[..e.valid_up_to()]).unwrap().chars());
            chars.push(BAD);
        }
    }
    Ok(chars)
}

fn is_xml_char(c: char) -> bool {
    matches!(c as u32, 0x9 | 0xA | 0xD | 0x20..=0xD7FF | 0xE000..=0xFFFD | 0x10000..=0x10FFFF)
}

fn is_name_start(c: char) -> bool {
    matches!(c as u32, 0x3A | 0x41..=0x5A | 0x5F | 0x61..=0x7A | 0xC0..=0xD6 | 0xD8..=0xF6 | 0xF8..=0x2FF | 0x370..=0x37D
        | 0x37F..=0x1FFF | 0x200C..=0x200D | 0x2070..=0x218F | 0x2C00..=0x2FEF | 0x3001..=0xD7FF | 0xF900..=0xFDCF
        | 0xFDF0..=0xFFFD | 0x10000..=0xEFFFF)
}

fn is_name_char(c: char) -> bool {
    is_name_start(c) || matches!(c as u32, 0x2D | 0x2E | 0x30..=0x39 | 0xB7 | 0x300..=0x36F | 0x203F..=0x2040)
}

fn is_ws(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\r')
}

struct Parser {
    s: Vec<char>,
    i: usize,
    entities: HashMap<String, String>,
}

struct Open {
    raw: String,
    element: Element,
    scope: Vec<(String, Option<String>)>,
}

type R<T> = Result<T, (&'static str, usize)>;

impl Parser {
    fn at(&self, i: usize) -> Option<char> {
        self.s.get(i).copied()
    }

    fn starts(&self, i: usize, text: &str) -> bool {
        let mut j = i;
        for c in text.chars() {
            if self.at(j) != Some(c) {
                return false;
            }
            j += 1;
        }
        true
    }

    fn position(&self, i: usize) -> (usize, usize) {
        let before = &self.s[..i.min(self.s.len())];
        let line = before.iter().filter(|c| **c == '\n').count() + 1;
        let col = match before.iter().rposition(|c| *c == '\n') {
            Some(nl) => i - nl - 1,
            None => i,
        };
        (line, col)
    }

    fn skip_ws(&mut self) {
        while self.at(self.i).is_some_and(is_ws) {
            self.i += 1;
        }
    }

    /// A name at the cursor; errors at the first offending character.
    fn name(&mut self) -> R<String> {
        let start = self.i;
        match self.at(self.i) {
            Some(c) if is_name_start(c) && c != ':' => {}
            Some(_) => return Err(("not well-formed (invalid token)", self.i)),
            None => return Err(("unclosed token", start)),
        }
        while self.at(self.i).is_some_and(is_name_char) {
            self.i += 1;
        }
        let name: String = self.s[start..self.i].iter().collect();
        // Namespace processing: at most one colon, with a name on both sides.
        if let Some(colon) = name.find(':') {
            let after = &name[colon + 1..];
            if after.is_empty() || after.contains(':') || !after.chars().next().is_some_and(is_name_start) {
                let bad = start + name[..colon].chars().count() + 1 + after.find(':').map_or(0, |k| after[..k].chars().count());
                return Err(("not well-formed (invalid token)", bad.min(self.i)));
            }
        }
        Ok(name)
    }

    /// `&...;` at the cursor (cursor on `&`); appends the replacement text.
    fn reference(&mut self, out: &mut String, depth: usize) -> R<()> {
        let amp = self.i;
        self.i += 1;
        if self.at(self.i) == Some('#') {
            self.i += 1;
            let hex = self.at(self.i) == Some('x');
            if hex {
                self.i += 1;
            }
            let start = self.i;
            while self.at(self.i).is_some_and(|c| if hex { c.is_ascii_hexdigit() } else { c.is_ascii_digit() }) {
                self.i += 1;
            }
            if self.i == start || self.at(self.i) != Some(';') {
                return Err(("not well-formed (invalid token)", self.i));
            }
            let digits: String = self.s[start..self.i].iter().collect();
            self.i += 1;
            let code = u32::from_str_radix(&digits, if hex { 16 } else { 10 }).ok();
            match code.and_then(char::from_u32).filter(|c| is_xml_char(*c)) {
                Some(c) => out.push(c),
                None => return Err(("reference to invalid character number", amp)),
            }
            return Ok(());
        }
        match self.at(self.i) {
            Some(c) if is_name_start(c) && c != ':' => {}
            None => return Err(("unclosed token", amp)),
            Some(_) => return Err(("not well-formed (invalid token)", self.i)),
        }
        let start = self.i;
        while self.at(self.i).is_some_and(|c| is_name_char(c) && c != ':') {
            self.i += 1;
        }
        if self.at(self.i) != Some(';') {
            return Err(("not well-formed (invalid token)", self.i));
        }
        let name: String = self.s[start..self.i].iter().collect();
        self.i += 1;
        match name.as_str() {
            "lt" => out.push('<'),
            "gt" => out.push('>'),
            "amp" => out.push('&'),
            "quot" => out.push('"'),
            "apos" => out.push('\''),
            _ => match self.entities.get(&name).cloned() {
                Some(value) if depth < 16 => {
                    let mut inner = Parser { s: value.chars().collect(), i: 0, entities: self.entities.clone() };
                    while inner.i < inner.s.len() {
                        if inner.s[inner.i] == '&' {
                            inner.reference(out, depth + 1).map_err(|(m, _)| (m, amp))?;
                        } else {
                            out.push(inner.s[inner.i]);
                            inner.i += 1;
                        }
                    }
                }
                Some(_) => return Err(("recursive entity reference", amp)),
                None => return Err(("undefined entity", amp)),
            },
        }
        Ok(())
    }

    /// `<!-- ... -->`, cursor on `<`.
    fn comment(&mut self) -> R<()> {
        let start = self.i;
        self.i += 4;
        loop {
            match self.at(self.i) {
                None => return Err(("unclosed token", start)),
                Some('-') if self.starts(self.i, "--") => {
                    if self.starts(self.i, "-->") {
                        self.i += 3;
                        return Ok(());
                    }
                    return Err(("not well-formed (invalid token)", self.i + 2));
                }
                Some(c) if !is_xml_char(c) => return Err(("not well-formed (invalid token)", self.i)),
                _ => self.i += 1,
            }
        }
    }

    /// expat `prologTok` for the text at `i` (not `<`, not whitespace): Ok when it is a token
    /// the grammar then rejects, Err(position) when tokenizing fails.
    fn prolog_token(&self, i: usize) -> Result<(), usize> {
        let c = self.at(i).unwrap();
        let ends_name = |c: Option<char>| matches!(c, None | Some('>' | ')' | ',' | '|' | '[' | '%' | ' ' | '\t' | '\n' | '\r' | '+' | '*' | '?'));
        match c {
            '"' | '\'' => {
                let mut j = i + 1;
                loop {
                    match self.at(j) {
                        None => return Ok(()),
                        Some(d) if !is_xml_char(d) => return Err(j),
                        Some(d) if d == c => break,
                        _ => j += 1,
                    }
                }
                match self.at(j + 1) {
                    None | Some(' ' | '\t' | '\n' | '\r' | '>' | '%' | '[') => Ok(()),
                    Some(_) => Err(j + 1),
                }
            }
            '(' | ')' | '[' | ']' | ',' | '|' | '>' => Ok(()),
            '%' => match self.at(i + 1) {
                None | Some(' ' | '\t' | '\n' | '\r' | '%') => Ok(()),
                Some(d) if is_name_start(d) && d != ':' => Ok(()),
                Some(_) => Err(i + 1),
            },
            '#' => {
                if !self.at(i + 1).is_some_and(|d| is_name_start(d) && d != ':') {
                    return Err(i + 1);
                }
                let mut j = i + 2;
                while self.at(j).is_some_and(|d| is_name_char(d) && d != ':') {
                    j += 1;
                }
                if matches!(self.at(j), None | Some(' ' | '\t' | '\n' | '\r' | ')' | '>' | '%' | '|')) { Ok(()) } else { Err(j) }
            }
            _ if is_name_char(c) => {
                let mut j = i + 1;
                while self.at(j).is_some_and(is_name_char) {
                    j += 1;
                }
                if ends_name(self.at(j)) { Ok(()) } else { Err(j) }
            }
            _ => Err(i),
        }
    }

    /// `<!` that starts none of `patterns`: expat reports the first character that diverges.
    fn bad_markup(&self, patterns: &[&str]) -> (&'static str, usize) {
        let common = patterns
            .iter()
            .map(|p| p.chars().zip(self.s[self.i..].iter()).take_while(|(a, b)| a == *b).count())
            .max()
            .unwrap_or(0);
        if self.i + common >= self.s.len() {
            ("unclosed token", self.i)
        } else {
            ("not well-formed (invalid token)", self.i + common)
        }
    }

    /// The XML declaration at the very start (expat `doParseXmlDecl`): Some((encoding, offset))
    /// when it names an encoding. Other leading PIs are left to `document`.
    fn xml_decl(&mut self) -> R<Option<(String, usize)>> {
        if !self.starts(0, "<?") {
            return Ok(None);
        }
        let mut j = 2;
        while self.at(j).is_some_and(is_name_char) {
            j += 1;
        }
        let target: String = self.s[2..j].iter().collect();
        if target != "xml" || !matches!(self.at(j), Some(c) if is_ws(c) || c == '?') {
            return Ok(None);
        }
        let mut k = j;
        let end = loop {
            match self.at(k) {
                None => return Err(("unclosed token", 0)),
                Some('?') if self.at(k + 1) == Some('>') => break k,
                Some(c) if !is_xml_char(c) => return Err(("not well-formed (invalid token)", k)),
                _ => k += 1,
            }
        };
        let declared = parse_decl(&self.s, 5, end).map_err(|bad| ("XML declaration not well-formed", bad))?;
        self.i = end + 2;
        Ok(declared)
    }

    /// `<?target ...?>`, cursor on `<`.
    fn pi(&mut self) -> R<()> {
        let start = self.i;
        self.i += 2;
        let name = self.name()?;
        let after = self.i;
        match self.at(after) {
            None => return Err(("unclosed token", start)),
            Some(c) if is_ws(c) || c == '?' => {}
            Some(_) => return Err(("not well-formed (invalid token)", after)),
        }
        if name == "xml" {
            return Err(("XML or text declaration not at start of entity", start));
        }
        if name.eq_ignore_ascii_case("xml") {
            return Err(("not well-formed (invalid token)", after));
        }
        loop {
            match self.at(self.i) {
                None => return Err(("unclosed token", start)),
                Some('?') if self.at(self.i + 1) == Some('>') => {
                    self.i += 2;
                    return Ok(());
                }
                Some(c) if !is_xml_char(c) => return Err(("not well-formed (invalid token)", self.i)),
                _ => self.i += 1,
            }
        }
    }

    /// `<!DOCTYPE ...>`: skipped, except internal general entities with literal values.
    fn doctype(&mut self) -> R<()> {
        let start = self.i;
        let mut depth = 0usize;
        while let Some(c) = self.at(self.i) {
            if c == '[' {
                depth += 1;
            } else if c == ']' {
                depth = depth.saturating_sub(1);
            } else if c == '<' && depth > 0 && self.starts(self.i, "<!ENTITY") {
                let save = self.i;
                self.i += 8;
                self.skip_ws();
                if self.at(self.i) != Some('%') {
                    if let Ok(name) = self.name() {
                        self.skip_ws();
                        if let Some(q @ ('"' | '\'')) = self.at(self.i) {
                            let vstart = self.i + 1;
                            if let Some(len) = self.s[vstart..].iter().position(|c| *c == q) {
                                let value: String = self.s[vstart..vstart + len].iter().collect();
                                self.entities.entry(name).or_insert(value);
                                self.i = vstart + len + 1;
                                continue;
                            }
                        }
                    }
                }
                self.i = save + 1;
                continue;
            } else if c == '>' && depth == 0 {
                self.i += 1;
                return Ok(());
            }
            self.i += 1;
        }
        Err(("unclosed token", start))
    }

    fn resolve(scopes: &[Vec<(String, Option<String>)>], prefix: &str) -> Option<Option<String>> {
        for scope in scopes.iter().rev() {
            for (p, uri) in scope.iter().rev() {
                if p == prefix {
                    return Some(uri.clone());
                }
            }
        }
        None
    }

    /// Start tag at the cursor (on `<`): returns the element, its raw name, its namespace
    /// declarations, and whether it was self-closing.
    fn start_tag(&mut self, scopes: &[Vec<(String, Option<String>)>]) -> R<(Element, String, Vec<(String, Option<String>)>, bool)> {
        let lt = self.i;
        self.i += 1;
        let raw = self.name()?;
        let mut raw_attrs: Vec<(String, String, usize)> = Vec::new();
        let closed;
        loop {
            let had_ws = self.at(self.i).is_some_and(is_ws);
            self.skip_ws();
            match self.at(self.i) {
                None => return Err(("unclosed token", lt)),
                Some('>') => {
                    self.i += 1;
                    closed = false;
                    break;
                }
                Some('/') => {
                    if self.at(self.i + 1) == Some('>') {
                        self.i += 2;
                        closed = true;
                        break;
                    }
                    return Err(match self.at(self.i + 1) {
                        None => ("unclosed token", lt),
                        Some(_) => ("not well-formed (invalid token)", self.i + 1),
                    });
                }
                Some(_) if !had_ws => return Err(("not well-formed (invalid token)", self.i)),
                Some(_) => {
                    let name_at = self.i;
                    let name = self.name()?;
                    self.skip_ws();
                    match self.at(self.i) {
                        Some('=') => self.i += 1,
                        None => return Err(("unclosed token", lt)),
                        Some(_) => return Err(("not well-formed (invalid token)", self.i)),
                    }
                    self.skip_ws();
                    let quote = match self.at(self.i) {
                        Some(q @ ('"' | '\'')) => q,
                        None => return Err(("unclosed token", lt)),
                        Some(_) => return Err(("not well-formed (invalid token)", self.i)),
                    };
                    self.i += 1;
                    let mut value = String::new();
                    loop {
                        match self.at(self.i) {
                            None => return Err(("unclosed token", lt)),
                            Some(c) if c == quote => {
                                self.i += 1;
                                break;
                            }
                            Some('<') => return Err(("not well-formed (invalid token)", self.i)),
                            Some('&') => self.reference(&mut value, 0)?,
                            Some(c) if !is_xml_char(c) => return Err(("not well-formed (invalid token)", self.i)),
                            Some(c) => {
                                value.push(if is_ws(c) { ' ' } else { c });
                                self.i += 1;
                            }
                        }
                    }
                    if raw_attrs.iter().any(|(n, _, _)| *n == name) {
                        return Err(("duplicate attribute", name_at));
                    }
                    raw_attrs.push((name, value, name_at));
                }
            }
        }
        // Namespace declarations first, then names.
        let mut decls: Vec<(String, Option<String>)> = Vec::new();
        for (name, value, _) in &raw_attrs {
            if name == "xmlns" {
                decls.push((String::new(), if value.is_empty() { None } else { Some(value.clone()) }));
            } else if let Some(prefix) = name.strip_prefix("xmlns:") {
                if value.is_empty() {
                    return Err(("must not undeclare prefix", lt));
                }
                decls.push((prefix.to_string(), Some(value.clone())));
            }
        }
        let mut all: Vec<Vec<(String, Option<String>)>> = scopes.to_vec();
        all.push(decls.clone());
        let qualify = |name: &str, element: bool| -> Option<String> {
            match name.split_once(':') {
                Some(("xml", local)) => Some(format!("{{{}}}{}", XML_NS, local)),
                Some((prefix, local)) => match Self::resolve(&all, prefix) {
                    Some(Some(uri)) => Some(format!("{{{}}}{}", uri, local)),
                    _ => None,
                },
                None if element => match Self::resolve(&all, "") {
                    Some(Some(uri)) => Some(format!("{{{}}}{}", uri, name)),
                    _ => Some(name.to_string()),
                },
                None => Some(name.to_string()),
            }
        };
        let tag = qualify(&raw, true).ok_or(("unbound prefix", lt))?;
        let mut attrs: Vec<(String, String)> = Vec::new();
        for (name, value, at) in raw_attrs {
            if name == "xmlns" || name.starts_with("xmlns:") {
                continue;
            }
            let key = qualify(&name, false).ok_or(("unbound prefix", lt))?;
            if attrs.iter().any(|(k, _)| *k == key) {
                return Err(("duplicate attribute", at));
            }
            attrs.push((key, value));
        }
        Ok((Element { tag, attrs, text: None, children: Vec::new() }, raw, decls, closed))
    }

    fn document(&mut self) -> R<Element> {
        let mut seen_root = false;
        let mut root: Option<Element> = None;
        let mut stack: Vec<Open> = Vec::new();
        let mut text = String::new();
        loop {
            if stack.is_empty() {
                // Prolog or epilog: whitespace, comments, PIs (and a DOCTYPE before the root).
                self.skip_ws();
                match self.at(self.i) {
                    None => {
                        return root.ok_or(("no element found", self.i));
                    }
                    Some('<') if self.starts(self.i, "<!--") => self.comment()?,
                    Some('<') if self.starts(self.i, "<?") => self.pi()?,
                    Some('<') if self.starts(self.i, "<!DOCTYPE") && !seen_root => self.doctype()?,
                    Some('<') if self.starts(self.i, "<!") => {
                        let next = self.at(self.i + 2);
                        if next == Some('[') || next.is_some_and(|c| is_name_start(c) && c != ':') {
                            return Err(if seen_root { ("junk after document element", self.i) } else { ("syntax error", self.i) });
                        }
                        return Err(self.bad_markup(&["<!--"]));
                    }
                    Some('<') if !seen_root => {
                        let scopes: Vec<Vec<(String, Option<String>)>> = Vec::new();
                        let (element, raw, decls, closed) = self.start_tag(&scopes)?;
                        seen_root = true;
                        if closed {
                            root = Some(element);
                        } else {
                            stack.push(Open { raw, element, scope: decls });
                        }
                    }
                    Some(_) => {
                        return Err(match self.prolog_token(self.i) {
                            Err(at) => ("not well-formed (invalid token)", at),
                            Ok(()) if seen_root => ("junk after document element", self.i),
                            Ok(()) => ("syntax error", self.i),
                        });
                    }
                }
                continue;
            }
            match self.at(self.i) {
                None => return Err(("no element found", self.i)),
                Some('<') => {
                    if self.starts(self.i, "<!--") {
                        self.comment()?;
                    } else if self.starts(self.i, "<![CDATA[") {
                        let start = self.i;
                        self.i += 9;
                        loop {
                            match self.at(self.i) {
                                None => {
                                    let _ = start;
                                    return Err(("unclosed CDATA section", self.s.len()));
                                }
                                Some(']') if self.starts(self.i, "]]>") => {
                                    self.i += 3;
                                    break;
                                }
                                Some(c) if !is_xml_char(c) => return Err(("not well-formed (invalid token)", self.i)),
                                Some(c) => {
                                    text.push(c);
                                    self.i += 1;
                                }
                            }
                        }
                    } else if self.starts(self.i, "<!") {
                        return Err(self.bad_markup(&["<!--", "<![CDATA["]));
                    } else if self.starts(self.i, "<?") {
                        self.pi()?;
                    } else if self.starts(self.i, "</") {
                        self.i += 2;
                        let name_at = self.i;
                        match self.at(self.i) {
                            Some(c) if is_name_start(c) && c != ':' => {}
                            Some(_) => return Err(("not well-formed (invalid token)", self.i)),
                            None => return Err(("unclosed token", name_at - 2)),
                        }
                        while self.at(self.i).is_some_and(is_name_char) {
                            self.i += 1;
                        }
                        let name: String = self.s[name_at..self.i].iter().collect();
                        self.skip_ws();
                        match self.at(self.i) {
                            Some('>') => self.i += 1,
                            None => return Err(("unclosed token", name_at - 2)),
                            Some(_) => return Err(("not well-formed (invalid token)", self.i)),
                        }
                        let open = stack.pop().unwrap();
                        if open.raw != name {
                            return Err(("mismatched tag", name_at));
                        }
                        let mut element = open.element;
                        if element.children.is_empty() && !text.is_empty() {
                            element.text = Some(std::mem::take(&mut text));
                        }
                        text.clear();
                        match stack.last_mut() {
                            Some(parent) => parent.element.children.push(element),
                            None => root = Some(element),
                        }
                    } else {
                        let scopes: Vec<Vec<(String, Option<String>)>> = stack.iter().map(|o| o.scope.clone()).collect();
                        if let Some(parent) = stack.last_mut() {
                            if parent.element.children.is_empty() && parent.element.text.is_none() && !text.is_empty() {
                                parent.element.text = Some(std::mem::take(&mut text));
                            }
                        }
                        text.clear();
                        let (element, raw, decls, closed) = self.start_tag(&scopes)?;
                        if closed {
                            stack.last_mut().unwrap().element.children.push(element);
                        } else {
                            stack.push(Open { raw, element, scope: decls });
                        }
                    }
                }
                Some('&') => {
                    let mut value = String::new();
                    self.reference(&mut value, 0)?;
                    text.push_str(&value);
                }
                Some(']') if self.starts(self.i, "]]>") => return Err(("not well-formed (invalid token)", self.i + 2)),
                Some(c) if !is_xml_char(c) => return Err(("not well-formed (invalid token)", self.i)),
                Some(c) => {
                    text.push(c);
                    self.i += 1;
                }
            }
        }
    }
}

/// Line ends normalise to \n before parsing, as expat does.
fn normalize_newlines(raw: Vec<char>) -> Vec<char> {
    let mut s = Vec::with_capacity(raw.len());
    let mut k = 0;
    while k < raw.len() {
        if raw[k] == '\r' {
            s.push('\n');
            if raw.get(k + 1) == Some(&'\n') {
                k += 1;
            }
        } else {
            s.push(raw[k]);
        }
        k += 1;
    }
    s
}

/// expat `parsePseudoAttribute`: Ok(None) at the end, Ok(Some((name, name_end, value, next))).
fn pseudo_attr(s: &[char], mut p: usize, end: usize) -> Result<Option<(usize, usize, usize, usize)>, usize> {
    let ascii = |i: usize| -> i32 {
        if i < end && (s[i] as u32) < 0x80 {
            s[i] as i32
        } else {
            -1
        }
    };
    let space = |c: i32| matches!(c, 0x20 | 0x09 | 0x0A | 0x0D);
    if p == end {
        return Ok(None);
    }
    if !space(ascii(p)) {
        return Err(p);
    }
    loop {
        p += 1;
        if !space(ascii(p)) {
            break;
        }
    }
    if p == end {
        return Ok(None);
    }
    let name = p;
    let name_end;
    loop {
        let c = ascii(p);
        if c == -1 {
            return Err(p);
        }
        if c == '=' as i32 {
            name_end = p;
            break;
        }
        if space(c) {
            name_end = p;
            loop {
                p += 1;
                if !space(ascii(p)) {
                    break;
                }
            }
            if ascii(p) != '=' as i32 {
                return Err(p);
            }
            break;
        }
        p += 1;
    }
    if p == name {
        return Err(p);
    }
    p += 1;
    while space(ascii(p)) {
        p += 1;
    }
    let open = ascii(p);
    if open != '"' as i32 && open != '\'' as i32 {
        return Err(p);
    }
    p += 1;
    let value = p;
    loop {
        let c = ascii(p);
        if c == open {
            break;
        }
        let ok = (c >= 'a' as i32 && c <= 'z' as i32) || (c >= 'A' as i32 && c <= 'Z' as i32) || (c >= '0' as i32 && c <= '9' as i32)
            || c == '.' as i32 || c == '-' as i32 || c == '_' as i32;
        if !ok {
            return Err(p);
        }
        p += 1;
    }
    Ok(Some((name, name_end, value, p + 1)))
}

/// expat `doParseXmlDecl` over s[start..end) (`end` is the `?` of `?>`).
fn parse_decl(s: &[char], start: usize, end: usize) -> Result<Option<(String, usize)>, usize> {
    let word = |a: usize, b: usize| -> String { s[a..b].iter().collect() };
    let Some((mut name, mut name_end, _, mut p)) = pseudo_attr(s, start, end)? else { return Err(start) };
    let mut value;
    if word(name, name_end) != "version" {
        return Err(name);
    }
    match pseudo_attr(s, p, end)? {
        None => return Ok(None),
        Some(next) => (name, name_end, value, p) = next,
    }
    let mut encoding = None;
    if word(name, name_end) == "encoding" {
        if !s.get(value).is_some_and(|c| c.is_ascii_alphabetic()) {
            return Err(value);
        }
        encoding = Some((word(value, p - 1), value));
        match pseudo_attr(s, p, end)? {
            None => return Ok(encoding),
            Some(next) => (name, name_end, value, p) = next,
        }
    }
    if word(name, name_end) != "standalone" {
        return Err(name);
    }
    let flag = word(value, p - 1);
    if flag != "yes" && flag != "no" {
        return Err(value);
    }
    while p < end && matches!(s[p], ' ' | '\t' | '\n' | '\r') {
        p += 1;
    }
    if p != end {
        return Err(p);
    }
    Ok(encoding)
}

/// `ET.fromstring(data)`.
pub fn fromstring(data: &[u8]) -> Result<Element, XmlErr> {
    let utf16 = data.starts_with(b"\xff\xfe") || data.starts_with(b"\xfe\xff");
    let body = data.strip_prefix(b"\xef\xbb\xbf").unwrap_or(data);
    let mut p = Parser { s: normalize_newlines(decode(data)?), i: 0, entities: HashMap::new() };
    let fail = |p: &Parser, (msg, at): (&'static str, usize)| {
        let (line, col) = p.position(at);
        parse_error(format!("{}: line {}, column {}", msg, line, col))
    };
    let declared = p.xml_decl().map_err(|e| fail(&p, e))?;
    if let (Some((name, at)), false) = (declared, utf16) {
        let redecoded = match name.to_ascii_uppercase().as_str() {
            "UTF-8" => None,
            "ISO-8859-1" => Some(body.iter().map(|&c| c as char).collect::<Vec<char>>()),
            "US-ASCII" => Some(body.iter().map(|&c| if c < 0x80 { c as char } else { BAD }).collect()),
            "UTF-16" | "UTF-16BE" | "UTF-16LE" => return Err(fail(&p, ("encoding specified in XML declaration is incorrect", at))),
            _ => Some(decode_codec(&name, body)?),
        };
        if let Some(chars) = redecoded {
            let at = p.i;
            p.s = normalize_newlines(chars);
            p.i = at;
        }
    }
    p.document().map_err(|e| fail(&p, e))
}
