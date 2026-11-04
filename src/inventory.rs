//! Sources, coverage bookkeeping, and the file inventory (2.3.0 `config_for`, `initial_coverage`,
//! `problem`, `inspect_file`, `inventory`).

use crate::ojson::{self, Obj, OV};
use crate::pathx::{self, from_os};
use crate::pyerr::{Fail, PyOsError};
use crate::pyio;
use crate::pytext::casefold;
use crate::session;
use std::collections::{HashMap, HashSet};
use std::io::{BufReader, Seek};

pub const VERSION: &str = "3.4.0";

pub const ANTIGRAVITY_EXCLUDED: &str = "Antigravity: only <conversation>/.system_generated/logs/transcript.jsonl is searched; \
     transcript_full.jsonl (the same records untruncated, at the same lines), logs/chunks/ (byte-split copies) and step thinking are not";
const SUBAGENTS_CREATED: [&str; 2] = ["Created the following subagents:", "Created the following subagent:"];

pub fn env(name: &str) -> Option<String> {
    std::env::var_os(name).map(|v| from_os(&v))
}

/// 2.3.0 `UUID.fullmatch` (case-insensitive hex).
pub fn is_uuid(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 36
        && b.iter().enumerate().all(|(i, c)| if matches!(i, 8 | 13 | 18 | 23) { *c == b'-' } else { c.is_ascii_hexdigit() })
}

/// `UUID.findall(text)`.
pub fn uuid_findall(text: &str) -> Vec<String> {
    let b = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i + 36 <= b.len() {
        if text.is_char_boundary(i) && text.is_char_boundary(i + 36) && is_uuid(&text[i..i + 36]) {
            out.push(text[i..i + 36].to_string());
            i += 36;
        } else {
            i += 1;
        }
    }
    out
}

// ------------------------------------------------------------------------------- config ---

/// `config_for(args)`.
pub fn config_for(sources_arg: Option<&str>) -> Result<OV, Fail> {
    let config_path = match sources_arg {
        Some(s) if !s.is_empty() => Some(s.to_string()),
        _ => env("XSESSION_CONFIG").filter(|s| !s.is_empty()),
    };
    if let Some(path) = config_path {
        let path = pathx::norm(&path);
        let text = match pyio::read_text_sig(&path) {
            Ok(t) => t,
            Err(pyio::TextError::Os(e)) => return Err(Fail::Os(e)),
            Err(pyio::TextError::Decode(m)) => return Err(Fail::Invalid(m)),
        };
        return ojson::loads(&text).map_err(Fail::Invalid);
    }
    let home = pathx::home()?;
    // The Windows-side home: the caller's own home unless XSESSION_WINDOWS_HOME names another
    // (under WSL, /mnt/c/Users/<name> reads the Windows stores).
    let windows = match env("XSESSION_WINDOWS_HOME").filter(|s| !s.is_empty()) {
        Some(h) => pathx::norm(&h),
        None => home.clone(),
    };
    let under = |rel: &str| pathx::join(&windows, rel);
    let claude = env("CLAUDE_PROJECTS_DIR").unwrap_or_else(|| under(".claude/projects"));
    let win_codex = pathx::norm(&env("CODEX_WIN_SESSIONS_DIR").unwrap_or_else(|| under(".codex/sessions")));
    let source = |name: &str, kind: &str, path: String| {
        OV::Obj(Obj::new().with("name", OV::str(name)).with("kind", OV::str(kind)).with("path", OV::Str(path)))
    };
    let mut sources = vec![
        source("claude", "claude", claude),
        source("codex-windows", "codex", win_codex.clone()),
        source("codex-archive", "codex", pathx::join(&pathx::parent(&win_codex), "archived_sessions")),
        source("chatgpt-projects", "chatgpt_project", env("CHATGPT_PROJECTS_DIR").unwrap_or_else(|| under(".codex/.chatgpt-projects"))),
        source("antigravity", "antigravity", env("ANTIGRAVITY_BRAIN_DIR").unwrap_or_else(|| under(".gemini/antigravity/brain"))),
    ];
    let mut wsl = env("CODEX_SESSIONS_DIR");
    if !cfg!(windows) && wsl.as_deref().map_or(true, str::is_empty) {
        wsl = Some(pathx::join(&home, ".codex/sessions"));
    }
    // Off Windows without XSESSION_WINDOWS_HOME, codex-windows already is ~/.codex/sessions:
    // never list the same store twice.
    let duplicate = |w: &String| !cfg!(windows) && pathx::norm(w) == win_codex;
    if let Some(wsl) = wsl.filter(|w| !w.is_empty() && !duplicate(w)) {
        let archive = pathx::join(&pathx::parent(&wsl), "archived_sessions");
        sources.push(source("codex-wsl", "codex", wsl));
        sources.push(source("codex-wsl-archive", "codex", archive));
    }
    let excluded = ["native app stores require supported app tools",
                    "WSL is opt-in via CODEX_SESSIONS_DIR on Windows; no distro is started",
                    "memories and handoffs require explicit doc source roots"];
    Ok(OV::Obj(
        Obj::new()
            .with("sources", OV::Arr(sources))
            .with("title_index", OV::Str(under(".codex/session_index.jsonl")))
            .with("imports", OV::Str(under(".codex/external_agent_session_imports.json")))
            .with("excluded", OV::Arr(excluded.iter().map(|s| OV::str(*s)).collect())),
    ))
}

