//! Object constructor and Object.prototype; Reflect.

use alloc::vec::Vec;

use crate::object::{attr, Class, Key};
use crate::value::{ObjId, Value};
use crate::vm::props::PropDesc;
use crate::vm::{JsResult, Vm};

pub fn arg(args: &[Value], i: usize) -> Value {
    args.get(i).copied().unwrap_or(Value::UNDEFINED)
}

pub fn init(vm: &mut Vm) {
    let op = vm.intrinsics.object_prototype;
    let c = vm.constructor("Object", 1, object_ctor, op);
    vm.intrinsics.object = c;
    vm.global_prop("Object", Value::object(c));
    vm.method(op, "hasOwnProperty", 1, has_own_property);
    vm.method(op, "isPrototypeOf", 1, is_prototype_of);
    vm.method(op, "propertyIsEnumerable", 1, property_is_enumerable);
    vm.method(op, "toString", 0, object_to_string);
    vm.method(op, "toLocaleString", 0, |vm, this, _, _| {
        let k = Key::Str(vm.names.to_string);
        let f = vm.get_v(this, k)?;
        vm.call(f, this, &[])
    });
    vm.method(op, "valueOf", 0, |vm, this, _, _| Ok(Value::object(vm.to_object(this)?)));
    vm.method(op, "__defineGetter__", 2, |vm, this, a, _| define_legacy(vm, this, a, true));
    vm.method(op, "__defineSetter__", 2, |vm, this, a, _| define_legacy(vm, this, a, false));
    vm.method(op, "__lookupGetter__", 1, |vm, this, a, _| lookup_legacy(vm, this, a, true));
    vm.method(op, "__lookupSetter__", 1, |vm, this, a, _| lookup_legacy(vm, this, a, false));
    vm.getter(op, "__proto__", proto_get, Some(proto_set));

    vm.method(c, "getPrototypeOf", 1, |vm, _, a, _| {
        let o = vm.to_object(arg(a, 0))?;
        Ok(vm.get_prototype_of(o)?.map(Value::object).unwrap_or(Value::NULL))
    });
    vm.method(c, "setPrototypeOf", 2, |vm, _, a, _| {
        let (o, p) = (arg(a, 0), arg(a, 1));
        if o.is_nullish() {
            return Err(vm.type_error("Object.setPrototypeOf called on null or undefined"));
        }
        if !(p.is_object() || p.is_null()) {
            return Err(vm.type_error("Object prototype may only be an Object or null"));
        }
        if let Some(oo) = o.as_object() {
            if !vm.set_prototype_of(oo, p.as_object())? {
                return Err(vm.type_error("Cannot set prototype"));
            }
        }
        Ok(o)
    });
    vm.method(c, "create", 2, |vm, _, a, _| {
        let p = arg(a, 0);
        if !(p.is_object() || p.is_null()) {
            return Err(vm.type_error("Object prototype may only be an Object or null"));
        }
        let o = vm.alloc_object(p.as_object(), Class::Ordinary);
        let props = arg(a, 1);
        if !props.is_undefined() {
            define_properties(vm, o, props)?;
        }
        Ok(Value::object(o))
    });
    vm.method(c, "defineProperty", 3, |vm, _, a, _| {
        let Some(o) = arg(a, 0).as_object() else {
            return Err(vm.type_error("Object.defineProperty called on non-object"));
        };
        let k = vm.to_property_key(arg(a, 1))?;
        let d = to_property_descriptor(vm, arg(a, 2))?;
        vm.define_property_or_throw(o, k, d)?;
        Ok(Value::object(o))
    });
    vm.method(c, "defineProperties", 2, |vm, _, a, _| {
        let Some(o) = arg(a, 0).as_object() else {
            return Err(vm.type_error("Object.defineProperties called on non-object"));
        };
        define_properties(vm, o, arg(a, 1))?;
        Ok(Value::object(o))
    });
    vm.method(c, "getOwnPropertyDescriptor", 2, |vm, _, a, _| {
        let o = vm.to_object(arg(a, 0))?;
        let k = vm.to_property_key(arg(a, 1))?;
        match vm.get_own_property(o, k)? {
            Some(d) => from_property_descriptor(vm, &d),
            None => Ok(Value::UNDEFINED),
        }
    });
    vm.method(c, "getOwnPropertyDescriptors", 1, |vm, _, a, _| {
        let o = vm.to_object(arg(a, 0))?;
        let keys = vm.own_keys(o)?;
        let r = vm.new_object();
        vm.root(Value::object(r));
        for k in keys {
            if let Some(d) = vm.get_own_property(o, k)? {
                let dv = from_property_descriptor(vm, &d)?;
                vm.create_data_property_or_throw(r, k, dv)?;
            }
        }
        Ok(Value::object(r))
    });
    vm.method(c, "getOwnPropertyNames", 1, |vm, _, a, _| {
        let o = vm.to_object(arg(a, 0))?;
        let keys = vm.own_keys(o)?;
        let vals: Vec<Value> = keys.into_iter().filter(|k| !matches!(k, Key::Sym(_))).map(|k| vm.key_to_value(k)).collect();
        Ok(Value::object(vm.new_array(vals)))
    });
    vm.method(c, "getOwnPropertySymbols", 1, |vm, _, a, _| {
        let o = vm.to_object(arg(a, 0))?;
        let keys = vm.own_keys(o)?;
        let vals: Vec<Value> = keys.into_iter().filter_map(|k| if let Key::Sym(s) = k { Some(Value::symbol(s)) } else { None }).collect();
        Ok(Value::object(vm.new_array(vals)))
    });
    vm.method(c, "keys", 1, |vm, _, a, _| enumerable_own(vm, arg(a, 0), 0));
    vm.method(c, "values", 1, |vm, _, a, _| enumerable_own(vm, arg(a, 0), 1));
    vm.method(c, "entries", 1, |vm, _, a, _| enumerable_own(vm, arg(a, 0), 2));
    vm.method(c, "assign", 2, |vm, _, a, _| {
        let to = vm.to_object(arg(a, 0))?;
        vm.root(Value::object(to));
        for &src in a.iter().skip(1) {
            if src.is_nullish() {
                continue;
            }
            let from = vm.to_object(src)?;
            vm.root(Value::object(from));
            let keys = vm.own_keys(from)?;
            for k in keys {
                if let Some(d) = vm.get_own_property(from, k)? {
                    if d.enumerable == Some(true) {
                        let v = vm.get(from, k)?;
                        vm.set_or_throw(to, k, v)?;
                    }
                }
            }
        }
        Ok(Value::object(to))
    });
    vm.method(c, "is", 2, |vm, _, a, _| Ok(Value::bool(vm.same_value(arg(a, 0), arg(a, 1)))));
    vm.method(c, "freeze", 1, |vm, _, a, _| integrity(vm, arg(a, 0), true));
    vm.method(c, "seal", 1, |vm, _, a, _| integrity(vm, arg(a, 0), false));
    vm.method(c, "preventExtensions", 1, |vm, _, a, _| {
        if let Some(o) = arg(a, 0).as_object() {
            if !vm.prevent_extensions(o)? {
                return Err(vm.type_error("Cannot prevent extensions"));
            }
        }
        Ok(arg(a, 0))
    });
    vm.method(c, "isFrozen", 1, |vm, _, a, _| match arg(a, 0).as_object() {
        Some(o) => Ok(Value::bool(vm.test_integrity(o, true)?)),
        None => Ok(Value::TRUE),
    });
    vm.method(c, "isSealed", 1, |vm, _, a, _| match arg(a, 0).as_object() {
        Some(o) => Ok(Value::bool(vm.test_integrity(o, false)?)),
        None => Ok(Value::TRUE),
    });
    vm.method(c, "isExtensible", 1, |vm, _, a, _| match arg(a, 0).as_object() {
        Some(o) => Ok(Value::bool(vm.is_extensible(o)?)),
        None => Ok(Value::FALSE),
    });
    vm.method(c, "fromEntries", 1, |vm, _, a, _| {
        let it = arg(a, 0);
        if it.is_nullish() {
            return Err(vm.type_error("Object.fromEntries requires an iterable"));
        }
        let o = vm.new_object();
        vm.root(Value::object(o));
        let (iter, next) = vm.get_iterator(it)?;
        vm.root(iter);
        vm.root(next);
        while let Some(e) = vm.iterator_step(iter, next)? {
            let r = (|| -> JsResult<()> {
                let Some(eo) = e.as_object() else {
                    return Err(vm.type_error("Iterator value is not an entry object"));
                };
                let k = vm.get(eo, Key::Index(0))?;
                let v = vm.get(eo, Key::Index(1))?;
                let pk = vm.to_property_key(k)?;
                vm.create_data_property_or_throw(o, pk, v)
            })();
            if let Err(err) = r {
                let _ = vm.iterator_close(iter);
                return Err(err);
            }
        }
        Ok(Value::object(o))
    });
    vm.method(c, "hasOwn", 2, |vm, _, a, _| {
        let o = vm.to_object(arg(a, 0))?;
        let k = vm.to_property_key(arg(a, 1))?;
        Ok(Value::bool(vm.has_own_property(o, k)?))
    });
    vm.method(c, "groupBy", 2, |vm, _, a, _| crate::builtins::array::group_by(vm, arg(a, 0), arg(a, 1), false));
}

