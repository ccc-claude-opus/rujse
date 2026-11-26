//! Unicode property escapes (`\\p{…}`) for regular expressions.

use crate::regex::CharSet;

/// The code points of a property (`name` or `name=value`); None if unknown.
pub fn lookup(name: &str, _unicode_sets: bool) -> Option<CharSet> {
    let set = |r: &[(u32, u32)]| Some(CharSet { ranges: r.to_vec(), strings: alloc::vec::Vec::new() });
    match name {
        "Any" => set(&[(0, 0x10FFFF)]),
        "ASCII" => set(&[(0, 0x7F)]),
        "ASCII_Hex_Digit" | "AHex" => set(&[(0x30, 0x39), (0x41, 0x46), (0x61, 0x66)]),
        _ => None,
    }
}
