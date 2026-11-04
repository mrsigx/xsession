//! The `pathlib` behaviour 2.3.0 relies on, on path strings: `str(Path(x))` normalisation,
//! `/` joins, `name`/`stem`/`suffix`/`parts`/`parent`, `relative_to`, `is_relative_to`,
//! `expanduser`, `resolve`, and OS string conversion (lone surrogates carried as in `ojson`).

use crate::ojson::SURROGATE_BASE;
use crate::pyerr::{os_error, Api, Fail, PyOsError};
use std::ffi::{OsStr, OsString};
use std::path::PathBuf;

#[cfg(windows)]
pub const SEP: &str = "\\";
#[cfg(not(windows))]
pub const SEP: &str = "/";

// ------------------------------------------------------------------------- OS string bridge ---

/// An OS path as Python sees it: lone UTF-16 surrogates (Windows) or `surrogateescape` bytes
/// (POSIX) become the plane-16 code points `ojson` uses for lone surrogates.
pub fn from_os(os: &OsStr) -> String {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        let mut out = String::new();
        for r in char::decode_utf16(os.encode_wide()) {
            match r {
                Ok(c) => out.push(c),
                Err(e) => out.push(char::from_u32(SURROGATE_BASE + (e.unpaired_surrogate() as u32 - 0xD800)).unwrap()),
            }
        }
        out
    }
    #[cfg(not(windows))]
    {
        use std::os::unix::ffi::OsStrExt;
        let b = os.as_bytes();
        match std::str::from_utf8(b) {
            Ok(s) => s.to_string(),
            Err(_) => {
                let mut out = String::new();
                let mut i = 0;
                while i < b.len() {
                    match std::str::from_utf8(&b[i..]) {
                        Ok(s) => {
                            out.push_str(s);
                            break;
                        }
                        Err(e) => {
                            let good = e.valid_up_to();
                            out.push_str(std::str::from_utf8(&b[i..i + good]).unwrap());
                            i += good;
                            let bad = e.error_len().unwrap_or(b.len() - i);
                            for &byte in &b[i..i + bad] {
                                out.push(char::from_u32(SURROGATE_BASE + (0xDC00 + byte as u32 - 0xD800)).unwrap());
                            }
                            i += bad;
                        }
                    }
                }
                out
            }
        }
    }
}

pub fn to_os(s: &str) -> OsString {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStringExt;
        let mut wide: Vec<u16> = Vec::with_capacity(s.len());
        for c in s.chars() {
            let code = c as u32;
            if code >= SURROGATE_BASE {
                wide.push((0xD800 + code - SURROGATE_BASE) as u16);
            } else {
                let mut buf = [0u16; 2];
                wide.extend_from_slice(c.encode_utf16(&mut buf));
            }
        }
        OsString::from_wide(&wide)
    }
    #[cfg(not(windows))]
    {
        use std::os::unix::ffi::OsStringExt;
        let mut bytes = Vec::with_capacity(s.len());
        for c in s.chars() {
            let code = c as u32;
            if code >= SURROGATE_BASE {
                let unit = 0xD800 + code - SURROGATE_BASE;
                bytes.push(if (0xDC80..=0xDCFF).contains(&unit) { (unit - 0xDC00) as u8 } else { b'?' });
            } else {
                let mut buf = [0u8; 4];
                bytes.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            }
        }
        OsString::from_vec(bytes)
    }
}

pub fn to_path(s: &str) -> PathBuf {
    PathBuf::from(to_os(s))
}

// ------------------------------------------------------------------------------- parsing ---

