//! Proxy exotic objects (ECMA-262 §10.5) and the Proxy constructor.

use alloc::boxed::Box;
use alloc::vec::Vec;

use super::object::{arg, from_property_descriptor, to_property_descriptor};
use crate::object::{Class, Key};
use crate::value::{ObjId, Value};
use crate::vm::props::{is_compatible, PropDesc};
use crate::vm::{JsResult, Vm};

pub fn init(vm: &mut Vm) {
    let c = vm.native_fn_data("Proxy", 2, proxy_ctor, true, Box::new([]));
    vm.intrinsics.proxy = c;
    vm.global_prop("Proxy", Value::object(c));
    vm.method(c, "revocable", 2, |vm, _, a, _| {
        let p = proxy_create(vm, arg(a, 0), arg(a, 1))?;
        let revoke = vm.native_fn_data("", 0, |vm, _, _, _| {
            let p = vm.native_data(0);
            if let Some(po) = p.as_object() {
                if let Class::Proxy { target, handler, .. } = &mut vm.obj_mut(po).class {
                    *target = None;
                    *handler = None;
                }
                let me = vm.current_native.as_object().unwrap();
                vm.set_native_data(me, 0, Value::NULL);
            }
            Ok(Value::UNDEFINED)
        }, false, Box::new([p]));
        let o = vm.new_object();
        let kp = vm.key("proxy");
        vm.create_data_property_or_throw(o, kp, p)?;
        let kr = vm.key("revoke");
        vm.create_data_property_or_throw(o, kr, Value::object(revoke))?;
        Ok(Value::object(o))
    });
}

fn proxy_ctor(vm: &mut Vm, _: Value, a: &[Value], nt: Value) -> JsResult<Value> {
    if nt.is_undefined() {
        return Err(vm.type_error("Constructor Proxy requires 'new'"));
    }
    proxy_create(vm, arg(a, 0), arg(a, 1))
}

fn proxy_create(vm: &mut Vm, target: Value, handler: Value) -> JsResult<Value> {
    let (Some(t), Some(h)) = (target.as_object(), handler.as_object()) else {
        return Err(vm.type_error("Cannot create proxy with a non-object as target or handler"));
    };
    let callable = vm.is_callable(target);
    let ctor = vm.is_constructor(target);
    let o = vm.alloc_object(None, Class::Proxy { target: Some(t), handler: Some(h), callable, ctor });
    Ok(Value::object(o))
}

/// (target, handler) or TypeError when revoked.
fn parts(vm: &mut Vm, o: ObjId) -> JsResult<(ObjId, ObjId)> {
    match vm.obj(o).class {
        Class::Proxy { target: Some(t), handler: Some(h), .. } => Ok((t, h)),
        _ => Err(vm.type_error("Cannot perform operation on a revoked proxy")),
    }
}

fn trap(vm: &mut Vm, h: ObjId, name: &str) -> JsResult<Option<Value>> {
    let k = vm.key(name);
    vm.get_method(Value::object(h), k)
}

pub fn get_prototype_of(vm: &mut Vm, o: ObjId) -> JsResult<Option<ObjId>> {
    let (t, h) = parts(vm, o)?;
    let Some(f) = trap(vm, h, "getPrototypeOf")? else { return vm.get_prototype_of(t) };
    let p = vm.call(f, Value::object(h), &[Value::object(t)])?;
    if !(p.is_object() || p.is_null()) {
        return Err(vm.type_error("'getPrototypeOf' on proxy: trap returned neither object nor null"));
    }
    if vm.is_extensible(t)? {
        return Ok(p.as_object());
    }
    let tp = vm.get_prototype_of(t)?;
    if p.as_object() != tp {
        return Err(vm.type_error("'getPrototypeOf' on proxy: proxy target is non-extensible but the trap did not return its actual prototype"));
    }
    Ok(tp)
}

pub fn set_prototype_of(vm: &mut Vm, o: ObjId, p: Option<ObjId>) -> JsResult<bool> {
    let (t, h) = parts(vm, o)?;
    let Some(f) = trap(vm, h, "setPrototypeOf")? else { return vm.set_prototype_of(t, p) };
    let pv = p.map(Value::object).unwrap_or(Value::NULL);
    let r = vm.call(f, Value::object(h), &[Value::object(t), pv])?;
    if !vm.to_boolean(r) {
        return Ok(false);
    }
    if vm.is_extensible(t)? {
        return Ok(true);
    }
    if vm.get_prototype_of(t)? != p {
        return Err(vm.type_error("'setPrototypeOf' on proxy: trap returned truish for setting a new prototype on the non-extensible proxy target"));
    }
    Ok(true)
}

