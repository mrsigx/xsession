//! 2.3.0 `main` and `emit`: validation, the find/list paging flow, resolve/path/current/read,
//! and how each exception class ends the run.

use crate::args::Args;
use crate::inventory::{self, config_for, env, initial_coverage, inventory, is_complete, problem, VERSION};
use crate::listing::{list_rows, resolve, sort_listed};
use crate::metadata::read_metadata;
use crate::ojson::{self, Obj, OV, SURROGATE_BASE};
use crate::pathx;
use crate::pyerr::{py_int, Fail};
use crate::pyobj::PyDict;
use crate::pytext::{casefold, strip};
use crate::scan::{context_for, SelfIds};
use crate::search::search;
use crate::snapshot::{self, PageArgs, Snapshot};
use std::io::Write;
use std::time::Instant;

/// Python's text streams: `\n` becomes the platform line end, and lone surrogates are written
/// with `errors='backslashreplace'`.
pub struct Out {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

fn encode_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    for c in s.chars() {
        let code = c as u32;
        if code >= SURROGATE_BASE {
            out.push_str(&format!("\\u{:04x}", 0xD800 + (code - SURROGATE_BASE)));
        } else if c == '\n' && cfg!(windows) {
            out.push_str("\r\n");
        } else {
            out.push(c);
        }
    }
    out
}

impl Out {
    pub fn new() -> Out {
        Out { stdout: Vec::new(), stderr: Vec::new() }
    }
    pub fn print(&mut self, s: &str) {
        self.stdout.extend_from_slice(encode_text(&format!("{}\n", s)).as_bytes());
    }
    pub fn eprint(&mut self, s: &str) {
        self.stderr.extend_from_slice(encode_text(&format!("{}\n", s)).as_bytes());
    }
    pub fn flush(&mut self) {
        let _ = std::io::stdout().write_all(&self.stdout);
        let _ = std::io::stdout().flush();
        let _ = std::io::stderr().write_all(&self.stderr);
        let _ = std::io::stderr().flush();
        self.stdout.clear();
        self.stderr.clear();
    }
}

fn cpu_count() -> i128 {
    if let Some(v) = env("PYTHON_CPU_COUNT") {
        if let Ok(n) = v.trim().parse::<i128>() {
            if n > 0 {
                return n;
            }
        }
    }
    #[cfg(windows)]
    {
        extern "system" {
            fn GetActiveProcessorCount(group: u16) -> u32;
        }
        // SAFETY: plain query; 0xFFFF is ALL_PROCESSOR_GROUPS.
        let n = unsafe { GetActiveProcessorCount(0xFFFF) };
        n.max(1) as i128
    }
    #[cfg(not(windows))]
    {
        extern "C" {
            fn sysconf(name: i32) -> i64;
        }
        // SAFETY: plain query; 84 is _SC_NPROCESSORS_ONLN on Linux.
        let n = unsafe { sysconf(84) };
        if n > 0 {
            n as i128
        } else {
            1
        }
    }
}

/// `uuid.uuid4().hex`.
fn uuid4_hex() -> String {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let mut bytes = [0u8; 16];
    for (i, chunk) in bytes.chunks_mut(8).enumerate() {
        let mut h = RandomState::new().build_hasher();
        h.write_u64(i as u64);
        h.write_u128(nanos);
        h.write_u32(std::process::id());
        chunk.copy_from_slice(&h.finish().to_le_bytes());
    }
    bytes[6] = (bytes[6] & 0x0F) | 0x40;
    bytes[8] = (bytes[8] & 0x3F) | 0x80;
    crate::hash::hex(&bytes)
}

