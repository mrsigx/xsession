//! 2.3.0 `time_key`: `datetime.fromisoformat(value.replace('Z', '+00:00'))`, then `.timestamp()`
//! for aware values and -inf otherwise. Mirrors CPython's C implementation (`_datetimemodule.c`),
//! including how it reads a NUL-terminated UTF-8 buffer.

use crate::ojson::{OV, SURROGATE_BASE};

pub fn time_key(value: Option<&OV>) -> f64 {
    match value {
        Some(OV::Str(s)) => aware_timestamp(&s.replace('Z', "+00:00")).unwrap_or(f64::NEG_INFINITY),
        _ => f64::NEG_INFINITY,
    }
}

fn is_surrogate(c: char) -> bool {
    c as u32 >= SURROGATE_BASE
}

/// POSIX timestamp of an aware ISO string; None for naive or invalid input.
pub fn aware_timestamp(text: &str) -> Option<f64> {
    let mut chars: Vec<char> = text.chars().collect();
    if chars.len() < 7 {
        return None;
    }
    // A surrogate is allowed only as the separator (positions 7, 8 or 10).
    for pos in [7usize, 8, 10] {
        if pos > chars.len() {
            break;
        }
        if pos < chars.len() && is_surrogate(chars[pos]) {
            chars[pos] = 'T';
            break;
        }
    }
    if chars.iter().any(|c| is_surrogate(*c)) {
        return None;
    }
    let s: String = chars.into_iter().collect();
    let b = s.as_bytes();
    let len = b.len();
    let sep = separator(b)?;
    let (mut year, mut month, mut day) = parse_date(b, sep)?;
    let mut t = Time::default();
    if len > sep {
        t = parse_time(b, sep + 1, len)?;
    }
    let offset = t.offset?;
    if t.hour == 24 && month <= 12 {
        let dim = days_in_month(year, month);
        if day <= dim {
            if t.minute == 0 && t.second == 0 && t.micro == 0 {
                t.hour = 0;
                day += 1;
                if day > dim {
                    day = 1;
                    month += 1;
                    if month > 12 {
                        month = 1;
                        year += 1;
                    }
                }
            } else {
                return None;
            }
        }
    }
    if !(1..=9999).contains(&year) || !(1..=12).contains(&month) || day < 1 || day > days_in_month(year, month) {
        return None;
    }
    if !(0..=23).contains(&t.hour) || !(0..=59).contains(&t.minute) || !(0..=59).contains(&t.second) || !(0..=999_999).contains(&t.micro) {
        return None;
    }
    let days = ymd_to_ord(year, month, day) as i128 - 719_163;
    let seconds = days * 86_400 + (t.hour * 3600 + t.minute * 60 + t.second) as i128;
    let total = seconds * 1_000_000 + t.micro as i128 - (offset.0 as i128 * 1_000_000 + offset.1 as i128);
    Some(micros_to_seconds(total))
}

/// `int / 10**6` as Python's true division: correctly rounded.
fn micros_to_seconds(total: i128) -> f64 {
    let sign = if total < 0 { "-" } else { "" };
    let a = total.unsigned_abs();
    format!("{}{}.{:06}", sign, a / 1_000_000, a % 1_000_000).parse().unwrap()
}

fn at(b: &[u8], i: usize) -> u8 {
    b.get(i).copied().unwrap_or(0)
}

fn digits(b: &[u8], p: &mut usize, n: usize) -> Option<i32> {
    let mut v = 0i32;
    for _ in 0..n {
        let d = at(b, *p).wrapping_sub(b'0');
        *p += 1;
        if d > 9 {
            return None;
        }
        v = v * 10 + d as i32;
    }
    Some(v)
}

