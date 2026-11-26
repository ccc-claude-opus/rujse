//! Regular expressions (ECMA-262 §22.2): parser with the early errors,
//! compiler to a small bytecode, and a backtracking matcher with an
//! explicit stack (no recursion except for lookarounds) and a step limit.
//!
//! Strings are UTF-16 code units; with the `u` or `v` flag the pattern and
//! the input are read as code points (surrogate pairs combined).

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct Flags {
    pub global: bool,
    pub ignore_case: bool,
    pub multiline: bool,
    pub dot_all: bool,
    pub unicode: bool,
    pub unicode_sets: bool,
    pub sticky: bool,
    pub has_indices: bool,
}

impl Flags {
    pub fn parse(s: &str) -> Option<Flags> {
        let mut f = Flags::default();
        for c in s.chars() {
            let slot = match c {
                'g' => &mut f.global,
                'i' => &mut f.ignore_case,
                'm' => &mut f.multiline,
                's' => &mut f.dot_all,
                'u' => &mut f.unicode,
                'v' => &mut f.unicode_sets,
                'y' => &mut f.sticky,
                'd' => &mut f.has_indices,
                _ => return None,
            };
            if *slot {
                return None;
            }
            *slot = true;
        }
        if f.unicode && f.unicode_sets {
            return None;
        }
        Some(f)
    }

    fn either_unicode(&self) -> bool {
        self.unicode || self.unicode_sets
    }
}

/// A set of code points (sorted, merged ranges) plus strings (`v` mode).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CharSet {
    pub ranges: Vec<(u32, u32)>,
    pub strings: Vec<Vec<u32>>,
}

impl CharSet {
    fn single(c: u32) -> CharSet {
        CharSet { ranges: vec![(c, c)], strings: Vec::new() }
    }

    fn add_range(&mut self, a: u32, b: u32) {
        self.ranges.push((a, b));
    }

    fn normalize(&mut self) {
        self.ranges.sort();
        let mut out: Vec<(u32, u32)> = Vec::new();
        for &(a, b) in &self.ranges {
            if let Some(last) = out.last_mut() {
                if a <= last.1.saturating_add(1) {
                    last.1 = last.1.max(b);
                    continue;
                }
            }
            out.push((a, b));
        }
        self.ranges = out;
        self.strings.sort();
        self.strings.dedup();
    }

    fn contains(&self, c: u32) -> bool {
        self.ranges
            .binary_search_by(|&(a, b)| {
                if b < c {
                    core::cmp::Ordering::Less
                } else if a > c {
                    core::cmp::Ordering::Greater
                } else {
                    core::cmp::Ordering::Equal
                }
            })
            .is_ok()
    }

    fn complement(&self, max: u32) -> CharSet {
        let mut out = Vec::new();
        let mut next = 0u32;
        for &(a, b) in &self.ranges {
            if a > next {
                out.push((next, a - 1));
            }
            next = b + 1;
        }
        if next <= max {
            out.push((next, max));
        }
        CharSet { ranges: out, strings: Vec::new() }
    }

    fn union(&mut self, o: &CharSet) {
        self.ranges.extend_from_slice(&o.ranges);
        self.strings.extend(o.strings.iter().cloned());
        self.normalize();
    }

    fn intersect(&self, o: &CharSet) -> CharSet {
        let mut out = Vec::new();
        for &(a, b) in &self.ranges {
            for &(c, d) in &o.ranges {
                let lo = a.max(c);
                let hi = b.min(d);
                if lo <= hi {
                    out.push((lo, hi));
                }
            }
        }
        let strings = self.strings.iter().filter(|s| o.strings.contains(s)).cloned().collect();
        let mut r = CharSet { ranges: out, strings };
        r.normalize();
        r
    }

    fn subtract(&self, o: &CharSet) -> CharSet {
        let comp = o.complement(0x10FFFF);
        let mut r = CharSet { ranges: self.intersect(&comp).ranges, strings: self.strings.iter().filter(|s| !o.strings.contains(s)).cloned().collect() };
        r.normalize();
        r
    }
}

#[derive(Debug, Clone)]
enum Node {
    Char(u32),
    /// `.`
    Any,
    Class(Box<CharSet>, bool),
    Seq(Vec<Node>),
    Alt(Vec<Node>),
    Group(Box<Node>, Option<usize>),
    Repeat { node: Box<Node>, min: u32, max: Option<u32>, greedy: bool },
    Start,
    End,
    WordB(bool),
    Look { node: Box<Node>, ahead: bool, negate: bool },
    BackRef(Vec<usize>),
    /// Modifiers `(?ims-ims:…)`: flags to set (i, m, s) and their values.
    Mods { node: Box<Node>, set: [Option<bool>; 3] },
}

pub struct Parser<'a> {
    s: &'a [u32],
    i: usize,
    flags: Flags,
    ngroups: usize,
    group_names: Vec<(Vec<u32>, usize)>,
    /// The pattern has named groups (enables \k in non-unicode mode).
    has_names: bool,
    next_group: usize,
    /// Names declared in the current alternative chain (duplicate check).
    names_seen: Vec<Vec<(Vec<u32>, usize)>>,
    backrefs_named: Vec<Vec<u32>>,
}

type PResult<T> = Result<T, String>;

fn is_syntax_char(c: u32) -> bool {
    matches!(char::from_u32(c), Some('^' | '$' | '\\' | '.' | '*' | '+' | '?' | '(' | ')' | '[' | ']' | '{' | '}' | '|'))
}

fn is_line_term(c: u32) -> bool {
    matches!(c, 0x0A | 0x0D | 0x2028 | 0x2029)
}

fn is_word(c: u32) -> bool {
    matches!(char::from_u32(c), Some('a'..='z' | 'A'..='Z' | '0'..='9' | '_'))
}

fn space_set() -> CharSet {
    let mut s = CharSet::default();
    for c in [0x09u32, 0x0A, 0x0B, 0x0C, 0x0D, 0x20, 0xA0, 0x1680, 0x2028, 0x2029, 0x202F, 0x205F, 0x3000, 0xFEFF] {
        s.add_range(c, c);
    }
    s.add_range(0x2000, 0x200A);
    s.normalize();
    s
}

fn digit_set() -> CharSet {
    CharSet { ranges: vec![(0x30, 0x39)], strings: Vec::new() }
}

fn word_set(flags: Flags) -> CharSet {
    let mut s = CharSet { ranges: vec![(0x30, 0x39), (0x41, 0x5A), (0x5F, 0x5F), (0x61, 0x7A)], strings: Vec::new() };
    if flags.ignore_case && flags.either_unicode() {
        // Characters that fold into the word set (ſ, K sign).
        s.add_range(0x017F, 0x017F);
        s.add_range(0x212A, 0x212A);
        s.normalize();
    }
    s
}

impl<'a> Parser<'a> {
    fn peek(&self) -> Option<u32> {
        self.s.get(self.i).copied()
    }

    fn peek_at(&self, k: usize) -> Option<u32> {
        self.s.get(self.i + k).copied()
    }

    fn eat(&mut self, c: char) -> bool {
        if self.peek() == Some(c as u32) {
            self.i += 1;
            true
        } else {
            false
        }
    }

    fn u(&self) -> bool {
        self.flags.either_unicode()
    }

    fn err<T>(&self, m: &str) -> PResult<T> {
        Err(String::from(m))
    }

    fn disjunction(&mut self) -> PResult<Node> {
        let mut alts = Vec::new();
        self.names_seen.push(Vec::new());
        let mut branch_names: Vec<Vec<(Vec<u32>, usize)>> = Vec::new();
        loop {
            let mark = self.names_seen.last().unwrap().len();
            alts.push(self.alternative()?);
            let added: Vec<(Vec<u32>, usize)> = self.names_seen.last_mut().unwrap().drain(mark..).collect();
            branch_names.push(added);
            if !self.eat('|') {
                break;
            }
        }
        self.names_seen.pop();
        // Names from all branches are visible to the enclosing sequence.
        if let Some(outer) = self.names_seen.last_mut() {
            for b in branch_names {
                outer.extend(b);
            }
        }
        Ok(if alts.len() == 1 { alts.pop().unwrap() } else { Node::Alt(alts) })
    }

