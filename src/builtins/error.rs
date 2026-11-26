//! Error and its subclasses, Symbol, Boolean, and the global functions.

use alloc::string::String;
use alloc::vec::Vec;

use super::object::arg;
use crate::object::{attr, Class, Key};
use crate::value::{ObjId, Value};
use crate::vm::{JsResult, Vm};

fn install_cause(vm: &mut Vm, o: ObjId, opts: Value) -> JsResult<()> {
    if let Some(oo) = opts.as_object() {
        let kc = Key::Str(vm.names.cause);
        if vm.has_property(oo, kc)? {
            let c = vm.get(oo, kc)?;
            vm.define_data(o, kc, c, attr::HIDDEN);
        }
    }
    Ok(())
}

fn make(vm: &mut Vm, nt: Value, fallback: fn(&crate::vm::Intrinsics) -> ObjId, msg: Value, opts: Value) -> JsResult<ObjId> {
    let nt = if nt.is_undefined() { Value::object(vm.current_native.as_object().unwrap()) } else { nt };
    let p = vm.proto_from_ctor(nt, fallback)?;
    let o = vm.alloc_object(Some(p), Class::Error);
    vm.root(Value::object(o));
    if !msg.is_undefined() {
        let m = vm.to_string_value(msg)?;
        let k = Key::Str(vm.names.message);
        vm.define_data(o, k, m, attr::HIDDEN);
    }
    install_cause(vm, o, opts)?;
    vm.attach_stack(o);
    Ok(o)
}

pub fn init(vm: &mut Vm) {
    let op = vm.intrinsics.object_prototype;
    let ep = vm.alloc_object(Some(op), Class::Ordinary);
    vm.intrinsics.error_prototype = ep;
    let ec = vm.constructor("Error", 1, |vm, _, a, nt| Ok(Value::object(make(vm, nt, |i| i.error_prototype, arg(a, 0), arg(a, 1))?)), ep);
    vm.intrinsics.error = ec;
    vm.global_prop("Error", Value::object(ec));
    let n = vm.new_string("Error");
    vm.prop(ep, "name", n, attr::HIDDEN);
    let e = Value::str(vm.names.empty);
    vm.prop(ep, "message", e, attr::HIDDEN);
    vm.method(ep, "toString", 0, error_to_string);
    vm.method(ec, "captureStackTrace", 1, |vm, _, a, _| {
        if let Some(o) = arg(a, 0).as_object() {
            vm.attach_stack(o);
        }
        Ok(Value::UNDEFINED)
    });
    macro_rules! sub {
        ($name:expr, $field:ident, $ctor:ident) => {{
            let p = vm.alloc_object(Some(ep), Class::Ordinary);
            vm.intrinsics.$field = p;
            let c = vm.constructor($name, 1, |vm, _, a, nt| Ok(Value::object(make(vm, nt, |i| i.$field, arg(a, 0), arg(a, 1))?)), p);
            vm.obj_mut(c).proto = Some(ec);
            let n = vm.new_string($name);
            vm.prop(p, "name", n, attr::HIDDEN);
            let e = Value::str(vm.names.empty);
            vm.prop(p, "message", e, attr::HIDDEN);
            vm.global_prop($name, Value::object(c));
            c
        }};
    }
    vm.intrinsics.type_error = sub!("TypeError", type_error_prototype, TypeError);
    vm.intrinsics.range_error = sub!("RangeError", range_error_prototype, RangeError);
    vm.intrinsics.syntax_error = sub!("SyntaxError", syntax_error_prototype, SyntaxError);
    vm.intrinsics.reference_error = sub!("ReferenceError", reference_error_prototype, ReferenceError);
    sub!("EvalError", eval_error_prototype, EvalError);
    sub!("URIError", uri_error_prototype, URIError);
    // AggregateError(errors, message, options)
    let ap = vm.alloc_object(Some(ep), Class::Ordinary);
    vm.intrinsics.aggregate_error_prototype = ap;
    let ac = vm.constructor("AggregateError", 2, aggregate_error, ap);
    vm.obj_mut(ac).proto = Some(ec);
    let n = vm.new_string("AggregateError");
    vm.prop(ap, "name", n, attr::HIDDEN);
    vm.prop(ap, "message", e, attr::HIDDEN);
    vm.global_prop("AggregateError", Value::object(ac));
}

