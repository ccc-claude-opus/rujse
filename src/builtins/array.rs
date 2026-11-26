//! Array constructor, Array.prototype (core) and the array iterator.

use alloc::boxed::Box;
use alloc::vec::Vec;

use super::object::arg;
use crate::object::{attr, Class, Key};
use crate::value::{ObjId, Value};
use crate::vm::props::PropDesc;
use crate::vm::{JsResult, Vm};

pub fn init(vm: &mut Vm) {
    let op = vm.intrinsics.object_prototype;
    let ap = vm.alloc_object(Some(op), Class::Array);
    vm.intrinsics.array_prototype = ap;
    let c = vm.constructor("Array", 1, array_ctor, ap);
    vm.intrinsics.array = c;
    vm.global_prop("Array", Value::object(c));
    vm.method(c, "isArray", 1, |vm, _, a, _| Ok(Value::bool(is_array(vm, arg(a, 0))?)));
    let sp = vm.intrinsics.sym_species;
    vm.getter_sym(c, sp, "[Symbol.species]", |_, this, _, _| Ok(this));
    vm.method(ap, "push", 1, push);
    vm.method(ap, "join", 1, join);
    vm.method(ap, "toString", 0, |vm, this, _, _| {
        let o = vm.to_object(this)?;
        let kj = Key::Str(vm.names.join);
        let j = vm.get(o, kj)?;
        if vm.is_callable(j) {
            return vm.call(j, Value::object(o), &[]);
        }
        super::object::object_to_string(vm, Value::object(o), &[], Value::UNDEFINED)
    });
    // Iterators.
    let ip = vm.intrinsics.iterator_prototype;
    let aip = vm.alloc_object(Some(ip), Class::Ordinary);
    vm.intrinsics.array_iterator_prototype = aip;
    vm.method(aip, "next", 0, array_iterator_next);
    let tag = vm.new_string("Array Iterator");
    let kt = Key::Sym(vm.intrinsics.sym_to_string_tag);
    vm.define_data(aip, kt, tag, attr::CONFIGURABLE);
    vm.method(ap, "keys", 0, |vm, this, _, _| array_iterator(vm, this, 0));
    vm.method(ap, "entries", 0, |vm, this, _, _| array_iterator(vm, this, 2));
    let values = vm.method(ap, "values", 0, |vm, this, _, _| array_iterator(vm, this, 1));
    vm.intrinsics.array_values = values;
    let si = vm.intrinsics.sym_iterator;
    vm.define_data(ap, Key::Sym(si), Value::object(values), attr::HIDDEN);
    crate::builtins::array_methods::init(vm, ap, c);
}

fn array_ctor(vm: &mut Vm, _: Value, a: &[Value], nt: Value) -> JsResult<Value> {
    let nt = if nt.is_undefined() { Value::object(vm.intrinsics.array) } else { nt };
    let p = vm.proto_from_ctor(nt, |i| i.array_prototype)?;
    let o = vm.new_array(Vec::new());
    vm.obj_mut(o).proto = Some(p);
    if a.len() == 1 {
        let len = a[0];
        if let Some(n) = len.as_number() {
            let l = crate::vm::conv::to_uint32_f(n);
            if l as f64 != n {
                return Err(vm.range_error("Invalid array length"));
            }
            vm.obj_mut(o).length = l;
        } else {
            vm.obj_mut(o).elements.push(len);
            vm.obj_mut(o).length = 1;
        }
    } else {
        vm.obj_mut(o).elements = a.to_vec();
        vm.obj_mut(o).length = a.len() as u32;
    }
    Ok(Value::object(o))
}

/// IsArray (sees through proxies).
pub fn is_array(vm: &mut Vm, v: Value) -> JsResult<bool> {
    let Some(o) = v.as_object() else { return Ok(false) };
    match &vm.obj(o).class {
        Class::Array => Ok(true),
        Class::Proxy { target, .. } => match target {
            Some(t) => is_array(vm, Value::object(*t)),
            None => Err(vm.type_error("Cannot perform 'IsArray' on a proxy that has been revoked")),
        },
        _ => Ok(false),
    }
}

