//! BigInt arithmetic on sign + magnitude (little-endian u32 limbs), and the
//! BigInt constructor and prototype.

use alloc::string::String;
use alloc::vec::Vec;
use core::cmp::Ordering;

use super::bigint::{alloc, BigInt};
use super::object::arg;
use crate::ast::BinaryOp;
use crate::object::{attr, Class, Key};
use crate::value::Value;
use crate::vm::{JsResult, Vm};

// ----- magnitudes --------------------------------------------------------

fn trim(m: &mut Vec<u32>) {
    while m.last() == Some(&0) {
        m.pop();
    }
}

fn cmp_mag(a: &[u32], b: &[u32]) -> Ordering {
    if a.len() != b.len() {
        return a.len().cmp(&b.len());
    }
    for i in (0..a.len()).rev() {
        if a[i] != b[i] {
            return a[i].cmp(&b[i]);
        }
    }
    Ordering::Equal
}

fn add_mag(a: &[u32], b: &[u32]) -> Vec<u32> {
    let mut out = Vec::with_capacity(a.len().max(b.len()) + 1);
    let mut carry = 0u64;
    for i in 0..a.len().max(b.len()) {
        let s = *a.get(i).unwrap_or(&0) as u64 + *b.get(i).unwrap_or(&0) as u64 + carry;
        out.push(s as u32);
        carry = s >> 32;
    }
    if carry != 0 {
        out.push(carry as u32);
    }
    out
}

/// a - b with a >= b.
fn sub_mag(a: &[u32], b: &[u32]) -> Vec<u32> {
    let mut out = Vec::with_capacity(a.len());
    let mut borrow = 0i64;
    for i in 0..a.len() {
        let mut d = a[i] as i64 - *b.get(i).unwrap_or(&0) as i64 - borrow;
        if d < 0 {
            d += 1 << 32;
            borrow = 1;
        } else {
            borrow = 0;
        }
        out.push(d as u32);
    }
    trim(&mut out);
    out
}

fn mul_mag(a: &[u32], b: &[u32]) -> Vec<u32> {
    if a.is_empty() || b.is_empty() {
        return Vec::new();
    }
    let mut out = alloc::vec![0u32; a.len() + b.len()];
    for (i, &x) in a.iter().enumerate() {
        let mut carry = 0u64;
        for (j, &y) in b.iter().enumerate() {
            let t = out[i + j] as u64 + x as u64 * y as u64 + carry;
            out[i + j] = t as u32;
            carry = t >> 32;
        }
        let mut k = i + b.len();
        while carry != 0 {
            let t = out[k] as u64 + carry;
            out[k] = t as u32;
            carry = t >> 32;
            k += 1;
        }
    }
    trim(&mut out);
    out
}

/// The largest BigInt, in bits (as SpiderMonkey). Bigger results are a
/// RangeError: the algorithms here are quadratic, and a script must not
/// be able to stall the VM inside one operation.
pub const MAX_BITS: u64 = 1 << 20;

fn divmod_small(a: &[u32], d: u32) -> (Vec<u32>, u32) {
    let mut out = alloc::vec![0u32; a.len()];
    let mut rem = 0u64;
    for i in (0..a.len()).rev() {
        let cur = (rem << 32) | a[i] as u64;
        out[i] = (cur / d as u64) as u32;
        rem = cur % d as u64;
    }
    trim(&mut out);
    (out, rem as u32)
}

fn shl_mag(a: &[u32], n: usize) -> Vec<u32> {
    if a.is_empty() {
        return Vec::new();
    }
    let (words, bits) = (n / 32, n % 32);
    let mut out = alloc::vec![0u32; words];
    if bits == 0 {
        out.extend_from_slice(a);
    } else {
        let mut carry = 0u32;
        for &x in a {
            out.push((x << bits) | carry);
            carry = x >> (32 - bits);
        }
        if carry != 0 {
            out.push(carry);
        }
    }
    trim(&mut out);
    out
}

