//! Filesystem calls as 2.3.0 makes them (`os.stat`, `open(..., 'rb')`, `os.scandir`, `os.walk`,
//! `Path.is_*`), with the error texts Python would produce.

use crate::hash::sha256_hex;
use crate::ojson::OV;
use crate::pathx::{from_os, os_join, to_path};
use crate::pyerr::{decode_utf8_sig, os_error, universal_newlines, Api, Fail, PyOsError};
use std::fs::File;
use std::io::{self, BufRead, Read, Seek, SeekFrom};

pub struct Stat {
    pub size: u64,
    pub mtime_ns: i128,
}

fn mtime_ns(m: &std::fs::Metadata) -> i128 {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        (m.last_write_time() as i128 - 116_444_736_000_000_000) * 100
    }
    #[cfg(not(windows))]
    {
        use std::os::unix::fs::MetadataExt;
        m.mtime() as i128 * 1_000_000_000 + m.mtime_nsec() as i128
    }
}

/// `os.stat(path)` (follows links).
pub fn stat(path: &str) -> Result<Stat, PyOsError> {
    let m = std::fs::metadata(to_path(path)).map_err(|e| os_error(&e, Api::Os, Some(path)))?;
    Ok(Stat { size: m.len(), mtime_ns: mtime_ns(&m) })
}

/// `open(path, 'rb')`.
pub fn open(path: &str) -> Result<File, PyOsError> {
    let f = File::open(to_path(path)).map_err(|e| os_error(&e, Api::Open, Some(path)))?;
    #[cfg(not(windows))]
    {
        if f.metadata().map(|m| m.is_dir()).unwrap_or(false) {
            let e = io::Error::from_raw_os_error(21);
            return Err(os_error(&e, Api::Open, Some(path)));
        }
    }
    Ok(f)
}

pub fn read_err(e: &io::Error) -> PyOsError {
    os_error(e, Api::Open, None)
}

/// `Path(path).read_bytes()`.
pub fn read_bytes(path: &str) -> Result<Vec<u8>, PyOsError> {
    let mut f = open(path)?;
    let mut out = Vec::new();
    f.read_to_end(&mut out).map_err(|e| read_err(&e))?;
    Ok(out)
}

pub enum TextError {
    Os(PyOsError),
    /// UnicodeDecodeError text (a ValueError).
    Decode(String),
}

/// `Path(path).read_text(encoding='utf-8-sig')`, universal newlines included.
pub fn read_text_sig(path: &str) -> Result<String, TextError> {
    let raw = read_bytes(path).map_err(TextError::Os)?;
    let text = decode_utf8_sig(&raw).map_err(TextError::Decode)?;
    Ok(universal_newlines(text))
}

pub fn is_dir(path: &str) -> bool {
    std::fs::metadata(to_path(path)).map(|m| m.is_dir()).unwrap_or(false)
}

pub fn is_file(path: &str) -> bool {
    std::fs::metadata(to_path(path)).map(|m| m.is_file()).unwrap_or(false)
}

#[cfg(windows)]
const TAG_SYMLINK: u32 = 0xA000_000C;
#[cfg(windows)]
const TAG_MOUNT_POINT: u32 = 0xA000_0003;

/// The reparse tag `os.lstat` reports (0 for an ordinary file); None when it cannot be read.
#[cfg(windows)]
fn reparse_tag(path: &str) -> Option<u32> {
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    #[repr(C)]
    struct TagInfo {
        attributes: u32,
        tag: u32,
    }
    extern "system" {
        fn GetFileInformationByHandleEx(h: *mut std::ffi::c_void, class: i32, info: *mut std::ffi::c_void, size: u32) -> i32;
    }
    const FILE_READ_ATTRIBUTES: u32 = 0x80;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    const FILE_ATTRIBUTE_TAG_INFO: i32 = 9;
    let f = std::fs::OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS)
        .open(to_path(path))
        .ok()?;
    let mut info = TagInfo { attributes: 0, tag: 0 };
    // SAFETY: valid handle and an output buffer of the documented size.
    let ok = unsafe {
        GetFileInformationByHandleEx(f.as_raw_handle() as *mut _, FILE_ATTRIBUTE_TAG_INFO, &mut info as *mut TagInfo as *mut _, 8)
    };
    if ok == 0 {
        return None;
    }
    Some(if info.attributes & 0x400 == 0 { 0 } else { info.tag })
}

/// `Path(path).is_symlink()`.
pub fn is_symlink(path: &str) -> bool {
    #[cfg(windows)]
    {
        reparse_tag(path) == Some(TAG_SYMLINK)
    }
    #[cfg(not(windows))]
    {
        std::fs::symlink_metadata(to_path(path)).map(|m| m.file_type().is_symlink()).unwrap_or(false)
    }
}

pub fn is_link_or_junction(path: &str) -> bool {
    #[cfg(windows)]
    {
        matches!(reparse_tag(path), Some(TAG_SYMLINK) | Some(TAG_MOUNT_POINT))
    }
    #[cfg(not(windows))]
    {
        is_symlink(path)
    }
}

