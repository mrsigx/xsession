//! Python exception behaviour the CLI reproduces: which `except` clause catches what, the
//! exception class names, and `str(exc)` for OS, Unicode and `int()` errors.

use crate::ojson::{OV, SURROGATE_BASE};
use crate::tables::NONPRINT;

/// How `main()` ends when an exception escapes.
#[derive(Debug)]
pub enum Fail {
    MatchLimit(String),
    Stale(String),
    /// ValueError, KeyError, TypeError and their subclasses: exit 2 with the message.
    Invalid(String),
    /// OSError and subclasses.
    Os(PyOsError),
    /// Any other exception: class name and message.
    Other(String, String),
}

#[derive(Debug, Clone)]
pub struct PyOsError {
    pub class: &'static str,
    pub text: String,
}

impl From<PyOsError> for Fail {
    fn from(e: PyOsError) -> Fail {
        Fail::Os(e)
    }
}

/// Which Python call raised: the built-in `open()` reports C-runtime errno values on Windows,
/// `os.*` calls report the Win32 error.
#[derive(Clone, Copy, PartialEq)]
pub enum Api {
    Open,
    Os,
}

/// A plain `OSError('text')` raised by the tool itself.
pub fn os_text(text: &str) -> PyOsError {
    PyOsError { class: "OSError", text: text.to_string() }
}

/// `str(exc)` and class of the OSError Python raises for this failure.
pub fn os_error(e: &std::io::Error, api: Api, filename: Option<&str>) -> PyOsError {
    let Some(code) = e.raw_os_error() else {
        return os_text(&e.to_string());
    };
    let suffix = filename.map(|f| format!(": {}", repr_str(f))).unwrap_or_default();
    if cfg!(windows) {
        if api == Api::Open {
            let errno = crt_errno(code);
            return PyOsError { class: class_for_errno(errno), text: format!("[Errno {}] {}{}", errno, crt_strerror(errno), suffix) };
        }
        let errno = winerror_to_errno(code);
        return PyOsError { class: class_for_errno(errno), text: format!("[WinError {}] {}{}", code, system_message(e), suffix) };
    }
    PyOsError { class: class_for_errno(code), text: format!("[Errno {}] {}{}", code, system_message(e), suffix) }
}

/// The OS message without Rust's " (os error N)" suffix; on Windows Python also drops
/// trailing whitespace and dots.
fn system_message(e: &std::io::Error) -> String {
    let text = e.to_string();
    let text = match text.rfind(" (os error ") {
        Some(at) => text[..at].to_string(),
        None => text,
    };
    if cfg!(windows) {
        text.trim_end_matches(|c: char| c <= ' ' || c == '.').to_string()
    } else {
        text
    }
}

#[cfg(windows)]
mod errno {
    pub const ENOENT: i32 = 2;
    pub const E2BIG: i32 = 7;
    pub const ENOEXEC: i32 = 8;
    pub const EBADF: i32 = 9;
    pub const ECHILD: i32 = 10;
    pub const EAGAIN: i32 = 11;
    pub const ENOMEM: i32 = 12;
    pub const EACCES: i32 = 13;
    pub const EEXIST: i32 = 17;
    pub const EXDEV: i32 = 18;
    pub const ENOTDIR: i32 = 20;
    pub const EISDIR: i32 = 21;
    pub const EINVAL: i32 = 22;
    pub const EMFILE: i32 = 24;
    pub const ENOSPC: i32 = 28;
    pub const EPIPE: i32 = 32;
    pub const ENOTEMPTY: i32 = 41;
    pub const EILSEQ: i32 = 42;
    pub const EINTR: i32 = 4;
    pub const EPERM: i32 = 1;
    pub const ESRCH: i32 = 3;
    pub const ETIMEDOUT: i32 = 10060;
}

#[cfg(not(windows))]
mod errno {
    pub const ENOENT: i32 = 2;
    pub const EACCES: i32 = 13;
    pub const EEXIST: i32 = 17;
    pub const ENOTDIR: i32 = 20;
    pub const EISDIR: i32 = 21;
    pub const EINTR: i32 = 4;
    pub const EPERM: i32 = 1;
    pub const ESRCH: i32 = 3;
    pub const ECHILD: i32 = 10;
    pub const EAGAIN: i32 = 11;
    pub const EPIPE: i32 = 32;
    pub const ETIMEDOUT: i32 = 110;
}

/// Python's errno → OSError subclass map (the file-relevant part).
fn class_for_errno(n: i32) -> &'static str {
    use errno::*;
    match n {
        ENOENT => "FileNotFoundError",
        EACCES | EPERM => "PermissionError",
        EEXIST => "FileExistsError",
        ENOTDIR => "NotADirectoryError",
        EISDIR => "IsADirectoryError",
        EINTR => "InterruptedError",
        ESRCH => "ProcessLookupError",
        ECHILD => "ChildProcessError",
        EAGAIN => "BlockingIOError",
        EPIPE => "BrokenPipeError",
        ETIMEDOUT => "TimeoutError",
        _ => "OSError",
    }
}

