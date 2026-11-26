//! %TypedArray%.prototype methods and %TypedArray%.from/of (ECMA-262
//! §23.2). Element reads go through `get_index`, which gives undefined
//! for an index that a shrunk or detached buffer no longer covers, so the
//! callback methods follow the spec when user code resizes the buffer.

use alloc::vec::Vec;
use core::cmp::Ordering;

use super::buffer::{
    convert_for, create_from_ctor, create_ta, element_location, get_index, kind_of, proto_of_kind,
    read_raw, set_index, species_create, ta_length_checked, validate, write_raw,
};
use super::object::arg;
use crate::object::{attr, Key};
use crate::value::{Kind, ObjId, Value};
use crate::vm::{JsResult, Vm};

pub fn init(vm: &mut Vm, proto: ObjId, ctor: ObjId) {
    vm.method(ctor, "from", 1, from);
    vm.method(ctor, "of", 0, of);

    vm.method(proto, "at", 1, at);
    vm.method(proto, "copyWithin", 2, copy_within);
    vm.method(proto, "every", 1, |vm, this, a, _| iterate(vm, this, a, Mode::Every));
    vm.method(proto, "fill", 1, fill);
    vm.method(proto, "filter", 1, filter);
    vm.method(proto, "find", 1, |vm, this, a, _| iterate(vm, this, a, Mode::Find));
    vm.method(proto, "findIndex", 1, |vm, this, a, _| iterate(vm, this, a, Mode::FindIndex));
    vm.method(proto, "findLast", 1, |vm, this, a, _| iterate(vm, this, a, Mode::FindLast));
    vm.method(proto, "findLastIndex", 1, |vm, this, a, _| iterate(vm, this, a, Mode::FindLastIndex));
    vm.method(proto, "forEach", 1, |vm, this, a, _| iterate(vm, this, a, Mode::ForEach));
    vm.method(proto, "includes", 1, includes);
    vm.method(proto, "indexOf", 1, index_of);
    vm.method(proto, "join", 1, join);
    vm.method(proto, "lastIndexOf", 1, last_index_of);
    vm.method(proto, "map", 1, map);
    vm.method(proto, "reduce", 1, |vm, this, a, _| reduce(vm, this, a, false));
    vm.method(proto, "reduceRight", 1, |vm, this, a, _| reduce(vm, this, a, true));
    vm.method(proto, "reverse", 0, reverse);
    vm.method(proto, "slice", 2, slice);
    vm.method(proto, "some", 1, |vm, this, a, _| iterate(vm, this, a, Mode::Some));
    vm.method(proto, "sort", 1, sort);
    vm.method(proto, "toLocaleString", 0, to_locale_string);
    vm.method(proto, "toReversed", 0, to_reversed);
    vm.method(proto, "toSorted", 1, to_sorted);
    vm.method(proto, "with", 2, with);
    // %TypedArray%.prototype.toString is %Array.prototype.toString%.
    let ap = vm.intrinsics.array_prototype;
    let k = vm.key("toString");
    if let Some(crate::vm::props::Slot::Data(f, _)) = vm.raw_get_own(ap, k) {
        vm.define_data(proto, k, f, attr::HIDDEN);
    }
}

// ----- helpers ---------------------------------------------------------------

/// A relative index argument (negative counts from the end), clamped to
/// 0..=len.
fn relative(vm: &mut Vm, v: Value, len: usize, default: usize) -> JsResult<usize> {
    if v.is_undefined() {
        return Ok(default);
    }
    let n = vm.to_integer_or_infinity(v)?;
    let len = len as f64;
    Ok(if n < 0.0 { (len + n).max(0.0) } else { n.min(len) } as usize)
}

fn callback(vm: &mut Vm, f: Value) -> JsResult<Value> {
    if vm.is_callable(f) {
        Ok(f)
    } else {
        let s = vm.display(f);
        Err(vm.type_error(&alloc::format!("{s} is not a function")))
    }
}

/// A new typed array of the same element type and `len` elements
/// (TypedArrayCreateSameType).
fn create_same_type(vm: &mut Vm, o: ObjId, len: usize) -> JsResult<ObjId> {
    let k = kind_of(vm, o);
    let p = proto_of_kind(vm, k);
    create_ta(vm, p, k, len)
}

