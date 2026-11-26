//! Unicode character properties (tables generated from the UCD).

mod tables;

pub use tables::UNICODE_VERSION;

fn in_table(table: &[(u32, u32)], c: u32) -> bool {
    table
        .binary_search_by(|&(lo, hi)| {
            if hi < c {
                core::cmp::Ordering::Less
            } else if lo > c {
                core::cmp::Ordering::Greater
            } else {
                core::cmp::Ordering::Equal
            }
        })
        .is_ok()
}

/// ID_Start (identifiers may also start with `$` and `_`).
pub fn is_id_start(c: char) -> bool {
    if c.is_ascii() {
        return c.is_ascii_alphabetic();
    }
    in_table(tables::ID_START, c as u32)
}

/// ID_Continue (identifiers may also contain `$`, ZWNJ and ZWJ).
pub fn is_id_continue(c: char) -> bool {
    if c.is_ascii() {
        return c.is_ascii_alphanumeric() || c == '_';
    }
    in_table(tables::ID_CONTINUE, c as u32)
}

/// WhiteSpace of ECMA-262 §12.2 (TAB, VT, FF, ZWNBSP and Zs).
pub fn is_js_whitespace(c: char) -> bool {
    matches!(c, '\t' | '\u{B}' | '\u{C}' | ' ' | '\u{A0}' | '\u{FEFF}')
        || (!c.is_ascii() && in_table(tables::SPACE_SEPARATOR, c as u32))
}

/// LineTerminator of ECMA-262 §12.3.
pub fn is_line_terminator(c: char) -> bool {
    matches!(c, '\n' | '\r' | '\u{2028}' | '\u{2029}')
}