    fn alternative(&mut self) -> PResult<Node> {
        let mut seq = Vec::new();
        while let Some(c) = self.peek() {
            if c == '|' as u32 || c == ')' as u32 {
                break;
            }
            seq.push(self.term()?);
        }
        Ok(if seq.len() == 1 { seq.pop().unwrap() } else { Node::Seq(seq) })
    }

    fn term(&mut self) -> PResult<Node> {
        let c = self.peek().unwrap();
        // Assertions.
        match char::from_u32(c) {
            Some('^') => {
                self.i += 1;
                return Ok(Node::Start);
            }
            Some('$') => {
                self.i += 1;
                return Ok(Node::End);
            }
            Some('\\') if matches!(self.peek_at(1), Some(0x62 | 0x42)) => {
                let b = self.peek_at(1) == Some(0x62);
                self.i += 2;
                return Ok(Node::WordB(b));
            }
            Some('(') if self.peek_at(1) == Some('?' as u32) => {
                let k = self.peek_at(2);
                let look = match k.and_then(char::from_u32) {
                    Some('=') => Some((true, false, 3)),
                    Some('!') => Some((true, true, 3)),
                    Some('<') if matches!(self.peek_at(3), Some(0x3D | 0x21)) => Some((false, self.peek_at(3) == Some(0x21), 4)),
                    _ => None,
                };
                if let Some((ahead, negate, len)) = look {
                    self.i += len;
                    let node = self.disjunction()?;
                    if !self.eat(')') {
                        return self.err("missing )");
                    }
                    let n = Node::Look { node: Box::new(node), ahead, negate };
                    // Annex B: lookaheads may be quantified in non-unicode mode.
                    if ahead && !self.u() {
                        return self.quantifier(n);
                    }
                    if self.at_quantifier() {
                        return self.err("nothing to repeat");
                    }
                    return Ok(n);
                }
            }
            _ => {}
        }
        let atom = self.atom()?;
        self.quantifier(atom)
    }

    fn at_quantifier(&self) -> bool {
        match self.peek().and_then(char::from_u32) {
            Some('*' | '+' | '?') => true,
            Some('{') => self.try_braces().is_some(),
            _ => false,
        }
    }

    /// `{n}`, `{n,}`, `{n,m}` at the current position: (min, max, length).
    fn try_braces(&self) -> Option<(u32, Option<u32>, usize)> {
        let mut j = self.i + 1;
        let num = |j: &mut usize| -> Option<u32> {
            let st = *j;
            let mut v: u64 = 0;
            while let Some(&d) = self.s.get(*j) {
                if !(0x30..=0x39).contains(&d) {
                    break;
                }
                v = (v * 10 + (d - 0x30) as u64).min(u32::MAX as u64);
                *j += 1;
            }
            if *j == st { None } else { Some(v as u32) }
        };
        let min = num(&mut j)?;
        let max;
        if self.s.get(j) == Some(&(',' as u32)) {
            j += 1;
            if self.s.get(j) == Some(&('}' as u32)) {
                max = None;
            } else {
                max = Some(num(&mut j)?);
            }
        } else {
            max = Some(min);
        }
        if self.s.get(j) != Some(&('}' as u32)) {
            return None;
        }
        Some((min, max, j + 1 - self.i))
    }

    fn quantifier(&mut self, atom: Node) -> PResult<Node> {
        let (min, max) = match self.peek().and_then(char::from_u32) {
            Some('*') => {
                self.i += 1;
                (0, None)
            }
            Some('+') => {
                self.i += 1;
                (1, None)
            }
            Some('?') => {
                self.i += 1;
                (0, Some(1))
            }
            Some('{') => match self.try_braces() {
                Some((a, b, len)) => {
                    if let Some(b) = b {
                        if b < a {
                            return self.err("numbers out of order in {} quantifier");
                        }
                    }
                    self.i += len;
                    (a, b)
                }
                None => {
                    if self.u() {
                        return self.err("incomplete quantifier");
                    }
                    return Ok(atom);
                }
            },
            _ => return Ok(atom),
        };
        let greedy = !self.eat('?');
        if matches!(atom, Node::Start | Node::End | Node::WordB(_)) || matches!(atom, Node::Look { ahead: false, .. }) {
            return self.err("nothing to repeat");
        }
        Ok(Node::Repeat { node: Box::new(atom), min, max, greedy })
    }

    fn atom(&mut self) -> PResult<Node> {
        let c = self.peek().unwrap();
        match char::from_u32(c) {
            Some('.') => {
                self.i += 1;
                Ok(Node::Any)
            }
            Some('(') => self.group(),
            Some('[') => {
                self.i += 1;
                if self.flags.unicode_sets {
                    let (set, neg) = self.class_v()?;
                    Ok(Node::Class(Box::new(set), neg))
                } else {
                    let (set, neg) = self.class()?;
                    Ok(Node::Class(Box::new(set), neg))
                }
            }
            Some('\\') => {
                self.i += 1;
                self.atom_escape()
            }
            Some('*' | '+' | '?') => self.err("nothing to repeat"),
            Some('{') => {
                if self.u() {
                    return self.err("lone quantifier brackets");
                }
                if self.try_braces().is_some() {
                    return self.err("nothing to repeat");
                }
                self.i += 1;
                Ok(Node::Char(c))
            }
            Some('}' | ']') => {
                if self.u() {
                    return self.err("lone quantifier brackets");
                }
                self.i += 1;
                Ok(Node::Char(c))
            }
            Some(')') => self.err("unmatched )"),
            _ => {
                self.i += 1;
                Ok(Node::Char(c))
            }
        }
    }

    fn group_name(&mut self) -> PResult<Vec<u32>> {
        // After `<`; up to `>`.
        let mut name = Vec::new();
        loop {
            let Some(mut c) = self.peek() else { return self.err("invalid capture group name") };
            self.i += 1;
            if c == '>' as u32 {
                break;
            }
            if c == '\\' as u32 {
                if !self.eat('u') {
                    return self.err("invalid capture group name");
                }
                c = self.unicode_escape_in_name()?;
            } else if (0xD800..0xDC00).contains(&c) {
                // A surrogate pair written directly (non-unicode patterns).
                if let Some(lo) = self.peek() {
                    if (0xDC00..0xE000).contains(&lo) {
                        self.i += 1;
                        c = 0x10000 + ((c - 0xD800) << 10) + (lo - 0xDC00);
                    }
                }
            }
            let ch = char::from_u32(c).ok_or_else(|| String::from("invalid capture group name"))?;
            let ok = if name.is_empty() {
                ch == '$' || ch == '_' || crate::unicode::is_id_start(ch)
            } else {
                ch == '$' || ch == '\u{200C}' || ch == '\u{200D}' || crate::unicode::is_id_continue(ch)
            };
            if !ok {
                return self.err("invalid capture group name");
            }
            name.push(c);
        }
        if name.is_empty() {
            return self.err("invalid capture group name");
        }
        Ok(name)
    }