// ----------------------------------------------------------------------------- coverage ---

pub fn initial_coverage(config: &Obj) -> Obj {
    let mut excluded = config.get("excluded").cloned().unwrap_or(OV::Arr(Vec::new()));
    let antigravity = match config.get("sources") {
        Some(OV::Arr(sources)) => sources.iter().any(|s| s.as_obj().and_then(|o| o.str_of("kind")) == Some("antigravity")),
        _ => false,
    };
    if let (OV::Arr(items), true) = (&mut excluded, antigravity) {
        items.push(OV::str(ANTIGRAVITY_EXCLUDED));
    }
    Obj::new()
        .with("scope", OV::str("configured sources only"))
        .with("complete", OV::Bool(true))
        .with("sources", OV::Arr(Vec::new()))
        .with("errors", OV::Arr(Vec::new()))
        .with("excluded", excluded)
        .with("files", OV::Int(0))
        .with("malformed_records", OV::Int(0))
        .with("unfinished_records", OV::Int(0))
        .with("changed_files", OV::Int(0))
        .with("unsupported_records", OV::Int(0))
        .with("notes", OV::Arr(Vec::new()))
        .with("mode", OV::str("completed_record_prefixes"))
}

fn push_to(cov: &mut Obj, key: &str, item: OV) {
    match cov.get_mut(key) {
        Some(OV::Arr(items)) => items.push(item),
        _ => cov.set(key, OV::Arr(vec![item])),
    }
}

/// `problem(coverage, kind, path, detail)`.
pub fn problem(cov: &mut Obj, kind: &str, path: &str, detail: &str) {
    cov.set("complete", OV::Bool(false));
    push_to(cov, "errors", OV::Obj(Obj::new().with("kind", OV::str(kind)).with("path", OV::str(path)).with("detail", OV::str(detail))));
}

pub fn note(cov: &mut Obj, item: Obj) {
    push_to(cov, "notes", OV::Obj(item));
}

pub fn note3(cov: &mut Obj, kind: &str, path: &str, detail: &str) {
    note(cov, Obj::new().with("kind", OV::str(kind)).with("path", OV::str(path)).with("detail", OV::str(detail)));
}

/// `coverage[key] += by`.
pub fn bump(cov: &mut Obj, key: &str, by: i64) {
    let now = cov.get(key).and_then(OV::as_i64).unwrap_or(0);
    cov.set(key, OV::Int(now + by));
}

pub fn is_complete(cov: &Obj) -> bool {
    cov.get("complete").is_some_and(OV::truthy)
}

fn status_mut(cov: &mut Obj, index: usize) -> Option<&mut Obj> {
    match cov.get_mut("sources") {
        Some(OV::Arr(items)) => items.get_mut(index).and_then(OV::as_obj_mut),
        _ => None,
    }
}

pub fn set_status(cov: &mut Obj, index: usize, status: &str) {
    if let Some(s) = status_mut(cov, index) {
        s.set("status", OV::str(status));
    }
}

/// `status['files'] += 1; coverage['files'] += 1`.
pub fn count_file(cov: &mut Obj, index: usize) {
    if let Some(s) = status_mut(cov, index) {
        bump(s, "files", 1);
    }
    bump(cov, "files", 1);
}

