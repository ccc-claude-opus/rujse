//! The rest of Array.prototype and Array.from/of.

use alloc::vec::Vec;
use core::cmp::Ordering;

use super::object::arg;
use crate::builtins::array::{array_create, is_array, species_create};
use crate::object::{Class, Key, attr};
use crate::value::{ObjId, Value};
use crate::vm::{JsResult, Vm};

const MAX_LENGTH: u64 = 9_007_199_254_740_991;

pub fn init(vm: &mut Vm, proto: ObjId, ctor: ObjId) {
    vm.method(ctor, "from", 1, array_from);
    vm.method(ctor, "of", 0, array_of);

    vm.method(proto, "at", 1, at);
    vm.method(proto, "concat", 1, concat);
    vm.method(proto, "copyWithin", 2, copy_within);
    vm.method(proto, "every", 1, every);
    vm.method(proto, "fill", 1, fill);
    vm.method(proto, "filter", 1, filter);
    vm.method(proto, "find", 1, find);
    vm.method(proto, "findIndex", 1, find_index);
    vm.method(proto, "findLast", 1, find_last);
    vm.method(proto, "findLastIndex", 1, find_last_index);
    vm.method(proto, "flat", 0, flat);
    vm.method(proto, "flatMap", 1, flat_map);
    vm.method(proto, "forEach", 1, for_each);
    vm.method(proto, "includes", 1, includes);
    vm.method(proto, "indexOf", 1, index_of);
    vm.method(proto, "lastIndexOf", 1, last_index_of);
    vm.method(proto, "map", 1, map);
    vm.method(proto, "pop", 0, pop);
    vm.method(proto, "reduce", 1, reduce);
    vm.method(proto, "reduceRight", 1, reduce_right);
    vm.method(proto, "reverse", 0, reverse);
    vm.method(proto, "shift", 0, shift);
    vm.method(proto, "slice", 2, slice);
    vm.method(proto, "some", 1, some);
    vm.method(proto, "sort", 1, sort);
    vm.method(proto, "splice", 2, splice);
    vm.method(proto, "toLocaleString", 0, to_locale_string);
    vm.method(proto, "toReversed", 0, to_reversed);
    vm.method(proto, "toSorted", 1, to_sorted);
    vm.method(proto, "toSpliced", 2, to_spliced);
    vm.method(proto, "unshift", 1, unshift);
    vm.method(proto, "with", 2, with);

    // Array.prototype [ %Symbol.unscopables% ] (ECMA-262 2026).
    let unscopables = vm.alloc_object(None, Class::Ordinary);
    for name in [
        "at",
        "copyWithin",
        "entries",
        "fill",
        "find",
        "findIndex",
        "findLast",
        "findLastIndex",
        "flat",
        "flatMap",
        "includes",
        "keys",
        "toReversed",
        "toSorted",
        "toSpliced",
        "values",
    ] {
        let key = vm.key(name);
        vm.define_data(unscopables, key, Value::TRUE, attr::DEFAULT);
    }
    let key = Key::Sym(vm.intrinsics.sym_unscopables);
    vm.define_data(proto, key, Value::object(unscopables), attr::CONFIGURABLE);
}

fn root_if_heap(vm: &mut Vm, value: Value) {
    if value.is_heap() {
        vm.root(value);
    }
}

fn rooted_object(vm: &mut Vm, this: Value) -> JsResult<ObjId> {
    let o = vm.to_object(this)?;
    vm.root(Value::object(o));
    Ok(o)
}

fn set_length(vm: &mut Vm, o: ObjId, len: u64) -> JsResult<()> {
    let key = Key::Str(vm.names.length);
    vm.set_or_throw(o, key, Value::number(len as f64))
}

fn array_from(vm: &mut Vm, this: Value, args: &[Value], _: Value) -> JsResult<Value> {
    let items = arg(args, 0);
    let mapper = arg(args, 1);
    let this_arg = arg(args, 2);
    let mapping = !mapper.is_undefined();
    if mapping && !vm.is_callable(mapper) {
        return Err(vm.type_error("Array.from mapper is not callable"));
    }

    let iterator_key = Key::Sym(vm.intrinsics.sym_iterator);
    let using_iterator = vm.get_method(items, iterator_key)?;
    if let Some(method) = using_iterator {
        root_if_heap(vm, method);

        let result = if vm.is_constructor(this) {
            vm.construct(this, &[], this)?
        } else {
            Value::object(array_create(vm, 0)?)
        };
        let Some(a) = result.as_object() else {
            return Err(vm.type_error("Array.from constructor did not return an object"));
        };
        vm.root(Value::object(a));

        let (iterator, next) = iterator_from_method(vm, items, method)?;
        let mut k = 0u64;
        loop {
            if k >= MAX_LENGTH {
                let error = vm.type_error("Array.from iterator produced too many elements");
                let _ = vm.iterator_close(iterator);
                return Err(error);
            }

            let Some(next_value) = vm.iterator_step(iterator, next)? else {
                set_length(vm, a, k)?;
                return Ok(Value::object(a));
            };
            root_if_heap(vm, next_value);

            let mapped = if mapping {
                match vm.call(mapper, this_arg, &[next_value, Value::number(k as f64)]) {
                    Ok(v) => v,
                    Err(error) => {
                        let _ = vm.iterator_close(iterator);
                        return Err(error);
                    }
                }
            } else {
                next_value
            };
            root_if_heap(vm, mapped);

            let key = vm.index_key(k);
            if let Err(error) = vm.create_data_property_or_throw(a, key, mapped) {
                let _ = vm.iterator_close(iterator);
                return Err(error);
            }
            k += 1;
        }
    }

    let array_like = rooted_object(vm, items)?;
    let len = vm.length_of(array_like)?;
    let result = if vm.is_constructor(this) {
        vm.construct(this, &[Value::number(len as f64)], this)?
    } else {
        Value::object(array_create(vm, len)?)
    };
    let Some(a) = result.as_object() else {
        return Err(vm.type_error("Array.from constructor did not return an object"));
    };
    vm.root(Value::object(a));

    let mut k = 0u64;
    while k < len {
        let source_key = vm.index_key(k);
        let value = vm.get(array_like, source_key)?;
        root_if_heap(vm, value);
        let mapped = if mapping {
            let v = vm.call(mapper, this_arg, &[value, Value::number(k as f64)])?;
            root_if_heap(vm, v);
            v
        } else {
            value
        };
        let target_key = vm.index_key(k);
        vm.create_data_property_or_throw(a, target_key, mapped)?;
        k += 1;
    }
    set_length(vm, a, len)?;
    Ok(Value::object(a))
}