    fn unicode_escape_in_name(&mut self) -> PResult<u32> {
        if self.eat('{') {
            let mut v: u32 = 0;
            let mut n = 0;
            while let Some(d) = self.peek().and_then(|c| char::from_u32(c)).and_then(|c| c.to_digit(16)) {
                v = v.saturating_mul(16).saturating_add(d);
                self.i += 1;
                n += 1;
            }
            if n == 0 || !self.eat('}') || v > 0x10FFFF {
                return self.err("invalid unicode escape");
            }
            return Ok(v);
        }
        let hex4 = |p: &mut Self| -> Option<u32> {
            let mut v = 0;
            for _ in 0..4 {
                let d = char::from_u32(p.peek()?)?.to_digit(16)?;
                v = v * 16 + d;
                p.i += 1;
            }
            Some(v)
        };
        let hi = hex4(self).ok_or_else(|| String::from("invalid unicode escape"))?;
        if (0xD800..0xDC00).contains(&hi) && self.peek() == Some('\\' as u32) && self.peek_at(1) == Some('u' as u32) {
            let save = self.i;
            self.i += 2;
            if let Some(lo) = hex4(self) {
                if (0xDC00..0xE000).contains(&lo) {
                    return Ok(0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00));
                }
            }
            self.i = save;
        }
        Ok(hi)
    }

    fn group(&mut self) -> PResult<Node> {
        self.i += 1; // (
        if self.eat('?') {
            if self.eat(':') {
                let n = self.disjunction()?;
                if !self.eat(')') {
                    return self.err("missing )");
                }
                return Ok(Node::Group(Box::new(n), None));
            }
            if self.eat('<') {
                let name = self.group_name()?;
                let idx = self.next_group;
                self.next_group += 1;
                // Duplicate names only in different alternatives.
                for scope in &self.names_seen {
                    if scope.iter().any(|(n, _)| *n == name) {
                        return self.err("duplicate capture group name");
                    }
                }
                self.names_seen.last_mut().unwrap().push((name.clone(), idx));
                self.group_names.push((name, idx));
                let n = self.disjunction()?;
                if !self.eat(')') {
                    return self.err("missing )");
                }
                return Ok(Node::Group(Box::new(n), Some(idx)));
            }
            // Modifiers (?ims-ims:…)
            let mut set: [Option<bool>; 3] = [None; 3];
            let mut neg = false;
            let mut any = false;
            loop {
                let Some(c) = self.peek().and_then(char::from_u32) else { return self.err("invalid group") };
                let slot = match c {
                    'i' => 0,
                    'm' => 1,
                    's' => 2,
                    '-' if !neg => {
                        neg = true;
                        self.i += 1;
                        continue;
                    }
                    ':' => {
                        self.i += 1;
                        break;
                    }
                    _ => return self.err("invalid group"),
                };
                if set[slot].is_some() {
                    return self.err("repeated regular expression modifier");
                }
                set[slot] = Some(!neg);
                any = true;
                self.i += 1;
            }
            if !any && neg {
                return self.err("invalid regular expression modifiers");
            }
            let n = self.disjunction()?;
            if !self.eat(')') {
                return self.err("missing )");
            }
            return Ok(Node::Mods { node: Box::new(n), set });
        }
        let idx = self.next_group;
        self.next_group += 1;
        let n = self.disjunction()?;
        if !self.eat(')') {
            return self.err("missing )");
        }
        Ok(Node::Group(Box::new(n), Some(idx)))
    }

    fn atom_escape(&mut self) -> PResult<Node> {
        let Some(c) = self.peek() else { return self.err("\\ at end of pattern") };
        let ch = char::from_u32(c).unwrap_or('\0');
        match ch {
            '1'..='9' => {
                let start = self.i;
                let mut v: usize = 0;
                while let Some(d) = self.peek().filter(|d| (0x30..=0x39).contains(d)) {
                    v = v.saturating_mul(10).saturating_add((d - 0x30) as usize);
                    self.i += 1;
                }
                if v <= self.ngroups {
                    return Ok(Node::BackRef(vec![v]));
                }
                if self.u() {
                    return self.err("invalid escape");
                }
                // Annex B: legacy octal escape or identity escape.
                self.i = start;
                if ch >= '8' {
                    self.i += 1;
                    return Ok(Node::Char(c));
                }
                Ok(Node::Char(self.legacy_octal()))
            }
            'd' | 'D' | 's' | 'S' | 'w' | 'W' => {
                self.i += 1;
                let set = match ch {
                    'd' | 'D' => digit_set(),
                    's' | 'S' => space_set(),
                    _ => word_set(self.flags),
                };
                Ok(Node::Class(Box::new(set), ch.is_uppercase()))
            }
            'p' | 'P' if self.u() => {
                self.i += 1;
                let set = self.property_escape()?;
                if ch == 'P' && !set.strings.is_empty() {
                    return self.err("negated property of strings");
                }
                Ok(Node::Class(Box::new(set), ch == 'P'))
            }
            'k' if self.u() || self.has_names => {
                self.i += 1;
                if !self.eat('<') {
                    return self.err("invalid named reference");
                }
                let name = self.group_name()?;
                self.backrefs_named.push(name.clone());
                let groups: Vec<usize> = self.group_names_all(&name);
                Ok(Node::BackRef(groups))
            }
            _ => {
                let c = self.character_escape(false)?;
                Ok(Node::Char(c))
            }
        }
    }

    /// All group indices (1-based) with this name (resolved after parsing).
    fn group_names_all(&self, name: &[u32]) -> Vec<usize> {
        // Filled in later: store the name's hash in a placeholder index
        // list; resolve_named_backrefs replaces it.
        let _ = name;
        vec![usize::MAX - self.backrefs_named.len() + 1]
    }

    fn legacy_octal(&mut self) -> u32 {
        let d0 = self.peek().unwrap() - 0x30;
        self.i += 1;
        let mut v = d0;
        let oct = |c: Option<u32>| c.filter(|c| (0x30..=0x37).contains(c)).map(|c| c - 0x30);
        if let Some(d1) = oct(self.peek()) {
            v = v * 8 + d1;
            self.i += 1;
            if d0 <= 3 {
                if let Some(d2) = oct(self.peek()) {
                    v = v * 8 + d2;
                    self.i += 1;
                }
            }
        }
        v
    }

    /// CharacterEscape (after `\`); `in_class` allows `\b` and `\-`.
    fn character_escape(&mut self, in_class: bool) -> PResult<u32> {
        let c = self.peek().unwrap();
        self.i += 1;
        let ch = char::from_u32(c).unwrap_or('\0');
        Ok(match ch {
            'f' => 0x0C,
            'n' => 0x0A,
            'r' => 0x0D,
            't' => 0x09,
            'v' => 0x0B,
            'b' if in_class => 0x08,
            '-' if in_class && self.u() => '-' as u32,
            'c' => {
                if let Some(l) = self.peek().filter(|l| char::from_u32(*l).is_some_and(|x| x.is_ascii_alphabetic())) {
                    self.i += 1;
                    l % 32
                } else if !self.u() && in_class && self.peek().is_some_and(|l| (0x30..=0x39).contains(&l) || l == '_' as u32) {
                    let l = self.peek().unwrap();
                    self.i += 1;
                    l % 32
                } else {
                    if self.u() {
                        return self.err("invalid unicode escape");
                    }
                    // Annex B: `\c` is a literal backslash, `c` re-read.
                    self.i -= 1;
                    '\\' as u32
                }
            }
            '0' if !self.peek().is_some_and(|d| (0x30..=0x39).contains(&d)) => 0,
            '0'..='7' if !self.u() => {
                self.i -= 1;
                self.legacy_octal()
            }
            'x' => {
                let h = |p: &Self, k: usize| p.peek_at(k).and_then(char::from_u32).and_then(|c| c.to_digit(16));
                match (h(self, 0), h(self, 1)) {
                    (Some(a), Some(b)) => {
                        self.i += 2;
                        a * 16 + b
                    }
                    _ => {
                        if self.u() {
                            return self.err("invalid escape");
                        }
                        'x' as u32
                    }
                }
            }
            'u' => {
                let save = self.i;
                if self.u() {
                    return self.unicode_escape_in_name().map_err(|_| String::from("invalid unicode escape"));
                }
                let hex4 = |p: &mut Self| -> Option<u32> {
                    let mut v = 0;
                    for _ in 0..4 {
                        let d = char::from_u32(p.peek()?)?.to_digit(16)?;
                        v = v * 16 + d;
                        p.i += 1;
                    }
                    Some(v)
                };
                match hex4(self) {
                    Some(v) => v,
                    None => {
                        self.i = save;
                        'u' as u32
                    }
                }
            }
            _ => {
                if self.u() {
                    if is_syntax_char(c) || c == '/' as u32 {
                        c
                    } else {
                        return self.err("invalid escape");
                    }
                } else if ch == 'k' && self.has_names {
                    return self.err("invalid named reference");
                } else {
                    c
                }
            }
        })
    }

    fn property_escape(&mut self) -> PResult<CharSet> {
        if !self.eat('{') {
            return self.err("invalid property name");
        }
        let mut name = String::new();
        loop {
            let Some(c) = self.peek() else { return self.err("invalid property name") };
            self.i += 1;
            if c == '}' as u32 {
                break;
            }
            name.push(char::from_u32(c).unwrap_or('\0'));
        }
        crate::regex_props::lookup(&name, self.flags.unicode_sets).ok_or_else(|| String::from("invalid property name"))
    }

    /// A non-`v` character class (after `[`): (set, negated).
    fn class(&mut self) -> PResult<(CharSet, bool)> {
        let neg = self.eat('^');
        let mut set = CharSet::default();
        loop {
            let Some(c) = self.peek() else { return self.err("unterminated character class") };
            if c == ']' as u32 {
                self.i += 1;
                break;
            }
            let a = self.class_atom()?;
            if self.peek() == Some('-' as u32) && self.peek_at(1).is_some_and(|x| x != ']' as u32) {
                self.i += 1;
                let b = self.class_atom()?;
                match (&a, &b) {
                    (ClassAtom::Char(x), ClassAtom::Char(y)) => {
                        if x > y {
                            return self.err("range out of order in character class");
                        }
                        set.add_range(*x, *y);
                    }
                    _ => {
                        if self.u() {
                            return self.err("invalid character class");
                        }
                        // Annex B: the dash is literal.
                        a.add_to(&mut set);
                        set.add_range('-' as u32, '-' as u32);
                        b.add_to(&mut set);
                    }
                }
            } else {
                a.add_to(&mut set);
            }
        }
        set.normalize();
        Ok((set, neg))
    }

    fn class_atom(&mut self) -> PResult<ClassAtom> {
        let c = self.peek().unwrap();
        self.i += 1;
        if c != '\\' as u32 {
            return Ok(ClassAtom::Char(c));
        }
        let Some(e) = self.peek() else { return self.err("\\ at end of pattern") };
        let ch = char::from_u32(e).unwrap_or('\0');
        match ch {
            'd' | 'D' | 's' | 'S' | 'w' | 'W' => {
                self.i += 1;
                let set = match ch {
                    'd' | 'D' => digit_set(),
                    's' | 'S' => space_set(),
                    _ => word_set(self.flags),
                };
                Ok(ClassAtom::Set(if ch.is_uppercase() { set.complement(if self.u() { 0x10FFFF } else { 0xFFFF }) } else { set }))
            }
            'p' | 'P' if self.u() => {
                self.i += 1;
                let set = self.property_escape()?;
                Ok(ClassAtom::Set(if ch == 'P' { set.complement(0x10FFFF) } else { set }))
            }
            '1'..='9' if !self.u() => {
                if ch >= '8' {
                    self.i += 1;
                    return Ok(ClassAtom::Char(e));
                }
                Ok(ClassAtom::Char(self.legacy_octal()))
            }
            'k' if !self.u() => {
                self.i += 1;
                Ok(ClassAtom::Char('k' as u32))
            }
            _ => Ok(ClassAtom::Char(self.character_escape(true)?)),
        }
    }

    // ----- `v` mode classes ------------------------------------------------

    fn class_v(&mut self) -> PResult<(CharSet, bool)> {
        let neg = self.eat('^');
        let set = self.class_set_expression()?;
        if neg && !set.strings.is_empty() {
            return self.err("negated character class may contain strings");
        }
        Ok((set, neg))
    }

    fn class_set_expression(&mut self) -> PResult<CharSet> {
        // ClassUnion | ClassIntersection | ClassSubtraction, until `]`.
        let first = self.class_set_operand_or_range()?;
        let Some(first) = first else {
            if self.eat(']') {
                return Ok(CharSet::default());
            }
            return self.err("invalid character class");
        };
        if self.peek() == Some('&' as u32) && self.peek_at(1) == Some('&' as u32) {
            let mut acc = first;
            while self.peek() == Some('&' as u32) && self.peek_at(1) == Some('&' as u32) {
                self.i += 2;
                if self.peek() == Some('&' as u32) {
                    return self.err("invalid set operation");
                }
                let Some(o) = self.class_set_operand()? else { return self.err("invalid set operation") };
                acc = acc.intersect(&o);
            }
            if !self.eat(']') {
                return self.err("invalid set operation");
            }
            return Ok(acc);
        }
        if self.peek() == Some('-' as u32) && self.peek_at(1) == Some('-' as u32) {
            let mut acc = first;
            while self.peek() == Some('-' as u32) && self.peek_at(1) == Some('-' as u32) {
                self.i += 2;
                let Some(o) = self.class_set_operand()? else { return self.err("invalid set operation") };
                acc = acc.subtract(&o);
            }
            if !self.eat(']') {
                return self.err("invalid set operation");
            }
            return Ok(acc);
        }
        let mut acc = first;
        loop {
            if self.eat(']') {
                break;
            }
            if (self.peek() == Some('&' as u32) && self.peek_at(1) == Some('&' as u32)) || (self.peek() == Some('-' as u32) && self.peek_at(1) == Some('-' as u32)) {
                return self.err("invalid set operation");
            }
            let Some(o) = self.class_set_operand_or_range()? else { return self.err("unterminated character class") };
            acc.union(&o);
        }
        acc.normalize();
        Ok(acc)
    }

    fn class_set_operand_or_range(&mut self) -> PResult<Option<CharSet>> {
        let save = self.i;
        let Some(a) = self.class_set_character()? else {
            self.i = save;
            return self.class_set_operand();
        };
        if self.peek() == Some('-' as u32) && self.peek_at(1) != Some('-' as u32) {
            self.i += 1;
            let Some(b) = self.class_set_character()? else { return self.err("invalid character class range") };
            if a > b {
                return self.err("range out of order in character class");
            }
            return Ok(Some(CharSet { ranges: vec![(a, b)], strings: Vec::new() }));
        }
        Ok(Some(CharSet::single(a)))
    }

    /// A single ClassSetCharacter, or None if the next item is not one.
    fn class_set_character(&mut self) -> PResult<Option<u32>> {
        let Some(c) = self.peek() else { return self.err("unterminated character class") };
        let ch = char::from_u32(c).unwrap_or('\0');
        if c == '\\' as u32 {
            let Some(e) = self.peek_at(1) else { return self.err("\\ at end of pattern") };
            let ech = char::from_u32(e).unwrap_or('\0');
            if matches!(ech, 'd' | 'D' | 's' | 'S' | 'w' | 'W' | 'p' | 'P' | 'q') {
                return Ok(None);
            }
            self.i += 1;
            // ClassSetReservedPunctuator may be escaped.
            if "&-!#%,:;<=>@`~".contains(ech) {
                self.i += 1;
                return Ok(Some(e));
            }
            return Ok(Some(self.character_escape(true)?));
        }
        if matches!(ch, '[' | ']') {
            return Ok(None);
        }
        // Reserved double punctuators and syntax characters.
        if let Some(n) = self.peek_at(1) {
            if n == c && "&!#$%*+,.:;<=>?@^`~".contains(ch) {
                return self.err("invalid set operation in character class");
            }
        }
        if matches!(ch, '(' | ')' | '{' | '}' | '/' | '-' | '|') {
            return self.err("invalid character in character class");
        }
        self.i += 1;
        Ok(Some(c))
    }

    fn class_set_operand(&mut self) -> PResult<Option<CharSet>> {
        let Some(c) = self.peek() else { return self.err("unterminated character class") };
        if c == '[' as u32 {
            self.i += 1;
            let (set, neg) = self.class_v()?;
            return Ok(Some(if neg { set.complement(0x10FFFF) } else { set }));
        }
        if c == '\\' as u32 {
            let e = self.peek_at(1).unwrap_or(0);
            let ech = char::from_u32(e).unwrap_or('\0');
            match ech {
                'd' | 'D' | 's' | 'S' | 'w' | 'W' => {
                    self.i += 2;
                    let set = match ech {
                        'd' | 'D' => digit_set(),
                        's' | 'S' => space_set(),
                        _ => word_set(self.flags),
                    };
                    return Ok(Some(if ech.is_uppercase() { set.complement(0x10FFFF) } else { set }));
                }
                'p' | 'P' => {
                    self.i += 2;
                    let set = self.property_escape()?;
                    if ech == 'P' {
                        if !set.strings.is_empty() {
                            return self.err("negated property of strings");
                        }
                        return Ok(Some(set.complement(0x10FFFF)));
                    }
                    return Ok(Some(set));
                }
                'q' => {
                    self.i += 2;
                    if !self.eat('{') {
                        return self.err("invalid escape");
                    }
                    let mut set = CharSet::default();
                    let mut cur: Vec<u32> = Vec::new();
                    loop {
                        let Some(x) = self.peek() else { return self.err("unterminated \\q") };
                        if x == '}' as u32 || x == '|' as u32 {
                            self.i += 1;
                            if cur.len() == 1 {
                                set.add_range(cur[0], cur[0]);
                            } else {
                                set.strings.push(core::mem::take(&mut cur));
                            }
                            cur.clear();
                            if x == '}' as u32 {
                                break;
                            }
                            continue;
                        }
                        if x == '\\' as u32 {
                            self.i += 1;
                            cur.push(self.character_escape(true)?);
                        } else {
                            if matches!(char::from_u32(x), Some('(' | ')' | '[' | ']' | '{' | '/' | '-')) {
                                return self.err("invalid character in \\q");
                            }
                            self.i += 1;
                            cur.push(x);
                        }
                    }
                    set.normalize();
                    return Ok(Some(set));
                }
                _ => {}
            }
        }
        match self.class_set_character()? {
            Some(x) => Ok(Some(CharSet::single(x))),
            None => Ok(None),
        }
    }
}

