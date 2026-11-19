//! Official data exports (ChatGPT and claude.ai), extracted from their ZIP: 2.9 `epoch_iso`,
//! `export_spans`, `export_rows`, `export_messages`, `chatgpt_fields`, `claude_export_fields`,
//! `export_message_fields`, `export_records` and `export_list_row`. Each conversations.json (or
//! numbered conversations-NNN.json) is a JSON array of conversations; each conversation is one row,
//! read from its own byte span of that file.

use crate::hash::sha256_hex;
use crate::inventory::{bump, count_file, int_ov, note3, problem, set_status, Source};
use crate::listing::public_row;
use crate::ojson::{self, Obj, OV};
use crate::pathx;
use crate::pyerr::{Fail, PyOsError};
use crate::pyio;
use crate::pyobj::py_eq;
use crate::pytext::{casefold, classify, excerpt};
use crate::scan::Emit;
use crate::timeparse::time_key;
use std::collections::{HashMap, HashSet};
use std::io::{Seek, SeekFrom};

pub const CHATGPT_EXPORT_EXCLUDED: &str = "ChatGPT export: only conversations.json and conversations-NNN.json are searched; \
     thoughts and reasoning_recap content, images, audio, other files and the export's \
     other documents (chat.html, user, settings, feedback, manifest) are not";
pub const CLAUDE_EXPORT_EXCLUDED: &str = "Claude export: only conversations.json and conversations-NNN.json are searched; \
     thinking, token_budget and flag blocks, files without extracted text, and users.json, \
     projects.json and memories.json are not";

/// One searchable field: (role, text, block).
type Fields = Vec<(&'static str, String, OV)>;

/// Bounds on what one export file may hold; see the Python core's `EXPORT_FILE_LIMIT`.
const EXPORT_FILE_LIMIT: u64 = 1 << 30;
const EXPORT_DEPTH_LIMIT: usize = 256;
const EXPORT_UNSUPPORTED: &str = "not a UTF-8 JSON array of conversations nested at most 256 levels deep";
const CHATGPT_MEDIA: [&str; 3] = ["image_asset_pointer", "audio_asset_pointer", "real_time_user_audio_video_asset_pointer"];
const CLAUDE_RESULT_ITEMS: [&str; 4] = ["text", "knowledge", "local_resource", "rag_reference"];

pub fn is_export_kind(kind: &str) -> bool {
    matches!(kind, "chatgpt_export" | "claude_export")
}

pub fn is_export(row: &Obj) -> bool {
    row.str_of("agent").is_some_and(is_export_kind)
}

/// `EXPORT_FILE.fullmatch(name)`: `conversations(-[0-9]+)?\.json`.
fn is_export_file(name: &str) -> bool {
    if name == "conversations.json" {
        return true;
    }
    match name.strip_prefix("conversations-").and_then(|rest| rest.strip_suffix(".json")) {
        Some(digits) => !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()),
        None => false,
    }
}

/// Days since 1970-01-01 as a proleptic Gregorian (year, month, day).
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(month <= 2), month, day)
}

/// `epoch_iso(value)`: a ChatGPT export time, seconds since the Unix epoch as a JSON number, as
/// ISO 8601 UTC text, read from the number's shortest decimal form and cut, not rounded, to
/// microseconds. Other values, and whole parts of more than 11 digits (after the year 5138), give
/// None.
pub fn epoch_iso(value: Option<&OV>) -> OV {
    let text = match value {
        Some(OV::Int(i)) => i.to_string(),
        Some(OV::UInt(u)) => u.to_string(),
        Some(OV::Big(digits)) => digits.clone(),
        Some(OV::Float(f)) => ojson::py_float_repr(*f, false),
        _ => return OV::Null,
    };
    let (whole, fraction) = match text.split_once('.') {
        Some((whole, fraction)) => (whole, Some(fraction)),
        None => (text.as_str(), None),
    };
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    if !digits(whole) || whole.len() > 11 || fraction.is_some_and(|f| !digits(f)) {
        return OV::Null;
    }
    let seconds: i64 = whole.parse().unwrap_or(0);
    let (year, month, day) = civil_from_days(seconds / 86_400);
    let rest = seconds % 86_400;
    let mut micro: String = fraction.unwrap_or("").chars().take(6).collect();
    while micro.len() < 6 {
        micro.push('0');
    }
    OV::Str(format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{}Z", year, month, day, rest / 3600, rest % 3600 / 60, rest % 60, micro))
}

/// The end (exclusive) of the JSON string that opens at `i`.
fn string_end(b: &[u8], i: usize) -> Option<usize> {
    let mut j = i + 1;
    while j < b.len() {
        match b[j] {
            b'\\' => j += 2,
            b'"' => return Some(j + 1),
            _ => j += 1,
        }
    }
    None
}

/// The end (exclusive) of the JSON value that starts at `i`, by its brackets, quotes and
/// delimiters; the parser then checks the value itself.
fn value_end(b: &[u8], i: usize) -> Option<usize> {
    match *b.get(i)? {
        b'{' | b'[' => {
            let mut depth = 0usize;
            let mut j = i;
            while j < b.len() {
                match b[j] {
                    b'"' => {
                        j = string_end(b, j)?;
                        continue;
                    }
                    b'{' | b'[' => depth += 1,
                    b'}' | b']' => {
                        depth -= 1;
                        if depth == 0 {
                            return Some(j + 1);
                        }
                    }
                    _ => {}
                }
                j += 1;
            }
            None
        }
        b'"' => string_end(b, i),
        _ => {
            let mut j = i;
            while j < b.len() && !matches!(b[j], b',' | b']' | b'}' | b' ' | b'\t' | b'\n' | b'\r') {
                j += 1;
            }
            Some(j)
        }
    }
}

/// `nested_deeper(data, limit)`: whether JSON text nests arrays and objects more than `limit` deep
/// outside strings. Exact for valid JSON.
pub fn nested_deeper(b: &[u8], limit: usize) -> bool {
    let mut depth = 0usize;
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'"' => {
                i += 1;
                while i < b.len() && b[i] != b'"' {
                    i += if b[i] == b'\\' { 2 } else { 1 };
                }
            }
            b'[' | b'{' => {
                depth += 1;
                if depth > limit {
                    return true;
                }
            }
            b']' | b'}' => depth = depth.saturating_sub(1),
            _ => {}
        }
        i += 1;
    }
    false
}

