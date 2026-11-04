//! 2.3.0 `blocks_from_event` and `input_strings`, generic over the two JSON representations:
//! `serde_json::Value` (fast scan; key order not preserved) and `OV` (exact, ordered).

use crate::ojson::{self, Obj, OV};

pub trait Json: Sized {
    fn as_str(&self) -> Option<&str>;
    /// Object member; None when absent or not an object.
    fn get(&self, key: &str) -> Option<&Self>;
    fn is_obj(&self) -> bool;
    fn as_slice(&self) -> Option<&[Self]>;
    /// Python truthiness.
    fn truthy(&self) -> bool;
    fn for_each_entry(&self, f: &mut dyn FnMut(&str, &Self));
}

impl Json for serde_json::Value {
    fn as_str(&self) -> Option<&str> {
        serde_json::Value::as_str(self)
    }
    fn get(&self, key: &str) -> Option<&Self> {
        self.as_object().and_then(|m| m.get(key))
    }
    fn is_obj(&self) -> bool {
        self.is_object()
    }
    fn as_slice(&self) -> Option<&[Self]> {
        self.as_array().map(Vec::as_slice)
    }
    fn truthy(&self) -> bool {
        use serde_json::Value::*;
        match self {
            Null => false,
            Bool(b) => *b,
            Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
            String(s) => !s.is_empty(),
            Array(a) => !a.is_empty(),
            Object(o) => !o.is_empty(),
        }
    }
    fn for_each_entry(&self, f: &mut dyn FnMut(&str, &Self)) {
        if let Some(m) = self.as_object() {
            for (k, v) in m {
                f(k, v);
            }
        }
    }
}

impl Json for OV {
    fn as_str(&self) -> Option<&str> {
        OV::as_str(self)
    }
    fn get(&self, key: &str) -> Option<&Self> {
        self.as_obj().and_then(|o| o.get(key))
    }
    fn is_obj(&self) -> bool {
        matches!(self, OV::Obj(_))
    }
    fn as_slice(&self) -> Option<&[Self]> {
        self.as_arr().map(Vec::as_slice)
    }
    fn truthy(&self) -> bool {
        OV::truthy(self)
    }
    fn for_each_entry(&self, f: &mut dyn FnMut(&str, &Self)) {
        if let OV::Obj(Obj(items)) = self {
            for (k, v) in items {
                f(k, v);
            }
        }
    }
}

/// A field's role: a fixed name, or the record's own `role` value (any JSON type).
pub enum Role<'a, J> {
    Name(&'a str),
    Value(&'a J),
}

impl<J> Clone for Role<'_, J> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<J> Copy for Role<'_, J> {}

pub enum Part<'p> {
    Idx(usize),
    Key(&'p str),
}

/// A field's block identity: content index, `i.content[j]`, or a tool-input field path.
pub enum Block<'p> {
    Index(usize),
    Sub(usize, usize),
    Field(&'p [Part<'p>]),
}

impl Block<'_> {
    /// The value Python stores in `block`.
    pub fn to_ov(&self) -> OV {
        match self {
            Block::Index(i) => OV::Int(*i as i64),
            Block::Sub(i, j) => OV::Str(format!("{}.content[{}]", i, j)),
            Block::Field(parts) => {
                // json.dumps(field, ensure_ascii=False, separators=(',', ':'))
                let mut out = String::from("[");
                for (n, p) in parts.iter().enumerate() {
                    if n > 0 {
                        out.push(',');
                    }
                    match p {
                        Part::Idx(i) => out.push_str(&i.to_string()),
                        Part::Key(k) => ojson::write_string(&mut out, k, false),
                    }
                }
                out.push(']');
                OV::Str(out)
            }
        }
    }
}

pub type Visit<'v, 'a, J> = dyn FnMut(Role<'a, J>, &str, Block<'_>) + 'v;

fn input_strings<'a, K: Json, J: Json>(value: &K, path: &mut Vec<Part<'_>>, visit: &mut Visit<'_, 'a, J>) {
    if let Some(s) = value.as_str() {
        visit(Role::Name("tool_input"), s, Block::Field(path));
    } else if value.is_obj() {
        // Keys live as long as `value`; the path is rebuilt per entry so borrows stay local.
        value.for_each_entry(&mut |key, child| {
            let mut local: Vec<Part<'_>> = path.iter().map(|p| match p {
                Part::Idx(i) => Part::Idx(*i),
                Part::Key(k) => Part::Key(k),
            }).collect();
            local.push(Part::Key(key));
            input_strings(child, &mut local, visit);
        });
    } else if let Some(items) = value.as_slice() {
        for (i, child) in items.iter().enumerate() {
            path.push(Part::Idx(i));
            input_strings(child, path, visit);
            path.pop();
        }
    }
}

