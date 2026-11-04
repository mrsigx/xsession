//! ChatGPT project stores (2.3.0 `project_format`, `project_manifest`, `project_rows`).

use crate::hash::sha256_hex;
use crate::inventory::{count_file, int_ov, note3, problem, Source};
use crate::ojson::{self, Obj, OV};
use crate::pathx;
use crate::pyerr::Fail;
use crate::pyio::{self, TextError};
use crate::pyobj::py_eq;
use std::collections::{HashMap, HashSet};

const PROJECT_TEXT_SUFFIXES: [&str; 23] = [".md", ".markdown", ".txt", ".json", ".jsonl", ".csv", ".tsv", ".jsx", ".js", ".ts",
    ".tsx", ".py", ".rs", ".html", ".htm", ".css", ".xml", ".yaml", ".yml", ".toml", ".ps1", ".sh", ".sql"];

pub fn project_format(name: &str) -> Option<&'static str> {
    let suffix = pathx::suffix(name).to_lowercase();
    if PROJECT_TEXT_SUFFIXES.contains(&suffix.as_str()) {
        return Some("text");
    }
    match suffix.as_str() {
        ".docx" => Some("docx"),
        ".xlsx" => Some("xlsx"),
        _ => None,
    }
}

/// `.metadata/<project>.json`, schema version 1: {name: sha256}, or None (problem recorded).
fn project_manifest(root: &str, project: &str, cov: &mut Obj) -> Option<HashMap<String, String>> {
    let path = pathx::join(&pathx::join(root, ".metadata"), &format!("{}.json", pathx::name(project)));
    let unsupported = |cov: &mut Obj, detail: &str| problem(cov, "project_manifest_unsupported", &path, detail);
    let text = match pyio::read_text_sig(&path) {
        Ok(t) => t,
        Err(TextError::Os(e)) => {
            problem(cov, "project_manifest_unavailable", &path, &e.text);
            return None;
        }
        Err(TextError::Decode(m)) => {
            unsupported(cov, &m);
            return None;
        }
    };
    let data = match ojson::loads(&text) {
        Ok(d) => d,
        Err(m) => {
            unsupported(cov, &m);
            return None;
        }
    };
    let files = match &data {
        OV::Obj(o) if o.get("version").is_some_and(|v| py_eq(v, &OV::Int(1))) => match o.get("files") {
            Some(OV::Arr(files)) => files,
            _ => {
                unsupported(cov, "unsupported manifest schema");
                return None;
            }
        },
        _ => {
            unsupported(cov, "unsupported manifest schema");
            return None;
        }
    };
    let mut listed = HashMap::new();
    for entry in files {
        match (entry.as_obj().and_then(|e| e.str_of("name")), entry.as_obj().and_then(|e| e.str_of("sha256"))) {
            (Some(name), Some(sha)) => {
                listed.insert(name.to_string(), sha.to_lowercase());
            }
            _ => {
                unsupported(cov, "unsupported manifest entry");
                return None;
            }
        }
    }
    Some(listed)
}

/// `project_rows(root, resolved_root, source, status, coverage, seen_paths, guard)`.
pub fn project_rows(root: &str, resolved_root: &str, source: &Source, index: usize, cov: &mut Obj,
                    seen: &mut HashSet<String>, guard: bool) -> Result<Vec<Obj>, Fail> {
    let mut rows = Vec::new();
    let mut projects: Vec<String> = Vec::new();
    for e in pyio::scandir(root)? {
        let p = pathx::join(root, &e.name);
        if pyio::is_dir(&p) && !e.name.starts_with('.') {
            projects.push(p);
        }
    }
    pathx::sort_paths(&mut projects);
    for project in projects {
        if pyio::is_link_or_junction(&project) {
            problem(cov, "excluded_link", &project, "");
            continue;
        }
        let listed = project_manifest(root, &project, cov);
        let instructions = pathx::join(&project, "AGENTS.md");
        let mut files: Vec<(String, bool)> = Vec::new();
        if pyio::is_file(&instructions) {
            files.push((instructions, true));
        }
        let folder = pathx::join(&project, "sources");
        if pyio::is_dir(&folder) {
            let mut entries: Vec<String> = pyio::scandir(&folder)?.into_iter().map(|e| pathx::join(&folder, &e.name)).collect();
            pathx::sort_paths(&mut entries);
            for p in entries {
                if pyio::is_link_or_junction(&p) {
                    problem(cov, "excluded_link", &p, "");
                } else if pyio::is_dir(&p) {
                    note3(cov, "project_subdirectory_not_searched", &p, "");
                } else {
                    files.push((p, false));
                }
            }
        }
        let project_name = pathx::name(&project);
        let mut present: HashSet<String> = HashSet::new();
        for (p, is_instructions) in files {
            let resolved = pathx::join_parts(resolved_root, &pathx::relative_parts(&p, root));
            if !seen.insert(resolved.clone()) {
                continue;
            }
            count_file(cov, index);
            let name = pathx::name(&p);
            if !is_instructions {
                present.insert(name.clone());
            }
            let Some(form) = project_format(&name) else {
                note3(cov, "not_text_searchable", &resolved, &pathx::suffix(&p).to_lowercase());
                continue;
            };
            let built = (|| -> Result<Obj, Fail> {
                let st = pyio::stat(&p)?;
                let state = if is_instructions {
                    "instructions_not_manifested"
                } else {
                    match &listed {
                        None => "unverified",
                        Some(listed) => match listed.get(&name) {
                            None => {
                                note3(cov, "project_file_unlisted", &resolved, "");
                                "unlisted"
                            }
                            Some(expected) => {
                                if &sha256_hex(&pyio::read_bytes(&p)?) == expected {
                                    "verified"
                                } else {
                                    note3(cov, "project_manifest_mismatch", &resolved, "file bytes differ from the project manifest SHA-256");
                                    "hash_mismatch"
                                }
                            }
                        },
                    }
                };
                let guards = if guard { pyio::capture_guards(&p, st.size)? } else { OV::Arr(Vec::new()) };
                let rel = pathx::as_posix(&pathx::relative_parts(&p, &project));
                Ok(Obj::new()
                    .with("id", OV::Str(format!("chatgpt:{}:{}", project_name, rel)))
                    .with("agent", OV::str("chatgpt_project"))
                    .with("source", OV::str(source.name))
                    .with("kind", OV::str(if is_instructions { "project_instructions" } else { "project_source" }))
                    .with("parent_id", OV::str(&project_name))
                    .with("path", OV::str(&resolved))
                    .with("bytes", OV::Int(st.size as i64))
                    .with("mtime_ns", int_ov(st.mtime_ns))
                    .with("timestamp", OV::Null)
                    .with("timestamp_basis", OV::str("unknown"))
                    .with("title", OV::str(&name))
                    .with("format", OV::str(form))
                    .with("manifest", OV::str(state))
                    .with("_guards", guards))
            })();
            match built {
                Ok(row) => rows.push(row),
                Err(Fail::Os(e)) => problem(cov, "read_error", &p, &e.text),
                Err(other) => return Err(other),
            }
        }
        if let Some(listed) = &listed {
            let mut missing: Vec<&String> = listed.keys().filter(|n| !present.contains(*n)).collect();
            missing.sort();
            for name in missing {
                problem(cov, "project_file_missing", &pathx::join(&folder, name), "listed in the project manifest");
            }
        }
    }
    Ok(rows)
}