fn iterator_from_method(vm: &mut Vm, items: Value, method: Value) -> JsResult<(Value, Value)> {
    let iterator = vm.call(method, items, &[])?;
    if !iterator.is_object() {
        return Err(vm.type_error("Result of the Symbol.iterator method is not an object"));
    }
    vm.root(iterator);
    let key = Key::Str(vm.names.next);
    let next = vm.get_v(iterator, key)?;
    root_if_heap(vm, next);
    Ok((iterator, next))
}

fn array_of(vm: &mut Vm, this: Value, args: &[Value], _: Value) -> JsResult<Value> {
    let len = args.len() as u64;
    let result = if vm.is_constructor(this) {
        vm.construct(this, &[Value::number(len as f64)], this)?
    } else {
        Value::object(array_create(vm, len)?)
    };
    let Some(a) = result.as_object() else {
        return Err(vm.type_error("Array.of constructor did not return an object"));
    };
    vm.root(Value::object(a));

    for (k, &value) in args.iter().enumerate() {
        root_if_heap(vm, value);
        let key = vm.index_key(k as u64);
        vm.create_data_property_or_throw(a, key, value)?;
    }
    set_length(vm, a, len)?;
    Ok(Value::object(a))
}

fn at(vm: &mut Vm, this: Value, args: &[Value], _: Value) -> JsResult<Value> {
    let o = rooted_object(vm, this)?;
    let len = vm.length_of(o)?;
    let relative = vm.to_integer_or_infinity(arg(args, 0))?;
    let k = if relative >= 0.0 {
        relative
    } else {
        len as f64 + relative
    };
    if k < 0.0 || k >= len as f64 {
        return Ok(Value::UNDEFINED);
    }
    let key = vm.index_key(k as u64);
    vm.get(o, key)
}

fn is_concat_spreadable(vm: &mut Vm, value: Value) -> JsResult<bool> {
    let Some(o) = value.as_object() else {
        return Ok(false);
    };
    let key = Key::Sym(vm.intrinsics.sym_is_concat_spreadable);
    let spreadable = vm.get(o, key)?;
    if !spreadable.is_undefined() {
        return Ok(vm.to_boolean(spreadable));
    }
    is_array(vm, value)
}

fn concat(vm: &mut Vm, this: Value, args: &[Value], _: Value) -> JsResult<Value> {
    let o = rooted_object(vm, this)?;
    let a = species_create(vm, o, 0)?;
    vm.root(Value::object(a));

    let mut n = 0u64;
    let mut items = Vec::with_capacity(args.len() + 1);
    items.push(Value::object(o));
    items.extend_from_slice(args);

    for element in items {
        root_if_heap(vm, element);
        if is_concat_spreadable(vm, element)? {
            let eo = element.as_object().unwrap();
            let len = vm.length_of(eo)?;
            if len > MAX_LENGTH - n {
                return Err(vm.type_error("Array.prototype.concat result is too long"));
            }
            let mut k = 0u64;
            while k < len {
                let source_key = vm.index_key(k);
                if vm.has_property(eo, source_key)? {
                    let value = vm.get(eo, source_key)?;
                    root_if_heap(vm, value);
                    let target_key = vm.index_key(n);
                    vm.create_data_property_or_throw(a, target_key, value)?;
                }
                n += 1;
                k += 1;
            }
        } else {
            if n >= MAX_LENGTH {
                return Err(vm.type_error("Array.prototype.concat result is too long"));
            }
            let target_key = vm.index_key(n);
            vm.create_data_property_or_throw(a, target_key, element)?;
            n += 1;
        }
    }
    set_length(vm, a, n)?;
    Ok(Value::object(a))
}

fn clamp_relative(len: u64, relative: f64) -> u64 {
    if relative.is_infinite() && relative.is_sign_negative() {
        0
    } else if relative < 0.0 {
        (len as f64 + relative).max(0.0) as u64
    } else {
        (relative.min(len as f64)) as u64
    }
}

fn copy_within(vm: &mut Vm, this: Value, args: &[Value], _: Value) -> JsResult<Value> {
    let o = rooted_object(vm, this)?;
    let len = vm.length_of(o)?;

    let relative_target = vm.to_integer_or_infinity(arg(args, 0))?;
    let mut to = clamp_relative(len, relative_target);
    let relative_start = vm.to_integer_or_infinity(arg(args, 1))?;
    let mut from = clamp_relative(len, relative_start);
    let relative_end = if arg(args, 2).is_undefined() {
        len as f64
    } else {
        vm.to_integer_or_infinity(arg(args, 2))?
    };
    let final_ = clamp_relative(len, relative_end);

    let mut count = final_.saturating_sub(from).min(len.saturating_sub(to));
    let mut direction = 1i64;
    if from < to && to < from + count {
        direction = -1;
        from += count - 1;
        to += count - 1;
    }

    while count > 0 {
        let from_key = vm.index_key(from);
        if vm.has_property(o, from_key)? {
            let value = vm.get(o, from_key)?;
            root_if_heap(vm, value);
            let to_key = vm.index_key(to);
            vm.set_or_throw(o, to_key, value)?;
        } else {
            let to_key = vm.index_key(to);
            vm.delete_or_throw(o, to_key)?;
        }
        if direction < 0 {
            from = from.saturating_sub(1);
            to = to.saturating_sub(1);
        } else {
            from += 1;
            to += 1;
        }
        count -= 1;
    }
    Ok(Value::object(o))
}

