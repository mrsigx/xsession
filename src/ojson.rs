//! Ordered JSON with Python `json` semantics, as xsession 2.3.0 behaves:
//! - `OV`/`Obj`: insertion-ordered values; setting an existing key replaces it in place
//!   (Python `dict` behaviour, including duplicate keys while parsing).
//! - `dumps`: byte-compatible with `json.dumps(value, ensure_ascii=...)` (default separators),
//!   including float repr, NaN/Infinity, and lone surrogates printed as `\udXXX`.
//! - `loads`/`loads_bytes`: accepts what `json.loads` accepts (NaN, Infinity, lone surrogates,
//!   any nesting depth; bytes are decoded the way `json.detect_encoding` + `surrogatepass` does).
//!
//! Lone UTF-16 surrogates cannot live in a Rust `String`; they are carried as the code points
//! U+10F800..U+10FFFF (plane-16 private use) and written back as `\udXXX`.

use std::fmt::Write as _;

pub const SURROGATE_BASE: u32 = 0x10F800;

#[derive(Clone, Debug, PartialEq)]
pub enum OV {
    Null,
    Bool(bool),
    Int(i64),
    UInt(u64),
    /// An integer outside 64 bits, kept as its decimal text (Python ints are unbounded).
    Big(String),
    Float(f64),
    Str(String),
    Arr(Vec<OV>),
    Obj(Obj),
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct Obj(pub Vec<(String, OV)>);

impl Obj {
    pub fn new() -> Obj {
        Obj(Vec::new())
    }
    pub fn get(&self, key: &str) -> Option<&OV> {
        self.0.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }
    pub fn get_mut(&mut self, key: &str) -> Option<&mut OV> {
        self.0.iter_mut().find(|(k, _)| k == key).map(|(_, v)| v)
    }
    pub fn contains(&self, key: &str) -> bool {
        self.0.iter().any(|(k, _)| k == key)
    }
    /// Python `d[key] = value`: replace in place, or append.
    pub fn set(&mut self, key: &str, value: OV) {
        match self.get_mut(key) {
            Some(slot) => *slot = value,
            None => self.0.push((key.to_string(), value)),
        }
    }
    /// Python `d.pop(key)`.
    pub fn remove(&mut self, key: &str) -> Option<OV> {
        let i = self.0.iter().position(|(k, _)| k == key)?;
        Some(self.0.remove(i).1)
    }
    pub fn str_of(&self, key: &str) -> Option<&str> {
        self.get(key).and_then(OV::as_str)
    }
    pub fn int_of(&self, key: &str) -> Option<i64> {
        self.get(key).and_then(OV::as_i64)
    }
    pub fn with(mut self, key: &str, value: OV) -> Obj {
        self.set(key, value);
        self
    }
}

impl OV {
    pub fn str(s: impl Into<String>) -> OV {
        OV::Str(s.into())
    }
    pub fn as_str(&self) -> Option<&str> {
        match self {
            OV::Str(s) => Some(s),
            _ => None,
        }
    }
    pub fn as_i64(&self) -> Option<i64> {
        match self {
            OV::Int(i) => Some(*i),
            OV::UInt(u) => i64::try_from(*u).ok(),
            _ => None,
        }
    }
    pub fn as_obj(&self) -> Option<&Obj> {
        match self {
            OV::Obj(o) => Some(o),
            _ => None,
        }
    }
    pub fn as_obj_mut(&mut self) -> Option<&mut Obj> {
        match self {
            OV::Obj(o) => Some(o),
            _ => None,
        }
    }
    pub fn as_arr(&self) -> Option<&Vec<OV>> {
        match self {
            OV::Arr(a) => Some(a),
            _ => None,
        }
    }
    pub fn is_null(&self) -> bool {
        matches!(self, OV::Null)
    }
    /// Python truthiness.
    pub fn truthy(&self) -> bool {
        match self {
            OV::Null => false,
            OV::Bool(b) => *b,
            OV::Int(i) => *i != 0,
            OV::UInt(u) => *u != 0,
            OV::Big(_) => true,
            OV::Float(f) => *f != 0.0,
            OV::Str(s) => !s.is_empty(),
            OV::Arr(a) => !a.is_empty(),
            OV::Obj(o) => !o.0.is_empty(),
        }
    }
}

// ---------------------------------------------------------------------------------- writing ---

pub fn dumps(value: &OV, ensure_ascii: bool) -> String {
    let mut out = String::with_capacity(256);
    write_value(&mut out, value, ensure_ascii, false);
    out
}

/// `json.dumps(value, ensure_ascii=False)` with lone surrogates left in place (as Python's str
/// holds them), for sizing and for `utf8_encode_error`.
pub fn dumps_raw(value: &OV) -> String {
    let mut out = String::with_capacity(256);
    write_value(&mut out, value, false, true);
    out
}

/// `json.dumps(value, sort_keys=True)` for objects built here (used only for request keys).
pub fn dumps_sorted(value: &OV) -> String {
    fn sorted(v: &OV) -> OV {
        match v {
            OV::Obj(o) => {
                let mut items: Vec<(String, OV)> = o.0.iter().map(|(k, v)| (k.clone(), sorted(v))).collect();
                items.sort_by(|a, b| a.0.cmp(&b.0));
                OV::Obj(Obj(items))
            }
            OV::Arr(a) => OV::Arr(a.iter().map(sorted).collect()),
            other => other.clone(),
        }
    }
    dumps(&sorted(value), true)
}

fn write_value(out: &mut String, value: &OV, ensure_ascii: bool, raw: bool) {
    match value {
        OV::Null => out.push_str("null"),
        OV::Bool(true) => out.push_str("true"),
        OV::Bool(false) => out.push_str("false"),
        OV::Int(i) => {
            let _ = write!(out, "{}", i);
        }
        OV::UInt(u) => {
            let _ = write!(out, "{}", u);
        }
        OV::Big(digits) => out.push_str(digits),
        OV::Float(f) => out.push_str(&py_float_repr(*f, true)),
        OV::Str(s) => write_string_mode(out, s, ensure_ascii, raw),
        OV::Arr(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write_value(out, item, ensure_ascii, raw);
            }
            out.push(']');
        }
        OV::Obj(obj) => {
            out.push('{');
            for (i, (key, item)) in obj.0.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write_string_mode(out, key, ensure_ascii, raw);
                out.push_str(": ");
                write_value(out, item, ensure_ascii, raw);
            }
            out.push('}');
        }
    }
}

