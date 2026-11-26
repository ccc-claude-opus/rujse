//! JSON.parse and JSON.stringify (ECMA-262 §25.5).

use alloc::string::String;
use alloc::vec::Vec;

use super::object::arg;
use crate::object::{attr, Class, Key};
use crate::value::{Kind, ObjId, Value};
use crate::vm::{JsResult, Vm};

pub fn init(vm: &mut Vm) {
    let o = vm.new_object();
    vm.intrinsics.json = o;
    vm.global_prop("JSON", Value::object(o));
    let tag = vm.new_string("JSON");
    let kt = Key::Sym(vm.intrinsics.sym_to_string_tag);
    vm.define_data(o, kt, tag, attr::CONFIGURABLE);
    vm.method(o, "parse", 2, parse);
    vm.method(o, "stringify", 3, stringify);
    vm.method(o, "rawJSON", 1, |vm, _, a, _| {
        let s = vm.to_string(arg(a, 0))?;
        let text = vm.str(s).to_units();
        let bad = |c: u16| matches!(c, 0x09 | 0x0A | 0x0D | 0x20);
        if text.is_empty() || bad(text[0]) || bad(*text.last().unwrap()) {
            return Err(vm.syntax_error("Invalid value for JSON.rawJSON"));
        }
        let mut p = JsonParser { s: &text, i: 0 };
        let v = p.value(vm).and_then(|v| {
            p.ws();
            if p.i != text.len() { Err(()) } else { Ok(v) }
        });
        match v {
            Ok(v) if !v.is_object() => {}
            _ => return Err(vm.syntax_error("Invalid value for JSON.rawJSON")),
        }
        let r = vm.alloc_object(None, Class::Ordinary);
        let k = vm.key("rawJSON");
        vm.define_data(r, k, Value::str(s), attr::ENUMERABLE);
        vm.obj_mut(r).extensible = false;
        vm.raw_json.push(r);
        Ok(Value::object(r))
    });
    vm.method(o, "isRawJSON", 1, |vm, _, a, _| Ok(Value::bool(arg(a, 0).as_object().is_some_and(|o| vm.raw_json.contains(&o)))));
}

struct JsonParser<'a> {
    s: &'a [u16],
    i: usize,
}

