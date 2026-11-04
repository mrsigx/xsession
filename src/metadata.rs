//! 2.3.0 `read_metadata`: Codex thread titles and the external-agent import registry.

use crate::hash::sha256_hex;
use crate::inventory::{problem, uuid_findall};
use crate::ojson::{self, Obj, OV};
use crate::pathx;
use crate::pyerr::{type_name, Fail};
use crate::pyio::{self, TextError};
use crate::pyobj::{dict_key_error, set_element_error, unhashable, PyDict};
use std::io::{BufRead, BufReader};

/// `Path(value)` for a configured metadata path; Python raises TypeError for non-strings.
fn config_path(value: &OV) -> Result<String, Fail> {
    match value {
        OV::Str(s) => Ok(pathx::norm(s)),
        other => Err(Fail::Invalid(format!(
            "argument should be a str or an os.PathLike object where __fspath__ returns a str, not '{}'",
            type_name(other)
        ))),
    }
}

fn read_titles(p: &str, titles: &mut PyDict<Obj>, cov: &mut Obj) -> Result<(), Fail> {
    let file = match pyio::open(p) {
        Ok(f) => f,
        Err(e) => {
            problem(cov, "metadata_unavailable", p, &e.text);
            return Ok(());
        }
    };
    let mut reader = BufReader::new(file);
    let mut raw = Vec::new();
    let mut offset: u64 = 0;
    let mut number = 0u64;
    loop {
        raw.clear();
        match reader.read_until(b'\n', &mut raw) {
            Ok(0) => break,
            Ok(_) => {}
            Err(e) => {
                problem(cov, "metadata_unavailable", p, &pyio::read_err(&e).text);
                return Ok(());
            }
        }
        number += 1;
        match ojson::loads_bytes(&raw) {
            Ok(OV::Obj(e)) => {
                let id = e.get("id").filter(|v| v.truthy());
                let name = e.get("thread_name").filter(|v| v.truthy());
                if let (Some(id), Some(name)) = (id, name) {
                    if unhashable(id) {
                        return Err(dict_key_error(id));
                    }
                    titles.insert(id.clone(), Obj::new()
                        .with("text", name.clone())
                        .with("path", OV::str(p))
                        .with("line", OV::Int(number as i64))
                        .with("byte_offset", OV::Int(offset as i64))
                        .with("record_sha256", OV::Str(sha256_hex(&raw))));
                }
            }
            // Not an object (AttributeError) or not JSON (ValueError): malformed.
            _ => problem(cov, "metadata_malformed", p, &format!("line {}", number)),
        }
        offset += raw.len() as u64;
    }
    Ok(())
}

/// The import registry, or the `str(exc)` 2.3.0 records as `metadata_unavailable`.
fn read_imports(p: &str) -> Result<PyDict<Obj>, String> {
    let text = match pyio::read_text_sig(p) {
        Ok(t) => t,
        Err(TextError::Os(e)) => return Err(e.text),
        Err(TextError::Decode(m)) => return Err(m),
    };
    let data = ojson::loads(&text)?;
    let OV::Obj(data) = data else {
        return Err(format!("'{}' object has no attribute 'get'", type_name(&data)));
    };
    let empty = OV::Arr(Vec::new());
    let records = data.get("records").unwrap_or(&empty);
    let items: Vec<OV> = match records {
        OV::Arr(a) => a.clone(),
        OV::Obj(o) => o.0.iter().map(|(k, _)| OV::Str(k.clone())).collect(),
        OV::Str(s) => s.chars().map(|c| OV::Str(c.to_string())).collect(),
        other => return Err(format!("'{}' object is not iterable", type_name(other))),
    };
    let mut imports = PyDict::new();
    for r in items {
        let OV::Obj(r) = r else {
            return Err(format!("'{}' object has no attribute 'get'", type_name(&r)));
        };
        if let Some(key) = r.get("imported_thread_id").filter(|v| v.truthy()) {
            if unhashable(key) {
                let t = type_name(key);
                return Err(format!("cannot use '{}' as a dict key (unhashable type: '{}')", t, t));
            }
            imports.insert(key.clone(), r.clone());
        }
    }
    Ok(imports)
}

/// `read_metadata(config, rows, coverage)`: returns the titles.
pub fn read_metadata(config: &Obj, rows: &mut [Obj], cov: &mut Obj) -> Result<PyDict<Obj>, Fail> {
    let mut titles = PyDict::new();
    let mut imports = PyDict::new();
    if let Some(value) = config.get("title_index").filter(|v| v.truthy()) {
        let p = config_path(value)?;
        read_titles(&p, &mut titles, cov)?;
    }
    if let Some(value) = config.get("imports").filter(|v| v.truthy()) {
        let p = config_path(value)?;
        match read_imports(&p) {
            Ok(found) => imports = found,
            Err(detail) => problem(cov, "metadata_unavailable", &p, &detail),
        }
    }
    let mut original_ids: PyDict<()> = PyDict::new();
    for r in rows.iter() {
        if r.str_of("agent") == Some("claude") {
            let id = r.get("id").cloned().unwrap_or(OV::Null);
            if unhashable(&id) {
                return Err(set_element_error(&id));
            }
            original_ids.insert(id, ());
        }
    }
    for row in rows.iter_mut() {
        let id = row.get("id").cloned().unwrap_or(OV::Null);
        if unhashable(&id) {
            return Err(dict_key_error(&id));
        }
        if let Some(title) = titles.get(&id) {
            row.set("title", title.get("text").cloned().unwrap_or(OV::Null));
        }
        if let Some(record) = imports.get(&id) {
            row.set("kind", OV::str("import"));
            let source_path = match record.get("source_path") {
                None => String::new(),
                Some(OV::Str(s)) => s.clone(),
                Some(other) => {
                    return Err(Fail::Invalid(format!("expected string or bytes-like object, got '{}'", type_name(other))))
                }
            };
            let original = uuid_findall(&source_path).pop().map_or(OV::Null, OV::Str);
            let available = original_ids.contains(&original);
            row.set("original_id", original);
            row.set("original_available", OV::Bool(available));
        }
    }
    Ok(titles)
}
