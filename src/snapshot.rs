//! Opt-in paging (2.3.0 `snapshot_directory`, `request_key`, `metadata_prefixes`, `load_snapshot`,
//! `save_snapshot`, `encode_cursor`, `verify_search_snapshot`, `reopen_hit`, `paginate`,
//! `fit_output`).

use crate::fields::row_blocks;
use crate::hash::sha256_hex;
use crate::inventory::VERSION;
use crate::ojson::{self, Obj, OV};
use crate::pathx;
use crate::pyerr::{decode_utf8_sig, os_error, os_text, type_name, utf8_encode_error, Api, Fail, PyOsError};
use crate::pyio;
use crate::pyobj::{dict_to_obj, py_eq, PyDict};
use crate::pytext::{casefold, excerpt};
use crate::scan::prefix_check;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};

pub const SNAPSHOT_SLOTS: i64 = 16;
const SNAPSHOT_MAX_BYTES: usize = 4 * 1024 * 1024;

/// The pieces of the parsed command line paging needs.
pub struct PageArgs<'a> {
    pub command: &'a str,
    pub snapshot_dir: Option<&'a str>,
    pub cursor: Option<&'a str>,
    pub context: i64,
    pub max_matches: Option<i64>,
    pub max_output_bytes: i64,
}

/// `snapshot_directory(args)`.
pub fn snapshot_directory(args: &PageArgs) -> Result<Option<String>, Fail> {
    let configured = match args.snapshot_dir {
        Some(d) if !d.is_empty() => Some(d.to_string()),
        _ => crate::inventory::env("XSESSION_SNAPSHOT_DIR").filter(|d| !d.is_empty()),
    };
    configured.map(|c| pathx::expanduser(&c)).transpose()
}

/// `request_key(args, config, query)`.
pub fn request_key(args: &PageArgs, config: &OV, query: &OV) -> String {
    let key = OV::Arr(vec![
        OV::str(VERSION),
        OV::str(args.command),
        config.clone(),
        query.clone(),
        OV::Int(args.context),
        args.max_matches.map_or(OV::Null, OV::Int),
    ]);
    sha256_hex(ojson::dumps_sorted(&key).as_bytes())
}

/// `metadata_prefixes(config)`.
pub fn metadata_prefixes(config: &Obj) -> Result<Vec<OV>, Fail> {
    let mut out = Vec::new();
    for name in ["title_index", "imports"] {
        if let Some(value) = config.get(name).filter(|v| v.truthy()) {
            let OV::Str(raw) = value else {
                return Err(Fail::Invalid(format!(
                    "argument should be a str or an os.PathLike object where __fspath__ returns a str, not '{}'", type_name(value))));
            };
            let path = pathx::norm(raw);
            if pyio::is_file(&path) {
                let size = pyio::stat(&path)?.size;
                let digest = prefix_check(&path, size, true)?;
                out.push(OV::Obj(Obj::new().with("path", OV::str(&path)).with("bytes", OV::Int(size as i64)).with("_prefix_sha256", OV::Str(digest))));
            }
        }
    }
    Ok(out)
}

// ------------------------------------------------------------------------------- base64 ---

fn b64_value(c: u8) -> Option<u8> {
    match c {
        b'A'..=b'Z' => Some(c - b'A'),
        b'a'..=b'z' => Some(c - b'a' + 26),
        b'0'..=b'9' => Some(c - b'0' + 52),
        b'+' | b'-' => Some(62),
        b'/' | b'_' => Some(63),
        _ => None,
    }
}

/// `base64.urlsafe_b64decode` (non-strict: other characters are skipped).
fn b64decode(s: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    let (mut quad, mut left, mut pads) = (0u8, 0u32, 0u8);
    for &c in s {
        if c == b'=' {
            if quad >= 2 {
                pads += 1;
                if quad + pads >= 4 {
                    return Ok(out);
                }
            }
            continue;
        }
        let Some(v) = b64_value(c) else { continue };
        pads = 0;
        let v = v as u32;
        match quad {
            0 => {
                left = v;
                quad = 1;
            }
            1 => {
                out.push(((left << 2) | (v >> 4)) as u8);
                left = v & 0x0F;
                quad = 2;
            }
            2 => {
                out.push(((left << 4) | (v >> 2)) as u8);
                left = v & 0x03;
                quad = 3;
            }
            _ => {
                out.push(((left << 6) | v) as u8);
                quad = 0;
            }
        }
    }
    if quad != 0 {
        return Err(if quad == 1 { "Invalid base64-encoded string".into() } else { "Incorrect padding".into() });
    }
    Ok(out)
}