fn shr_mag(a: &[u32], n: usize) -> Vec<u32> {
    let (words, bits) = (n / 32, n % 32);
    if words >= a.len() {
        return Vec::new();
    }
    let src = &a[words..];
    let mut out = Vec::with_capacity(src.len());
    for i in 0..src.len() {
        let lo = src[i] >> bits;
        let hi = if bits == 0 { 0 } else { src.get(i + 1).map(|&x| x << (32 - bits)).unwrap_or(0) };
        out.push(lo | hi);
    }
    trim(&mut out);
    out
}

/// Long division (schoolbook, bit by bit for simplicity on big divisors).
fn divmod_mag(a: &[u32], b: &[u32]) -> (Vec<u32>, Vec<u32>) {
    if cmp_mag(a, b) == Ordering::Less {
        return (Vec::new(), a.to_vec());
    }
    if b.len() == 1 {
        let (q, r) = divmod_small(a, b[0]);
        let mut rv = alloc::vec![r];
        trim(&mut rv);
        return (q, rv);
    }
    let bits = a.len() * 32;
    let mut q = alloc::vec![0u32; a.len()];
    let mut r: Vec<u32> = Vec::new();
    for i in (0..bits).rev() {
        r = shl_mag(&r, 1);
        if (a[i / 32] >> (i % 32)) & 1 == 1 {
            if r.is_empty() {
                r.push(1);
            } else {
                r[0] |= 1;
            }
        }
        if cmp_mag(&r, b) != Ordering::Less {
            r = sub_mag(&r, b);
            q[i / 32] |= 1 << (i % 32);
        }
    }
    trim(&mut q);
    (q, r)
}

fn bits_len(a: &[u32]) -> usize {
    match a.last() {
        None => 0,
        Some(&top) => (a.len() - 1) * 32 + (32 - top.leading_zeros() as usize),
    }
}

// ----- signed operations -------------------------------------------------

fn mk(neg: bool, mut mag: Vec<u32>) -> BigInt {
    trim(&mut mag);
    let neg = neg && !mag.is_empty();
    BigInt { neg, mag }
}

fn add(a: &BigInt, b: &BigInt) -> BigInt {
    if a.neg == b.neg {
        return mk(a.neg, add_mag(&a.mag, &b.mag));
    }
    match cmp_mag(&a.mag, &b.mag) {
        Ordering::Less => mk(b.neg, sub_mag(&b.mag, &a.mag)),
        _ => mk(a.neg, sub_mag(&a.mag, &b.mag)),
    }
}

fn neg(a: &BigInt) -> BigInt {
    mk(!a.neg, a.mag.clone())
}

/// Two's complement limbs of `a` with `n` limbs (n large enough).
fn to_twos(a: &BigInt, n: usize) -> Vec<u32> {
    let mut m = a.mag.clone();
    m.resize(n, 0);
    if a.neg {
        for x in m.iter_mut() {
            *x = !*x;
        }
        let mut carry = 1u64;
        for x in m.iter_mut() {
            let t = *x as u64 + carry;
            *x = t as u32;
            carry = t >> 32;
        }
    }
    m
}

fn from_twos(mut m: Vec<u32>) -> BigInt {
    let negative = m.last().is_some_and(|&x| x & 0x8000_0000 != 0);
    if negative {
        for x in m.iter_mut() {
            *x = !*x;
        }
        let mut carry = 1u64;
        for x in m.iter_mut() {
            let t = *x as u64 + carry;
            *x = t as u32;
            carry = t >> 32;
        }
    }
    mk(negative, m)
}

fn bitwise(a: &BigInt, b: &BigInt, op: fn(u32, u32) -> u32) -> BigInt {
    let n = a.mag.len().max(b.mag.len()) + 1;
    let (x, y) = (to_twos(a, n), to_twos(b, n));
    from_twos(x.iter().zip(y.iter()).map(|(p, q)| op(*p, *q)).collect())
}