/// Python `json` string encoding (`ensure_ascii` selects `py_encode_basestring_ascii`).
pub fn write_string(out: &mut String, s: &str, ensure_ascii: bool) {
    write_string_mode(out, s, ensure_ascii, false)
}

fn write_string_mode(out: &mut String, s: &str, ensure_ascii: bool, raw: bool) {
    out.push('"');
    for c in s.chars() {
        let code = c as u32;
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            _ if code < 0x20 => {
                let _ = write!(out, "\\u{:04x}", code);
            }
            _ if code >= SURROGATE_BASE && raw => out.push(c),
            _ if code >= SURROGATE_BASE => {
                // A lone surrogate: Python prints it as \udXXX in both modes.
                let _ = write!(out, "\\u{:04x}", 0xD800 + (code - SURROGATE_BASE));
            }
            _ if ensure_ascii && !(0x20..=0x7e).contains(&code) => {
                if code > 0xFFFF {
                    let v = code - 0x10000;
                    let _ = write!(out, "\\u{:04x}\\u{:04x}", 0xD800 + (v >> 10), 0xDC00 + (v & 0x3FF));
                } else {
                    let _ = write!(out, "\\u{:04x}", code);
                }
            }
            _ => out.push(c),
        }
    }
    out.push('"');
}

/// Python `repr(float)`; with `json` true, non-finite values use `json.dumps` spellings.
pub fn py_float_repr(f: f64, json: bool) -> String {
    if f.is_nan() {
        return if json { "NaN".into() } else { "nan".into() };
    }
    if f.is_infinite() {
        let s = if json { "Infinity" } else { "inf" };
        return if f < 0.0 { format!("-{}", s) } else { s.to_string() };
    }
    // Shortest round-trip digits and decimal exponent, then Python's 'r' layout.
    let sci = format!("{:e}", f.abs());
    let (mantissa, exponent) = sci.split_once('e').unwrap();
    let exponent: i32 = exponent.parse().unwrap();
    let digits: String = mantissa.chars().filter(|c| *c != '.').collect();
    let digits = digits.trim_end_matches('0');
    let digits = if digits.is_empty() { "0" } else { digits };
    let tie_checked = even_tie(f.abs(), digits, exponent);
    let digits = tie_checked.as_str();
    let decpt = exponent + 1;
    let n = digits.len() as i32;
    let mut out = String::new();
    if f.is_sign_negative() {
        out.push('-');
    }
    if decpt > -4 && decpt <= 16 {
        if decpt <= 0 {
            out.push_str("0.");
            out.push_str(&"0".repeat((-decpt) as usize));
            out.push_str(digits);
        } else if decpt >= n {
            out.push_str(digits);
            out.push_str(&"0".repeat((decpt - n) as usize));
            out.push_str(".0");
        } else {
            out.push_str(&digits[..decpt as usize]);
            out.push('.');
            out.push_str(&digits[decpt as usize..]);
        }
    } else {
        out.push_str(&digits[..1]);
        if n > 1 {
            out.push('.');
            out.push_str(&digits[1..]);
        }
        let e = decpt - 1;
        let _ = write!(out, "e{}{:02}", if e < 0 { '-' } else { '+' }, e.abs());
    }
    out
}

