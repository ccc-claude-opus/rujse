//! String constructor, String.prototype and the string iterator.

use alloc::boxed::Box;
use alloc::vec::Vec;

use super::object::arg;
use crate::object::{attr, Class, Key};
use crate::value::{StrId, Value};
use crate::vm::conv::is_js_space;
use crate::vm::{JsResult, Vm};



pub fn init(vm: &mut Vm) {
    let op = vm.intrinsics.object_prototype;
    let e = vm.names.empty;
    let sp = vm.alloc_object(Some(op), Class::String(e));
    vm.intrinsics.string_prototype = sp;
    let kl = Key::Str(vm.names.length);
    vm.define_data(sp, kl, Value::int(0), attr::NONE);
    let c = vm.constructor("String", 1, string_ctor, sp);
    vm.global_prop("String", Value::object(c));
    vm.method(c, "fromCharCode", 1, |vm, _, a, _| {
        let mut u = Vec::with_capacity(a.len());
        for &v in a {
            u.push(vm.to_uint32(v)? as u16);
        }
        Ok(vm.new_string_units(&u))
    });
    vm.method(c, "fromCodePoint", 1, |vm, _, a, _| {
        let mut u = Vec::with_capacity(a.len());
        for &v in a {
            let n = vm.to_number(v)?;
            if crate::math::trunc(n) != n || !(0.0..=1114111.0).contains(&n) {
                let d = vm.display(v);
                return Err(vm.range_error(&alloc::format!("Invalid code point {d}")));
            }
            push_cp(&mut u, n as u32);
        }
        Ok(vm.new_string_units(&u))
    });
    vm.method(c, "raw", 1, string_raw);

    macro_rules! m {
        ($name:expr, $len:expr, $f:expr) => {
            vm.method(sp, $name, $len, $f);
        };
    }
    m!("toString", 0, |vm, this, _, _| Ok(Value::str(this_string_value(vm, this)?)));
    m!("valueOf", 0, |vm, this, _, _| Ok(Value::str(this_string_value(vm, this)?)));
    m!("at", 1, |vm, this, a, _| {
        let s = this_str(vm, this)?;
        let len = vm.str(s).len() as f64;
        let rel = vm.to_integer_or_infinity(arg(a, 0))?;
        let k = if rel >= 0.0 { rel } else { len + rel };
        if k < 0.0 || k >= len {
            return Ok(Value::UNDEFINED);
        }
        let c = vm.str(s).at(k as usize);
        Ok(vm.char_string(c))
    });
    m!("charAt", 1, |vm, this, a, _| {
        let s = this_str(vm, this)?;
        let p = vm.to_integer_or_infinity(arg(a, 0))?;
        let len = vm.str(s).len();
        if p < 0.0 || p >= len as f64 {
            return Ok(Value::str(vm.names.empty));
        }
        let c = vm.str(s).at(p as usize);
        Ok(vm.char_string(c))
    });
    m!("charCodeAt", 1, |vm, this, a, _| {
        let s = this_str(vm, this)?;
        let p = vm.to_integer_or_infinity(arg(a, 0))?;
        let len = vm.str(s).len();
        if p < 0.0 || p >= len as f64 {
            return Ok(Value::NAN);
        }
        Ok(Value::int(vm.str(s).at(p as usize) as i32))
    });
    m!("codePointAt", 1, |vm, this, a, _| {
        let s = this_str(vm, this)?;
        let p = vm.to_integer_or_infinity(arg(a, 0))?;
        let u = vm.str(s).to_units();
        if p < 0.0 || p >= u.len() as f64 {
            return Ok(Value::UNDEFINED);
        }
        Ok(Value::int(code_point_at(&u, p as usize).0 as i32))
    });
    m!("concat", 1, |vm, this, a, _| {
        let s = this_str(vm, this)?;
        let mut out = vm.str(s).to_units();
        for &v in a {
            let t = vm.to_string(v)?;
            out.extend(vm.str(t).units());
            if out.len() > vm.max_string_len() {
                return Err(vm.range_error("Invalid string length"));
            }
        }
        Ok(vm.new_string_units(&out))
    });
    m!("endsWith", 1, |vm, this, a, _| {
        let s = this_str(vm, this)?;
        let search = search_string(vm, arg(a, 0), "endsWith")?;
        let su = vm.str(s).to_units();
        let len = su.len() as f64;
        let end = if arg(a, 1).is_undefined() { len } else { vm.to_integer_or_infinity(arg(a, 1))?.clamp(0.0, len) } as usize;
        let n = search.len();
        Ok(Value::bool(n <= end && su[end - n..end] == search[..]))
    });
    m!("includes", 1, |vm, this, a, _| {
        let s = this_str(vm, this)?;
        let search = search_string(vm, arg(a, 0), "includes")?;
        let su = vm.str(s).to_units();
        let pos = vm.to_integer_or_infinity(arg(a, 1))?.clamp(0.0, su.len() as f64) as usize;
        Ok(Value::bool(index_of(&su, &search, pos).is_some()))
    });
    m!("indexOf", 1, |vm, this, a, _| {
        let s = this_str(vm, this)?;
        let t = vm.to_string(arg(a, 0))?;
        let su = vm.str(s).to_units();
        let tu = vm.str(t).to_units();
        let pos = vm.to_integer_or_infinity(arg(a, 1))?.clamp(0.0, su.len() as f64) as usize;
        Ok(match index_of(&su, &tu, pos) {
            Some(i) => Value::from_usize(i),
            None => Value::int(-1),
        })
    });
    m!("lastIndexOf", 1, |vm, this, a, _| {
        let s = this_str(vm, this)?;
        let t = vm.to_string(arg(a, 0))?;
        let n = vm.to_number(arg(a, 1))?;
        let su = vm.str(s).to_units();
        let tu = vm.str(t).to_units();
        let pos = if n.is_nan() { f64::INFINITY } else { crate::math::trunc(n) };
        let len = su.len();
        let start = pos.clamp(0.0, len as f64) as usize;
        if tu.len() > len {
            return Ok(Value::int(-1));
        }
        let mut i = start.min(len - tu.len());
        loop {
            if su[i..i + tu.len()] == tu[..] {
                return Ok(Value::from_usize(i));
            }
            if i == 0 {
                return Ok(Value::int(-1));
            }
            i -= 1;
        }
    });
    m!("isWellFormed", 0, |vm, this, _, _| {
        let s = this_str(vm, this)?;
        let u = vm.str(s).to_units();
        Ok(Value::bool(char::decode_utf16(u.iter().copied()).all(|r| r.is_ok())))
    });
    m!("toWellFormed", 0, |vm, this, _, _| {
        let s = this_str(vm, this)?;
        let u = vm.str(s).to_units();
        let mut out = Vec::with_capacity(u.len());
        for r in char::decode_utf16(u.iter().copied()) {
            match r {
                Ok(c) => {
                    let mut b = [0u16; 2];
                    out.extend_from_slice(c.encode_utf16(&mut b));
                }
                Err(_) => out.push(0xFFFD),
            }
        }
        Ok(vm.new_string_units(&out))
    });
    m!("localeCompare", 1, |vm, this, a, _| {
        let s = this_str(vm, this)?;
        let t = vm.to_string(arg(a, 0))?;
        Ok(Value::int(match vm.str(s).cmp_units(vm.str(t)) {
            core::cmp::Ordering::Less => -1,
            core::cmp::Ordering::Equal => 0,
            core::cmp::Ordering::Greater => 1,
        }))
    });
    m!("normalize", 0, |vm, this, a, _| {
        use crate::unicode::normalize::{normalize, Form};
        let s = this_str(vm, this)?;
        let f = arg(a, 0);
        let form = if f.is_undefined() {
            Form::Nfc
        } else {
            let fs = vm.to_string(f)?;
            match vm.str(fs).to_rust_lossy().as_str() {
                "NFC" => Form::Nfc,
                "NFD" => Form::Nfd,
                "NFKC" => Form::Nfkc,
                "NFKD" => Form::Nfkd,
                _ => return Err(vm.range_error("The normalization form should be one of NFC, NFD, NFKC, NFKD.")),
            }
        };
        let units = vm.str(s).to_units();
        let out = normalize(&units, form);
        if out == units {
            return Ok(Value::str(s));
        }
        Ok(vm.new_string_units(&out))
    });
    m!("padEnd", 1, |vm, this, a, _| pad(vm, this, a, false));
    m!("padStart", 1, |vm, this, a, _| pad(vm, this, a, true));
    m!("repeat", 1, |vm, this, a, _| {
        let s = this_str(vm, this)?;
        let n = vm.to_integer_or_infinity(arg(a, 0))?;
        if n < 0.0 || n == f64::INFINITY {
            return Err(vm.range_error("Invalid count value"));
        }
        let u = vm.str(s).to_units();
        if u.is_empty() || n == 0.0 {
            return Ok(Value::str(vm.names.empty));
        }
        if u.len() as f64 * n > vm.max_string_len() as f64 {
            return Err(vm.range_error("Invalid string length"));
        }
        let total = u.len() * n as usize;
        if u.iter().all(|&c| c < 0x100) {
            let b: Vec<u8> = u.iter().map(|&c| c as u8).cycle().take(total).collect();
            return Ok(Value::str(vm.alloc_string(crate::string::JsStr::latin1(b.into_boxed_slice()))));
        }
        let out: Vec<u16> = u.iter().copied().cycle().take(total).collect();
        Ok(vm.new_string_units(&out))
    });
    m!("slice", 2, |vm, this, a, _| {
        let s = this_str(vm, this)?;
        let len = vm.str(s).len() as f64;
        let (from, to) = rel_range(vm, arg(a, 0), arg(a, 1), len)?;
        if from >= to {
            return Ok(Value::str(vm.names.empty));
        }
        let r = vm.str(s).slice(from, to);
        Ok(Value::str(vm.alloc_string(r)))
    });
    m!("startsWith", 1, |vm, this, a, _| {
        let s = this_str(vm, this)?;
        let search = search_string(vm, arg(a, 0), "startsWith")?;
        let su = vm.str(s).to_units();
        let start = vm.to_integer_or_infinity(arg(a, 1))?.clamp(0.0, su.len() as f64) as usize;
        Ok(Value::bool(start + search.len() <= su.len() && su[start..start + search.len()] == search[..]))
    });
    m!("substring", 2, |vm, this, a, _| {
        let s = this_str(vm, this)?;
        let len = vm.str(s).len() as f64;
        let st = vm.to_integer_or_infinity(arg(a, 0))?.clamp(0.0, len);
        let en = if arg(a, 1).is_undefined() { len } else { vm.to_integer_or_infinity(arg(a, 1))?.clamp(0.0, len) };
        let (from, to) = (st.min(en) as usize, st.max(en) as usize);
        let r = vm.str(s).slice(from, to);
        Ok(Value::str(vm.alloc_string(r)))
    });
    m!("substr", 2, |vm, this, a, _| {
        let s = this_str(vm, this)?;
        let size = vm.str(s).len() as f64;
        let mut start = vm.to_integer_or_infinity(arg(a, 0))?;
        if start == f64::NEG_INFINITY {
            start = 0.0;
        } else if start < 0.0 {
            start = (size + start).max(0.0);
        } else {
            start = start.min(size);
        }
        let length = if arg(a, 1).is_undefined() { size } else { vm.to_integer_or_infinity(arg(a, 1))? };
        let end = (start + length).min(size);
        if start >= end {
            return Ok(Value::str(vm.names.empty));
        }
        let r = vm.str(s).slice(start as usize, end as usize);
        Ok(Value::str(vm.alloc_string(r)))
    });
    m!("toLowerCase", 0, |vm, this, _, _| change_case(vm, this, false));
    m!("toUpperCase", 0, |vm, this, _, _| change_case(vm, this, true));
    m!("toLocaleLowerCase", 0, |vm, this, _, _| change_case(vm, this, false));
    m!("toLocaleUpperCase", 0, |vm, this, _, _| change_case(vm, this, true));
    m!("trim", 0, |vm, this, _, _| trim(vm, this, true, true));
    let ts = vm.method(sp, "trimStart", 0, |vm, this, _, _| trim(vm, this, true, false));
    let te = vm.method(sp, "trimEnd", 0, |vm, this, _, _| trim(vm, this, false, true));
    let kl = vm.key("trimLeft");
    vm.define_data(sp, kl, Value::object(ts), attr::HIDDEN);
    let kr = vm.key("trimRight");
    vm.define_data(sp, kr, Value::object(te), attr::HIDDEN);
    m!("match", 1, |vm, this, a, _| regexp_delegate(vm, this, arg(a, 0), 0, a));
    m!("matchAll", 1, |vm, this, a, _| regexp_delegate(vm, this, arg(a, 0), 1, a));
    m!("search", 1, |vm, this, a, _| regexp_delegate(vm, this, arg(a, 0), 2, a));
    m!("replace", 2, |vm, this, a, _| replace(vm, this, a, false));
    m!("replaceAll", 2, |vm, this, a, _| replace(vm, this, a, true));
    m!("split", 2, split);
    // Annex B HTML methods.
    for (name, tag, attr_name) in [
        ("anchor", "a", "name"),
        ("big", "big", ""),
        ("blink", "blink", ""),
        ("bold", "b", ""),
        ("fixed", "tt", ""),
        ("fontcolor", "font", "color"),
        ("fontsize", "font", "size"),
        ("italics", "i", ""),
        ("link", "a", "href"),
        ("small", "small", ""),
        ("strike", "strike", ""),
        ("sub", "sub", ""),
        ("sup", "sup", ""),
    ] {
        let t = vm.new_string(tag);
        let an = vm.new_string(attr_name);
        let f = vm.native_fn_data(name, if attr_name.is_empty() { 0 } else { 1 }, html_method, false, Box::new([t, an]));
        let k = vm.key(name);
        vm.define_data(sp, k, Value::object(f), attr::HIDDEN);
    }
    // Iterator.
    let ip = vm.intrinsics.iterator_prototype;
    let sip = vm.alloc_object(Some(ip), Class::Ordinary);
    vm.intrinsics.string_iterator_prototype = sip;
    vm.method(sip, "next", 0, string_iterator_next);
    let tag = vm.new_string("String Iterator");
    let kt = Key::Sym(vm.intrinsics.sym_to_string_tag);
    vm.define_data(sip, kt, tag, attr::CONFIGURABLE);
    let si = vm.intrinsics.sym_iterator;
    vm.method_sym(sp, si, "[Symbol.iterator]", 0, |vm, this, _, _| {
        let s = this_str(vm, this)?;
        let p = vm.intrinsics.string_iterator_prototype;
        let st = super::iterators::IterState { kind: 3, target: Value::str(s), pos: 0, extra: Vec::new(), done: false };
        Ok(Value::object(vm.alloc_object(Some(p), Class::Iter(Box::new(st)))))
    });
}

