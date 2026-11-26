//! RegExp objects (ECMA-262 §22.2.4–22.2.9) on top of `crate::regex`.

use alloc::boxed::Box;
use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec::Vec;

use super::object::arg;
use crate::object::{attr, Class, Key};
use crate::regex::Regex;
use crate::value::{ObjId, Value};
use crate::vm::gc::Trace;
use crate::vm::{JsResult, Throw, Vm};

pub struct RegExpData {
    /// The pattern as given (OriginalSource) and flags string.
    pub source: Value,
    pub flags: Value,
    pub re: Option<Rc<Regex>>,
    /// Created by the RegExp constructor of this realm (legacy features).
    pub legacy: bool,
}

impl Trace for RegExpData {
    fn trace(&self, out: &mut Vec<Value>) {
        out.push(self.source);
        out.push(self.flags);
    }
}

fn compile(vm: &mut Vm, p: Value, f: Value) -> JsResult<Rc<Regex>> {
    let pu = vm.str(p.as_string().unwrap()).to_units();
    let fs = vm.str(f.as_string().unwrap()).to_rust_lossy();
    let key = (pu.clone(), fs.clone());
    if let Some(r) = vm.regex_cache.iter().find(|(k, _)| *k == key).map(|x| x.1.clone()) {
        return Ok(r);
    }
    match crate::regex::compile(&pu, &fs) {
        Ok(r) => {
            let r = Rc::new(r);
            if vm.regex_cache.len() > 64 {
                vm.regex_cache.remove(0);
            }
            vm.regex_cache.push((key, r.clone()));
            Ok(r)
        }
        Err(m) => {
            let src = String::from_utf16_lossy(&pu);
            Err(vm.syntax_error(&alloc::format!("Invalid regular expression: /{src}/{fs}: {m}")))
        }
    }
}

/// A regular expression literal.
pub fn create(vm: &mut Vm, pattern: Value, flags: Value) -> JsResult<Value> {
    let p = vm.intrinsics.regexp_prototype;
    let o = vm.alloc_object(Some(p), Class::RegExp(Box::new(RegExpData { source: pattern, flags, re: None, legacy: true })));
    initialize(vm, o, pattern, flags)?;
    Ok(Value::object(o))
}

/// RegExpInitialize.
fn initialize(vm: &mut Vm, o: ObjId, pattern: Value, flags: Value) -> JsResult<()> {
    let p = if pattern.is_undefined() { Value::str(vm.names.empty) } else { vm.to_string_value(pattern)? };
    let f = if flags.is_undefined() { Value::str(vm.names.empty) } else { vm.to_string_value(flags)? };
    let re = compile(vm, p, f)?;
    if let Class::RegExp(d) = &mut vm.obj_mut(o).class {
        d.source = p;
        d.flags = f;
        d.re = Some(re);
    }
    let k = Key::Str(vm.names.last_index);
    // lastIndex is created by RegExpAlloc; Set(…, true).
    if vm.raw_get_own(o, k).is_none() {
        vm.define_data(o, k, Value::int(0), attr::WRITABLE);
        Ok(())
    } else {
        vm.set_or_throw(o, k, Value::int(0))
    }
}