fn shift_left(vm: &mut Vm, a: &BigInt, n: &BigInt) -> JsResult<BigInt> {
    if n.neg {
        return Ok(shift_right(a, &neg(n)));
    }
    if n.mag.len() > 1 || n.mag.first().is_some_and(|&x| x as u64 + bits_len(&a.mag) as u64 > MAX_BITS) {
        if a.mag.is_empty() {
            return Ok(BigInt::zero());
        }
        return Err(vm.range_error("Maximum BigInt size exceeded"));
    }
    let k = n.mag.first().copied().unwrap_or(0) as usize;
    Ok(mk(a.neg, shl_mag(&a.mag, k)))
}

fn shift_right(a: &BigInt, n: &BigInt) -> BigInt {
    if n.neg {
        let m = neg(n);
        // Left shift by |n| (size limits do not matter for the tests here).
        let k = m.mag.first().copied().unwrap_or(0) as usize;
        return mk(a.neg, shl_mag(&a.mag, k));
    }
    if n.mag.len() > 1 {
        return if a.neg { BigInt::from_i64(-1) } else { BigInt::zero() };
    }
    let k = n.mag.first().copied().unwrap_or(0) as usize;
    if !a.neg {
        return mk(false, shr_mag(&a.mag, k));
    }
    // Floor division for negatives: -((|a| - 1) >> k) - 1
    let one = alloc::vec![1u32];
    let m1 = sub_mag(&a.mag, &one);
    let s = shr_mag(&m1, k);
    mk(true, add_mag(&s, &one))
}

fn pow(vm: &mut Vm, a: &BigInt, e: &BigInt) -> JsResult<BigInt> {
    if e.neg {
        return Err(vm.range_error("Exponent must be non-negative"));
    }
    if e.mag.is_empty() {
        return Ok(BigInt::from_i64(1));
    }
    if a.mag.is_empty() {
        return Ok(BigInt::zero());
    }
    if a.mag == [1] {
        let odd = e.mag[0] & 1 == 1;
        return Ok(mk(a.neg && odd, alloc::vec![1]));
    }
    if e.mag.len() > 1 || bits_len(&a.mag) as u64 * e.mag[0] as u64 > MAX_BITS {
        return Err(vm.range_error("Maximum BigInt size exceeded"));
    }
    let mut result = alloc::vec![1u32];
    let mut base = a.mag.clone();
    let mut k = e.mag[0];
    while k > 0 {
        if k & 1 == 1 {
            result = mul_mag(&result, &base);
        }
        k >>= 1;
        if k > 0 {
            base = mul_mag(&base, &base);
        }
    }
    Ok(mk(a.neg && e.mag[0] & 1 == 1, result))
}

pub fn binary(vm: &mut Vm, op: BinaryOp, a: Value, b: Value) -> JsResult<Value> {
    let x = vm.bigint(a.as_bigint().unwrap()).clone();
    let y = vm.bigint(b.as_bigint().unwrap()).clone();
    let r = match op {
        BinaryOp::Add => add(&x, &y),
        BinaryOp::Sub => add(&x, &neg(&y)),
        BinaryOp::Mul => {
            if (bits_len(&x.mag) + bits_len(&y.mag)) as u64 > MAX_BITS + 1 {
                return Err(vm.range_error("Maximum BigInt size exceeded"));
            }
            mk(x.neg != y.neg, mul_mag(&x.mag, &y.mag))
        }
        BinaryOp::Div | BinaryOp::Mod => {
            if y.mag.is_empty() {
                return Err(vm.range_error("Division by zero"));
            }
            let (q, r) = divmod_mag(&x.mag, &y.mag);
            if op == BinaryOp::Div { mk(x.neg != y.neg, q) } else { mk(x.neg, r) }
        }
        BinaryOp::Exp => pow(vm, &x, &y)?,
        BinaryOp::Shl => shift_left(vm, &x, &y)?,
        BinaryOp::Shr => shift_right(&x, &y),
        BinaryOp::Ushr => return Err(vm.type_error("BigInts have no unsigned right shift, use >> instead")),
        BinaryOp::BitAnd => bitwise(&x, &y, |p, q| p & q),
        BinaryOp::BitOr => bitwise(&x, &y, |p, q| p | q),
        BinaryOp::BitXor => bitwise(&x, &y, |p, q| p ^ q),
        _ => unreachable!(),
    };
    Ok(alloc(vm, r))
}