fn every(vm: &mut Vm, this: Value, args: &[Value], _: Value) -> JsResult<Value> {
    let o = rooted_object(vm, this)?;
    let len = vm.length_of(o)?;
    let callback = arg(args, 0);
    if !vm.is_callable(callback) {
        return Err(vm.type_error("Array.prototype.every callback is not callable"));
    }
    let this_arg = arg(args, 1);
    for k in 0..len {
        let key = vm.index_key(k);
        if vm.has_property(o, key)? {
            let value = vm.get(o, key)?;
            root_if_heap(vm, value);
            let r = vm.call(
                callback,
                this_arg,
                &[value, Value::number(k as f64), Value::object(o)],
            )?;
            if !vm.to_boolean(r) {
                return Ok(Value::FALSE);
            }
        }
    }
    Ok(Value::TRUE)
}

fn fill(vm: &mut Vm, this: Value, args: &[Value], _: Value) -> JsResult<Value> {
    let o = rooted_object(vm, this)?;
    let len = vm.length_of(o)?;
    let relative_start = vm.to_integer_or_infinity(arg(args, 1))?;
    let mut k = clamp_relative(len, relative_start);
    let relative_end = if arg(args, 2).is_undefined() {
        len as f64
    } else {
        vm.to_integer_or_infinity(arg(args, 2))?
    };
    let final_ = clamp_relative(len, relative_end);
    let value = arg(args, 0);
    root_if_heap(vm, value);
    while k < final_ {
        let key = vm.index_key(k);
        vm.set_or_throw(o, key, value)?;
        k += 1;
    }
    Ok(Value::object(o))
}

fn filter(vm: &mut Vm, this: Value, args: &[Value], _: Value) -> JsResult<Value> {
    let o = rooted_object(vm, this)?;
    let len = vm.length_of(o)?;
    let callback = arg(args, 0);
    if !vm.is_callable(callback) {
        return Err(vm.type_error("Array.prototype.filter callback is not callable"));
    }
    let this_arg = arg(args, 1);
    let a = species_create(vm, o, 0)?;
    vm.root(Value::object(a));
    let mut to = 0u64;
    for k in 0..len {
        let key = vm.index_key(k);
        if vm.has_property(o, key)? {
            let value = vm.get(o, key)?;
            root_if_heap(vm, value);
            let r = vm.call(
                callback,
                this_arg,
                &[value, Value::number(k as f64), Value::object(o)],
            )?;
            if vm.to_boolean(r) {
                let target = vm.index_key(to);
                vm.create_data_property_or_throw(a, target, value)?;
                to += 1;
            }
        }
    }
    Ok(Value::object(a))
}

fn find_via_predicate(
    vm: &mut Vm,
    o: ObjId,
    len: u64,
    descending: bool,
    predicate: Value,
    this_arg: Value,
) -> JsResult<Option<(u64, Value)>> {
    if !vm.is_callable(predicate) {
        return Err(vm.type_error("Array find predicate is not callable"));
    }
    if descending {
        for k in (0..len).rev() {
            let key = vm.index_key(k);
            let value = vm.get(o, key)?;
            root_if_heap(vm, value);
            let r = vm.call(
                predicate,
                this_arg,
                &[value, Value::number(k as f64), Value::object(o)],
            )?;
            if vm.to_boolean(r) {
                return Ok(Some((k, value)));
            }
        }
    } else {
        for k in 0..len {
            let key = vm.index_key(k);
            let value = vm.get(o, key)?;
            root_if_heap(vm, value);
            let r = vm.call(
                predicate,
                this_arg,
                &[value, Value::number(k as f64), Value::object(o)],
            )?;
            if vm.to_boolean(r) {
                return Ok(Some((k, value)));
            }
        }
    }
    Ok(None)
}

fn find(vm: &mut Vm, this: Value, args: &[Value], _: Value) -> JsResult<Value> {
    let o = rooted_object(vm, this)?;
    let len = vm.length_of(o)?;
    match find_via_predicate(vm, o, len, false, arg(args, 0), arg(args, 1))? {
        Some((_, value)) => Ok(value),
        None => Ok(Value::UNDEFINED),
    }
}

fn find_index(vm: &mut Vm, this: Value, args: &[Value], _: Value) -> JsResult<Value> {
    let o = rooted_object(vm, this)?;
    let len = vm.length_of(o)?;
    match find_via_predicate(vm, o, len, false, arg(args, 0), arg(args, 1))? {
        Some((k, _)) => Ok(Value::number(k as f64)),
        None => Ok(Value::int(-1)),
    }
}

fn find_last(vm: &mut Vm, this: Value, args: &[Value], _: Value) -> JsResult<Value> {
    let o = rooted_object(vm, this)?;
    let len = vm.length_of(o)?;
    match find_via_predicate(vm, o, len, true, arg(args, 0), arg(args, 1))? {
        Some((_, value)) => Ok(value),
        None => Ok(Value::UNDEFINED),
    }
}