pub fn init(vm: &mut Vm) {
    let op = vm.intrinsics.object_prototype;
    let rp = vm.alloc_object(Some(op), Class::Ordinary);
    vm.intrinsics.regexp_prototype = rp;
    let c = vm.constructor("RegExp", 2, regexp_ctor, rp);
    vm.intrinsics.regexp = c;
    vm.global_prop("RegExp", Value::object(c));
    let sp = vm.intrinsics.sym_species;
    vm.getter_sym(c, sp, "[Symbol.species]", |_, this, _, _| Ok(this));
    vm.method(c, "escape", 1, escape);
    vm.method(rp, "exec", 1, |vm, this, a, _| {
        let r = this_regexp(vm, this)?;
        let s = vm.to_string(arg(a, 0))?;
        builtin_exec(vm, r, s)
    });
    vm.method(rp, "test", 1, |vm, this, a, _| {
        let Some(r) = this.as_object() else { return Err(vm.type_error("RegExp.prototype.test called on non-object")) };
        let s = vm.to_string(arg(a, 0))?;
        Ok(Value::bool(!regexp_exec(vm, r, s)?.is_null()))
    });
    vm.method(rp, "toString", 0, |vm, this, _, _| {
        let Some(r) = this.as_object() else { return Err(vm.type_error("RegExp.prototype.toString called on non-object")) };
        let ks = Key::Str(vm.names.source);
        let s = vm.get(r, ks)?;
        let s = vm.to_string(s)?;
        let kf = Key::Str(vm.names.flags);
        let f = vm.get(r, kf)?;
        let f = vm.to_string(f)?;
        let mut out: Vec<u16> = alloc::vec![b'/' as u16];
        out.extend(vm.str(s).units());
        out.push(b'/' as u16);
        out.extend(vm.str(f).units());
        Ok(vm.new_string_units(&out))
    });
    vm.method(rp, "compile", 2, |vm, this, a, _| {
        let o = this_regexp(vm, this)?;
        let (p, f) = (arg(a, 0), arg(a, 1));
        let (p, f) = match p.as_object().map(|x| &vm.obj(x).class) {
            Some(Class::RegExp(d)) => {
                if !f.is_undefined() {
                    return Err(vm.type_error("Cannot supply flags when constructing one RegExp from another"));
                }
                (d.source, d.flags)
            }
            _ => (p, f),
        };
        initialize(vm, o, p, f)?;
        Ok(Value::object(o))
    });
    vm.getter(rp, "flags", flags_getter, None);
    vm.getter(rp, "source", |vm, this, _, _| {
        let Some(o) = this.as_object() else { return Err(vm.type_error("RegExp.prototype.source getter called on non-object")) };
        match &vm.obj(o).class {
            Class::RegExp(d) => {
                let s = d.source;
                let u = vm.str(s.as_string().unwrap()).to_units();
                Ok(vm.new_string_units(&escape_pattern(&u)))
            }
            _ if o == vm.intrinsics.regexp_prototype => Ok(vm.new_string("(?:)")),
            _ => Err(vm.type_error("RegExp.prototype.source getter called on non-RegExp")),
        }
    }, None);
    for (name, ch) in [
        ("hasIndices", 'd'),
        ("global", 'g'),
        ("ignoreCase", 'i'),
        ("multiline", 'm'),
        ("dotAll", 's'),
        ("unicode", 'u'),
        ("unicodeSets", 'v'),
        ("sticky", 'y'),
    ] {
        let gname = alloc::format!("get {name}");
        let g = vm.native_fn_data(&gname, 0, flag_getter, false, Box::new([Value::int(ch as i32)]));
        let k = vm.key(name);
        vm.define_accessor(rp, k, Value::object(g), Value::UNDEFINED, attr::CONFIGURABLE);
    }
    let i = &vm.intrinsics;
    let (sm, sma, sr, ss, ssp) = (i.sym_match, i.sym_match_all, i.sym_replace, i.sym_search, i.sym_split);
    vm.method_sym(rp, sm, "[Symbol.match]", 1, symbol_match);
    vm.method_sym(rp, sma, "[Symbol.matchAll]", 1, symbol_match_all);
    vm.method_sym(rp, sr, "[Symbol.replace]", 2, symbol_replace);
    vm.method_sym(rp, ss, "[Symbol.search]", 1, symbol_search);
    vm.method_sym(rp, ssp, "[Symbol.split]", 2, symbol_split);
    // %RegExpStringIteratorPrototype%
    let ip = vm.intrinsics.iterator_prototype;
    let rsip = vm.alloc_object(Some(ip), Class::Ordinary);
    vm.intrinsics.regexp_string_iterator_prototype = rsip;
    vm.method(rsip, "next", 0, string_iterator_next);
    let tag = vm.new_string("RegExp String Iterator");
    let kt = Key::Sym(vm.intrinsics.sym_to_string_tag);
    vm.define_data(rsip, kt, tag, attr::CONFIGURABLE);
}

fn this_regexp(vm: &mut Vm, this: Value) -> JsResult<ObjId> {
    match this.as_object() {
        Some(o) if matches!(vm.obj(o).class, Class::RegExp(_)) => Ok(o),
        _ => Err(vm.type_error("RegExp method called on incompatible receiver")),
    }
}

/// IsRegExp.
pub fn is_regexp(vm: &mut Vm, v: Value) -> JsResult<bool> {
    let Some(o) = v.as_object() else { return Ok(false) };
    let km = Key::Sym(vm.intrinsics.sym_match);
    let m = vm.get(o, km)?;
    if !m.is_undefined() {
        return Ok(vm.to_boolean(m));
    }
    Ok(matches!(vm.obj(o).class, Class::RegExp(_)))
}