fn aggregate_error(vm: &mut Vm, _this: Value, a: &[Value], nt: Value) -> JsResult<Value> {
    let o = make(vm, nt, |i| i.aggregate_error_prototype, arg(a, 1), arg(a, 2))?;
    let errors = super::array::iterable_to_list(vm, arg(a, 0))?;
    let arr = vm.new_array(errors);
    let k = Key::Str(vm.names.errors);
    vm.define_data(o, k, Value::object(arr), attr::HIDDEN);
    Ok(Value::object(o))
}

fn error_to_string(vm: &mut Vm, this: Value, _: &[Value], _: Value) -> JsResult<Value> {
    let Some(o) = this.as_object() else {
        return Err(vm.type_error("Error.prototype.toString called on non-object"));
    };
    let kn = Key::Str(vm.names.name);
    let n = vm.get(o, kn)?;
    let name = if n.is_undefined() { String::from("Error") } else { let s = vm.to_string(n)?; vm.str(s).to_rust_lossy() };
    let km = Key::Str(vm.names.message);
    let m = vm.get(o, km)?;
    let msg = if m.is_undefined() { String::new() } else { let s = vm.to_string(m)?; vm.str(s).to_rust_lossy() };
    let r = if name.is_empty() {
        msg
    } else if msg.is_empty() {
        name
    } else {
        alloc::format!("{name}: {msg}")
    };
    Ok(vm.new_string(&r))
}

// ----- Symbol ------------------------------------------------------------

pub fn init_symbol(vm: &mut Vm) {
    let op = vm.intrinsics.object_prototype;
    let sp = vm.alloc_object(Some(op), Class::Ordinary);
    vm.intrinsics.symbol_prototype = sp;
    let c = vm.constructor("Symbol", 0, symbol_ctor, sp);
    vm.global_prop("Symbol", Value::object(c));
    let i = &vm.intrinsics;
    let wk = [
        ("asyncIterator", i.sym_async_iterator),
        ("hasInstance", i.sym_has_instance),
        ("isConcatSpreadable", i.sym_is_concat_spreadable),
        ("iterator", i.sym_iterator),
        ("match", i.sym_match),
        ("matchAll", i.sym_match_all),
        ("replace", i.sym_replace),
        ("search", i.sym_search),
        ("species", i.sym_species),
        ("split", i.sym_split),
        ("toPrimitive", i.sym_to_primitive),
        ("toStringTag", i.sym_to_string_tag),
        ("unscopables", i.sym_unscopables),
    ];
    for (n, s) in wk {
        vm.prop(c, n, Value::symbol(s), attr::NONE);
    }
    vm.method(c, "for", 1, |vm, _, a, _| {
        let s = vm.to_string(arg(a, 0))?;
        let s = vm.intern_id(s);
        if let Some(&sym) = vm.symbol_registry.get(&s) {
            return Ok(Value::symbol(sym));
        }
        let sym = vm.new_symbol(Some(s));
        vm.symbols.get_mut(sym.0).registered = true;
        vm.symbol_registry.insert(s, sym);
        Ok(Value::symbol(sym))
    });
    vm.method(c, "keyFor", 1, |vm, _, a, _| {
        let Some(s) = arg(a, 0).as_symbol() else {
            return Err(vm.type_error("Symbol.keyFor: argument is not a symbol"));
        };
        let sym = vm.symbols.get(s.0);
        Ok(if sym.registered { Value::str(sym.description.unwrap()) } else { Value::UNDEFINED })
    });
    vm.method(sp, "toString", 0, |vm, this, _, _| {
        let s = this_symbol(vm, this)?;
        let d = vm.display(Value::symbol(s));
        Ok(vm.new_string(&d))
    });
    vm.method(sp, "valueOf", 0, |vm, this, _, _| Ok(Value::symbol(this_symbol(vm, this)?)));
    vm.getter(sp, "description", |vm, this, _, _| {
        let s = this_symbol(vm, this)?;
        Ok(vm.symbols.get(s.0).description.map(Value::str).unwrap_or(Value::UNDEFINED))
    }, None);
    let tp = vm.intrinsics.sym_to_primitive;
    let f = vm.native_fn("[Symbol.toPrimitive]", 1, |vm, this, _, _| Ok(Value::symbol(this_symbol(vm, this)?)));
    vm.define_data(sp, Key::Sym(tp), Value::object(f), attr::CONFIGURABLE);
    let tag = vm.new_string("Symbol");
    let kt = Key::Sym(vm.intrinsics.sym_to_string_tag);
    vm.define_data(sp, kt, tag, attr::CONFIGURABLE);
}