pub fn is_extensible(vm: &mut Vm, o: ObjId) -> JsResult<bool> {
    let (t, h) = parts(vm, o)?;
    let Some(f) = trap(vm, h, "isExtensible")? else { return vm.is_extensible(t) };
    let r = vm.call(f, Value::object(h), &[Value::object(t)])?;
    let b = vm.to_boolean(r);
    if b != vm.is_extensible(t)? {
        return Err(vm.type_error("'isExtensible' on proxy: trap result does not reflect extensibility of proxy target"));
    }
    Ok(b)
}

pub fn prevent_extensions(vm: &mut Vm, o: ObjId) -> JsResult<bool> {
    let (t, h) = parts(vm, o)?;
    let Some(f) = trap(vm, h, "preventExtensions")? else { return vm.prevent_extensions(t) };
    let r = vm.call(f, Value::object(h), &[Value::object(t)])?;
    let b = vm.to_boolean(r);
    if b && vm.is_extensible(t)? {
        return Err(vm.type_error("'preventExtensions' on proxy: trap returned truish but the proxy target is extensible"));
    }
    Ok(b)
}

pub fn get_own_property(vm: &mut Vm, o: ObjId, k: Key) -> JsResult<Option<PropDesc>> {
    let (t, h) = parts(vm, o)?;
    let Some(f) = trap(vm, h, "getOwnPropertyDescriptor")? else { return vm.get_own_property(t, k) };
    let kv = vm.key_to_value(k);
    let r = vm.call(f, Value::object(h), &[Value::object(t), kv])?;
    if !(r.is_object() || r.is_undefined()) {
        return Err(vm.type_error("'getOwnPropertyDescriptor' on proxy: trap returned neither object nor undefined"));
    }
    let target_desc = vm.get_own_property(t, k)?;
    if r.is_undefined() {
        let Some(td) = target_desc else { return Ok(None) };
        if td.configurable == Some(false) {
            return Err(vm.type_error("'getOwnPropertyDescriptor' on proxy: trap returned undefined for a non-configurable property"));
        }
        if !vm.is_extensible(t)? {
            return Err(vm.type_error("'getOwnPropertyDescriptor' on proxy: trap returned undefined for a property of a non-extensible target"));
        }
        return Ok(None);
    }
    let ext = vm.is_extensible(t)?;
    let mut d = to_property_descriptor(vm, r)?;
    // CompletePropertyDescriptor.
    if !d.is_accessor() {
        d.value.get_or_insert(Value::UNDEFINED);
        d.writable.get_or_insert(false);
    } else {
        d.get.get_or_insert(Value::UNDEFINED);
        d.set.get_or_insert(Value::UNDEFINED);
    }
    d.enumerable.get_or_insert(false);
    d.configurable.get_or_insert(false);
    if !is_compatible(vm, ext, &d, target_desc.as_ref()) {
        return Err(vm.type_error("'getOwnPropertyDescriptor' on proxy: trap returned a descriptor incompatible with the target property"));
    }
    if d.configurable == Some(false) {
        match &target_desc {
            None => return Err(vm.type_error("'getOwnPropertyDescriptor' on proxy: trap reported non-configurability for a property that does not exist on the target")),
            Some(td) if td.configurable == Some(true) => {
                return Err(vm.type_error("'getOwnPropertyDescriptor' on proxy: trap reported non-configurability for a configurable target property"));
            }
            Some(td) => {
                if d.writable == Some(false) && td.writable == Some(true) {
                    return Err(vm.type_error("'getOwnPropertyDescriptor' on proxy: trap reported non-writable for a writable target property"));
                }
            }
        }
    }
    Ok(Some(d))
}

