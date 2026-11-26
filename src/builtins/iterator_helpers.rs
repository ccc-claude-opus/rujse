//! The Iterator constructor and iterator helpers (ECMA-262 §27.1.3).

use alloc::boxed::Box;
use alloc::vec::Vec;

use super::iterators::IterState;
use super::object::arg;
use crate::object::{attr, Class, Key};
use crate::value::{ObjId, Value};
use crate::vm::{JsResult, Throw, Vm};

// IterState for helpers (kind 7): target = underlying iterator,
// extra = [next method, fn, counter/limit, helper kind, inner iterator,
// inner next, running flag].
const H_MAP: i32 = 0;
const H_FILTER: i32 = 1;
const H_TAKE: i32 = 2;
const H_DROP: i32 = 3;
const H_FLATMAP: i32 = 4;
const H_CONCAT: i32 = 5;

pub fn init(vm: &mut Vm) {
    let ip = vm.intrinsics.iterator_prototype;
    let fp = vm.intrinsics.function_prototype;
    let c = vm.constructor("Iterator", 0, |vm, _, _, nt| {
        if nt.is_undefined() || nt == Value::object(vm.intrinsics.iterator) {
            return Err(vm.type_error("Iterator is an abstract class"));
        }
        let p = vm.proto_from_ctor(nt, |i| i.iterator_prototype)?;
        Ok(Value::object(vm.alloc_object(Some(p), Class::Ordinary)))
    }, ip);
    vm.obj_mut(c).proto = Some(fp);
    vm.intrinsics.iterator = c;
    vm.global_prop("Iterator", Value::object(c));
    // Iterator.prototype.constructor and [Symbol.toStringTag] are accessors.
    let kc = Key::Str(vm.names.constructor);
    let gc = vm.native_fn("get constructor", 0, |vm, _, _, _| Ok(Value::object(vm.intrinsics.iterator)));
    let sc = vm.native_fn("set constructor", 1, |vm, this, a, _| setter_that_ignores_prototype(vm, this, Key::Str(vm.names.constructor), arg(a, 0)));
    vm.define_accessor(ip, kc, Value::object(gc), Value::object(sc), attr::CONFIGURABLE);
    let kt = Key::Sym(vm.intrinsics.sym_to_string_tag);
    let gt = vm.native_fn("get [Symbol.toStringTag]", 0, |vm, _, _, _| Ok(vm.new_string("Iterator")));
    let st = vm.native_fn("set [Symbol.toStringTag]", 1, |vm, this, a, _| {
        let k = Key::Sym(vm.intrinsics.sym_to_string_tag);
        setter_that_ignores_prototype(vm, this, k, arg(a, 0))
    });
    vm.define_accessor(ip, kt, Value::object(gt), Value::object(st), attr::CONFIGURABLE);
    vm.method(c, "from", 1, iterator_from);
    vm.method(c, "concat", 0, iterator_concat);
    let ihp = vm.alloc_object(Some(ip), Class::Ordinary);
    vm.intrinsics.iterator_helper_prototype = ihp;
    vm.method(ihp, "next", 0, helper_next);
    vm.method(ihp, "return", 0, helper_return);
    let tag = vm.new_string("Iterator Helper");
    vm.define_data(ihp, kt, tag, attr::CONFIGURABLE);
    let wp = vm.alloc_object(Some(ip), Class::Ordinary);
    vm.intrinsics.wrap_for_valid_iterator_prototype = wp;
    vm.method(wp, "next", 0, |vm, this, _, _| {
        let (it, next) = wrapped(vm, this)?;
        vm.call(next, it, &[])
    });
    vm.method(wp, "return", 0, |vm, this, _, _| {
        let (it, _) = wrapped(vm, this)?;
        let kr = Key::Str(vm.names.return_);
        match vm.get_method(it, kr)? {
            None => Ok(vm.iter_result(Value::UNDEFINED, true)),
            Some(m) => vm.call(m, it, &[]),
        }
    });
    for (name, len, kind) in [("map", 1, H_MAP), ("filter", 1, H_FILTER), ("take", 1, H_TAKE), ("drop", 1, H_DROP), ("flatMap", 1, H_FLATMAP)] {
        let f = vm.native_fn_data(name, len, make_helper, false, Box::new([Value::int(kind)]));
        let k = vm.key(name);
        vm.define_data(ip, k, Value::object(f), attr::HIDDEN);
    }
    for (name, kind) in [("reduce", 0), ("toArray", 1), ("forEach", 2), ("some", 3), ("every", 4), ("find", 5)] {
        let len = if kind == 1 { 0 } else { 1 };
        let f = vm.native_fn_data(name, len, consume, false, Box::new([Value::int(kind)]));
        let k = vm.key(name);
        vm.define_data(ip, k, Value::object(f), attr::HIDDEN);
    }
}