fn push_cp(u: &mut Vec<u16>, cp: u32) {
    if cp >= 0x10000 {
        let c = cp - 0x10000;
        u.push(0xD800 + (c >> 10) as u16);
        u.push(0xDC00 + (c & 0x3FF) as u16);
    } else {
        u.push(cp as u16);
    }
}

/// CodePointAt: (code point, units).
fn code_point_at(u: &[u16], i: usize) -> (u32, usize) {
    let c = u[i];
    if (0xD800..0xDC00).contains(&c) && i + 1 < u.len() && (0xDC00..0xE000).contains(&u[i + 1]) {
        return (0x10000 + (((c - 0xD800) as u32) << 10) + (u[i + 1] - 0xDC00) as u32, 2);
    }
    (c as u32, 1)
}

fn string_ctor(vm: &mut Vm, _: Value, a: &[Value], nt: Value) -> JsResult<Value> {
    let s = if a.is_empty() {
        vm.names.empty
    } else {
        let v = a[0];
        if nt.is_undefined() {
            if let Some(sym) = v.as_symbol() {
                let d = vm.display(Value::symbol(sym));
                return Ok(vm.new_string(&d));
            }
        }
        vm.to_string(v)?
    };
    if nt.is_undefined() {
        return Ok(Value::str(s));
    }
    let p = vm.proto_from_ctor(nt, |i| i.string_prototype)?;
    Ok(Value::object(vm.alloc_object(Some(p), Class::String(s))))
}