pub fn define_own_property(vm: &mut Vm, o: ObjId, k: Key, d: PropDesc) -> JsResult<bool> {
    let (t, h) = parts(vm, o)?;
    let Some(f) = trap(vm, h, "defineProperty")? else { return vm.define_own_property(t, k, d) };
    let dv = from_property_descriptor(vm, &d)?;
    let kv = vm.key_to_value(k);
    let r = vm.call(f, Value::object(h), &[Value::object(t), kv, dv])?;
    if !vm.to_boolean(r) {
        return Ok(false);
    }
    let target_desc = vm.get_own_property(t, k)?;
    let ext = vm.is_extensible(t)?;
    let setting_non_config = d.configurable == Some(false);
    match target_desc {
        None => {
            if !ext {
                return Err(vm.type_error("'defineProperty' on proxy: trap returned truish for adding a property to a non-extensible target"));
            }
            if setting_non_config {
                return Err(vm.type_error("'defineProperty' on proxy: trap returned truish for defining a non-configurable property that is not on the target"));
            }
        }
        Some(td) => {
            if !is_compatible(vm, ext, &d, Some(&td)) {
                return Err(vm.type_error("'defineProperty' on proxy: trap returned truish for a descriptor incompatible with the target property"));
            }
            if setting_non_config && td.configurable == Some(true) {
                return Err(vm.type_error("'defineProperty' on proxy: trap returned truish for defining a non-configurable property that is configurable on the target"));
            }
            if td.is_data() && td.configurable == Some(false) && td.writable == Some(true) && d.writable == Some(false) {
                return Err(vm.type_error("'defineProperty' on proxy: trap returned truish for making a non-configurable writable property non-writable"));
            }
        }
    }
    Ok(true)
}

pub fn has(vm: &mut Vm, o: ObjId, k: Key) -> JsResult<bool> {
    let (t, h) = parts(vm, o)?;
    let Some(f) = trap(vm, h, "has")? else { return vm.has_property(t, k) };
    let kv = vm.key_to_value(k);
    let r = vm.call(f, Value::object(h), &[Value::object(t), kv])?;
    let b = vm.to_boolean(r);
    if !b {
        if let Some(td) = vm.get_own_property(t, k)? {
            if td.configurable == Some(false) {
                return Err(vm.type_error("'has' on proxy: trap returned falsish for a non-configurable property"));
            }
            if !vm.is_extensible(t)? {
                return Err(vm.type_error("'has' on proxy: trap returned falsish for a property of a non-extensible target"));
            }
        }
    }
    Ok(b)
}

pub fn get(vm: &mut Vm, o: ObjId, k: Key, receiver: Value) -> JsResult<Value> {
    let (t, h) = parts(vm, o)?;
    let Some(f) = trap(vm, h, "get")? else { return vm.get_with(t, k, receiver) };
    let kv = vm.key_to_value(k);
    let v = vm.call(f, Value::object(h), &[Value::object(t), kv, receiver])?;
    if let Some(td) = vm.get_own_property(t, k)? {
        if td.configurable == Some(false) {
            if td.is_data() && td.writable == Some(false) && !vm.same_value(v, td.value.unwrap_or(Value::UNDEFINED)) {
                return Err(vm.type_error("'get' on proxy: the trap result differs from a non-writable, non-configurable target property"));
            }
            if td.is_accessor() && td.get.unwrap_or(Value::UNDEFINED).is_undefined() && !v.is_undefined() {
                return Err(vm.type_error("'get' on proxy: a non-configurable accessor without getter must report undefined"));
            }
        }
    }
    Ok(v)
}

pub fn set(vm: &mut Vm, o: ObjId, k: Key, v: Value, receiver: Value) -> JsResult<bool> {
    let (t, h) = parts(vm, o)?;
    let Some(f) = trap(vm, h, "set")? else { return vm.set_with(t, k, v, receiver) };
    let kv = vm.key_to_value(k);
    let r = vm.call(f, Value::object(h), &[Value::object(t), kv, v, receiver])?;
    if !vm.to_boolean(r) {
        return Ok(false);
    }
    if let Some(td) = vm.get_own_property(t, k)? {
        if td.configurable == Some(false) {
            if td.is_data() && td.writable == Some(false) && !vm.same_value(v, td.value.unwrap_or(Value::UNDEFINED)) {
                return Err(vm.type_error("'set' on proxy: trap returned truish for a non-writable, non-configurable property with a different value"));
            }
            if td.is_accessor() && td.set.unwrap_or(Value::UNDEFINED).is_undefined() {
                return Err(vm.type_error("'set' on proxy: trap returned truish for an accessor without setter"));
            }
        }
    }
    Ok(true)
}

