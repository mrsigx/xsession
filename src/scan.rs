//! 2.3.0 `records`, `package_records` and `context_for`. A search reads each record with
//! serde_json first (fast, key order not kept); records with a matching field, and anything
//! serde_json rejects, are decoded again with the exact Python-compatible parser.

use crate::fields::{row_blocks, Role};
use crate::hash::{sha256_hex, PrefixCheck};
use crate::inventory::{bump, note, problem};
use crate::ojson::{self, Obj, OV};
use crate::package::package_units;
use crate::pathx;
use crate::pyerr::{decode_utf8_sig, os_text, Fail, PyOsError};
use crate::pyio;
use crate::pyobj::py_eq;
use crate::pytext::{bytes_blank, casefold, casefold_into, classify, excerpt};
use crate::session;
use std::collections::VecDeque;
use std::io::{BufReader, Read};

/// Environment identities for the `self` flag, read once.
pub struct SelfIds(pub Vec<OV>);

impl SelfIds {
    pub fn from_env() -> SelfIds {
        let get = |n: &str| crate::inventory::env(n).map_or(OV::Null, OV::Str);
        SelfIds(vec![get("CODEX_THREAD_ID"), get("CLAUDE_CODE_SESSION_ID")])
    }
    fn contains(&self, id: &OV) -> bool {
        self.0.iter().any(|v| py_eq(v, id))
    }
}

fn row_int(row: &Obj, key: &str) -> i64 {
    row.get(key).and_then(OV::as_i64).unwrap_or(0)
}

/// One searchable field of a record, decoded exactly.
struct Field {
    role: OV,
    text: String,
    block: OV,
}

fn exact_fields(antigravity: bool, event: &OV) -> Result<Vec<Field>, String> {
    let mut out = Vec::new();
    row_blocks(antigravity, event, true, &mut |role, text, block| {
        let role = match role {
            Role::Name(n) => OV::str(n),
            Role::Value(v) => v.clone(),
        };
        out.push(Field { role, text: text.to_string(), block: block.to_ov() });
    })?;
    Ok(out)
}

/// The hit dict `records()` yields.
#[allow(clippy::too_many_arguments)]
fn make_hit(row: &Obj, number: u64, start: OV, field: Field, digest: &Option<String>, event: &Obj, ids: &SelfIds) -> Obj {
    let mut role = field.role;
    if event.get("isCompactSummary").is_some_and(OV::truthy) || event.get("is_compact_summary").is_some_and(OV::truthy) {
        role = OV::str("summary");
    }
    let (mut provenance, mut confidence) = classify(row, &role, &field.text);
    let content_provenance = provenance;
    let record_session_id = if row.str_of("agent") == Some("claude") { event.get("sessionId").cloned().unwrap_or(OV::Null) } else { OV::Null };
    let row_id = row.get("id").cloned().unwrap_or(OV::Null);
    if record_session_id.truthy() && row.str_of("kind") == Some("conversation") && !py_eq(&record_session_id, &row_id) {
        provenance = "resumed_history";
        confidence = "record_session_id";
    }
    let antigravity = row.str_of("agent") == Some("antigravity");
    let (timestamp, record_id) = if antigravity {
        (event.get("created_at").cloned().unwrap_or(OV::Null), event.get("step_index").cloned().unwrap_or(OV::Null))
    } else {
        let mut timestamp = event.get("timestamp").cloned().unwrap_or(OV::Null);
        if !timestamp.truthy() {
            if let Some(OV::Obj(payload)) = event.get("payload") {
                timestamp = payload.get("timestamp").cloned().unwrap_or(OV::Null);
            }
        }
        let record_id = match event.get("uuid").filter(|v| v.truthy()) {
            Some(v) => v.clone(),
            None => event.get("id").cloned().unwrap_or(OV::Null),
        };
        (timestamp, record_id)
    };
    let basis = if timestamp.truthy() { "record" } else { "unknown" };
    let shown_role = if role.as_str() == Some("tool_input") { OV::str("assistant") } else { role };
    let mut hit = row.clone();
    hit.set("line", OV::Int(number as i64));
    hit.set("byte_offset", start);
    hit.set("block", field.block);
    hit.set("record_sha256", digest.clone().map_or(OV::Null, OV::Str));
    hit.set("record_id", record_id);
    hit.set("timestamp", timestamp);
    hit.set("timestamp_basis", OV::str(basis));
    hit.set("role", shown_role);
    hit.set("record_session_id", record_session_id);
    hit.set("provenance", OV::str(provenance));
    hit.set("content_provenance", OV::str(content_provenance));
    hit.set("confidence", OV::str(confidence));
    hit.set("text", OV::Str(field.text));
    hit.set("self", OV::Bool(ids.contains(&row_id)));
    if antigravity {
        // Those fields keep their first and last ~2 KiB here; transcript_full.jsonl has them whole.
        if let Some(fields) = event.get("truncated_fields").filter(|v| v.truthy()) {
            hit.set("truncated_fields", fields.clone());
        }
    }
    hit
}