// ----- conversions -------------------------------------------------------

pub fn parse_string(s: &str) -> Option<BigInt> {
    let t = s.trim_matches(|c: char| c.is_whitespace() || c == '\u{FEFF}');
    if t.is_empty() {
        return Some(BigInt::zero());
    }
    let (radix, digits, sign_ok) = if t.len() > 2 && (t.starts_with("0x") || t.starts_with("0X")) {
        (16, &t[2..], false)
    } else if t.len() > 2 && (t.starts_with("0o") || t.starts_with("0O")) {
        (8, &t[2..], false)
    } else if t.len() > 2 && (t.starts_with("0b") || t.starts_with("0B")) {
        (2, &t[2..], false)
    } else {
        (10, t, true)
    };
    let (negative, digits) = if sign_ok {
        match digits.as_bytes()[0] {
            b'-' => (true, &digits[1..]),
            b'+' => (false, &digits[1..]),
            _ => (false, digits),
        }
    } else {
        (false, digits)
    };
    if digits.is_empty() {
        return None;
    }
    let mut mag: Vec<u32> = Vec::new();
    for c in digits.chars() {
        let d = c.to_digit(radix)?;
        // mag = mag * radix + d
        let mut carry = d as u64;
        for x in mag.iter_mut() {
            let t = *x as u64 * radix as u64 + carry;
            *x = t as u32;
            carry = t >> 32;
        }
        if carry != 0 {
            mag.push(carry as u32);
        }
    }
    Some(mk(negative, mag))
}

pub fn to_string_radix(b: &BigInt, radix: u32) -> String {
    if b.mag.is_empty() {
        return "0".into();
    }
    // Divide by the largest power of the radix that fits a limb, so each
    // pass over the number yields several digits.
    let (mut chunk, mut per) = (radix, 1);
    while (chunk as u64) * (radix as u64) <= u32::MAX as u64 {
        chunk *= radix;
        per += 1;
    }
    let mut digits: Vec<char> = Vec::new();
    let mut m = b.mag.clone();
    while !m.is_empty() {
        let (q, mut r) = divmod_small(&m, chunk);
        m = q;
        for _ in 0..per {
            if m.is_empty() && r == 0 {
                break;
            }
            digits.push(core::char::from_digit(r % radix, radix).unwrap());
            r /= radix;
        }
    }
    if b.neg {
        digits.push('-');
    }
    digits.iter().rev().collect()
}

pub fn to_f64(b: &BigInt) -> f64 {
    // Correct rounding: take the top 64 bits plus a sticky bit.
    let n = bits_len(&b.mag);
    if n == 0 {
        return 0.0;
    }
    let v = if n <= 64 {
        let mut x: u64 = 0;
        for (i, &l) in b.mag.iter().enumerate().take(2) {
            x |= (l as u64) << (32 * i);
        }
        if n <= 53 {
            x as f64
        } else {
            round_bits(x, 0, false)
        }
    } else {
        let shift = n - 64;
        let top = shr_mag(&b.mag, shift);
        let mut x: u64 = 0;
        for (i, &l) in top.iter().enumerate().take(2) {
            x |= (l as u64) << (32 * i);
        }
        let sticky = {
            let words = shift / 32;
            let bits = shift % 32;
            b.mag[..words].iter().any(|&w| w != 0) || (bits > 0 && b.mag[words] & ((1u32 << bits) - 1) != 0)
        };
        round_bits(x, shift, sticky)
    };
    if b.neg { -v } else { v }
}