fn regexp_ctor(vm: &mut Vm, _: Value, a: &[Value], nt: Value) -> JsResult<Value> {
    let (pattern, flags) = (arg(a, 0), arg(a, 1));
    let pattern_is_re = is_regexp(vm, pattern)?;
    let me = Value::object(vm.intrinsics.regexp);
    let nt = if nt.is_undefined() {
        if pattern_is_re && flags.is_undefined() {
            let kc = Key::Str(vm.names.constructor);
            let c = vm.get(pattern.as_object().unwrap(), kc)?;
            if vm.same_value(c, me) {
                return Ok(pattern);
            }
        }
        me
    } else {
        nt
    };
    let (p, f) = if let Some(Class::RegExp(d)) = pattern.as_object().map(|o| &vm.obj(o).class) {
        (d.source, if flags.is_undefined() { d.flags } else { flags })
    } else if pattern_is_re {
        let po = pattern.as_object().unwrap();
        let ks = Key::Str(vm.names.source);
        let p = vm.get(po, ks)?;
        let f = if flags.is_undefined() {
            let kf = Key::Str(vm.names.flags);
            vm.get(po, kf)?
        } else {
            flags
        };
        (p, f)
    } else {
        (pattern, flags)
    };
    let proto = vm.proto_from_ctor(nt, |i| i.regexp_prototype)?;
    let o = vm.alloc_object(Some(proto), Class::RegExp(Box::new(RegExpData { source: Value::str(vm.names.empty), flags: Value::str(vm.names.empty), re: None, legacy: false })));
    let kl = Key::Str(vm.names.last_index);
    vm.define_data(o, kl, Value::int(0), attr::WRITABLE);
    vm.root(Value::object(o));
    initialize(vm, o, p, f)?;
    Ok(Value::object(o))
}

fn flag_getter(vm: &mut Vm, this: Value, _: &[Value], _: Value) -> JsResult<Value> {
    let ch = char::from_u32(vm.native_data(0).as_int().unwrap() as u32).unwrap();
    let Some(o) = this.as_object() else { return Err(vm.type_error("RegExp flag getter called on non-object")) };
    match &vm.obj(o).class {
        Class::RegExp(d) => {
            let f = d.flags;
            Ok(Value::bool(vm.str(f.as_string().unwrap()).to_rust_lossy().contains(ch)))
        }
        _ if o == vm.intrinsics.regexp_prototype => Ok(Value::UNDEFINED),
        _ => Err(vm.type_error("RegExp flag getter called on non-RegExp")),
    }
}

fn flags_getter(vm: &mut Vm, this: Value, _: &[Value], _: Value) -> JsResult<Value> {
    let Some(o) = this.as_object() else { return Err(vm.type_error("RegExp.prototype.flags getter called on non-object")) };
    let mut out = String::new();
    for (name, ch) in [("hasIndices", 'd'), ("global", 'g'), ("ignoreCase", 'i'), ("multiline", 'm'), ("dotAll", 's'), ("unicode", 'u'), ("unicodeSets", 'v'), ("sticky", 'y')] {
        let k = vm.key(name);
        let v = vm.get(o, k)?;
        if vm.to_boolean(v) {
            out.push(ch);
        }
    }
    Ok(vm.new_string(&out))
}

/// EscapeRegExpPattern for `source`.
fn escape_pattern(u: &[u16]) -> Vec<u16> {
    if u.is_empty() {
        return "(?:)".encode_utf16().collect();
    }
    let mut out = Vec::new();
    let mut in_class = false;
    let mut i = 0;
    while i < u.len() {
        let c = u[i];
        match c {
            0x5C => {
                out.push(c);
                if i + 1 < u.len() {
                    i += 1;
                    out.push(u[i]);
                }
            }
            0x2F if !in_class => out.extend("\\/".encode_utf16()),
            0x5B => {
                in_class = true;
                out.push(c);
            }
            0x5D => {
                in_class = false;
                out.push(c);
            }
            0x0A => out.extend("\\n".encode_utf16()),
            0x0D => out.extend("\\r".encode_utf16()),
            0x2028 => out.extend("\\u2028".encode_utf16()),
            0x2029 => out.extend("\\u2029".encode_utf16()),
            _ => out.push(c),
        }
        i += 1;
    }
    out
}