/// Whether the record has a field matching `needle`, using the fast decoder.
/// Ok(None): serde_json rejected the record (decode exactly instead). Err: unsupported shape.
fn fast_match(antigravity: bool, raw: &[u8], needle: &str, buf: &mut String) -> Result<Option<bool>, Result<(), String>> {
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(raw) else { return Ok(None) };
    if !value.is_object() {
        return Err(Ok(()));
    }
    let mut matched = false;
    row_blocks(antigravity, &value, false, &mut |_, text, _| {
        if !matched {
            casefold_into(text, buf);
            matched = buf.contains(needle);
        }
    })
    .map_err(Err)?;
    Ok(Some(matched))
}

pub type Emit<'e> = dyn FnMut(Obj) -> bool + 'e;

/// `records(row, coverage, needle, strong)`: calls `emit` per hit; `emit` returning false
/// stops the generator (its closing checks then do not run, as in Python). Err carries an
/// exception 2.3.0 does not catch here (it ends the run).
pub fn records(row: &mut Obj, cov: &mut Obj, needle: Option<&str>, strong: bool, ids: &SelfIds, emit: &mut Emit) -> Result<(), Fail> {
    if matches!(row.str_of("format"), Some("docx") | Some("xlsx")) {
        return package_records(row, cov, needle, strong, emit);
    }
    let text_mode = row.str_of("kind") == Some("document") || row.str_of("format") == Some("text");
    let p = pathx::norm(row.str_of("path").unwrap_or(""));
    if let Err(e) = records_inner(row, cov, needle, strong, ids, emit, text_mode, &p) {
        if row.get("_prefix_sha256").is_some_and(OV::truthy) {
            cov.set("snapshot_invalid", OV::Bool(true));
        }
        problem(cov, "read_error", &p, &e.text);
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn records_inner(row: &mut Obj, cov: &mut Obj, needle: Option<&str>, strong: bool, ids: &SelfIds, emit: &mut Emit,
                 text_mode: bool, p: &str) -> Result<(), PyOsError> {
    let compressed = session::is_compressed(p);
    let antigravity = row.str_of("agent") == Some("antigravity");
    let before = session::stat(p, None)?;
    let mut boundary = row_int(row, "bytes").max(0) as u64;
    let expected = row.get("_prefix_sha256").filter(|v| v.truthy()).and_then(OV::as_str).map(str::to_string);
    let strong = strong || expected.is_some();
    let mut digest = PrefixCheck::new(strong);
    let changed = before.size < boundary
        || (expected.is_none()
            && (!pyio::guards_match(p, row.get("_guards"))?
                || (before.size == boundary && !py_eq(&crate::inventory::int_ov(before.mtime_ns), row.get("mtime_ns").unwrap_or(&OV::Null)))));
    if changed {
        problem(cov, "changed_since_inventory", p, "");
    } else if before.size > boundary {
        note(cov, Obj::new().with("kind", OV::str("append_outside_snapshot")).with("path", OV::str(p)).with("captured_bytes", OV::Int(boundary as i64)));
    }
    let mut offset: u64 = 0;
    let file = session::open(p)?;
    let mut reader = BufReader::with_capacity(session::read_ahead(p, 1 << 20), file);
    let mut raw: Vec<u8> = Vec::with_capacity(1 << 16);
    let mut buf = String::new();
    let mut number: u64 = 0;
    let empty = Obj::new();
    while offset < boundary {
        pyio::read_line(&mut reader, boundary - offset, &mut raw).map_err(|e| pyio::read_err(&e))?;
        if raw.is_empty() {
            break;
        }
        number += 1;
        let start = offset;
        offset += raw.len() as u64;
        if !text_mode && raw.last() != Some(&b'\n') && offset == boundary {
            bump(cov, "unfinished_records", 1);
            note(cov, Obj::new().with("kind", OV::str("unfinished_tail_excluded")).with("path", OV::str(p)).with("bytes", OV::Int(raw.len() as i64)));
            boundary = start;
            row.set("bytes", OV::Int(start as i64));
            if !compressed {
                row.set("_guards", pyio::capture_guards(p, start)?);
            }
            break;
        }
        digest.update(&raw);
        if bytes_blank(&raw) {
            continue;
        }
        let mut event = OV::Obj(Obj::new());
        let fields: Vec<Field> = if text_mode {
            match decode_utf8_sig(&raw) {
                Ok(text) => vec![Field { role: OV::str("document"), text: text.trim_end_matches(['\r', '\n']).to_string(), block: OV::Int(0) }],
                Err(m) => {
                    problem(cov, "encoding_error", p, &m);
                    continue;
                }
            }
        } else {
            if let Some(needle) = needle {
                match fast_match(antigravity, &raw, needle, &mut buf) {
                    Ok(Some(false)) => continue,
                    Ok(Some(true)) | Ok(None) => {}
                    Err(Ok(())) => {
                        bump(cov, "unsupported_records", 1);
                        problem(cov, "unsupported_record", p, &format!("line {}", number));
                        continue;
                    }
                    Err(Err(m)) => {
                        bump(cov, "unsupported_records", 1);
                        problem(cov, "unsupported_record", p, &format!("line {}: {}", number, m));
                        continue;
                    }
                }
            }
            match ojson::loads_bytes(&raw) {
                Ok(v @ OV::Obj(_)) => event = v,
                Ok(_) => {
                    bump(cov, "unsupported_records", 1);
                    problem(cov, "unsupported_record", p, &format!("line {}", number));
                    continue;
                }
                Err(_) => {
                    let unfinished = raw.last() != Some(&b'\n') && offset == before.size;
                    let key = if unfinished { "unfinished_records" } else { "malformed_records" };
                    bump(cov, key, 1);
                    problem(cov, key, p, &format!("line {}", number));
                    continue;
                }
            }
            match exact_fields(antigravity, &event) {
                Ok(f) => f,
                Err(m) => {
                    bump(cov, "unsupported_records", 1);
                    problem(cov, "unsupported_record", p, &format!("line {}: {}", number, m));
                    continue;
                }
            }
        };
        let fields: Vec<Field> = match needle {
            Some(needle) => fields.into_iter().filter(|f| casefold(&f.text).contains(needle)).collect(),
            None => fields,
        };
        let record_digest = if fields.is_empty() { None } else { Some(sha256_hex(&raw)) };
        let event_obj = event.as_obj().unwrap_or(&empty);
        for field in fields {
            let hit = make_hit(row, number, OV::Int(start as i64), field, &record_digest, event_obj, ids);
            if !emit(hit) {
                return Ok(());
            }
        }
    }
    if compressed {
        // Decode to the end of the stream (final blocks, checksum, later frames), so every reader
        // meets a corrupt block wherever it lies.
        let mut sink = vec![0u8; 1 << 20];
        loop {
            match reader.read(&mut sink) {
                Ok(0) => break,
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(pyio::read_err(&e)),
            }
        }
    }
    drop(reader);
    let after = session::stat(p, Some(&before))?;
    let observed = digest.value();
    if strong {
        row.set("_prefix_sha256", OV::Str(observed.clone()));
    }
    if let Some(expected) = &expected {
        if &observed != expected {
            cov.set("snapshot_invalid", OV::Bool(true));
            problem(cov, "snapshot_prefix_changed", p, "");
        }
    }
    let moved = before != after;
    if after.size < boundary || offset < boundary || (moved && prefix_check(p, boundary, strong)? != observed) {
        bump(cov, "changed_files", 1);
        problem(cov, "changed_during_read", p, "");
    }
    Ok(())
}

/// `prefix_check(path, size, strong)` / `prefix_digest(path, size)`.
pub fn prefix_check(p: &str, size: u64, strong: bool) -> Result<String, PyOsError> {
    let mut check = PrefixCheck::new(strong);
    let mut f = session::open(p)?;
    let mut remaining = size;
    let mut chunk = vec![0u8; 1024 * 1024];
    while remaining > 0 {
        let want = remaining.min(chunk.len() as u64) as usize;
        let n = f.read(&mut chunk[..want]).map_err(|e| pyio::read_err(&e))?;
        if n == 0 {
            return Err(os_text("captured prefix was truncated"));
        }
        check.update(&chunk[..n]);
        remaining -= n as u64;
    }
    Ok(check.value())
}

/// `package_records(row, coverage, needle, strong)`.
fn package_records(row: &mut Obj, cov: &mut Obj, needle: Option<&str>, strong: bool, emit: &mut Emit) -> Result<(), Fail> {
    let p = pathx::norm(row.str_of("path").unwrap_or(""));
    let data = match pyio::read_bytes(&p).and_then(|data| {
        let same = data.len() as i64 == row_int(row, "bytes") && pyio::guards_match(&p, row.get("_guards"))?;
        Ok((data, same))
    }) {
        Ok((data, same)) => {
            if !same {
                problem(cov, "changed_since_inventory", &p, "");
            }
            data
        }
        Err(e) => {
            problem(cov, "read_error", &p, &e.text);
            return Ok(());
        }
    };
    let expected = row.get("_prefix_sha256").filter(|v| v.truthy()).cloned();
    if strong || expected.is_some() {
        let digest = sha256_hex(&data);
        if let Some(expected) = &expected {
            if expected.as_str() != Some(digest.as_str()) {
                cov.set("snapshot_invalid", OV::Bool(true));
                problem(cov, "snapshot_prefix_changed", &p, "");
            }
        }
        row.set("_prefix_sha256", OV::Str(digest));
    }
    let form = row.str_of("format").unwrap_or("").to_string();
    let units = match package_units(&form, &data) {
        Ok(u) => u,
        // LookupError is not among 2.3.0's PACKAGE_ERRORS: it ends the run.
        Err(e) if e.class == "LookupError" => return Err(Fail::Other(e.class.into(), e.msg)),
        Err(e) => {
            let detail: String = format!("{}: {}", e.class, e.msg.chars().take(300).collect::<String>());
            problem(cov, "project_file_unreadable", &p, &detail);
            return Ok(());
        }
    };
    for (i, text) in units.into_iter().enumerate() {
        if let Some(needle) = needle {
            if !casefold(&text).contains(needle) {
                continue;
            }
        }
        let (provenance, confidence) = classify(row, &OV::str("document"), &text);
        let mut hit = row.clone();
        hit.set("line", OV::Int(i as i64 + 1));
        hit.set("byte_offset", OV::Null);
        hit.set("block", OV::Int(0));
        hit.set("record_sha256", OV::Str(sha256_hex(text.as_bytes())));
        hit.set("record_id", OV::Null);
        hit.set("timestamp", OV::Null);
        hit.set("timestamp_basis", OV::str("unknown"));
        hit.set("role", OV::str("document"));
        hit.set("record_session_id", OV::Null);
        hit.set("provenance", OV::str(provenance));
        hit.set("content_provenance", OV::str(provenance));
        hit.set("confidence", OV::str(confidence));
        hit.set("text", OV::Str(text));
        hit.set("self", OV::Bool(false));
        if !emit(hit) {
            return Ok(());
        }
    }
    Ok(())
}

/// `context_for(row, line, count, coverage)`.
pub fn context_for(row: &mut Obj, line: i64, count: usize, cov: &mut Obj, ids: &SelfIds) -> Result<Vec<Obj>, Fail> {
    let mut before: VecDeque<Obj> = VecDeque::new();
    let mut selected = Vec::new();
    let mut found = false;
    let mut remaining = count;
    let mark = session::mark(cov);
    let path = pathx::norm(row.str_of("path").unwrap_or(""));
    let mut stopped = false;
    records(row, cov, None, true, ids, &mut |record: Obj| {
        let this = record.int_of("line") == Some(line);
        if this {
            if !found {
                selected.extend(before.drain(..));
            }
            selected.push(excerpt(&record, ""));
            found = true;
        } else if found {
            if remaining > 0 {
                selected.push(excerpt(&record, ""));
                remaining -= 1;
            } else {
                stopped = true;
                return false;
            }
        } else if count > 0 {
            if before.len() == count {
                before.pop_front();
            }
            before.push_back(excerpt(&record, ""));
        }
        true
    })?;
    if stopped && session::is_compressed(&path) && !session::undecodable(cov, &path, mark.errors) && !session::validates(&path) {
        problem(cov, "read_error", &path, session::UNDECODABLE);
    }
    if session::undecodable(cov, &path, mark.errors) {
        session::undecodable_only(cov, &mark, &path);
        return Ok(Vec::new());
    }
    Ok(selected)
}