/// Tool-call arguments: a JSON string holding an object or array is searched as its fields.
fn tool_arguments<'a, J: Json>(value: Option<&J>, ordered: bool, visit: &mut Visit<'_, 'a, J>) {
    let mut path = vec![Part::Key("input")];
    let Some(value) = value else {
        visit(Role::Name("tool_input"), "", Block::Field(&path));
        return;
    };
    if let Some(s) = value.as_str() {
        if !ordered {
            if let Ok(decoded) = serde_json::from_str::<serde_json::Value>(s) {
                if decoded.is_object() || decoded.is_array() {
                    input_strings(&decoded, &mut path, visit);
                } else {
                    input_strings(value, &mut path, visit);
                }
                return;
            }
        }
        if let Ok(decoded) = ojson::loads(s) {
            if matches!(decoded, OV::Obj(_) | OV::Arr(_)) {
                input_strings(&decoded, &mut path, visit);
                return;
            }
        }
    }
    input_strings(value, &mut path, visit);
}

enum Content<'a, J> {
    Val(&'a J),
    EmptyStr,
    Nothing,
}

/// The fields of one record: an Antigravity step, or a Claude/Codex event.
pub fn row_blocks<'a, J: Json>(antigravity: bool, e: &'a J, ordered: bool, visit: &mut Visit<'_, 'a, J>) -> Result<(), String> {
    if antigravity {
        antigravity_blocks(e, visit);
        Ok(())
    } else {
        blocks(e, ordered, visit)
    }
}

const ANTIGRAVITY_SYSTEM_STEPS: [&str; 4] = ["SYSTEM_MESSAGE", "EPHEMERAL_MESSAGE", "CONVERSATION_HISTORY", "ERROR_MESSAGE"];
const ANTIGRAVITY_TOOL_STEPS: [&str; 11] = ["GENERIC", "RUN_COMMAND", "VIEW_FILE", "CODE_ACTION", "LIST_DIRECTORY", "GREP_SEARCH",
                                            "SEARCH_WEB", "READ_URL_CONTENT", "GENERATE_IMAGE", "INVOKE_SUBAGENT", "ASK_QUESTION"];

/// `user_request_spans(text)`: (true, inside) for each `<USER_REQUEST>` request, (false, span) for
/// the app's text around it; an unclosed tag runs to the end; tags and empty spans are dropped.
pub fn user_request_spans(text: &str) -> Vec<(bool, &str)> {
    const OPEN: &str = "<USER_REQUEST>";
    const CLOSE: &str = "</USER_REQUEST>";
    let mut spans = Vec::new();
    let mut rest = text;
    while let Some(at) = rest.find(OPEN) {
        spans.push((false, &rest[..at]));
        let inside = &rest[at + OPEN.len()..];
        let (request, after) = match inside.find(CLOSE) {
            Some(end) => (&inside[..end], &inside[end + CLOSE.len()..]),
            None => (inside, ""),
        };
        spans.push((true, request));
        rest = after;
    }
    spans.push((false, rest));
    spans.retain(|(_, span)| !span.is_empty());
    spans
}

/// `antigravity_argument(value, field)`: an argument stored as a JSON literal is searched decoded;
/// one that does not parse (a truncated one) as stored.
fn antigravity_argument<'a, K: Json, J: Json>(value: &K, path: &mut Vec<Part<'_>>, visit: &mut Visit<'_, 'a, J>) {
    match value.as_str().map(ojson::loads) {
        Some(Ok(decoded)) => input_strings(&decoded, path, visit),
        Some(Err(_)) => visit(Role::Name("tool_input"), value.as_str().unwrap_or(""), Block::Field(path)),
        None => input_strings(value, path, visit),
    }
}

/// Port of `blocks_from_antigravity`. `thinking` is never read. Only the inside of `<USER_REQUEST>`
/// in a USER_EXPLICIT USER_INPUT has the user role; text the app wrote has role `system`.
fn antigravity_blocks<'a, J: Json>(e: &'a J, visit: &mut Visit<'_, 'a, J>) {
    if !e.is_obj() {
        return;
    }
    let kind = e.get("type").and_then(Json::as_str);
    let content = e.get("content").and_then(Json::as_str);
    match kind {
        Some("USER_INPUT") => {
            if let Some(text) = content {
                let explicit = e.get("source").and_then(Json::as_str) == Some("USER_EXPLICIT");
                for (index, (inside, span)) in user_request_spans(text).into_iter().enumerate() {
                    visit(Role::Name(if inside && explicit { "user" } else { "system" }), span, Block::Index(index));
                }
            }
        }
        Some("PLANNER_RESPONSE") => {
            if let Some(text) = content {
                visit(Role::Name("assistant"), text, Block::Index(0));
            }
            for (index, call) in e.get("tool_calls").and_then(Json::as_slice).unwrap_or(&[]).iter().enumerate() {
                if let Some(args) = call.get("args").filter(|a| a.is_obj()) {
                    args.for_each_entry(&mut |key, value| {
                        let mut path = vec![Part::Key("tool_calls"), Part::Idx(index), Part::Key("args"), Part::Key(key)];
                        antigravity_argument(value, &mut path, visit);
                    });
                }
            }
        }
        _ => {
            let role = match kind {
                Some("CHECKPOINT") => "summary",
                Some(k) if ANTIGRAVITY_SYSTEM_STEPS.contains(&k) => "system",
                Some(k) if ANTIGRAVITY_TOOL_STEPS.contains(&k) => "tool",
                _ => "unknown",
            };
            if let Some(text) = content {
                visit(Role::Name(role), text, Block::Index(0));
            } else if let Some(error) = e.get("error").and_then(Json::as_str) {
                visit(Role::Name(role), error, Block::Field(&[Part::Key("error")]));
            }
        }
    }
}