/// RegExpExec(R, S).
pub fn regexp_exec(vm: &mut Vm, r: ObjId, s: crate::value::StrId) -> JsResult<Value> {
    let ke = Key::Str(vm.names.exec);
    let exec = vm.get(r, ke)?;
    if vm.is_callable(exec) {
        let res = vm.call(exec, Value::object(r), &[Value::str(s)])?;
        if !(res.is_object() || res.is_null()) {
            return Err(vm.type_error("exec result must be an object or null"));
        }
        return Ok(res);
    }
    if !matches!(vm.obj(r).class, Class::RegExp(_)) {
        return Err(vm.type_error("RegExp exec method called on incompatible receiver"));
    }
    builtin_exec(vm, r, s)
}

fn advance(s: &[u16], i: usize, unicode: bool) -> usize {
    if unicode && i + 1 < s.len() && (0xD800..0xDC00).contains(&s[i]) && (0xDC00..0xE000).contains(&s[i + 1]) {
        i + 2
    } else {
        i + 1
    }
}

/// RegExpBuiltinExec.
pub fn builtin_exec(vm: &mut Vm, r: ObjId, s: crate::value::StrId) -> JsResult<Value> {
    let kl = Key::Str(vm.names.last_index);
    let li = vm.get(r, kl)?;
    let last_index = vm.to_length(li)?;
    let re = match &vm.obj(r).class {
        Class::RegExp(d) => d.re.clone().unwrap(),
        _ => unreachable!(),
    };
    let flags = re.flags;
    let global_or_sticky = flags.global || flags.sticky;
    let mut li = if global_or_sticky { last_index as usize } else { 0 };
    let input = vm.str(s).to_units();
    let unicode = flags.unicode || flags.unicode_sets;
    let limit = vm.budget.map(|b| b.saturating_mul(16)).unwrap_or(u64::MAX);
    let caps = loop {
        if li > input.len() {
            if global_or_sticky {
                vm.set_or_throw(r, kl, Value::int(0))?;
            }
            return Ok(Value::NULL);
        }
        match re.match_at(&input, li, limit) {
            Err(_) => return Err(Throw::Terminate),
            Ok(Some(c)) => break c,
            Ok(None) => {
                if flags.sticky {
                    vm.set_or_throw(r, kl, Value::int(0))?;
                    return Ok(Value::NULL);
                }
                li = advance(&input, li, unicode);
            }
        }
    };
    let (start, end) = caps[0].unwrap();
    if global_or_sticky {
        vm.set_or_throw(r, kl, Value::from_usize(end))?;
    }
    // The result array.
    let mut elems = Vec::with_capacity(caps.len());
    for c in &caps {
        elems.push(match c {
            Some((a, b)) => vm.new_string_units(&input[*a..*b]),
            None => Value::UNDEFINED,
        });
        if let Some(v) = elems.last() {
            vm.root(*v);
        }
    }
    let arr = vm.new_array(elems.clone());
    vm.root(Value::object(arr));
    let ki = Key::Str(vm.names.index);
    vm.create_data_property_or_throw(arr, ki, Value::from_usize(start))?;
    let kin = Key::Str(vm.names.input);
    vm.create_data_property_or_throw(arr, kin, Value::str(s))?;
    let groups = if re.names.is_empty() {
        Value::UNDEFINED
    } else {
        let g = vm.alloc_object(None, Class::Ordinary);
        for (name, idxs) in &re.names {
            let v = idxs.iter().find_map(|&i| caps[i].map(|_| elems[i])).unwrap_or(Value::UNDEFINED);
            let ns = vm.new_string_units(name);
            let k = vm.to_property_key(ns)?;
            vm.create_data_property_or_throw(g, k, v)?;
        }
        Value::object(g)
    };
    let kg = Key::Str(vm.names.groups);
    vm.create_data_property_or_throw(arr, kg, groups)?;
    if flags.has_indices {
        let mut pairs = Vec::new();
        for c in &caps {
            pairs.push(match c {
                Some((a, b)) => Value::object(vm.new_array(alloc::vec![Value::from_usize(*a), Value::from_usize(*b)])),
                None => Value::UNDEFINED,
            });
            if let Some(v) = pairs.last() {
                vm.root(*v);
            }
        }
        let ind = vm.new_array(pairs.clone());
        vm.root(Value::object(ind));
        let gi = if re.names.is_empty() {
            Value::UNDEFINED
        } else {
            let g = vm.alloc_object(None, Class::Ordinary);
            for (name, idxs) in &re.names {
                let v = idxs.iter().find_map(|&i| caps[i].map(|_| pairs[i])).unwrap_or(Value::UNDEFINED);
                let ns = vm.new_string_units(name);
                let k = vm.to_property_key(ns)?;
                vm.create_data_property_or_throw(g, k, v)?;
            }
            Value::object(g)
        };
        vm.create_data_property_or_throw(ind, kg, gi)?;
        let kd = Key::Str(vm.names.indices);
        vm.create_data_property_or_throw(arr, kd, Value::object(ind))?;
    }
    Ok(Value::object(arr))
}