/// SetterThatIgnoresPrototypeProperties.
fn setter_that_ignores_prototype(vm: &mut Vm, this: Value, k: Key, v: Value) -> JsResult<Value> {
    let Some(o) = this.as_object() else { return Err(vm.type_error("setter called on non-object")) };
    if o == vm.intrinsics.iterator_prototype {
        return Err(vm.type_error("Cannot assign to this property of Iterator.prototype"));
    }
    if vm.get_own_property(o, k)?.is_none() {
        vm.create_data_property_or_throw(o, k, v)?;
    } else {
        vm.set_or_throw(o, k, v)?;
    }
    Ok(Value::UNDEFINED)
}

fn wrapped(vm: &mut Vm, this: Value) -> JsResult<(Value, Value)> {
    match this.as_object().map(|o| &vm.obj(o).class) {
        Some(Class::Iter(s)) if s.kind == 8 => Ok((s.target, s.extra[0])),
        _ => Err(vm.type_error("not a wrapped iterator")),
    }
}

/// GetIteratorDirect.
fn direct(vm: &mut Vm, this: Value) -> JsResult<(Value, Value)> {
    if !this.is_object() {
        return Err(vm.type_error("Iterator helper called on non-object"));
    }
    let kn = Key::Str(vm.names.next);
    let next = vm.get_v(this, kn)?;
    Ok((this, next))
}

fn iterator_from(vm: &mut Vm, _: Value, a: &[Value], _: Value) -> JsResult<Value> {
    let o = arg(a, 0);
    // GetIteratorFlattenable(o, iterate-strings).
    if !(o.is_object() || o.is_string()) {
        return Err(vm.type_error("Iterator.from requires an object or string"));
    }
    let ki = Key::Sym(vm.intrinsics.sym_iterator);
    let m = vm.get_method(o, ki)?;
    let it = match m {
        None => o,
        Some(m) => {
            let it = vm.call(m, o, &[])?;
            if !it.is_object() {
                return Err(vm.type_error("iterator is not an object"));
            }
            it
        }
    };
    let kn = Key::Str(vm.names.next);
    let next = vm.get_v(it, kn)?;
    // OrdinaryHasInstance(%Iterator%, it)
    let ic = Value::object(vm.intrinsics.iterator);
    if vm.ordinary_has_instance(ic, it)? {
        return Ok(it);
    }
    let p = vm.intrinsics.wrap_for_valid_iterator_prototype;
    let st = IterState { kind: 8, target: it, pos: 0, extra: alloc::vec![next], done: false };
    Ok(Value::object(vm.alloc_object(Some(p), Class::Iter(Box::new(st)))))
}

