//! 2.3.0 `scan_row`, `scan_rows` (threads instead of worker processes; results merge in
//! inventory order, as before) and `search` (resume relations, title hits, ranking, sort,
//! import-copy collapse).

use crate::inventory::is_complete;
use crate::ojson::{Obj, OV};
use crate::pyerr::{py_int, Fail};
use crate::pyobj::{casefold_of, cmp_f64, py_cmp, py_eq, try_sort_by, PyDict};
use crate::pytext::{casefold, excerpt};
use crate::scan::{records, SelfIds};
use crate::timeparse::time_key;
use std::cmp::Ordering;
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
use std::sync::Mutex;

const PARALLEL_MIN_BYTES: i128 = 256 * 1024 * 1024;

fn new_part() -> Obj {
    Obj::new()
        .with("complete", OV::Bool(true))
        .with("errors", OV::Arr(Vec::new()))
        .with("notes", OV::Arr(Vec::new()))
        .with("malformed_records", OV::Int(0))
        .with("unfinished_records", OV::Int(0))
        .with("changed_files", OV::Int(0))
        .with("unsupported_records", OV::Int(0))
}

/// `scan_row`: hits (excerpted, at most max_matches + 1) and this file's coverage part.
fn scan_row(row: &mut Obj, needle: &str, query: &str, strong: bool, max_matches: usize, ids: &SelfIds) -> Result<(Vec<Obj>, Obj), Fail> {
    let mut part = new_part();
    let mark = crate::session::mark(&part);
    let mut hits = Vec::new();
    records(row, &mut part, Some(needle), strong, ids, &mut |hit| {
        hits.push(excerpt(&hit, query));
        hits.len() <= max_matches
    })?;
    let path = crate::pathx::norm(row.str_of("path").unwrap_or(""));
    if crate::session::undecodable(&part, &path, 0) {
        hits.clear();
        crate::session::undecodable_only(&mut part, &mark, &path);
    }
    Ok((hits, part))
}

fn extend(cov: &mut Obj, key: &str, items: Option<&OV>) {
    let Some(OV::Arr(items)) = items else { return };
    match cov.get_mut(key) {
        Some(OV::Arr(target)) => target.extend(items.iter().cloned()),
        _ => cov.set(key, OV::Arr(items.clone())),
    }
}

/// `scan_rows`: per-row hits, and the worker count used.
#[allow(clippy::too_many_arguments)]
fn scan_rows(rows: &mut [Obj], needle: &str, query: &str, strong: bool, max_matches: usize, jobs: usize, cov: &mut Obj,
             ids: &SelfIds) -> Result<(Vec<Vec<Obj>>, usize), Fail> {
    let minimum = match crate::inventory::env("XSESSION_PARALLEL_MIN_BYTES") {
        Some(v) => py_int(&v).map_err(Fail::Invalid)?,
        None => PARALLEL_MIN_BYTES,
    };
    let total: i128 = rows.iter().map(|r| r.int_of("bytes").unwrap_or(0) as i128).sum();
    let parallel = jobs > 1 && rows.len() > 1 && total >= minimum;
    let mut results: Vec<Option<Result<(Obj, Vec<Obj>, Obj), Fail>>> = (0..rows.len()).map(|_| None).collect();
    if parallel {
        // Largest files first; each thread takes the next file.
        let mut order: Vec<usize> = (0..rows.len()).collect();
        order.sort_by_key(|&i| std::cmp::Reverse(rows[i].int_of("bytes").unwrap_or(0)));
        let next = AtomicUsize::new(0);
        let slots: Vec<Mutex<Option<Result<(Obj, Vec<Obj>, Obj), Fail>>>> = (0..rows.len()).map(|_| Mutex::new(None)).collect();
        let shared: &[Obj] = rows;
        std::thread::scope(|scope| {
            for _ in 0..jobs.min(shared.len()) {
                scope.spawn(|| loop {
                    let k = next.fetch_add(1, AtomicOrdering::Relaxed);
                    if k >= order.len() {
                        break;
                    }
                    let i = order[k];
                    let mut row = shared[i].clone();
                    let outcome = scan_row(&mut row, needle, query, strong, max_matches, ids).map(|(hits, part)| (row, hits, part));
                    *slots[i].lock().unwrap() = Some(outcome);
                });
            }
        });
        for (i, slot) in slots.into_iter().enumerate() {
            results[i] = slot.into_inner().unwrap();
        }
    }
    let mut per_row = Vec::with_capacity(rows.len());
    for (i, row) in rows.iter_mut().enumerate() {
        let (updated, hits, part) = match results[i].take() {
            Some(r) => r?,
            None => {
                let mut copy = row.clone();
                let (hits, part) = scan_row(&mut copy, needle, query, strong, max_matches, ids)?;
                (copy, hits, part)
            }
        };
        *row = updated;
        if !is_complete(&part) {
            cov.set("complete", OV::Bool(false));
        }
        extend(cov, "errors", part.get("errors"));
        extend(cov, "notes", part.get("notes"));
        for key in ["malformed_records", "unfinished_records", "changed_files", "unsupported_records"] {
            crate::inventory::bump(cov, key, part.int_of(key).unwrap_or(0));
        }
        if part.get("snapshot_invalid").is_some_and(OV::truthy) {
            cov.set("snapshot_invalid", OV::Bool(true));
        }
        per_row.push(hits);
    }
    Ok((per_row, if parallel { jobs } else { 1 }))
}