/// The numeric order of the default sort: -0 before +0, NaN last; BigInt
/// kinds compare as BigInts.
fn default_compare(vm: &Vm, a: Value, b: Value) -> Ordering {
    if let (Kind::BigInt(x), Kind::BigInt(y)) = (a.kind(), b.kind()) {
        return crate::builtins::bigint_impl::cmp(vm.bigint(x), vm.bigint(y));
    }
    let (x, y) = (a.num(), b.num());
    match (x.is_nan(), y.is_nan()) {
        (true, true) => Ordering::Equal,
        (true, false) => Ordering::Greater,
        (false, true) => Ordering::Less,
        _ => {
            if x < y {
                Ordering::Less
            } else if x > y {
                Ordering::Greater
            } else if x == 0.0 && y == 0.0 {
                // -0 sorts before +0.
                y.is_sign_negative().cmp(&x.is_sign_negative())
            } else {
                Ordering::Equal
            }
        }
    }
}

/// A stable merge sort with a fallible comparison.
fn merge_sort(vm: &mut Vm, items: &mut Vec<Value>, cmp: &mut dyn FnMut(&mut Vm, Value, Value) -> JsResult<Ordering>) -> JsResult<()> {
    let n = items.len();
    if n < 2 {
        return Ok(());
    }
    let mut buf = items.clone();
    let mut width = 1;
    while width < n {
        let mut i = 0;
        while i < n {
            let mid = (i + width).min(n);
            let end = (i + 2 * width).min(n);
            let (mut l, mut r, mut k) = (i, mid, i);
            while l < mid && r < end {
                // Take from the right only when it is strictly smaller.
                if cmp(vm, items[r], items[l])? == Ordering::Less {
                    buf[k] = items[r];
                    r += 1;
                } else {
                    buf[k] = items[l];
                    l += 1;
                }
                k += 1;
            }
            while l < mid {
                buf[k] = items[l];
                l += 1;
                k += 1;
            }
            while r < end {
                buf[k] = items[r];
                r += 1;
                k += 1;
            }
            i += 2 * width;
        }
        core::mem::swap(items, &mut buf);
        width *= 2;
    }
    Ok(())
}

/// Sorts the elements with `comparefn` (undefined: numeric order).
fn sorted(vm: &mut Vm, mut items: Vec<Value>, comparefn: Value) -> JsResult<Vec<Value>> {
    if comparefn.is_undefined() {
        let mut c = |vm: &mut Vm, a: Value, b: Value| Ok(default_compare(vm, a, b));
        merge_sort(vm, &mut items, &mut c)?;
    } else {
        let mut c = |vm: &mut Vm, a: Value, b: Value| {
            let r = vm.call(comparefn, Value::UNDEFINED, &[a, b])?;
            let n = vm.to_number(r)?;
            Ok(if n < 0.0 {
                Ordering::Less
            } else if n > 0.0 {
                Ordering::Greater
            } else {
                Ordering::Equal
            })
        };
        merge_sort(vm, &mut items, &mut c)?;
    }
    Ok(items)
}

fn check_comparefn(vm: &mut Vm, f: Value) -> JsResult<()> {
    if !f.is_undefined() && !vm.is_callable(f) {
        return Err(vm.type_error("The comparison function must be either a function or undefined"));
    }
    Ok(())
}

// ----- constructor functions -------------------------------------------------