enum ClassAtom {
    Char(u32),
    Set(CharSet),
}

impl ClassAtom {
    fn add_to(&self, s: &mut CharSet) {
        match self {
            ClassAtom::Char(c) => s.add_range(*c, *c),
            ClassAtom::Set(o) => s.ranges.extend_from_slice(&o.ranges),
        }
    }
}

/// Counts capture groups and checks for named groups before parsing.
fn prescan(s: &[u32]) -> (usize, bool) {
    let mut n = 0;
    let mut names = false;
    let mut i = 0;
    let mut in_class = false;
    while i < s.len() {
        let c = s[i];
        if c == '\\' as u32 {
            i += 2;
            continue;
        }
        if in_class {
            if c == ']' as u32 {
                in_class = false;
            }
        } else if c == '[' as u32 {
            in_class = true;
        } else if c == '(' as u32 {
            if s.get(i + 1) != Some(&('?' as u32)) {
                n += 1;
            } else if s.get(i + 2) == Some(&('<' as u32)) && !matches!(s.get(i + 3), Some(&0x3D) | Some(&0x21)) {
                n += 1;
                names = true;
            }
        }
        i += 1;
    }
    (n, names)
}

// ----- compiled program ------------------------------------------------

#[derive(Clone, Debug)]
enum Inst {
    /// (char, backward, fold)
    Char(u32, bool, bool),
    /// `.`: (dot_all, backward)
    Any(bool, bool),
    /// (set, negated, backward, fold)
    Class(u32, bool, bool, bool),
    Split(u32, u32),
    Jmp(u32),
    Save(u32),
    ClearCaps(u32, u32),
    Start(bool),
    End(bool),
    WordB(bool, bool),
    BackRef(u32, bool, bool),
    Look { start: u32, end: u32, negate: bool },
    SetReg(u32, u32),
    /// Loop control: (counter reg, min, max, exit target, greedy).
    Repeat { reg: u32, min: u32, max: u32, exit: u32, greedy: bool },
    IncReg(u32),
    /// Record the position at the start of an iteration (reg).
    SavePos(u32),
    /// Fail if the iteration (counter past min) matched empty: (pos reg,
    /// counter reg, min).
    EmptyCheck(u32, u32, u32),
    /// Matches one of the strings of a `v`-mode class (longest first).
    Strings(u32, bool),
    Match,
}