fn find_last_index(vm: &mut Vm, this: Value, args: &[Value], _: Value) -> JsResult<Value> {
    let o = rooted_object(vm, this)?;
    let len = vm.length_of(o)?;
    match find_via_predicate(vm, o, len, true, arg(args, 0), arg(args, 1))? {
        Some((k, _)) => Ok(Value::number(k as f64)),
        None => Ok(Value::int(-1)),
    }
}

fn flatten_into_array(
    vm: &mut Vm,
    target: ObjId,
    source: ObjId,
    source_len: u64,
    start: u64,
    depth: f64,
    mapper: Option<Value>,
    this_arg: Value,
) -> JsResult<u64> {
    let mut target_index = start;
    let mut source_index = 0u64;
    while source_index < source_len {
        let key = vm.index_key(source_index);
        if vm.has_property(source, key)? {
            let mut element = vm.get(source, key)?;
            root_if_heap(vm, element);
            if let Some(callback) = mapper {
                element = vm.call(
                    callback,
                    this_arg,
                    &[
                        element,
                        Value::number(source_index as f64),
                        Value::object(source),
                    ],
                )?;
                root_if_heap(vm, element);
            }

            if depth > 0.0 && is_array(vm, element)? {
                let element_object = element.as_object().unwrap();
                let element_len = vm.length_of(element_object)?;
                let new_depth = if depth.is_infinite() && depth.is_sign_positive() {
                    depth
                } else {
                    depth - 1.0
                };
                target_index = flatten_into_array(
                    vm,
                    target,
                    element_object,
                    element_len,
                    target_index,
                    new_depth,
                    None,
                    Value::UNDEFINED,
                )?;
            } else {
                if target_index >= MAX_LENGTH {
                    return Err(vm.type_error("Flattened array is too long"));
                }
                let target_key = vm.index_key(target_index);
                vm.create_data_property_or_throw(target, target_key, element)?;
                target_index += 1;
            }
        }
        source_index += 1;
    }
    Ok(target_index)
}

fn flat(vm: &mut Vm, this: Value, args: &[Value], _: Value) -> JsResult<Value> {
    let o = rooted_object(vm, this)?;
    let source_len = vm.length_of(o)?;
    let mut depth = 1.0f64;
    if !arg(args, 0).is_undefined() {
        depth = vm.to_integer_or_infinity(arg(args, 0))?;
        if depth < 0.0 {
            depth = 0.0;
        }
    }
    let a = species_create(vm, o, 0)?;
    vm.root(Value::object(a));
    flatten_into_array(vm, a, o, source_len, 0, depth, None, Value::UNDEFINED)?;
    Ok(Value::object(a))
}

fn flat_map(vm: &mut Vm, this: Value, args: &[Value], _: Value) -> JsResult<Value> {
    let o = rooted_object(vm, this)?;
    let source_len = vm.length_of(o)?;
    let mapper = arg(args, 0);
    if !vm.is_callable(mapper) {
        return Err(vm.type_error("Array.prototype.flatMap mapper is not callable"));
    }
    let a = species_create(vm, o, 0)?;
    vm.root(Value::object(a));
    flatten_into_array(vm, a, o, source_len, 0, 1.0, Some(mapper), arg(args, 1))?;
    Ok(Value::object(a))
}

fn for_each(vm: &mut Vm, this: Value, args: &[Value], _: Value) -> JsResult<Value> {
    let o = rooted_object(vm, this)?;
    let len = vm.length_of(o)?;
    let callback = arg(args, 0);
    if !vm.is_callable(callback) {
        return Err(vm.type_error("Array.prototype.forEach callback is not callable"));
    }
    let this_arg = arg(args, 1);
    for k in 0..len {
        let key = vm.index_key(k);
        if vm.has_property(o, key)? {
            let value = vm.get(o, key)?;
            root_if_heap(vm, value);
            vm.call(
                callback,
                this_arg,
                &[value, Value::number(k as f64), Value::object(o)],
            )?;
        }
    }
    Ok(Value::UNDEFINED)
}

fn includes(vm: &mut Vm, this: Value, args: &[Value], _: Value) -> JsResult<Value> {
    let o = rooted_object(vm, this)?;
    let len = vm.length_of(o)?;
    if len == 0 {
        return Ok(Value::FALSE);
    }
    let search = arg(args, 0);
    let mut n = vm.to_integer_or_infinity(arg(args, 1))?;
    if n.is_infinite() && n.is_sign_positive() {
        return Ok(Value::FALSE);
    }
    if n.is_infinite() && n.is_sign_negative() {
        n = 0.0;
    }
    let mut k = if n >= 0.0 {
        if n >= len as f64 {
            return Ok(Value::FALSE);
        }
        n as u64
    } else {
        let k = len as f64 + n;
        if k < 0.0 { 0 } else { k as u64 }
    };
    while k < len {
        let key = vm.index_key(k);
        let value = vm.get(o, key)?;
        if vm.same_value_zero(search, value) {
            return Ok(Value::TRUE);
        }
        k += 1;
    }
    Ok(Value::FALSE)
}

fn index_of(vm: &mut Vm, this: Value, args: &[Value], _: Value) -> JsResult<Value> {
    let o = rooted_object(vm, this)?;
    let len = vm.length_of(o)?;
    if len == 0 {
        return Ok(Value::int(-1));
    }
    let search = arg(args, 0);
    let mut n = vm.to_integer_or_infinity(arg(args, 1))?;
    if n.is_infinite() && n.is_sign_positive() {
        return Ok(Value::int(-1));
    }
    if n.is_infinite() && n.is_sign_negative() {
        n = 0.0;
    }
    let mut k = if n >= 0.0 {
        if n >= len as f64 {
            return Ok(Value::int(-1));
        }
        n as u64
    } else {
        let k = len as f64 + n;
        if k < 0.0 { 0 } else { k as u64 }
    };
    while k < len {
        let key = vm.index_key(k);
        if vm.has_property(o, key)? {
            let value = vm.get(o, key)?;
            if vm.strict_equals(search, value) {
                return Ok(Value::number(k as f64));
            }
        }
        k += 1;
    }
    Ok(Value::int(-1))
}