/// `parser.splitroot` (ntpath or posixpath).
pub fn splitroot(p: &str) -> (String, String, String) {
    #[cfg(windows)]
    {
        let normp = p.replace('/', "\\");
        let chars: Vec<char> = normp.chars().collect();
        let byte_at = |n: usize| -> usize { normp.char_indices().nth(n).map(|(i, _)| i).unwrap_or(normp.len()) };
        if chars.first() == Some(&'\\') {
            if chars.get(1) == Some(&'\\') {
                let start = if chars.len() >= 8 && chars[..8].iter().collect::<String>().to_uppercase() == "\\\\?\\UNC\\" { 8 } else { 2 };
                let find = |from: usize| chars.iter().skip(from).position(|c| *c == '\\').map(|i| i + from);
                let Some(index) = find(start) else { return (p.to_string(), String::new(), String::new()) };
                let Some(index2) = find(index + 1) else { return (p.to_string(), String::new(), String::new()) };
                let (a, b) = (byte_at(index2), byte_at(index2 + 1));
                return (p[..a].to_string(), p[a..b].to_string(), p[b..].to_string());
            }
            let b = byte_at(1);
            return (String::new(), p[..b].to_string(), p[b..].to_string());
        }
        if chars.get(1) == Some(&':') {
            let two = byte_at(2);
            if chars.get(2) == Some(&'\\') {
                let three = byte_at(3);
                return (p[..two].to_string(), p[two..three].to_string(), p[three..].to_string());
            }
            return (p[..two].to_string(), String::new(), p[two..].to_string());
        }
        (String::new(), String::new(), p.to_string())
    }
    #[cfg(not(windows))]
    {
        if !p.starts_with('/') {
            (String::new(), String::new(), p.to_string())
        } else if !p[1..].starts_with('/') || p[2..].starts_with('/') {
            (String::new(), "/".to_string(), p[1..].to_string())
        } else {
            (String::new(), p[..2].to_string(), p[2..].to_string())
        }
    }
}

/// `PurePath._parse_path`: (drive, root, tail parts).
pub fn parse(path: &str) -> (String, String, Vec<String>) {
    if path.is_empty() {
        return (String::new(), String::new(), Vec::new());
    }
    #[cfg(windows)]
    let path = path.replace('/', "\\");
    #[cfg(not(windows))]
    let path = path.to_string();
    let (drv, mut root, rel) = splitroot(&path);
    #[cfg(windows)]
    {
        if root.is_empty() && drv.starts_with('\\') && !drv.ends_with('\\') {
            let drv_parts: Vec<&str> = drv.split('\\').collect();
            // Python tests `drv_parts[2] not in '?.'`, a substring test.
            if (drv_parts.len() == 4 && !"?.".contains(drv_parts[2])) || drv_parts.len() == 6 {
                root = "\\".to_string();
            }
        }
    }
    #[cfg(not(windows))]
    let _ = &mut root;
    let tail = rel.split(SEP).filter(|x| !x.is_empty() && *x != ".").map(str::to_string).collect();
    (drv, root, tail)
}

/// `_format_parsed_parts`.
pub fn format(drv: &str, root: &str, tail: &[String]) -> String {
    if !drv.is_empty() || !root.is_empty() {
        return format!("{}{}{}", drv, root, tail.join(SEP));
    }
    if let Some(first) = tail.first() {
        if !splitroot(first).0.is_empty() {
            return format!(".{}{}", SEP, tail.join(SEP));
        }
    }
    if tail.is_empty() {
        ".".to_string()
    } else {
        tail.join(SEP)
    }
}

/// `str(Path(s))`.
pub fn norm(s: &str) -> String {
    let (d, r, t) = parse(s);
    format(&d, &r, &t)
}

/// `os.path.join(a, b)` (unnormalised).
pub fn os_join(a: &str, b: &str) -> String {
    #[cfg(windows)]
    {
        let (mut rd, mut rr, mut rp) = splitroot(a);
        let (pd, pr, pp) = splitroot(b);
        if !pr.is_empty() {
            if !pd.is_empty() || rd.is_empty() {
                rd = pd;
            }
            rr = pr;
            rp = pp;
        } else {
            let mut relative = true;
            if !pd.is_empty() && pd != rd {
                if pd.to_lowercase() != rd.to_lowercase() {
                    rd = pd;
                    rr = pr;
                    rp = pp.clone();
                    relative = false;
                } else {
                    rd = pd;
                }
            }
            if relative {
                if !rp.is_empty() && !rp.ends_with(['\\', '/']) {
                    rp.push('\\');
                }
                rp.push_str(&pp);
            }
        }
        if !rp.is_empty() && rr.is_empty() && !rd.is_empty() && !rd.ends_with([':', '\\', '/']) {
            return format!("{}\\{}", rd, rp);
        }
        format!("{}{}{}", rd, rr, rp)
    }
    #[cfg(not(windows))]
    {
        if b.starts_with('/') {
            b.to_string()
        } else if a.is_empty() || a.ends_with('/') {
            format!("{}{}", a, b)
        } else {
            format!("{}/{}", a, b)
        }
    }
}