pub struct Regex {
    prog: Vec<Inst>,
    sets: Vec<CharSet>,
    string_sets: Vec<Vec<Vec<u32>>>,
    backrefs: Vec<Vec<usize>>,
    pub ncaps: usize,
    pub names: Vec<(Vec<u16>, Vec<usize>)>,
    pub flags: Flags,
    nregs: usize,
}

struct Compiler<'f> {
    prog: Vec<Inst>,
    sets: Vec<CharSet>,
    string_sets: Vec<Vec<Vec<u32>>>,
    backrefs: Vec<Vec<usize>>,
    nregs: u32,
    flags: &'f Flags,
    fold: bool,
    multi: bool,
    dot_all: bool,
    backward: bool,
}

/// Canonicalize (ECMA-262 §22.2.2.7.3).
pub fn canonicalize(c: u32, unicode: bool) -> u32 {
    if unicode {
        return crate::unicode::simple_fold(c);
    }
    if c < 0x80 {
        return if (0x61..=0x7A).contains(&c) { c - 32 } else { c };
    }
    let Some(ch) = char::from_u32(c) else { return c };
    let mut up = ch.to_uppercase();
    let (Some(u), None) = (up.next(), up.next()) else { return c };
    let u = u as u32;
    if u > 0xFFFF || u < 0x80 {
        return c;
    }
    u
}

/// The set {Canonicalize(a) | a in set} ∪ set (for case-insensitive tests).
fn fold_set(set: &CharSet, unicode: bool) -> CharSet {
    let mut out = set.clone();
    if unicode {
        for &(k, v) in crate::unicode::tables_case_fold() {
            if set.contains(k) {
                out.add_range(v, v);
            }
        }
    } else {
        for &(a, b) in &set.ranges {
            let b = b.min(0xFFFF);
            if a > b {
                continue;
            }
            for c in a..=b {
                let k = canonicalize(c, false);
                if k != c {
                    out.add_range(k, k);
                }
            }
        }
    }
    out.normalize();
    out
}