fn string_raw(vm: &mut Vm, _: Value, a: &[Value], _: Value) -> JsResult<Value> {
    let cooked = vm.to_object(arg(a, 0))?;
    let kr = Key::Str(vm.names.raw);
    let raw = vm.get(cooked, kr)?;
    let raw = vm.to_object(raw)?;
    let len = vm.length_of(raw)?;
    let mut out: Vec<u16> = Vec::new();
    for i in 0..len {
        let k = vm.index_key(i);
        let seg = vm.get(raw, k)?;
        let s = vm.to_string(seg)?;
        out.extend(vm.str(s).units());
        if i + 1 < len && (i as usize + 1) < a.len() {
            let t = vm.to_string(a[i as usize + 1])?;
            out.extend(vm.str(t).units());
        }
    }
    Ok(vm.new_string_units(&out))
}

/// thisStringValue.
fn this_string_value(vm: &mut Vm, v: Value) -> JsResult<StrId> {
    if let Some(s) = v.as_string() {
        return Ok(s);
    }
    if let Some(o) = v.as_object() {
        if let Class::String(s) = vm.obj(o).class {
            return Ok(s);
        }
    }
    Err(vm.type_error("String.prototype.toString requires that 'this' be a String"))
}

/// RequireObjectCoercible(this) + ToString.
fn this_str(vm: &mut Vm, this: Value) -> JsResult<StrId> {
    if this.is_nullish() {
        return Err(vm.type_error("String.prototype method called on null or undefined"));
    }
    vm.to_string(this)
}