fn object_ctor(vm: &mut Vm, _this: Value, args: &[Value], nt: Value) -> JsResult<Value> {
    if !nt.is_undefined() && nt != Value::object(vm.intrinsics.object) {
        let p = vm.proto_from_ctor(nt, |i| i.object_prototype)?;
        return Ok(Value::object(vm.alloc_object(Some(p), Class::Ordinary)));
    }
    let v = arg(args, 0);
    if v.is_nullish() {
        return Ok(Value::object(vm.new_object()));
    }
    Ok(Value::object(vm.to_object(v)?))
}

fn has_own_property(vm: &mut Vm, this: Value, args: &[Value], _: Value) -> JsResult<Value> {
    let k = vm.to_property_key(arg(args, 0))?;
    let o = vm.to_object(this)?;
    Ok(Value::bool(vm.has_own_property(o, k)?))
}

fn is_prototype_of(vm: &mut Vm, this: Value, args: &[Value], _: Value) -> JsResult<Value> {
    let Some(mut v) = arg(args, 0).as_object() else { return Ok(Value::FALSE) };
    let o = vm.to_object(this)?;
    loop {
        match vm.get_prototype_of(v)? {
            Some(p) if p == o => return Ok(Value::TRUE),
            Some(p) => v = p,
            None => return Ok(Value::FALSE),
        }
    }
}