pub fn delete(vm: &mut Vm, o: ObjId, k: Key) -> JsResult<bool> {
    let (t, h) = parts(vm, o)?;
    let Some(f) = trap(vm, h, "deleteProperty")? else { return vm.delete(t, k) };
    let kv = vm.key_to_value(k);
    let r = vm.call(f, Value::object(h), &[Value::object(t), kv])?;
    if !vm.to_boolean(r) {
        return Ok(false);
    }
    if let Some(td) = vm.get_own_property(t, k)? {
        if td.configurable == Some(false) {
            return Err(vm.type_error("'deleteProperty' on proxy: trap returned truish for a non-configurable property"));
        }
        if !vm.is_extensible(t)? {
            return Err(vm.type_error("'deleteProperty' on proxy: trap returned truish for a property of a non-extensible target"));
        }
    }
    Ok(true)
}

pub fn own_keys(vm: &mut Vm, o: ObjId) -> JsResult<Vec<Key>> {
    let (t, h) = parts(vm, o)?;
    let Some(f) = trap(vm, h, "ownKeys")? else { return vm.own_keys(t) };
    let r = vm.call(f, Value::object(h), &[Value::object(t)])?;
    let Some(ro) = r.as_object() else {
        return Err(vm.type_error("'ownKeys' on proxy: trap returned a non-object"));
    };
    vm.root(r);
    let len = vm.length_of(ro)?;
    let mut keys: Vec<Key> = Vec::new();
    for i in 0..len {
        let k = vm.index_key(i);
        let v = vm.get(ro, k)?;
        if !(v.is_string() || v.is_symbol()) {
            return Err(vm.type_error("'ownKeys' on proxy: trap result contains a non-property-key"));
        }
        let pk = vm.to_property_key(v)?;
        if keys.contains(&pk) {
            return Err(vm.type_error("'ownKeys' on proxy: trap returned duplicate entries"));
        }
        keys.push(pk);
    }
    let ext = vm.is_extensible(t)?;
    let target_keys = vm.own_keys(t)?;
    let mut configurable = Vec::new();
    let mut nonconfig = Vec::new();
    for k in target_keys {
        match vm.get_own_property(t, k)? {
            Some(d) if d.configurable == Some(false) => nonconfig.push(k),
            _ => configurable.push(k),
        }
    }
    if ext && nonconfig.is_empty() {
        return Ok(keys);
    }
    let mut unchecked = keys.clone();
    for k in nonconfig {
        match unchecked.iter().position(|x| *x == k) {
            Some(i) => {
                unchecked.remove(i);
            }
            None => return Err(vm.type_error("'ownKeys' on proxy: trap result did not include a non-configurable key")),
        }
    }
    if ext {
        return Ok(keys);
    }
    for k in configurable {
        match unchecked.iter().position(|x| *x == k) {
            Some(i) => {
                unchecked.remove(i);
            }
            None => return Err(vm.type_error("'ownKeys' on proxy: trap result did not include all keys of a non-extensible target")),
        }
    }
    if !unchecked.is_empty() {
        return Err(vm.type_error("'ownKeys' on proxy: trap returned extra keys for a non-extensible target"));
    }
    Ok(keys)
}

pub fn call(vm: &mut Vm, o: ObjId, this: Value, a: &[Value]) -> JsResult<Value> {
    let (t, h) = parts(vm, o)?;
    let Some(f) = trap(vm, h, "apply")? else { return vm.call(Value::object(t), this, a) };
    let arr = vm.new_array(a.to_vec());
    vm.call(f, Value::object(h), &[Value::object(t), this, Value::object(arr)])
}

pub fn construct(vm: &mut Vm, o: ObjId, a: &[Value], nt: Value) -> JsResult<Value> {
    let (t, h) = parts(vm, o)?;
    let Some(f) = trap(vm, h, "construct")? else { return vm.construct(Value::object(t), a, nt) };
    let arr = vm.new_array(a.to_vec());
    let r = vm.call(f, Value::object(h), &[Value::object(t), Value::object(arr), nt])?;
    if !r.is_object() {
        return Err(vm.type_error("'construct' on proxy: trap returned non-object"));
    }
    Ok(r)
}
