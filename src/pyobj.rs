//! Python object semantics over decoded JSON values: equality, hashability, ordering, and a
//! dict keyed the way Python's is (1 == 1.0 == True), for identity values that may not be strings.

use crate::ojson::{Obj, OV};
use crate::pyerr::{type_name, Fail};
use std::cmp::Ordering;
use std::collections::HashMap;

fn as_number(v: &OV) -> Option<f64> {
    match v {
        OV::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
        OV::Int(i) => Some(*i as f64),
        OV::UInt(u) => Some(*u as f64),
        OV::Big(s) => s.parse::<f64>().ok(),
        OV::Float(f) => Some(*f),
        _ => None,
    }
}

fn as_integer(v: &OV) -> Option<i128> {
    match v {
        OV::Bool(b) => Some(*b as i128),
        OV::Int(i) => Some(*i as i128),
        OV::UInt(u) => Some(*u as i128),
        _ => None,
    }
}

/// Python `a == b`.
pub fn py_eq(a: &OV, b: &OV) -> bool {
    match (a, b) {
        (OV::Null, OV::Null) => true,
        (OV::Str(x), OV::Str(y)) => x == y,
        (OV::Arr(x), OV::Arr(y)) => x.len() == y.len() && x.iter().zip(y).all(|(p, q)| py_eq(p, q)),
        (OV::Obj(x), OV::Obj(y)) => {
            x.0.len() == y.0.len() && x.0.iter().all(|(k, v)| y.get(k).is_some_and(|w| py_eq(v, w)))
        }
        (OV::Big(x), OV::Big(y)) => x == y,
        _ => match (as_integer(a), as_integer(b)) {
            (Some(x), Some(y)) => x == y,
            _ => match (as_number(a), as_number(b)) {
                (Some(x), Some(y)) => x == y,
                _ => false,
            },
        },
    }
}

pub fn unhashable(v: &OV) -> bool {
    matches!(v, OV::Arr(_) | OV::Obj(_))
}

pub fn dict_key_error(v: &OV) -> Fail {
    let t = type_name(v);
    Fail::Invalid(format!("cannot use '{}' as a dict key (unhashable type: '{}')", t, t))
}

pub fn set_element_error(v: &OV) -> Fail {
    let t = type_name(v);
    Fail::Invalid(format!("cannot use '{}' as a set element (unhashable type: '{}')", t, t))
}

/// `'X' object has no attribute 'name'`.
pub fn attribute_error(v: &OV, name: &str) -> Fail {
    Fail::Other("AttributeError".into(), format!("'{}' object has no attribute '{}'", type_name(v), name))
}

/// `value.casefold()` for an identity value.
pub fn casefold_of(v: &OV) -> Result<String, Fail> {
    match v {
        OV::Str(s) => Ok(crate::pytext::casefold(s)),
        other => Err(attribute_error(other, "casefold")),
    }
}

/// Python `a < b` ordering for sort keys; Err is the TypeError text.
pub fn py_cmp(a: &OV, b: &OV) -> Result<Ordering, String> {
    match (a, b) {
        (OV::Str(x), OV::Str(y)) => Ok(x.cmp(y)),
        (OV::Arr(x), OV::Arr(y)) => {
            for (p, q) in x.iter().zip(y) {
                if !py_eq(p, q) {
                    return py_cmp(p, q);
                }
            }
            Ok(x.len().cmp(&y.len()))
        }
        _ => {
            if let (Some(x), Some(y)) = (as_integer(a), as_integer(b)) {
                return Ok(x.cmp(&y));
            }
            if let (Some(x), Some(y)) = (as_number(a), as_number(b)) {
                return Ok(x.partial_cmp(&y).unwrap_or(Ordering::Equal));
            }
            Err(format!("'<' not supported between instances of '{}' and '{}'", type_name(a), type_name(b)))
        }
    }
}

