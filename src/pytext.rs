//! Python `str` semantics the tool depends on: `casefold`, `isspace`/`lstrip`/`strip`,
//! code-point indexing, and 2.3.0's `classify` and `excerpt`.

use crate::ojson::{Obj, OV};
use crate::tables::{FOLD, SPACE};

/// Python `str.casefold()` (tables generated from the same Python).
pub fn casefold(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    casefold_into(text, &mut out);
    out
}

pub fn casefold_into(text: &str, out: &mut String) {
    out.clear();
    if text.is_ascii() {
        out.push_str(text);
        out.make_ascii_lowercase();
        return;
    }
    for c in text.chars() {
        if c.is_ascii() {
            out.push(c.to_ascii_lowercase());
        } else {
            match FOLD.binary_search_by_key(&(c as u32), |&(cp, _)| cp) {
                Ok(i) => out.push_str(FOLD[i].1),
                Err(_) => out.push(c),
            }
        }
    }
}

/// Length of `casefold(c)` in code points.
pub fn fold_len(c: char) -> usize {
    if c.is_ascii() {
        return 1;
    }
    match FOLD.binary_search_by_key(&(c as u32), |&(cp, _)| cp) {
        Ok(i) => FOLD[i].1.chars().count(),
        Err(_) => 1,
    }
}

pub fn is_py_space(c: char) -> bool {
    SPACE.binary_search(&(c as u32)).is_ok()
}

pub fn lstrip(text: &str) -> &str {
    text.trim_start_matches(is_py_space)
}

pub fn strip(text: &str) -> &str {
    text.trim_matches(is_py_space)
}

/// Python `bytes.strip()` emptiness: only ASCII whitespace (space, \t\n\r\x0b\x0c).
pub fn bytes_blank(raw: &[u8]) -> bool {
    raw.iter().all(|b| matches!(b, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c))
}

/// Python `text[start:start+length]` by code points.
pub fn slice_chars(text: &str, start: usize, length: usize) -> String {
    text.chars().skip(start).take(length).collect()
}

pub fn char_len(text: &str) -> usize {
    text.chars().count()
}

/// Python `str.find` on code points; -1 when absent.
pub fn find_chars(haystack: &str, needle: &str) -> i64 {
    match haystack.find(needle) {
        Some(byte) => haystack[..byte].chars().count() as i64,
        None => -1,
    }
}

/// 2.3.0 `classify(row, role, text)`.
pub fn classify(row: &Obj, role: &OV, text: &str) -> (&'static str, &'static str) {
    let kind = row.str_of("kind").unwrap_or("");
    if kind == "document" {
        return ("document", "source_type");
    }
    if kind == "project_instructions" {
        return ("project_instructions", "source_type");
    }
    if kind == "project_source" {
        return ("project_source", "source_type");
    }
    if kind == "import" {
        return ("import_copy", "import_registry");
    }
    let role_str = role.as_str();
    match role_str {
        Some("summary") => return ("compaction_summary", "record_type"),
        Some("tool") => return ("tool_output", "record_type"),
        Some("tool_input") => return ("tool_input", "record_type"),
        Some("metadata") => return ("title_metadata", "record_type"),
        Some("system") if row.str_of("agent") == Some("antigravity") => return ("injected_context", "record_type"),
        _ => {}
    }
    let lower = casefold(lstrip(text));
    if lower.contains("<codex_delegation>") || ((kind == "subagent" || kind == "reviewer") && role_str == Some("user")) {
        return ("delegated_prompt", "metadata_or_marker");
    }
    const TOKENS: [&str; 8] = ["<memory>", "<environment_context>", "<user_instructions>", "<system-reminder>",
                               "memory_summary begins", "# agents.md instructions",
                               "current-frontier (memory status head)", "## skills\n"];
    if TOKENS.iter().any(|t| lower.contains(t)) {
        return ("injected_context", "wrapper_heuristic");
    }
    if role_str == Some("user") && (lower.starts_with('>') || lower.starts_with("```") || lower.starts_with("<external_")) {
        return ("quoted_or_pasted", "format_heuristic");
    }
    if role_str == Some("user") {
        return ("user_message", "role_only");
    }
    (if role_str == Some("assistant") { "assistant" } else { "unknown" }, "record_role")
}

/// 2.3.0 `excerpt(hit, query, length=800)`: drops private keys, slices around the first match.
pub fn excerpt(hit: &Obj, query: &str) -> Obj {
    const LENGTH: usize = 800;
    let mut out = Obj(hit.0.iter().filter(|(k, _)| !k.starts_with('_')).cloned().collect());
    let text = match out.get("text") {
        Some(OV::Str(s)) => s.clone(),
        _ => String::new(),
    };
    let folded = casefold(&text);
    let text_len = char_len(&text);
    let mut index: i64 = if query.is_empty() { 0 } else { find_chars(&folded, &casefold(query)) };
    if index > 0 && char_len(&folded) != text_len {
        let mut folded_position = 0usize;
        for (original_position, c) in text.chars().enumerate() {
            folded_position += fold_len(c);
            if folded_position as i64 > index {
                index = original_position as i64;
                break;
            }
        }
    }
    let start = (index - (LENGTH / 3) as i64).max(0) as usize;
    out.set("text", OV::Str(slice_chars(&text, start, LENGTH)));
    out.set("text_truncated", OV::Bool(start > 0 || start + LENGTH < text_len));
    out.set("text_start_character", OV::Int(start as i64));
    out
}