/// `native_locator(value)`.
fn native_locator(value: &str) -> Result<Option<Obj>, Fail> {
    if !value.starts_with("codex://") {
        return Ok(None);
    }
    let invalid = || Fail::Invalid("expected codex://threads/<full UUID>?hostId=<host>".into());
    // urlsplit: tab/CR/LF are removed anywhere; the scheme is known to be codex.
    let url: String = value.chars().filter(|c| !matches!(c, '\t' | '\r' | '\n')).collect();
    let rest = &url["codex:".len()..];
    let (netloc, after) = match rest.strip_prefix("//") {
        Some(r) => {
            let end = r.find(['/', '?', '#']).unwrap_or(r.len());
            (&r[..end], &r[end..])
        }
        None => ("", rest),
    };
    if netloc.contains('[') != netloc.contains(']') {
        return Err(Fail::Invalid("Invalid IPv6 URL".into()));
    }
    let after = after.split('#').next().unwrap_or("");
    let (path, query) = match after.split_once('?') {
        Some((p, q)) => (p, q),
        None => (after, ""),
    };
    let ident = path.trim_matches('/');
    if netloc != "threads" || !inventory::is_uuid(ident) {
        return Err(invalid());
    }
    let mut host = OV::Null;
    for pair in query.split('&') {
        let Some((name, val)) = pair.split_once('=') else { continue };
        if val.is_empty() {
            continue;
        }
        if unquote_plus(name) == "hostId" {
            host = OV::Str(unquote_plus(val));
            break;
        }
    }
    Ok(Some(
        Obj::new()
            .with("thread_id", OV::str(ident))
            .with("host_id", host)
            .with("tool", OV::str("read_thread"))
            .with("instruction", OV::str("Use the supported native app tool; page until hasMore=false. Preserve output truncation flags.")),
    ))
}