fn symbol_ctor(vm: &mut Vm, _: Value, a: &[Value], nt: Value) -> JsResult<Value> {
    if !nt.is_undefined() {
        return Err(vm.type_error("Symbol is not a constructor"));
    }
    let d = arg(a, 0);
    let desc = if d.is_undefined() { None } else { Some(vm.to_string(d)?) };
    Ok(Value::symbol(vm.new_symbol(desc)))
}

fn this_symbol(vm: &mut Vm, v: Value) -> JsResult<crate::value::SymId> {
    if let Some(s) = v.as_symbol() {
        return Ok(s);
    }
    if let Some(o) = v.as_object() {
        if let Class::Symbol(s) = vm.obj(o).class {
            return Ok(s);
        }
    }
    Err(vm.type_error("not a symbol"))
}

// ----- Boolean -----------------------------------------------------------

pub fn init_boolean(vm: &mut Vm) {
    let op = vm.intrinsics.object_prototype;
    let bp = vm.alloc_object(Some(op), Class::Boolean(false));
    vm.intrinsics.boolean_prototype = bp;
    let c = vm.constructor("Boolean", 1, |vm, _, a, nt| {
        let b = vm.to_boolean(arg(a, 0));
        if nt.is_undefined() {
            return Ok(Value::bool(b));
        }
        let p = vm.proto_from_ctor(nt, |i| i.boolean_prototype)?;
        Ok(Value::object(vm.alloc_object(Some(p), Class::Boolean(b))))
    }, bp);
    vm.global_prop("Boolean", Value::object(c));
    fn this_bool(vm: &mut Vm, v: Value) -> JsResult<bool> {
        if v.is_bool() {
            return Ok(v == Value::TRUE);
        }
        if let Some(o) = v.as_object() {
            if let Class::Boolean(b) = vm.obj(o).class {
                return Ok(b);
            }
        }
        Err(vm.type_error("Boolean.prototype method called on incompatible receiver"))
    }
    vm.method(bp, "toString", 0, |vm, this, _, _| {
        let b = this_bool(vm, this)?;
        Ok(Value::str(if b { vm.names.true_ } else { vm.names.false_ }))
    });
    vm.method(bp, "valueOf", 0, |vm, this, _, _| Ok(Value::bool(this_bool(vm, this)?)));
}

// ----- global functions --------------------------------------------------

