//! Transcript files as 2.4.0 reads them (`is_compressed`, `session_stem`, `zstd_content_size`,
//! `open_session`, `session_stat`, `stored_unchanged`, `session_guards_match`). A `.jsonl.zst`
//! reads as its decompressed bytes, so line numbers, byte offsets and record digests mean what
//! they mean for the plain `.jsonl`. Frames are decoded by libzstd through the `zstd` crate; the
//! frame walk that sizes a file without decoding it is ours.

use crate::ojson::{Obj, OV};
use crate::pathx;
use crate::pyerr::{os_text, PyOsError};
use crate::pyio::{self, Stat};
use std::fs::File;
use std::io::{self, BufReader, Read, Seek, SeekFrom};

pub const ZSTD_SUFFIX: &str = ".jsonl.zst";
pub const UNDECODABLE: &str = "zstd: data could not be decoded";
const MAGIC: [u8; 4] = [0x28, 0xb5, 0x2f, 0xfd];
/// Python's ZstdFile reads compressed input, and serves decompressed output, in
/// io.DEFAULT_BUFFER_SIZE units (128 KiB on 3.14). Bounded reads of a compressed stream use the
/// same units here, so the two implementations read ahead equally and meet a corrupt block at the
/// same point. Reads that span a whole file do not depend on it (see `undecodable`).
pub const CHUNK: usize = 128 * 1024;

/// `is_compressed(path)`.
pub fn is_compressed(path: &str) -> bool {
    path.ends_with(ZSTD_SUFFIX)
}

/// `session_stem(path)`: `Path.stem` of the transcript name, ignoring a `.zst` storage suffix.
pub fn stem(path: &str) -> String {
    if is_compressed(path) {
        let name = pathx::name(path);
        pathx::stem(&name[..name.len() - 4])
    } else {
        pathx::stem(path)
    }
}

fn read_exact_or(f: &mut File, count: usize) -> Result<Vec<u8>, PyOsError> {
    let data = pyio::read_up_to(f, count as u64).map_err(|e| pyio::read_err(&e))?;
    if data.len() != count {
        return Err(os_text("zstd: truncated frame"));
    }
    Ok(data)
}

fn little_endian(bytes: &[u8]) -> u64 {
    bytes.iter().rev().fold(0u64, |acc, b| (acc << 8) | u64::from(*b))
}

fn seek_to(f: &mut File, position: u64) -> Result<(), PyOsError> {
    f.seek(SeekFrom::Start(position)).map(drop).map_err(|e| pyio::read_err(&e))
}

/// `zstd_content_size(stream, stored_size)`: the decompressed size from the frame headers
/// (RFC 8878), walking block headers without decoding. Skippable frames carry no content.
pub fn content_size(f: &mut File, stored: u64) -> Result<u64, PyOsError> {
    let mut total: u64 = 0;
    let mut position: u64 = 0;
    while position < stored {
        seek_to(f, position)?;
        let magic = read_exact_or(f, 4)?;
        if (0x184D_2A50..=0x184D_2A5F).contains(&little_endian(&magic)) {
            position = position.saturating_add(8 + little_endian(&read_exact_or(f, 4)?));
            if position > stored {
                return Err(os_text("zstd: truncated frame"));
            }
            continue;
        }
        if magic != MAGIC {
            return Err(os_text("zstd: not a zstd frame"));
        }
        let descriptor = read_exact_or(f, 1)?[0];
        if descriptor & 0x08 != 0 {
            return Err(os_text("zstd: reserved frame header bit set"));
        }
        let single = (descriptor >> 5) & 1;
        if single == 0 {
            read_exact_or(f, 1)?;
        }
        let dictionary = [0usize, 1, 2, 4][usize::from(descriptor & 3)];
        if dictionary > 0 && little_endian(&read_exact_or(f, dictionary)?) != 0 {
            return Err(os_text("zstd: frame needs a dictionary"));
        }
        let width = [usize::from(single), 2, 4, 8][usize::from(descriptor >> 6)];
        if width == 0 {
            return Err(os_text("zstd: frame without content size"));
        }
        let size = little_endian(&read_exact_or(f, width)?);
        total = total.saturating_add(if width == 2 { size + 256 } else { size });
        position = f.stream_position().map_err(|e| pyio::read_err(&e))?;
        loop {
            let block = little_endian(&read_exact_or(f, 3)?);
            let kind = (block >> 1) & 3;
            if kind == 3 {
                return Err(os_text("zstd: reserved block type"));
            }
            position += 3 + if kind == 1 { 1 } else { block >> 3 };
            if position > stored {
                return Err(os_text("zstd: truncated frame"));
            }
            seek_to(f, position)?;
            if block & 1 == 1 {
                break;
            }
        }
        if descriptor & 0x04 != 0 {
            position += 4;
            if position > stored {
                return Err(os_text("zstd: truncated frame"));
            }
        }
    }
    Ok(total)
}

