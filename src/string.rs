//! JavaScript strings: sequences of UTF-16 code units, stored as Latin-1
//! bytes when every unit is below 0x100 (most strings), else as UTF-16.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;
use core::cell::OnceCell;

use crate::value::StrId;

/// Flat storage of the code units.
#[derive(Clone, Debug)]
pub enum StrData {
    Latin1(Box<[u8]>),
    Utf16(Box<[u16]>),
}

/// How a string is held: flat, or as a concatenation not yet copied.
#[derive(Clone, Debug)]
pub enum Repr {
    Flat(StrData),
    Rope(Box<Rope>),
}

/// `left + right`, built by `+` in O(1). `Vm::str` copies the units into
/// `flat` on first access; from then on the children are not needed (the
/// GC stops tracing them).
#[derive(Clone, Debug)]
pub struct Rope {
    pub left: StrId,
    pub right: StrId,
    pub len: u32,
    pub latin1: bool,
    pub flat: OnceCell<StrData>,
}

/// The longest string (code units); longer ones are a RangeError.
pub const MAX_LEN: usize = 1 << 29;

/// Concatenations at least this long become ropes.
pub const ROPE_MIN: usize = 32;

#[derive(Clone, Debug)]
pub struct JsStr {
    pub repr: Repr,
    /// Hash of the code units (0 until computed; see `hash_units`).
    pub hash: u32,
    /// The string is in the atom table (used as a property key).
    pub atom: bool,
}

impl JsStr {
    pub fn from_str(s: &str) -> JsStr {
        if s.is_ascii() {
            return JsStr::latin1(s.as_bytes().into());
        }
        if s.chars().all(|c| (c as u32) < 0x100) {
            return JsStr::latin1(s.chars().map(|c| c as u8).collect());
        }
        JsStr::utf16(s.encode_utf16().collect())
    }

    pub fn latin1(b: Box<[u8]>) -> JsStr {
        JsStr { repr: Repr::Flat(StrData::Latin1(b)), hash: 0, atom: false }
    }

    pub fn utf16(u: Box<[u16]>) -> JsStr {
        JsStr { repr: Repr::Flat(StrData::Utf16(u)), hash: 0, atom: false }
    }

    /// From UTF-16 units, choosing Latin-1 storage when possible.
    pub fn from_units(u: &[u16]) -> JsStr {
        if u.iter().all(|&c| c < 0x100) {
            JsStr::latin1(u.iter().map(|&c| c as u8).collect())
        } else {
            JsStr::utf16(u.into())
        }
    }

    pub fn from_units_vec(u: Vec<u16>) -> JsStr {
        if u.iter().all(|&c| c < 0x100) {
            JsStr::latin1(u.iter().map(|&c| c as u8).collect())
        } else {
            JsStr::utf16(u.into_boxed_slice())
        }
    }

    pub fn rope(left: StrId, right: StrId, len: usize, latin1: bool) -> JsStr {
        let r = Rope { left, right, len: len as u32, latin1, flat: OnceCell::new() };
        JsStr { repr: Repr::Rope(Box::new(r)), hash: 0, atom: false }
    }

    /// The rope, while its units have not been copied yet.
    #[inline]
    pub fn unflattened(&self) -> Option<&Rope> {
        match &self.repr {
            Repr::Rope(r) if r.flat.get().is_none() => Some(r),
            _ => None,
        }
    }

    /// The flat units. Strings reached through `Vm::str` are always flat.
    #[inline]
    fn d(&self) -> &StrData {
        match &self.repr {
            Repr::Flat(d) => d,
            Repr::Rope(r) => r.flat.get().expect("rope read before Vm::str flattened it"),
        }
    }

    #[inline]
    pub fn len(&self) -> usize {
        match &self.repr {
            Repr::Rope(r) => r.len as usize,
            Repr::Flat(StrData::Latin1(b)) => b.len(),
            Repr::Flat(StrData::Utf16(u)) => u.len(),
        }
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    #[inline]
    pub fn at(&self, i: usize) -> u16 {
        match self.d() {
            StrData::Latin1(b) => b[i] as u16,
            StrData::Utf16(u) => u[i],
        }
    }

    pub fn units(&self) -> Units<'_> {
        Units { s: self, i: 0 }
    }

    pub fn to_units(&self) -> Vec<u16> {
        self.units().collect()
    }

    pub fn is_latin1(&self) -> bool {
        match &self.repr {
            Repr::Rope(r) => r.latin1,
            Repr::Flat(d) => matches!(d, StrData::Latin1(_)),
        }
    }

    /// Appends the units to a Latin-1 buffer (the string must be flat and
    /// Latin-1).
    pub fn push_latin1(&self, out: &mut Vec<u8>) {
        if let StrData::Latin1(b) = self.d() {
            out.extend_from_slice(b);
        }
    }

    /// Appends the units to a UTF-16 buffer (the string must be flat).
    pub fn push_units(&self, out: &mut Vec<u16>) {
        match self.d() {
            StrData::Latin1(b) => out.extend(b.iter().map(|&c| c as u16)),
            StrData::Utf16(u) => out.extend_from_slice(u),
        }
    }

    /// The bytes, when every unit is ASCII (zero-copy `&str`).
    pub fn as_ascii(&self) -> Option<&str> {
        match self.d() {
            StrData::Latin1(b) if b.is_ascii() => core::str::from_utf8(b).ok(),
            _ => None,
        }
    }