pub struct Entry {
    pub name: String,
    /// `DirEntry.is_dir()`: follows true symlinks; junctions count as directories.
    pub is_dir: bool,
}

/// `os.scandir(path)` in listing order.
pub fn scandir(path: &str) -> Result<Vec<Entry>, PyOsError> {
    let fail = |e: io::Error| os_error(&e, Api::Os, Some(path));
    let mut out = Vec::new();
    for entry in std::fs::read_dir(to_path(path)).map_err(fail)? {
        let entry = entry.map_err(fail)?;
        let name = from_os(&entry.file_name());
        let full = os_join(path, &name);
        #[cfg(windows)]
        let is_dir = {
            use std::os::windows::fs::MetadataExt;
            let attributes = entry.metadata().map(|m| m.file_attributes()).unwrap_or(0);
            if attributes & 0x400 != 0 && reparse_tag(&full) == Some(TAG_SYMLINK) {
                is_dir(&full)
            } else {
                attributes & 0x10 != 0
            }
        };
        #[cfg(not(windows))]
        let is_dir = match entry.file_type() {
            Ok(t) if t.is_symlink() => is_dir(&full),
            Ok(t) => t.is_dir(),
            Err(_) => false,
        };
        out.push(Entry { name, is_dir });
    }
    Ok(out)
}

/// `os.walk(top, followlinks=False, onerror=...)`, top-down. `visit` gets the directory path,
/// its subdirectory names (it may filter and reorder them, as `dirs[:] = ...` does), and its
/// other names.
pub fn walk(
    top: &str,
    on_error: &mut dyn FnMut(PyOsError),
    visit: &mut dyn FnMut(&str, &mut Vec<String>, Vec<String>) -> Result<(), Fail>,
) -> Result<(), Fail> {
    let mut stack = vec![top.to_string()];
    while let Some(dir) = stack.pop() {
        let entries = match scandir(&dir) {
            Ok(entries) => entries,
            Err(e) => {
                on_error(e);
                continue;
            }
        };
        let mut dirs = Vec::new();
        let mut names = Vec::new();
        for e in entries {
            if e.is_dir {
                dirs.push(e.name);
            } else {
                names.push(e.name);
            }
        }
        visit(&dir, &mut dirs, names)?;
        for d in dirs.iter().rev() {
            let next = os_join(&dir, d);
            if !is_symlink(&next) {
                stack.push(next);
            }
        }
    }
    Ok(())
}

/// `stream.read(n)`: up to n bytes, fewer at end of file.
pub fn read_up_to<R: Read>(f: &mut R, n: u64) -> io::Result<Vec<u8>> {
    let mut out = Vec::new();
    f.take(n).read_to_end(&mut out)?;
    Ok(out)
}

/// `stream.readline(limit)`.
pub fn read_line<R: BufRead>(r: &mut R, limit: u64, out: &mut Vec<u8>) -> io::Result<usize> {
    out.clear();
    r.take(limit).read_until(b'\n', out)
}

fn guard_entry(start: u64, length: u64, data: &[u8]) -> OV {
    OV::Arr(vec![OV::Int(start as i64), OV::Int(length as i64), OV::Str(sha256_hex(data))])
}

/// `guards_from_stream(stream, size)`: SHA-256 of the first and last 4 KiB.
pub fn guards_from_file<R: Read + Seek>(f: &mut R, size: u64) -> io::Result<OV> {
    let mut guards = Vec::new();
    let tail_len = 4096.min(size.saturating_sub(4096));
    for (start, length) in [(0, size.min(4096)), (4096u64.max(size.saturating_sub(4096)), tail_len)] {
        if length > 0 {
            f.seek(SeekFrom::Start(start))?;
            let data = read_up_to(f, length)?;
            guards.push(guard_entry(start, length, &data));
        }
    }
    Ok(OV::Arr(guards))
}

/// `capture_guards(path, size)`.
pub fn capture_guards(path: &str, size: u64) -> Result<OV, PyOsError> {
    let mut f = open(path)?;
    guards_from_file(&mut f, size).map_err(|e| read_err(&e))
}

/// `guards_match(path, guards)`.
pub fn guards_match(path: &str, guards: Option<&OV>) -> Result<bool, PyOsError> {
    let mut f = open(path)?;
    let Some(OV::Arr(items)) = guards else { return Ok(true) };
    for g in items {
        let Some([start, length, digest]) = g.as_arr().and_then(|a| <&[OV; 3]>::try_from(a.as_slice()).ok()) else {
            return Ok(false);
        };
        let (Some(start), Some(length), Some(digest)) = (start.as_i64(), length.as_i64(), digest.as_str()) else {
            return Ok(false);
        };
        if start < 0 || length < 0 {
            return Err(read_err(&io::Error::from_raw_os_error(22)));
        }
        f.seek(SeekFrom::Start(start as u64)).map_err(|e| read_err(&e))?;
        let data = read_up_to(&mut f, length as u64).map_err(|e| read_err(&e))?;
        if sha256_hex(&data) != digest {
            return Ok(false);
        }
    }
    Ok(true)
}