fn b64encode_urlsafe(data: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    for chunk in data.chunks(3) {
        let n = (chunk[0] as u32) << 16 | (*chunk.get(1).unwrap_or(&0) as u32) << 8 | *chunk.get(2).unwrap_or(&0) as u32;
        out.push(A[(n >> 18) as usize & 63] as char);
        out.push(A[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { A[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { A[n as usize & 63] as char } else { '=' });
    }
    out
}

/// `encode_cursor(reference, offset)`.
pub fn encode_cursor(reference: &Obj, offset: i128) -> String {
    let cursor = reference.clone().with("offset", crate::inventory::int_ov(offset));
    b64encode_urlsafe(ojson::dumps(&OV::Obj(cursor), true).as_bytes())
}

fn slot_path(folder: &str, slot: i64) -> String {
    pathx::join(folder, &format!("xsession-snapshot-{:02}.json", slot))
}

pub struct Loaded {
    pub saved: Obj,
    pub reference: Obj,
    pub offset: i128,
}

fn is_hex(s: &str, n: usize) -> bool {
    s.len() == n && s.bytes().all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}

/// `load_snapshot(args, config, query)`.
pub fn load_snapshot(args: &PageArgs, config: &OV, query: &OV) -> Result<Loaded, Fail> {
    let invalid = || Fail::Invalid("invalid cursor".into());
    let cursor_text = args.cursor.unwrap_or("");
    if crate::pyerr::utf8_encode_error(cursor_text).is_some() {
        return Err(invalid());
    }
    let decoded = b64decode(cursor_text.as_bytes()).map_err(|_| invalid())?;
    let cursor = match ojson::loads_bytes(&decoded) {
        Ok(OV::Obj(c)) => c,
        _ => return Err(invalid()),
    };
    let (Some(name), Some(digest), Some(offset)) = (cursor.get("snapshot"), cursor.get("sha256"), cursor.get("offset")) else {
        return Err(invalid());
    };
    let (Some(name), Some(digest)) = (name.as_str(), digest.as_str()) else { return Err(invalid()) };
    if !is_hex(name, 32) || !is_hex(digest, 64) {
        return Err(invalid());
    }
    let offset: i128 = match offset {
        OV::Int(i) if *i >= 0 => *i as i128,
        OV::UInt(u) => *u as i128,
        OV::Big(b) if !b.starts_with('-') => i128::MAX.min(b.parse::<f64>().unwrap_or(f64::MAX) as i128),
        _ => return Err(invalid()),
    };
    let slot = match cursor.get("slot") {
        Some(OV::Int(s)) if (0..SNAPSHOT_SLOTS).contains(s) => *s,
        _ => return Err(Fail::Stale("unsupported cursor storage format; start a new search".into())),
    };
    let unavailable = || Fail::Stale("snapshot metadata unavailable or invalid".into());
    let folder = snapshot_directory(args)?.ok_or_else(|| Fail::Stale("continuation requires the original --snapshot-dir or XSESSION_SNAPSHOT_DIR".into()))?;
    let mut raw = Vec::new();
    match pyio::open(&slot_path(&folder, slot)) {
        Ok(f) => {
            if f.take(SNAPSHOT_MAX_BYTES as u64 + 1).read_to_end(&mut raw).is_err() {
                return Err(unavailable());
            }
        }
        Err(_) => return Err(unavailable()),
    }
    if raw.len() > SNAPSHOT_MAX_BYTES {
        return Err(Fail::Stale("snapshot exceeds the storage limit".into()));
    }
    if sha256_hex(&raw) != digest {
        return Err(Fail::Stale("snapshot metadata changed".into()));
    }
    let saved = match ojson::loads_bytes(&raw) {
        Ok(OV::Obj(s)) => s,
        Ok(other) => return Err(Fail::Other("AttributeError".into(), format!("'{}' object has no attribute 'get'", type_name(&other)))),
        Err(_) => return Err(unavailable()),
    };
    if !py_eq(saved.get("id").unwrap_or(&OV::Null), &OV::str(name)) {
        return Err(Fail::Stale("snapshot identity changed".into()));
    }
    match saved.get("request_key") {
        None => return Err(unavailable()),
        Some(k) if !py_eq(k, &OV::Str(request_key(args, config, query))) => {
            return Err(Fail::Stale("query or source configuration changed".into()))
        }
        _ => {}
    }
    let created = match saved.get("created_at") {
        Some(OV::Int(i)) => *i as f64,
        Some(OV::UInt(u)) => *u as f64,
        Some(OV::Float(f)) => *f,
        Some(OV::Bool(b)) => *b as i64 as f64,
        _ => return Err(unavailable()),
    };
    if now() - created > 86400.0 {
        return Err(Fail::Stale("snapshot is older than 24 hours; start a new search".into()));
    }
    let mut roots: Vec<(String, String)> = Vec::new();
    if let Some(OV::Arr(sources)) = config.as_obj().and_then(|c| c.get("sources")) {
        for s in sources.iter().filter_map(OV::as_obj) {
            let root = pathx::resolve(&pathx::expanduser(s.str_of("path").unwrap_or(""))?)?;
            let source_name = s.str_of("name").unwrap_or("").to_string();
            match roots.iter_mut().find(|(n, _)| *n == source_name) {
                Some(entry) => entry.1 = root,
                None => roots.push((source_name, root)),
            }
        }
    }
    let rows = match saved.get("rows") {
        Some(OV::Arr(rows)) => rows,
        Some(_) => return Err(Fail::Invalid("list indices must be integers or slices, not str".into())),
        None => return Err(Fail::Invalid("'rows'".into())),
    };
    for row in rows {
        let Some(row) = row.as_obj() else { return Err(Fail::Invalid("string indices must be integers, not 'str'".into())) };
        let source = row.get("source").ok_or_else(|| Fail::Invalid("'source'".into()))?;
        let root = source.as_str().and_then(|s| roots.iter().find(|(n, _)| n == s)).map(|(_, r)| r.clone());
        let Some(root) = root else { return Err(Fail::Stale("snapshot path falls outside configured sources".into())) };
        let path = match row.get("path") {
            Some(OV::Str(p)) => p.clone(),
            Some(other) => return Err(Fail::Invalid(format!(
                "argument should be a str or an os.PathLike object where __fspath__ returns a str, not '{}'", type_name(other)))),
            None => return Err(Fail::Invalid("'path'".into())),
        };
        if !pathx::is_relative_to(&pathx::resolve(&path)?, &root) {
            return Err(Fail::Stale("snapshot path falls outside configured sources".into()));
        }
    }
    let reference = Obj::new().with("snapshot", OV::str(name)).with("sha256", OV::str(digest)).with("slot", OV::Int(slot));
    Ok(Loaded { saved, reference, offset })
}

pub fn now() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

/// `pathlib.Path.mkdir(parents=True, exist_ok=True)`.
fn mkdir_parents(path: &str) -> Result<(), PyOsError> {
    match std::fs::create_dir(pathx::to_path(path)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let parent = pathx::parent(path);
            if parent == path {
                return Err(os_error(&e, Api::Os, Some(path)));
            }
            mkdir_parents(&parent)?;
            match std::fs::create_dir(pathx::to_path(path)) {
                Ok(()) => Ok(()),
                Err(e) if pyio::is_dir(path) => {
                    let _ = e;
                    Ok(())
                }
                Err(e) => Err(os_error(&e, Api::Os, Some(path))),
            }
        }
        Err(e) => {
            if pyio::is_dir(path) {
                Ok(())
            } else {
                Err(os_error(&e, Api::Os, Some(path)))
            }
        }
    }
}

/// The snapshot body; `save` writes it into the first free slot.
pub struct Snapshot<'a> {
    pub config: &'a OV,
    pub query: &'a OV,
    pub rows: &'a [Obj],
    pub titles: &'a PyDict<Obj>,
    pub titles_json: Option<&'a OV>,
    pub coverage: &'a Obj,
    pub created_at: f64,
    pub hits: &'a [Obj],
    pub metadata: &'a [OV],
}

/// `save_snapshot(...)`: Err(Fail::Os) is reported by the caller as pagination unavailable.
pub fn save_snapshot(args: &PageArgs, snap: &Snapshot, reference: &mut Obj) -> Result<(), Fail> {
    let folder = snapshot_directory(args)?.unwrap_or_default();
    let matches = if args.command == "find" {
        OV::Arr(snap.hits.iter().map(|h| OV::Obj(Obj(h.0.iter().filter(|(k, _)| k != "text" && k != "context").cloned().collect()))).collect())
    } else {
        OV::Null
    };
    let saved = Obj::new()
        .with("version", OV::str(VERSION))
        .with("id", reference.get("snapshot").cloned().unwrap_or(OV::Null))
        .with("request_key", OV::Str(request_key(args, snap.config, snap.query)))
        .with("created_at", OV::Float(snap.created_at))
        .with("rows", OV::Arr(snap.rows.iter().cloned().map(OV::Obj).collect()))
        .with("titles", snap.titles_json.cloned().unwrap_or_else(|| OV::Obj(dict_to_obj(&titles_ov(snap.titles)))))
        .with("coverage", OV::Obj(snap.coverage.clone()))
        .with("metadata", OV::Arr(snap.metadata.to_vec()))
        .with("matches", matches);
    let text = ojson::dumps_raw(&OV::Obj(saved));
    if let Some(msg) = utf8_encode_error(&text) {
        return Err(Fail::Invalid(msg));
    }
    let raw = text.into_bytes();
    if raw.len() > SNAPSHOT_MAX_BYTES {
        return Err(Fail::Os(os_text("snapshot exceeds 4 MiB; narrow the source or query")));
    }
    mkdir_parents(&folder)?;
    for slot in 0..SNAPSHOT_SLOTS {
        let path = slot_path(&folder, slot);
        let mut f = match std::fs::OpenOptions::new().write(true).create_new(true).open(pathx::to_path(&path)) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(Fail::Os(os_error(&e, Api::Open, Some(&path)))),
        };
        f.write_all(&raw).map_err(|e| Fail::Os(pyio::read_err(&e)))?;
        reference.set("sha256", OV::Str(sha256_hex(&raw)));
        reference.set("slot", OV::Int(slot));
        return Ok(());
    }
    Err(Fail::Os(os_text("snapshot store has 16 occupied slots; operator-managed cleanup or another explicitly selected directory is required")))
}