/// CPython `winerror_to_errno` (PC/errmap.h).
#[cfg(windows)]
fn winerror_to_errno(mut code: i32) -> i32 {
    use errno::*;
    if (code as u32 & 0xFFFF_0000) == 0x8007_0000 {
        code &= 0xFFFF;
    }
    if (10000..12000).contains(&code) {
        return match code {
            10004 | 10009 | 10013 | 10014 | 10022 | 10024 => code - 10000,
            _ => code,
        };
    }
    match code {
        2 | 3 | 15 | 18 | 53 | 67 | 161 | 206 => ENOENT,
        10 => E2BIG,
        11 | 188..=202 => ENOEXEC,
        6 | 114 | 130 => EBADF,
        128 | 129 => ECHILD,
        89 | 164 | 215 => EAGAIN,
        7 | 8 | 9 | 1816 => ENOMEM,
        5 | 16 | 19..=36 | 65 | 82 | 83 | 108 | 132 | 158 | 167 => EACCES,
        80 | 183 => EEXIST,
        17 => EXDEV,
        267 => ENOTDIR,
        4 => EMFILE,
        112 => ENOSPC,
        109 | 232 => EPIPE,
        145 => ENOTEMPTY,
        1113 => EILSEQ,
        _ => EINVAL,
    }
}

#[cfg(not(windows))]
fn winerror_to_errno(code: i32) -> i32 {
    code
}

/// The C runtime's `_dosmaperr`, which sets errno for `open()` on Windows.
#[cfg(windows)]
fn crt_errno(code: i32) -> i32 {
    use errno::*;
    match code {
        1 | 12 | 13 | 87 | 131 => EINVAL,
        2 | 3 | 15 | 18 | 53 | 67 | 161 | 206 => ENOENT,
        4 => EMFILE,
        5 | 16 | 33 | 65 | 82 | 83 | 108 | 132 | 158 | 167 => EACCES,
        6 | 114 | 130 => EBADF,
        7 | 8 | 9 | 1816 => ENOMEM,
        10 => E2BIG,
        11 => ENOEXEC,
        17 => EXDEV,
        80 | 183 => EEXIST,
        89 | 164 | 215 => EAGAIN,
        109 => EPIPE,
        112 => ENOSPC,
        128 | 129 => ECHILD,
        145 => ENOTEMPTY,
        1113 => EILSEQ,
        19..=36 => EACCES,
        188..=202 => ENOEXEC,
        _ => EINVAL,
    }
}

#[cfg(not(windows))]
fn crt_errno(code: i32) -> i32 {
    code
}

/// The Microsoft C runtime's `strerror` texts.
#[cfg(windows)]
fn crt_strerror(n: i32) -> &'static str {
    match n {
        1 => "Operation not permitted",
        2 => "No such file or directory",
        3 => "No such process",
        4 => "Interrupted function call",
        5 => "Input/output error",
        6 => "No such device or address",
        7 => "Arg list too long",
        8 => "Exec format error",
        9 => "Bad file descriptor",
        10 => "No child processes",
        11 => "Resource temporarily unavailable",
        12 => "Not enough space",
        13 => "Permission denied",
        14 => "Bad address",
        16 => "Resource device",
        17 => "File exists",
        18 => "Improper link",
        19 => "No such device",
        20 => "Not a directory",
        21 => "Is a directory",
        22 => "Invalid argument",
        23 => "Too many open files in system",
        24 => "Too many open files",
        25 => "Inappropriate I/O control operation",
        27 => "File too large",
        28 => "No space left on device",
        29 => "Invalid seek",
        30 => "Read-only file system",
        31 => "Too many links",
        32 => "Broken pipe",
        33 => "Domain error",
        34 => "Result too large",
        36 => "Resource deadlock avoided",
        38 => "Filename too long",
        39 => "No locks available",
        40 => "Function not implemented",
        41 => "Directory not empty",
        42 => "Illegal byte sequence",
        _ => "Unknown error",
    }
}

#[cfg(not(windows))]
fn crt_strerror(_n: i32) -> &'static str {
    "Unknown error"
}

// ------------------------------------------------------------------------------ repr / types ---

fn printable(c: char) -> bool {
    let code = c as u32;
    if code >= SURROGATE_BASE {
        return false;
    }
    NONPRINT.binary_search_by(|&(lo, hi)| {
        if hi < code {
            std::cmp::Ordering::Less
        } else if lo > code {
            std::cmp::Ordering::Greater
        } else {
            std::cmp::Ordering::Equal
        }
    }).is_err()
}