fn last_index_of(vm: &mut Vm, this: Value, args: &[Value], _: Value) -> JsResult<Value> {
    let o = rooted_object(vm, this)?;
    let len = vm.length_of(o)?;
    if len == 0 {
        return Ok(Value::int(-1));
    }
    let search = arg(args, 0);
    let mut n = if args.len() > 1 {
        vm.to_integer_or_infinity(arg(args, 1))?
    } else {
        len as f64 - 1.0
    };
    if n.is_infinite() && n.is_sign_negative() {
        return Ok(Value::int(-1));
    }
    let mut k = if n >= 0.0 {
        n.min(len as f64 - 1.0) as u64
    } else {
        let k = len as f64 + n;
        if k < 0.0 {
            return Ok(Value::int(-1));
        }
        k as u64
    };
    loop {
        let key = vm.index_key(k);
        if vm.has_property(o, key)? {
            let value = vm.get(o, key)?;
            if vm.strict_equals(search, value) {
                return Ok(Value::number(k as f64));
            }
        }
        if k == 0 {
            break;
        }
        k -= 1;
    }
    Ok(Value::int(-1))
}

fn map(vm: &mut Vm, this: Value, args: &[Value], _: Value) -> JsResult<Value> {
    let o = rooted_object(vm, this)?;
    let len = vm.length_of(o)?;
    let callback = arg(args, 0);
    if !vm.is_callable(callback) {
        return Err(vm.type_error("Array.prototype.map callback is not callable"));
    }
    let this_arg = arg(args, 1);
    let a = species_create(vm, o, len)?;
    vm.root(Value::object(a));
    for k in 0..len {
        let key = vm.index_key(k);
        if vm.has_property(o, key)? {
            let value = vm.get(o, key)?;
            root_if_heap(vm, value);
            let mapped = vm.call(
                callback,
                this_arg,
                &[value, Value::number(k as f64), Value::object(o)],
            )?;
            root_if_heap(vm, mapped);
            vm.create_data_property_or_throw(a, key, mapped)?;
        }
    }
    Ok(Value::object(a))
}

fn pop(vm: &mut Vm, this: Value, _: &[Value], _: Value) -> JsResult<Value> {
    let o = rooted_object(vm, this)?;
    let len = vm.length_of(o)?;
    if len == 0 {
        set_length(vm, o, 0)?;
        return Ok(Value::UNDEFINED);
    }
    let index = len - 1;
    let key = vm.index_key(index);
    let element = vm.get(o, key)?;
    root_if_heap(vm, element);
    vm.delete_or_throw(o, key)?;
    set_length(vm, o, index)?;
    Ok(element)
}

fn reduce(vm: &mut Vm, this: Value, args: &[Value], _: Value) -> JsResult<Value> {
    let o = rooted_object(vm, this)?;
    let len = vm.length_of(o)?;
    let callback = arg(args, 0);
    if !vm.is_callable(callback) {
        return Err(vm.type_error("Array.prototype.reduce callback is not callable"));
    }
    let has_initial = args.len() > 1;
    if len == 0 && !has_initial {
        return Err(vm.type_error("Reduce of empty array with no initial value"));
    }

    let mut k = 0u64;
    let mut accumulator = if has_initial {
        let value = arg(args, 1);
        root_if_heap(vm, value);
        value
    } else {
        let mut found = false;
        let mut value = Value::UNDEFINED;
        while k < len {
            let key = vm.index_key(k);
            if vm.has_property(o, key)? {
                value = vm.get(o, key)?;
                root_if_heap(vm, value);
                found = true;
                k += 1;
                break;
            }
            k += 1;
        }
        if !found {
            return Err(vm.type_error("Reduce of empty array with no initial value"));
        }
        value
    };

    while k < len {
        let key = vm.index_key(k);
        if vm.has_property(o, key)? {
            let value = vm.get(o, key)?;
            root_if_heap(vm, value);
            accumulator = vm.call(
                callback,
                Value::UNDEFINED,
                &[
                    accumulator,
                    value,
                    Value::number(k as f64),
                    Value::object(o),
                ],
            )?;
            root_if_heap(vm, accumulator);
        }
        k += 1;
    }
    Ok(accumulator)
}

fn reduce_right(vm: &mut Vm, this: Value, args: &[Value], _: Value) -> JsResult<Value> {
    let o = rooted_object(vm, this)?;
    let len = vm.length_of(o)?;
    let callback = arg(args, 0);
    if !vm.is_callable(callback) {
        return Err(vm.type_error("Array.prototype.reduceRight callback is not callable"));
    }
    let has_initial = args.len() > 1;
    if len == 0 && !has_initial {
        return Err(vm.type_error("Reduce of empty array with no initial value"));
    }

    let mut k = len as i64 - 1;
    let mut accumulator = if has_initial {
        let value = arg(args, 1);
        root_if_heap(vm, value);
        value
    } else {
        let mut found = false;
        let mut value = Value::UNDEFINED;
        while k >= 0 {
            let key = vm.index_key(k as u64);
            if vm.has_property(o, key)? {
                value = vm.get(o, key)?;
                root_if_heap(vm, value);
                found = true;
                k -= 1;
                break;
            }
            k -= 1;
        }
        if !found {
            return Err(vm.type_error("Reduce of empty array with no initial value"));
        }
        value
    };

    while k >= 0 {
        let key = vm.index_key(k as u64);
        if vm.has_property(o, key)? {
            let value = vm.get(o, key)?;
            root_if_heap(vm, value);
            accumulator = vm.call(
                callback,
                Value::UNDEFINED,
                &[
                    accumulator,
                    value,
                    Value::number(k as f64),
                    Value::object(o),
                ],
            )?;
            root_if_heap(vm, accumulator);
        }
        k -= 1;
    }
    Ok(accumulator)
}

