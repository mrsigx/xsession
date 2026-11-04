//! The part of Python's `zipfile` that 2.3.0 `package_units` uses, over an archive in memory:
//! open (end record, zip64, central directory), `getinfo`, `namelist`, `read` (stored or
//! deflate, CRC-checked), with zipfile's checks and error texts. bzip2/LZMA/zstd members are
//! not decoded (Python can): they report NotImplementedError.

use crate::hash::crc32_update;
use crate::pyerr::{decode_utf8_strict, repr_str};
use crate::tables::CP437_HIGH;
use std::collections::HashMap;

/// An exception as `type(exc).__name__` and `str(exc)`.
#[derive(Debug)]
pub struct PkgErr {
    pub class: &'static str,
    pub msg: String,
}

fn bad(msg: impl Into<String>) -> PkgErr {
    PkgErr { class: "BadZipFile", msg: msg.into() }
}

pub struct ZipInfo {
    pub orig_filename: String,
    pub filename: String,
    flag_bits: u16,
    compress_type: u16,
    crc: u32,
    compress_size: u64,
    pub file_size: u64,
    header_offset: i64,
    end_offset: i64,
}

pub struct Zip<'a> {
    data: &'a [u8],
    pub filelist: Vec<ZipInfo>,
    names: HashMap<String, usize>,
}

fn u16_at(b: &[u8], i: usize) -> u16 {
    u16::from_le_bytes([b[i], b[i + 1]])
}

fn u32_at(b: &[u8], i: usize) -> u32 {
    u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]])
}

fn u64_at(b: &[u8], i: usize) -> u64 {
    u64::from_le_bytes(b[i..i + 8].try_into().unwrap())
}

/// `fp.seek(pos); fp.read(n)` over the archive bytes.
fn read_at(data: &[u8], pos: i64, n: usize) -> &[u8] {
    if pos < 0 || pos as usize >= data.len() {
        return &[];
    }
    let start = pos as usize;
    &data[start..(start + n).min(data.len())]
}

fn sanitize(name: &str) -> String {
    let name = match name.find('\0') {
        Some(i) => &name[..i],
        None => name,
    };
    if cfg!(windows) {
        name.replace('\\', "/")
    } else {
        name.to_string()
    }
}

fn cp437(b: &[u8]) -> String {
    b.iter().map(|&c| if c < 0x80 { c as char } else { CP437_HIGH[(c - 0x80) as usize] }).collect()
}

fn decode_name(b: &[u8], utf8: bool) -> Result<String, PkgErr> {
    if utf8 {
        decode_utf8_strict(b).map(str::to_string).map_err(|m| PkgErr { class: "UnicodeDecodeError", msg: m })
    } else {
        Ok(cp437(b))
    }
}

struct EndRec {
    size: u64,
    offset: u64,
    location: i64,
}

/// `_EndRecData` + `_EndRecData64`; Ok(None) means "File is not a zip file".
fn end_record(data: &[u8]) -> Result<Option<EndRec>, PkgErr> {
    const SIG: &[u8] = b"PK\x05\x06";
    let filesize = data.len();
    if filesize < 22 {
        return Ok(None);
    }
    let tail = &data[filesize - 22..];
    let (rec, location) = if tail.starts_with(SIG) && tail[20..22] == [0, 0] {
        (tail, (filesize - 22) as i64)
    } else {
        let max_comment_start = filesize.saturating_sub(65535 + 22);
        let region = &data[max_comment_start..];
        let Some(start) = region.windows(4).rposition(|w| w == SIG) else { return Ok(None) };
        let rec = &region[start..];
        if rec.len() < 22 {
            return Ok(None);
        }
        (&rec[..22], (max_comment_start + start) as i64)
    };
    let mut end = EndRec { size: u32_at(rec, 12) as u64, offset: u32_at(rec, 16) as u64, location };
    // zip64 end-of-central-directory locator and record.
    let mut offset = location - 20;
    if offset < 0 {
        return Ok(Some(end));
    }
    let loc = read_at(data, offset, 20);
    if loc.len() != 20 {
        return Ok(None);
    }
    if &loc[..4] != b"PK\x06\x07" {
        return Ok(Some(end));
    }
    let (diskno, reloff, disks) = (u32_at(loc, 4), u64_at(loc, 8), u32_at(loc, 16));
    if diskno != 0 || disks > 1 {
        return Err(bad("zipfiles that span multiple disks are not supported"));
    }
    offset -= 56;
    if reloff as i128 > offset as i128 {
        return Err(bad("Corrupt zip64 end of central directory locator"));
    }
    let mut extrasz = offset - reloff as i64;
    let mut rec64 = read_at(data, reloff as i64, 56);
    if rec64.len() != 56 {
        return Ok(None);
    }
    if !rec64.starts_with(b"PK\x06\x06") && reloff as i64 != offset {
        extrasz = 0;
        rec64 = read_at(data, offset, 56);
        if rec64.len() != 56 {
            return Ok(None);
        }
    }
    if !rec64.starts_with(b"PK\x06\x06") {
        return Err(bad("Zip64 end of central directory record not found"));
    }
    let (sz, dirsize, diroffset) = (u64_at(rec64, 4), u64_at(rec64, 40), u64_at(rec64, 48));
    if diroffset as i128 + dirsize as i128 != reloff as i128 || sz as i128 + 12 != 56 + extrasz as i128 {
        return Err(bad("Corrupt zip64 end of central directory record"));
    }
    end.size = dirsize;
    end.offset = diroffset;
    end.location = offset - extrasz;
    Ok(Some(end))
}