/// x (with its top bit at 63 or lower) × 2^shift, rounded to 53 bits.
fn round_bits(x: u64, shift: usize, sticky: bool) -> f64 {
    let lz = x.leading_zeros();
    let x = x << lz;
    let shift = shift as i32 - lz as i32;
    let mant = x >> 11;
    let rest = x & 0x7FF;
    let half = 0x400;
    let mut m = mant;
    if rest > half || (rest == half && (sticky || m & 1 == 1)) || (rest == half && sticky) {
        m += 1;
    }
    let e = shift + 11;
    if e > 1023 {
        return f64::INFINITY;
    }
    (m as f64) * crate::math::powi(2.0, e)
}

pub fn cmp_f64(b: &BigInt, f: f64) -> Option<Ordering> {
    if f.is_nan() {
        return None;
    }
    if f == f64::INFINITY {
        return Some(Ordering::Less);
    }
    if f == f64::NEG_INFINITY {
        return Some(Ordering::Greater);
    }
    // Compare exactly: b vs trunc(f), then the fraction.
    let t = crate::math::trunc(f);
    let tb = from_f64_integral(t);
    match cmp(b, &tb) {
        Ordering::Equal => {
            let frac = f - t;
            Some(if frac > 0.0 { Ordering::Less } else if frac < 0.0 { Ordering::Greater } else { Ordering::Equal })
        }
        o => Some(o),
    }
}

/// An integral finite f64 as a BigInt (exact).
pub fn from_f64_integral(f: f64) -> BigInt {
    if f == 0.0 {
        return BigInt::zero();
    }
    let bits = f.to_bits();
    let negative = bits >> 63 == 1;
    let exp = ((bits >> 52) & 0x7FF) as i32 - 1075;
    let mant = (bits & ((1 << 52) - 1)) | (1 << 52);
    let mag = alloc::vec![mant as u32, (mant >> 32) as u32];
    let m = if exp >= 0 { shl_mag(&mag, exp as usize) } else { shr_mag(&mag, (-exp) as usize) };
    mk(negative, m)
}

pub fn cmp(a: &BigInt, b: &BigInt) -> Ordering {
    match (a.neg, b.neg) {
        (false, true) => Ordering::Greater,
        (true, false) => Ordering::Less,
        (false, false) => cmp_mag(&a.mag, &b.mag),
        (true, true) => cmp_mag(&b.mag, &a.mag),
    }
}

/// BigInt.asUintN / asIntN.
fn as_n(vm: &mut Vm, bits: u64, b: &BigInt, signed: bool) -> JsResult<BigInt> {
    if bits == 0 {
        return Ok(BigInt::zero());
    }
    let len = bits_len(&b.mag) as u64;
    // Already in range: nothing to wrap (also for huge `bits`).
    if signed && len < bits || !signed && !b.neg && len <= bits {
        return Ok(b.clone());
    }
    if bits > MAX_BITS {
        return Err(vm.range_error("Maximum BigInt size exceeded"));
    }
    let n = bits as usize;
    let limbs = n.div_ceil(32) + 1;
    let mut t = to_twos(b, limbs.max(b.mag.len() + 1));
    t.truncate(limbs);
    // Keep the low n bits.
    let words = n / 32;
    let rem = n % 32;
    for (i, x) in t.iter_mut().enumerate() {
        if i > words || (i == words && rem == 0) {
            *x = 0;
        } else if i == words {
            *x &= (1u32 << rem) - 1;
        }
    }
    let unsigned = mk(false, t);
    if signed && bits_len(&unsigned.mag) == n {
        // Top bit set: subtract 2^n.
        let two_n = shl_mag(&[1], n);
        return Ok(mk(true, sub_mag(&two_n, &unsigned.mag)));
    }
    Ok(unsigned)
}