fn titles_ov(titles: &PyDict<Obj>) -> PyDict<OV> {
    let mut out = PyDict::new();
    for (k, v) in &titles.items {
        out.insert(k.clone(), OV::Obj(v.clone()));
    }
    out
}

/// `verify_search_snapshot(rows)`.
pub fn verify_search_snapshot(rows: &[OV]) -> Result<(), Fail> {
    for row in rows {
        let Some(row) = row.as_obj() else { continue };
        let Some(expected) = row.get("_prefix_sha256").filter(|v| v.truthy()) else { continue };
        let path = row.str_of("path").unwrap_or("").to_string();
        let size = row.int_of("bytes").unwrap_or(0).max(0) as u64;
        match prefix_check(&pathx::norm(&path), size, true) {
            Ok(d) if expected.as_str() == Some(d.as_str()) => {}
            Ok(_) => return Err(Fail::Stale(format!("captured source prefix changed: {}", path))),
            Err(_) => return Err(Fail::Stale(format!("captured source unavailable: {}", path))),
        }
    }
    Ok(())
}

/// `reopen_hit(hit, query, titles)` for a saved match.
pub fn reopen_hit(hit: &Obj, query: &str, titles: &Obj) -> Result<Obj, Fail> {
    let path = hit.str_of("path").unwrap_or("").to_string();
    if hit.str_of("provenance") == Some("title_metadata") {
        if let Some(OV::Obj(title)) = hit.str_of("id").and_then(|id| titles.get(id)) {
            if title.get("path").is_some_and(|p| py_eq(p, &OV::str(&path))) {
                return Ok(hit.clone().with("text", title.get("text").cloned().unwrap_or(OV::Null)));
            }
        }
    }
    let changed = || Fail::Stale(format!("matching record changed: {}", path));
    let line = hit.int_of("line").unwrap_or(0);
    let expected = hit.str_of("record_sha256").unwrap_or("").to_string();
    if let Some(form @ ("docx" | "xlsx")) = hit.str_of("format") {
        let unavailable = || Fail::Stale(format!("matching package unavailable: {}", path));
        let data = pyio::read_bytes(&pathx::norm(&path)).map_err(|_| unavailable())?;
        let units = match crate::package::package_units(form, &data) {
            Ok(u) => u,
            Err(e) if e.class == "LookupError" => return Err(Fail::Other(e.class.into(), e.msg)),
            Err(_) => return Err(unavailable()),
        };
        if line < 1 || line as usize > units.len() || sha256_hex(units[line as usize - 1].as_bytes()) != expected {
            return Err(changed());
        }
        let text = units[line as usize - 1].clone();
        return Ok(excerpt(&hit.clone().with("text", OV::Str(text)), query));
    }
    let mut f = crate::session::open(&pathx::norm(&path))?;
    let offset = hit.int_of("byte_offset").unwrap_or(0).max(0) as u64;
    f.seek(SeekFrom::Start(offset)).map_err(|e| pyio::read_err(&e))?;
    let mut raw = Vec::new();
    BufReader::new(f).read_until(b'\n', &mut raw).map_err(|e| pyio::read_err(&e))?;
    if sha256_hex(&raw) != expected {
        return Err(changed());
    }
    let text = if hit.str_of("kind") == Some("document") || hit.str_of("format") == Some("text") {
        decode_utf8_sig(&raw).map_err(Fail::Invalid)?.trim_end_matches(['\r', '\n']).to_string()
    } else {
        let event = ojson::loads_bytes(&raw).map_err(Fail::Invalid)?;
        let want = hit.get("block").cloned().unwrap_or(OV::Null);
        let mut texts: Vec<String> = Vec::new();
        row_blocks(hit.str_of("agent") == Some("antigravity"), &event, true, &mut |_, text, block| {
            if py_eq(&block.to_ov(), &want) {
                texts.push(text.to_string());
            }
        })
        .map_err(Fail::Invalid)?;
        if texts.is_empty() {
            return Err(Fail::Stale("matching content block unavailable".into()));
        }
        let folded = casefold(query);
        texts.iter().find(|t| casefold(t).contains(&folded)).cloned().unwrap_or_else(|| texts[0].clone())
    };
    Ok(excerpt(&hit.clone().with("text", OV::Str(text)), query))
}