// ---------------------------------------------------------------------------- inventory ---

pub struct Source<'a> {
    pub name: &'a str,
    pub kind: &'a str,
}

/// `inventory(config, coverage, guard)`.
pub fn inventory(config: &Obj, cov: &mut Obj, guard: bool) -> Result<Vec<Obj>, Fail> {
    let mut rows = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let sources: Vec<Obj> = match config.get("sources") {
        Some(OV::Arr(items)) => items.iter().filter_map(|s| s.as_obj().cloned()).collect(),
        _ => Vec::new(),
    };
    for src in &sources {
        let source = Source { name: src.str_of("name").unwrap_or(""), kind: src.str_of("kind").unwrap_or("") };
        let root = pathx::expanduser(src.str_of("path").unwrap_or(""))?;
        let index = match cov.get("sources") {
            Some(OV::Arr(items)) => items.len(),
            _ => 0,
        };
        push_to(cov, "sources", OV::Obj(
            Obj::new().with("name", OV::str(source.name)).with("kind", OV::str(source.kind)).with("path", OV::str(&root))
                .with("status", OV::str("searched")).with("files", OV::Int(0)),
        ));
        if !pyio::is_dir(&root) {
            set_status(cov, index, "unavailable");
            problem(cov, "source_unavailable", &root, "");
            continue;
        }
        if source.kind == "chatgpt_project" || source.kind == "antigravity" {
            let result = match pathx::resolve(&root) {
                Ok(resolved_root) if source.kind == "chatgpt_project" => {
                    crate::projects::project_rows(&root, &resolved_root, &source, index, cov, &mut seen, guard)
                }
                Ok(resolved_root) => antigravity_rows(&root, &resolved_root, &source, index, cov, &mut seen, guard),
                Err(e) => Err(Fail::Os(e)),
            };
            match result {
                Ok(found) => rows.extend(found),
                Err(Fail::Os(e)) => {
                    set_status(cov, index, "partial");
                    problem(cov, "read_error", &root, &e.text);
                }
                Err(other) => return Err(other),
            }
            continue;
        }
        let resolved_root = pathx::resolve(&root)?;
        {
            let cov_cell = std::cell::RefCell::new(&mut *cov);
            let mut on_error = |e: PyOsError| {
                let mut cov = cov_cell.borrow_mut();
                set_status(&mut cov, index, "partial");
                problem(&mut cov, "read_error", &root, &e.text);
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
                    let eligible = if source.kind == "document" {
                        matches!(pathx::suffix(&name).to_lowercase().as_str(), ".md" | ".json" | ".jsonl" | ".txt")
                    } else {
                        let readable = (name.ends_with(".jsonl") && name != "journal.jsonl")
                            || (name.ends_with(session::ZSTD_SUFFIX) && name != "journal.jsonl.zst");
                        if !readable && !name.starts_with("journal.jsonl") {
                            // A transcript stored in a form this tool cannot read is a coverage gap, never a silent skip.
                            if let Some(at) = name.find(".jsonl.") {
                                problem(&mut cov, "unsupported_session_file", &pathx::join(directory, &name),
                                        &format!("unrecognized transcript encoding '{}'; not searched", &name[at + 6..]));
                            }
                        }
                        readable
                    };
                    if !eligible {
                        continue;
                    }
                    let p = pathx::join(directory, &name);
                    if pyio::is_symlink(&p) {
                        problem(&mut cov, "excluded_link", &p, "");
                        continue;
                    }
                    let rel = pathx::relative_parts(&p, &root);
                    let resolved = pathx::join_parts(&resolved_root, &rel);
                    if !seen.insert(resolved.clone()) {
                        continue;
                    }
                    count_file(&mut cov, index);
                    let result = if source.kind == "document" {
                        document_row(&p, &name, &rel, &source, &resolved, guard)
                    } else {
                        inspect_file(&p, &source, &mut cov, guard, &resolved)
                    };
                    match result {
                        Ok(Some(row)) => rows.push(row),
                        Ok(None) => {}
                        Err(Fail::Os(e)) => problem(&mut cov, "read_error", &p, &e.text),
                        Err(other) => return Err(other),
                    }
                }
                Ok(())
            };
            pyio::walk(&root, &mut on_error, &mut visit)?;
        }
    }
    link_subagents(&mut rows, cov);
    Ok(rows)
}