/// A character in U+10F800..U+10FFFF, literal or as a JSON escape whose surrogate pair decodes into
/// it (high surrogate `\udbfe` or `\udbff`): this core's parser cannot tell either from the
/// placeholder it uses for a lone surrogate.
fn has_lookalike(text: &str) -> bool {
    text.chars().any(|c| c as u32 >= ojson::SURROGATE_BASE)
        || text.as_bytes().windows(6).any(|w| {
            w[0] == b'\\' && w[1] == b'u' && matches!(w[2], b'd' | b'D') && matches!(w[3], b'b' | b'B')
                && matches!(w[4], b'f' | b'F') && matches!(w[5], b'e' | b'E' | b'f' | b'F')
        })
}

/// `export_spans(data)`: `visit(start, length, value, lookalike)` for each element of the top-level
/// JSON array, in order, with byte offsets into `data`. Err, possibly after some elements, when
/// `data` is not one UTF-8 JSON array nested at most `EXPORT_DEPTH_LIMIT` deep.
pub fn export_spans(data: &[u8], visit: &mut dyn FnMut(usize, usize, OV, bool)) -> Result<(), ()> {
    let bom = if data.starts_with(b"\xef\xbb\xbf") { 3 } else { 0 };
    let text = std::str::from_utf8(&data[bom..]).map_err(|_| ())?;
    let b = text.as_bytes();
    if nested_deeper(b, EXPORT_DEPTH_LIMIT) {
        return Err(());
    }
    let space = |mut i: usize| {
        while i < b.len() && matches!(b[i], b' ' | b'\t' | b'\n' | b'\r') {
            i += 1;
        }
        i
    };
    let mut i = space(0);
    if b.get(i) != Some(&b'[') {
        return Err(());
    }
    i = space(i + 1);
    if b.get(i) == Some(&b']') {
        i += 1;
    } else {
        loop {
            let end = value_end(b, i).ok_or(())?;
            let value = ojson::loads(&text[i..end]).map_err(|_| ())?;
            visit(bom + i, end - i, value, has_lookalike(&text[i..end]));
            i = space(end);
            match b.get(i) {
                Some(b',') => i = space(i + 1),
                Some(b']') => {
                    i += 1;
                    break;
                }
                _ => return Err(()),
            }
        }
    }
    if space(i) != b.len() {
        return Err(());
    }
    Ok(())
}

fn str_or_null(value: Option<&OV>) -> OV {
    match value {
        Some(OV::Str(s)) => OV::Str(s.clone()),
        _ => OV::Null,
    }
}

/// `export_conversation_fields(kind, value)`: (identity, title, created, updated).
fn conversation_fields(kind: &str, c: &Obj) -> (Option<String>, OV, OV, OV) {
    let (ident, title, created, updated) = if kind == "chatgpt_export" {
        let ident = match c.get("conversation_id").filter(|v| v.truthy()) {
            Some(v) => Some(v),
            None => c.get("id"),
        };
        (ident, c.get("title"), epoch_iso(c.get("create_time")), epoch_iso(c.get("update_time")))
    } else {
        (c.get("uuid"), c.get("name"), str_or_null(c.get("created_at")), str_or_null(c.get("updated_at")))
    };
    let ident = ident.and_then(OV::as_str).filter(|s| !s.is_empty()).map(str::to_string);
    let title = match title {
        Some(OV::Str(s)) if !s.is_empty() => OV::Str(s.clone()),
        _ => OV::Null,
    };
    (ident, title, created, updated)
}

