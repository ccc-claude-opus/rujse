//! Number constructor and Number.prototype.

use alloc::string::String;
use alloc::vec::Vec;

use super::object::arg;
use crate::object::{attr, Class, Key};
use crate::value::Value;
use crate::vm::conv::number_to_string;
use crate::vm::{JsResult, Vm};

pub fn init(vm: &mut Vm) {
    let op = vm.intrinsics.object_prototype;
    let np = vm.alloc_object(Some(op), Class::Number(0.0));
    vm.intrinsics.number_prototype = np;
    let c = vm.constructor("Number", 1, number_ctor, np);
    vm.global_prop("Number", Value::object(c));
    let consts = [
        ("EPSILON", f64::EPSILON),
        ("MAX_SAFE_INTEGER", 9007199254740991.0),
        ("MIN_SAFE_INTEGER", -9007199254740991.0),
        ("MAX_VALUE", f64::MAX),
        ("MIN_VALUE", 5e-324),
        ("NaN", f64::NAN),
        ("NEGATIVE_INFINITY", f64::NEG_INFINITY),
        ("POSITIVE_INFINITY", f64::INFINITY),
    ];
    for (n, v) in consts {
        vm.prop(c, n, Value::number(v), attr::NONE);
    }
    vm.method(c, "isFinite", 1, |_, _, a, _| Ok(Value::bool(arg(a, 0).as_number().is_some_and(|n| n.is_finite()))));
    vm.method(c, "isNaN", 1, |_, _, a, _| Ok(Value::bool(arg(a, 0).as_number().is_some_and(|n| n.is_nan()))));
    vm.method(c, "isInteger", 1, |_, _, a, _| Ok(Value::bool(arg(a, 0).as_number().is_some_and(is_integral))));
    vm.method(c, "isSafeInteger", 1, |_, _, a, _| Ok(Value::bool(arg(a, 0).as_number().is_some_and(|n| is_integral(n) && n.abs() <= 9007199254740991.0))));
    // parseFloat/parseInt are the global functions (set up later by init_global).
    vm.method(np, "toString", 1, to_string);
    vm.method(np, "toLocaleString", 0, |vm, this, _, _| {
        let x = this_number(vm, this)?;
        Ok(vm.new_string(&number_to_string(x)))
    });
    vm.method(np, "valueOf", 0, |vm, this, _, _| Ok(Value::number(this_number(vm, this)?)));
    vm.method(np, "toFixed", 1, to_fixed);
    vm.method(np, "toExponential", 1, to_exponential);
    vm.method(np, "toPrecision", 1, to_precision);
}

/// Links Number.parseFloat/parseInt to the global functions.
pub fn link_globals(vm: &mut Vm) {
    let g = vm.global;
    let kn = vm.key("Number");
    let Ok(n) = vm.get(g, kn) else { return };
    let Some(n) = n.as_object() else { return };
    for name in ["parseFloat", "parseInt"] {
        let k = vm.key(name);
        if let Ok(f) = vm.get(g, k) {
            vm.define_data(n, k, f, attr::HIDDEN);
        }
    }
}

fn is_integral(n: f64) -> bool {
    n.is_finite() && crate::math::trunc(n) == n
}

fn number_ctor(vm: &mut Vm, _: Value, a: &[Value], nt: Value) -> JsResult<Value> {
    let n = if a.is_empty() {
        0.0
    } else {
        let p = vm.to_numeric(a[0])?;
        match p.as_bigint() {
            Some(b) => crate::builtins::bigint_impl::to_f64(vm.bigint(b)),
            None => p.num(),
        }
    };
    if nt.is_undefined() {
        return Ok(Value::number(n));
    }
    let p = vm.proto_from_ctor(nt, |i| i.number_prototype)?;
    Ok(Value::object(vm.alloc_object(Some(p), Class::Number(n))))
}

fn this_number(vm: &mut Vm, v: Value) -> JsResult<f64> {
    if let Some(n) = v.as_number() {
        return Ok(n);
    }
    if let Some(o) = v.as_object() {
        if let Class::Number(n) = vm.obj(o).class {
            return Ok(n);
        }
    }
    Err(vm.type_error("Number.prototype method called on incompatible receiver"))
}

fn to_string(vm: &mut Vm, this: Value, a: &[Value], _: Value) -> JsResult<Value> {
    let x = this_number(vm, this)?;
    let r = arg(a, 0);
    let radix = if r.is_undefined() { 10.0 } else { vm.to_integer_or_infinity(r)? };
    if !(2.0..=36.0).contains(&radix) {
        return Err(vm.range_error("toString() radix must be between 2 and 36"));
    }
    if radix == 10.0 {
        return Ok(vm.new_string(&number_to_string(x)));
    }
    Ok(vm.new_string(&to_radix_string(x, radix as u32)))
}

