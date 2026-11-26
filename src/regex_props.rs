//! Unicode property escapes (`\p{…}`) for regular expressions.

use crate::regex::CharSet;
use crate::regex_props_tables::{BINARY_ALIASES, GC_ALIASES, SC_ALIASES, TABLES};

fn table(name: &str) -> Option<CharSet> {
    let i = TABLES.binary_search_by(|(n, _)| (*n).cmp(name)).ok()?;
    Some(CharSet { ranges: TABLES[i].1.to_vec(), strings: alloc::vec::Vec::new() })
}

fn alias(list: &[(&str, &'static str)], name: &str) -> Option<&'static str> {
    list.iter().find(|(a, _)| *a == name).map(|x| x.1)
}

/// The code points of a property (`name` or `name=value`); None if
/// unknown. Names and values must match exactly (case-sensitive).
pub fn lookup(name: &str, _unicode_sets: bool) -> Option<CharSet> {
    if let Some((p, v)) = name.split_once('=') {
        return match p {
            "General_Category" | "gc" => table(&alloc::format!("G_{}", alias(GC_ALIASES, v)?)),
            "Script" | "sc" => table(&alloc::format!("S_{}", alias(SC_ALIASES, v)?)),
            "Script_Extensions" | "scx" => table(&alloc::format!("X_{}", alias(SC_ALIASES, v)?)),
            _ => None,
        };
    }
    if let Some(g) = alias(GC_ALIASES, name) {
        return table(&alloc::format!("G_{g}"));
    }
    let b = alias(BINARY_ALIASES, name)?;
    if b == "Any" {
        return Some(CharSet { ranges: alloc::vec![(0, 0x10FFFF)], strings: alloc::vec::Vec::new() });
    }
    table(&alloc::format!("B_{b}"))
}