/// `export_rows(root, resolved_root, source, status, coverage, seen_paths, guard)`.
pub fn export_rows(root: &str, resolved_root: &str, source: &Source, index: usize, cov: &mut Obj,
                   seen: &mut HashSet<String>, _guard: bool) -> Result<Vec<Obj>, Fail> {
    let mut rows = Vec::new();
    {
        let cov_cell = std::cell::RefCell::new(&mut *cov);
        let mut on_error = |e: PyOsError| {
            let mut cov = cov_cell.borrow_mut();
            set_status(&mut cov, index, "partial");
            problem(&mut cov, "read_error", root, &e.text);
        };
        let mut visit = |directory: &str, dirs: &mut Vec<String>, names: Vec<String>| -> Result<(), Fail> {
            let mut cov = cov_cell.borrow_mut();
            let mut kept = Vec::new();
            for d in dirs.drain(..) {
                let p = pathx::join(directory, &d);
                if pyio::is_link_or_junction(&p) {
                    problem(&mut cov, "excluded_link", &p, "");
                } else {
                    kept.push(d);
                }
            }
            kept.sort();
            *dirs = kept;
            let mut names = names;
            names.sort();
            for name in names {
                let p = pathx::join(directory, &name);
                let resolved = pathx::join_parts(resolved_root, &pathx::relative_parts(&p, root));
                if name.to_lowercase().ends_with(".zip") {
                    note3(&mut cov, "export_archive_not_searched", &resolved, "extract the archive to search its conversations");
                    continue;
                }
                if !is_export_file(&name) {
                    continue;
                }
                if pyio::is_symlink(&p) {
                    problem(&mut cov, "excluded_link", &p, "");
                    continue;
                }
                if !seen.insert(resolved.clone()) {
                    continue;
                }
                count_file(&mut cov, index);
                let st = match pyio::stat(&p) {
                    Ok(st) => st,
                    Err(e) => {
                        problem(&mut cov, "read_error", &p, &e.text);
                        continue;
                    }
                };
                if st.size > EXPORT_FILE_LIMIT {
                    problem(&mut cov, "export_too_large", &p, "larger than 1 GiB; not read");
                    continue;
                }
                let data = match pyio::read_bytes(&p) {
                    Ok(data) => data,
                    Err(e) => {
                        problem(&mut cov, "read_error", &p, &e.text);
                        continue;
                    }
                };
                if data.len() as u64 != st.size {
                    problem(&mut cov, "changed_during_read", &p, "");
                    continue;
                }
                let mut found: Vec<Obj> = Vec::new();
                // (problem kind, detail, counted as an unsupported record)
                let mut issues: Vec<(&str, String, bool)> = Vec::new();
                let mut identities: HashSet<String> = HashSet::new();
                let mut newest = OV::Null;
                let mut number = 0usize;
                let parsed = export_spans(&data, &mut |start, length, value, lookalike| {
                    number += 1;
                    let place = format!("conversation {}", number);
                    if lookalike {
                        issues.push(("unsupported_record", format!("{}: a character in U+10F800..U+10FFFF", place), true));
                        return;
                    }
                    let OV::Obj(conversation) = value else {
                        issues.push(("unsupported_record", place, true));
                        return;
                    };
                    let (ident, title, created, updated) = conversation_fields(source.kind, &conversation);
                    let Some(ident) = ident else {
                        issues.push(("identity_unavailable", place, false));
                        return;
                    };
                    if !identities.insert(casefold(&ident)) {
                        issues.push(("identity_duplicate", place, false));
                        return;
                    }
                    if updated.truthy() && time_key(Some(&updated)) > time_key(Some(&newest)) {
                        newest = updated;
                    }
                    let guards = OV::Arr(vec![OV::Arr(vec![OV::Int(start as i64), OV::Int(length as i64),
                                                           OV::Str(sha256_hex(&data[start..start + length]))])]);
                    let basis = if created.truthy() { "conversation created" } else { "unknown" };
                    found.push(Obj::new()
                        .with("id", OV::Str(ident))
                        .with("agent", OV::str(source.kind))
                        .with("source", OV::str(source.name))
                        .with("kind", OV::str("conversation"))
                        .with("parent_id", OV::Null)
                        .with("path", OV::str(&resolved))
                        .with("bytes", OV::Int(length as i64))
                        .with("mtime_ns", int_ov(st.mtime_ns))
                        .with("timestamp", created)
                        .with("timestamp_basis", OV::str(basis))
                        .with("title", title)
                        .with("export_offset", OV::Int(start as i64))
                        .with("export_conversation", OV::Int(number as i64))
                        .with("_guards", guards));
                });
                if parsed.is_err() {
                    problem(&mut cov, "export_unsupported", &p, EXPORT_UNSUPPORTED);
                    continue;
                }
                for (kind, detail, counted) in issues {
                    if counted {
                        bump(&mut cov, "unsupported_records", 1);
                    }
                    problem(&mut cov, kind, &p, &detail);
                }
                rows.extend(found);
                let newest = match &newest {
                    OV::Str(s) => s.clone(),
                    _ => "unknown".to_string(),
                };
                note3(&mut cov, "export_snapshot", &resolved, &format!("a snapshot as exported; newest conversation update {}", newest));
            }
            Ok(())
        };
        pyio::walk(root, &mut on_error, &mut visit)?;
    }
    Ok(rows)
}