/// %TypedArray%.from(source, mapfn, thisArg)
fn from(vm: &mut Vm, this: Value, a: &[Value], _: Value) -> JsResult<Value> {
    let c = this;
    if !vm.is_constructor(c) {
        return Err(vm.type_error("TypedArray.from: this is not a constructor"));
    }
    let (source, mapfn, this_arg) = (arg(a, 0), arg(a, 1), arg(a, 2));
    let mapping = !mapfn.is_undefined();
    if mapping {
        callback(vm, mapfn)?;
    }
    let si = Key::Sym(vm.intrinsics.sym_iterator);
    let using_iterator = vm.get_method(source, si)?;
    if using_iterator.is_some() {
        let values = super::array::iterable_to_list(vm, source)?;
        let len = values.len();
        let target = create_from_ctor(vm, c, &[Value::from_usize(len)], Some(len))?;
        vm.root(Value::object(target));
        for (k, v) in values.into_iter().enumerate() {
            let mapped = if mapping { vm.call(mapfn, this_arg, &[v, Value::from_usize(k)])? } else { v };
            vm.set_or_throw(target, Key::Index(k as u32), mapped)?;
        }
        return Ok(Value::object(target));
    }
    let array_like = vm.to_object(source)?;
    let len = vm.length_of(array_like)? as usize;
    let target = create_from_ctor(vm, c, &[Value::from_usize(len)], Some(len))?;
    vm.root(Value::object(target));
    for k in 0..len {
        let v = vm.get(array_like, Key::Index(k as u32))?;
        let mapped = if mapping { vm.call(mapfn, this_arg, &[v, Value::from_usize(k)])? } else { v };
        vm.set_or_throw(target, Key::Index(k as u32), mapped)?;
    }
    Ok(Value::object(target))
}

/// %TypedArray%.of(...items)
fn of(vm: &mut Vm, this: Value, a: &[Value], _: Value) -> JsResult<Value> {
    if !vm.is_constructor(this) {
        return Err(vm.type_error("TypedArray.of: this is not a constructor"));
    }
    let len = a.len();
    let target = create_from_ctor(vm, this, &[Value::from_usize(len)], Some(len))?;
    for (k, v) in a.iter().enumerate() {
        vm.set_or_throw(target, Key::Index(k as u32), *v)?;
    }
    Ok(Value::object(target))
}

// ----- prototype methods -----------------------------------------------------

fn at(vm: &mut Vm, this: Value, a: &[Value], _: Value) -> JsResult<Value> {
    let (o, len) = validate(vm, this)?;
    let n = vm.to_integer_or_infinity(arg(a, 0))?;
    let k = if n >= 0.0 { n } else { len as f64 + n };
    if k < 0.0 || k >= len as f64 {
        return Ok(Value::UNDEFINED);
    }
    Ok(get_index(vm, o, k as usize))
}

fn copy_within(vm: &mut Vm, this: Value, a: &[Value], _: Value) -> JsResult<Value> {
    let (o, len) = validate(vm, this)?;
    let to = relative(vm, arg(a, 0), len, 0)?;
    let from = relative(vm, arg(a, 1), len, 0)?;
    let fin = relative(vm, arg(a, 2), len, len)?;
    let count = fin.saturating_sub(from).min(len - to);
    if count > 0 {
        let len = ta_length_checked(vm, o)?;
        let (buffer, offset, size) = element_location(vm, o, 0);
        let limit = len * size + offset;
        let (mut src, mut dst) = (from * size + offset, to * size + offset);
        let mut n = count * size;
        let data = vm.array_buffer_bytes_mut(Value::object(buffer)).unwrap_or_default();
        if src < dst && dst < src + n {
            src += n - 1;
            dst += n - 1;
            while n > 0 {
                if src < limit && dst < limit {
                    data[dst] = data[src];
                }
                src = src.wrapping_sub(1);
                dst = dst.wrapping_sub(1);
                n -= 1;
            }
        } else {
            while n > 0 {
                if src < limit && dst < limit {
                    data[dst] = data[src];
                }
                src += 1;
                dst += 1;
                n -= 1;
            }
        }
    }
    Ok(Value::object(o))
}

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Every,
    Some,
    ForEach,
    Find,
    FindIndex,
    FindLast,
    FindLastIndex,
}

/// every, some, forEach and the find family.
fn iterate(vm: &mut Vm, this: Value, a: &[Value], mode: Mode) -> JsResult<Value> {
    let (o, len) = validate(vm, this)?;
    let f = callback(vm, arg(a, 0))?;
    let this_arg = arg(a, 1);
    let backwards = matches!(mode, Mode::FindLast | Mode::FindLastIndex);
    for i in 0..len {
        let k = if backwards { len - 1 - i } else { i };
        let v = get_index(vm, o, k);
        let kv = Value::from_usize(k);
        let r = vm.call(f, this_arg, &[v, kv, Value::object(o)])?;
        let t = vm.to_boolean(r);
        match mode {
            Mode::Every if !t => return Ok(Value::FALSE),
            Mode::Some if t => return Ok(Value::TRUE),
            Mode::Find | Mode::FindLast if t => return Ok(v),
            Mode::FindIndex | Mode::FindLastIndex if t => return Ok(kv),
            _ => {}
        }
    }
    Ok(match mode {
        Mode::Every => Value::TRUE,
        Mode::Some => Value::FALSE,
        Mode::FindIndex | Mode::FindLastIndex => Value::int(-1),
        _ => Value::UNDEFINED,
    })
}