    /// Converts to Rust UTF-8, replacing lone surrogates with U+FFFD.
    pub fn to_rust_lossy(&self) -> String {
        match self.d() {
            StrData::Latin1(b) => b.iter().map(|&c| c as char).collect(),
            StrData::Utf16(u) => char::decode_utf16(u.iter().copied())
                .map(|r| r.unwrap_or(char::REPLACEMENT_CHARACTER))
                .collect(),
        }
    }

    /// WTF-8: like UTF-8 but lone surrogates are encoded as 3-byte
    /// sequences (round-trips every JS string).
    pub fn to_wtf8(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.len());
        let units: Vec<u16> = self.to_units();
        for r in char::decode_utf16(units.iter().copied()) {
            match r {
                Ok(c) => {
                    let mut b = [0u8; 4];
                    out.extend_from_slice(c.encode_utf8(&mut b).as_bytes());
                }
                Err(e) => {
                    let u = e.unpaired_surrogate() as u32;
                    out.push(0xE0 | (u >> 12) as u8);
                    out.push(0x80 | ((u >> 6) & 0x3F) as u8);
                    out.push(0x80 | (u & 0x3F) as u8);
                }
            }
        }
        out
    }

    pub fn eq_units(&self, other: &JsStr) -> bool {
        match (self.d(), other.d()) {
            (StrData::Latin1(a), StrData::Latin1(b)) => a == b,
            (StrData::Utf16(a), StrData::Utf16(b)) => a == b,
            _ => self.len() == other.len() && self.units().eq(other.units()),
        }
    }

    pub fn eq_str(&self, s: &str) -> bool {
        match self.d() {
            StrData::Latin1(b) if s.is_ascii() => &**b == s.as_bytes(),
            _ => self.units().eq(s.encode_utf16()),
        }
    }

    /// Code-unit order (the `<` of strings).
    pub fn cmp_units(&self, other: &JsStr) -> core::cmp::Ordering {
        match (self.d(), other.d()) {
            (StrData::Latin1(a), StrData::Latin1(b)) => a.cmp(b),
            _ => self.units().cmp(other.units()),
        }
    }

    pub fn hash(&mut self) -> u32 {
        if self.hash == 0 {
            self.hash = hash_units(self.units());
        }
        self.hash
    }

    /// `self` is a canonical array index ("0" … "4294967294").
    pub fn array_index(&self) -> Option<u32> {
        let n = self.len();
        if n == 0 || n > 10 {
            return None;
        }
        let first = self.at(0);
        if first == b'0' as u16 {
            return if n == 1 { Some(0) } else { None };
        }
        let mut v: u64 = 0;
        for c in self.units() {
            if !(b'0' as u16..=b'9' as u16).contains(&c) {
                return None;
            }
            v = v * 10 + (c - b'0' as u16) as u64;
        }
        if v < u32::MAX as u64 { Some(v as u32) } else { None }
    }

    pub fn concat(&self, other: &JsStr) -> JsStr {
        match (self.d(), other.d()) {
            (StrData::Latin1(a), StrData::Latin1(b)) => {
                let mut v = Vec::with_capacity(a.len() + b.len());
                v.extend_from_slice(a);
                v.extend_from_slice(b);
                JsStr::latin1(v.into_boxed_slice())
            }
            _ => {
                let mut v = Vec::with_capacity(self.len() + other.len());
                v.extend(self.units());
                v.extend(other.units());
                JsStr::utf16(v.into_boxed_slice())
            }
        }
    }

    pub fn slice(&self, start: usize, end: usize) -> JsStr {
        match self.d() {
            StrData::Latin1(b) => JsStr::latin1(b[start..end].into()),
            StrData::Utf16(u) => JsStr::from_units(&u[start..end]),
        }
    }
}

/// FNV-1a over code units (the same for Latin-1 and UTF-16 storage).
pub fn hash_units(units: impl Iterator<Item = u16>) -> u32 {
    let mut h: u32 = 0x811C_9DC5;
    for u in units {
        h ^= u as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    if h == 0 { 1 } else { h }
}

pub struct Units<'a> {
    s: &'a JsStr,
    i: usize,
}

impl Iterator for Units<'_> {
    type Item = u16;
    #[inline]
    fn next(&mut self) -> Option<u16> {
        if self.i < self.s.len() {
            let c = self.s.at(self.i);
            self.i += 1;
            Some(c)
        } else {
            None
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let n = self.s.len() - self.i;
        (n, Some(n))
    }
}

impl ExactSizeIterator for Units<'_> {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basics() {
        let a = JsStr::from_str("héllo");
        assert!(a.is_latin1());
        assert_eq!(a.len(), 5);
        let b = JsStr::from_str("€");
        assert!(!b.is_latin1());
        let c = a.concat(&b);
        assert_eq!(c.to_rust_lossy(), "héllo€");
        assert!(JsStr::from_str("12").array_index() == Some(12));
        assert!(JsStr::from_str("012").array_index().is_none());
        assert!(JsStr::from_str("4294967295").array_index().is_none());
        assert_eq!(JsStr::from_str("4294967294").array_index(), Some(4294967294));
        let lone = JsStr::utf16(alloc::vec![0xD800u16].into());
        assert_eq!(lone.to_wtf8(), [0xED, 0xA0, 0x80]);
        assert_eq!(hash_units(a.units()), hash_units(JsStr::utf16(a.to_units().into()).units()));
    }
}