/// `str(Path(a) / b)`.
pub fn join(a: &str, b: &str) -> String {
    norm(&os_join(a, b))
}

pub fn join_parts(a: &str, parts: &[String]) -> String {
    let mut out = norm(a);
    for p in parts {
        out = join(&out, p);
    }
    out
}

/// `Path(s).name`.
pub fn name(s: &str) -> String {
    parse(s).2.pop().unwrap_or_default()
}

/// `genericpath._splitext` applied to a name: (stem, suffix).
pub fn splitext(name: &str) -> (String, String) {
    if let Some(dot) = name.rfind('.') {
        if name[..dot].chars().any(|c| c != '.') {
            return (name[..dot].to_string(), name[dot..].to_string());
        }
    }
    (name.to_string(), String::new())
}

pub fn stem(s: &str) -> String {
    splitext(&name(s)).0
}

pub fn suffix(s: &str) -> String {
    splitext(&name(s)).1
}

/// `Path(s).parts`.
pub fn parts(s: &str) -> Vec<String> {
    let (d, r, t) = parse(s);
    let mut out = Vec::new();
    if !d.is_empty() || !r.is_empty() {
        out.push(format!("{}{}", d, r));
    }
    out.extend(t);
    out
}

/// `str(Path(s).parent)`.
pub fn parent(s: &str) -> String {
    let (d, r, mut t) = parse(s);
    t.pop();
    format(&d, &r, &t)
}

fn normcase(s: &str) -> String {
    if cfg!(windows) {
        s.to_lowercase()
    } else {
        s.to_string()
    }
}

/// `Path(p).is_relative_to(base)`.
pub fn is_relative_to(p: &str, base: &str) -> bool {
    let (pd, pr, pt) = parse(p);
    let (bd, br, bt) = parse(base);
    if normcase(&format!("{}{}", pd, pr)) != normcase(&format!("{}{}", bd, br)) || bt.len() > pt.len() {
        return false;
    }
    normcase(&format(&pd, &pr, &pt[..bt.len()].to_vec())) == normcase(&format(&bd, &br, &bt))
}

/// Tail parts of `p` below `base` (both normalised; `p` is known to be inside `base`).
pub fn relative_parts(p: &str, base: &str) -> Vec<String> {
    let pt = parse(p).2;
    let bt = parse(base).2;
    pt[bt.len().min(pt.len())..].to_vec()
}

/// `Path(...).as_posix()` of relative parts.
pub fn as_posix(parts: &[String]) -> String {
    let s = format("", "", parts);
    if cfg!(windows) {
        s.replace('\\', "/")
    } else {
        s
    }
}

// ------------------------------------------------------------------------- home / expanduser ---

fn env(name: &str) -> Option<String> {
    std::env::var_os(name).map(|v| from_os(&v))
}

/// `os.path.expanduser` of a single `~` or `~user` component; unchanged when it cannot.
fn expand_tilde(first: &str) -> String {
    #[cfg(windows)]
    {
        let userhome = match env("USERPROFILE") {
            Some(h) => h,
            None => match env("HOMEPATH") {
                None => return first.to_string(),
                Some(hp) => os_join(&env("HOMEDRIVE").unwrap_or_default(), &hp),
            },
        };
        let mut userhome = userhome;
        if first.len() > 1 {
            let target = &first[1..];
            let current = env("USERNAME");
            if current.as_deref() != Some(target) {
                let base = os_basename(&userhome);
                if current.as_deref() != Some(base.as_str()) {
                    return first.to_string();
                }
                userhome = os_join(&os_dirname(&userhome), target);
            }
        }
        userhome
    }
    #[cfg(not(windows))]
    {
        let home = if first == "~" {
            match env("HOME") {
                Some(h) => Some(h),
                None => posix_home(None),
            }
        } else {
            posix_home(Some(&first[1..]))
        };
        match home {
            None => first.to_string(),
            Some(h) => {
                let h = h.trim_end_matches('/').to_string();
                if h.is_empty() {
                    "/".to_string()
                } else {
                    h
                }
            }
        }
    }
}

#[cfg(windows)]
fn os_split(p: &str) -> (String, String) {
    let (d, r, rest) = splitroot(p);
    let cut = rest.rfind(['\\', '/']).map(|i| i + 1).unwrap_or(0);
    let head = rest[..cut].trim_end_matches(['\\', '/']).to_string();
    (format!("{}{}{}", d, r, head), rest[cut..].to_string())
}

