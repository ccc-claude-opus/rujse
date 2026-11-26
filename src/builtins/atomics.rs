//! Atomics (single agent: every operation is trivially atomic).

use super::buffer;
use super::object::arg;
use crate::object::{attr, Class, Key};
use crate::value::{ObjId, Value};
use crate::vm::{JsResult, Vm};

/// ValidateIntegerTypedArray + ValidateAtomicAccess: (array, index).
fn validate(vm: &mut Vm, ta: Value, index: Value, waitable: bool) -> JsResult<(ObjId, usize)> {
    let (o, len) = buffer::validate(vm, ta)?;
    let k = buffer::kind_of(vm, o);
    let ok = if waitable { k == 5 || k == 10 } else { matches!(k, 0 | 1 | 3 | 4 | 5 | 6 | 10 | 11) };
    if !ok {
        return Err(vm.type_error("Atomics operation on an invalid typed array type"));
    }
    let i = vm.to_index(index)?;
    if i as usize >= len {
        return Err(vm.range_error("Atomics access index out of range"));
    }
    Ok((o, i as usize))
}

fn revalidate(vm: &mut Vm, o: ObjId, i: usize) -> JsResult<()> {
    let len = buffer::ta_length_checked(vm, o)?;
    if i >= len {
        return Err(vm.range_error("Atomics access index out of range"));
    }
    Ok(())
}

/// Read-modify-write with `op(old, operand) -> new`; returns the old value.
fn rmw(vm: &mut Vm, a: &[Value], op: u8) -> JsResult<Value> {
    let (o, i) = validate(vm, arg(a, 0), arg(a, 1), false)?;
    let v = buffer::convert_for(vm, o, arg(a, 2))?;
    revalidate(vm, o, i)?;
    let old = buffer::read_raw(vm, o, i);
    let new = if op == 0 {
        v
    } else if old.is_bigint() {
        let bop = match op {
            1 => crate::ast::BinaryOp::Add,
            2 => crate::ast::BinaryOp::Sub,
            3 => crate::ast::BinaryOp::BitAnd,
            4 => crate::ast::BinaryOp::BitOr,
            _ => crate::ast::BinaryOp::BitXor,
        };
        crate::builtins::bigint::binary(vm, bop, old, v)?
    } else {
        let (x, y) = (old.num(), v.num());
        let (xi, yi) = (crate::vm::conv::to_int32_f(x) as i64, crate::vm::conv::to_int32_f(y) as i64);
        let xu = x as i64;
        Value::number(match op {
            1 => (xu as f64) + y,
            2 => (xu as f64) - y,
            3 => (xi & yi) as f64,
            4 => (xi | yi) as f64,
            _ => (xi ^ yi) as f64,
        })
    };
    buffer::write_raw(vm, o, i, new);
    Ok(old)
}

pub fn init(vm: &mut Vm) {
    let at = vm.new_object();
    vm.intrinsics.atomics = at;
    vm.global_prop("Atomics", Value::object(at));
    let tag = vm.new_string("Atomics");
    let kt = Key::Sym(vm.intrinsics.sym_to_string_tag);
    vm.define_data(at, kt, tag, attr::CONFIGURABLE);
    vm.method(at, "add", 3, |vm, _, a, _| rmw(vm, a, 1));
    vm.method(at, "sub", 3, |vm, _, a, _| rmw(vm, a, 2));
    vm.method(at, "and", 3, |vm, _, a, _| rmw(vm, a, 3));
    vm.method(at, "or", 3, |vm, _, a, _| rmw(vm, a, 4));
    vm.method(at, "xor", 3, |vm, _, a, _| rmw(vm, a, 5));
    vm.method(at, "exchange", 3, |vm, _, a, _| rmw(vm, a, 0));
    vm.method(at, "load", 2, |vm, _, a, _| {
        let (o, i) = validate(vm, arg(a, 0), arg(a, 1), false)?;
        revalidate(vm, o, i)?;
        Ok(buffer::read_raw(vm, o, i))
    });
    vm.method(at, "store", 3, |vm, _, a, _| {
        let (o, i) = validate(vm, arg(a, 0), arg(a, 1), false)?;
        let v = if buffer::is_bigint_kind(buffer::kind_of(vm, o)) {
            buffer::convert_for(vm, o, arg(a, 2))?
        } else {
            Value::number(vm.to_integer_or_infinity(arg(a, 2))?)
        };
        revalidate(vm, o, i)?;
        buffer::write_raw(vm, o, i, v);
        Ok(v)
    });
    vm.method(at, "compareExchange", 4, |vm, _, a, _| {
        let (o, i) = validate(vm, arg(a, 0), arg(a, 1), false)?;
        let expected = buffer::convert_for(vm, o, arg(a, 2))?;
        let replacement = buffer::convert_for(vm, o, arg(a, 3))?;
        revalidate(vm, o, i)?;
        let old = buffer::read_raw(vm, o, i);
        // Compare the stored representations.
        let (b, at, size) = buffer::element_location(vm, o, i);
        let cur: alloc::vec::Vec<u8> = match &vm.obj(b).class {
            Class::ArrayBuffer(bd) => bd.data[at..at + size].to_vec(),
            _ => alloc::vec::Vec::new(),
        };
        buffer::write_raw(vm, o, i, expected);
        let exp: alloc::vec::Vec<u8> = match &vm.obj(b).class {
            Class::ArrayBuffer(bd) => bd.data[at..at + size].to_vec(),
            _ => alloc::vec::Vec::new(),
        };
        if cur == exp {
            buffer::write_raw(vm, o, i, replacement);
        } else {
            buffer::write_raw(vm, o, i, old);
        }
        Ok(old)
    });
    vm.method(at, "isLockFree", 1, |vm, _, a, _| {
        let n = vm.to_integer_or_infinity(arg(a, 0))?;
        Ok(Value::bool(n == 1.0 || n == 2.0 || n == 4.0 || n == 8.0))
    });
    vm.method(at, "wait", 4, |vm, _, a, _| {
        let (o, i) = validate(vm, arg(a, 0), arg(a, 1), true)?;
        if !buffer::is_shared(vm, o) {
            return Err(vm.type_error("Atomics.wait requires a shared typed array"));
        }
        let v = buffer::convert_for(vm, o, arg(a, 2))?;
        let t = vm.to_number(arg(a, 3))?;
        let _ = if t.is_nan() { f64::INFINITY } else { t.max(0.0) };
        let cur = buffer::read_raw(vm, o, i);
        if !vm.strict_equals(cur, v) {
            return Ok(vm.new_string("not-equal"));
        }
        // A single agent: nobody can notify; report a timeout.
        Ok(vm.new_string("timed-out"))
    });
    vm.method(at, "notify", 3, |vm, _, a, _| {
        let (o, _i) = validate(vm, arg(a, 0), arg(a, 1), true)?;
        if !arg(a, 2).is_undefined() {
            let c = vm.to_integer_or_infinity(arg(a, 2))?;
            let _ = c.max(0.0);
        }
        let _ = o;
        Ok(Value::int(0))
    });
    vm.method(at, "pause", 0, |vm, _, a, _| {
        let n = arg(a, 0);
        if !n.is_undefined() && !(n.as_number().is_some_and(|x| x.is_finite() && crate::math::trunc(x) == x)) {
            return Err(vm.type_error("Atomics.pause argument must be an integer"));
        }
        Ok(Value::UNDEFINED)
    });
}