type Decoder = zstd::stream::read::Decoder<'static, BufReader<File>>;

fn decode_error() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, UNDECODABLE)
}

/// `ZstdLogical`: the decompressed bytes of a `.jsonl.zst`. Seeking forward decodes; seeking
/// back restarts at the first frame. Decoder failures read as one fixed error text.
pub struct ZstdView {
    decoder: Option<Decoder>,
    pos: u64,
}

impl ZstdView {
    fn over(file: File) -> io::Result<ZstdView> {
        let decoder = zstd::stream::read::Decoder::with_buffer(BufReader::with_capacity(CHUNK, file)).map_err(|_| decode_error())?;
        Ok(ZstdView { decoder: Some(decoder), pos: 0 })
    }
}

impl Read for ZstdView {
    /// One unit of at most CHUNK bytes, filled as Python's decompressor fills it; a failure
    /// anywhere in the unit loses the whole unit, as Python's raise does.
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let Some(decoder) = self.decoder.as_mut() else { return Err(decode_error()) };
        let limit = buf.len().min(CHUNK);
        let mut filled = 0;
        while filled < limit {
            match decoder.read(&mut buf[filled..limit]) {
                Ok(0) => break,
                Ok(n) => filled += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => return Err(decode_error()),
            }
        }
        self.pos += filled as u64;
        Ok(filled)
    }
}

impl Seek for ZstdView {
    fn seek(&mut self, to: SeekFrom) -> io::Result<u64> {
        let target = match to {
            SeekFrom::Start(n) => n,
            SeekFrom::Current(delta) => self.pos.checked_add_signed(delta).ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?,
            SeekFrom::End(_) => return Err(io::Error::new(io::ErrorKind::Unsupported, "seek from the end of a zstd stream")),
        };
        if target < self.pos {
            let mut stored = self.decoder.take().ok_or_else(decode_error)?.finish();
            stored.rewind()?;
            self.decoder = Some(zstd::stream::read::Decoder::with_buffer(stored).map_err(|_| decode_error())?);
            self.pos = 0;
        }
        let mut scratch = vec![0u8; CHUNK];
        while self.pos < target {
            let want = (target - self.pos).min(scratch.len() as u64) as usize;
            if self.read(&mut scratch[..want])? == 0 {
                break;
            }
        }
        Ok(self.pos)
    }
}

/// A transcript opened for reading: the file itself, or the decompressed view of a `.jsonl.zst`.
pub enum Logical {
    Plain(File),
    Zstd(ZstdView),
}

impl Logical {
    /// The stored file underneath, for guards over the stored bytes.
    pub fn into_file(self) -> Option<File> {
        match self {
            Logical::Plain(f) => Some(f),
            Logical::Zstd(mut view) => view.decoder.take().map(|d| d.finish().into_inner()),
        }
    }
}

impl Read for Logical {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Logical::Plain(f) => f.read(buf),
            Logical::Zstd(view) => view.read(buf),
        }
    }
}

impl Seek for Logical {
    fn seek(&mut self, to: SeekFrom) -> io::Result<u64> {
        match self {
            Logical::Plain(f) => f.seek(to),
            Logical::Zstd(view) => view.seek(to),
        }
    }
}

/// The decompressed view over an open `.jsonl.zst`. An empty file holds no frame and reads as an
/// empty transcript, like an empty `.jsonl`.
pub fn logical_over(file: File) -> io::Result<Logical> {
    if file.metadata()?.len() == 0 {
        return Ok(Logical::Plain(file));
    }
    Ok(Logical::Zstd(ZstdView::over(file)?))
}

/// `open_session(path)`.
pub fn open(path: &str) -> Result<Logical, PyOsError> {
    let file = pyio::open(path)?;
    if !is_compressed(path) {
        return Ok(Logical::Plain(file));
    }
    logical_over(file).map_err(|e| pyio::read_err(&e))
}

/// Every decompressed byte (the legacy `lines:` count).
pub fn read_all(path: &str) -> Result<Vec<u8>, PyOsError> {
    let mut f = open(path)?;
    let mut out = Vec::new();
    f.read_to_end(&mut out).map_err(|e| pyio::read_err(&e))?;
    Ok(out)
}