/// `antigravity_rows(root, resolved_root, source, status, coverage, seen_paths, guard)`: one row per
/// conversation folder (named by its UUID), from `.system_generated/logs/transcript.jsonl`.
/// `transcript_full.jsonl` and `chunks/` beside it hold copies of the same records and are not read.
fn antigravity_rows(root: &str, resolved_root: &str, source: &Source, index: usize, cov: &mut Obj,
                    seen: &mut HashSet<String>, guard: bool) -> Result<Vec<Obj>, Fail> {
    let mut rows = Vec::new();
    let mut folders: Vec<String> = pyio::scandir(root)?.into_iter().map(|e| e.name).collect();
    folders.sort();
    for name in folders.iter().filter(|n| is_uuid(n)) {
        let folder = pathx::join(root, name);
        let generated = pathx::join(&folder, ".system_generated");
        let logs = pathx::join(&generated, "logs");
        if let Some(linked) = [&folder, &generated, &logs].into_iter().find(|p| pyio::is_link_or_junction(p)) {
            problem(cov, "excluded_link", linked, "");
            continue;
        }
        if !pyio::is_dir(&logs) {
            continue;
        }
        let mut names: Vec<String> = match pyio::scandir(&logs) {
            Ok(entries) => entries.into_iter().map(|e| e.name)
                .filter(|n| (n == "transcript.jsonl" || n.starts_with("transcript.jsonl.")) && !pyio::is_dir(&pathx::join(&logs, n)))
                .collect(),
            Err(e) => {
                problem(cov, "read_error", &folder, &e.text);
                continue;
            }
        };
        names.sort();
        for name in names {
            let p = pathx::join(&logs, &name);
            if name != "transcript.jsonl" && name != "transcript.jsonl.zst" {
                let at = name.find(".jsonl.").unwrap_or(0);
                problem(cov, "unsupported_session_file", &p, &format!("unrecognized transcript encoding '{}'; not searched", &name[at + 6..]));
                continue;
            }
            if pyio::is_symlink(&p) {
                problem(cov, "excluded_link", &p, "");
                continue;
            }
            let resolved = pathx::join_parts(resolved_root, &pathx::relative_parts(&p, root));
            if !seen.insert(resolved.clone()) {
                continue;
            }
            count_file(cov, index);
            match inspect_file(&p, source, cov, guard, &resolved) {
                Ok(Some(row)) => rows.push(row),
                Ok(None) => {}
                Err(Fail::Os(e)) => problem(cov, "read_error", &p, &e.text),
                Err(other) => return Err(other),
            }
        }
    }
    Ok(rows)
}

/// `CONVERSATION_ID.findall(text)`: `"conversationId":[ \t\r\n]*"([^"]*)"`.
fn conversation_ids(text: &str) -> Vec<&str> {
    const KEY: &str = "\"conversationId\":";
    let b = text.as_bytes();
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(at) = text[from..].find(KEY) {
        let start = from + at;
        let mut i = start + KEY.len();
        while i < b.len() && matches!(b[i], b' ' | b'\t' | b'\r' | b'\n') {
            i += 1;
        }
        if b.get(i) == Some(&b'"') {
            if let Some(end) = text[i + 1..].find('"') {
                out.push(&text[i + 1..i + 1 + end]);
                from = i + 2 + end;
                continue;
            }
        }
        from = start + 1;
    }
    out
}

/// Records each conversation the transcript at `path` says it created, as found: a read error
/// keeps the children found before it, as in Python.
fn created_children(path: &str, creator: &OV, created: &mut HashMap<String, OV>) -> Result<(), PyOsError> {
    const MARK: &[u8] = b"Created the following subagent";
    let mut reader = BufReader::with_capacity(session::read_ahead(path, 1 << 20), session::open(path)?);
    let mut raw = Vec::new();
    loop {
        pyio::read_line(&mut reader, u64::MAX, &mut raw).map_err(|e| pyio::read_err(&e))?;
        if raw.is_empty() {
            break;
        }
        if !raw.windows(MARK.len()).any(|w| w == MARK) {
            continue;
        }
        let Ok(OV::Obj(e)) = ojson::loads_bytes(&raw) else { continue };
        if matches!(e.str_of("type"), Some("USER_INPUT" | "PLANNER_RESPONSE")) {
            continue;
        }
        let Some(mut text) = e.str_of("content") else { continue };
        while text.starts_with("Created At:") || text.starts_with("Completed At:") {
            text = text.split_once('\n').map_or("", |(_, rest)| rest);
        }
        let (head, rest) = text.split_once('\n').unwrap_or((text, ""));
        if SUBAGENTS_CREATED.contains(&head) {
            for child in conversation_ids(rest).into_iter().filter(|c| is_uuid(c)) {
                created.entry(casefold(child)).or_insert_with(|| creator.clone());
            }
        }
    }
    Ok(())
}