/// `export_messages(kind, conversation)`: (message, on_current_branch) in reading order. A ChatGPT
/// `mapping` is read depth-first from each root, children in stored order, then any node left
/// unreached; `current_node` names the branch shown in the app. Err on an unknown shape.
fn export_messages(kind: &str, conversation: &Obj) -> Result<Vec<(OV, OV)>, ()> {
    if kind == "claude_export" {
        let Some(OV::Arr(messages)) = conversation.get("chat_messages") else { return Err(()) };
        return Ok(messages.iter().map(|m| (m.clone(), OV::Null)).collect());
    }
    let Some(OV::Obj(mapping)) = conversation.get("mapping") else { return Err(()) };
    let nodes = &mapping.0;
    let index: HashMap<&str, usize> = nodes.iter().enumerate().map(|(i, (k, _))| (k.as_str(), i)).collect();
    let parent_of = |i: usize| -> Option<usize> {
        match &nodes[i].1 {
            OV::Obj(node) => match node.get("parent") {
                Some(OV::Str(p)) => index.get(p.as_str()).copied(),
                _ => None,
            },
            _ => None,
        }
    };
    let mut current: HashSet<usize> = HashSet::new();
    let mut key = match conversation.get("current_node") {
        Some(OV::Str(s)) => index.get(s.as_str()).copied(),
        _ => None,
    };
    while let Some(i) = key {
        if !current.insert(i) {
            break;
        }
        key = parent_of(i);
    }
    let mut starts: Vec<usize> = (0..nodes.len()).filter(|&i| parent_of(i).is_none()).collect();
    starts.extend(0..nodes.len());
    let mut seen = vec![false; nodes.len()];
    let mut order = Vec::new();
    for start in starts {
        let mut stack = vec![start];
        while let Some(i) = stack.pop() {
            if seen[i] {
                continue;
            }
            seen[i] = true;
            order.push(i);
            if let OV::Obj(node) = &nodes[i].1 {
                if let Some(OV::Arr(children)) = node.get("children") {
                    let next: Vec<usize> = children.iter()
                        .filter_map(|c| c.as_str().and_then(|c| index.get(c).copied()))
                        .filter(|&c| !seen[c])
                        .collect();
                    stack.extend(next.into_iter().rev());
                }
            }
        }
    }
    let mut messages = Vec::new();
    for i in order {
        match &nodes[i].1 {
            OV::Obj(node) => {
                if let Some(message) = node.get("message").filter(|m| !m.is_null()) {
                    messages.push((message.clone(), OV::Bool(current.contains(&i))));
                }
            }
            // Not a node at all (null included): passed on, to be counted as unsupported.
            other => messages.push((other.clone(), OV::Bool(current.contains(&i)))),
        }
    }
    Ok(messages)
}