fn property_is_enumerable(vm: &mut Vm, this: Value, args: &[Value], _: Value) -> JsResult<Value> {
    let k = vm.to_property_key(arg(args, 0))?;
    let o = vm.to_object(this)?;
    Ok(Value::bool(vm.get_own_property(o, k)?.is_some_and(|d| d.enumerable == Some(true))))
}

/// Object.prototype.toString.
pub fn object_to_string(vm: &mut Vm, this: Value, _args: &[Value], _: Value) -> JsResult<Value> {
    if this.is_undefined() {
        return Ok(vm.new_string("[object Undefined]"));
    }
    if this.is_null() {
        return Ok(vm.new_string("[object Null]"));
    }
    let o = vm.to_object(this)?;
    let builtin = if crate::builtins::array::is_array(vm, Value::object(o))? {
        "Array"
    } else {
        match &vm.obj(o).class {
            Class::Arguments(_) => "Arguments",
            Class::Function(_) | Class::Native(_) | Class::Bound { .. } => "Function",
            Class::Proxy { callable: true, .. } => "Function",
            Class::Error => "Error",
            Class::Boolean(_) => "Boolean",
            Class::Number(_) => "Number",
            Class::String(_) => "String",
            Class::Date(_) => "Date",
            Class::RegExp(_) => "RegExp",
            _ => "Object",
        }
    };
    let kt = Key::Sym(vm.intrinsics.sym_to_string_tag);
    let tag = vm.get(o, kt)?;
    let t = match tag.as_string() {
        Some(s) => vm.str(s).to_rust_lossy(),
        None => builtin.into(),
    };
    Ok(vm.new_string(&alloc::format!("[object {t}]")))
}