fn this_obj(vm: &mut Vm, this: Value, name: &str) -> JsResult<ObjId> {
    match this.as_object() {
        Some(o) => Ok(o),
        None => Err(vm.type_error(&alloc::format!("RegExp.prototype[{name}] called on non-object"))),
    }
}

fn flags_of(vm: &mut Vm, r: ObjId) -> JsResult<String> {
    let kf = Key::Str(vm.names.flags);
    let f = vm.get(r, kf)?;
    let s = vm.to_string(f)?;
    Ok(vm.str(s).to_rust_lossy())
}

fn set_last_index(vm: &mut Vm, r: ObjId, v: Value) -> JsResult<()> {
    let kl = Key::Str(vm.names.last_index);
    vm.set_or_throw(r, kl, v)
}

fn match_string(vm: &mut Vm, result: Value) -> JsResult<crate::value::StrId> {
    let o = result.as_object().unwrap();
    let v = vm.get(o, Key::Index(0))?;
    vm.to_string(v)
}

fn advance_last_index(vm: &mut Vm, r: ObjId, s: crate::value::StrId, full_unicode: bool) -> JsResult<()> {
    let kl = Key::Str(vm.names.last_index);
    let li = vm.get(r, kl)?;
    let this_index = vm.to_length(li)?;
    let u = vm.str(s).to_units();
    let next = if (this_index as usize) < u.len() { advance(&u, this_index as usize, full_unicode) as u64 } else { this_index + 1 };
    vm.set_or_throw(r, kl, Value::number(next as f64))
}

fn symbol_match(vm: &mut Vm, this: Value, a: &[Value], _: Value) -> JsResult<Value> {
    let r = this_obj(vm, this, "Symbol.match")?;
    let s = vm.to_string(arg(a, 0))?;
    let flags = flags_of(vm, r)?;
    if !flags.contains('g') {
        return regexp_exec(vm, r, s);
    }
    let full_unicode = flags.contains('u') || flags.contains('v');
    set_last_index(vm, r, Value::int(0))?;
    let mut out = Vec::new();
    loop {
        let res = regexp_exec(vm, r, s)?;
        if res.is_null() {
            return Ok(if out.is_empty() { Value::NULL } else { Value::object(vm.new_array(out)) });
        }
        let m = match_string(vm, res)?;
        vm.root(Value::str(m));
        out.push(Value::str(m));
        if vm.str(m).is_empty() {
            advance_last_index(vm, r, s, full_unicode)?;
        }
    }
}

fn symbol_search(vm: &mut Vm, this: Value, a: &[Value], _: Value) -> JsResult<Value> {
    let r = this_obj(vm, this, "Symbol.search")?;
    let s = vm.to_string(arg(a, 0))?;
    let kl = Key::Str(vm.names.last_index);
    let prev = vm.get(r, kl)?;
    if !vm.same_value(prev, Value::int(0)) {
        vm.set_or_throw(r, kl, Value::int(0))?;
    }
    let res = regexp_exec(vm, r, s)?;
    let cur = vm.get(r, kl)?;
    if !vm.same_value(cur, prev) {
        vm.set_or_throw(r, kl, prev)?;
    }
    if res.is_null() {
        return Ok(Value::int(-1));
    }
    let ki = Key::Str(vm.names.index);
    vm.get(res.as_object().unwrap(), ki)
}