fn iterator_concat(vm: &mut Vm, _: Value, a: &[Value], _: Value) -> JsResult<Value> {
    // Validate all iterables first: (iterable, @@iterator method).
    let mut list: Vec<Value> = Vec::new();
    for &it in a {
        if !it.is_object() {
            return Err(vm.type_error("Iterator.concat requires iterable objects"));
        }
        let ki = Key::Sym(vm.intrinsics.sym_iterator);
        let m = vm.get_method(it, ki)?;
        let Some(m) = m else { return Err(vm.type_error("Iterator.concat argument is not iterable")) };
        list.push(it);
        list.push(m);
    }
    let p = vm.intrinsics.iterator_helper_prototype;
    let items = vm.new_array(list);
    let st = IterState {
        kind: 7,
        target: Value::object(items),
        pos: 0,
        extra: alloc::vec![Value::UNDEFINED, Value::UNDEFINED, Value::int(0), Value::int(H_CONCAT), Value::UNDEFINED, Value::UNDEFINED, Value::FALSE],
        done: false,
    };
    Ok(Value::object(vm.alloc_object(Some(p), Class::Iter(Box::new(st)))))
}

fn make_helper(vm: &mut Vm, this: Value, a: &[Value], _: Value) -> JsResult<Value> {
    let kind = vm.native_data(0).as_int().unwrap();
    if !this.is_object() {
        return Err(vm.type_error("Iterator helper called on non-object"));
    }
    let close = |vm: &mut Vm, e: Throw| -> Throw {
        let _ = vm.iterator_close(this);
        e
    };
    let mut counter = Value::int(0);
    let f = arg(a, 0);
    match kind {
        H_TAKE | H_DROP => {
            let n = match vm.to_number(f) {
                Ok(n) => n,
                Err(e) => return Err(close(vm, e)),
            };
            if n.is_nan() {
                let e = vm.range_error("limit must be a number");
                return Err(close(vm, e));
            }
            let n = crate::math::trunc(n);
            if n < 0.0 {
                let e = vm.range_error("limit must be non-negative");
                return Err(close(vm, e));
            }
            counter = Value::number(n);
        }
        _ => {
            if !vm.is_callable(f) {
                let e = vm.type_error("Iterator helper argument is not a function");
                return Err(close(vm, e));
            }
        }
    }
    let (it, next) = direct(vm, this)?;
    let p = vm.intrinsics.iterator_helper_prototype;
    let st = IterState { kind: 7, target: it, pos: 0, extra: alloc::vec![next, f, counter, Value::int(kind), Value::UNDEFINED, Value::UNDEFINED, Value::FALSE], done: false };
    Ok(Value::object(vm.alloc_object(Some(p), Class::Iter(Box::new(st)))))
}

fn helper_state(vm: &mut Vm, this: Value) -> JsResult<ObjId> {
    match this.as_object() {
        Some(o) if matches!(&vm.obj(o).class, Class::Iter(s) if s.kind == 7) => Ok(o),
        _ => Err(vm.type_error("Iterator helper method called on incompatible receiver")),
    }
}

fn st(vm: &Vm, h: ObjId) -> &IterState {
    match &vm.obj(h).class {
        Class::Iter(s) => s,
        _ => unreachable!(),
    }
}

fn st_mut(vm: &mut Vm, h: ObjId) -> &mut IterState {
    match &mut vm.obj_mut(h).class {
        Class::Iter(s) => s,
        _ => unreachable!(),
    }
}

fn helper_next(vm: &mut Vm, this: Value, _: &[Value], _: Value) -> JsResult<Value> {
    let h = helper_state(vm, this)?;
    if st(vm, h).extra[6] == Value::TRUE {
        return Err(vm.type_error("Generator is already running"));
    }
    if st(vm, h).done {
        return Ok(vm.iter_result(Value::UNDEFINED, true));
    }
    st_mut(vm, h).extra[6] = Value::TRUE;
    let r = helper_step(vm, h);
    st_mut(vm, h).extra[6] = Value::FALSE;
    match r {
        Ok(Some(v)) => Ok(vm.iter_result(v, false)),
        Ok(None) => {
            st_mut(vm, h).done = true;
            Ok(vm.iter_result(Value::UNDEFINED, true))
        }
        Err(e) => {
            st_mut(vm, h).done = true;
            Err(e)
        }
    }
}