fn fill(vm: &mut Vm, this: Value, a: &[Value], _: Value) -> JsResult<Value> {
    let (o, len) = validate(vm, this)?;
    let value = convert_for(vm, o, arg(a, 0))?;
    let start = relative(vm, arg(a, 1), len, 0)?;
    let end = relative(vm, arg(a, 2), len, len)?;
    let len = ta_length_checked(vm, o)?;
    for k in start..end.min(len) {
        write_raw(vm, o, k, value);
    }
    Ok(Value::object(o))
}

fn filter(vm: &mut Vm, this: Value, a: &[Value], _: Value) -> JsResult<Value> {
    let (o, len) = validate(vm, this)?;
    let f = callback(vm, arg(a, 0))?;
    let this_arg = arg(a, 1);
    let roots = vm.native_roots.len();
    let mut kept = Vec::new();
    for k in 0..len {
        let v = get_index(vm, o, k);
        let r = vm.call(f, this_arg, &[v, Value::from_usize(k), Value::object(o)])?;
        if vm.to_boolean(r) {
            vm.root(v);
            kept.push(v);
        }
    }
    let r = species_create(vm, o, kept.len())?;
    for (n, v) in kept.into_iter().enumerate() {
        set_index(vm, r, n, v)?;
    }
    vm.native_roots.truncate(roots);
    Ok(Value::object(r))
}

fn includes(vm: &mut Vm, this: Value, a: &[Value], _: Value) -> JsResult<Value> {
    let (o, len) = validate(vm, this)?;
    if len == 0 {
        return Ok(Value::FALSE);
    }
    let start = relative(vm, arg(a, 1), len, 0)?;
    let target = arg(a, 0);
    for k in start..len {
        let v = get_index(vm, o, k);
        if vm.same_value_zero(v, target) {
            return Ok(Value::TRUE);
        }
    }
    Ok(Value::FALSE)
}

/// The current length, or 0 once the array is detached / out of bounds.
fn current_len(vm: &mut Vm, o: ObjId) -> usize {
    ta_length_checked(vm, o).unwrap_or(0)
}

fn index_of(vm: &mut Vm, this: Value, a: &[Value], _: Value) -> JsResult<Value> {
    let (o, len) = validate(vm, this)?;
    if len == 0 {
        return Ok(Value::int(-1));
    }
    let start = relative(vm, arg(a, 1), len, 0)?;
    let target = arg(a, 0);
    // HasProperty: indexes the (possibly shrunk) array no longer has are
    // skipped.
    let now = current_len(vm, o).min(len);
    for k in start..now {
        let v = get_index(vm, o, k);
        if vm.strict_equals(v, target) {
            return Ok(Value::from_usize(k));
        }
    }
    Ok(Value::int(-1))
}

fn last_index_of(vm: &mut Vm, this: Value, a: &[Value], _: Value) -> JsResult<Value> {
    let (o, len) = validate(vm, this)?;
    if len == 0 {
        return Ok(Value::int(-1));
    }
    let n = if a.len() > 1 { vm.to_integer_or_infinity(a[1])? } else { len as f64 - 1.0 };
    if n == f64::NEG_INFINITY {
        return Ok(Value::int(-1));
    }
    let start = if n >= 0.0 { n.min(len as f64 - 1.0) } else { len as f64 + n };
    if start < 0.0 {
        return Ok(Value::int(-1));
    }
    let target = arg(a, 0);
    let now = current_len(vm, o);
    let mut k = start as usize;
    loop {
        if k < now {
            let v = get_index(vm, o, k);
            if vm.strict_equals(v, target) {
                return Ok(Value::from_usize(k));
            }
        }
        if k == 0 {
            return Ok(Value::int(-1));
        }
        k -= 1;
    }
}