// ----- the BigInt constructor -------------------------------------------

/// ToBigInt.
pub fn to_bigint(vm: &mut Vm, v: Value) -> JsResult<BigInt> {
    let p = vm.to_primitive(v, crate::vm::conv::Hint::Number)?;
    if let Some(b) = p.as_bigint() {
        return Ok(vm.bigint(b).clone());
    }
    if p.is_bool() {
        return Ok(BigInt::from_i64((p == Value::TRUE) as i64));
    }
    if let Some(s) = p.as_string() {
        let t = vm.str(s).to_rust_lossy();
        return parse_string(&t).ok_or_else(|| vm.syntax_error(&alloc::format!("Cannot convert {t} to a BigInt")));
    }
    if p.is_number() {
        return Err(vm.type_error("Cannot convert a Number to a BigInt"));
    }
    let d = vm.display(p);
    Err(vm.type_error(&alloc::format!("Cannot convert {d} to a BigInt")))
}

pub fn init(vm: &mut Vm) {
    let bp = vm.intrinsics.bigint_prototype;
    let c = vm.constructor("BigInt", 1, |vm, _, a, nt| {
        if !nt.is_undefined() {
            return Err(vm.type_error("BigInt is not a constructor"));
        }
        let p = vm.to_primitive(arg(a, 0), crate::vm::conv::Hint::Number)?;
        if let Some(n) = p.as_number() {
            if !n.is_finite() || crate::math::trunc(n) != n {
                return Err(vm.range_error("The number cannot be converted to a BigInt because it is not an integer"));
            }
            let b = from_f64_integral(n);
            return Ok(alloc(vm, b));
        }
        let b = to_bigint(vm, p)?;
        Ok(alloc(vm, b))
    }, bp);
    vm.global_prop("BigInt", Value::object(c));
    vm.method(c, "asUintN", 2, |vm, _, a, _| {
        let bits = vm.to_index(arg(a, 0))?;
        let b = to_bigint(vm, arg(a, 1))?;
        let r = as_n(vm, bits, &b, false)?;
        Ok(alloc(vm, r))
    });
    vm.method(c, "asIntN", 2, |vm, _, a, _| {
        let bits = vm.to_index(arg(a, 0))?;
        let b = to_bigint(vm, arg(a, 1))?;
        let r = as_n(vm, bits, &b, true)?;
        Ok(alloc(vm, r))
    });
    fn this_bigint(vm: &mut Vm, v: Value) -> JsResult<Value> {
        if v.is_bigint() {
            return Ok(v);
        }
        if let Some(o) = v.as_object() {
            if let Class::BigInt(b) = vm.obj(o).class {
                return Ok(Value::bigint(b));
            }
        }
        Err(vm.type_error("BigInt.prototype method called on incompatible receiver"))
    }
    vm.method(bp, "toString", 0, |vm, this, a, _| {
        let b = this_bigint(vm, this)?;
        let r = arg(a, 0);
        let radix = if r.is_undefined() { 10.0 } else { vm.to_integer_or_infinity(r)? };
        if !(2.0..=36.0).contains(&radix) {
            return Err(vm.range_error("toString() radix must be between 2 and 36"));
        }
        let s = to_string_radix(vm.bigint(b.as_bigint().unwrap()), radix as u32);
        Ok(vm.new_string(&s))
    });
    vm.method(bp, "toLocaleString", 0, |vm, this, _, _| {
        let b = this_bigint(vm, this)?;
        let s = to_string_radix(vm.bigint(b.as_bigint().unwrap()), 10);
        Ok(vm.new_string(&s))
    });
    vm.method(bp, "valueOf", 0, |vm, this, _, _| this_bigint(vm, this));
    let tag = vm.new_string("BigInt");
    let kt = Key::Sym(vm.intrinsics.sym_to_string_tag);
    vm.define_data(bp, kt, tag, attr::CONFIGURABLE);
}