/// Python's repr (David Gay's shortest mode) breaks an exact tie between two shortest digit
/// strings toward the even last digit; Rust's formatter may pick the other one.
fn even_tie(a: f64, digits: &str, exponent: i32) -> String {
    let n = digits.len();
    let split = |s: String| -> (String, i32) {
        let (m, e) = s.split_once('e').unwrap();
        (m.chars().filter(|c| *c != '.').collect(), e.parse().unwrap())
    };
    // A tie needs the exact value to be an (n+1)-digit decimal ending in 5.
    let (rounded, e1) = split(format!("{:.*e}", n, a));
    if !rounded.ends_with('5') || e1 != exponent {
        return digits.to_string();
    }
    let (exact, e2) = split(format!("{:.*e}", 1100, a));
    if e2 != exponent || !(exact[n..].starts_with('5') && exact[n + 1..].bytes().all(|b| b == b'0')) {
        return digits.to_string();
    }
    let low = exact[..n].to_string();
    let mut high: Vec<u8> = low.clone().into_bytes();
    let mut k = high.len();
    loop {
        if k == 0 {
            return digits.to_string(); // carries into a new digit: not a same-length tie
        }
        k -= 1;
        if high[k] == b'9' {
            high[k] = b'0';
        } else {
            high[k] += 1;
            break;
        }
    }
    let high = String::from_utf8(high).unwrap();
    let even = if low.bytes().last().is_some_and(|d| d % 2 == 0) { low } else { high };
    let candidate = format!("{}.{}e{}", &even[..1], &even[1..], exponent);
    if candidate.parse::<f64>().ok() == Some(a) {
        even.trim_end_matches('0').to_string()
    } else {
        digits.to_string()
    }
}

// ---------------------------------------------------------------------------------- parsing ---

/// `json.loads(bytes)`: encoding detection as `json.detect_encoding`, decoded with `surrogatepass`.
pub fn loads_bytes(raw: &[u8]) -> Result<OV, String> {
    let text = decode_json_bytes(raw)?;
    parse_document(&text)
}