fn search_string(vm: &mut Vm, v: Value, name: &str) -> JsResult<Vec<u16>> {
    if super::regexp::is_regexp(vm, v)? {
        return Err(vm.type_error(&alloc::format!("First argument to String.prototype.{name} must not be a regular expression")));
    }
    let s = vm.to_string(v)?;
    Ok(vm.str(s).to_units())
}

pub fn index_of(s: &[u16], t: &[u16], from: usize) -> Option<usize> {
    if t.is_empty() {
        return if from <= s.len() { Some(from) } else { None };
    }
    if t.len() > s.len() {
        return None;
    }
    (from..=s.len() - t.len()).find(|&i| s[i..i + t.len()] == *t)
}

fn rel_range(vm: &mut Vm, start: Value, end: Value, len: f64) -> JsResult<(usize, usize)> {
    let s = vm.to_integer_or_infinity(start)?;
    let from = if s < 0.0 { (len + s).max(0.0) } else { s.min(len) };
    let e = if end.is_undefined() { len } else { vm.to_integer_or_infinity(end)? };
    let to = if e < 0.0 { (len + e).max(0.0) } else { e.min(len) };
    Ok((from as usize, to as usize))
}

fn pad(vm: &mut Vm, this: Value, a: &[Value], at_start: bool) -> JsResult<Value> {
    let s = this_str(vm, this)?;
    let max = vm.to_length(arg(a, 0))?;
    let su = vm.str(s).to_units();
    if max <= su.len() as u64 {
        return Ok(Value::str(s));
    }
    let filler: Vec<u16> = if arg(a, 1).is_undefined() {
        alloc::vec![b' ' as u16]
    } else {
        let f = vm.to_string(arg(a, 1))?;
        vm.str(f).to_units()
    };
    if filler.is_empty() {
        return Ok(Value::str(s));
    }
    if max as usize > vm.max_string_len() {
        return Err(vm.range_error("Invalid string length"));
    }
    let fill_len = max as usize - su.len();
    let fill: Vec<u16> = filler.iter().copied().cycle().take(fill_len).collect();
    let out: Vec<u16> = if at_start { fill.into_iter().chain(su).collect() } else { su.into_iter().chain(fill).collect() };
    Ok(vm.new_string_units(&out))
}