/// `chatgpt_fields(message)`: (fields, unknown content types). Err for an unknown author or shape.
fn chatgpt_fields(message: &Obj) -> Result<(Fields, Vec<String>), ()> {
    let author = match message.get("author") {
        Some(OV::Obj(author)) => author.str_of("role"),
        _ => None,
    };
    let mut role: &'static str = match author {
        Some("user") => "user",
        Some("assistant") => "assistant",
        Some("system") => "system",
        Some("tool") => "tool",
        _ => return Err(()),
    };
    let Some(OV::Obj(content)) = message.get("content") else { return Err(()) };
    let kind = content.get("content_type").and_then(OV::as_str);
    let hidden = matches!(message.get("metadata"),
                          Some(OV::Obj(m)) if matches!(m.get("is_visually_hidden_from_conversation"), Some(OV::Bool(true))));
    if role == "user" && hidden {
        role = "system";
    }
    let to_tool = match message.get("recipient") {
        None | Some(OV::Null) => false,
        Some(OV::Str(s)) => s != "all",
        Some(_) => true,
    };
    if role == "assistant" && to_tool {
        role = "tool_input";
    }
    let mut fields = Vec::new();
    if matches!(kind, Some("text" | "multimodal_text")) {
        let Some(OV::Arr(parts)) = content.get("parts") else { return Err(()) };
        let mut unknown = Vec::new();
        for (i, part) in parts.iter().enumerate() {
            let part_kind = part.as_obj().and_then(|p| p.get("content_type")).and_then(OV::as_str);
            match part {
                OV::Str(s) => {
                    if !s.is_empty() {
                        fields.push((role, s.clone(), OV::Str(format!("[\"content\",\"parts\",{}]", i))));
                    }
                }
                OV::Obj(p) if part_kind == Some("audio_transcription") => {
                    if let Some(t) = p.str_of("text").filter(|t| !t.is_empty()) {
                        fields.push((role, t.to_string(), OV::Str(format!("[\"content\",\"parts\",{},\"text\"]", i))));
                    }
                }
                _ if part_kind.is_some_and(|k| CHATGPT_MEDIA.contains(&k)) => {}
                _ => unknown.push(format!("part {}", part_kind.unwrap_or("(none)"))),
            }
        }
        return Ok((fields, unknown));
    }
    let names: &[&str] = match kind {
        Some("thoughts" | "reasoning_recap" | "computer_output") => return Ok((fields, Vec::new())),
        Some("code" | "execution_output") => &["text"],
        Some("tether_browsing_display") => &["result", "summary"],
        Some("tether_quote") => &["title", "text"],
        Some("sonic_webpage") => &["title", "snippet", "text"],
        Some("system_error") => &["name", "text"],
        Some("user_editable_context") => &["user_profile", "user_instructions"],
        Some(other) => return Ok((fields, vec![format!("content type {}", other)])),
        None => return Ok((fields, vec!["content type (none)".to_string()])),
    };
    if kind == Some("user_editable_context") {
        role = "system";
    }
    for name in names {
        if let Some(value) = content.str_of(name).filter(|v| !v.is_empty()) {
            fields.push((role, value.to_string(), OV::Str(format!("[\"content\",\"{}\"]", name))));
        }
    }
    Ok((fields, Vec::new()))
}

/// `input_strings(value, field)`: decoded tool argument strings with their field paths.
fn input_strings(value: &OV, path: &mut Vec<OV>, out: &mut Vec<(String, String)>) {
    match value {
        OV::Str(s) => {
            let mut field = String::from("[");
            for (n, part) in path.iter().enumerate() {
                if n > 0 {
                    field.push(',');
                }
                match part {
                    OV::Int(i) => field.push_str(&i.to_string()),
                    // As json.dumps(ensure_ascii=False) writes it: a lone surrogate stays raw.
                    OV::Str(k) => field.push_str(&ojson::dumps_raw(&OV::Str(k.clone()))),
                    _ => {}
                }
            }
            field.push(']');
            out.push((s.clone(), field));
        }
        OV::Obj(o) => {
            for (key, child) in &o.0 {
                path.push(OV::Str(key.clone()));
                input_strings(child, path, out);
                path.pop();
            }
        }
        OV::Arr(items) => {
            for (i, child) in items.iter().enumerate() {
                path.push(OV::Int(i as i64));
                input_strings(child, path, out);
                path.pop();
            }
        }
        _ => {}
    }
}

/// `claude_export_fields(message)`: (fields, unknown block types). Err for an unknown sender or shape.
fn claude_export_fields(message: &Obj) -> Result<(Fields, Vec<String>), ()> {
    let role: &'static str = match message.str_of("sender") {
        Some("human") => "user",
        Some("assistant") => "assistant",
        _ => return Err(()),
    };
    let Some(OV::Arr(content)) = message.get("content") else { return Err(()) };
    let mut fields = Vec::new();
    let mut unknown = Vec::new();
    for (i, block) in content.iter().enumerate() {
        let block = block.as_obj();
        match block.and_then(|b| b.get("type")).and_then(OV::as_str) {
            Some("text" | "voice_note") => {
                if let Some(t) = block.and_then(|b| b.str_of("text")).filter(|t| !t.is_empty()) {
                    fields.push((role, t.to_string(), OV::Int(i as i64)));
                }
            }
            Some("tool_use") => {
                let empty = OV::Obj(Obj::new());
                let value = block.and_then(|b| b.get("input")).unwrap_or(&empty);
                let mut found = Vec::new();
                input_strings(value, &mut vec![OV::Int(i as i64), OV::str("input")], &mut found);
                fields.extend(found.into_iter().filter(|(t, _)| !t.is_empty()).map(|(t, f)| ("tool_input", t, OV::Str(f))));
            }
            Some("tool_result") => match block.and_then(|b| b.get("content")) {
                None | Some(OV::Null) => {}
                Some(OV::Str(s)) => {
                    if !s.is_empty() {
                        fields.push(("tool", s.clone(), OV::Int(i as i64)));
                    }
                }
                Some(OV::Arr(items)) => {
                    for (j, sub) in items.iter().enumerate() {
                        let sub_kind = sub.as_obj().and_then(|s| s.get("type")).and_then(OV::as_str);
                        if !sub_kind.is_some_and(|k| CLAUDE_RESULT_ITEMS.contains(&k)) {
                            unknown.push(format!("tool result {}", sub_kind.unwrap_or("(none)")));
                        } else if let Some(t) = sub.as_obj().and_then(|s| s.str_of("text")).filter(|t| !t.is_empty()) {
                            fields.push(("tool", t.to_string(), OV::Str(format!("{}.content[{}]", i, j))));
                        }
                    }
                }
                Some(_) => unknown.push("tool result content".to_string()),
            },
            Some("thinking" | "token_budget" | "flag") => {}
            Some(other) => unknown.push(format!("content type {}", other)),
            None => unknown.push("content type (none)".to_string()),
        }
    }
    if let Some(OV::Arr(items)) = message.get("attachments") {
        for (i, item) in items.iter().enumerate() {
            let Some(item) = item.as_obj() else {
                unknown.push("attachment".to_string());
                continue;
            };
            if let Some(t) = item.str_of("extracted_content").filter(|t| !t.is_empty()) {
                fields.push(("attachment", t.to_string(), OV::Str(format!("[\"attachments\",{},\"extracted_content\"]", i))));
            }
        }
    }
    Ok((fields, unknown))
}