impl Compiler<'_> {
    fn emit(&mut self, i: Inst) -> usize {
        self.prog.push(i);
        self.prog.len() - 1
    }

    fn here(&self) -> u32 {
        self.prog.len() as u32
    }

    fn node(&mut self, n: &Node) {
        match n {
            Node::Char(c) => {
                let c = if self.fold { canonicalize(*c, self.flags.either_unicode()) } else { *c };
                self.emit(Inst::Char(c, self.backward, self.fold));
            }
            Node::Any => {
                self.emit(Inst::Any(self.dot_all, self.backward));
            }
            Node::Class(set, neg) => {
                let mut s = (**set).clone();
                if !s.strings.is_empty() && !*neg {
                    // Strings first (longest), then single characters.
                    let mut strings = s.strings.clone();
                    strings.sort_by_key(|x| core::cmp::Reverse(x.len()));
                    let idx = self.string_sets.len() as u32;
                    self.string_sets.push(strings);
                    let split = self.emit(Inst::Split(0, 0));
                    let a = self.here();
                    self.emit(Inst::Strings(idx, self.backward));
                    let j = self.emit(Inst::Jmp(0));
                    let b = self.here();
                    s.strings.clear();
                    let si = self.sets.len() as u32;
                    let s2 = if self.fold { fold_set(&s, self.flags.either_unicode()) } else { s };
                    self.sets.push(s2);
                    self.emit(Inst::Class(si, false, self.backward, self.fold));
                    let end = self.here();
                    self.prog[split] = Inst::Split(a, b);
                    self.prog[j] = Inst::Jmp(end);
                    return;
                }
                if self.fold {
                    s = fold_set(&s, self.flags.either_unicode());
                }
                let si = self.sets.len() as u32;
                self.sets.push(s);
                self.emit(Inst::Class(si, *neg, self.backward, self.fold));
            }
            Node::Seq(xs) => {
                if self.backward {
                    for x in xs.iter().rev() {
                        self.node(x);
                    }
                } else {
                    for x in xs {
                        self.node(x);
                    }
                }
            }
            Node::Alt(xs) => {
                let mut jumps = Vec::new();
                for (i, x) in xs.iter().enumerate() {
                    if i + 1 < xs.len() {
                        let split = self.emit(Inst::Split(0, 0));
                        let a = self.here();
                        self.node(x);
                        jumps.push(self.emit(Inst::Jmp(0)));
                        let b = self.here();
                        self.prog[split] = Inst::Split(a, b);
                    } else {
                        self.node(x);
                    }
                }
                let end = self.here();
                for j in jumps {
                    self.prog[j] = Inst::Jmp(end);
                }
            }
            Node::Group(x, cap) => match cap {
                Some(i) => {
                    let (s, e) = (2 * *i as u32, 2 * *i as u32 + 1);
                    self.emit(Inst::Save(if self.backward { e } else { s }));
                    self.node(x);
                    self.emit(Inst::Save(if self.backward { s } else { e }));
                }
                None => self.node(x),
            },
            Node::Repeat { node, min, max, greedy } => self.repeat(node, *min, *max, *greedy),
            Node::Start => {
                self.emit(Inst::Start(self.multi));
            }
            Node::End => {
                self.emit(Inst::End(self.multi));
            }
            Node::WordB(b) => {
                self.emit(Inst::WordB(*b, self.fold && self.flags.either_unicode()));
            }
            Node::Look { node, ahead, negate } => {
                let at = self.emit(Inst::Look { start: 0, end: 0, negate: *negate });
                let start = self.here();
                let saved = self.backward;
                self.backward = !*ahead;
                self.node(node);
                self.backward = saved;
                self.emit(Inst::Match);
                let end = self.here();
                self.prog[at] = Inst::Look { start, end, negate: *negate };
            }
            Node::BackRef(groups) => {
                let i = self.backrefs.len() as u32;
                self.backrefs.push(groups.clone());
                self.emit(Inst::BackRef(i, self.fold, self.backward));
            }
            Node::Mods { node, set } => {
                let (f, m, d) = (self.fold, self.multi, self.dot_all);
                if let Some(v) = set[0] {
                    self.fold = v;
                }
                if let Some(v) = set[1] {
                    self.multi = v;
                }
                if let Some(v) = set[2] {
                    self.dot_all = v;
                }
                self.node(node);
                self.fold = f;
                self.multi = m;
                self.dot_all = d;
            }
        }
    }

    fn caps_in(n: &Node, out: &mut Vec<usize>) {
        match n {
            Node::Group(x, c) => {
                if let Some(i) = c {
                    out.push(*i);
                }
                Self::caps_in(x, out);
            }
            Node::Seq(xs) | Node::Alt(xs) => xs.iter().for_each(|x| Self::caps_in(x, out)),
            Node::Repeat { node, .. } | Node::Look { node, .. } | Node::Mods { node, .. } => Self::caps_in(node, out),
            _ => {}
        }
    }

    fn repeat(&mut self, node: &Node, min: u32, max: Option<u32>, greedy: bool) {
        let max = max.unwrap_or(u32::MAX);
        if max == 0 {
            return;
        }
        let mut caps = Vec::new();
        Self::caps_in(node, &mut caps);
        let (cl, ch) = match (caps.iter().min(), caps.iter().max()) {
            (Some(a), Some(b)) => (2 * *a as u32, 2 * *b as u32 + 2),
            _ => (0, 0),
        };
        let reg = self.nregs;
        let pos_reg = self.nregs + 1;
        self.nregs += 2;
        self.emit(Inst::SetReg(reg, 0));
        let top = self.here();
        let rep = self.emit(Inst::Repeat { reg, min, max, exit: 0, greedy });
        self.emit(Inst::SavePos(pos_reg));
        if ch > cl {
            self.emit(Inst::ClearCaps(cl, ch));
        }
        self.node(node);
        self.emit(Inst::EmptyCheck(pos_reg, reg, min));
        self.emit(Inst::IncReg(reg));
        self.emit(Inst::Jmp(top));
        let exit = self.here();
        self.prog[rep] = Inst::Repeat { reg, min, max, exit, greedy };
    }
}

/// Parses and compiles a pattern (UTF-16) with flags.
pub fn compile(pattern: &[u16], flags_str: &str) -> Result<Regex, String> {
    let flags = Flags::parse(flags_str).ok_or_else(|| String::from("invalid regular expression flags"))?;
    // Code points in unicode mode, code units otherwise.
    let s: Vec<u32> = if flags.either_unicode() {
        char::decode_utf16(pattern.iter().copied()).map(|r| r.map(|c| c as u32).unwrap_or_else(|e| e.unpaired_surrogate() as u32)).collect()
    } else {
        pattern.iter().map(|&u| u as u32).collect()
    };
    let (ngroups, has_names) = prescan(&s);
    let mut p = Parser { s: &s, i: 0, flags, ngroups, group_names: Vec::new(), has_names, next_group: 1, names_seen: Vec::new(), backrefs_named: Vec::new() };
    let mut ast = p.disjunction()?;
    if p.i < s.len() {
        return Err(if s[p.i] == ')' as u32 { "unmatched )".into() } else { "invalid regular expression".into() });
    }
    // Resolve named backreferences.
    let names = p.group_names.clone();
    let refs = p.backrefs_named.clone();
    let mut bad = false;
    resolve_named(&mut ast, &names, &refs, &mut bad);
    if bad {
        return Err("invalid named capture referenced".into());
    }
    let mut c = Compiler {
        prog: Vec::new(),
        sets: Vec::new(),
        string_sets: Vec::new(),
        backrefs: Vec::new(),
        nregs: 0,
        flags: &flags,
        fold: flags.ignore_case,
        multi: flags.multiline,
        dot_all: flags.dot_all,
        backward: false,
    };
    c.emit(Inst::Save(0));
    c.node(&ast);
    c.emit(Inst::Save(1));
    c.emit(Inst::Match);
    let mut name_list: Vec<(Vec<u16>, Vec<usize>)> = Vec::new();
    for (n, idx) in &names {
        let u: Vec<u16> = n.iter().flat_map(|&c| {
            let mut b = [0u16; 2];
            char::from_u32(c).map(|ch| ch.encode_utf16(&mut b).to_vec()).unwrap_or_else(|| vec![c as u16])
        }).collect();
        match name_list.iter_mut().find(|(x, _)| *x == u) {
            Some((_, list)) => list.push(*idx),
            None => name_list.push((u, vec![*idx])),
        }
    }
    Ok(Regex {
        prog: c.prog,
        sets: c.sets,
        string_sets: c.string_sets,
        backrefs: c.backrefs,
        ncaps: ngroups + 1,
        names: name_list,
        flags,
        nregs: c.nregs as usize,
    })
}

fn resolve_named(n: &mut Node, names: &[(Vec<u32>, usize)], refs: &[Vec<u32>], bad: &mut bool) {
    match n {
        Node::BackRef(g) => {
            if g.len() == 1 && g[0] > usize::MAX / 2 {
                let k = usize::MAX - g[0];
                let name = &refs[k];
                let list: Vec<usize> = names.iter().filter(|(x, _)| x == name).map(|x| x.1).collect();
                if list.is_empty() {
                    *bad = true;
                }
                *g = list;
            }
        }
        Node::Seq(xs) | Node::Alt(xs) => xs.iter_mut().for_each(|x| resolve_named(x, names, refs, bad)),
        Node::Group(x, _) => resolve_named(x, names, refs, bad),
        Node::Repeat { node, .. } | Node::Look { node, .. } | Node::Mods { node, .. } => resolve_named(node, names, refs, bad),
        _ => {}
    }
}