fn change_case(vm: &mut Vm, this: Value, upper: bool) -> JsResult<Value> {
    let s = this_str(vm, this)?;
    let u = vm.str(s).to_units();
    if u.iter().all(|&c| c < 0x80) {
        let out: Vec<u16> = u.iter().map(|&c| {
            let b = c as u8;
            (if upper { b.to_ascii_uppercase() } else { b.to_ascii_lowercase() }) as u16
        }).collect();
        return Ok(vm.new_string_units(&out));
    }
    let chars: Vec<Result<char, u16>> = char::decode_utf16(u.iter().copied()).map(|r| r.map_err(|e| e.unpaired_surrogate())).collect();
    let mut out: Vec<u16> = Vec::with_capacity(u.len());
    for (i, r) in chars.iter().enumerate() {
        match r {
            Err(lone) => out.push(*lone),
            Ok(c) => {
                if !upper && *c == 'Σ' {
                    // Final sigma: preceded by a cased letter and not
                    // followed by one (ignoring case-ignorable characters).
                    let before = chars[..i].iter().rev().filter_map(|x| x.ok()).find(|x| !is_case_ignorable(*x)).is_some_and(|x| x.is_alphabetic() && (x.is_lowercase() || x.is_uppercase()));
                    let after = chars[i + 1..].iter().filter_map(|x| x.ok()).find(|x| !is_case_ignorable(*x)).is_some_and(|x| x.is_alphabetic() && (x.is_lowercase() || x.is_uppercase()));
                    out.push(if before && !after { 'ς' as u16 } else { 'σ' as u16 });
                    continue;
                }
                let mut b = [0u16; 2];
                if upper {
                    for x in c.to_uppercase() {
                        out.extend_from_slice(x.encode_utf16(&mut b));
                    }
                } else {
                    for x in c.to_lowercase() {
                        out.extend_from_slice(x.encode_utf16(&mut b));
                    }
                }
            }
        }
    }
    Ok(vm.new_string_units(&out))
}