impl JsonParser<'_> {
    fn ws(&mut self) {
        while self.i < self.s.len() && matches!(self.s[self.i], 0x09 | 0x0A | 0x0D | 0x20) {
            self.i += 1;
        }
    }

    fn eat(&mut self, c: u8) -> bool {
        self.ws();
        if self.i < self.s.len() && self.s[self.i] == c as u16 {
            self.i += 1;
            true
        } else {
            false
        }
    }

    fn string(&mut self) -> Result<Vec<u16>, ()> {
        // At the opening quote.
        self.i += 1;
        let mut out = Vec::new();
        loop {
            let c = *self.s.get(self.i).ok_or(())?;
            self.i += 1;
            match c {
                0x22 => return Ok(out),
                0x5C => {
                    let e = *self.s.get(self.i).ok_or(())?;
                    self.i += 1;
                    out.push(match e {
                        0x22 => 0x22,
                        0x5C => 0x5C,
                        0x2F => 0x2F,
                        0x62 => 8,
                        0x66 => 12,
                        0x6E => 10,
                        0x72 => 13,
                        0x74 => 9,
                        0x75 => {
                            let mut v = 0u16;
                            for _ in 0..4 {
                                let h = *self.s.get(self.i).ok_or(())?;
                                self.i += 1;
                                let d = char::from_u32(h as u32).and_then(|c| c.to_digit(16)).ok_or(())?;
                                v = v * 16 + d as u16;
                            }
                            v
                        }
                        _ => return Err(()),
                    });
                }
                c if c < 0x20 => return Err(()),
                c => out.push(c),
            }
        }
    }

    fn value(&mut self, vm: &mut Vm) -> Result<Value, ()> {
        self.ws();
        let c = *self.s.get(self.i).ok_or(())?;
        match c {
            0x7B => {
                self.i += 1;
                let o = vm.new_object();
                vm.root(Value::object(o));
                if self.eat(b'}') {
                    return Ok(Value::object(o));
                }
                loop {
                    self.ws();
                    if self.s.get(self.i) != Some(&0x22) {
                        return Err(());
                    }
                    let k = self.string()?;
                    if !self.eat(b':') {
                        return Err(());
                    }
                    let v = self.value(vm)?;
                    let ks = vm.new_string_units(&k);
                    let key = vm.to_property_key(ks).map_err(|_| ())?;
                    vm.create_data_property(o, key, v).map_err(|_| ())?;
                    if self.eat(b',') {
                        continue;
                    }
                    if self.eat(b'}') {
                        return Ok(Value::object(o));
                    }
                    return Err(());
                }
            }
            0x5B => {
                self.i += 1;
                let a = vm.new_array(Vec::new());
                vm.root(Value::object(a));
                if self.eat(b']') {
                    return Ok(Value::object(a));
                }
                loop {
                    let v = self.value(vm)?;
                    let ob = vm.obj_mut(a);
                    ob.elements.push(v);
                    ob.length += 1;
                    if self.eat(b',') {
                        continue;
                    }
                    if self.eat(b']') {
                        return Ok(Value::object(a));
                    }
                    return Err(());
                }
            }
            0x22 => {
                let s = self.string()?;
                Ok(vm.new_string_units(&s))
            }
            b if b == b't' as u16 => self.lit("true", Value::TRUE),
            b if b == b'f' as u16 => self.lit("false", Value::FALSE),
            b if b == b'n' as u16 => self.lit("null", Value::NULL),
            _ => self.number(),
        }
    }

    fn lit(&mut self, w: &str, v: Value) -> Result<Value, ()> {
        for b in w.bytes() {
            if self.s.get(self.i) != Some(&(b as u16)) {
                return Err(());
            }
            self.i += 1;
        }
        Ok(v)
    }

    fn number(&mut self) -> Result<Value, ()> {
        let start = self.i;
        let digit = |p: &Self| p.s.get(p.i).is_some_and(|&c| (0x30..=0x39).contains(&c));
        if self.s.get(self.i) == Some(&(b'-' as u16)) {
            self.i += 1;
        }
        if self.s.get(self.i) == Some(&(b'0' as u16)) {
            self.i += 1;
        } else if digit(self) {
            while digit(self) {
                self.i += 1;
            }
        } else {
            return Err(());
        }
        if self.s.get(self.i) == Some(&(b'.' as u16)) {
            self.i += 1;
            if !digit(self) {
                return Err(());
            }
            while digit(self) {
                self.i += 1;
            }
        }
        if matches!(self.s.get(self.i), Some(&0x65) | Some(&0x45)) {
            self.i += 1;
            if matches!(self.s.get(self.i), Some(&0x2B) | Some(&0x2D)) {
                self.i += 1;
            }
            if !digit(self) {
                return Err(());
            }
            while digit(self) {
                self.i += 1;
            }
        }
        let t: String = self.s[start..self.i].iter().map(|&c| c as u8 as char).collect();
        Ok(Value::number(t.parse::<f64>().map_err(|_| ())?))
    }
}

fn parse(vm: &mut Vm, _: Value, a: &[Value], _: Value) -> JsResult<Value> {
    let s = vm.to_string(arg(a, 0))?;
    let text = vm.str(s).to_units();
    let mut p = JsonParser { s: &text, i: 0 };
    let r = p.value(vm).and_then(|v| {
        p.ws();
        if p.i != text.len() { Err(()) } else { Ok(v) }
    });
    let v = match r {
        Ok(v) => v,
        Err(()) => return Err(vm.syntax_error(&alloc::format!("Unexpected token in JSON at position {}", p.i))),
    };
    let reviver = arg(a, 1);
    if vm.is_callable(reviver) {
        let root = vm.new_object();
        vm.root(Value::object(root));
        let k = Key::Str(vm.names.empty);
        vm.create_data_property_or_throw(root, k, v)?;
        return internalize(vm, root, k, reviver);
    }
    Ok(v)
}

fn internalize(vm: &mut Vm, holder: ObjId, name: Key, reviver: Value) -> JsResult<Value> {
    let val = vm.get(holder, name)?;
    if let Some(o) = val.as_object() {
        vm.root(val);
        if super::array::is_array(vm, val)? {
            let len = vm.length_of(o)?;
            for i in 0..len {
                let k = vm.index_key(i);
                let nv = internalize(vm, o, k, reviver)?;
                if nv.is_undefined() {
                    vm.delete(o, k)?;
                } else {
                    vm.create_data_property(o, k, nv)?;
                }
            }
        } else {
            let keys = vm.own_keys(o)?;
            for k in keys {
                if let Key::Sym(_) = k {
                    continue;
                }
                if !vm.get_own_property(o, k)?.is_some_and(|d| d.enumerable == Some(true)) {
                    continue;
                }
                let nv = internalize(vm, o, k, reviver)?;
                if nv.is_undefined() {
                    vm.delete(o, k)?;
                } else {
                    vm.create_data_property(o, k, nv)?;
                }
            }
        }
    }
    let kv = vm.key_to_value(name);
    vm.call(reviver, Value::object(holder), &[kv, val])
}