/// `export_span(row)`.
fn read_span(row: &Obj) -> Result<Vec<u8>, PyOsError> {
    let p = pathx::norm(row.str_of("path").unwrap_or(""));
    let mut f = pyio::open(&p)?;
    f.seek(SeekFrom::Start(row.int_of("export_offset").unwrap_or(0).max(0) as u64)).map_err(|e| pyio::read_err(&e))?;
    pyio::read_up_to(&mut f, row.int_of("bytes").unwrap_or(0).max(0) as u64).map_err(|e| pyio::read_err(&e))
}

/// SHA-256 of a row's span, for `verify_search_snapshot`.
pub fn span_digest(row: &Obj) -> Result<String, PyOsError> {
    read_span(row).map(|data| sha256_hex(&data))
}

/// `export_message_fields(row, coverage, strong, report)`: `visit(row, number, message, current,
/// fields)` per message this reader can read, after that message's problems are recorded; `visit`
/// returning false stops, as a consumer leaving the Python generator does. A span that differs
/// from the inventory's is checked again for depth and lookalike characters. With `report` false
/// (listing), what this reader cannot read is skipped without being counted.
fn each_message(row: &mut Obj, cov: &mut Obj, strong: bool, report: bool,
                visit: &mut dyn FnMut(&Obj, usize, &Obj, &OV, Fields) -> bool) {
    let p = pathx::norm(row.str_of("path").unwrap_or(""));
    let data = match read_span(row) {
        Ok(data) => data,
        Err(e) => {
            if row.get("_prefix_sha256").is_some_and(OV::truthy) {
                cov.set("snapshot_invalid", OV::Bool(true));
            }
            problem(cov, "read_error", &p, &e.text);
            return;
        }
    };
    let digest = sha256_hex(&data);
    let guard = OV::Arr(vec![row.get("export_offset").cloned().unwrap_or(OV::Null), row.get("bytes").cloned().unwrap_or(OV::Null),
                             OV::Str(digest.clone())]);
    let guards: &[OV] = match row.get("_guards") {
        Some(OV::Arr(items)) => items,
        _ => &[],
    };
    let same_length = Some(data.len() as i64) == row.int_of("bytes");
    let guards_differ = guards.iter().any(|g| !py_eq(g, &guard));
    let verified = !guards.is_empty() && same_length && !guards_differ;
    if !same_length || guards_differ {
        problem(cov, "changed_since_inventory", &p, "");
    }
    let expected = row.get("_prefix_sha256").filter(|v| v.truthy()).cloned();
    if let Some(expected) = &expected {
        if expected.as_str() != Some(digest.as_str()) {
            cov.set("snapshot_invalid", OV::Bool(true));
            problem(cov, "snapshot_prefix_changed", &p, "");
        }
    }
    if strong || expected.is_some() {
        row.set("_prefix_sha256", OV::Str(digest));
    }
    let place = format!("conversation {}", row.int_of("export_conversation").unwrap_or(0));
    let kind = row.str_of("agent").unwrap_or("").to_string();
    let unsupported = |cov: &mut Obj, detail: &str| {
        if report {
            bump(cov, "unsupported_records", 1);
            problem(cov, "unsupported_record", &p, detail);
        }
    };
    let readable = |text: &&str| verified || !(nested_deeper(text.as_bytes(), EXPORT_DEPTH_LIMIT) || has_lookalike(text));
    let messages = match std::str::from_utf8(&data).ok().filter(readable).and_then(|text| ojson::loads(text).ok()) {
        Some(OV::Obj(conversation)) => export_messages(&kind, &conversation).ok(),
        _ => None,
    };
    let Some(messages) = messages else {
        unsupported(cov, &place);
        return;
    };
    let id_key = if kind == "chatgpt_export" { "id" } else { "uuid" };
    let mut identities: HashSet<String> = HashSet::new();
    for (n, (message, current)) in messages.into_iter().enumerate() {
        let number = n + 1;
        if let Some(ident) = message.as_obj().and_then(|m| m.str_of(id_key)) {
            // A message id names one message; a second message with it is not read.
            if !identities.insert(ident.to_string()) {
                unsupported(cov, &format!("{} message {}: duplicate message id", place, number));
                continue;
            }
        }
        let read = match &message {
            OV::Obj(m) if kind == "chatgpt_export" => chatgpt_fields(m),
            OV::Obj(m) => claude_export_fields(m),
            _ => Err(()),
        };
        let Ok((fields, unknown)) = read else {
            unsupported(cov, &format!("{} message {}", place, number));
            continue;
        };
        for name in unknown {
            unsupported(cov, &format!("{} message {}: {}", place, number, name));
        }
        let OV::Obj(message) = message else { continue };
        if !visit(row, number, &message, &current, fields) {
            return;
        }
    }
}