fn proto_get(vm: &mut Vm, this: Value, _: &[Value], _: Value) -> JsResult<Value> {
    let o = vm.to_object(this)?;
    Ok(vm.get_prototype_of(o)?.map(Value::object).unwrap_or(Value::NULL))
}

fn proto_set(vm: &mut Vm, this: Value, args: &[Value], _: Value) -> JsResult<Value> {
    if this.is_nullish() {
        return Err(vm.type_error("Object.prototype.__proto__ called on null or undefined"));
    }
    let p = arg(args, 0);
    if !(p.is_object() || p.is_null()) {
        return Ok(Value::UNDEFINED);
    }
    if let Some(o) = this.as_object() {
        if !vm.set_prototype_of(o, p.as_object())? {
            return Err(vm.type_error("Object.prototype.__proto__ setter failed"));
        }
    }
    Ok(Value::UNDEFINED)
}

fn define_legacy(vm: &mut Vm, this: Value, a: &[Value], getter: bool) -> JsResult<Value> {
    let o = vm.to_object(this)?;
    let f = arg(a, 1);
    if !vm.is_callable(f) {
        return Err(vm.type_error("Object.prototype.__defineGetter__: Expecting function"));
    }
    let mut d = PropDesc { enumerable: Some(true), configurable: Some(true), ..Default::default() };
    if getter {
        d.get = Some(f);
    } else {
        d.set = Some(f);
    }
    let k = vm.to_property_key(arg(a, 0))?;
    vm.define_property_or_throw(o, k, d)?;
    Ok(Value::UNDEFINED)
}

fn lookup_legacy(vm: &mut Vm, this: Value, a: &[Value], getter: bool) -> JsResult<Value> {
    let mut o = vm.to_object(this)?;
    let k = vm.to_property_key(arg(a, 0))?;
    loop {
        if let Some(d) = vm.get_own_property(o, k)? {
            if d.is_accessor() {
                return Ok(if getter { d.get } else { d.set }.unwrap_or(Value::UNDEFINED));
            }
            return Ok(Value::UNDEFINED);
        }
        match vm.get_prototype_of(o)? {
            Some(p) => o = p,
            None => return Ok(Value::UNDEFINED),
        }
    }
}

fn integrity(vm: &mut Vm, v: Value, frozen: bool) -> JsResult<Value> {
    if let Some(o) = v.as_object() {
        if !vm.set_integrity(o, frozen)? {
            return Err(vm.type_error(if frozen { "Cannot freeze" } else { "Cannot seal" }));
        }
    }
    Ok(v)
}

fn enumerable_own(vm: &mut Vm, v: Value, kind: u8) -> JsResult<Value> {
    let o = vm.to_object(v)?;
    vm.root(Value::object(o));
    let keys = vm.own_keys(o)?;
    let mut out = Vec::new();
    for k in keys {
        if let Key::Sym(_) = k {
            continue;
        }
        if let Some(d) = vm.get_own_property(o, k)? {
            if d.enumerable != Some(true) {
                continue;
            }
            let kv = vm.key_to_value(k);
            match kind {
                0 => out.push(kv),
                1 => {
                    let val = vm.get(o, k)?;
                    out.push(val);
                }
                _ => {
                    let val = vm.get(o, k)?;
                    let pair = vm.new_array(alloc::vec![kv, val]);
                    out.push(Value::object(pair));
                }
            }
            if let Some(&last) = out.last() {
                vm.root(last);
            }
        }
    }
    Ok(Value::object(vm.new_array(out)))
}

fn define_properties(vm: &mut Vm, o: ObjId, props: Value) -> JsResult<()> {
    let p = vm.to_object(props)?;
    vm.root(Value::object(p));
    let keys = vm.own_keys(p)?;
    let mut descs = Vec::new();
    for k in keys {
        if let Some(d) = vm.get_own_property(p, k)? {
            if d.enumerable == Some(true) {
                let dv = vm.get(p, k)?;
                let desc = to_property_descriptor(vm, dv)?;
                for v in [desc.value, desc.get, desc.set].into_iter().flatten() {
                    vm.root(v);
                }
                descs.push((k, desc));
            }
        }
    }
    for (k, d) in descs {
        vm.define_property_or_throw(o, k, d)?;
    }
    Ok(())
}

