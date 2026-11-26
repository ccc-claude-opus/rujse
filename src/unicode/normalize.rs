//! Unicode normalization (UAX #15) for `String.prototype.normalize`:
//! decomposition (canonical or compatibility, applied recursively),
//! canonical ordering of combining marks, and canonical composition.
//! Hangul syllables are handled algorithmically.

use alloc::vec::Vec;

use super::normalize_tables::{CCC, COMPOSE, DECOMP, DECOMP_DATA};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Form {
    Nfc,
    Nfd,
    Nfkc,
    Nfkd,
}

const S_BASE: u32 = 0xAC00;
const L_BASE: u32 = 0x1100;
const V_BASE: u32 = 0x1161;
const T_BASE: u32 = 0x11A7;
const L_COUNT: u32 = 19;
const V_COUNT: u32 = 21;
const T_COUNT: u32 = 28;
const N_COUNT: u32 = V_COUNT * T_COUNT;
const S_COUNT: u32 = L_COUNT * N_COUNT;

/// Canonical combining class.
pub fn ccc(c: u32) -> u8 {
    match CCC.binary_search_by_key(&c, |&(cp, _)| cp) {
        Ok(i) => CCC[i].1,
        Err(_) => 0,
    }
}

fn decompose(c: u32, compat: bool, out: &mut Vec<u32>) {
    if (S_BASE..S_BASE + S_COUNT).contains(&c) {
        let s = c - S_BASE;
        out.push(L_BASE + s / N_COUNT);
        out.push(V_BASE + (s % N_COUNT) / T_COUNT);
        let t = s % T_COUNT;
        if t != 0 {
            out.push(T_BASE + t);
        }
        return;
    }
    if let Ok(i) = DECOMP.binary_search_by_key(&c, |e| e.0) {
        let (_, start, len, is_compat) = DECOMP[i];
        if compat || !is_compat {
            for &d in &DECOMP_DATA[start as usize..start as usize + len as usize] {
                decompose(d, compat, out);
            }
            return;
        }
    }
    out.push(c);
}

/// Sorts each run of non-starters by combining class (stable).
fn canonical_order(cs: &mut [u32]) {
    let mut i = 0;
    while i < cs.len() {
        if ccc(cs[i]) == 0 {
            i += 1;
            continue;
        }
        let start = i;
        while i < cs.len() && ccc(cs[i]) != 0 {
            i += 1;
        }
        cs[start..i].sort_by_key(|&c| ccc(c));
    }
}

fn compose_pair(a: u32, b: u32) -> Option<u32> {
    // Hangul LV and LVT.
    if (L_BASE..L_BASE + L_COUNT).contains(&a) && (V_BASE..V_BASE + V_COUNT).contains(&b) {
        return Some(S_BASE + ((a - L_BASE) * V_COUNT + (b - V_BASE)) * T_COUNT);
    }
    if (S_BASE..S_BASE + S_COUNT).contains(&a) && (a - S_BASE) % T_COUNT == 0 && (T_BASE + 1..T_BASE + T_COUNT).contains(&b) {
        return Some(a + (b - T_BASE));
    }
    COMPOSE.binary_search_by(|&(x, y, _)| (x, y).cmp(&(a, b))).ok().map(|i| COMPOSE[i].2)
}

/// Canonical composition of a decomposed, canonically ordered sequence.
fn compose(cs: &mut Vec<u32>) {
    if cs.is_empty() {
        return;
    }
    let mut starter_pos = 0;
    let mut starter = cs[0];
    // A sequence that starts with a non-starter has nothing to compose
    // it with.
    let mut last_class: u16 = if ccc(starter) == 0 { 0 } else { 256 };
    let mut out_len = 1;
    for i in 1..cs.len() {
        let c = cs[i];
        let class = ccc(c) as u16;
        match compose_pair(starter, c) {
            // Blocked unless nothing in between or everything in between
            // has a lower class.
            Some(comp) if last_class < class || last_class == 0 => {
                cs[starter_pos] = comp;
                starter = comp;
            }
            _ => {
                if class == 0 {
                    starter_pos = out_len;
                    starter = c;
                }
                last_class = class;
                cs[out_len] = c;
                out_len += 1;
            }
        }
    }
    cs.truncate(out_len);
}

/// Normalizes UTF-16 code units (lone surrogates pass through).
pub fn normalize(units: &[u16], form: Form) -> Vec<u16> {
    // Nothing below U+0300 decomposes or combines except Latin-1 letters
    // with marks (and compatibility characters), so ASCII is unchanged.
    if units.iter().all(|&u| u < 0x80) {
        return units.to_vec();
    }
    let compat = matches!(form, Form::Nfkc | Form::Nfkd);
    let mut cps: Vec<u32> = Vec::with_capacity(units.len());
    for r in char::decode_utf16(units.iter().copied()) {
        match r {
            Ok(c) => decompose(c as u32, compat, &mut cps),
            Err(e) => cps.push(e.unpaired_surrogate() as u32),
        }
    }
    canonical_order(&mut cps);
    if matches!(form, Form::Nfc | Form::Nfkc) {
        compose(&mut cps);
    }
    let mut out = Vec::with_capacity(cps.len());
    for c in cps {
        match char::from_u32(c) {
            Some(ch) => {
                let mut b = [0u16; 2];
                out.extend_from_slice(ch.encode_utf16(&mut b));
            }
            None => out.push(c as u16), // a lone surrogate
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn n(s: &str, f: Form) -> alloc::string::String {
        let u: Vec<u16> = s.encode_utf16().collect();
        alloc::string::String::from_utf16(&normalize(&u, f)).unwrap()
    }

    #[test]
    fn forms() {
        assert_eq!(n("\u{E1}", Form::Nfd), "a\u{301}");
        assert_eq!(n("a\u{301}", Form::Nfc), "\u{E1}");
        assert_eq!(n("\u{1E9B}\u{323}", Form::Nfc), "\u{1E9B}\u{323}");
        assert_eq!(n("\u{1E9B}\u{323}", Form::Nfd), "\u{17F}\u{323}\u{307}");
        assert_eq!(n("\u{1E9B}\u{323}", Form::Nfkc), "\u{1E69}");
        assert_eq!(n("\u{1E9B}\u{323}", Form::Nfkd), "s\u{323}\u{307}");
        assert_eq!(n("\u{D55C}", Form::Nfd), "\u{1112}\u{1161}\u{11AB}");
        assert_eq!(n("\u{1112}\u{1161}\u{11AB}", Form::Nfc), "\u{D55C}");
        assert_eq!(n("\u{FB01}", Form::Nfkc), "fi");
        assert_eq!(n("\u{2126}", Form::Nfc), "\u{3A9}");
    }
}