fn reverse(vm: &mut Vm, this: Value, _: &[Value], _: Value) -> JsResult<Value> {
    let o = rooted_object(vm, this)?;
    let len = vm.length_of(o)?;
    let middle = len / 2;
    let mut lower = 0u64;
    while lower != middle {
        let upper = len - lower - 1;
        let lower_key = vm.index_key(lower);
        let upper_key = vm.index_key(upper);
        let lower_exists = vm.has_property(o, lower_key)?;
        let lower_value = if lower_exists {
            let value = vm.get(o, lower_key)?;
            root_if_heap(vm, value);
            value
        } else {
            Value::UNDEFINED
        };
        let upper_exists = vm.has_property(o, upper_key)?;
        let upper_value = if upper_exists {
            let value = vm.get(o, upper_key)?;
            root_if_heap(vm, value);
            value
        } else {
            Value::UNDEFINED
        };
        if lower_exists && upper_exists {
            vm.set_or_throw(o, lower_key, upper_value)?;
            vm.set_or_throw(o, upper_key, lower_value)?;
        } else if !lower_exists && upper_exists {
            vm.set_or_throw(o, lower_key, upper_value)?;
            vm.delete_or_throw(o, upper_key)?;
        } else if lower_exists && !upper_exists {
            vm.delete_or_throw(o, lower_key)?;
            vm.set_or_throw(o, upper_key, lower_value)?;
        }
        lower += 1;
    }
    Ok(Value::object(o))
}

fn shift(vm: &mut Vm, this: Value, _: &[Value], _: Value) -> JsResult<Value> {
    let o = rooted_object(vm, this)?;
    let len = vm.length_of(o)?;
    if len == 0 {
        set_length(vm, o, 0)?;
        return Ok(Value::UNDEFINED);
    }
    let first_key = vm.index_key(0);
    let first = vm.get(o, first_key)?;
    root_if_heap(vm, first);
    let mut k = 1u64;
    while k < len {
        let from = vm.index_key(k);
        let to = vm.index_key(k - 1);
        if vm.has_property(o, from)? {
            let value = vm.get(o, from)?;
            root_if_heap(vm, value);
            vm.set_or_throw(o, to, value)?;
        } else {
            vm.delete_or_throw(o, to)?;
        }
        k += 1;
    }
    let last = vm.index_key(len - 1);
    vm.delete_or_throw(o, last)?;
    set_length(vm, o, len - 1)?;
    Ok(first)
}

fn slice(vm: &mut Vm, this: Value, args: &[Value], _: Value) -> JsResult<Value> {
    let o = rooted_object(vm, this)?;
    let len = vm.length_of(o)?;
    let relative_start = vm.to_integer_or_infinity(arg(args, 0))?;
    let mut k = clamp_relative(len, relative_start);
    let relative_end = if arg(args, 1).is_undefined() {
        len as f64
    } else {
        vm.to_integer_or_infinity(arg(args, 1))?
    };
    let final_ = clamp_relative(len, relative_end);
    let count = final_.saturating_sub(k);
    let a = species_create(vm, o, count)?;
    vm.root(Value::object(a));
    let mut n = 0u64;
    while k < final_ {
        let source_key = vm.index_key(k);
        if vm.has_property(o, source_key)? {
            let value = vm.get(o, source_key)?;
            root_if_heap(vm, value);
            let target = vm.index_key(n);
            vm.create_data_property_or_throw(a, target, value)?;
        }
        k += 1;
        n += 1;
    }
    set_length(vm, a, n)?;
    Ok(Value::object(a))
}

fn some(vm: &mut Vm, this: Value, args: &[Value], _: Value) -> JsResult<Value> {
    let o = rooted_object(vm, this)?;
    let len = vm.length_of(o)?;
    let callback = arg(args, 0);
    if !vm.is_callable(callback) {
        return Err(vm.type_error("Array.prototype.some callback is not callable"));
    }
    let this_arg = arg(args, 1);
    for k in 0..len {
        let key = vm.index_key(k);
        if vm.has_property(o, key)? {
            let value = vm.get(o, key)?;
            root_if_heap(vm, value);
            let r = vm.call(
                callback,
                this_arg,
                &[value, Value::number(k as f64), Value::object(o)],
            )?;
            if vm.to_boolean(r) {
                return Ok(Value::TRUE);
            }
        }
    }
    Ok(Value::FALSE)
}

fn compare_array_elements(
    vm: &mut Vm,
    x: Value,
    y: Value,
    comparator: Value,
) -> JsResult<Ordering> {
    if x.is_undefined() && y.is_undefined() {
        return Ok(Ordering::Equal);
    }
    if x.is_undefined() {
        return Ok(Ordering::Greater);
    }
    if y.is_undefined() {
        return Ok(Ordering::Less);
    }
    if !comparator.is_undefined() {
        let r = vm.call(comparator, Value::UNDEFINED, &[x, y])?;
        let n = vm.to_number(r)?;
        if n.is_nan() {
            Ok(Ordering::Equal)
        } else if n < 0.0 {
            Ok(Ordering::Less)
        } else if n > 0.0 {
            Ok(Ordering::Greater)
        } else {
            Ok(Ordering::Equal)
        }
    } else {
        let xs = vm.to_string(x)?;
        let ys = vm.to_string(y)?;
        Ok(vm.str(xs).cmp_units(vm.str(ys)))
    }
}