/// `urllib.parse.unquote(s.replace('+', ' '))` (UTF-8, errors='replace').
fn unquote_plus(s: &str) -> String {
    let s = s.replace('+', " ");
    let b = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() + 0 && b.get(i + 1).is_some_and(u8::is_ascii_hexdigit) && b.get(i + 2).is_some_and(u8::is_ascii_hexdigit) {
            out.push(u8::from_str_radix(&s[i + 1..i + 3], 16).unwrap());
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn round3(x: f64) -> f64 {
    format!("{:.3}", x).parse().unwrap_or(x)
}

/// `emit(args, result, code)`.
fn emit(a: &Args, result: &Obj, code: i32, out: &mut Out) -> Result<i32, Fail> {
    if a.json {
        out.print(&ojson::dumps(&OV::Obj(result.clone()), false));
        return Ok(code);
    }
    let results: Vec<OV> = result.get("results").and_then(OV::as_arr).cloned().unwrap_or_default();
    if matches!(a.command.as_str(), "path" | "current") {
        if code == 0 || (code == 4 && result.get("exact_path_available").is_some_and(OV::truthy)) {
            let group = results.first().and_then(OV::as_obj).cloned().unwrap_or_default();
            let segment = group.get("segments").and_then(OV::as_arr).and_then(|s| s.first()).and_then(OV::as_obj).cloned().unwrap_or_default();
            let original = segment.str_of("path").unwrap_or("").to_string();
            let mut path = original.clone();
            let style = a.path_format.clone().or_else(|| env("XSESSION_PATH_FORMAT")).unwrap_or_else(|| "native".into());
            let b = path.as_bytes();
            let drive = b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':';
            if (style == "msys" || style == "wsl") && drive {
                let rest: String = path.chars().skip(2).collect::<String>().replace('\\', "/");
                path = format!("{}{}{}", if style == "wsl" { "/mnt/" } else { "/" }, (b[0] as char).to_ascii_lowercase(), rest);
            } else if style == "windows" {
                let tail = path.strip_prefix("/mnt/").or_else(|| path.strip_prefix('/'));
                if let Some(t) = tail {
                    let tb = t.as_bytes();
                    if tb.len() >= 2 && tb[0].is_ascii_alphabetic() && tb[1] == b'/' {
                        let rest = t[2..].split('\n').next().unwrap_or("").replace('/', "\\");
                        path = format!("{}:\\{}", (tb[0] as char).to_ascii_uppercase(), rest);
                    }
                }
            }
            if a.legacy {
                let data = crate::session::read_all(&pathx::norm(&original))?;
                let lines = data.iter().filter(|c| **c == b'\n').count() + usize::from(!data.is_empty() && data.last() != Some(&b'\n'));
                out.print(&format!("session_id: {}", group.str_of("id").unwrap_or("")));
                out.print(&format!("transcript: {}", path));
                out.print(&format!("lines: {}", lines));
                out.print(&format!("size: {}", segment.get("bytes").map_or("None".into(), |v| ojson::dumps(v, false))));
                out.print(&format!("project: {}", pathx::name(&pathx::parent(&original))));
            } else {
                out.print(&path);
            }
        }
    } else {
        for row in &results {
            out.print(&ojson::dumps(row, false));
        }
    }
    if result.contains("pagination") {
        let mut p = Obj::new();
        for k in ["pagination", "truncated", "total_matches", "next_cursor"] {
            p.set(k, result.get(k).cloned().unwrap_or(OV::Null));
        }
        out.eprint(&ojson::dumps(&OV::Obj(p), false));
    }
    if matches!(code, 1 | 2 | 3 | 5 | 6) {
        out.eprint(result.str_of("status").unwrap_or(""));
        if code == 3 {
            for row in &results {
                let id = row.as_obj().and_then(|r| r.get("id")).cloned().unwrap_or(OV::Null);
                out.eprint(&match id {
                    OV::Str(s) => s,
                    other => ojson::dumps(&other, false),
                });
            }
        }
    }
    let coverage = Obj::new().with("coverage", result.get("coverage").cloned().unwrap_or(OV::Null));
    out.eprint(&ojson::dumps(&OV::Obj(coverage), false));
    Ok(code)
}

fn coverage_of(result: &mut Obj) -> &mut Obj {
    if !matches!(result.get("coverage"), Some(OV::Obj(_))) {
        result.set("coverage", OV::Obj(Obj::new()));
    }
    result.get_mut("coverage").and_then(OV::as_obj_mut).unwrap()
}

pub fn run(a: &Args, out: &mut Out) -> i32 {
    let started = Instant::now();
    let mut result = Obj::new();
    match body(a, out, started, &mut result) {
        Ok(code) => code,
        Err(Fail::MatchLimit(msg)) => {
            coverage_of(&mut result).set("complete", OV::Bool(false));
            result.set("status", OV::str("match_limit_exceeded"));
            result.set("results", OV::Arr(Vec::new()));
            result.set("error", OV::Str(msg));
            result.set("next_cursor", OV::Null);
            finish_emit(a, &result, 2, out)
        }
        Err(Fail::Stale(msg)) => {
            coverage_of(&mut result).set("complete", OV::Bool(false));
            result.set("status", OV::str("stale_cursor"));
            result.set("results", OV::Arr(Vec::new()));
            result.set("error", OV::Str(msg));
            finish_emit(a, &result, 3, out)
        }
        Err(other) => report(a, other, out),
    }
}

fn finish_emit(a: &Args, result: &Obj, code: i32, out: &mut Out) -> i32 {
    match emit(a, result, code, out) {
        Ok(code) => code,
        Err(e) => report(a, e, out),
    }
}

fn report(a: &Args, fail: Fail, out: &mut Out) -> i32 {
    let error = |kind: &str, detail: &str| {
        let body = Obj::new()
            .with("schema_version", OV::Int(1))
            .with("version", OV::str(VERSION))
            .with("status", OV::str("operational_error"))
            .with("results", OV::Arr(Vec::new()))
            .with("error", OV::Obj(Obj::new().with("type", OV::str(kind)).with("detail", OV::str(detail))));
        ojson::dumps(&OV::Obj(body), true)
    };
    match fail {
        Fail::Invalid(msg) => {
            out.eprint(&format!("xsession: invalid input/config: {}", msg));
            2
        }
        Fail::Os(e) => {
            if a.json {
                out.print(&error(e.class, &e.text));
            } else {
                out.eprint(&format!("xsession: {}", e.text));
            }
            6
        }
        Fail::Other(kind, msg) => {
            let short: String = msg.chars().take(500).collect();
            if a.json {
                out.print(&error(&kind, &short));
            } else {
                out.eprint(&format!("xsession: operational error: {}: {}", kind, short));
            }
            6
        }
        // Raised only inside the guarded body.
        Fail::MatchLimit(m) | Fail::Stale(m) => {
            out.eprint(&format!("xsession: {}", m));
            6
        }
    }
}

fn page_args<'a>(a: &'a Args) -> PageArgs<'a> {
    PageArgs {
        command: &a.command,
        snapshot_dir: a.snapshot_dir.as_deref(),
        cursor: a.cursor.as_deref(),
        context: a.context.unwrap_or(0) as i64,
        max_matches: a.max_matches.map(|m| m as i64),
        max_output_bytes: a.max_output_bytes.unwrap_or(4096) as i64,
    }
}