/// Port of `blocks_from_event`: visits each documented text-bearing field. Err carries the
/// ValueError text for record shapes 2.3.0 rejects.
pub fn blocks<'a, J: Json>(e: &'a J, ordered: bool, visit: &mut Visit<'_, 'a, J>) -> Result<(), String> {
    if !e.is_obj() {
        return Ok(());
    }
    let kind = e.get("type").and_then(Json::as_str);
    let (role, content): (Role<'a, J>, Content<'a, J>) = match kind {
        Some(k @ ("user" | "assistant")) => match e.get("message").filter(|m| m.truthy()) {
            None => return Ok(()),
            Some(m) if m.is_obj() => {
                let role = match m.get("role").filter(|r| r.truthy()) {
                    Some(r) => Role::Value(r),
                    None => Role::Name(k),
                };
                (role, m.get("content").map_or(Content::Nothing, Content::Val))
            }
            Some(m) => (Role::Name(k), Content::Val(m)),
        },
        Some("response_item") => {
            let Some(m) = e.get("payload").filter(|m| m.truthy()) else { return Ok(()) };
            if !m.is_obj() {
                return Err("response_item payload must be an object".into());
            }
            match m.get("type").and_then(Json::as_str) {
                Some("message") => {
                    let role = m.get("role").map_or(Role::Name("unknown"), Role::Value);
                    (role, m.get("content").map_or(Content::Nothing, Content::Val))
                }
                Some("function_call_output" | "custom_tool_call_output") => {
                    (Role::Name("tool"), m.get("output").map_or(Content::EmptyStr, Content::Val))
                }
                Some("function_call" | "custom_tool_call") => {
                    tool_arguments(m.get("arguments").or_else(|| m.get("input")), ordered, visit);
                    return Ok(());
                }
                _ => return Ok(()),
            }
        }
        Some("event_msg") => {
            let Some(m) = e.get("payload").filter(|m| m.truthy()) else { return Ok(()) };
            if !m.is_obj() {
                return Err("event_msg payload must be an object".into());
            }
            let role = match m.get("type").and_then(Json::as_str) {
                Some("user_message") => "user",
                Some("agent_message") => "assistant",
                _ => return Ok(()),
            };
            (Role::Name(role), m.get("message").map_or(Content::EmptyStr, Content::Val))
        }
        Some("compacted" | "compaction" | "summary") => {
            let m = match e.get("payload").filter(|m| m.truthy()) {
                None => e,
                Some(p) if p.is_obj() => p,
                Some(_) => return Err("summary payload must be an object".into()),
            };
            let content = match m.get("summary").or_else(|| m.get("message")) {
                Some(v) => Content::Val(v),
                None => Content::EmptyStr,
            };
            (Role::Name("summary"), content)
        }
        Some("custom-title") => match e.get("customTitle") {
            Some(t) if t.as_str().is_some() => (Role::Name("metadata"), Content::Val(t)),
            _ => return Ok(()),
        },
        _ => return Ok(()),
    };
    let content = match content {
        Content::Nothing => return Ok(()),
        Content::EmptyStr => {
            visit(role, "", Block::Index(0));
            return Ok(());
        }
        Content::Val(v) => v,
    };
    if let Some(s) = content.as_str() {
        visit(role, s, Block::Index(0));
        return Ok(());
    }
    let Some(items) = content.as_slice() else { return Ok(()) };
    for (index, block) in items.iter().enumerate() {
        if !block.is_obj() {
            continue;
        }
        let btype = block.get("type").and_then(Json::as_str);
        match btype {
            Some("text" | "input_text" | "output_text") if block.get("text").and_then(Json::as_str).is_some() => {
                visit(role, block.get("text").and_then(Json::as_str).unwrap(), Block::Index(index));
            }
            Some("tool_use") => {
                if let Some(input) = block.get("input") {
                    let mut path = vec![Part::Idx(index), Part::Key("input")];
                    input_strings(input, &mut path, visit);
                }
            }
            Some("tool_result") => match block.get("content") {
                None => visit(Role::Name("tool"), "", Block::Index(index)),
                Some(v) => {
                    if let Some(s) = v.as_str() {
                        visit(Role::Name("tool"), s, Block::Index(index));
                    } else if let Some(subs) = v.as_slice() {
                        for (j, sub) in subs.iter().enumerate() {
                            if let Some(t) = sub.get("text").and_then(Json::as_str) {
                                visit(Role::Name("tool"), t, Block::Sub(index, j));
                            }
                        }
                    }
                }
            },
            _ => {}
        }
    }
    Ok(())
}