struct Stringify {
    replacer: Value,
    allow: Option<Vec<Key>>,
    gap: Vec<u16>,
    indent: Vec<u16>,
    stack: Vec<ObjId>,
    out: Vec<u16>,
}

fn stringify(vm: &mut Vm, _: Value, a: &[Value], _: Value) -> JsResult<Value> {
    let mut st = Stringify { replacer: Value::UNDEFINED, allow: None, gap: Vec::new(), indent: Vec::new(), stack: Vec::new(), out: Vec::new() };
    let r = arg(a, 1);
    if let Some(ro) = r.as_object() {
        if vm.is_callable(r) {
            st.replacer = r;
        } else if super::array::is_array(vm, r)? {
            let len = vm.length_of(ro)?;
            let mut list: Vec<Key> = Vec::new();
            for i in 0..len {
                let k = vm.index_key(i);
                let v = vm.get(ro, k)?;
                let item = match v.kind() {
                    Kind::Str(_) | Kind::Int(_) | Kind::Double(_) => Some(vm.to_property_key(v)?),
                    Kind::Object(o) => match vm.obj(o).class {
                        Class::String(_) | Class::Number(_) => {
                            let s = vm.to_string(v)?;
                            Some(vm.key_from_str(s))
                        }
                        _ => None,
                    },
                    _ => None,
                };
                if let Some(k) = item {
                    if !list.contains(&k) {
                        list.push(k);
                    }
                }
            }
            st.allow = Some(list);
        }
    }
    let mut space = arg(a, 2);
    if let Some(so) = space.as_object() {
        match vm.obj(so).class {
            Class::Number(_) => space = Value::number(vm.to_number(space)?),
            Class::String(_) => space = vm.to_string_value(space)?,
            _ => {}
        }
    }
    if let Some(n) = space.as_number() {
        let n = crate::math::trunc(n).clamp(0.0, 10.0) as usize;
        st.gap = alloc::vec![b' ' as u16; n];
    } else if let Some(s) = space.as_string() {
        st.gap = vm.str(s).units().take(10).collect();
    }
    let wrapper = vm.new_object();
    vm.root(Value::object(wrapper));
    let k = Key::Str(vm.names.empty);
    vm.create_data_property_or_throw(wrapper, k, arg(a, 0))?;
    if serialize_property(vm, &mut st, k, wrapper)? {
        Ok(vm.new_string_units(&st.out))
    } else {
        Ok(Value::UNDEFINED)
    }
}

/// SerializeJSONProperty: appends to `st.out`; false for undefined.
fn serialize_property(vm: &mut Vm, st: &mut Stringify, key: Key, holder: ObjId) -> JsResult<bool> {
    let mut v = vm.get(holder, key)?;
    if v.is_object() || v.is_bigint() {
        let kt = vm.key("toJSON");
        let f = vm.get_v(v, kt)?;
        if vm.is_callable(f) {
            let kv = vm.key_to_value(key);
            v = vm.call(f, v, &[kv])?;
        }
    }
    if !st.replacer.is_undefined() {
        let kv = vm.key_to_value(key);
        v = vm.call(st.replacer, Value::object(holder), &[kv, v])?;
    }
    if let Some(o) = v.as_object() {
        if vm.raw_json.contains(&o) {
            let k = vm.key("rawJSON");
            let r = vm.get(o, k)?;
            let s = vm.to_string(r)?;
            st.out.extend(vm.str(s).units());
            return Ok(true);
        }
        match vm.obj(o).class {
            Class::Number(_) => v = Value::number(vm.to_number(v)?),
            Class::String(_) => v = vm.to_string_value(v)?,
            Class::Boolean(b) => v = Value::bool(b),
            Class::BigInt(b) => v = Value::bigint(b),
            _ => {}
        }
    }
    match v.kind() {
        Kind::Null => st.out.extend("null".encode_utf16()),
        Kind::Bool(b) => st.out.extend(if b { "true" } else { "false" }.encode_utf16()),
        Kind::Str(s) => {
            let u = vm.str(s).to_units();
            quote(&mut st.out, &u);
        }
        Kind::Int(_) | Kind::Double(_) => {
            let n = v.num();
            if n.is_finite() {
                st.out.extend(crate::vm::conv::number_to_string(n).encode_utf16());
            } else {
                st.out.extend("null".encode_utf16());
            }
        }
        Kind::BigInt(_) => return Err(vm.type_error("Do not know how to serialize a BigInt")),
        Kind::Object(o) if !vm.is_callable(v) => {
            if super::array::is_array(vm, v)? {
                serialize_array(vm, st, o)?;
            } else {
                serialize_object(vm, st, o)?;
            }
        }
        _ => return Ok(false),
    }
    Ok(true)
}