/// `link_subagents(rows, coverage)`: an Antigravity conversation that another one created is a
/// subagent; its USER_INPUT holds its creator's prompt, not the operator's.
fn link_subagents(rows: &mut [Obj], cov: &mut Obj) {
    let mut created: HashMap<String, OV> = HashMap::new();
    for row in rows.iter().filter(|r| r.str_of("agent") == Some("antigravity")) {
        let path = pathx::norm(row.str_of("path").unwrap_or(""));
        if let Err(e) = created_children(&path, row.get("id").unwrap_or(&OV::Null), &mut created) {
            problem(cov, "read_error", &path, &e.text);
        }
    }
    for row in rows.iter_mut().filter(|r| r.str_of("agent") == Some("antigravity")) {
        let own = casefold(row.str_of("id").unwrap_or(""));
        if let Some(parent) = created.get(&own).filter(|p| casefold(p.as_str().unwrap_or("")) != own).cloned() {
            row.set("kind", OV::str("subagent"));
            row.set("parent_id", parent);
        }
    }
}

fn document_row(p: &str, name: &str, rel: &[String], source: &Source, resolved: &str, guard: bool) -> Result<Option<Obj>, Fail> {
    let st = pyio::stat(p)?;
    let guards = if guard { pyio::capture_guards(p, st.size)? } else { OV::Arr(Vec::new()) };
    Ok(Some(
        Obj::new()
            .with("id", OV::Str(format!("doc:{}:{}", source.name, pathx::as_posix(rel))))
            .with("agent", OV::str("document"))
            .with("source", OV::str(source.name))
            .with("kind", OV::str("document"))
            .with("parent_id", OV::Null)
            .with("path", OV::str(resolved))
            .with("bytes", OV::Int(st.size as i64))
            .with("mtime_ns", int_ov(st.mtime_ns))
            .with("timestamp", OV::Null)
            .with("timestamp_basis", OV::str("unknown"))
            .with("title", OV::str(name))
            .with("_guards", guards),
    ))
}

pub fn int_ov(v: i128) -> OV {
    match i64::try_from(v) {
        Ok(i) => OV::Int(i),
        Err(_) => OV::Big(v.to_string()),
    }
}

/// `value + '::'` for the parent identity, with Python's TypeError when it is not a string.
fn concat_parent(parent: &OV) -> Result<String, Fail> {
    match parent {
        OV::Str(s) => Ok(format!("{}::", s)),
        OV::Arr(_) => Err(Fail::Invalid("can only concatenate list (not \"str\") to list".into())),
        other => Err(Fail::Invalid(format!("unsupported operand type(s) for +: '{}' and 'str'", crate::pyerr::type_name(other)))),
    }
}

