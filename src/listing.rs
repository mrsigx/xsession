//! 2.3.0 `list_row` (bounded head/tail inspection) and `resolve` (identity groups and
//! header-observed resume relations).

use crate::fields::{row_blocks, Role};
use crate::hash::sha256_hex;
use crate::inventory::problem;
use crate::ojson::{self, Obj, OV};
use crate::pathx;
use crate::pyerr::{os_text, Fail, PyOsError};
use crate::pyio;
use crate::pyobj::{casefold_of, cmp_f64, py_cmp, py_eq, try_sort_by};
use crate::session;
use crate::timeparse::time_key;
use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};

pub fn public_row(row: &Obj) -> Obj {
    Obj(row.0.iter().filter(|(k, _)| !k.starts_with('_')).cloned().collect())
}

/// `bytes.splitlines()`: \n, \r\n and \r end lines.
fn splitlines(raw: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut i = 0;
    while i < raw.len() {
        match raw[i] {
            b'\n' => {
                out.push(&raw[start..i]);
                start = i + 1;
            }
            b'\r' => {
                out.push(&raw[start..i]);
                if raw.get(i + 1) == Some(&b'\n') {
                    i += 1;
                }
                start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    if start < raw.len() {
        out.push(&raw[start..]);
    }
    out
}

/// One window line: title, latest timestamp and first user text, as 2.3.0 reads them.
fn inspect_line(line: &[u8], antigravity: bool, title: &mut OV, last: &mut OV, preview: &mut Option<String>) {
    // serde_json first; anything it rejects goes to the exact parser (Python accepts more).
    let value: OV = match serde_json::from_slice::<serde_json::Value>(line) {
        Ok(v) => {
            if !v.is_object() {
                return;
            }
            return inspect_event(&v, antigravity, title, last, preview);
        }
        Err(_) => match ojson::loads_bytes(line) {
            Ok(v) => v,
            Err(_) => return,
        },
    };
    if matches!(value, OV::Obj(_)) {
        inspect_event(&value, antigravity, title, last, preview);
    }
}

fn inspect_event<J: crate::fields::Json + ToOv>(e: &J, antigravity: bool, title: &mut OV, last: &mut OV, preview: &mut Option<String>) {
    if e.get("type").and_then(|t| t.as_str()) == Some("custom-title") {
        if let Some(t) = e.get("customTitle").and_then(|t| t.as_str()) {
            *title = OV::str(t);
        }
    }
    let mut fields: Vec<(bool, String)> = Vec::new();
    let mut any = false;
    if row_blocks(antigravity, e, false, &mut |role, text, _| {
        any = true;
        let is_user = match role {
            Role::Name(n) => n == "user",
            Role::Value(v) => v.as_str() == Some("user"),
        };
        fields.push((is_user, text.to_string()));
    })
    .is_err()
    {
        return;
    }
    let mut stamp = e.get(if antigravity { "created_at" } else { "timestamp" }).map(ToOv::to_ov).unwrap_or(OV::Null);
    if !stamp.truthy() && !antigravity {
        if let Some(payload) = e.get("payload").filter(|p| p.is_obj()) {
            stamp = payload.get("timestamp").map(ToOv::to_ov).unwrap_or(OV::Null);
        }
    }
    if any && time_key(Some(&stamp)) > time_key(Some(last)) {
        *last = stamp;
    }
    if preview.is_none() {
        if let Some((_, text)) = fields.iter().find(|(user, _)| *user) {
            *preview = Some(text.chars().take(160).collect());
        }
    }
}

/// Exact value of a decoded field (only strings reach the output from list_row).
pub trait ToOv {
    fn to_ov(&self) -> OV;
}

impl ToOv for serde_json::Value {
    fn to_ov(&self) -> OV {
        ojson::from_serde(self)
    }
}

impl ToOv for OV {
    fn to_ov(&self) -> OV {
        self.clone()
    }
}

fn read_window<R: Read + Seek>(f: &mut R, start: u64, length: u64) -> std::io::Result<Vec<u8>> {
    f.seek(SeekFrom::Start(start))?;
    let mut raw = Vec::new();
    f.take(length).read_to_end(&mut raw)?;
    Ok(raw)
}

/// `list_row(row, coverage)`.
pub fn list_row(row: &mut Obj, cov: &mut Obj) -> Obj {
    const WINDOW: u64 = 65536;
    let size = row.int_of("bytes").unwrap_or(0).max(0) as u64;
    let mut ranges = vec![(0u64, size.min(WINDOW))];
    if size > WINDOW {
        let start = WINDOW.max(size - WINDOW);
        ranges.push((start, size - start));
    }
    let p = pathx::norm(row.str_of("path").unwrap_or(""));
    let mut preview: Option<String> = None;
    let mut last = OV::Null;
    let mut title = row.get("title").cloned().unwrap_or(OV::Null);
    let compressed = session::is_compressed(&p);
    let antigravity = row.str_of("agent") == Some("antigravity");
    let outcome = (|| -> Result<(OV, Option<pyio::Stat>), PyOsError> {
        let mut hashes = Vec::new();
        let stored = if compressed { Some(pyio::stat(&p)?) } else { None };
        let mut f = session::open(&p)?;
        for &(start, length) in &ranges {
            // The byte before the window, read first so a compressed stream only seeks forward.
            let preceding = if start > 0 { Some(read_window(&mut f, start - 1, 1).map_err(|e| pyio::read_err(&e))?) } else { None };
            let mut raw = read_window(&mut f, start, length).map_err(|e| pyio::read_err(&e))?;
            hashes.push(OV::Arr(vec![OV::Int(start as i64), OV::Int(length as i64), OV::Str(sha256_hex(&raw))]));
            if raw.len() as u64 != length {
                return Err(os_text("captured list window was truncated"));
            }
            if let Some(before) = preceding {
                if before != b"\n" {
                    raw = match raw.iter().position(|b| *b == b'\n') {
                        Some(i) => raw[i + 1..].to_vec(),
                        None => Vec::new(),
                    };
                }
            }
            if !raw.is_empty() && raw.last() != Some(&b'\n') {
                raw = match raw.iter().rposition(|b| *b == b'\n') {
                    Some(i) => raw[..i].to_vec(),
                    None => Vec::new(),
                };
            }
            for line in splitlines(&raw) {
                inspect_line(line, antigravity, &mut title, &mut last, &mut preview);
            }
        }
        if compressed {
            // Past the tail window to the end of the stream: a corrupt final block, checksum or
            // later frame is met here by every reader alike.
            let mut probe = [0u8; 1];
            f.read(&mut probe).map_err(|e| pyio::read_err(&e))?;
        }
        Ok((OV::Arr(hashes), stored))
    })();
    match outcome {
        Ok((hashes, stored)) => {
            let check = (|| -> Result<(), PyOsError> {
                let previous = row.get("_list_guards").filter(|g| g.truthy()).cloned();
                match previous {
                    Some(prev) if !py_eq(&prev, &hashes) => {
                        cov.set("snapshot_invalid", OV::Bool(true));
                        problem(cov, "snapshot_list_window_changed", &p, "");
                    }
                    _ => {
                        let unchanged = match &stored {
                            Some(stored) => session::stored_unchanged(&p, stored)?,
                            None => pyio::guards_match(&p, Some(&hashes))?,
                        };
                        if !unchanged {
                            problem(cov, "changed_during_list", &p, "");
                        }
                    }
                }
                Ok(())
            })();
            match check {
                Ok(()) => row.set("_list_guards", hashes),
                Err(e) => {
                    if row.get("_list_guards").is_some_and(OV::truthy) {
                        cov.set("snapshot_invalid", OV::Bool(true));
                    }
                    problem(cov, "read_error", &p, &e.text);
                }
            }
        }
        Err(e) => {
            if row.get("_list_guards").is_some_and(OV::truthy) {
                cov.set("snapshot_invalid", OV::Bool(true));
            }
            problem(cov, "read_error", &p, &e.text);
            if e.text == session::UNDECODABLE {
                preview = None;
                last = OV::Null;
                title = row.get("title").cloned().unwrap_or(OV::Null);
            }
        }
    }
    let basis = if last.truthy() { "latest message observed in bounded head/tail" } else { "unknown" };
    let inspected: u64 = ranges.iter().map(|r| r.1).sum();
    public_row(row)
        .with("timestamp", last)
        .with("timestamp_basis", OV::str(basis))
        .with("preview", preview.map_or(OV::Null, OV::Str))
        .with("title", title)
        .with("inspected_bytes", OV::Int(inspected as i64))
}

/// `[list_row(row, coverage) for row in rows]` on worker threads. Each row reports into its own
/// coverage part; parts merge in row order, so the result equals a serial pass.
pub fn list_rows(rows: &mut [Obj], cov: &mut Obj, jobs: usize) -> Vec<Obj> {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;
    type Slot = Mutex<Option<(Obj, Obj, Obj)>>;
    let fresh_part = || Obj::new().with("complete", OV::Bool(true)).with("errors", OV::Arr(Vec::new()));
    let slots: Vec<Slot> = (0..rows.len()).map(|_| Mutex::new(None)).collect();
    let next = AtomicUsize::new(0);
    let shared: &[Obj] = rows;
    std::thread::scope(|scope| {
        for _ in 0..jobs.clamp(1, shared.len().max(1)) {
            scope.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                if i >= shared.len() {
                    break;
                }
                let mut row = shared[i].clone();
                let mut part = fresh_part();
                let listed = list_row(&mut row, &mut part);
                *slots[i].lock().unwrap() = Some((row, listed, part));
            });
        }
    });
    let mut out = Vec::with_capacity(rows.len());
    for (i, slot) in slots.into_iter().enumerate() {
        let (row, listed, part) = slot.into_inner().unwrap().expect("every row is listed");
        rows[i] = row;
        if !part.get("complete").is_some_and(OV::truthy) {
            cov.set("complete", OV::Bool(false));
        }
        if let Some(OV::Arr(errors)) = part.get("errors") {
            if !errors.is_empty() {
                match cov.get_mut("errors") {
                    Some(OV::Arr(target)) => target.extend(errors.iter().cloned()),
                    _ => cov.set("errors", OV::Arr(errors.clone())),
                }
            }
        }
        if part.get("snapshot_invalid").is_some_and(OV::truthy) {
            cov.set("snapshot_invalid", OV::Bool(true));
        }
        out.push(listed);
    }
    out
}

/// `hits.sort(key=lambda r: (time_key(r['timestamp']), r['id'], r['path']), reverse=True)`.
pub fn sort_listed(hits: &mut Vec<Obj>) -> Result<(), Fail> {
    let mut keyed: Vec<(f64, OV, OV, Obj)> = hits
        .drain(..)
        .map(|h| (time_key(h.get("timestamp")), h.get("id").cloned().unwrap_or(OV::Null), h.get("path").cloned().unwrap_or(OV::Null), h))
        .collect();
    try_sort_by(&mut keyed, |a, b| {
        if a.0 != b.0 {
            return Ok(cmp_f64(b.0, a.0));
        }
        if !py_eq(&a.1, &b.1) {
            return py_cmp(&b.1, &a.1);
        }
        if !py_eq(&a.2, &b.2) {
            return py_cmp(&b.2, &a.2);
        }
        Ok(std::cmp::Ordering::Equal)
    })
    .map_err(Fail::Invalid)?;
    *hits = keyed.into_iter().map(|k| k.3).collect();
    Ok(())
}

/// `resolve(rows, query)`.
pub fn resolve(rows: &[Obj], query: &str) -> Result<Vec<Obj>, Fail> {
    let mut groups: Vec<Obj> = Vec::new();
    let mut index: HashMap<(String, String), usize> = HashMap::new();
    let new_group = |id: OV, agent: &str, kind: OV| {
        Obj::new().with("id", id).with("agent", OV::str(agent)).with("kind", kind)
            .with("segments", OV::Arr(Vec::new())).with("related_segments", OV::Arr(Vec::new()))
    };
    for row in rows {
        let agent = row.str_of("agent").unwrap_or("").to_string();
        let id = row.get("id").cloned().unwrap_or(OV::Null);
        let key = (agent.clone(), casefold_of(&id)?);
        let at = *index.entry(key).or_insert_with(|| {
            groups.push(new_group(id.clone(), &agent, row.get("kind").cloned().unwrap_or(OV::Null)));
            groups.len() - 1
        });
        let group = &mut groups[at];
        group.set("kind", row.get("kind").cloned().unwrap_or(OV::Null));
        if let Some(OV::Arr(segments)) = group.get_mut("segments") {
            segments.push(OV::Obj(row.clone()));
        }
        if let Some(ancestor) = row.get("resumed_from").filter(|v| v.truthy()) {
            let key = (agent.clone(), casefold_of(ancestor)?);
            let at = *index.entry(key).or_insert_with(|| {
                groups.push(new_group(ancestor.clone(), &agent, OV::str("inherited_history")));
                groups.len() - 1
            });
            let related = public_row(row)
                .with("relation", OV::str("resumed_history"))
                .with("relation_basis", OV::str("recorded header sessionId differs from container UUID"))
                .with("read_identity", id.clone());
            if let Some(OV::Arr(list)) = groups[at].get_mut("related_segments") {
                list.push(OV::Obj(related));
            }
        }
    }
    let folded_query = crate::pytext::casefold(query);
    let folded_ids: Vec<String> = groups.iter().map(|g| casefold_of(g.get("id").unwrap_or(&OV::Null))).collect::<Result<_, _>>()?;
    let mut selected: Vec<usize> = (0..groups.len()).filter(|&i| folded_ids[i] == folded_query).collect();
    if selected.is_empty() {
        selected = (0..groups.len()).filter(|&i| folded_ids[i].starts_with(&folded_query)).collect();
    }
    for group in groups.iter_mut() {
        for (key, by_time) in [("segments", true), ("related_segments", false)] {
            if let Some(OV::Arr(list)) = group.get_mut(key) {
                let mut items: Vec<OV> = std::mem::take(list);
                try_sort_by(&mut items, |a, b| {
                    let (a, b) = (a.as_obj().unwrap(), b.as_obj().unwrap());
                    if by_time {
                        let (ta, tb) = (time_key(a.get("timestamp")), time_key(b.get("timestamp")));
                        if ta != tb {
                            return Ok(cmp_f64(ta, tb));
                        }
                    } else {
                        let (ia, ib) = (a.get("id").unwrap_or(&OV::Null), b.get("id").unwrap_or(&OV::Null));
                        if !py_eq(ia, ib) {
                            return py_cmp(ia, ib);
                        }
                    }
                    py_cmp(a.get("path").unwrap_or(&OV::Null), b.get("path").unwrap_or(&OV::Null))
                })
                .map_err(Fail::Invalid)?;
                *list = items;
            }
        }
    }
    let mut chosen: Vec<Obj> = selected.into_iter().map(|i| groups[i].clone()).collect();
    try_sort_by(&mut chosen, |a, b| {
        let (aa, ba) = (a.get("agent").unwrap_or(&OV::Null), b.get("agent").unwrap_or(&OV::Null));
        if !py_eq(aa, ba) {
            return py_cmp(aa, ba);
        }
        py_cmp(a.get("id").unwrap_or(&OV::Null), b.get("id").unwrap_or(&OV::Null))
    })
    .map_err(Fail::Invalid)?;
    Ok(chosen)
}