fn is_case_ignorable(c: char) -> bool {
    matches!(c, '\'' | '.' | ':' | '^' | '`' | '\u{AD}' | '\u{B7}' | '\u{2019}') || ('\u{300}'..='\u{36F}').contains(&c)
}

fn trim(vm: &mut Vm, this: Value, start: bool, end: bool) -> JsResult<Value> {
    let s = this_str(vm, this)?;
    let u = vm.str(s).to_units();
    let mut a = 0;
    let mut b = u.len();
    if start {
        while a < b && is_js_space(u[a]) {
            a += 1;
        }
    }
    if end {
        while b > a && is_js_space(u[b - 1]) {
            b -= 1;
        }
    }
    if a == 0 && b == u.len() {
        return Ok(Value::str(s));
    }
    Ok(vm.new_string_units(&u[a..b]))
}

/// match (0), matchAll (1), search (2).
fn regexp_delegate(vm: &mut Vm, this: Value, regexp: Value, kind: u8, a: &[Value]) -> JsResult<Value> {
    let _ = a;
    if this.is_nullish() {
        return Err(vm.type_error("String.prototype method called on null or undefined"));
    }
    let sym = match kind {
        0 => vm.intrinsics.sym_match,
        1 => vm.intrinsics.sym_match_all,
        _ => vm.intrinsics.sym_search,
    };
    if !regexp.is_nullish() {
        if kind == 1 && super::regexp::is_regexp(vm, regexp)? {
            let ro = regexp.as_object().unwrap();
            let kf = Key::Str(vm.names.flags);
            let f = vm.get(ro, kf)?;
            if f.is_nullish() {
                return Err(vm.type_error("RegExp flags are null or undefined"));
            }
            let fs = vm.to_string(f)?;
            if !vm.str(fs).to_rust_lossy().contains('g') {
                return Err(vm.type_error("String.prototype.matchAll called with a non-global RegExp argument"));
            }
        }
        if let Some(m) = vm.get_method(regexp, Key::Sym(sym))? {
            return vm.call(m, regexp, &[this]);
        }
    }
    let s = vm.to_string(this)?;
    vm.root(Value::str(s));
    let pattern = if regexp.is_undefined() { Value::str(vm.names.empty) } else { vm.to_string_value(regexp)? };
    let flags = if kind == 1 { vm.new_string("g") } else { Value::UNDEFINED };
    let rc = Value::object(vm.intrinsics.regexp);
    let rx = vm.construct(rc, &[pattern, flags], rc)?;
    vm.root(rx);
    let m = vm.get_v(rx, Key::Sym(sym))?;
    vm.call(m, rx, &[Value::str(s)])
}