/// UTF-8 size of `json.dumps(result, ensure_ascii=False)`; Err when Python's encode would fail.
pub fn output_size(result: &Obj) -> Result<usize, Fail> {
    let text = ojson::dumps_raw(&OV::Obj(result.clone()));
    match utf8_encode_error(&text) {
        Some(msg) => Err(Fail::Invalid(msg)),
        None => Ok(text.len()),
    }
}

fn pagination_live(result: &Obj) -> bool {
    matches!(result.get("pagination").and_then(OV::as_obj).and_then(|p| p.str_of("status")), Some("requested" | "ready"))
}

/// `paginate(hits, args, key, result)`.
pub fn paginate(hits: &[Obj], limit: i64, offset: i128, key: &Obj, result: &mut Obj) {
    let total = hits.len() as i128;
    let start = offset.clamp(0, total) as usize;
    let end = (offset + limit as i128).clamp(0, total) as usize;
    result.set("results", OV::Arr(hits[start..end.max(start)].iter().cloned().map(OV::Obj).collect()));
    result.set("total_matches", OV::Int(total as i64));
    result.set("truncated", OV::Bool(offset + (limit as i128) < total));
    result.set("next_cursor", OV::Null);
    result.set("offset", crate::inventory::int_ov(offset));
    if result.get("truncated").is_some_and(OV::truthy) && pagination_live(result) {
        result.set("next_cursor", OV::Str(encode_cursor(key, offset + limit as i128)));
    }
}