fn join(vm: &mut Vm, this: Value, a: &[Value], _: Value) -> JsResult<Value> {
    let (o, len) = validate(vm, this)?;
    let sep = arg(a, 0);
    let sep: Vec<u16> = if sep.is_undefined() {
        alloc::vec![b',' as u16]
    } else {
        let s = vm.to_string(sep)?;
        vm.str(s).to_units()
    };
    let mut out: Vec<u16> = Vec::new();
    for k in 0..len {
        if k > 0 {
            out.extend_from_slice(&sep);
        }
        let v = get_index(vm, o, k);
        if !v.is_undefined() {
            let s = vm.to_string(v)?;
            out.extend(vm.str(s).units());
        }
    }
    Ok(vm.new_string_units(&out))
}

fn map(vm: &mut Vm, this: Value, a: &[Value], _: Value) -> JsResult<Value> {
    let (o, len) = validate(vm, this)?;
    let f = callback(vm, arg(a, 0))?;
    let this_arg = arg(a, 1);
    let r = species_create(vm, o, len)?;
    let roots = vm.native_roots.len();
    vm.root(Value::object(r));
    for k in 0..len {
        let v = get_index(vm, o, k);
        let m = vm.call(f, this_arg, &[v, Value::from_usize(k), Value::object(o)])?;
        set_index(vm, r, k, m)?;
    }
    vm.native_roots.truncate(roots);
    Ok(Value::object(r))
}

fn reduce(vm: &mut Vm, this: Value, a: &[Value], right: bool) -> JsResult<Value> {
    let (o, len) = validate(vm, this)?;
    let f = callback(vm, arg(a, 0))?;
    let mut i = 0;
    let mut acc = if a.len() > 1 {
        a[1]
    } else {
        if len == 0 {
            return Err(vm.type_error("Reduce of empty array with no initial value"));
        }
        i = 1;
        get_index(vm, o, if right { len - 1 } else { 0 })
    };
    while i < len {
        let k = if right { len - 1 - i } else { i };
        let v = get_index(vm, o, k);
        acc = vm.call(f, Value::UNDEFINED, &[acc, v, Value::from_usize(k), Value::object(o)])?;
        i += 1;
    }
    Ok(acc)
}

fn reverse(vm: &mut Vm, this: Value, _: &[Value], _: Value) -> JsResult<Value> {
    let (o, len) = validate(vm, this)?;
    let (mut lo, mut hi) = (0, len);
    while lo + 1 < hi {
        hi -= 1;
        let (x, y) = (read_raw(vm, o, lo), read_raw(vm, o, hi));
        write_raw(vm, o, lo, y);
        write_raw(vm, o, hi, x);
        lo += 1;
    }
    Ok(Value::object(o))
}

fn slice(vm: &mut Vm, this: Value, a: &[Value], _: Value) -> JsResult<Value> {
    let (o, len) = validate(vm, this)?;
    let start = relative(vm, arg(a, 0), len, 0)?;
    let end = relative(vm, arg(a, 1), len, len)?;
    let count = end.saturating_sub(start);
    let r = species_create(vm, o, count)?;
    if count > 0 {
        let len = ta_length_checked(vm, o)?;
        let end = end.min(len);
        let count = end.saturating_sub(start);
        if kind_of(vm, o) == kind_of(vm, r) {
            // Same element type: copy the bytes (NaN payloads included).
            let (sb, soff, size) = element_location(vm, o, start);
            let (tb, toff, _) = element_location(vm, r, 0);
            let src_limit = element_location(vm, o, 0).1 + len * size;
            let n = (count * size).min(src_limit.saturating_sub(soff));
            // Byte by byte, forward, as the spec does: when the species
            // constructor returned a view of the same buffer the overlap
            // shows.
            if sb == tb {
                if let Some(d) = vm.array_buffer_bytes_mut(Value::object(sb)) {
                    for i in 0..n {
                        if soff + i < d.len() && toff + i < d.len() {
                            d[toff + i] = d[soff + i];
                        }
                    }
                }
            } else {
                let bytes: Vec<u8> = vm
                    .array_buffer_bytes(Value::object(sb))
                    .and_then(|d| d.get(soff..(soff + n).min(d.len())).map(<[u8]>::to_vec))
                    .unwrap_or_default();
                if let Some(d) = vm.array_buffer_bytes_mut(Value::object(tb)) {
                    let end = (toff + bytes.len()).min(d.len());
                    if toff < end {
                        d[toff..end].copy_from_slice(&bytes[..end - toff]);
                    }
                }
            }
        } else {
            for (n, k) in (start..end).enumerate() {
                let v = get_index(vm, o, k);
                set_index(vm, r, n, v)?;
            }
        }
    }
    Ok(Value::object(r))
}