fn stable_sort(vm: &mut Vm, mut items: Vec<Value>, comparator: Value) -> JsResult<Vec<Value>> {
    let len = items.len();
    if len < 2 {
        return Ok(items);
    }
    let mut source = items;
    let mut target = alloc::vec![Value::UNDEFINED; len];
    let mut width = 1usize;
    while width < len {
        let mut left = 0usize;
        while left < len {
            let middle = (left + width).min(len);
            let right = (left + width * 2).min(len);
            let mut i = left;
            let mut j = middle;
            let mut k = left;
            while i < middle && j < right {
                let ordering = compare_array_elements(vm, source[i], source[j], comparator)?;
                if ordering != Ordering::Greater {
                    target[k] = source[i];
                    i += 1;
                } else {
                    target[k] = source[j];
                    j += 1;
                }
                k += 1;
            }
            while i < middle {
                target[k] = source[i];
                i += 1;
                k += 1;
            }
            while j < right {
                target[k] = source[j];
                j += 1;
                k += 1;
            }
            left += width * 2;
        }
        core::mem::swap(&mut source, &mut target);
        width = width.saturating_mul(2);
        if width == 0 {
            break;
        }
    }
    items = source;
    Ok(items)
}

fn sort(vm: &mut Vm, this: Value, args: &[Value], _: Value) -> JsResult<Value> {
    let comparator = arg(args, 0);
    if !comparator.is_undefined() && !vm.is_callable(comparator) {
        return Err(vm.type_error("Array.prototype.sort comparator is not callable"));
    }
    let o = rooted_object(vm, this)?;
    let len = vm.length_of(o)?;
    let mut items = Vec::new();
    for k in 0..len {
        let key = vm.index_key(k);
        if vm.has_property(o, key)? {
            let value = vm.get(o, key)?;
            root_if_heap(vm, value);
            items.push(value);
        }
    }
    let sorted = stable_sort(vm, items, comparator)?;
    let item_count = sorted.len() as u64;
    for (k, value) in sorted.into_iter().enumerate() {
        let key = vm.index_key(k as u64);
        vm.set_or_throw(o, key, value)?;
    }
    // Remove the indices that were holes in the original list. The count
    // comes from SortIndexedProperties, before any writes or deletions.
    let mut k = item_count;
    while k < len {
        let key = vm.index_key(k);
        vm.delete_or_throw(o, key)?;
        k += 1;
    }
    Ok(Value::object(o))
}

fn splice(vm: &mut Vm, this: Value, args: &[Value], _: Value) -> JsResult<Value> {
    let o = rooted_object(vm, this)?;
    let len = vm.length_of(o)?;
    let relative_start = vm.to_integer_or_infinity(arg(args, 0))?;
    let actual_start = clamp_relative(len, relative_start);
    let item_count = args.len().saturating_sub(2) as u64;
    let actual_delete_count = if args.is_empty() {
        0
    } else if args.len() == 1 {
        len - actual_start
    } else {
        let dc = vm.to_integer_or_infinity(arg(args, 1))?;
        dc.max(0.0).min((len - actual_start) as f64) as u64
    };
    let new_len = len - actual_delete_count + item_count;
    if new_len > MAX_LENGTH {
        return Err(vm.type_error("Array.prototype.splice result is too long"));
    }

    let a = species_create(vm, o, actual_delete_count)?;
    vm.root(Value::object(a));
    for k in 0..actual_delete_count {
        let from = vm.index_key(actual_start + k);
        if vm.has_property(o, from)? {
            let value = vm.get(o, from)?;
            root_if_heap(vm, value);
            let target = vm.index_key(k);
            vm.create_data_property_or_throw(a, target, value)?;
        }
    }
    set_length(vm, a, actual_delete_count)?;

    if item_count < actual_delete_count {
        let mut k = actual_start;
        while k < len - actual_delete_count {
            let from = vm.index_key(k + actual_delete_count);
            let to = vm.index_key(k + item_count);
            if vm.has_property(o, from)? {
                let value = vm.get(o, from)?;
                root_if_heap(vm, value);
                vm.set_or_throw(o, to, value)?;
            } else {
                vm.delete_or_throw(o, to)?;
            }
            k += 1;
        }
        let mut k = len;
        while k > len - actual_delete_count + item_count {
            let key = vm.index_key(k - 1);
            vm.delete_or_throw(o, key)?;
            k -= 1;
        }
    } else if item_count > actual_delete_count {
        let mut k = len - actual_delete_count;
        while k > actual_start {
            let from = vm.index_key(k + actual_delete_count - 1);
            let to = vm.index_key(k + item_count - 1);
            if vm.has_property(o, from)? {
                let value = vm.get(o, from)?;
                root_if_heap(vm, value);
                vm.set_or_throw(o, to, value)?;
            } else {
                vm.delete_or_throw(o, to)?;
            }
            k -= 1;
        }
    }

    let mut k = actual_start;
    for &value in args.iter().skip(2) {
        root_if_heap(vm, value);
        let key = vm.index_key(k);
        vm.set_or_throw(o, key, value)?;
        k += 1;
    }
    set_length(vm, o, new_len)?;
    Ok(Value::object(a))
}

fn to_locale_string(vm: &mut Vm, this: Value, _: &[Value], _: Value) -> JsResult<Value> {
    let array = rooted_object(vm, this)?;
    let len = vm.length_of(array)?;
    let separator = vm.new_string(",").as_string().unwrap();
    let mut result = Vec::<u16>::new();
    for k in 0..len {
        if k > 0 {
            result.extend(vm.str(separator).units());
        }
        let key = vm.index_key(k);
        let element = vm.get(array, key)?;
        if !element.is_nullish() {
            let method_key = vm.key("toLocaleString");
            let method = vm.get_v(element, method_key)?;
            if !vm.is_callable(method) {
                return Err(vm.type_error("Element toLocaleString is not callable"));
            }
            let value = vm.call(method, element, &[])?;
            let string = vm.to_string(value)?;
            result.extend(vm.str(string).units());
        }
    }
    Ok(vm.new_string_units(&result))
}