fn quote(out: &mut Vec<u16>, s: &[u16]) {
    out.push(b'"' as u16);
    let mut i = 0;
    while i < s.len() {
        let c = s[i];
        match c {
            0x08 => out.extend("\\b".encode_utf16()),
            0x09 => out.extend("\\t".encode_utf16()),
            0x0A => out.extend("\\n".encode_utf16()),
            0x0C => out.extend("\\f".encode_utf16()),
            0x0D => out.extend("\\r".encode_utf16()),
            0x22 => out.extend("\\\"".encode_utf16()),
            0x5C => out.extend("\\\\".encode_utf16()),
            c if c < 0x20 => out.extend(alloc::format!("\\u{c:04x}").encode_utf16()),
            0xD800..=0xDBFF if i + 1 < s.len() && (0xDC00..=0xDFFF).contains(&s[i + 1]) => {
                out.push(c);
                out.push(s[i + 1]);
                i += 1;
            }
            0xD800..=0xDFFF => out.extend(alloc::format!("\\u{c:04x}").encode_utf16()),
            c => out.push(c),
        }
        i += 1;
    }
    out.push(b'"' as u16);
}

fn enter(vm: &mut Vm, st: &mut Stringify, o: ObjId) -> JsResult<Vec<u16>> {
    if st.stack.contains(&o) {
        return Err(vm.type_error("Converting circular structure to JSON"));
    }
    st.stack.push(o);
    vm.root(Value::object(o));
    let stepback = st.indent.clone();
    st.indent.extend_from_slice(&st.gap.clone());
    Ok(stepback)
}

fn serialize_object(vm: &mut Vm, st: &mut Stringify, o: ObjId) -> JsResult<()> {
    let stepback = enter(vm, st, o)?;
    let keys: Vec<Key> = match &st.allow {
        Some(k) => k.clone(),
        None => {
            let ks = vm.own_keys(o)?;
            let mut out = Vec::new();
            for k in ks {
                if let Key::Sym(_) = k {
                    continue;
                }
                if vm.get_own_property(o, k)?.is_some_and(|d| d.enumerable == Some(true)) {
                    out.push(k);
                }
            }
            out
        }
    };
    st.out.push(b'{' as u16);
    let mut first = true;
    for k in keys {
        let mark = st.out.len();
        if !first {
            st.out.push(b',' as u16);
        }
        if !st.gap.is_empty() {
            st.out.push(b'\n' as u16);
            st.out.extend_from_slice(&st.indent.clone());
        }
        let kv = vm.key_to_value(k);
        let ks = vm.to_string(kv)?;
        let ku = vm.str(ks).to_units();
        quote(&mut st.out, &ku);
        st.out.push(b':' as u16);
        if !st.gap.is_empty() {
            st.out.push(b' ' as u16);
        }
        if serialize_property(vm, st, k, o)? {
            first = false;
        } else {
            st.out.truncate(mark);
        }
    }
    if !first && !st.gap.is_empty() {
        st.out.push(b'\n' as u16);
        st.out.extend_from_slice(&stepback);
    }
    st.out.push(b'}' as u16);
    st.stack.pop();
    st.indent = stepback;
    Ok(())
}

fn serialize_array(vm: &mut Vm, st: &mut Stringify, o: ObjId) -> JsResult<()> {
    let stepback = enter(vm, st, o)?;
    let len = vm.length_of(o)?;
    st.out.push(b'[' as u16);
    for i in 0..len {
        if i > 0 {
            st.out.push(b',' as u16);
        }
        if !st.gap.is_empty() {
            st.out.push(b'\n' as u16);
            st.out.extend_from_slice(&st.indent.clone());
        }
        let k = vm.index_key(i);
        if !serialize_property(vm, st, k, o)? {
            st.out.extend("null".encode_utf16());
        }
    }
    if len > 0 && !st.gap.is_empty() {
        st.out.push(b'\n' as u16);
        st.out.extend_from_slice(&stepback);
    }
    st.out.push(b']' as u16);
    st.stack.pop();
    st.indent = stepback;
    Ok(())
}