fn sort(vm: &mut Vm, this: Value, a: &[Value], _: Value) -> JsResult<Value> {
    let comparefn = arg(a, 0);
    check_comparefn(vm, comparefn)?;
    let (o, len) = validate(vm, this)?;
    let items: Vec<Value> = (0..len).map(|k| get_index(vm, o, k)).collect();
    let roots = vm.native_roots.len();
    for v in &items {
        vm.root(*v);
    }
    let items = sorted(vm, items, comparefn)?;
    for (k, v) in items.into_iter().enumerate() {
        set_index(vm, o, k, v)?;
    }
    vm.native_roots.truncate(roots);
    Ok(Value::object(o))
}

fn to_locale_string(vm: &mut Vm, this: Value, _: &[Value], _: Value) -> JsResult<Value> {
    let (o, len) = validate(vm, this)?;
    let mut out: Vec<u16> = Vec::new();
    for k in 0..len {
        if k > 0 {
            out.push(b',' as u16);
        }
        let v = get_index(vm, o, k);
        if !v.is_nullish() {
            let key = vm.key("toLocaleString");
            let f = vm.get_v(v, key)?;
            let f = callback(vm, f)?;
            let r = vm.call(f, v, &[])?;
            let s = vm.to_string(r)?;
            out.extend(vm.str(s).units());
        }
    }
    Ok(vm.new_string_units(&out))
}

fn to_reversed(vm: &mut Vm, this: Value, _: &[Value], _: Value) -> JsResult<Value> {
    let (o, len) = validate(vm, this)?;
    let r = create_same_type(vm, o, len)?;
    for k in 0..len {
        let v = get_index(vm, o, len - 1 - k);
        set_index(vm, r, k, v)?;
    }
    Ok(Value::object(r))
}

fn to_sorted(vm: &mut Vm, this: Value, a: &[Value], _: Value) -> JsResult<Value> {
    let comparefn = arg(a, 0);
    check_comparefn(vm, comparefn)?;
    let (o, len) = validate(vm, this)?;
    let r = create_same_type(vm, o, len)?;
    let roots = vm.native_roots.len();
    vm.root(Value::object(r));
    let items: Vec<Value> = (0..len).map(|k| get_index(vm, o, k)).collect();
    for v in &items {
        vm.root(*v);
    }
    let items = sorted(vm, items, comparefn)?;
    for (k, v) in items.into_iter().enumerate() {
        set_index(vm, r, k, v)?;
    }
    vm.native_roots.truncate(roots);
    Ok(Value::object(r))
}

fn with(vm: &mut Vm, this: Value, a: &[Value], _: Value) -> JsResult<Value> {
    let (o, len) = validate(vm, this)?;
    let rel = vm.to_integer_or_infinity(arg(a, 0))?;
    let actual = if rel >= 0.0 { rel } else { len as f64 + rel };
    let value = convert_for(vm, o, arg(a, 1))?;
    // IsValidIntegerIndex on the array as it is now.
    let now = current_len(vm, o);
    if !(actual >= 0.0 && actual < now as f64 && actual.fract() == 0.0) {
        return Err(vm.range_error("Invalid typed array index"));
    }
    let actual = actual as usize;
    let r = create_same_type(vm, o, len)?;
    for k in 0..len {
        let v = if k == actual { value } else { get_index(vm, o, k) };
        set_index(vm, r, k, v)?;
    }
    Ok(Value::object(r))
}