fn rank(hit: &Obj) -> i64 {
    let mut provenance = hit.str_of("provenance").unwrap_or("");
    if provenance == "resumed_history" && !hit.get("original_available").is_some_and(OV::truthy) {
        provenance = hit.str_of("content_provenance").unwrap_or("");
    }
    match provenance {
        "user_message" => 0,
        "quoted_or_pasted" => 1,
        "assistant" => 2,
        "tool_input" | "tool_output" => 3,
        "document" | "project_instructions" | "project_source" => 4,
        "title_metadata" => 5,
        "delegated_prompt" => 6,
        "compaction_summary" => 7,
        "injected_context" => 8,
        "import_copy" | "resumed_history" => 9,
        _ => 10,
    }
}

/// `str(value)` for a block identity.
fn py_str(v: &OV) -> String {
    match v {
        OV::Str(s) => s.clone(),
        OV::Null => "None".into(),
        OV::Bool(b) => if *b { "True".into() } else { "False".into() },
        OV::Float(f) => crate::ojson::py_float_repr(*f, false),
        other => crate::ojson::dumps(other, false),
    }
}

struct Key {
    rank: i64,
    time: f64,
    id: OV,
    path: OV,
    line: i64,
    block: String,
}

/// Python tuple ordering: the first unequal element decides.
fn cmp_keys(a: &Key, b: &Key) -> Result<Ordering, String> {
    if a.rank != b.rank {
        return Ok(a.rank.cmp(&b.rank));
    }
    if a.time != b.time {
        return Ok(cmp_f64(a.time, b.time));
    }
    if !py_eq(&a.id, &b.id) {
        return py_cmp(&a.id, &b.id);
    }
    if !py_eq(&a.path, &b.path) {
        return py_cmp(&a.path, &b.path);
    }
    if a.line != b.line {
        return Ok(a.line.cmp(&b.line));
    }
    Ok(a.block.cmp(&b.block))
}

fn match_limit(max_matches: usize) -> Fail {
    Fail::MatchLimit(format!("more than {} matches; narrow the source or query", max_matches))
}