/// C `_find_isoformat_datetime_separator`.
fn separator(b: &[u8]) -> Option<usize> {
    let len = b.len();
    if len == 7 {
        return Some(7);
    }
    if at(b, 4) == b'-' {
        if at(b, 5) == b'W' {
            if len < 8 {
                return None;
            }
            if len > 8 && at(b, 8) == b'-' {
                if len == 9 {
                    return None;
                }
                if len > 10 && at(b, 10).is_ascii_digit() {
                    return Some(8);
                }
                return Some(10);
            }
            return Some(8);
        }
        return Some(10);
    }
    if at(b, 4) == b'W' {
        let mut idx = 7;
        while idx < len && at(b, idx).is_ascii_digit() {
            idx += 1;
        }
        if idx < 9 {
            return Some(idx);
        }
        return Some(if idx % 2 == 0 { 7 } else { 8 });
    }
    Some(8)
}

/// C `parse_isoformat_date` over the first `len` bytes (reads may run past them, as in C).
fn parse_date(b: &[u8], len: usize) -> Option<(i32, i32, i32)> {
    let mut p = 0;
    let year = digits(b, &mut p, 4)?;
    let sep = at(b, p) == b'-';
    if sep {
        p += 1;
    }
    if at(b, p) == b'W' {
        p += 1;
        let week = digits(b, &mut p, 2)?;
        let mut day = 1;
        if p < len {
            if sep {
                let c = at(b, p);
                p += 1;
                if c != b'-' {
                    return None;
                }
            }
            day = digits(b, &mut p, 1)?;
        }
        return iso_to_ymd(year, week, day);
    }
    let month = digits(b, &mut p, 2)?;
    if sep {
        let c = at(b, p);
        p += 1;
        if c != b'-' {
            return None;
        }
    }
    let day = digits(b, &mut p, 2)?;
    Some((year, month, day))
}

#[derive(Default)]
struct Time {
    hour: i32,
    minute: i32,
    second: i32,
    micro: i32,
    /// None: naive. Some((seconds, microseconds)): UTC offset.
    offset: Option<(i32, i32)>,
}

/// C `parse_hh_mm_ss_ff` over b[p..end]: values, microseconds, and whether more text follows.
fn hh_mm_ss_ff(b: &[u8], mut p: usize, end: usize) -> Result<([i32; 3], i32, bool), [i32; 3]> {
    let mut vals = [0i32; 3];
    let mut has_sep = true;
    for i in 0..3 {
        match digits(b, &mut p, 2) {
            Some(v) => vals[i] = v,
            None => return Err(vals),
        }
        let c = at(b, p);
        p += 1;
        if i == 0 {
            has_sep = c == b':';
        }
        if p >= end {
            return Ok((vals, 0, c != 0));
        } else if has_sep && c == b':' {
            if i == 2 {
                return Err(vals);
            }
            continue;
        } else if c == b'.' || c == b',' {
            if i < 2 {
                return Err(vals);
            }
            break;
        } else if !has_sep {
            p -= 1;
        } else {
            return Err(vals);
        }
    }
    let to_parse = (end - p).min(6);
    let mut micro = match digits(b, &mut p, to_parse) {
        Some(v) => v,
        None => return Err(vals),
    };
    if to_parse < 6 {
        micro *= [100_000, 10_000, 1_000, 100, 10][to_parse - 1];
    }
    while at(b, p).is_ascii_digit() {
        p += 1;
    }
    Ok((vals, micro, at(b, p) != 0))
}