/// ToPropertyDescriptor.
pub fn to_property_descriptor(vm: &mut Vm, v: Value) -> JsResult<PropDesc> {
    let Some(o) = v.as_object() else {
        return Err(vm.type_error("Property description must be an object"));
    };
    let mut d = PropDesc::default();
    let n = vm.names.enumerable;
    if vm.has_property(o, Key::Str(n))? {
        let x = vm.get(o, Key::Str(n))?;
        d.enumerable = Some(vm.to_boolean(x));
    }
    let n = vm.names.configurable;
    if vm.has_property(o, Key::Str(n))? {
        let x = vm.get(o, Key::Str(n))?;
        d.configurable = Some(vm.to_boolean(x));
    }
    let n = vm.names.value;
    if vm.has_property(o, Key::Str(n))? {
        d.value = Some(vm.get(o, Key::Str(n))?);
    }
    let n = vm.names.writable;
    if vm.has_property(o, Key::Str(n))? {
        let x = vm.get(o, Key::Str(n))?;
        d.writable = Some(vm.to_boolean(x));
    }
    let n = vm.names.get;
    if vm.has_property(o, Key::Str(n))? {
        let g = vm.get(o, Key::Str(n))?;
        if !g.is_undefined() && !vm.is_callable(g) {
            return Err(vm.type_error("Getter must be a function"));
        }
        d.get = Some(g);
    }
    let n = vm.names.set;
    if vm.has_property(o, Key::Str(n))? {
        let s = vm.get(o, Key::Str(n))?;
        if !s.is_undefined() && !vm.is_callable(s) {
            return Err(vm.type_error("Setter must be a function"));
        }
        d.set = Some(s);
    }
    if (d.get.is_some() || d.set.is_some()) && (d.value.is_some() || d.writable.is_some()) {
        return Err(vm.type_error("Invalid property descriptor. Cannot both specify accessors and a value or writable attribute"));
    }
    Ok(d)
}

/// FromPropertyDescriptor.
pub fn from_property_descriptor(vm: &mut Vm, d: &PropDesc) -> JsResult<Value> {
    let o = vm.new_object();
    if let Some(v) = d.value {
        let k = Key::Str(vm.names.value);
        vm.define_data(o, k, v, attr::DEFAULT);
    }
    if let Some(w) = d.writable {
        let k = Key::Str(vm.names.writable);
        vm.define_data(o, k, Value::bool(w), attr::DEFAULT);
    }
    if let Some(g) = d.get {
        let k = Key::Str(vm.names.get);
        vm.define_data(o, k, g, attr::DEFAULT);
    }
    if let Some(s) = d.set {
        let k = Key::Str(vm.names.set);
        vm.define_data(o, k, s, attr::DEFAULT);
    }
    if let Some(e) = d.enumerable {
        let k = Key::Str(vm.names.enumerable);
        vm.define_data(o, k, Value::bool(e), attr::DEFAULT);
    }
    if let Some(c) = d.configurable {
        let k = Key::Str(vm.names.configurable);
        vm.define_data(o, k, Value::bool(c), attr::DEFAULT);
    }
    Ok(Value::object(o))
}