/// Number::toString(x, radix) for radix != 10 (shortest-enough digits,
/// following V8's DoubleToRadixCString).
pub fn to_radix_string(x: f64, radix: u32) -> String {
    if x.is_nan() {
        return "NaN".into();
    }
    if x.is_infinite() {
        return if x > 0.0 { "Infinity".into() } else { "-Infinity".into() };
    }
    if x == 0.0 {
        return "0".into();
    }
    let neg = x < 0.0;
    let value = x.abs();
    let digit = |d: u32| core::char::from_digit(d, radix).unwrap();
    let mut integer = crate::math::floor(value);
    let mut fraction = value - integer;
    // Precision of the last representable step.
    let delta0 = 0.5 * (next_up(value) - value);
    let mut delta = if delta0 > 0.0 { delta0 } else { f64::MIN_POSITIVE };
    let mut frac_digits: Vec<char> = Vec::new();
    if fraction >= delta {
        loop {
            fraction *= radix as f64;
            delta *= radix as f64;
            let d = crate::math::floor(fraction) as u32;
            frac_digits.push(digit(d));
            fraction -= d as f64;
            if fraction > 0.5 || (fraction == 0.5 && (d & 1) == 1) {
                if fraction + delta > 1.0 {
                    // Round up and propagate.
                    loop {
                        match frac_digits.pop() {
                            None => {
                                integer += 1.0;
                                break;
                            }
                            Some(c) => {
                                let v = c.to_digit(radix).unwrap() + 1;
                                if v < radix {
                                    frac_digits.push(digit(v));
                                    break;
                                }
                            }
                        }
                    }
                    break;
                }
            }
            if fraction < delta {
                break;
            }
        }
    }
    // Integer part: exact for values below 2^53; above, digits beyond the
    // precision are emitted as zeros by repeated division.
    let mut int_digits: Vec<char> = Vec::new();
    let mut i = integer;
    while i >= 1.0 {
        let r = crate::math::fmod(i, radix as f64);
        int_digits.push(digit(r as u32));
        i = crate::math::floor((i - r) / radix as f64);
    }
    if int_digits.is_empty() {
        int_digits.push('0');
    }
    let mut out = String::new();
    if neg {
        out.push('-');
    }
    out.extend(int_digits.iter().rev());
    if !frac_digits.is_empty() {
        out.push('.');
        out.extend(frac_digits.iter());
    }
    out
}

fn next_up(x: f64) -> f64 {
    if x.is_nan() || x == f64::INFINITY {
        return x;
    }
    if x == 0.0 {
        return f64::from_bits(1);
    }
    let b = x.to_bits();
    f64::from_bits(if x > 0.0 { b + 1 } else { b - 1 })
}

/// Exact decimal digits of |x| and its exponent: x = 0.d1d2d3… × 10^exp.
fn exact_digits(x: f64) -> (Vec<u8>, i32) {
    // `{:.800e}` prints the exact binary value (it has at most 767
    // significant digits), so no rounding happens here.
    let s = alloc::format!("{:.800e}", x.abs());
    let (m, e) = s.split_once('e').unwrap();
    let mut digits: Vec<u8> = m.bytes().filter(|b| b.is_ascii_digit()).map(|b| b - b'0').collect();
    while digits.len() > 1 && *digits.last().unwrap() == 0 {
        digits.pop();
    }
    (digits, e.parse::<i32>().unwrap() + 1)
}

/// Rounds an exact digit string to `n` significant digits, ties away from
/// zero ("pick the larger n"). Returns the digits and the new exponent.
fn round_digits(digits: &[u8], exp: i32, n: usize) -> (Vec<u8>, i32) {
    let mut out: Vec<u8> = digits.iter().take(n).copied().collect();
    while out.len() < n {
        out.push(0);
    }
    let round_up = digits.get(n).is_some_and(|&d| d >= 5);
    let mut exp = exp;
    if round_up {
        let mut i = n;
        loop {
            if i == 0 {
                out.insert(0, 1);
                out.truncate(n.max(1));
                exp += 1;
                break;
            }
            i -= 1;
            if out[i] == 9 {
                out[i] = 0;
            } else {
                out[i] += 1;
                break;
            }
        }
    }
    (out, exp)
}