fn to_reversed(vm: &mut Vm, this: Value, _: &[Value], _: Value) -> JsResult<Value> {
    let o = rooted_object(vm, this)?;
    let len = vm.length_of(o)?;
    let a = array_create(vm, len)?;
    vm.root(Value::object(a));
    for k in 0..len {
        let from = vm.index_key(len - k - 1);
        let value = vm.get(o, from)?;
        root_if_heap(vm, value);
        let key = vm.index_key(k);
        vm.create_data_property_or_throw(a, key, value)?;
    }
    Ok(Value::object(a))
}

fn to_sorted(vm: &mut Vm, this: Value, args: &[Value], _: Value) -> JsResult<Value> {
    let comparator = arg(args, 0);
    if !comparator.is_undefined() && !vm.is_callable(comparator) {
        return Err(vm.type_error("Array.prototype.toSorted comparator is not callable"));
    }
    let o = rooted_object(vm, this)?;
    let len = vm.length_of(o)?;
    // ArrayCreate is performed before indexed properties are read. In
    // particular, an Array length above 2^32 - 1 throws before any getter is
    // invoked.
    let a = array_create(vm, len)?;
    vm.root(Value::object(a));
    let mut items = Vec::new();
    for k in 0..len {
        let key = vm.index_key(k);
        let value = vm.get(o, key)?;
        root_if_heap(vm, value);
        items.push(value);
    }
    let sorted = stable_sort(vm, items, comparator)?;
    for (k, value) in sorted.into_iter().enumerate() {
        let key = vm.index_key(k as u64);
        vm.create_data_property_or_throw(a, key, value)?;
    }
    Ok(Value::object(a))
}

fn to_spliced(vm: &mut Vm, this: Value, args: &[Value], _: Value) -> JsResult<Value> {
    let o = rooted_object(vm, this)?;
    let len = vm.length_of(o)?;
    let relative_start = vm.to_integer_or_infinity(arg(args, 0))?;
    let actual_start = clamp_relative(len, relative_start);
    let insert_count = args.len().saturating_sub(2) as u64;
    let actual_skip_count = if args.is_empty() {
        0
    } else if args.len() == 1 {
        len - actual_start
    } else {
        let sc = vm.to_integer_or_infinity(arg(args, 1))?;
        sc.max(0.0).min((len - actual_start) as f64) as u64
    };
    let new_len = len - actual_skip_count + insert_count;
    if new_len > MAX_LENGTH {
        return Err(vm.type_error("Array.prototype.toSpliced result is too long"));
    }
    let a = array_create(vm, new_len)?;
    vm.root(Value::object(a));

    let mut i = 0u64;
    while i < actual_start {
        let key = vm.index_key(i);
        let value = vm.get(o, key)?;
        root_if_heap(vm, value);
        vm.create_data_property_or_throw(a, key, value)?;
        i += 1;
    }
    for &value in args.iter().skip(2) {
        root_if_heap(vm, value);
        let key = vm.index_key(i);
        vm.create_data_property_or_throw(a, key, value)?;
        i += 1;
    }
    let mut r = actual_start + actual_skip_count;
    while i < new_len {
        let key = vm.index_key(i);
        let from = vm.index_key(r);
        let value = vm.get(o, from)?;
        root_if_heap(vm, value);
        vm.create_data_property_or_throw(a, key, value)?;
        i += 1;
        r += 1;
    }
    Ok(Value::object(a))
}

fn unshift(vm: &mut Vm, this: Value, args: &[Value], _: Value) -> JsResult<Value> {
    let o = rooted_object(vm, this)?;
    let len = vm.length_of(o)?;
    let arg_count = args.len() as u64;
    if arg_count > 0 && len > MAX_LENGTH - arg_count {
        return Err(vm.type_error("Array.prototype.unshift result is too long"));
    }
    if arg_count > 0 {
        let mut k = len;
        while k > 0 {
            let from = vm.index_key(k - 1);
            let to = vm.index_key(k + arg_count - 1);
            if vm.has_property(o, from)? {
                let value = vm.get(o, from)?;
                root_if_heap(vm, value);
                vm.set_or_throw(o, to, value)?;
            } else {
                vm.delete_or_throw(o, to)?;
            }
            k -= 1;
        }
        for (j, &value) in args.iter().enumerate() {
            root_if_heap(vm, value);
            let key = vm.index_key(j as u64);
            vm.set_or_throw(o, key, value)?;
        }
    }
    let new_len = len + arg_count;
    set_length(vm, o, new_len)?;
    Ok(Value::number(new_len as f64))
}

fn with(vm: &mut Vm, this: Value, args: &[Value], _: Value) -> JsResult<Value> {
    let o = rooted_object(vm, this)?;
    let len = vm.length_of(o)?;
    let relative = vm.to_integer_or_infinity(arg(args, 0))?;
    let actual = if relative >= 0.0 {
        relative
    } else {
        len as f64 + relative
    };
    if actual < 0.0 || actual >= len as f64 {
        return Err(vm.range_error("Array.prototype.with index is out of range"));
    }
    let actual_index = actual as u64;
    let a = array_create(vm, len)?;
    vm.root(Value::object(a));
    for k in 0..len {
        let key = vm.index_key(k);
        let value = if k == actual_index {
            arg(args, 1)
        } else {
            let v = vm.get(o, key)?;
            root_if_heap(vm, v);
            v
        };
        vm.create_data_property_or_throw(a, key, value)?;
    }
    Ok(Value::object(a))
}