/// The elements of an iterable, as a list.
pub fn iterable_to_list(vm: &mut Vm, v: Value) -> JsResult<Vec<Value>> {
    let (it, next) = vm.get_iterator(v)?;
    vm.root(it);
    vm.root(next);
    let mut out = Vec::new();
    while let Some(x) = vm.iterator_step(it, next)? {
        vm.root(x);
        out.push(x);
    }
    Ok(out)
}

fn push(vm: &mut Vm, this: Value, a: &[Value], _: Value) -> JsResult<Value> {
    if let Some(o) = this.as_object() {
        let ob = vm.obj_mut(o);
        if ob.is_array() && ob.elements.len() == ob.length as usize && ob.extensible && !ob.length_readonly && !ob.sparse && ob.frozen_elements == 0 && (ob.length as usize + a.len()) < u32::MAX as usize {
            let cap = ob.elements.capacity();
            ob.elements.extend_from_slice(a);
            ob.length += a.len() as u32;
            let (len, grown) = (ob.length, ob.elements.capacity() - cap);
            vm.alloc_bytes += grown * 8;
            return Ok(Value::from_u32(len));
        }
    }
    let o = vm.to_object(this)?;
    let len = vm.length_of(o)?;
    if len + a.len() as u64 > 9007199254740991 {
        return Err(vm.type_error("Pushing too many elements"));
    }
    for (i, &v) in a.iter().enumerate() {
        let k = vm.index_key(len + i as u64);
        vm.set_or_throw(o, k, v)?;
    }
    let n = Value::number((len + a.len() as u64) as f64);
    let kl = Key::Str(vm.names.length);
    vm.set_or_throw(o, kl, n)?;
    Ok(n)
}

fn join(vm: &mut Vm, this: Value, a: &[Value], _: Value) -> JsResult<Value> {
    let o = vm.to_object(this)?;
    let len = vm.length_of(o)?;
    let sep: Vec<u16> = if arg(a, 0).is_undefined() {
        alloc::vec![b',' as u16]
    } else {
        let s = vm.to_string(arg(a, 0))?;
        vm.str(s).to_units()
    };
    // Cycle detection: a join already in progress on this object → "".
    if vm.join_stack.contains(&o) {
        return Ok(Value::str(vm.names.empty));
    }
    vm.join_stack.push(o);
    let r = (|| -> JsResult<Value> {
        let mut out: Vec<u16> = Vec::new();
        for i in 0..len {
            if i > 0 {
                out.extend_from_slice(&sep);
            }
            let k = vm.index_key(i);
            let e = vm.get(o, k)?;
            if !e.is_nullish() {
                let s = vm.to_string(e)?;
                out.extend(vm.str(s).units());
            }
            if out.len() > (1 << 29) {
                return Err(vm.range_error("Invalid string length"));
            }
        }
        Ok(vm.new_string_units(&out))
    })();
    vm.join_stack.pop();
    r
}

fn array_iterator(vm: &mut Vm, this: Value, kind: u8) -> JsResult<Value> {
    let o = vm.to_object(this)?;
    Ok(make_array_iterator(vm, Value::object(o), kind))
}

pub fn make_array_iterator(vm: &mut Vm, target: Value, kind: u8) -> Value {
    let p = vm.intrinsics.array_iterator_prototype;
    let st = crate::builtins::iterators::IterState { kind, target, pos: 0, extra: Vec::new(), done: false };
    Value::object(vm.alloc_object(Some(p), Class::Iter(Box::new(st))))
}

fn array_iterator_next(vm: &mut Vm, this: Value, _: &[Value], _: Value) -> JsResult<Value> {
    let Some(it) = this.as_object() else {
        return Err(vm.type_error("next method called on incompatible receiver"));
    };
    let (kind, target, pos, done) = match &vm.obj(it).class {
        Class::Iter(s) if s.kind <= 2 => (s.kind, s.target, s.pos, s.done),
        _ => return Err(vm.type_error("next method called on incompatible receiver")),
    };
    if done {
        return Ok(vm.iter_result(Value::UNDEFINED, true));
    }
    let o = target.as_object().unwrap();
    let len = match &vm.obj(o).class {
        Class::TypedArray(_) => crate::builtins::buffer::ta_length_checked(vm, o)? as u64,
        _ => vm.length_of(o)?,
    };
    if pos as u64 >= len {
        if let Class::Iter(s) = &mut vm.obj_mut(it).class {
            s.done = true;
        }
        return Ok(vm.iter_result(Value::UNDEFINED, true));
    }
    if let Class::Iter(s) = &mut vm.obj_mut(it).class {
        s.pos += 1;
    }
    let idx = Value::number(pos as f64);
    let v = match kind {
        0 => idx,
        1 => {
            let k = vm.index_key(pos as u64);
            vm.get(o, k)?
        }
        _ => {
            let k = vm.index_key(pos as u64);
            let e = vm.get(o, k)?;
            Value::object(vm.new_array(alloc::vec![idx, e]))
        }
    };
    Ok(vm.iter_result(v, false))
}