fn to_fixed(vm: &mut Vm, this: Value, a: &[Value], _: Value) -> JsResult<Value> {
    let x = this_number(vm, this)?;
    let f = vm.to_integer_or_infinity(arg(a, 0))?;
    if !f.is_finite() || !(0.0..=100.0).contains(&f) {
        return Err(vm.range_error("toFixed() digits argument must be between 0 and 100"));
    }
    if !x.is_finite() {
        return Ok(vm.new_string(&number_to_string(x)));
    }
    if x.abs() >= 1e21 {
        return Ok(vm.new_string(&number_to_string(x)));
    }
    let f = f as i32;
    // n = round(|x| × 10^f), ties away from zero, as decimal digits.
    let (digits, exp) = exact_digits(x);
    let keep = exp + f;
    let mut n: String = if x == 0.0 || keep < 0 {
        String::from("0")
    } else if keep == 0 {
        String::from(if digits.first().is_some_and(|&d| d >= 5) { "1" } else { "0" })
    } else {
        let (ds, e) = round_digits(&digits, exp, keep as usize);
        let mut s: String = ds.iter().map(|d| (b'0' + d) as char).collect();
        for _ in 0..(e - exp) {
            s.push('0');
        }
        let t = s.trim_start_matches('0');
        if t.is_empty() { String::from("0") } else { String::from(t) }
    };
    if f > 0 {
        while n.len() < f as usize + 1 {
            n.insert(0, '0');
        }
        let at = n.len() - f as usize;
        n.insert(at, '.');
    }
    if x < 0.0 {
        n.insert(0, '-');
    }
    Ok(vm.new_string(&n))
}

fn to_exponential(vm: &mut Vm, this: Value, a: &[Value], _: Value) -> JsResult<Value> {
    let x = this_number(vm, this)?;
    let fv = arg(a, 0);
    let f = vm.to_integer_or_infinity(fv)?;
    if !x.is_finite() {
        return Ok(vm.new_string(&number_to_string(x)));
    }
    if !f.is_finite() || !(0.0..=100.0).contains(&f) {
        return Err(vm.range_error("toExponential() argument must be between 0 and 100"));
    }
    let mut out = String::new();
    if x < 0.0 {
        out.push('-');
    }
    let (ds, e) = if x == 0.0 {
        (alloc::vec![0u8; if fv.is_undefined() { 1 } else { f as usize + 1 }], 1)
    } else if fv.is_undefined() {
        // As many digits as needed to represent x uniquely.
        let s = alloc::format!("{:e}", x.abs());
        let (m, e) = s.split_once('e').unwrap();
        let d: Vec<u8> = m.bytes().filter(|b| b.is_ascii_digit()).map(|b| b - b'0').collect();
        (d, e.parse::<i32>().unwrap() + 1)
    } else {
        let (digits, exp) = exact_digits(x);
        round_digits(&digits, exp, f as usize + 1)
    };
    out.push((b'0' + ds[0]) as char);
    if ds.len() > 1 {
        out.push('.');
        for d in &ds[1..] {
            out.push((b'0' + d) as char);
        }
    }
    let ee = if x == 0.0 { 0 } else { e - 1 };
    out.push('e');
    out.push(if ee >= 0 { '+' } else { '-' });
    out.push_str(&alloc::format!("{}", ee.abs()));
    Ok(vm.new_string(&out))
}

fn to_precision(vm: &mut Vm, this: Value, a: &[Value], _: Value) -> JsResult<Value> {
    let x = this_number(vm, this)?;
    let pv = arg(a, 0);
    if pv.is_undefined() {
        return Ok(vm.new_string(&number_to_string(x)));
    }
    let p = vm.to_integer_or_infinity(pv)?;
    if !x.is_finite() {
        return Ok(vm.new_string(&number_to_string(x)));
    }
    if !p.is_finite() || !(1.0..=100.0).contains(&p) {
        return Err(vm.range_error("toPrecision() argument must be between 1 and 100"));
    }
    let p = p as i32;
    let mut out = String::new();
    if x < 0.0 {
        out.push('-');
    }
    let (ds, e) = if x == 0.0 {
        (alloc::vec![0u8; p as usize], 1)
    } else {
        let (digits, exp) = exact_digits(x);
        round_digits(&digits, exp, p as usize)
    };
    let ee = e - 1;
    let digit = |i: usize| (b'0' + ds[i]) as char;
    if ee < -6 || ee >= p {
        out.push(digit(0));
        if p > 1 {
            out.push('.');
            for i in 1..p as usize {
                out.push(digit(i));
            }
        }
        out.push('e');
        out.push(if ee >= 0 { '+' } else { '-' });
        out.push_str(&alloc::format!("{}", ee.abs()));
    } else if ee >= 0 {
        for i in 0..=(ee as usize) {
            out.push(digit(i));
        }
        if ((ee + 1) as usize) < p as usize {
            out.push('.');
            for i in (ee as usize + 1)..p as usize {
                out.push(digit(i));
            }
        }
    } else {
        out.push_str("0.");
        for _ in 0..(-(ee + 1)) {
            out.push('0');
        }
        for i in 0..p as usize {
            out.push(digit(i));
        }
    }
    Ok(vm.new_string(&out))
}

pub fn _k(_: Key) {}