/// `export_records(row, coverage, needle, strong)`: `line` numbers the conversation's messages in
/// reading order, `byte_offset` is None, and `record_sha256` hashes the message object as
/// `json.dumps` writes it (ASCII, default separators).
pub fn export_records(row: &mut Obj, cov: &mut Obj, needle: Option<&str>, strong: bool, emit: &mut Emit) -> Result<(), Fail> {
    let chatgpt = row.str_of("agent") == Some("chatgpt_export");
    each_message(row, cov, strong, true, &mut |row, number, message, current, fields| {
        let fields: Fields = match needle {
            Some(needle) => fields.into_iter().filter(|f| casefold(&f.1).contains(needle)).collect(),
            None => fields,
        };
        if fields.is_empty() {
            return true;
        }
        let digest = sha256_hex(ojson::dumps(&OV::Obj(message.clone()), true).as_bytes());
        let (timestamp, record_id) = if chatgpt {
            (epoch_iso(message.get("create_time")), message.get("id"))
        } else {
            (str_or_null(message.get("created_at")), message.get("uuid"))
        };
        let record_id = str_or_null(record_id);
        for (role, text, block) in fields {
            let (provenance, confidence) = classify(row, &OV::str(role), &text);
            let shown = match role {
                "tool_input" => "assistant",
                "attachment" => "user",
                other => other,
            };
            let mut hit = row.clone();
            hit.set("line", OV::Int(number as i64));
            hit.set("byte_offset", OV::Null);
            hit.set("block", block);
            hit.set("record_sha256", OV::Str(digest.clone()));
            hit.set("record_id", record_id.clone());
            hit.set("timestamp", timestamp.clone());
            hit.set("timestamp_basis", OV::str(if timestamp.truthy() { "record" } else { "unknown" }));
            hit.set("role", OV::str(shown));
            hit.set("record_session_id", OV::Null);
            hit.set("provenance", OV::str(provenance));
            hit.set("content_provenance", OV::str(provenance));
            hit.set("confidence", OV::str(confidence));
            hit.set("text", OV::Str(text));
            hit.set("self", OV::Bool(false));
            if !current.is_null() {
                hit.set("current_branch", current.clone());
            }
            if !emit(hit) {
                return false;
            }
        }
        true
    });
    Ok(())
}

/// `export_list_row(row, coverage)`: the conversation's own span is read whole, and its SHA-256
/// pins the span for a paging snapshot. Like a transcript's list read, it skips what it cannot
/// read without counting it.
pub fn export_list_row(row: &mut Obj, cov: &mut Obj) -> Obj {
    let chatgpt = row.str_of("agent") == Some("chatgpt_export");
    let mut preview = OV::Null;
    let mut last = OV::Null;
    each_message(row, cov, true, false, &mut |_, _, message, _, fields| {
        let stamp = if chatgpt { epoch_iso(message.get("create_time")) } else { str_or_null(message.get("created_at")) };
        if !fields.is_empty() && time_key(Some(&stamp)) > time_key(Some(&last)) {
            last = stamp;
        }
        if preview.is_null() {
            if let Some((_, text, _)) = fields.iter().find(|f| f.0 == "user") {
                preview = OV::Str(text.chars().take(160).collect());
            }
        }
        true
    });
    let basis = if last.truthy() { "latest message in the exported conversation" } else { "unknown" };
    public_row(row)
        .with("timestamp", last)
        .with("timestamp_basis", OV::str(basis))
        .with("preview", preview)
        .with("title", row.get("title").cloned().unwrap_or(OV::Null))
        .with("inspected_bytes", row.get("bytes").cloned().unwrap_or(OV::Null))
}