/// C `parse_isoformat_time` + `tzinfo_from_isoformat_results`.
fn parse_time(b: &[u8], start: usize, end: usize) -> Option<Time> {
    let mut tz = start;
    loop {
        let c = at(b, tz);
        if c == b'Z' || c == b'+' || c == b'-' {
            break;
        }
        tz += 1;
        if tz >= end {
            break;
        }
    }
    let (vals, micro, more) = hh_mm_ss_ff(b, start, tz).ok()?;
    let mut t = Time { hour: vals[0], minute: vals[1], second: vals[2], micro, offset: None };
    if tz == end {
        return if more { None } else { Some(t) };
    }
    if at(b, tz) == b'Z' {
        if at(b, tz + 1) != 0 {
            return None;
        }
        t.offset = Some((0, 0));
        return Some(t);
    }
    let sign = if at(b, tz) == b'-' { -1 } else { 1 };
    let (tz_vals, tz_micro, tz_more) = match hh_mm_ss_ff(b, tz + 1, end) {
        Ok(r) => r,
        Err(_) => return None,
    };
    if !(0..=23).contains(&tz_vals[0]) || !(0..=59).contains(&tz_vals[1]) || !(0..=59).contains(&tz_vals[2]) || tz_more {
        return None;
    }
    let seconds = sign * (tz_vals[0] * 3600 + tz_vals[1] * 60 + tz_vals[2]);
    // A zero whole-second offset is UTC, whatever the fraction.
    t.offset = Some(if seconds == 0 { (0, 0) } else { (seconds, sign * tz_micro) });
    Some(t)
}

fn is_leap(y: i32) -> bool {
    y % 4 == 0 && (y % 100 != 0 || y % 400 == 0)
}

fn days_in_month(y: i32, m: i32) -> i32 {
    const DIM: [i32; 13] = [0, 31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    if m == 2 && is_leap(y) {
        29
    } else {
        DIM[m.clamp(0, 12) as usize]
    }
}

fn days_before_month(y: i32, m: i32) -> i32 {
    const DBM: [i32; 13] = [0, 0, 31, 59, 90, 120, 151, 181, 212, 243, 273, 304, 334];
    DBM[m as usize] + if m > 2 && is_leap(y) { 1 } else { 0 }
}

fn ymd_to_ord(y: i32, m: i32, d: i32) -> i64 {
    let y1 = (y - 1) as i64;
    y1 * 365 + y1 / 4 - y1 / 100 + y1 / 400 + days_before_month(y, m) as i64 + d as i64
}

fn ord_to_ymd(ordinal: i64) -> (i32, i32, i32) {
    const DI400Y: i64 = 146_097;
    const DI100Y: i64 = 36_524;
    const DI4Y: i64 = 1_461;
    let mut n = ordinal - 1;
    let n400 = n.div_euclid(DI400Y);
    n = n.rem_euclid(DI400Y);
    let mut year = n400 * 400 + 1;
    let n100 = n / DI100Y;
    n %= DI100Y;
    let n4 = n / DI4Y;
    n %= DI4Y;
    let n1 = n / 365;
    n %= 365;
    year += n100 * 100 + n4 * 4 + n1;
    if n1 == 4 || n100 == 4 {
        return ((year - 1) as i32, 12, 31);
    }
    let year = year as i32;
    let mut month = ((n + 50) >> 5) as i32;
    let mut preceding = days_before_month(year, month) as i64;
    if preceding > n {
        month -= 1;
        preceding -= days_in_month(year, month) as i64;
    }
    (year, month, (n - preceding + 1) as i32)
}

/// C `iso_to_ymd`.
fn iso_to_ymd(year: i32, week: i32, day: i32) -> Option<(i32, i32, i32)> {
    if !(1..=9999).contains(&year) {
        return None;
    }
    if week <= 0 || week >= 53 {
        let mut out_of_range = true;
        if week == 53 {
            let first_weekday = (ymd_to_ord(year, 1, 1) + 6) % 7;
            if first_weekday == 3 || (first_weekday == 2 && is_leap(year)) {
                out_of_range = false;
            }
        }
        if out_of_range {
            return None;
        }
    }
    if day <= 0 || day >= 8 {
        return None;
    }
    let first_day = ymd_to_ord(year, 1, 1);
    let first_weekday = (first_day + 6) % 7;
    let mut week1_monday = first_day - first_weekday;
    if first_weekday > 3 {
        week1_monday += 7;
    }
    Some(ord_to_ymd(week1_monday + ((week - 1) * 7 + day - 1) as i64))
}