pub fn decode_json_bytes(b: &[u8]) -> Result<String, String> {
    let utf32 = |be: bool, skip: usize| decode_utf32(&b[skip..], be);
    let utf16 = |be: bool, skip: usize| decode_utf16(&b[skip..], be);
    if b.starts_with(&[0, 0, 0xFE, 0xFF]) {
        return utf32(true, 4);
    }
    if b.starts_with(&[0xFF, 0xFE, 0, 0]) {
        return utf32(false, 4);
    }
    if b.starts_with(&[0xFE, 0xFF]) {
        return utf16(true, 2);
    }
    if b.starts_with(&[0xFF, 0xFE]) {
        return utf16(false, 2);
    }
    if b.starts_with(&[0xEF, 0xBB, 0xBF]) {
        return decode_utf8_surrogatepass(&b[3..]);
    }
    if b.len() >= 4 {
        if b[0] == 0 {
            return if b[1] != 0 { utf16(true, 0) } else { utf32(true, 0) };
        }
        if b[1] == 0 {
            return if b[2] != 0 || b[3] != 0 { utf16(false, 0) } else { utf32(false, 0) };
        }
    } else if b.len() == 2 {
        if b[0] == 0 {
            return utf16(true, 0);
        }
        if b[1] == 0 {
            return utf16(false, 0);
        }
    }
    decode_utf8_surrogatepass(b)
}

fn push_code(out: &mut String, code: u32) {
    if (0xD800..=0xDFFF).contains(&code) {
        out.push(char::from_u32(SURROGATE_BASE + (code - 0xD800)).unwrap());
    } else {
        out.push(char::from_u32(code).unwrap());
    }
}

/// UTF-8 with Python's `surrogatepass`: encoded surrogates (ED A0..BF xx) decode to lone surrogates.
pub fn decode_utf8_surrogatepass(b: &[u8]) -> Result<String, String> {
    match std::str::from_utf8(b) {
        Ok(s) => return Ok(s.to_string()),
        Err(_) => {}
    }
    let mut out = String::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match std::str::from_utf8(&b[i..]) {
            Ok(s) => {
                out.push_str(s);
                break;
            }
            Err(e) => {
                let good = e.valid_up_to();
                out.push_str(std::str::from_utf8(&b[i..i + good]).unwrap());
                i += good;
                if i + 2 < b.len() && b[i] == 0xED && (0xA0..=0xBF).contains(&b[i + 1]) && (0x80..=0xBF).contains(&b[i + 2]) {
                    let code = 0xD000 | ((b[i + 1] as u32 & 0x3F) << 6) | (b[i + 2] as u32 & 0x3F);
                    push_code(&mut out, code);
                    i += 3;
                } else {
                    return Err(format!("'utf-8' codec can't decode byte 0x{:02x} in position {}", b[i], i));
                }
            }
        }
    }
    Ok(out)
}

fn decode_utf16(b: &[u8], be: bool) -> Result<String, String> {
    if b.len() % 2 != 0 {
        return Err("truncated data".into());
    }
    let units: Vec<u16> = b.chunks(2).map(|c| if be { u16::from_be_bytes([c[0], c[1]]) } else { u16::from_le_bytes([c[0], c[1]]) }).collect();
    let mut out = String::new();
    for r in char::decode_utf16(units.iter().copied()) {
        match r {
            Ok(c) => out.push(c),
            Err(e) => push_code(&mut out, e.unpaired_surrogate() as u32),
        }
    }
    Ok(out)
}

fn decode_utf32(b: &[u8], be: bool) -> Result<String, String> {
    if b.len() % 4 != 0 {
        return Err("truncated data".into());
    }
    let mut out = String::new();
    for c in b.chunks(4) {
        let code = if be { u32::from_be_bytes([c[0], c[1], c[2], c[3]]) } else { u32::from_le_bytes([c[0], c[1], c[2], c[3]]) };
        if code > 0x10FFFF {
            return Err("code point not in range".into());
        }
        push_code(&mut out, code);
    }
    Ok(out)
}

/// `json.loads(str)`. Errors are `str(exc)` as Python's C scanner words them.
pub fn loads(text: &str) -> Result<OV, String> {
    if text.starts_with('\u{feff}') {
        return Err(decode_error(text, "Unexpected UTF-8 BOM (decode using utf-8-sig)", 0));
    }
    parse_document(text)
}