fn update_cursor(key: &Obj, result: &mut Obj) {
    let offset = match result.get("offset") {
        Some(OV::Int(i)) => *i as i128,
        Some(OV::Big(b)) => b.parse::<i128>().unwrap_or(i128::MAX),
        _ => 0,
    };
    let count = result.get("results").and_then(OV::as_arr).map_or(0, Vec::len) as i128;
    let following = offset + count;
    let total = result.int_of("total_matches").unwrap_or(0) as i128;
    let truncated = following < total;
    result.set("truncated", OV::Bool(truncated));
    let next = if truncated && pagination_live(result) { OV::Str(encode_cursor(key, following)) } else { OV::Null };
    result.set("next_cursor", next);
}

/// `fit_output(args, key, result)`: false when even one result does not fit.
pub fn fit_output(max_output_bytes: i64, key: &Obj, result: &mut Obj) -> Result<bool, Fail> {
    update_cursor(key, result);
    while output_size(result)? as i64 + 1 > max_output_bytes {
        let count = result.get("results").and_then(OV::as_arr).map_or(0, Vec::len);
        if count > 1 {
            if let Some(OV::Arr(items)) = result.get_mut("results") {
                items.pop();
            }
            update_cursor(key, result);
        } else {
            result.set("status", OV::str("output_budget_too_small"));
            result.set("results", OV::Arr(Vec::new()));
            update_cursor(key, result);
            return Ok(false);
        }
    }
    Ok(true)
}
