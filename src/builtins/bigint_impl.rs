//! BigInt arithmetic (placeholder until the full implementation lands).

use alloc::string::String;
use core::cmp::Ordering;

use super::bigint::BigInt;
use crate::ast::BinaryOp;
use crate::value::Value;
use crate::vm::{JsResult, Vm};

pub fn parse_string(s: &str) -> Option<BigInt> {
    let t = s.trim();
    let (neg, d) = match t.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, t.strip_prefix('+').unwrap_or(t)),
    };
    if d.is_empty() || !d.bytes().all(|b| b.is_ascii_digit()) {
        return if t.is_empty() { Some(BigInt::zero()) } else { None };
    }
    let v: i64 = d.parse().ok()?;
    let mut b = BigInt::from_i64(v);
    b.neg = neg && !b.is_zero();
    Some(b)
}

fn to_i128(b: &BigInt) -> i128 {
    let mut v: i128 = 0;
    for (i, l) in b.mag.iter().enumerate().take(3) {
        v |= (*l as i128) << (32 * i);
    }
    if b.neg { -v } else { v }
}

pub fn to_string_radix(b: &BigInt, radix: u32) -> String {
    let mut v = to_i128(b);
    if v == 0 {
        return "0".into();
    }
    let neg = v < 0;
    let mut out = alloc::vec::Vec::new();
    while v != 0 {
        let d = (v % radix as i128).unsigned_abs() as u32;
        out.push(core::char::from_digit(d, radix).unwrap());
        v /= radix as i128;
    }
    if neg {
        out.push('-');
    }
    out.iter().rev().collect()
}

pub fn cmp_f64(b: &BigInt, f: f64) -> Option<Ordering> {
    if f.is_nan() {
        return None;
    }
    (to_i128(b) as f64).partial_cmp(&f)
}

pub fn cmp(a: &BigInt, b: &BigInt) -> Ordering {
    to_i128(a).cmp(&to_i128(b))
}

pub fn binary(vm: &mut Vm, op: BinaryOp, a: Value, b: Value) -> JsResult<Value> {
    let x = to_i128(vm.bigint(a.as_bigint().unwrap()));
    let y = to_i128(vm.bigint(b.as_bigint().unwrap()));
    let r = match op {
        BinaryOp::Add => x + y,
        BinaryOp::Sub => x - y,
        BinaryOp::Mul => x * y,
        _ => return Err(vm.type_error("BigInt operation not implemented yet")),
    };
    let neg = r < 0;
    let mut m = r.unsigned_abs();
    let mut mag = alloc::vec::Vec::new();
    while m != 0 {
        mag.push(m as u32);
        m >>= 32;
    }
    Ok(super::bigint::alloc(vm, BigInt { neg, mag }))
}