impl<'a> Zip<'a> {
    /// `zipfile.ZipFile(io.BytesIO(data))`.
    pub fn open(data: &'a [u8]) -> Result<Zip<'a>, PkgErr> {
        let end = end_record(data)?.ok_or_else(|| bad("File is not a zip file"))?;
        let concat = end.location - end.size as i64 - end.offset as i64;
        let start_dir = end.offset as i64 + concat;
        if start_dir < 0 {
            return Err(bad("Bad offset for central directory"));
        }
        let cd = read_at(data, start_dir, end.size as usize);
        let mut zip = Zip { data, filelist: Vec::new(), names: HashMap::new() };
        let mut pos = 0usize;
        let mut total: u64 = 0;
        let take = |pos: &mut usize, n: usize| -> &[u8] {
            let start = (*pos).min(cd.len());
            let end = (start + n).min(cd.len());
            *pos = start + n;
            &cd[start..end]
        };
        while total < end.size {
            let h = take(&mut pos, 46);
            if h.len() != 46 {
                return Err(bad("Truncated central directory"));
            }
            if &h[..4] != b"PK\x01\x02" {
                return Err(bad("Bad magic number for central directory"));
            }
            let extract_version = h[6];
            let flag_bits = u16_at(h, 8);
            let compress_type = u16_at(h, 10);
            let crc = u32_at(h, 16);
            let (fn_len, extra_len, comment_len) = (u16_at(h, 28) as usize, u16_at(h, 30) as usize, u16_at(h, 32) as usize);
            let mut info = ZipInfo {
                orig_filename: String::new(),
                filename: String::new(),
                flag_bits,
                compress_type,
                crc,
                compress_size: u32_at(h, 20) as u64,
                file_size: u32_at(h, 24) as u64,
                header_offset: u32_at(h, 42) as i64,
                end_offset: 0,
            };
            let raw_name = take(&mut pos, fn_len).to_vec();
            let name_crc = crc32_update(0, &raw_name);
            info.orig_filename = decode_name(&raw_name, flag_bits & 0x800 != 0)?;
            info.filename = sanitize(&info.orig_filename);
            let extra = take(&mut pos, extra_len).to_vec();
            take(&mut pos, comment_len);
            if extract_version > 63 {
                return Err(PkgErr { class: "NotImplementedError", msg: format!("zip file version {:.1}", extract_version as f64 / 10.0) });
            }
            decode_extra(&mut info, &extra, name_crc)?;
            info.header_offset += concat;
            let index = zip.filelist.len();
            zip.names.insert(info.filename.clone(), index);
            zip.filelist.push(info);
            total += (46 + fn_len + extra_len + comment_len) as u64;
        }
        let mut order: Vec<usize> = (0..zip.filelist.len()).collect();
        order.sort_by_key(|&i| zip.filelist[i].header_offset);
        let mut end_offset = start_dir;
        for &i in order.iter().rev() {
            zip.filelist[i].end_offset = end_offset;
            end_offset = zip.filelist[i].header_offset;
        }
        Ok(zip)
    }

    pub fn namelist(&self) -> Vec<&str> {
        self.filelist.iter().map(|i| i.filename.as_str()).collect()
    }

    /// `getinfo(name)`.
    pub fn getinfo(&self, name: &str) -> Result<&ZipInfo, PkgErr> {
        match self.names.get(name) {
            Some(&i) => Ok(&self.filelist[i]),
            None => Err(PkgErr {
                class: "KeyError",
                msg: repr_str(&format!("There is no item named {} in the archive", repr_str(name))),
            }),
        }
    }

    /// `read(info)`.
    pub fn read(&self, info: &ZipInfo) -> Result<Vec<u8>, PkgErr> {
        let header = read_at(self.data, info.header_offset, 30);
        if header.len() != 30 {
            return Err(bad("Truncated file header"));
        }
        if &header[..4] != b"PK\x03\x04" {
            return Err(bad("Bad magic number for file header"));
        }
        let local_flags = u16_at(header, 6);
        let (fn_len, extra_len) = (u16_at(header, 26) as i64, u16_at(header, 28) as i64);
        let fname = read_at(self.data, info.header_offset + 30, fn_len as usize);
        if info.flag_bits & (1 << 5) != 0 {
            return Err(PkgErr { class: "NotImplementedError", msg: "compressed patched data (flag bit 5)".into() });
        }
        if info.flag_bits & (1 << 6) != 0 {
            return Err(PkgErr { class: "NotImplementedError", msg: "strong encryption (flag bit 6)".into() });
        }
        let fname_str = decode_name(fname, local_flags & 0x800 != 0)?;
        if fname_str != info.orig_filename {
            return Err(bad(format!("File name in directory {} and header {} differ.", repr_str(&info.orig_filename), bytes_repr(fname))));
        }
        let data_start = info.header_offset + 30 + fn_len + extra_len;
        if data_start + info.compress_size as i64 > info.end_offset && info.end_offset != info.header_offset {
            return Err(bad(format!("Overlapped entries: {} (possible zip bomb)", repr_str(&info.orig_filename))));
        }
        if info.flag_bits & 1 != 0 {
            return Err(PkgErr {
                class: "RuntimeError",
                msg: format!("File {} is encrypted, password required for extraction", repr_str(&info.filename)),
            });
        }
        if !matches!(info.compress_type, 0 | 8) {
            return Err(PkgErr { class: "NotImplementedError", msg: "That compression method is not supported".into() });
        }
        let available = read_at(self.data, data_start, info.compress_size as usize);
        if available.is_empty() && info.compress_size > 0 {
            return Err(PkgErr { class: "EOFError", msg: String::new() });
        }
        let truncated = (available.len() as u64) < info.compress_size;
        let limit = info.file_size as usize;
        let out = if info.compress_type == 0 {
            if truncated && (available.len() as u64) < info.file_size {
                return Err(PkgErr { class: "EOFError", msg: String::new() });
            }
            available[..available.len().min(limit)].to_vec()
        } else {
            use miniz_oxide::inflate::{decompress_to_vec_with_limit, TINFLStatus};
            match decompress_to_vec_with_limit(available, limit) {
                Ok(v) => v,
                Err(e) => match e.status {
                    TINFLStatus::HasMoreOutput => e.output,
                    TINFLStatus::FailedCannotMakeProgress | TINFLStatus::NeedsMoreInput => {
                        // Python asks the file for more bytes only while output is still owed.
                        if truncated && e.output.len() < limit {
                            return Err(PkgErr { class: "EOFError", msg: String::new() });
                        }
                        e.output
                    }
                    _ => {
                        return Err(PkgErr { class: "error", msg: "Error -3 while decompressing data: invalid deflate data".into() });
                    }
                },
            }
        };
        if crc32_update(0, &out) != info.crc {
            return Err(bad(format!("Bad CRC-32 for file {}", repr_str(&info.filename))));
        }
        Ok(out)
    }
}

/// `repr(bytes)`.
fn bytes_repr(b: &[u8]) -> String {
    let quote = if b.contains(&b'\'') && !b.contains(&b'"') { '"' } else { '\'' };
    let mut out = String::from("b");
    out.push(quote);
    for &c in b {
        match c {
            b'\\' => out.push_str("\\\\"),
            b'\t' => out.push_str("\\t"),
            b'\n' => out.push_str("\\n"),
            b'\r' => out.push_str("\\r"),
            _ if c as char == quote => {
                out.push('\\');
                out.push(quote);
            }
            0x20..=0x7e => out.push(c as char),
            _ => out.push_str(&format!("\\x{:02x}", c)),
        }
    }
    out.push(quote);
    out
}

/// `ZipInfo._decodeExtra`.
fn decode_extra(info: &mut ZipInfo, extra: &[u8], name_crc: u32) -> Result<(), PkgErr> {
    let mut extra = extra;
    while extra.len() >= 4 {
        let (tp, ln) = (u16_at(extra, 0), u16_at(extra, 2) as usize);
        if ln + 4 > extra.len() {
            return Err(bad(format!("Corrupt extra field {:04x} (size={})", tp, ln)));
        }
        let mut data = &extra[4..ln + 4];
        if tp == 0x0001 {
            let mut take = |field: &str| -> Result<u64, PkgErr> {
                if data.len() < 8 {
                    return Err(bad(format!("Corrupt zip64 extra field. {} not found.", field)));
                }
                let v = u64_at(data, 0);
                data = &data[8..];
                Ok(v)
            };
            if info.file_size == 0xFFFF_FFFF || info.file_size == u64::MAX {
                info.file_size = take("File size")?;
            }
            if info.compress_size == 0xFFFF_FFFF {
                info.compress_size = take("Compress size")?;
            }
            if info.header_offset == 0xFFFF_FFFF {
                info.header_offset = take("Header offset")? as i64;
            }
        } else if tp == 0x7075 {
            if data.len() < 5 {
                return Err(bad("Corrupt unicode path extra field (0x7075)"));
            }
            if data[0] == 1 && u32_at(data, 1) == name_crc {
                let name = decode_utf8_strict(&data[5..])
                    .map_err(|_| bad("Corrupt unicode path extra field (0x7075): invalid utf-8 bytes"))?;
                if !name.is_empty() {
                    info.filename = sanitize(name);
                }
            }
        }
        extra = &extra[ln + 4..];
    }
    Ok(())
}