// ----- matching --------------------------------------------------------

/// Matching ran out of steps (the caller treats it as an interruption).
#[derive(Debug)]
pub struct Exhausted;

struct Matcher<'r, 'i> {
    re: &'r Regex,
    input: &'i [u16],
    unicode: bool,
    caps: Vec<usize>,
    regs: Vec<usize>,
    /// Undo log: (slot, old value); slot < caps.len() are captures, else
    /// registers.
    undo: Vec<(usize, usize)>,
    steps: u64,
    limit: u64,
}

const NONE: usize = usize::MAX;

impl Matcher<'_, '_> {
    /// Reads the character at `pos` forwards: (char, width).
    fn fwd(&self, pos: usize) -> Option<(u32, usize)> {
        let c = *self.input.get(pos)? as u32;
        if self.unicode && (0xD800..0xDC00).contains(&c) {
            if let Some(&lo) = self.input.get(pos + 1) {
                if (0xDC00..0xE000).contains(&lo) {
                    return Some((0x10000 + ((c - 0xD800) << 10) + (lo as u32 - 0xDC00), 2));
                }
            }
        }
        Some((c, 1))
    }

    fn bwd(&self, pos: usize) -> Option<(u32, usize)> {
        if pos == 0 {
            return None;
        }
        let c = self.input[pos - 1] as u32;
        if self.unicode && (0xDC00..0xE000).contains(&c) && pos >= 2 {
            let hi = self.input[pos - 2] as u32;
            if (0xD800..0xDC00).contains(&hi) {
                return Some((0x10000 + ((hi - 0xD800) << 10) + (c - 0xDC00), 2));
            }
        }
        Some((c, 1))
    }

    fn read(&self, pos: usize, back: bool) -> Option<(u32, usize)> {
        if back { self.bwd(pos) } else { self.fwd(pos) }
    }

    fn set_slot(&mut self, slot: usize, v: usize) {
        let ncaps = self.caps.len();
        if slot < ncaps {
            self.undo.push((slot, self.caps[slot]));
            self.caps[slot] = v;
        } else {
            self.undo.push((slot, self.regs[slot - ncaps]));
            self.regs[slot - ncaps] = v;
        }
    }

    fn rollback(&mut self, mark: usize) {
        let ncaps = self.caps.len();
        while self.undo.len() > mark {
            let (slot, v) = self.undo.pop().unwrap();
            if slot < ncaps {
                self.caps[slot] = v;
            } else {
                self.regs[slot - ncaps] = v;
            }
        }
    }

    fn is_word_at(&self, pos: usize, fold_unicode: bool) -> bool {
        match self.input.get(pos) {
            Some(&c) => {
                let c = c as u32;
                is_word(c) || (fold_unicode && (c == 0x017F || c == 0x212A))
            }
            None => false,
        }
    }

    /// Runs the program from `pc` at `pos`; Some(end) on success.
    fn run(&mut self, pc0: usize, pos0: usize) -> Result<Option<usize>, Exhausted> {
        let mut stack: Vec<(usize, usize, usize)> = Vec::new();
        let mut pc = pc0;
        let mut pos = pos0;
        loop {
            self.steps += 1;
            if self.steps > self.limit {
                return Err(Exhausted);
            }
            let ok = match &self.re.prog[pc] {
                Inst::Match => return Ok(Some(pos)),
                Inst::Char(c, back, fold) => match self.read(pos, *back) {
                    Some((x, w)) => {
                        let x = if *fold { canonicalize(x, self.re.flags.either_unicode()) } else { x };
                        if x == *c {
                            pos = if *back { pos - w } else { pos + w };
                            pc += 1;
                            true
                        } else {
                            false
                        }
                    }
                    None => false,
                },
                Inst::Any(dot_all, back) => match self.read(pos, *back) {
                    Some((x, w)) if *dot_all || !is_line_term(x) => {
                        pos = if *back { pos - w } else { pos + w };
                        pc += 1;
                        true
                    }
                    _ => false,
                },
                Inst::Class(i, neg, back, fold) => match self.read(pos, *back) {
                    Some((x, w)) => {
                        let set = &self.re.sets[*i as usize];
                        let x2 = if *fold { canonicalize(x, self.re.flags.either_unicode()) } else { x };
                        let inside = set.contains(x2) || set.contains(x);
                        if inside != *neg {
                            pos = if *back { pos - w } else { pos + w };
                            pc += 1;
                            true
                        } else {
                            false
                        }
                    }
                    None => false,
                },
                Inst::Strings(i, back) => {
                    // Try each string (longest first) by pushing
                    // alternatives; simple approach: first that matches.
                    let mut matched = None;
                    for st in &self.re.string_sets[*i as usize] {
                        let mut p = pos;
                        let mut ok = true;
                        let it: Vec<u32> = if *back { st.iter().rev().copied().collect() } else { st.clone() };
                        for c in it {
                            match self.read(p, *back) {
                                Some((x, w)) if x == c => p = if *back { p - w } else { p + w },
                                _ => {
                                    ok = false;
                                    break;
                                }
                            }
                        }
                        if ok {
                            matched = Some(p);
                            break;
                        }
                    }
                    match matched {
                        Some(p) => {
                            pos = p;
                            pc += 1;
                            true
                        }
                        None => false,
                    }
                }
                Inst::Split(a, b) => {
                    stack.push((*b as usize, pos, self.undo.len()));
                    pc = *a as usize;
                    true
                }
                Inst::Jmp(t) => {
                    pc = *t as usize;
                    true
                }
                Inst::Save(slot) => {
                    self.set_slot(*slot as usize, pos);
                    pc += 1;
                    true
                }
                Inst::ClearCaps(a, b) => {
                    for s in *a..*b {
                        if self.caps[s as usize] != NONE {
                            self.set_slot(s as usize, NONE);
                        }
                    }
                    pc += 1;
                    true
                }
                Inst::Start(multi) => {
                    let ok = pos == 0 || (*multi && is_line_term(self.input[pos - 1] as u32));
                    pc += 1;
                    ok
                }
                Inst::End(multi) => {
                    let ok = pos == self.input.len() || (*multi && is_line_term(self.input[pos] as u32));
                    pc += 1;
                    ok
                }
                Inst::WordB(want, fu) => {
                    let a = pos > 0 && self.is_word_at(pos - 1, *fu);
                    let b = self.is_word_at(pos, *fu);
                    pc += 1;
                    (a != b) == *want
                }
                Inst::BackRef(i, fold, back) => {
                    // The first participating group of the list.
                    let groups = &self.re.backrefs[*i as usize];
                    let mut range = None;
                    for &g in groups {
                        let (s, e) = (self.caps[2 * g], self.caps[2 * g + 1]);
                        if s != NONE && e != NONE {
                            range = Some((s, e));
                            break;
                        }
                    }
                    match range {
                        None => {
                            pc += 1;
                            true
                        }
                        Some((s, e)) => {
                            let len = e - s;
                            let ok = if *back {
                                pos >= len && self.eq_range(s, pos - len, len, *fold)
                            } else {
                                pos + len <= self.input.len() && self.eq_range(s, pos, len, *fold)
                            };
                            if ok {
                                pos = if *back { pos - len } else { pos + len };
                                pc += 1;
                            }
                            ok
                        }
                    }
                }
                Inst::Look { start, end, negate } => {
                    let (start, end, negate) = (*start as usize, *end as usize, *negate);
                    let mark = self.undo.len();
                    let r = self.run(start, pos)?;
                    match (r.is_some(), negate) {
                        (true, false) => {
                            // Keep the captures (atomic: no backtracking in).
                            pc = end;
                            true
                        }
                        (false, true) => {
                            self.rollback(mark);
                            pc = end;
                            true
                        }
                        _ => {
                            self.rollback(mark);
                            false
                        }
                    }
                }
                Inst::SetReg(r, v) => {
                    let slot = self.caps.len() + *r as usize;
                    self.set_slot(slot, *v as usize);
                    pc += 1;
                    true
                }
                Inst::IncReg(r) => {
                    let slot = self.caps.len() + *r as usize;
                    let v = self.regs[*r as usize] + 1;
                    self.set_slot(slot, v);
                    pc += 1;
                    true
                }
                Inst::SavePos(r) => {
                    let slot = self.caps.len() + *r as usize;
                    self.set_slot(slot, pos);
                    pc += 1;
                    true
                }
                Inst::EmptyCheck(pr, cr, min) => {
                    let count = self.regs[*cr as usize];
                    let ok = !(count >= *min as usize && self.regs[*pr as usize] == pos);
                    pc += 1;
                    ok
                }
                Inst::Repeat { reg, min, max, exit, greedy } => {
                    let count = self.regs[*reg as usize];
                    let body = pc + 1;
                    if count < *min as usize {
                        pc = body;
                    } else if count >= *max as usize {
                        pc = *exit as usize;
                    } else if *greedy {
                        stack.push((*exit as usize, pos, self.undo.len()));
                        pc = body;
                    } else {
                        stack.push((body, pos, self.undo.len()));
                        pc = *exit as usize;
                    }
                    true
                }
            };
            if !ok {
                match stack.pop() {
                    Some((p, q, mark)) => {
                        self.rollback(mark);
                        pc = p;
                        pos = q;
                    }
                    None => return Ok(None),
                }
            }
        }
    }