/// `SessionStat`: a transcript's stat in logical terms. A `.jsonl.zst` reports its decompressed
/// size and the stored file's mtime; `stored` is the size on disk (None for a plain file).
#[derive(PartialEq)]
pub struct SessionStat {
    pub size: u64,
    pub mtime_ns: i128,
    pub stored: Option<u64>,
}

/// `session_stat(path, previous)`: a compressed file's decompressed size comes from its frame
/// headers, or from `previous` (an earlier stat of the same file) while the stored size and
/// mtime are unchanged.
pub fn stat(path: &str, previous: Option<&SessionStat>) -> Result<SessionStat, PyOsError> {
    let st = pyio::stat(path)?;
    if !is_compressed(path) {
        return Ok(SessionStat { size: st.size, mtime_ns: st.mtime_ns, stored: None });
    }
    if let Some(previous) = previous {
        if previous.stored == Some(st.size) && previous.mtime_ns == st.mtime_ns {
            return Ok(SessionStat { size: previous.size, mtime_ns: st.mtime_ns, stored: Some(st.size) });
        }
    }
    let mut f = pyio::open(path)?;
    let size = content_size(&mut f, st.size)?;
    Ok(SessionStat { size, mtime_ns: st.mtime_ns, stored: Some(st.size) })
}

/// `stored_unchanged(path, stored)`: a compressed transcript is rewritten whole, never appended.
pub fn stored_unchanged(path: &str, stored: &Stat) -> Result<bool, PyOsError> {
    let now = pyio::stat(path)?;
    Ok((now.size, now.mtime_ns) == (stored.size, stored.mtime_ns))
}

/// Read-ahead for a transcript opened through a `BufReader`: Python's units for a compressed
/// stream, the given capacity for a plain file.
pub fn read_ahead(path: &str, plain: usize) -> usize {
    if is_compressed(path) {
        CHUNK
    } else {
        plain
    }
}

/// `undecodable(coverage, path, since)`: whether reading `path` met undecodable zstd data (a
/// read_error recorded at or after `since`). How much of a corrupt stream decodes before the
/// failure depends on read-ahead, so a transcript that fails to decode contributes no records:
/// hits, context and list details from it are dropped, and the read_error alone stands for it.
pub fn undecodable(cov: &Obj, path: &str, since: usize) -> bool {
    let Some(OV::Arr(errors)) = cov.get("errors") else { return false };
    errors.iter().skip(since).any(|e| {
        e.as_obj().is_some_and(|e| {
            e.str_of("kind") == Some("read_error") && e.str_of("detail") == Some(UNDECODABLE) && e.str_of("path") == Some(path)
        })
    })
}

fn arr_len(cov: &Obj, key: &str) -> usize {
    match cov.get(key) {
        Some(OV::Arr(items)) => items.len(),
        _ => 0,
    }
}

const COUNTERS: [&str; 4] = ["malformed_records", "unfinished_records", "changed_files", "unsupported_records"];

/// `coverage_mark(coverage)`: the coverage state before one read of a transcript.
pub struct Mark {
    pub errors: usize,
    notes: usize,
    counters: Vec<OV>,
    complete: OV,
}

pub fn mark(cov: &Obj) -> Mark {
    Mark {
        errors: arr_len(cov, "errors"),
        notes: arr_len(cov, "notes"),
        counters: COUNTERS.iter().map(|k| cov.get(k).cloned().unwrap_or(OV::Null)).collect(),
        complete: cov.get("complete").cloned().unwrap_or(OV::Null),
    }
}

/// `undecodable_only(coverage, mark, path)`: undo what one read of `path` added to coverage, then
/// record its single read_error.
pub fn undecodable_only(cov: &mut Obj, mark: &Mark, path: &str) {
    for (key, keep) in [("errors", mark.errors), ("notes", mark.notes)] {
        if let Some(OV::Arr(items)) = cov.get_mut(key) {
            items.truncate(keep);
        }
    }
    for (key, value) in COUNTERS.iter().zip(&mark.counters) {
        cov.set(key, value.clone());
    }
    cov.set("complete", mark.complete.clone());
    crate::inventory::problem(cov, "read_error", path, UNDECODABLE);
}

/// `zstd_validates(path)`: decode a whole `.jsonl.zst`, discarding the output.
pub fn validates(path: &str) -> bool {
    let Ok(mut f) = open(path) else { return false };
    let mut buf = vec![0u8; 1 << 20];
    loop {
        match f.read(&mut buf) {
            Ok(0) => return true,
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => return false,
        }
    }
}