fn replace(vm: &mut Vm, this: Value, a: &[Value], all: bool) -> JsResult<Value> {
    if this.is_nullish() {
        return Err(vm.type_error("String.prototype.replace called on null or undefined"));
    }
    let (search, replace_value) = (arg(a, 0), arg(a, 1));
    if !search.is_nullish() {
        if all && super::regexp::is_regexp(vm, search)? {
            let so = search.as_object().unwrap();
            let kf = Key::Str(vm.names.flags);
            let f = vm.get(so, kf)?;
            if f.is_nullish() {
                return Err(vm.type_error("RegExp flags are null or undefined"));
            }
            let fs = vm.to_string(f)?;
            if !vm.str(fs).to_rust_lossy().contains('g') {
                return Err(vm.type_error("replaceAll must be called with a global RegExp"));
            }
        }
        let sr = Key::Sym(vm.intrinsics.sym_replace);
        if let Some(m) = vm.get_method(search, sr)? {
            return vm.call(m, search, &[this, replace_value]);
        }
    }
    let s = vm.to_string(this)?;
    vm.root(Value::str(s));
    let ss = vm.to_string(search)?;
    vm.root(Value::str(ss));
    let functional = vm.is_callable(replace_value);
    let rep: Vec<u16> = if functional {
        Vec::new()
    } else {
        let r = vm.to_string(replace_value)?;
        vm.str(r).to_units()
    };
    let su = vm.str(s).to_units();
    let tu = vm.str(ss).to_units();
    let adv = tu.len().max(1);
    let mut positions = Vec::new();
    let mut p = index_of(&su, &tu, 0);
    while let Some(i) = p {
        positions.push(i);
        if !all {
            break;
        }
        p = index_of(&su, &tu, i + adv);
    }
    let mut out = Vec::new();
    let mut end = 0;
    for pos in positions {
        out.extend_from_slice(&su[end..pos]);
        let replacement = if functional {
            let r = vm.call(replace_value, Value::UNDEFINED, &[Value::str(ss), Value::from_usize(pos), Value::str(s)])?;
            let rs = vm.to_string(r)?;
            vm.str(rs).to_units()
        } else {
            super::regexp::get_substitution(vm, &tu, &su, pos, &[], Value::UNDEFINED, &rep)?
        };
        out.extend_from_slice(&replacement);
        end = pos + tu.len();
        if out.len() > vm.max_string_len() {
            return Err(vm.range_error("Invalid string length"));
        }
    }
    out.extend_from_slice(&su[end..]);
    Ok(vm.new_string_units(&out))
}