/// `inspect_file(path, source, coverage, guard, resolved)`.
pub fn inspect_file(path: &str, source: &Source, cov: &mut Obj, guard: bool, resolved: &str) -> Result<Option<Obj>, Fail> {
    let st = pyio::stat(path)?;
    let compressed = session::is_compressed(path);
    let mut ident = OV::Null;
    let mut timestamp = OV::Null;
    let mut parent = OV::Null;
    let mut resumed_from = OV::Null;
    let mut row_kind = "conversation";
    let mut guards = OV::Arr(Vec::new());
    let mut file = pyio::open(path)?;
    // One open: the frame walk, the identity lines, and the stored-byte guards.
    let content = if compressed {
        let size = session::content_size(&mut file, st.size)?;
        file.rewind().map_err(|e| pyio::read_err(&e))?;
        size
    } else {
        st.size
    };
    let logical = if compressed { session::logical_over(file).map_err(|e| pyio::read_err(&e))? } else { session::Logical::Plain(file) };
    let mut reader = BufReader::with_capacity(session::read_ahead(path, 64 * 1024), logical);
    let mut line = Vec::new();
    for _ in 0..64 {
        pyio::read_line(&mut reader, 1024 * 1024 + 1, &mut line).map_err(|e| pyio::read_err(&e))?;
        if line.is_empty() || line.len() > 1024 * 1024 {
            break;
        }
        let Ok(OV::Obj(e)) = ojson::loads_bytes(&line) else { continue };
        if source.kind == "antigravity" {
            timestamp = e.get("created_at").cloned().unwrap_or(OV::Null);
            break;
        }
        if e.get("type").and_then(OV::as_str) == Some("session_meta") {
            let empty = OV::Obj(Obj::new());
            let p = e.get("payload").filter(|p| p.truthy()).unwrap_or(&empty);
            let Some(p) = p.as_obj() else {
                problem(cov, "unsupported_metadata", path, "");
                continue;
            };
            ident = p.get("id").filter(|v| v.truthy()).or_else(|| p.get("session_id")).cloned().unwrap_or(OV::Null);
            timestamp = p.get("timestamp").filter(|v| v.truthy()).or_else(|| e.get("timestamp")).cloned().unwrap_or(OV::Null);
            if let Some(OV::Obj(origin)) = p.get("source") {
                if let Some(sub) = origin.get("subagent") {
                    row_kind = "subagent";
                    if let OV::Obj(sub) = sub {
                        parent = match sub.get("spawn") {
                            None => OV::Null,
                            Some(OV::Obj(spawn)) => spawn.get("parent_thread_id").cloned().unwrap_or(OV::Null),
                            Some(_) => OV::Null,
                        };
                        if sub.contains("review") {
                            row_kind = "reviewer";
                        }
                    }
                }
            }
            break;
        }
        if source.kind == "claude" {
            if let Some(sid) = e.get("sessionId").filter(|v| v.truthy()) {
                ident = sid.clone();
                timestamp = e.get("timestamp").cloned().unwrap_or(OV::Null);
                break;
            }
        }
    }
    if guard {
        // Guards hash the stored bytes; for a .jsonl.zst these are the compressed bytes.
        let mut f = reader.into_inner().into_file().ok_or_else(|| crate::pyerr::os_text(session::UNDECODABLE))?;
        f.rewind().map_err(|e| pyio::read_err(&e))?;
        guards = pyio::guards_from_file(&mut f, st.size).map_err(|e| pyio::read_err(&e))?;
    }
    let stem = session::stem(path);
    if source.kind == "antigravity" {
        // <conversation id>/.system_generated/logs/transcript.jsonl: the folder names the identity.
        ident = OV::Str(pathx::name(&pathx::parent(&pathx::parent(&pathx::parent(path)))));
    } else if source.kind == "claude" && pathx::parts(path).iter().any(|p| p == "subagents") {
        parent = match pathx::parts(path).iter().rev().find(|p| is_uuid(p)) {
            Some(u) => OV::Str(u.clone()),
            None => ident.clone(),
        };
        let prefix = if parent.truthy() { concat_parent(&parent)? } else { String::new() };
        ident = OV::Str(prefix + &stem);
        row_kind = "subagent";
    } else if source.kind == "claude" && is_uuid(&stem) {
        if ident.truthy() {
            let folded = crate::pyobj::casefold_of(&ident)?;
            if folded != casefold(&stem) {
                resumed_from = ident.clone();
            }
        }
        ident = OV::Str(stem);
    }
    if !ident.truthy() {
        problem(cov, "identity_unavailable", path, "");
        return Ok(None);
    }
    // For a .jsonl.zst, `bytes` is the decompressed size and `mtime_ns` the stored file's.
    Ok(Some(
        Obj::new()
            .with("id", ident)
            .with("agent", OV::str(source.kind))
            .with("source", OV::str(source.name))
            .with("kind", OV::str(row_kind))
            .with("parent_id", parent)
            .with("resumed_from", resumed_from)
            .with("path", OV::str(resolved))
            .with("bytes", int_ov(content as i128))
            .with("mtime_ns", int_ov(st.mtime_ns))
            .with("timestamp", timestamp)
            .with("timestamp_basis", OV::str("first metadata record"))
            .with("title", OV::Null)
            .with("_guards", guards),
    ))
}