#[cfg(windows)]
fn os_basename(p: &str) -> String {
    os_split(p).1
}

#[cfg(windows)]
fn os_dirname(p: &str) -> String {
    os_split(p).0
}

#[cfg(not(windows))]
fn posix_home(user: Option<&str>) -> Option<String> {
    use std::ffi::{CStr, CString};
    #[repr(C)]
    struct Passwd {
        pw_name: *mut i8,
        pw_passwd: *mut i8,
        pw_uid: u32,
        pw_gid: u32,
        pw_gecos: *mut i8,
        pw_dir: *mut i8,
        pw_shell: *mut i8,
    }
    extern "C" {
        fn getpwnam(name: *const i8) -> *mut Passwd;
        fn getpwuid(uid: u32) -> *mut Passwd;
        fn getuid() -> u32;
    }
    // SAFETY: libc lookups; the returned record is read immediately.
    unsafe {
        let entry = match user {
            Some(name) => {
                let c = CString::new(name).ok()?;
                getpwnam(c.as_ptr())
            }
            None => getpwuid(getuid()),
        };
        if entry.is_null() || (*entry).pw_dir.is_null() {
            return None;
        }
        Some(CStr::from_ptr((*entry).pw_dir).to_string_lossy().into_owned())
    }
}

/// `Path.home()`.
pub fn home() -> Result<String, Fail> {
    let h = expand_tilde("~");
    if h == "~" {
        return Err(Fail::Other("RuntimeError".into(), "Could not determine home directory.".into()));
    }
    Ok(norm(&h))
}

/// `str(Path(s).expanduser())`.
pub fn expanduser(s: &str) -> Result<String, Fail> {
    let (d, r, t) = parse(s);
    if d.is_empty() && r.is_empty() && t.first().is_some_and(|f| f.starts_with('~')) {
        let homedir = expand_tilde(&t[0]);
        if homedir.starts_with('~') {
            return Err(Fail::Other("RuntimeError".into(), "Could not determine home directory.".into()));
        }
        let (hd, hr, mut ht) = parse(&homedir);
        ht.extend(t[1..].iter().cloned());
        return Ok(format(&hd, &hr, &ht));
    }
    Ok(format(&d, &r, &t))
}

// ------------------------------------------------------------------------------------ resolve ---

fn cwd() -> Result<String, PyOsError> {
    std::env::current_dir().map(|p| from_os(p.as_os_str())).map_err(|e| os_error(&e, Api::Os, None))
}

/// `str(Path(s).resolve())` (`os.path.realpath(strict=False)`).
pub fn resolve(s: &str) -> Result<String, PyOsError> {
    let path = norm(s);
    #[cfg(windows)]
    {
        const PREFIX: &str = "\\\\?\\";
        const UNC: &str = "\\\\?\\UNC\\";
        let path = normpath(&path);
        let had_prefix = path.starts_with(PREFIX);
        let head: String = path.chars().take(3).collect::<String>().replace('/', "\\");
        let absolute = head.get(1..).is_some_and(|h| h.starts_with(":\\")) || head.starts_with("\\\\");
        let mut path = if !had_prefix && !absolute { os_join(&cwd()?, &path) } else { path };
        let initial;
        match std::fs::canonicalize(to_path(&path)) {
            Ok(p) => {
                path = from_os(p.as_os_str());
                initial = 0;
            }
            Err(e) => {
                initial = e.raw_os_error().unwrap_or(-1);
                path = final_nonstrict(&path)?;
            }
        }
        if !had_prefix && path.starts_with(PREFIX) {
            let spath = if path.starts_with(UNC) { format!("\\\\{}", &path[UNC.len()..]) } else { path[PREFIX.len()..].to_string() };
            match std::fs::canonicalize(to_path(&spath)) {
                Ok(p) => {
                    if from_os(p.as_os_str()) == path {
                        path = spath;
                    }
                }
                Err(e) => {
                    if e.raw_os_error().unwrap_or(-1) == initial {
                        path = spath;
                    }
                }
            }
        }
        Ok(norm(&path))
    }
    #[cfg(not(windows))]
    {
        posix_realpath(&path)
    }
}