fn parse_document(text: &str) -> Result<OV, String> {
    let mut p = Parser { s: text.as_bytes(), i: 0, text };
    p.ws();
    let value = p.value().map_err(|e| e.render(text))?;
    p.ws();
    if p.i != p.s.len() {
        return Err(decode_error(text, "Extra data", p.i));
    }
    Ok(value)
}

/// `JSONDecodeError.__str__`: positions count code points.
pub fn decode_error(text: &str, msg: &str, byte_pos: usize) -> String {
    let before = &text[..byte_pos];
    let pos = before.chars().count();
    let lineno = before.bytes().filter(|b| *b == b'\n').count() + 1;
    let colno = match before.rfind('\n') {
        Some(nl) => pos - before[..nl].chars().count(),
        None => pos + 1,
    };
    format!("{}: line {} column {} (char {})", msg, lineno, colno, pos)
}

enum PErr {
    At(&'static str, usize),
    IntLimit(usize),
}

impl PErr {
    fn render(&self, text: &str) -> String {
        match self {
            PErr::At(msg, pos) => decode_error(text, msg, *pos),
            PErr::IntLimit(digits) => format!(
                "Exceeds the limit (4300 digits) for integer string conversion: value has {} digits; \
                 use sys.set_int_max_str_digits() to increase the limit",
                digits
            ),
        }
    }
}

struct Parser<'a> {
    s: &'a [u8],
    i: usize,
    text: &'a str,
}

/// An object under construction; large objects get a key index so duplicate handling stays O(1).
struct Building {
    obj: Obj,
    key: String,
    index: Option<std::collections::HashMap<String, usize>>,
}

impl Building {
    /// Python `dict[key] = value`: a repeated key keeps its first position and takes the last value.
    fn insert(&mut self, value: OV) {
        let key = std::mem::take(&mut self.key);
        if let Some(index) = &mut self.index {
            if let Some(&at) = index.get(&key) {
                self.obj.0[at].1 = value;
            } else {
                index.insert(key.clone(), self.obj.0.len());
                self.obj.0.push((key, value));
            }
            return;
        }
        if let Some(slot) = self.obj.get_mut(&key) {
            *slot = value;
            return;
        }
        self.obj.0.push((key, value));
        if self.obj.0.len() >= 32 {
            self.index = Some(self.obj.0.iter().enumerate().map(|(i, (k, _))| (k.clone(), i)).collect());
        }
    }
}

enum Frame {
    Arr(Vec<OV>),
    Obj(Building),
}

impl<'a> Parser<'a> {
    fn ws(&mut self) {
        while self.i < self.s.len() && matches!(self.s[self.i], b' ' | b'\t' | b'\n' | b'\r') {
            self.i += 1;
        }
    }