fn symbol_match_all(vm: &mut Vm, this: Value, a: &[Value], _: Value) -> JsResult<Value> {
    let r = this_obj(vm, this, "Symbol.matchAll")?;
    let s = vm.to_string(arg(a, 0))?;
    let rc = Value::object(vm.intrinsics.regexp);
    let c = super::promise::species_constructor(vm, r, rc)?;
    let flags = flags_of(vm, r)?;
    let fv = vm.new_string(&flags);
    let matcher = vm.construct(c, &[Value::object(r), fv], c)?;
    vm.root(matcher);
    let kl = Key::Str(vm.names.last_index);
    let li = vm.get(r, kl)?;
    let li = vm.to_length(li)?;
    let mo = matcher.as_object().unwrap();
    vm.set_or_throw(mo, kl, Value::number(li as f64))?;
    let global = flags.contains('g');
    let full_unicode = flags.contains('u') || flags.contains('v');
    let p = vm.intrinsics.regexp_string_iterator_prototype;
    let st = super::iterators::IterState { kind: 6, target: matcher, pos: 0, extra: alloc::vec![Value::str(s), Value::bool(global), Value::bool(full_unicode)], done: false };
    Ok(Value::object(vm.alloc_object(Some(p), Class::Iter(Box::new(st)))))
}

fn string_iterator_next(vm: &mut Vm, this: Value, _: &[Value], _: Value) -> JsResult<Value> {
    let Some(it) = this.as_object() else { return Err(vm.type_error("next called on incompatible receiver")) };
    let (r, s, global, fu, done) = match &vm.obj(it).class {
        Class::Iter(st) if st.kind == 6 => (st.target.as_object().unwrap(), st.extra[0].as_string().unwrap(), st.extra[1] == Value::TRUE, st.extra[2] == Value::TRUE, st.done),
        _ => return Err(vm.type_error("next called on incompatible receiver")),
    };
    if done {
        return Ok(vm.iter_result(Value::UNDEFINED, true));
    }
    let res = regexp_exec(vm, r, s)?;
    let finish = |vm: &mut Vm| {
        if let Class::Iter(st) = &mut vm.obj_mut(it).class {
            st.done = true;
        }
    };
    if res.is_null() {
        finish(vm);
        return Ok(vm.iter_result(Value::UNDEFINED, true));
    }
    if !global {
        finish(vm);
        return Ok(vm.iter_result(res, false));
    }
    vm.root(res);
    let m = match_string(vm, res)?;
    if vm.str(m).is_empty() {
        advance_last_index(vm, r, s, fu)?;
    }
    Ok(vm.iter_result(res, false))
}

/// GetSubstitution (shared with String.prototype.replace).
pub fn get_substitution(vm: &mut Vm, matched: &[u16], s: &[u16], position: usize, captures: &[Value], named: Value, replacement: &[u16]) -> JsResult<Vec<u16>> {
    let m = captures.len();
    let mut out = Vec::new();
    let tail = (position + matched.len()).min(s.len());
    let mut i = 0;
    let r = replacement;
    while i < r.len() {
        // `$1` repeated can make the result enormous.
        if out.len() > vm.max_string_len() {
            return Err(vm.range_error("Invalid string length"));
        }
        let c = r[i];
        if c != b'$' as u16 || i + 1 >= r.len() {
            out.push(c);
            i += 1;
            continue;
        }
        let n = r[i + 1];
        match n {
            0x24 => {
                out.push(0x24);
                i += 2;
            }
            0x26 => {
                out.extend_from_slice(matched);
                i += 2;
            }
            0x60 => {
                out.extend_from_slice(&s[..position.min(s.len())]);
                i += 2;
            }
            0x27 => {
                out.extend_from_slice(&s[tail..]);
                i += 2;
            }
            0x30..=0x39 => {
                let d1 = (n - 0x30) as usize;
                let two = r.get(i + 2).filter(|&&x| (0x30..=0x39).contains(&x)).map(|&x| d1 * 10 + (x - 0x30) as usize);
                let (idx, len) = match two {
                    Some(t) if t >= 1 && t <= m => (t, 3),
                    _ => (d1, 2),
                };
                if idx >= 1 && idx <= m {
                    let v = captures[idx - 1];
                    if !v.is_undefined() {
                        let st = vm.to_string(v)?;
                        out.extend(vm.str(st).units());
                    }
                    i += len;
                } else {
                    out.push(c);
                    i += 1;
                }
            }
            0x3C => {
                if named.is_undefined() {
                    out.push(c);
                    i += 1;
                    continue;
                }
                match r[i + 2..].iter().position(|&x| x == 0x3E) {
                    None => {
                        out.push(c);
                        i += 1;
                    }
                    Some(p) => {
                        let name = &r[i + 2..i + 2 + p];
                        let ns = vm.new_string_units(name);
                        let no = vm.to_object(named)?;
                        let k = vm.to_property_key(ns)?;
                        let v = vm.get(no, k)?;
                        if !v.is_undefined() {
                            let st = vm.to_string(v)?;
                            out.extend(vm.str(st).units());
                        }
                        i += 3 + p;
                    }
                }
            }
            _ => {
                out.push(c);
                i += 1;
            }
        }
    }
    Ok(out)
}