/// `ntpath.normpath`.
#[cfg(windows)]
fn normpath(path: &str) -> String {
    let path = path.replace('/', "\\");
    let (drive, root, rest) = splitroot(&path);
    let mut comps: Vec<&str> = rest.split('\\').collect();
    let mut i = 0;
    while i < comps.len() {
        if comps[i].is_empty() || comps[i] == "." {
            comps.remove(i);
        } else if comps[i] == ".." {
            if i > 0 && comps[i - 1] != ".." {
                comps.drain(i - 1..=i);
                i -= 1;
            } else if i == 0 && !root.is_empty() {
                comps.remove(i);
            } else {
                i += 1;
            }
        } else {
            i += 1;
        }
    }
    let prefix = format!("{}{}", drive, root);
    if prefix.is_empty() && comps.is_empty() {
        return ".".to_string();
    }
    format!("{}{}", prefix, comps.join("\\"))
}

/// `ntpath._getfinalpathname_nonstrict`, without following unresolvable links by hand.
#[cfg(windows)]
fn final_nonstrict(path: &str) -> Result<String, PyOsError> {
    const ALLOWED: [i32; 16] = [1, 2, 3, 5, 21, 32, 50, 53, 65, 67, 87, 123, 161, 1005, 1920, 1921];
    let mut path = path.to_string();
    let mut tail = String::new();
    while !path.is_empty() {
        match std::fs::canonicalize(to_path(&path)) {
            Ok(p) => {
                let p = from_os(p.as_os_str());
                return Ok(if tail.is_empty() { p } else { os_join(&p, &tail) });
            }
            Err(e) => {
                if !ALLOWED.contains(&e.raw_os_error().unwrap_or(-1)) {
                    return Err(os_error(&e, Api::Os, Some(&path)));
                }
                let (head, name) = os_split(&path);
                if !head.is_empty() && name.is_empty() {
                    return Ok(format!("{}{}", head, tail));
                }
                tail = if tail.is_empty() { name } else { os_join(&name, &tail) };
                path = head;
            }
        }
    }
    Ok(tail)
}

/// `posixpath.realpath(strict=False)`.
#[cfg(not(windows))]
fn posix_realpath(filename: &str) -> Result<String, PyOsError> {
    use std::collections::HashMap;
    enum Item {
        Part(String),
        Mark(String),
    }
    let mut rest: Vec<Item> = filename.trim_end_matches('/').split('/').rev().map(|s| Item::Part(s.to_string())).collect();
    let mut path = if filename.starts_with('/') { "/".to_string() } else { cwd()? };
    let mut seen: HashMap<String, Option<String>> = HashMap::new();
    while let Some(item) = rest.pop() {
        let name = match item {
            Item::Mark(link) => {
                seen.insert(link, Some(path.clone()));
                continue;
            }
            Item::Part(name) => name,
        };
        if name.is_empty() || name == "." {
            continue;
        }
        if name == ".." {
            path = match path.rfind('/') {
                Some(0) | None => "/".to_string(),
                Some(i) => path[..i].to_string(),
            };
            continue;
        }
        let newpath = if path == "/" { format!("/{}", name) } else { format!("{}/{}", path, name) };
        let meta = match std::fs::symlink_metadata(to_path(&newpath)) {
            Ok(m) => m,
            Err(_) => {
                path = newpath;
                continue;
            }
        };
        if !meta.file_type().is_symlink() {
            path = newpath;
            continue;
        }
        if let Some(cached) = seen.get(&newpath) {
            match cached {
                Some(resolved) => {
                    path = resolved.clone();
                    continue;
                }
                None => {
                    path = newpath;
                    continue;
                }
            }
        }
        let target = match std::fs::read_link(to_path(&newpath)) {
            Ok(t) => from_os(t.as_os_str()),
            Err(_) => {
                path = newpath;
                continue;
            }
        };
        if target.starts_with('/') {
            path = "/".to_string();
        }
        seen.insert(newpath.clone(), None);
        rest.push(Item::Mark(newpath));
        for part in target.split('/').rev() {
            rest.push(Item::Part(part.to_string()));
        }
    }
    Ok(path)
}

/// `sorted(paths)` for `Path` objects: by the case-normalised parts (case-insensitive on Windows).
pub fn sort_paths(paths: &mut [String]) {
    paths.sort_by_cached_key(|p| normcase(&norm(p)).split(SEP).map(str::to_string).collect::<Vec<String>>());
}