    /// Iterative, so nesting depth is bounded only by memory.
    fn value(&mut self) -> Result<OV, PErr> {
        let mut stack: Vec<Frame> = Vec::new();
        'outer: loop {
            let mut current = match self.s.get(self.i) {
                Some(b'{') => {
                    self.i += 1;
                    self.ws();
                    if self.s.get(self.i) == Some(&b'}') {
                        self.i += 1;
                        OV::Obj(Obj::new())
                    } else {
                        let key = self.key()?;
                        stack.push(Frame::Obj(Building { obj: Obj::new(), key, index: None }));
                        continue 'outer;
                    }
                }
                Some(b'[') => {
                    self.i += 1;
                    self.ws();
                    if self.s.get(self.i) == Some(&b']') {
                        self.i += 1;
                        OV::Arr(Vec::new())
                    } else {
                        stack.push(Frame::Arr(Vec::new()));
                        continue 'outer;
                    }
                }
                _ => self.scalar()?,
            };
            // Attach to the enclosing containers, closing as many as end here.
            loop {
                match stack.last_mut() {
                    None => return Ok(current),
                    Some(Frame::Arr(items)) => {
                        items.push(current);
                        self.ws();
                        match self.s.get(self.i) {
                            Some(b']') => {
                                self.i += 1;
                                let Some(Frame::Arr(items)) = stack.pop() else { unreachable!() };
                                current = OV::Arr(items);
                            }
                            Some(b',') => {
                                let comma = self.i;
                                self.i += 1;
                                self.ws();
                                if self.s.get(self.i) == Some(&b']') {
                                    return Err(PErr::At("Illegal trailing comma before end of array", comma));
                                }
                                continue 'outer;
                            }
                            _ => return Err(PErr::At("Expecting ',' delimiter", self.i)),
                        }
                    }
                    Some(Frame::Obj(building)) => {
                        building.insert(current);
                        self.ws();
                        match self.s.get(self.i) {
                            Some(b'}') => {
                                self.i += 1;
                                let Some(Frame::Obj(building)) = stack.pop() else { unreachable!() };
                                current = OV::Obj(building.obj);
                            }
                            Some(b',') => {
                                let comma = self.i;
                                self.i += 1;
                                self.ws();
                                if self.s.get(self.i) == Some(&b'}') {
                                    return Err(PErr::At("Illegal trailing comma before end of object", comma));
                                }
                                let next = self.key()?;
                                if let Some(Frame::Obj(building)) = stack.last_mut() {
                                    building.key = next;
                                }
                                continue 'outer;
                            }
                            _ => return Err(PErr::At("Expecting ',' delimiter", self.i)),
                        }
                    }
                }
            }
        }
    }

    /// `"key"`, optional whitespace, `:`, optional whitespace.
    fn key(&mut self) -> Result<String, PErr> {
        if self.s.get(self.i) != Some(&b'"') {
            return Err(PErr::At("Expecting property name enclosed in double quotes", self.i));
        }
        let key = self.string()?;
        self.ws();
        if self.s.get(self.i) != Some(&b':') {
            return Err(PErr::At("Expecting ':' delimiter", self.i));
        }
        self.i += 1;
        self.ws();
        Ok(key)
    }

    fn scalar(&mut self) -> Result<OV, PErr> {
        let rest = &self.s[self.i..];
        match rest.first() {
            Some(b'"') => Ok(OV::Str(self.string()?)),
            Some(b'n') if rest.starts_with(b"null") => {
                self.i += 4;
                Ok(OV::Null)
            }
            Some(b't') if rest.starts_with(b"true") => {
                self.i += 4;
                Ok(OV::Bool(true))
            }
            Some(b'f') if rest.starts_with(b"false") => {
                self.i += 5;
                Ok(OV::Bool(false))
            }
            Some(b'N') if rest.starts_with(b"NaN") => {
                self.i += 3;
                Ok(OV::Float(f64::NAN))
            }
            Some(b'I') if rest.starts_with(b"Infinity") => {
                self.i += 8;
                Ok(OV::Float(f64::INFINITY))
            }
            Some(b'-') if rest.starts_with(b"-Infinity") => {
                self.i += 9;
                Ok(OV::Float(f64::NEG_INFINITY))
            }
            _ => self.number(),
        }
    }

    fn number(&mut self) -> Result<OV, PErr> {
        let start = self.i;
        let s = self.s;
        let mut i = self.i;
        if s.get(i) == Some(&b'-') {
            i += 1;
        }
        match s.get(i) {
            Some(b'0') => i += 1,
            Some(b'1'..=b'9') => {
                while matches!(s.get(i), Some(b'0'..=b'9')) {
                    i += 1;
                }
            }
            _ => return Err(PErr::At("Expecting value", start)),
        }
        let int_end = i;
        let mut is_float = false;
        if s.get(i) == Some(&b'.') && matches!(s.get(i + 1), Some(b'0'..=b'9')) {
            i += 2;
            while matches!(s.get(i), Some(b'0'..=b'9')) {
                i += 1;
            }
            is_float = true;
        }
        if matches!(s.get(i), Some(b'e') | Some(b'E')) {
            let mut j = i + 1;
            if matches!(s.get(j), Some(b'+') | Some(b'-')) {
                j += 1;
            }
            if matches!(s.get(j), Some(b'0'..=b'9')) {
                while matches!(s.get(j), Some(b'0'..=b'9')) {
                    j += 1;
                }
                i = j;
                is_float = true;
            }
        }
        self.i = i;
        let text = &self.text[start..i];
        if is_float {
            return Ok(OV::Float(text.parse::<f64>().unwrap_or(if text.starts_with('-') { f64::NEG_INFINITY } else { f64::INFINITY })));
        }
        let digits = &self.text[start..int_end];
        let count = digits.trim_start_matches('-').len();
        if count > 4300 {
            return Err(PErr::IntLimit(count));
        }
        if let Ok(v) = digits.parse::<i64>() {
            return Ok(OV::Int(v));
        }
        if let Ok(v) = digits.parse::<u64>() {
            return Ok(OV::UInt(v));
        }
        Ok(OV::Big(digits.to_string()))
    }

    fn string(&mut self) -> Result<String, PErr> {
        let begin = self.i; // the opening quote
        self.i += 1;
        let mut out = String::new();
        loop {
            let start = self.i;
            while self.i < self.s.len() {
                let b = self.s[self.i];
                if b == b'"' || b == b'\\' {
                    break;
                }
                if b < 0x20 {
                    return Err(PErr::At("Invalid control character at", self.i));
                }
                self.i += 1;
            }
            out.push_str(&self.text[start..self.i]);
            match self.s.get(self.i) {
                None => return Err(PErr::At("Unterminated string starting at", begin)),
                Some(b'"') => {
                    self.i += 1;
                    return Ok(out);
                }
                _ => {}
            }
            let backslash = self.i;
            self.i += 1;
            let esc = match self.s.get(self.i) {
                Some(e) => *e,
                None => return Err(PErr::At("Unterminated string starting at", begin)),
            };
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
                    let u = self.i;
                    self.i += 1;
                    let mut code = self.hex4(u)?;
                    if (0xD800..=0xDBFF).contains(&code) && self.s.get(self.i) == Some(&b'\\') && self.s.get(self.i + 1) == Some(&b'u') {
                        let save = self.i;
                        self.i += 2;
                        let low = self.hex4(save + 1)?;
                        if (0xDC00..=0xDFFF).contains(&low) {
                            code = 0x10000 + ((code - 0xD800) << 10) + (low - 0xDC00);
                        } else {
                            self.i = save;
                        }
                    }
                    push_code(&mut out, code);
                    continue;
                }
                _ => return Err(PErr::At("Invalid \\escape", backslash)),
            }
            self.i += 1;
        }
    }

    /// Four hex digits at the cursor. Python also requires a character after them; errors
    /// point at the `u` (position `u`).
    fn hex4(&mut self, u: usize) -> Result<u32, PErr> {
        let digits = match self.s.get(self.i..self.i + 5) {
            Some(h) => &h[..4],
            None => return Err(PErr::At("Invalid \\uXXXX escape", u)),
        };
        if !digits.iter().all(|c| c.is_ascii_hexdigit()) {
            return Err(PErr::At("Invalid \\uXXXX escape", u));
        }
        self.i += 4;
        Ok(u32::from_str_radix(std::str::from_utf8(digits).unwrap(), 16).unwrap())
    }
}

// ------------------------------------------------------------------------- serde_json bridge ---

/// A decoded record value from the fast path, as a Python object would print.
pub fn from_serde(v: &serde_json::Value) -> OV {
    match v {
        serde_json::Value::Null => OV::Null,
        serde_json::Value::Bool(b) => OV::Bool(*b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                OV::Int(i)
            } else if let Some(u) = n.as_u64() {
                OV::UInt(u)
            } else {
                OV::Float(n.as_f64().unwrap_or(0.0))
            }
        }
        serde_json::Value::String(s) => OV::Str(s.clone()),
        serde_json::Value::Array(a) => OV::Arr(a.iter().map(from_serde).collect()),
        serde_json::Value::Object(m) => OV::Obj(Obj(m.iter().map(|(k, v)| (k.clone(), from_serde(v))).collect())),
    }
}