fn symbol_replace(vm: &mut Vm, this: Value, a: &[Value], _: Value) -> JsResult<Value> {
    let r = this_obj(vm, this, "Symbol.replace")?;
    let s = vm.to_string(arg(a, 0))?;
    vm.root(Value::str(s));
    let su = vm.str(s).to_units();
    let mut replace_value = arg(a, 1);
    let functional = vm.is_callable(replace_value);
    if !functional {
        replace_value = vm.to_string_value(replace_value)?;
        vm.root(replace_value);
    }
    let flags = flags_of(vm, r)?;
    let global = flags.contains('g');
    let full_unicode = flags.contains('u') || flags.contains('v');
    if global {
        set_last_index(vm, r, Value::int(0))?;
    }
    let mut results = Vec::new();
    loop {
        let res = regexp_exec(vm, r, s)?;
        if res.is_null() {
            break;
        }
        vm.root(res);
        results.push(res);
        if !global {
            break;
        }
        let m = match_string(vm, res)?;
        if vm.str(m).is_empty() {
            advance_last_index(vm, r, s, full_unicode)?;
        }
    }
    let mut acc: Vec<u16> = Vec::new();
    let mut next_pos = 0usize;
    for res in results {
        let ro = res.as_object().unwrap();
        let ncaps = vm.length_of(ro)?;
        let ncaps = ncaps.saturating_sub(1);
        let m0 = vm.get(ro, Key::Index(0))?;
        let matched = vm.to_string(m0)?;
        let mu = vm.str(matched).to_units();
        let ki = Key::Str(vm.names.index);
        let pv = vm.get(ro, ki)?;
        let p = vm.to_integer_or_infinity(pv)?;
        let position = p.clamp(0.0, su.len() as f64) as usize;
        let mut caps = Vec::new();
        for n in 1..=ncaps {
            let k = vm.index_key(n);
            let c = vm.get(ro, k)?;
            let c = if c.is_undefined() { c } else { vm.to_string_value(c)? };
            vm.root(c);
            caps.push(c);
        }
        let kg = Key::Str(vm.names.groups);
        let named = vm.get(ro, kg)?;
        let replacement: Vec<u16> = if functional {
            let mut args = alloc::vec![Value::str(matched)];
            args.extend_from_slice(&caps);
            args.push(Value::from_usize(position));
            args.push(Value::str(s));
            if !named.is_undefined() {
                args.push(named);
            }
            let rv = vm.call(replace_value, Value::UNDEFINED, &args)?;
            let rs = vm.to_string(rv)?;
            vm.str(rs).to_units()
        } else {
            let named = if named.is_undefined() { named } else { Value::object(vm.to_object(named)?) };
            let rep = vm.str(replace_value.as_string().unwrap()).to_units();
            get_substitution(vm, &mu, &su, position, &caps, named, &rep)?
        };
        if position >= next_pos {
            acc.extend_from_slice(&su[next_pos..position]);
            acc.extend_from_slice(&replacement);
            next_pos = position + mu.len();
            if acc.len() > vm.max_string_len() {
                return Err(vm.range_error("Invalid string length"));
            }
        }
    }
    if next_pos < su.len() {
        acc.extend_from_slice(&su[next_pos..]);
    }
    Ok(vm.new_string_units(&acc))
}