pub fn init_global(vm: &mut Vm) {
    let g = vm.global;
    vm.prop(g, "globalThis", Value::object(g), attr::HIDDEN);
    vm.prop(g, "NaN", Value::NAN, attr::NONE);
    vm.prop(g, "Infinity", Value::number(f64::INFINITY), attr::NONE);
    vm.prop(g, "undefined", Value::UNDEFINED, attr::NONE);
    let ev = vm.method(g, "eval", 1, |vm, _, a, _| {
        let x = arg(a, 0);
        let Some(s) = x.as_string() else { return Ok(x) };
        let src = vm.str(s).to_rust_lossy();
        vm.global_eval(&src)
    });
    vm.intrinsics.eval = ev;
    vm.method(g, "isNaN", 1, |vm, _, a, _| Ok(Value::bool(vm.to_number(arg(a, 0))?.is_nan())));
    vm.method(g, "isFinite", 1, |vm, _, a, _| Ok(Value::bool(vm.to_number(arg(a, 0))?.is_finite())));
    let pf = vm.method(g, "parseFloat", 1, parse_float);
    let pi = vm.method(g, "parseInt", 2, parse_int);
    vm.root(Value::object(pf));
    vm.root(Value::object(pi));
    vm.native_roots.clear();
    vm.method(g, "encodeURIComponent", 1, |vm, _, a, _| uri_encode(vm, arg(a, 0), ";/?:@&=+$,#"));
    vm.method(g, "encodeURI", 1, |vm, _, a, _| uri_encode(vm, arg(a, 0), ""));
    vm.method(g, "decodeURIComponent", 1, |vm, _, a, _| uri_decode(vm, arg(a, 0), ""));
    vm.method(g, "decodeURI", 1, |vm, _, a, _| uri_decode(vm, arg(a, 0), ";/?:@&=+$,#"));
}

fn trim_start_units(u: &[u16]) -> &[u16] {
    let mut i = 0;
    while i < u.len() && crate::vm::conv::is_js_space(u[i]) {
        i += 1;
    }
    &u[i..]
}

fn parse_float(vm: &mut Vm, _: Value, a: &[Value], _: Value) -> JsResult<Value> {
    let s = vm.to_string(arg(a, 0))?;
    let units = vm.str(s).to_units();
    let t = trim_start_units(&units);
    let st: String = t.iter().take_while(|&&c| c < 0x80).map(|&c| c as u8 as char).collect();
    // Longest prefix that is a StrDecimalLiteral.
    let b = st.as_bytes();
    let mut i = 0;
    if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
        i += 1;
    }
    if st[i..].starts_with("Infinity") {
        let neg = b.first() == Some(&b'-');
        return Ok(Value::number(if neg { f64::NEG_INFINITY } else { f64::INFINITY }));
    }
    let ds = i;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
    }
    let mut digits = i > ds;
    if i < b.len() && b[i] == b'.' {
        let j = i + 1;
        let mut k = j;
        while k < b.len() && b[k].is_ascii_digit() {
            k += 1;
        }
        if k > j || digits {
            digits |= k > j;
            i = k;
        }
    }
    if !digits {
        return Ok(Value::NAN);
    }
    if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
        let mut k = i + 1;
        if k < b.len() && (b[k] == b'+' || b[k] == b'-') {
            k += 1;
        }
        let es = k;
        while k < b.len() && b[k].is_ascii_digit() {
            k += 1;
        }
        if k > es {
            i = k;
        }
    }
    let mut lit = String::from(&st[..i]);
    if lit.ends_with('.') {
        lit.push('0');
    }
    if lit.starts_with('.') || lit.starts_with("-.") || lit.starts_with("+.") {
        lit = lit.replacen('.', "0.", 1);
    }
    Ok(Value::number(lit.parse::<f64>().unwrap_or(f64::NAN)))
}

fn parse_int(vm: &mut Vm, _: Value, a: &[Value], _: Value) -> JsResult<Value> {
    let s = vm.to_string(arg(a, 0))?;
    let units = vm.str(s).to_units();
    let mut t = trim_start_units(&units);
    let mut neg = false;
    if let Some(&c) = t.first() {
        if c == b'-' as u16 || c == b'+' as u16 {
            neg = c == b'-' as u16;
            t = &t[1..];
        }
    }
    let mut r = vm.to_int32(arg(a, 1))?;
    let mut strip = true;
    if r != 0 {
        if !(2..=36).contains(&r) {
            return Ok(Value::NAN);
        }
        if r != 16 {
            strip = false;
        }
    } else {
        r = 10;
    }
    if strip && t.len() >= 2 && t[0] == b'0' as u16 && (t[1] == b'x' as u16 || t[1] == b'X' as u16) {
        t = &t[2..];
        r = 16;
    }
    let digits: Vec<u32> = t.iter().map_while(|&c| char::from_u32(c as u32).and_then(|ch| ch.to_digit(r as u32))).collect();
    if digits.is_empty() {
        return Ok(Value::NAN);
    }
    let mut v = 0f64;
    if r == 10 && digits.len() > 15 {
        let st: String = digits.iter().map(|d| char::from_digit(*d, 10).unwrap()).collect();
        v = st.parse().unwrap();
    } else {
        for d in &digits {
            v = v * r as f64 + *d as f64;
        }
    }
    Ok(Value::number(if neg { -v } else { v }))
}