/// `search(rows, query, coverage, titles, max_matches, jobs, strong)`.
#[allow(clippy::too_many_arguments)]
pub fn search(rows: &mut [Obj], query: &str, cov: &mut Obj, titles: &PyDict<Obj>, max_matches: usize, jobs: usize,
              strong: bool, ids: &SelfIds) -> Result<Vec<Obj>, Fail> {
    let needle = casefold(query);
    let (per_row, used) = scan_rows(rows, &needle, query, strong, max_matches, jobs, cov, ids)?;
    cov.set("scan", OV::Obj(Obj::new().with("jobs", OV::Int(used as i64)).with("prefix_check", OV::str(if strong { "sha256" } else { "crc32" }))));
    let mut title_done: PyDict<()> = PyDict::new();
    let mut available: std::collections::HashSet<String> = std::collections::HashSet::new();
    for r in rows.iter() {
        if r.str_of("agent") == Some("claude") {
            available.insert(casefold_of(r.get("id").unwrap_or(&OV::Null))?);
        }
    }
    let mut hits: Vec<Obj> = Vec::new();
    for (row, row_hits) in rows.iter().zip(per_row) {
        for mut hit in row_hits {
            if hit.str_of("provenance") == Some("resumed_history") {
                let truncated = hit.remove("text_truncated").unwrap_or(OV::Null);
                let start = hit.remove("text_start_character").unwrap_or(OV::Null);
                let session = casefold_of(hit.get("record_session_id").unwrap_or(&OV::Null))?;
                hit.set("original_available", OV::Bool(available.contains(&session)));
                hit.set("availability_basis", OV::str("container identity in configured source inventory; absence does not establish deletion"));
                hit.set("text_truncated", truncated);
                hit.set("text_start_character", start);
            }
            hits.push(hit);
            if hits.len() > max_matches {
                return Err(match_limit(max_matches));
            }
        }
        let id = row.get("id").cloned().unwrap_or(OV::Null);
        if let Some(title) = titles.get(&id) {
            if !title.0.is_empty() && !title_done.contains(&id) {
                let text = title.get("text").unwrap_or(&OV::Null);
                if casefold_of(text)?.contains(&needle) {
                    title_done.insert(id.clone(), ());
                    let mut hit = row.clone();
                    for (k, v) in &title.0 {
                        hit.set(k, v.clone());
                    }
                    hit.set("role", OV::str("metadata"));
                    hit.set("provenance", OV::str("title_metadata"));
                    hit.set("confidence", OV::str("metadata"));
                    hit.set("timestamp_basis", OV::str("unknown"));
                    hit.set("block", OV::Int(0));
                    hits.push(hit);
                    if hits.len() > max_matches {
                        return Err(match_limit(max_matches));
                    }
                }
            }
        }
    }
    let mut keyed: Vec<(Key, Obj)> = hits
        .into_iter()
        .map(|h| {
            let key = Key {
                rank: rank(&h),
                time: time_key(h.get("timestamp")),
                id: h.get("id").cloned().unwrap_or(OV::Null),
                path: h.get("path").cloned().unwrap_or(OV::Null),
                line: h.int_of("line").unwrap_or(0),
                block: py_str(h.get("block").unwrap_or(&OV::Null)),
            };
            (key, h)
        })
        .collect();
    try_sort_by(&mut keyed, |a, b| cmp_keys(&a.0, &b.0)).map_err(Fail::Invalid)?;
    let mut hits: Vec<Obj> = keyed.into_iter().map(|(_, h)| h).collect();
    // Collapse only byte-identical extracted text tied by the import registry.
    let truncated = |h: &Obj| h.get("text_truncated").is_some_and(OV::truthy);
    let text_of = |h: &Obj| h.str_of("text").unwrap_or("").to_string();
    let mut by_string: HashMap<(String, String), usize> = HashMap::new();
    let mut by_other: Vec<(OV, String, usize)> = Vec::new();
    for (i, h) in hits.iter().enumerate() {
        if h.str_of("agent") == Some("claude") && !truncated(h) {
            match h.get("id") {
                Some(OV::Str(id)) => {
                    by_string.insert((id.clone(), text_of(h)), i);
                }
                Some(other) => {
                    let text = text_of(h);
                    match by_other.iter_mut().find(|(k, t, _)| py_eq(k, other) && *t == text) {
                        Some(entry) => entry.2 = i,
                        None => by_other.push((other.clone(), text, i)),
                    }
                }
                None => {}
            }
        }
    }
    let mut kept: Vec<usize> = Vec::new();
    let mut copies: Vec<(usize, Obj)> = Vec::new();
    for (i, h) in hits.iter().enumerate() {
        let original = if truncated(h) {
            None
        } else {
            let text = text_of(h);
            match h.get("original_id") {
                Some(OV::Str(id)) => by_string.get(&(id.clone(), text)).copied(),
                Some(other) => by_other.iter().find(|(k, t, _)| py_eq(k, other) && *t == text).map(|e| e.2),
                None => None,
            }
        };
        match original {
            Some(o) if h.str_of("provenance") == Some("import_copy") => {
                let mut copy = Obj::new();
                for k in ["id", "path", "line", "record_sha256"] {
                    copy.set(k, h.get(k).cloned().unwrap_or(OV::Null));
                }
                copies.push((o, copy));
            }
            _ => kept.push(i),
        }
    }
    for (o, copy) in copies {
        match hits[o].get_mut("copies") {
            Some(OV::Arr(list)) => list.push(OV::Obj(copy)),
            _ => hits[o].set("copies", OV::Arr(vec![OV::Obj(copy)])),
        }
    }
    let mut slots: Vec<Option<Obj>> = hits.into_iter().map(Some).collect();
    Ok(kept.into_iter().map(|i| slots[i].take().unwrap()).collect())
}