    fn eq_range(&self, a: usize, b: usize, len: usize, fold: bool) -> bool {
        let u = self.re.flags.either_unicode();
        for k in 0..len {
            let (x, y) = (self.input[a + k] as u32, self.input[b + k] as u32);
            if x != y && !(fold && canonicalize(x, u) == canonicalize(y, u)) {
                return false;
            }
        }
        true
    }
}

impl Regex {
    /// Matches at exactly `start`: the capture positions (start, end) per
    /// group, or None. `limit` bounds the backtracking steps.
    pub fn match_at(&self, input: &[u16], start: usize, limit: u64) -> Result<Option<Vec<Option<(usize, usize)>>>, Exhausted> {
        let mut m = Matcher {
            re: self,
            input,
            unicode: self.flags.either_unicode(),
            caps: vec![NONE; 2 * self.ncaps],
            regs: vec![0; self.nregs],
            undo: Vec::new(),
            steps: 0,
            limit,
        };
        match m.run(0, start)? {
            Some(_) => {
                let mut out = Vec::with_capacity(self.ncaps);
                for g in 0..self.ncaps {
                    let (s, e) = (m.caps[2 * g], m.caps[2 * g + 1]);
                    out.push(if s != NONE && e != NONE { Some((s, e)) } else { None });
                }
                Ok(Some(out))
            }
            None => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::ToString;

    fn u(s: &str) -> Vec<u16> {
        s.encode_utf16().collect()
    }

    /// First match anywhere: the group texts.
    fn find(p: &str, f: &str, input: &str) -> Option<Vec<Option<String>>> {
        let re = compile(&u(p), f).unwrap();
        let inp = u(input);
        for start in 0..=inp.len() {
            if let Some(c) = re.match_at(&inp, start, 1_000_000).unwrap() {
                return Some(c.iter().map(|g| g.map(|(a, b)| String::from_utf16_lossy(&inp[a..b]))).collect());
            }
        }
        None
    }

    fn m0(p: &str, f: &str, input: &str) -> Option<String> {
        find(p, f, input).map(|v| v[0].clone().unwrap())
    }

    #[test]
    fn basics() {
        assert_eq!(m0("b+", "", "abbbc").as_deref(), Some("bbb"));
        assert_eq!(m0("b+?", "", "abbbc").as_deref(), Some("b"));
        assert_eq!(m0("a|bc", "", "xbc").as_deref(), Some("bc"));
        assert_eq!(m0("[a-c]{2,3}", "", "zzabcd").as_deref(), Some("abc"));
        assert_eq!(m0("^ab$", "", "ab").as_deref(), Some("ab"));
        assert_eq!(m0("^b", "m", "a\nb").as_deref(), Some("b"));
        assert_eq!(m0("\\bfoo\\b", "", "a foo b").as_deref(), Some("foo"));
        assert_eq!(m0("A", "i", "xa").as_deref(), Some("a"));
        assert_eq!(m0("(?i:a)b", "", "AbAB").as_deref(), Some("Ab"));
        assert_eq!(m0(".", "", "\n"), None);
        assert_eq!(m0(".", "s", "\n").as_deref(), Some("\n"));
        assert_eq!(m0("\\d+", "", "ab123c").as_deref(), Some("123"));
        assert_eq!(m0("[^\\s]+", "", "  xy ").as_deref(), Some("xy"));
    }

    #[test]
    fn groups_and_refs() {
        let g = find("(a)(b)?", "", "a").unwrap();
        assert_eq!(g[1].as_deref(), Some("a"));
        assert_eq!(g[2], None);
        assert_eq!(m0("(a)\\1", "", "xaa").as_deref(), Some("aa"));
        assert_eq!(m0("(?<x>b)\\k<x>", "", "abb").as_deref(), Some("bb"));
        // Captures are reset in each iteration.
        let g = find("(z)((a+)?(b+)?(c))*", "", "zaacbbbcac").unwrap();
        assert_eq!(g[3].as_deref(), Some("a"));
        assert_eq!(g[4], None);
        assert_eq!(g[5].as_deref(), Some("c"));
        // Empty iterations stop a loop.
        assert_eq!(m0("(a*)*b", "", "b").as_deref(), Some("b"));
    }

    #[test]
    fn lookaround() {
        assert_eq!(m0("a(?=b)", "", "acab").as_deref(), Some("a"));
        assert_eq!(m0("a(?!b)", "", "abac").as_deref(), Some("a"));
        assert_eq!(m0("(?<=\\$)\\d+", "", "a$12").as_deref(), Some("12"));
        assert_eq!(m0("(?<!\\$)\\b\\d+", "", "$1 22").as_deref(), Some("22"));
        let g = find("(?<=(\\d+)(\\d+))$", "", "1053").unwrap();
        assert_eq!(g[1].as_deref(), Some("1"));
        assert_eq!(g[2].as_deref(), Some("053"));
    }

    #[test]
    fn unicode_mode() {
        assert_eq!(m0(".", "u", "😀").map(|s| s.chars().count()), Some(1));
        assert_eq!(m0("^.$", "", "😀"), None);
        assert_eq!(m0("\\u{1F600}", "u", "x😀").as_deref(), Some("😀"));
        assert_eq!(m0("[\\q{abc}x]", "v", "zabc").as_deref(), Some("abc"));
        assert!(compile(&u("\\u{110000}"), "u").is_err());
        assert!(compile(&u("a{2,1}"), "").is_err());
        assert!(compile(&u("(?<a>x)(?<a>y)"), "").is_err());
        assert!(compile(&u("(?<a>x)|(?<a>y)"), "").is_ok());
        assert_eq!(m0("ſ", "iu", "S").as_deref(), Some("S"));
        assert_eq!(m0("\\w", "iu", "ſ").as_deref(), Some("ſ"));
        let _ = "x".to_string();
    }

    #[test]
    fn annex_b() {
        assert_eq!(m0("a{", "", "a{").as_deref(), Some("a{"));
        assert_eq!(m0("\\8", "", "8").as_deref(), Some("8"));
        assert_eq!(m0("\\101", "", "A").as_deref(), Some("A"));
        assert_eq!(m0("]", "", "]").as_deref(), Some("]"));
        assert!(compile(&u("]"), "u").is_err());
    }
}