fn uri_encode(vm: &mut Vm, v: Value, extra_unescaped: &str) -> JsResult<Value> {
    let s = vm.to_string(v)?;
    let units = vm.str(s).to_units();
    let mut out = String::new();
    let mut i = 0;
    while i < units.len() {
        let c = units[i];
        let ch = c as u32;
        let unreserved = ch < 0x80 && {
            let b = ch as u8 as char;
            b.is_ascii_alphanumeric() || "-_.!~*'()".contains(b) || extra_unescaped.contains(b)
        };
        if unreserved {
            out.push(ch as u8 as char);
            i += 1;
            continue;
        }
        let cp = if (0xD800..0xDC00).contains(&c) {
            if i + 1 < units.len() && (0xDC00..0xE000).contains(&units[i + 1]) {
                let cp = 0x10000 + ((c as u32 - 0xD800) << 10) + (units[i + 1] as u32 - 0xDC00);
                i += 1;
                cp
            } else {
                return Err(vm.uri_error());
            }
        } else if (0xDC00..0xE000).contains(&c) {
            return Err(vm.uri_error());
        } else {
            ch
        };
        i += 1;
        let mut b = [0u8; 4];
        for byte in char::from_u32(cp).unwrap().encode_utf8(&mut b).bytes() {
            out.push_str(&alloc::format!("%{byte:02X}"));
        }
    }
    Ok(vm.new_string(&out))
}

fn uri_decode(vm: &mut Vm, v: Value, reserved: &str) -> JsResult<Value> {
    let s = vm.to_string(v)?;
    let units = vm.str(s).to_units();
    let mut out: Vec<u16> = Vec::new();
    let hex = |u: u16| char::from_u32(u as u32).and_then(|c| c.to_digit(16));
    let mut i = 0;
    while i < units.len() {
        if units[i] != b'%' as u16 {
            out.push(units[i]);
            i += 1;
            continue;
        }
        let start = i;
        let byte = |i: usize| -> Option<u8> {
            if i + 2 >= units.len() {
                return None;
            }
            Some((hex(units[i + 1])? * 16 + hex(units[i + 2])?) as u8)
        };
        let Some(b0) = byte(i) else { return Err(vm.uri_error()) };
        i += 3;
        if b0 < 0x80 {
            let c = b0 as char;
            if reserved.contains(c) {
                out.extend_from_slice(&units[start..i]);
            } else {
                out.push(b0 as u16);
            }
            continue;
        }
        let n = if b0 & 0xE0 == 0xC0 { 2 } else if b0 & 0xF0 == 0xE0 { 3 } else if b0 & 0xF8 == 0xF0 { 4 } else { return Err(vm.uri_error()) };
        let mut bytes = alloc::vec![b0];
        for _ in 1..n {
            if i >= units.len() || units[i] != b'%' as u16 {
                return Err(vm.uri_error());
            }
            let Some(b) = byte(i) else { return Err(vm.uri_error()) };
            if b & 0xC0 != 0x80 {
                return Err(vm.uri_error());
            }
            bytes.push(b);
            i += 3;
        }
        match core::str::from_utf8(&bytes) {
            Ok(st) => out.extend(st.encode_utf16()),
            Err(_) => return Err(vm.uri_error()),
        }
    }
    Ok(vm.new_string_units(&out))
}

impl Vm {
    pub fn uri_error(&mut self) -> crate::vm::Throw {
        let p = self.intrinsics.uri_error_prototype;
        crate::vm::Throw::Value(self.make_error(p, "URI malformed"))
    }
}