/// Python `repr(str)`.
pub fn repr_str(s: &str) -> String {
    let quote = if s.contains('\'') && !s.contains('"') { '"' } else { '\'' };
    let mut out = String::with_capacity(s.len() + 2);
    out.push(quote);
    for c in s.chars() {
        let code = c as u32;
        match c {
            '\\' => out.push_str("\\\\"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            _ if c == quote => {
                out.push('\\');
                out.push(c);
            }
            _ if code >= SURROGATE_BASE => out.push_str(&format!("\\u{:04x}", 0xD800 + (code - SURROGATE_BASE))),
            _ if code < 0x20 || code == 0x7f => out.push_str(&format!("\\x{:02x}", code)),
            _ if code < 0x7f => out.push(c),
            _ if printable(c) => out.push(c),
            _ if code <= 0xff => out.push_str(&format!("\\x{:02x}", code)),
            _ if code <= 0xffff => out.push_str(&format!("\\u{:04x}", code)),
            _ => out.push_str(&format!("\\U{:08x}", code)),
        }
    }
    out.push(quote);
    out
}

/// `type(value).__name__` for a decoded JSON value.
pub fn type_name(v: &OV) -> &'static str {
    match v {
        OV::Null => "NoneType",
        OV::Bool(_) => "bool",
        OV::Int(_) | OV::UInt(_) | OV::Big(_) => "int",
        OV::Float(_) => "float",
        OV::Str(_) => "str",
        OV::Arr(_) => "list",
        OV::Obj(_) => "dict",
    }
}

// ------------------------------------------------------------------------------------ Unicode ---

/// Strict UTF-8 decoding with CPython's `UnicodeDecodeError` text on failure.
pub fn decode_utf8_strict(b: &[u8]) -> Result<&str, String> {
    match std::str::from_utf8(b) {
        Ok(s) => Ok(s),
        Err(e) => {
            let start = e.valid_up_to();
            let (end, reason) = match e.error_len() {
                None => (b.len(), "unexpected end of data"),
                Some(n) => {
                    let lead = b[start];
                    let invalid_start = (0x80..=0xC1).contains(&lead) || lead >= 0xF5;
                    (start + n, if invalid_start { "invalid start byte" } else { "invalid continuation byte" })
                }
            };
            Err(if end == start + 1 {
                format!("'utf-8' codec can't decode byte 0x{:02x} in position {}: {}", b[start], start, reason)
            } else {
                format!("'utf-8' codec can't decode bytes in position {}-{}: {}", start, end - 1, reason)
            })
        }
    }
}

/// `bytes.decode('utf-8-sig')`: one leading BOM dropped, positions relative to what follows it.
pub fn decode_utf8_sig(b: &[u8]) -> Result<&str, String> {
    decode_utf8_strict(b.strip_prefix(b"\xef\xbb\xbf").unwrap_or(b))
}

/// Text-mode reading: universal newlines turn `\r\n` and `\r` into `\n`.
pub fn universal_newlines(s: &str) -> String {
    if !s.contains('\r') {
        return s.to_string();
    }
    s.replace("\r\n", "\n").replace('\r', "\n")
}

/// `str(UnicodeEncodeError)` for encoding `s` as strict UTF-8, if it holds lone surrogates.
pub fn utf8_encode_error(s: &str) -> Option<String> {
    let mut chars = s.chars().enumerate().peekable();
    while let Some((i, c)) = chars.next() {
        if (c as u32) < SURROGATE_BASE {
            continue;
        }
        let mut end = i + 1;
        while matches!(chars.peek(), Some((_, d)) if (*d as u32) >= SURROGATE_BASE) {
            chars.next();
            end += 1;
        }
        return Some(if end == i + 1 {
            format!("'utf-8' codec can't encode character '\\u{:04x}' in position {}: surrogates not allowed",
                    0xD800 + (c as u32 - SURROGATE_BASE), i)
        } else {
            format!("'utf-8' codec can't encode characters in position {}-{}: surrogates not allowed", i, end - 1)
        });
    }
    None
}

/// Python `int(text)` for environment values: whitespace, sign, digits and single underscores.
/// Values beyond 64 bits saturate (they only ever fail later range checks).
pub fn py_int(text: &str) -> Result<i128, String> {
    let fail = || format!("invalid literal for int() with base 10: {}", repr_str(text));
    let body = crate::pytext::strip(text);
    let (negative, digits) = match body.as_bytes().first() {
        Some(b'-') => (true, &body[1..]),
        Some(b'+') => (false, &body[1..]),
        _ => (false, body),
    };
    if digits.is_empty() || digits.starts_with('_') || digits.ends_with('_') || digits.contains("__") {
        return Err(fail());
    }
    let mut value: i128 = 0;
    for c in digits.chars() {
        if c == '_' {
            continue;
        }
        let d = c.to_digit(10).filter(|_| c.is_ascii_digit()).ok_or_else(fail)?;
        value = value.saturating_mul(10).saturating_add(d as i128);
    }
    Ok(if negative { -value } else { value })
}