/// Reflect.
pub fn init_reflect(vm: &mut Vm) {
    let r = vm.new_object();
    vm.intrinsics.reflect = r;
    vm.global_prop("Reflect", Value::object(r));
    let tag = vm.new_string("Reflect");
    let kt = Key::Sym(vm.intrinsics.sym_to_string_tag);
    vm.define_data(r, kt, tag, attr::CONFIGURABLE);
    fn target(vm: &mut Vm, a: &[Value]) -> JsResult<ObjId> {
        match arg(a, 0).as_object() {
            Some(o) => Ok(o),
            None => Err(vm.type_error("Reflect method called on non-object")),
        }
    }
    vm.method(r, "apply", 3, |vm, _, a, _| {
        let f = arg(a, 0);
        if !vm.is_callable(f) {
            return Err(vm.type_error("Reflect.apply target is not callable"));
        }
        let args = crate::builtins::function::create_list_from_array_like(vm, arg(a, 2))?;
        vm.call(f, arg(a, 1), &args)
    });
    vm.method(r, "construct", 2, |vm, _, a, _| {
        let f = arg(a, 0);
        if !vm.is_constructor(f) {
            return Err(vm.type_error("Reflect.construct target is not a constructor"));
        }
        let nt = if a.len() > 2 { arg(a, 2) } else { f };
        if !vm.is_constructor(nt) {
            return Err(vm.type_error("Reflect.construct newTarget is not a constructor"));
        }
        let args = crate::builtins::function::create_list_from_array_like(vm, arg(a, 1))?;
        vm.construct(f, &args, nt)
    });
    vm.method(r, "defineProperty", 3, |vm, _, a, _| {
        let o = target(vm, a)?;
        let k = vm.to_property_key(arg(a, 1))?;
        let d = to_property_descriptor(vm, arg(a, 2))?;
        Ok(Value::bool(vm.define_own_property(o, k, d)?))
    });
    vm.method(r, "deleteProperty", 2, |vm, _, a, _| {
        let o = target(vm, a)?;
        let k = vm.to_property_key(arg(a, 1))?;
        Ok(Value::bool(vm.delete(o, k)?))
    });
    vm.method(r, "get", 2, |vm, _, a, _| {
        let o = target(vm, a)?;
        let k = vm.to_property_key(arg(a, 1))?;
        let recv = if a.len() > 2 { arg(a, 2) } else { Value::object(o) };
        vm.get_with(o, k, recv)
    });
    vm.method(r, "getOwnPropertyDescriptor", 2, |vm, _, a, _| {
        let o = target(vm, a)?;
        let k = vm.to_property_key(arg(a, 1))?;
        match vm.get_own_property(o, k)? {
            Some(d) => from_property_descriptor(vm, &d),
            None => Ok(Value::UNDEFINED),
        }
    });
    vm.method(r, "getPrototypeOf", 1, |vm, _, a, _| {
        let o = target(vm, a)?;
        Ok(vm.get_prototype_of(o)?.map(Value::object).unwrap_or(Value::NULL))
    });
    vm.method(r, "has", 2, |vm, _, a, _| {
        let o = target(vm, a)?;
        let k = vm.to_property_key(arg(a, 1))?;
        Ok(Value::bool(vm.has_property(o, k)?))
    });
    vm.method(r, "isExtensible", 1, |vm, _, a, _| {
        let o = target(vm, a)?;
        Ok(Value::bool(vm.is_extensible(o)?))
    });
    vm.method(r, "ownKeys", 1, |vm, _, a, _| {
        let o = target(vm, a)?;
        let keys = vm.own_keys(o)?;
        let vals: Vec<Value> = keys.into_iter().map(|k| vm.key_to_value(k)).collect();
        Ok(Value::object(vm.new_array(vals)))
    });
    vm.method(r, "preventExtensions", 1, |vm, _, a, _| {
        let o = target(vm, a)?;
        Ok(Value::bool(vm.prevent_extensions(o)?))
    });
    vm.method(r, "set", 3, |vm, _, a, _| {
        let o = target(vm, a)?;
        let k = vm.to_property_key(arg(a, 1))?;
        let recv = if a.len() > 3 { arg(a, 3) } else { Value::object(o) };
        Ok(Value::bool(vm.set_with(o, k, arg(a, 2), recv)?))
    });
    vm.method(r, "setPrototypeOf", 2, |vm, _, a, _| {
        let o = target(vm, a)?;
        let p = arg(a, 1);
        if !(p.is_object() || p.is_null()) {
            return Err(vm.type_error("Object prototype may only be an Object or null"));
        }
        Ok(Value::bool(vm.set_prototype_of(o, p.as_object())?))
    });
}