fn helper_step(vm: &mut Vm, h: ObjId) -> JsResult<Option<Value>> {
    let s = st(vm, h);
    let (it, next, f, kind) = (s.target, s.extra[0], s.extra[1], s.extra[3].as_int().unwrap());
    let counter = s.pos;
    let close_with = |vm: &mut Vm, e: Throw| -> Throw {
        if let Throw::Value(_) = e {
            let _ = vm.iterator_close(it);
        }
        e
    };
    match kind {
        H_CONCAT => loop {
            let inner = st(vm, h).extra[4];
            if !inner.is_undefined() {
                let inext = st(vm, h).extra[5];
                match vm.iterator_step(inner, inext)? {
                    Some(v) => return Ok(Some(v)),
                    None => {
                        st_mut(vm, h).extra[4] = Value::UNDEFINED;
                        continue;
                    }
                }
            }
            let items = it.as_object().unwrap();
            let idx = st(vm, h).pos;
            let len = vm.obj(items).elements.len();
            if idx * 2 >= len {
                return Ok(None);
            }
            st_mut(vm, h).pos += 1;
            let (iterable, m) = (vm.obj(items).elements[idx * 2], vm.obj(items).elements[idx * 2 + 1]);
            let iter = vm.call(m, iterable, &[])?;
            if !iter.is_object() {
                return Err(vm.type_error("iterator is not an object"));
            }
            let kn = Key::Str(vm.names.next);
            let inext = vm.get_v(iter, kn)?;
            st_mut(vm, h).extra[4] = iter;
            st_mut(vm, h).extra[5] = inext;
        },
        H_FLATMAP => loop {
            let inner = st(vm, h).extra[4];
            if !inner.is_undefined() {
                let inext = st(vm, h).extra[5];
                match vm.iterator_step(inner, inext) {
                    Ok(Some(v)) => return Ok(Some(v)),
                    Ok(None) => {
                        st_mut(vm, h).extra[4] = Value::UNDEFINED;
                        continue;
                    }
                    Err(e) => return Err(close_with(vm, e)),
                }
            }
            let Some(v) = vm.iterator_step(it, next)? else { return Ok(None) };
            let c = st(vm, h).pos;
            st_mut(vm, h).pos += 1;
            let mapped = vm.call(f, Value::UNDEFINED, &[v, Value::from_usize(c)]).map_err(|e| close_with(vm, e))?;
            // GetIteratorFlattenable(mapped, reject-strings).
            let r = (|| -> JsResult<(Value, Value)> {
                if !mapped.is_object() {
                    return Err(vm.type_error("flatMap mapper must return an iterable object"));
                }
                let ki = Key::Sym(vm.intrinsics.sym_iterator);
                let m = vm.get_method(mapped, ki)?;
                let iter = match m {
                    None => mapped,
                    Some(m) => {
                        let x = vm.call(m, mapped, &[])?;
                        if !x.is_object() {
                            return Err(vm.type_error("iterator is not an object"));
                        }
                        x
                    }
                };
                let kn = Key::Str(vm.names.next);
                let inext = vm.get_v(iter, kn)?;
                Ok((iter, inext))
            })();
            let (iter, inext) = r.map_err(|e| close_with(vm, e))?;
            st_mut(vm, h).extra[4] = iter;
            st_mut(vm, h).extra[5] = inext;
        },
        H_TAKE => {
            let limit = st(vm, h).extra[2].num();
            if counter as f64 >= limit {
                vm.iterator_close(it)?;
                return Ok(None);
            }
            st_mut(vm, h).pos += 1;
            vm.iterator_step(it, next)
        }
        H_DROP => {
            let limit = st(vm, h).extra[2].num();
            while (st(vm, h).pos as f64) < limit {
                st_mut(vm, h).pos += 1;
                if vm.iterator_step(it, next)?.is_none() {
                    return Ok(None);
                }
            }
            vm.iterator_step(it, next)
        }
        _ => loop {
            let Some(v) = vm.iterator_step(it, next)? else { return Ok(None) };
            let c = st(vm, h).pos;
            st_mut(vm, h).pos += 1;
            let r = vm.call(f, Value::UNDEFINED, &[v, Value::from_usize(c)]).map_err(|e| close_with(vm, e))?;
            if kind == H_MAP {
                return Ok(Some(r));
            }
            if vm.to_boolean(r) {
                return Ok(Some(v));
            }
        },
    }
}