/// Object.groupBy / Map.groupBy.
pub fn group_by(vm: &mut Vm, items: Value, cb: Value, to_map: bool) -> JsResult<Value> {
    if items.is_nullish() {
        return Err(vm.type_error("groupBy called on null or undefined"));
    }
    if !vm.is_callable(cb) {
        return Err(vm.type_error("groupBy callback is not a function"));
    }
    let (it, next) = vm.get_iterator(items)?;
    vm.root(it);
    vm.root(next);
    let mut groups: Vec<(Value, Vec<Value>)> = Vec::new();
    let mut k = 0u64;
    while let Some(v) = vm.iterator_step(it, next)? {
        vm.root(v);
        let key = match vm.call(cb, Value::UNDEFINED, &[v, Value::number(k as f64)]) {
            Ok(x) => x,
            Err(e) => {
                let _ = vm.iterator_close(it);
                return Err(e);
            }
        };
        let key = if to_map {
            if key.as_number() == Some(0.0) { Value::int(0) } else { key }
        } else {
            match vm.to_property_key(key) {
                Ok(pk) => vm.key_to_value(pk),
                Err(e) => {
                    let _ = vm.iterator_close(it);
                    return Err(e);
                }
            }
        };
        vm.root(key);
        match groups.iter_mut().find(|(g, _)| vm.same_value_zero(*g, key)) {
            Some((_, list)) => list.push(v),
            None => groups.push((key, alloc::vec![v])),
        }
        k += 1;
    }
    if to_map {
        let mp = vm.intrinsics.map_prototype;
        let m = vm.alloc_object(Some(mp), Class::Map(Box::default()));
        for (key, list) in groups {
            let arr = vm.new_array(list);
            crate::builtins::collections::map_set(vm, m, key, Value::object(arr));
        }
        return Ok(Value::object(m));
    }
    let o = vm.alloc_object(None, Class::Ordinary);
    for (key, list) in groups {
        let arr = vm.new_array(list);
        let pk = vm.to_property_key(key)?;
        vm.create_data_property_or_throw(o, pk, Value::object(arr))?;
    }
    Ok(Value::object(o))
}

/// ArraySpeciesCreate(original, length).
pub fn species_create(vm: &mut Vm, original: ObjId, len: u64) -> JsResult<ObjId> {
    if !is_array(vm, Value::object(original))? {
        return array_create(vm, len);
    }
    let kc = Key::Str(vm.names.constructor);
    let mut c = vm.get(original, kc)?;
    if let Some(co) = c.as_object() {
        if vm.is_constructor(c) && co != vm.intrinsics.array {
            // A constructor from another realm would be replaced here.
        }
        let ks = Key::Sym(vm.intrinsics.sym_species);
        c = vm.get(co, ks)?;
        if c.is_null() {
            c = Value::UNDEFINED;
        }
    }
    if c.is_undefined() {
        return array_create(vm, len);
    }
    if !vm.is_constructor(c) {
        return Err(vm.type_error("object.constructor[Symbol.species] is not a constructor"));
    }
    let r = vm.construct(c, &[Value::number(len as f64)], c)?;
    match r.as_object() {
        Some(o) => Ok(o),
        None => Err(vm.type_error("species constructor did not return an object")),
    }
}

/// ArrayCreate(length).
pub fn array_create(vm: &mut Vm, len: u64) -> JsResult<ObjId> {
    if len > u32::MAX as u64 {
        return Err(vm.range_error("Invalid array length"));
    }
    let o = vm.new_array(Vec::new());
    vm.obj_mut(o).length = len as u32;
    Ok(o)
}

pub fn _unused(_: PropDesc) {}