/// Stable bottom-up merge sort with a fallible comparator, like Python's `list.sort` (which
/// only asks `right < left`). Never panics on inconsistent orderings. On Err the list is lost.
pub fn try_sort_by<T, F>(items: &mut Vec<T>, mut cmp: F) -> Result<(), String>
where
    F: FnMut(&T, &T) -> Result<Ordering, String>,
{
    let n = items.len();
    if n < 2 {
        return Ok(());
    }
    let src: Vec<T> = std::mem::take(items);
    let mut idx: Vec<usize> = (0..n).collect();
    let mut buf: Vec<usize> = vec![0; n];
    let mut width = 1;
    while width < n {
        let mut start = 0;
        while start < n {
            let mid = (start + width).min(n);
            let end = (start + 2 * width).min(n);
            let (mut l, mut r, mut k) = (start, mid, start);
            while l < mid && r < end {
                if cmp(&src[idx[r]], &src[idx[l]])? == Ordering::Less {
                    buf[k] = idx[r];
                    r += 1;
                } else {
                    buf[k] = idx[l];
                    l += 1;
                }
                k += 1;
            }
            while l < mid {
                buf[k] = idx[l];
                l += 1;
                k += 1;
            }
            while r < end {
                buf[k] = idx[r];
                r += 1;
                k += 1;
            }
            start += 2 * width;
        }
        std::mem::swap(&mut idx, &mut buf);
        width *= 2;
    }
    let mut slots: Vec<Option<T>> = src.into_iter().map(Some).collect();
    *items = idx.into_iter().map(|i| slots[i].take().unwrap()).collect();
    Ok(())
}

/// Float sort keys compared the way Python does (no NaN reaches here: keys are finite or -inf).
pub fn cmp_f64(a: f64, b: f64) -> Ordering {
    a.partial_cmp(&b).unwrap_or(Ordering::Equal)
}

/// A Python dict with JSON-value keys: insertion-ordered, a repeated key keeps its first
/// position and takes the new value, and equal numbers of different types are one key.
#[derive(Clone, Debug, Default)]
pub struct PyDict<V> {
    pub items: Vec<(OV, V)>,
    strings: HashMap<String, usize>,
    others: Vec<usize>,
}

impl<V> PyDict<V> {
    pub fn new() -> Self {
        PyDict { items: Vec::new(), strings: HashMap::new(), others: Vec::new() }
    }

    fn position(&self, key: &OV) -> Option<usize> {
        match key {
            OV::Str(s) => self.strings.get(s).copied(),
            _ => self.others.iter().copied().find(|&i| py_eq(&self.items[i].0, key)),
        }
    }

    pub fn get(&self, key: &OV) -> Option<&V> {
        self.position(key).map(|i| &self.items[i].1)
    }

    pub fn contains(&self, key: &OV) -> bool {
        self.position(key).is_some()
    }

    /// `d[key] = value`; the caller checks hashability first.
    pub fn insert(&mut self, key: OV, value: V) {
        match self.position(&key) {
            Some(i) => self.items[i].1 = value,
            None => {
                let i = self.items.len();
                match &key {
                    OV::Str(s) => {
                        self.strings.insert(s.clone(), i);
                    }
                    _ => self.others.push(i),
                }
                self.items.push((key, value));
            }
        }
    }
}

/// `json.dumps` of a dict whose keys may be non-strings (they become strings, as Python does).
pub fn dict_to_obj(d: &PyDict<OV>) -> Obj {
    let mut out = Obj::new();
    for (k, v) in &d.items {
        let key = match k {
            OV::Str(s) => s.clone(),
            OV::Null => "null".into(),
            OV::Bool(b) => if *b { "true".into() } else { "false".into() },
            OV::Float(f) => crate::ojson::py_float_repr(*f, true),
            other => crate::ojson::dumps(other, true),
        };
        // json.dumps writes every pair, even when two keys print alike.
        out.0.push((key, v.clone()));
    }
    out
}