fn split(vm: &mut Vm, this: Value, a: &[Value], _: Value) -> JsResult<Value> {
    if this.is_nullish() {
        return Err(vm.type_error("String.prototype.split called on null or undefined"));
    }
    let (sep, limit) = (arg(a, 0), arg(a, 1));
    if !sep.is_nullish() {
        let ks = Key::Sym(vm.intrinsics.sym_split);
        if let Some(m) = vm.get_method(sep, ks)? {
            return vm.call(m, sep, &[this, limit]);
        }
    }
    let s = vm.to_string(this)?;
    vm.root(Value::str(s));
    let lim = if limit.is_undefined() { u32::MAX } else { vm.to_uint32(limit)? };
    let r = vm.to_string(sep)?;
    let su = vm.str(s).to_units();
    let ru = vm.str(r).to_units();
    if lim == 0 {
        return Ok(Value::object(vm.new_array(Vec::new())));
    }
    if sep.is_undefined() {
        return Ok(Value::object(vm.new_array(alloc::vec![Value::str(s)])));
    }
    if su.is_empty() {
        if !ru.is_empty() {
            return Ok(Value::object(vm.new_array(alloc::vec![Value::str(s)])));
        }
        return Ok(Value::object(vm.new_array(Vec::new())));
    }
    let mut out = Vec::new();
    if ru.is_empty() {
        for &c in su.iter().take(lim as usize) {
            let v = vm.char_string(c);
            out.push(v);
        }
        return Ok(Value::object(vm.new_array(out)));
    }
    let mut p = 0;
    let mut q = index_of(&su, &ru, 0);
    while let Some(i) = q {
        let v = vm.new_string_units(&su[p..i]);
        out.push(v);
        if out.len() as u32 >= lim {
            return Ok(Value::object(vm.new_array(out)));
        }
        p = i + ru.len();
        q = index_of(&su, &ru, p);
    }
    let v = vm.new_string_units(&su[p..]);
    out.push(v);
    Ok(Value::object(vm.new_array(out)))
}

fn html_method(vm: &mut Vm, this: Value, a: &[Value], _: Value) -> JsResult<Value> {
    let s = this_str(vm, this)?;
    let tag = vm.display(vm.native_data(0));
    let attr_name = vm.display(vm.native_data(1));
    let mut out: Vec<u16> = alloc::format!("<{tag}").encode_utf16().collect();
    if !attr_name.is_empty() {
        let v = vm.to_string(arg(a, 0))?;
        let vu: Vec<u16> = vm.str(v).units().flat_map(|c| if c == b'"' as u16 { "&quot;".encode_utf16().collect::<Vec<u16>>() } else { alloc::vec![c] }).collect();
        out.extend(alloc::format!(" {attr_name}=\"").encode_utf16());
        out.extend(vu);
        out.push(b'"' as u16);
    }
    out.push(b'>' as u16);
    out.extend(vm.str(s).units());
    out.extend(alloc::format!("</{tag}>").encode_utf16());
    Ok(vm.new_string_units(&out))
}

fn string_iterator_next(vm: &mut Vm, this: Value, _: &[Value], _: Value) -> JsResult<Value> {
    let Some(it) = this.as_object() else { return Err(vm.type_error("next called on incompatible receiver")) };
    let (s, pos, done) = match &vm.obj(it).class {
        Class::Iter(st) if st.kind == 3 => (st.target.as_string().unwrap(), st.pos, st.done),
        _ => return Err(vm.type_error("next called on incompatible receiver")),
    };
    let len = vm.str(s).len();
    if done || pos >= len {
        if let Class::Iter(st) = &mut vm.obj_mut(it).class {
            st.done = true;
        }
        return Ok(vm.iter_result(Value::UNDEFINED, true));
    }
    let c = vm.str(s).at(pos);
    let n = if (0xD800..0xDC00).contains(&c) && pos + 1 < len && (0xDC00..0xE000).contains(&vm.str(s).at(pos + 1)) { 2 } else { 1 };
    let v = if n == 1 {
        vm.char_string(c)
    } else {
        let lo = vm.str(s).at(pos + 1);
        vm.new_string_units(&[c, lo])
    };
    if let Class::Iter(st) = &mut vm.obj_mut(it).class {
        st.pos += n;
    }
    Ok(vm.iter_result(v, false))
}