fn symbol_split(vm: &mut Vm, this: Value, a: &[Value], _: Value) -> JsResult<Value> {
    let r = this_obj(vm, this, "Symbol.split")?;
    let s = vm.to_string(arg(a, 0))?;
    vm.root(Value::str(s));
    let rc = Value::object(vm.intrinsics.regexp);
    let c = super::promise::species_constructor(vm, r, rc)?;
    let flags = flags_of(vm, r)?;
    let unicode = flags.contains('u') || flags.contains('v');
    let new_flags = if flags.contains('y') { flags.clone() } else { alloc::format!("{flags}y") };
    let fv = vm.new_string(&new_flags);
    let splitter = vm.construct(c, &[Value::object(r), fv], c)?;
    vm.root(splitter);
    let sp = splitter.as_object().unwrap();
    let lim = arg(a, 1);
    let lim = if lim.is_undefined() { u32::MAX } else { vm.to_uint32(lim)? };
    let mut out: Vec<Value> = Vec::new();
    if lim == 0 {
        return Ok(Value::object(vm.new_array(out)));
    }
    let su = vm.str(s).to_units();
    let size = su.len();
    if size == 0 {
        let z = regexp_exec(vm, sp, s)?;
        if !z.is_null() {
            return Ok(Value::object(vm.new_array(out)));
        }
        return Ok(Value::object(vm.new_array(alloc::vec![Value::str(s)])));
    }
    let kl = Key::Str(vm.names.last_index);
    let mut p = 0usize;
    let mut q = p;
    while q < size {
        vm.set_or_throw(sp, kl, Value::from_usize(q))?;
        let z = regexp_exec(vm, sp, s)?;
        if z.is_null() {
            q = advance(&su, q, unicode);
            continue;
        }
        let li = vm.get(sp, kl)?;
        let e = (vm.to_length(li)? as usize).min(size);
        if e == p {
            q = advance(&su, q, unicode);
            continue;
        }
        let t = vm.new_string_units(&su[p..q]);
        vm.root(t);
        out.push(t);
        if out.len() as u32 == lim {
            return Ok(Value::object(vm.new_array(out)));
        }
        p = e;
        let zo = z.as_object().unwrap();
        let ncaps = vm.length_of(zo)?.saturating_sub(1);
        for i in 1..=ncaps {
            let k = vm.index_key(i);
            let v = vm.get(zo, k)?;
            vm.root(v);
            out.push(v);
            if out.len() as u32 == lim {
                return Ok(Value::object(vm.new_array(out)));
            }
        }
        q = p;
    }
    let t = vm.new_string_units(&su[p..size]);
    out.push(t);
    Ok(Value::object(vm.new_array(out)))
}

/// RegExp.escape(S).
fn escape(vm: &mut Vm, _: Value, a: &[Value], _: Value) -> JsResult<Value> {
    let Some(s) = arg(a, 0).as_string() else { return Err(vm.type_error("RegExp.escape requires a string")) };
    let u = vm.str(s).to_units();
    let mut out: Vec<u16> = Vec::new();
    let chars: Vec<Result<char, u16>> = char::decode_utf16(u.iter().copied()).map(|r| r.map_err(|e| e.unpaired_surrogate())).collect();
    for (i, c) in chars.iter().enumerate() {
        match c {
            Ok(ch) => {
                let cp = *ch as u32;
                if i == 0 && ch.is_ascii_alphanumeric() {
                    out.extend(alloc::format!("\\x{cp:02x}").encode_utf16());
                } else if "^$\\.*+?()[]{}|/".contains(*ch) {
                    out.push(b'\\' as u16);
                    out.push(cp as u16);
                } else if matches!(ch, '\t' | '\n' | '\u{B}' | '\u{C}' | '\r') {
                    let e = match ch {
                        '\t' => "\\t",
                        '\n' => "\\n",
                        '\u{B}' => "\\v",
                        '\u{C}' => "\\f",
                        _ => "\\r",
                    };
                    out.extend(e.encode_utf16());
                } else if ",-=<>#&!%:;@~'`\"".contains(*ch) || crate::vm::conv::is_js_space(cp as u16) && cp <= 0xFFFF || crate::unicode::is_line_terminator(*ch) {
                    if cp <= 0xFF {
                        out.extend(alloc::format!("\\x{cp:02x}").encode_utf16());
                    } else {
                        out.extend(alloc::format!("\\u{cp:04x}").encode_utf16());
                    }
                } else {
                    let mut b = [0u16; 2];
                    out.extend_from_slice(ch.encode_utf16(&mut b));
                }
            }
            Err(lone) => out.extend(alloc::format!("\\u{lone:04x}").encode_utf16()),
        }
    }
    Ok(vm.new_string_units(&out))
}