fn body(a: &Args, out: &mut Out, started: Instant, result: &mut Obj) -> Result<i32, Fail> {
    let mut query: Option<String> = a.query.clone();
    let current_ids = [env("CODEX_THREAD_ID"), env("CLAUDE_CODE_SESSION_ID")];
    if a.command == "current" {
        query = current_ids.iter().flatten().find(|v| !v.is_empty()).cloned();
    }
    let mut config = config_for(a.sources.as_deref())?;
    let source_error = || Fail::Invalid("each source needs string name/path and kind claude, codex, antigravity, document, or chatgpt_project".into());
    {
        let Some(cfg) = config.as_obj() else {
            return Err(Fail::Invalid("source configuration must be an object with a sources array".into()));
        };
        let Some(OV::Arr(sources)) = cfg.get("sources") else {
            return Err(Fail::Invalid("source configuration must be an object with a sources array".into()));
        };
        for s in sources {
            let Some(s) = s.as_obj() else { return Err(source_error()) };
            let kind_ok = matches!(s.str_of("kind"), Some("claude" | "codex" | "antigravity" | "document" | "chatgpt_project"));
            if !kind_ok || s.str_of("name").is_none() || s.str_of("path").is_none() {
                return Err(source_error());
            }
        }
    }
    if !a.source.is_empty() {
        let cfg = config.as_obj_mut().unwrap();
        let sources: Vec<OV> = cfg.get("sources").and_then(OV::as_arr).cloned().unwrap_or_default();
        let names: Vec<&str> = sources.iter().filter_map(|s| s.as_obj().and_then(|o| o.str_of("name"))).collect();
        if a.source.iter().any(|n| !names.contains(&n.as_str())) {
            return Err(Fail::Invalid("unknown source name".into()));
        }
        let kept: Vec<OV> = sources.iter().filter(|s| s.as_obj().and_then(|o| o.str_of("name")).is_some_and(|n| a.source.iter().any(|x| x == n))).cloned().collect();
        cfg.set("sources", OV::Arr(kept));
    }
    let cfg_obj = config.as_obj().unwrap().clone();
    let coverage = initial_coverage(&cfg_obj);
    *result = Obj::new()
        .with("schema_version", OV::Int(1))
        .with("version", OV::str(VERSION))
        .with("status", OV::str("no_match"))
        .with("results", OV::Arr(Vec::new()))
        .with("coverage", OV::Obj(coverage));
    if a.command == "current" {
        if query.is_none() {
            result.set("status", OV::str("current_identity_unavailable"));
            return emit(a, result, 1, out);
        }
        let mut identities: Vec<String> = current_ids.iter().flatten().filter(|v| !v.is_empty()).cloned().collect();
        identities.sort();
        identities.dedup();
        if identities.len() > 1 {
            result.set("status", OV::str("ambiguous_current_identity"));
            result.set("results", OV::Arr(identities.into_iter().map(|i| OV::Obj(Obj::new().with("id", OV::Str(i)))).collect()));
            return emit(a, result, 3, out);
        }
    }
    if let Some(q) = query.as_deref().filter(|q| !q.is_empty()) {
        if let Some(native) = native_locator(q)? {
            result.set("status", OV::str("native_required"));
            result.set("native", OV::Obj(native));
            let cov = coverage_of(result);
            cov.set("complete", OV::Bool(false));
            cov.set("scope", OV::str("native source not searched by this CLI"));
            return emit(a, result, 5, out);
        }
    }
    if let Some(q) = &query {
        if strip(q).is_empty() {
            return Err(Fail::Invalid("empty query".into()));
        }
    }
    if !(1..=1000).contains(&a.limit.unwrap_or(1)) || !(0..=20).contains(&a.context.unwrap_or(0)) {
        return Err(Fail::Invalid("limit must be 1..1000 and context 0..20".into()));
    }
    if !(4096..=4194304).contains(&a.max_output_bytes.unwrap_or(4096)) {
        return Err(Fail::Invalid("max-output-bytes must be 4096..4194304".into()));
    }
    if a.command == "list" && !(1..=1000).contains(&a.count.unwrap_or(15)) {
        return Err(Fail::Invalid("count must be 1..1000".into()));
    }
    if a.command == "read" && a.line.unwrap_or(0) < 1 {
        return Err(Fail::Invalid("line must be positive".into()));
    }
    if a.command == "find" && !(1..=100000).contains(&a.max_matches.unwrap_or(10000)) {
        return Err(Fail::Invalid("max-matches must be 1..100000".into()));
    }
    let mut jobs: i128 = 1;
    if a.command == "find" {
        jobs = match a.jobs {
            Some(j) => j,
            None => match env("XSESSION_JOBS").filter(|v| !v.is_empty()) {
                Some(v) => py_int(&v).map_err(Fail::Invalid)?,
                None => cpu_count().min(8),
            },
        };
        if !(1..=64).contains(&jobs) {
            return Err(Fail::Invalid("jobs must be 1..64".into()));
        }
    }
    let ids = SelfIds::from_env();
    let pa = page_args(a);
    let query_ov = query.clone().map_or(OV::Null, OV::Str);
    let mut created_at = OV::Float(snapshot::now());
    let mut key = Obj::new().with("snapshot", OV::Str(uuid4_hex())).with("sha256", OV::Str("0".repeat(64))).with("slot", OV::Int(snapshot::SNAPSHOT_SLOTS - 1));
    let mut saved: Option<Obj> = None;
    let mut offset: i128 = 0;
    let mut rows: Vec<Obj>;
    let mut titles: PyDict<Obj> = PyDict::new();
    let mut titles_json: Option<OV> = None;
    let mut metadata: Vec<OV> = Vec::new();
    if matches!(a.command.as_str(), "find" | "list") && a.cursor.as_deref().is_some_and(|c| !c.is_empty()) {
        let loaded = snapshot::load_snapshot(&pa, &config, &query_ov)?;
        let s = loaded.saved;
        rows = match s.get("rows") {
            Some(OV::Arr(r)) => r.iter().filter_map(|x| x.as_obj().cloned()).collect(),
            _ => return Err(Fail::Invalid("'rows'".into())),
        };
        let t = s.get("titles").cloned().ok_or_else(|| Fail::Invalid("'titles'".into()))?;
        if let OV::Obj(t) = &t {
            for (k, v) in &t.0 {
                titles.insert(OV::Str(k.clone()), v.as_obj().cloned().unwrap_or_default());
            }
        }
        titles_json = Some(t);
        metadata = match s.get("metadata") {
            Some(OV::Arr(m)) => m.clone(),
            _ => Vec::new(),
        };
        snapshot::verify_search_snapshot(&metadata)?;
        let cov = s.get("coverage").cloned().ok_or_else(|| Fail::Invalid("'coverage'".into()))?;
        result.set("coverage", cov);
        created_at = s.get("created_at").cloned().unwrap_or(OV::Null);
        key = loaded.reference;
        offset = loaded.offset;
        saved = Some(s);
    } else {
        rows = inventory(&cfg_obj, coverage_of(result), a.command == "find")?;
    }
    if a.command == "sources" {
        let cov = coverage_of(result).clone();
        let complete = is_complete(&cov);
        result.set("status", OV::str(if complete { "ok" } else { "partial_coverage" }));
        result.set("results", cov.get("sources").cloned().unwrap_or(OV::Arr(Vec::new())));
        return emit(a, result, if complete { 0 } else { 4 }, out);
    }
    if matches!(a.command.as_str(), "find" | "list") {
        let folder = snapshot::snapshot_directory(&pa)?;
        let status = if saved.is_some() { "ready" } else if folder.is_some() { "requested" } else { "disabled" };
        let detail = if folder.is_some() {
            "Continuation uses the explicitly selected directory."
        } else {
            "No snapshot written. Use --snapshot-dir or XSESSION_SNAPSHOT_DIR for persistent paging."
        };
        result.set("pagination", OV::Obj(Obj::new().with("status", OV::str(status)).with("detail", OV::str(detail))));
        if saved.is_none() {
            titles = read_metadata(&cfg_obj, &mut rows, coverage_of(result))?;
            metadata = snapshot::metadata_prefixes(&cfg_obj)?;
        }
        let q = query.clone().unwrap_or_default();
        let saved_matches = saved.as_ref().and_then(|s| s.get("matches")).filter(|m| !m.is_null()).cloned();
        let mut limit = a.limit.unwrap_or(20) as i64;
        let mut hits: Vec<Obj> = if a.command == "find" {
            if let Some(m) = &saved_matches {
                let saved_rows: Vec<OV> = rows.iter().cloned().map(OV::Obj).collect();
                snapshot::verify_search_snapshot(&saved_rows)?;
                m.as_arr().map(|v| v.iter().filter_map(|h| h.as_obj().cloned()).collect()).unwrap_or_default()
            } else {
                let max = a.max_matches.unwrap_or(10000) as usize;
                search(&mut rows, &q, coverage_of(result), &titles, max, jobs as usize, folder.is_some(), &ids)?
            }
        } else {
            let cov = coverage_of(result);
            cov.set("mode", OV::str("bounded_inventory"));
            cov.set("scope", OV::str("configured file inventory and up to 64 KiB at each end; transcript interiors not validated"));
            // Bounded head/tail reads run on threads; results merge in inventory order.
            let mut listed = list_rows(&mut rows, coverage_of(result), cpu_count().min(8) as usize);
            sort_listed(&mut listed)?;
            limit = a.count.unwrap_or(15) as i64;
            listed
        };
        if coverage_of(result).get("snapshot_invalid").is_some_and(OV::truthy) {
            return Err(Fail::Stale("captured source bytes changed or disappeared".into()));
        }
        snapshot::paginate(&hits, limit, offset, &key, result);
        if a.command == "find" && saved_matches.is_some() {
            let titles_obj = titles_json.as_ref().and_then(OV::as_obj).cloned().unwrap_or_default();
            let page: Vec<OV> = result.get("results").and_then(OV::as_arr).cloned().unwrap_or_default();
            let mut reopened = Vec::with_capacity(page.len());
            for hit in page {
                reopened.push(OV::Obj(snapshot::reopen_hit(&hit.as_obj().cloned().unwrap_or_default(), &q, &titles_obj)?));
            }
            result.set("results", OV::Arr(reopened));
        }
        let context = a.context.unwrap_or(0);
        if a.command == "find" && context > 0 {
            let mut page: Vec<OV> = result.get("results").and_then(OV::as_arr).cloned().unwrap_or_default();
            for (j, item) in page.iter_mut().enumerate() {
                let Some(hit) = item.as_obj_mut() else { continue };
                if hit.str_of("provenance") == Some("title_metadata") {
                    continue;
                }
                let mut context_cov = initial_coverage(&cfg_obj);
                let line = hit.int_of("line").unwrap_or(0);
                let records = context_for(hit, line, context as usize, &mut context_cov, &ids)?;
                let expected = hit.get("record_sha256").cloned().unwrap_or(OV::Null);
                let ok = records.iter().find(|r| r.int_of("line") == Some(line)).is_some_and(|r| crate::pyobj::py_eq(r.get("record_sha256").unwrap_or(&OV::Null), &expected));
                hit.set("context", OV::Arr(records.into_iter().map(OV::Obj).collect()));
                let path = hit.str_of("path").unwrap_or("").to_string();
                if !ok {
                    problem(coverage_of(result), "context_record_changed", &path, "");
                }
                if !is_complete(&context_cov) {
                    let cov = coverage_of(result);
                    cov.set("complete", OV::Bool(false));
                    if let Some(OV::Arr(errors)) = context_cov.get("errors") {
                        if let Some(OV::Arr(target)) = cov.get_mut("errors") {
                            target.extend(errors.iter().cloned());
                        }
                    }
                }
                // The page shares its hit objects with the full match list, as in Python.
                if saved_matches.is_none() {
                    let at = (offset as usize).saturating_add(j);
                    if at < hits.len() {
                        hits[at] = hit.clone();
                    }
                }
            }
            result.set("results", OV::Arr(page));
        }
        result.set("elapsed_seconds", OV::Float(round3(started.elapsed().as_secs_f64())));
        result.set("snapshot", OV::Obj(
            Obj::new()
                .with("captured_at_unix", created_at.clone())
                .with("scope", OV::str("captured files and byte prefixes; subsequent files and appends are outside this search"))
                .with("id", if saved.is_some() { key.get("snapshot").cloned().unwrap_or(OV::Null) } else { OV::Null })
                .with("persisted", OV::Bool(saved.is_some())),
        ));
        result.set("status", OV::str(if hits.is_empty() { "no_match" } else { "ok" }));
        if !is_complete(coverage_of(result)) {
            result.set("status", OV::str("partial_coverage"));
        }
        let budget = pa.max_output_bytes;
        if !snapshot::fit_output(budget, &key, result)? {
            return emit(a, result, 2, out);
        }
        let has_cursor = result.get("next_cursor").is_some_and(OV::truthy);
        if has_cursor && saved.is_none() {
            let cov_now = coverage_of(result).clone();
            let snap = Snapshot {
                config: &config,
                query: &query_ov,
                rows: &rows,
                titles: &titles,
                titles_json: None,
                coverage: &cov_now,
                created_at: created_at.as_f64_or(0.0),
                hits: &hits,
                metadata: &metadata,
            };
            match snapshot::save_snapshot(&pa, &snap, &mut key) {
                Ok(()) => {
                    if let Some(OV::Obj(p)) = result.get_mut("pagination") {
                        p.set("status", OV::str("ready"));
                    }
                    if let Some(OV::Obj(s)) = result.get_mut("snapshot") {
                        s.set("id", key.get("snapshot").cloned().unwrap_or(OV::Null));
                        s.set("persisted", OV::Bool(true));
                    }
                }
                Err(Fail::Os(e)) => {
                    let detail: String = e.text.chars().take(500).collect();
                    result.set("pagination", OV::Obj(Obj::new().with("status", OV::str("unavailable"))
                        .with("detail", OV::Str(format!("Search results retained; continuation unavailable: {}", detail)))));
                }
                Err(other) => return Err(other),
            }
            if !snapshot::fit_output(budget, &key, result)? {
                return emit(a, result, 2, out);
            }
        } else if !result.get("truncated").is_some_and(OV::truthy) {
            if let Some(OV::Obj(p)) = result.get_mut("pagination") {
                if p.str_of("status") == Some("requested") {
                    p.set("status", OV::str("not_needed"));
                }
            }
        }
        result.set("elapsed_seconds", OV::Float(round3(started.elapsed().as_secs_f64())));
        let complete = is_complete(coverage_of(result));
        let code = if complete { if hits.is_empty() { 1 } else { 0 } } else { 4 };
        return emit(a, result, code, out);
    }
    // path, resolve, current, read
    let q = query.clone().unwrap_or_default();
    {
        let cov = coverage_of(result);
        cov.set("mode", OV::str("identity_inventory"));
        cov.set("scope", OV::str("configured file identities and header-observed resume relations; transcript interiors and exhaustive ancestry not scanned"));
    }
    let groups = resolve(&rows, &q)?;
    let folded = casefold(&q);
    let first_id_matches = groups.first().and_then(|g| g.str_of("id")).is_some_and(|id| casefold(id) == folded);
    let segments_of = |g: &Obj| g.get("segments").and_then(OV::as_arr).map_or(0, Vec::len);
    let related_of = |g: &Obj| g.get("related_segments").and_then(OV::as_arr).map_or(0, Vec::len);
    result.set("results", OV::Arr(groups.iter().cloned().map(OV::Obj).collect()));
    result.set("exact_path_available", OV::Bool(groups.len() == 1 && segments_of(&groups[0]) == 1 && first_id_matches));
    let mut code = if groups.is_empty() { 1 } else { 0 };
    result.set("status", OV::str(if groups.is_empty() { "no_match" } else { "ok" }));
    let complete = is_complete(coverage_of(result));
    if groups.len() > 1 {
        code = 3;
        result.set("status", OV::str("ambiguous"));
    } else if !groups.is_empty() && segments_of(&groups[0]) == 0 && related_of(&groups[0]) > 0 {
        if a.command == "resolve" {
            code = if complete { 0 } else { 4 };
            result.set("status", OV::str(if complete { "related_history_available" } else { "partial_coverage" }));
        } else {
            code = 3;
            result.set("status", OV::str("related_history_use_resolve"));
        }
    } else if !groups.is_empty() && matches!(a.command.as_str(), "path" | "current") && segments_of(&groups[0]) != 1 {
        code = 3;
        result.set("status", OV::str("multiple_segments_use_resolve"));
    } else if !complete {
        code = 4;
        result.set("status", OV::str("partial_coverage"));
    }
    if a.command == "read" && (code == 0 || (code == 4 && groups.len() == 1 && first_id_matches)) {
        let mut segments: Vec<Obj> = groups[0].get("segments").and_then(OV::as_arr).map(|s| s.iter().filter_map(|x| x.as_obj().cloned()).collect()).unwrap_or_default();
        if let Some(seg) = a.segment.as_deref().filter(|s| !s.is_empty()) {
            segments.retain(|s| s.str_of("path") == Some(seg));
            if segments.is_empty() {
                return Err(Fail::Invalid("segment must be one returned by resolve".into()));
            }
        }
        if segments.len() != 1 {
            code = 3;
            result.set("status", OV::str("multiple_segments_choose_segment"));
        } else {
            let line = a.line.unwrap_or(1) as i64;
            let mut cov = coverage_of(result).clone();
            let records = context_for(&mut segments[0], line, a.context.unwrap_or(0) as usize, &mut cov, &ids)?;
            result.set("coverage", OV::Obj(cov));
            if let Some(expected) = a.record_sha256.as_deref().filter(|s| !s.is_empty()) {
                let matched = records.iter().find(|r| r.int_of("line") == Some(line));
                if !matched.is_some_and(|r| r.str_of("record_sha256") == Some(expected)) {
                    result.set("status", OV::str("record_changed"));
                    result.set("results", OV::Arr(Vec::new()));
                    return emit(a, result, 3, out);
                }
            }
            let empty = records.is_empty();
            result.set("results", OV::Arr(records.into_iter().map(OV::Obj).collect()));
            let complete = is_complete(coverage_of(result));
            code = if complete { if empty { 1 } else { 0 } } else { 4 };
            result.set("status", OV::str(match code {
                0 => "ok",
                4 => "partial_coverage",
                _ => "no_match",
            }));
        }
    }
    emit(a, result, code, out)
}

trait AsF64 {
    fn as_f64_or(&self, d: f64) -> f64;
}

impl AsF64 for OV {
    fn as_f64_or(&self, d: f64) -> f64 {
        match self {
            OV::Float(f) => *f,
            OV::Int(i) => *i as f64,
            _ => d,
        }
    }
}
