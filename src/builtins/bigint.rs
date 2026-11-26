//! BigInt: arbitrary-precision integers (sign + magnitude in base 2^32).

use alloc::string::String;
use alloc::vec::Vec;
use core::cmp::Ordering;

use crate::ast::BinaryOp;
use crate::vm::{JsResult, Vm};
use crate::value::Value;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BigInt {
    /// true = negative (never for zero).
    pub neg: bool,
    /// Little-endian 32-bit limbs, no trailing zeros (zero = empty).
    pub mag: Vec<u32>,
}

impl BigInt {
    pub fn zero() -> BigInt {
        BigInt { neg: false, mag: Vec::new() }
    }

    pub fn is_zero(&self) -> bool {
        self.mag.is_empty()
    }

    pub fn from_i64(v: i64) -> BigInt {
        let neg = v < 0;
        let mut m = v.unsigned_abs();
        let mut mag = Vec::new();
        while m != 0 {
            mag.push(m as u32);
            m >>= 32;
        }
        BigInt { neg, mag }
    }

    /// Parses StringIntegerLiteral (for BigInt(string) and comparisons).
    pub fn parse_string(s: &str) -> Option<BigInt> {
        crate::builtins::bigint_impl::parse_string(s)
    }

    pub fn to_string_radix(&self, radix: u32) -> String {
        crate::builtins::bigint_impl::to_string_radix(self, radix)
    }

    /// Compares with a Number (None for NaN).
    pub fn cmp_f64(&self, f: f64) -> Option<Ordering> {
        crate::builtins::bigint_impl::cmp_f64(self, f)
    }
}

impl PartialOrd for BigInt {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(crate::builtins::bigint_impl::cmp(self, other))
    }
}

pub fn alloc(vm: &mut Vm, b: BigInt) -> Value {
    vm.alloc_bytes += 32 + b.mag.len() * 4;
    Value::bigint(crate::value::BigId(vm.bigints.alloc(b)))
}

pub fn from_literal(vm: &mut Vm, digits: &str) -> JsResult<Value> {
    let b = BigInt::parse_string(digits).unwrap_or_else(BigInt::zero);
    Ok(alloc(vm, b))
}

pub fn binary(vm: &mut Vm, op: BinaryOp, a: Value, b: Value) -> JsResult<Value> {
    crate::builtins::bigint_impl::binary(vm, op, a, b)
}

pub fn add_int(vm: &mut Vm, a: Value, i: i64) -> JsResult<Value> {
    let b = alloc(vm, BigInt::from_i64(i));
    binary(vm, BinaryOp::Add, a, b)
}

pub fn negate(vm: &mut Vm, a: Value) -> JsResult<Value> {
    let mut x = vm.bigint(a.as_bigint().unwrap()).clone();
    if !x.is_zero() {
        x.neg = !x.neg;
    }
    Ok(alloc(vm, x))
}

pub fn bitnot(vm: &mut Vm, a: Value) -> JsResult<Value> {
    // ~x = -x - 1
    let n = negate(vm, a)?;
    add_int(vm, n, -1)
}

pub fn init(vm: &mut Vm) {
    let op = vm.intrinsics.object_prototype;
    let bp = vm.alloc_object(Some(op), crate::object::Class::Ordinary);
    vm.intrinsics.bigint_prototype = bp;
}