fn helper_return(vm: &mut Vm, this: Value, _: &[Value], _: Value) -> JsResult<Value> {
    let h = helper_state(vm, this)?;
    if st(vm, h).extra[6] == Value::TRUE {
        return Err(vm.type_error("Generator is already running"));
    }
    if !st(vm, h).done {
        st_mut(vm, h).done = true;
        let s = st(vm, h);
        let (it, kind, inner) = (s.target, s.extra[3].as_int().unwrap(), s.extra[4]);
        if kind == H_CONCAT {
            if !inner.is_undefined() {
                vm.iterator_close(inner)?;
            }
        } else {
            if kind == H_FLATMAP && !inner.is_undefined() {
                vm.iterator_close(inner)?;
            }
            vm.iterator_close(it)?;
        }
    }
    Ok(vm.iter_result(Value::UNDEFINED, true))
}

/// reduce (0), toArray (1), forEach (2), some (3), every (4), find (5).
fn consume(vm: &mut Vm, this: Value, a: &[Value], _: Value) -> JsResult<Value> {
    let kind = vm.native_data(0).as_int().unwrap();
    if !this.is_object() {
        return Err(vm.type_error("Iterator method called on non-object"));
    }
    let f = arg(a, 0);
    if kind != 1 && !vm.is_callable(f) {
        let e = vm.type_error("Iterator method argument is not a function");
        let _ = vm.iterator_close(this);
        return Err(e);
    }
    let (it, next) = direct(vm, this)?;
    let mut counter = 0usize;
    let mut acc = Value::UNDEFINED;
    let mut out: Vec<Value> = Vec::new();
    if kind == 0 {
        if a.len() < 2 {
            match vm.iterator_step(it, next)? {
                Some(v) => {
                    acc = v;
                    counter = 1;
                }
                None => return Err(vm.type_error("Reduce of empty iterator with no initial value")),
            }
        } else {
            acc = a[1];
        }
        vm.root(acc);
    }
    while let Some(v) = vm.iterator_step(it, next)? {
        vm.root(v);
        let idx = Value::from_usize(counter);
        counter += 1;
        let r = match kind {
            0 => vm.call(f, Value::UNDEFINED, &[acc, v, idx]),
            1 => {
                out.push(v);
                continue;
            }
            _ => vm.call(f, Value::UNDEFINED, &[v, idx]),
        };
        let r = match r {
            Ok(r) => r,
            Err(e) => {
                if let Throw::Value(_) = e {
                    let _ = vm.iterator_close(it);
                }
                return Err(e);
            }
        };
        match kind {
            0 => {
                acc = r;
                vm.root(acc);
            }
            3 | 5 if vm.to_boolean(r) => {
                vm.iterator_close(it)?;
                return Ok(if kind == 3 { Value::TRUE } else { v });
            }
            4 if !vm.to_boolean(r) => {
                vm.iterator_close(it)?;
                return Ok(Value::FALSE);
            }
            _ => {}
        }
    }
    Ok(match kind {
        0 => acc,
        1 => Value::object(vm.new_array(out)),
        3 => Value::FALSE,
        4 => Value::TRUE,
        _ => Value::UNDEFINED,
    })
}