type SpanKey = (String, Option<i64>);

fn span_key(row: &Obj) -> SpanKey {
    (row.str_of("path").unwrap_or("").to_string(), row.int_of("export_offset"))
}

/// `reopen_export(hit, query, captured, cache)` for the saved matches of one page: each
/// conversation is read once, from the row the snapshot captured, whose span digest the bytes must
/// still match.
pub struct Reopen {
    captured: HashMap<SpanKey, Obj>,
    /// The (line, block) of each field the page shows, by span; only those are kept.
    wanted: HashMap<SpanKey, HashSet<(i64, String)>>,
    /// Per span, each shown field's digest and excerpt, not its whole text.
    cache: HashMap<SpanKey, HashMap<(i64, String), (OV, Obj)>>,
}

/// The captured rows of export conversations, by span (a later row with the same span wins).
fn captured_rows(rows: &[Obj]) -> HashMap<SpanKey, &Obj> {
    rows.iter().filter(|r| is_export(r)).map(|r| (span_key(r), r)).collect()
}

/// The span guard the inventory captured for an export hit, or none.
pub fn captured_guards(rows: &[Obj], hit: &Obj) -> OV {
    captured_rows(rows).get(&span_key(hit)).and_then(|r| r.get("_guards")).cloned().unwrap_or(OV::Arr(Vec::new()))
}

impl Reopen {
    pub fn new(rows: &[Obj], page: &[OV]) -> Reopen {
        let captured = captured_rows(rows).into_iter().map(|(k, r)| (k, r.clone())).collect();
        let mut wanted: HashMap<SpanKey, HashSet<(i64, String)>> = HashMap::new();
        for hit in page.iter().filter_map(OV::as_obj).filter(|h| is_export(h)) {
            let block = ojson::dumps(hit.get("block").unwrap_or(&OV::Null), true);
            wanted.entry(span_key(hit)).or_default().insert((hit.int_of("line").unwrap_or(0), block));
        }
        Reopen { captured, wanted, cache: HashMap::new() }
    }

    pub fn reopen(&mut self, hit: &Obj, query: &str) -> Result<Obj, Fail> {
        let path = hit.str_of("path").unwrap_or("").to_string();
        let key = span_key(hit);
        if !self.cache.contains_key(&key) {
            let Some(row) = self.captured.get(&key).filter(|r| r.get("_prefix_sha256").is_some_and(OV::truthy)) else {
                return Err(Fail::Stale(format!("captured source unavailable: {}", path)));
            };
            let mut row = row.clone();
            let mut part = Obj::new().with("complete", OV::Bool(true)).with("errors", OV::Arr(Vec::new()))
                .with("notes", OV::Arr(Vec::new())).with("malformed_records", OV::Int(0)).with("unfinished_records", OV::Int(0))
                .with("changed_files", OV::Int(0)).with("unsupported_records", OV::Int(0));
            let mut found: HashMap<(i64, String), (OV, Obj)> = HashMap::new();
            let empty = HashSet::new();
            let needed = self.wanted.get(&key).unwrap_or(&empty);
            export_records(&mut row, &mut part, None, true, &mut |record: Obj| {
                let place = (record.int_of("line").unwrap_or(0), ojson::dumps(record.get("block").unwrap_or(&OV::Null), true));
                if needed.contains(&place) && !found.contains_key(&place) {
                    let piece = excerpt(&Obj::new().with("text", record.get("text").cloned().unwrap_or(OV::Null)), query);
                    found.insert(place, (record.get("record_sha256").cloned().unwrap_or(OV::Null), piece));
                }
                true
            })?;
            let damaged = match part.get("errors") {
                Some(OV::Arr(errors)) => errors.iter().any(|e| {
                    matches!(e.as_obj().and_then(|e| e.str_of("kind")), Some("read_error" | "changed_since_inventory"))
                }),
                _ => false,
            };
            if part.get("snapshot_invalid").is_some_and(OV::truthy) || damaged {
                return Err(Fail::Stale(format!("captured source prefix changed: {}", path)));
            }
            self.cache.insert(key.clone(), found);
        }
        let block = ojson::dumps(hit.get("block").unwrap_or(&OV::Null), true);
        let Some((digest, piece)) = self.cache[&key].get(&(hit.int_of("line").unwrap_or(0), block)) else {
            return Err(Fail::Stale("matching content block unavailable".into()));
        };
        if Some(digest) != hit.get("record_sha256") {
            return Err(Fail::Stale(format!("matching record changed: {}", path)));
        }
        // The same fields, in the same order, as excerpt(hit with the whole text, query).
        let field = |k: &str| piece.get(k).cloned().unwrap_or(OV::Null);
        Ok(public_row(&hit.clone().with("text", field("text")))
            .with("text_truncated", field("text_truncated"))
            .with("text_start_character", field("text_start_character")))
    }
}
