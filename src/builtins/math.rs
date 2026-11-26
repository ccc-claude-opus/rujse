//! Math.

use alloc::vec::Vec;

use super::object::arg;
use crate::math as m;
use crate::object::{attr, Key};
use crate::value::Value;
use crate::vm::{JsResult, Vm};

/// Number::exponentiate for the non-trivial cases.
pub fn pow(x: f64, y: f64) -> f64 {
    m::powf(x, y)
}

macro_rules! unary {
    ($vm:expr, $o:expr, $($name:expr => $f:expr),* $(,)?) => {$(
        $vm.method($o, $name, 1, |vm, _, a, _| {
            let x = vm.to_number(arg(a, 0))?;
            let f: fn(f64) -> f64 = $f;
            Ok(Value::number(f(x)))
        });
    )*};
}

pub fn init(vm: &mut Vm) {
    let o = vm.new_object();
    vm.intrinsics.math = o;
    vm.global_prop("Math", Value::object(o));
    let tag = vm.new_string("Math");
    let kt = Key::Sym(vm.intrinsics.sym_to_string_tag);
    vm.define_data(o, kt, tag, attr::CONFIGURABLE);
    for (n, v) in [
        ("E", core::f64::consts::E),
        ("LN10", core::f64::consts::LN_10),
        ("LN2", core::f64::consts::LN_2),
        ("LOG10E", core::f64::consts::LOG10_E),
        ("LOG2E", core::f64::consts::LOG2_E),
        ("PI", core::f64::consts::PI),
        ("SQRT1_2", core::f64::consts::FRAC_1_SQRT_2),
        ("SQRT2", core::f64::consts::SQRT_2),
    ] {
        vm.prop(o, n, Value::number(v), attr::NONE);
    }
    unary!(vm, o,
        "abs" => |x| x.abs(),
        "acos" => m::acos,
        "acosh" => m::acosh,
        "asin" => m::asin,
        "asinh" => |x| if x == 0.0 { x } else { m::asinh(x) },
        "atan" => m::atan,
        "atanh" => |x| if x == 0.0 { x } else { m::atanh(x) },
        "cbrt" => m::cbrt,
        "ceil" => m::ceil,
        "cos" => m::cos,
        "cosh" => m::cosh,
        "exp" => m::exp,
        "expm1" => |x| if x == 0.0 { x } else { m::exp_m1(x) },
        "floor" => m::floor,
        "fround" => |x| x as f32 as f64,
        "log" => m::ln,
        "log1p" => |x| if x == 0.0 { x } else { m::ln_1p(x) },
        "log10" => m::log10,
        "log2" => m::log2,
        "sign" => |x| if x.is_nan() || x == 0.0 { x } else if x > 0.0 { 1.0 } else { -1.0 },
        "sin" => |x| if x == 0.0 { x } else { m::sin(x) },
        "sinh" => |x| if x == 0.0 { x } else { m::sinh(x) },
        "sqrt" => m::sqrt,
        "tan" => |x| if x == 0.0 { x } else { m::tan(x) },
        "tanh" => |x| if x == 0.0 { x } else { m::tanh(x) },
        "trunc" => m::trunc,
        "round" => round,
        "f16round" => f16round,
    );
    vm.method(o, "atan2", 2, |vm, _, a, _| {
        let y = vm.to_number(arg(a, 0))?;
        let x = vm.to_number(arg(a, 1))?;
        Ok(Value::number(m::atan2(y, x)))
    });
    vm.method(o, "pow", 2, |vm, _, a, _| {
        let x = vm.to_number(arg(a, 0))?;
        let y = vm.to_number(arg(a, 1))?;
        Ok(Value::number(crate::vm::conv::js_pow(x, y)))
    });
    vm.method(o, "max", 2, |vm, _, a, _| minmax(vm, a, true));
    vm.method(o, "min", 2, |vm, _, a, _| minmax(vm, a, false));
    vm.method(o, "hypot", 2, |vm, _, a, _| {
        let mut xs = Vec::with_capacity(a.len());
        for &v in a {
            xs.push(vm.to_number(v)?);
        }
        if xs.iter().any(|x| x.is_infinite()) {
            return Ok(Value::number(f64::INFINITY));
        }
        if xs.iter().any(|x| x.is_nan()) {
            return Ok(Value::NAN);
        }
        let big = xs.iter().fold(0.0f64, |acc, x| acc.max(x.abs()));
        if big == 0.0 {
            return Ok(Value::int(0));
        }
        // Scaled sum with Kahan compensation.
        let (mut sum, mut comp) = (0.0f64, 0.0f64);
        for x in xs {
            let r = x / big;
            let y = r * r - comp;
            let t = sum + y;
            comp = (t - sum) - y;
            sum = t;
        }
        Ok(Value::number(m::sqrt(sum) * big))
    });
    vm.method(o, "random", 0, |vm, _, _, _| Ok(Value::number(vm.random())));
    vm.method(o, "clz32", 1, |vm, _, a, _| Ok(Value::int(vm.to_uint32(arg(a, 0))?.leading_zeros() as i32)));
    vm.method(o, "imul", 2, |vm, _, a, _| {
        let x = vm.to_int32(arg(a, 0))?;
        let y = vm.to_int32(arg(a, 1))?;
        Ok(Value::int(x.wrapping_mul(y)))
    });
    vm.method(o, "sumPrecise", 1, sum_precise);
}

fn round(x: f64) -> f64 {
    if !x.is_finite() || x == 0.0 {
        return x;
    }
    if x > 0.0 && x < 0.5 {
        return 0.0;
    }
    if x < 0.0 && x >= -0.5 {
        return -0.0;
    }
    let f = m::floor(x);
    if x - f >= 0.5 { f + 1.0 } else { f }
}

/// Math.f16round: round to IEEE binary16 (ties to even), back to f64.
fn f16round(x: f64) -> f64 {
    if x.is_nan() || x.is_infinite() || x == 0.0 {
        return x;
    }
    let sign = if x < 0.0 { -1.0 } else { 1.0 };
    let a = x.abs();
    if a >= 65520.0 {
        return sign * f64::INFINITY;
    }
    // Smallest subnormal step 2^-24; normal step depends on the exponent.
    let e = m::floor(m::log2(a)).max(-14.0);
    let step = m::powi(2.0, (e - 10.0) as i32);
    let q = a / step;
    let r = m::round_half_even(q);
    sign * r * step
}

fn minmax(vm: &mut Vm, a: &[Value], max: bool) -> JsResult<Value> {
    let mut nums = Vec::with_capacity(a.len());
    for &v in a {
        nums.push(vm.to_number(v)?);
    }
    let mut r = if max { f64::NEG_INFINITY } else { f64::INFINITY };
    for x in nums {
        if x.is_nan() {
            r = f64::NAN;
            continue;
        }
        if r.is_nan() {
            continue;
        }
        if max {
            if x > r || (x == 0.0 && r == 0.0 && !x.is_sign_negative()) {
                r = x;
            }
        } else if x < r || (x == 0.0 && r == 0.0 && x.is_sign_negative()) {
            r = x;
        }
    }
    Ok(Value::number(r))
}

/// Math.sumPrecise(items): the exact sum, rounded once (Shewchuk partials).
fn sum_precise(vm: &mut Vm, _: Value, a: &[Value], _: Value) -> JsResult<Value> {
    let items = arg(a, 0);
    if items.is_nullish() {
        return Err(vm.type_error("Math.sumPrecise requires an iterable"));
    }
    let (it, next) = vm.get_iterator(items)?;
    vm.root(it);
    vm.root(next);
    let mut partials: Vec<f64> = Vec::new();
    let (mut pos_inf, mut neg_inf, mut nan, mut all_neg_zero, mut any) = (false, false, false, true, false);
    let mut count: u64 = 0;
    while let Some(v) = vm.iterator_step(it, next)? {
        count += 1;
        if count >= 1 << 53 {
            let _ = vm.iterator_close(it);
            return Err(vm.range_error("too many values"));
        }
        let Some(x) = v.as_number() else {
            let _ = vm.iterator_close(it);
            return Err(vm.type_error("Math.sumPrecise: value is not a number"));
        };
        any = true;
        if x.is_nan() {
            nan = true;
            continue;
        }
        if x == f64::INFINITY {
            pos_inf = true;
            continue;
        }
        if x == f64::NEG_INFINITY {
            neg_inf = true;
            continue;
        }
        if !(x == 0.0 && x.is_sign_negative()) {
            all_neg_zero = false;
        }
        // Add x into the partials (two-sum), with overflow handled by
        // halving when the sum would overflow.
        let mut x = x;
        let mut i = 0;
        for j in 0..partials.len() {
            let mut y = partials[j];
            if x.abs() < y.abs() {
                core::mem::swap(&mut x, &mut y);
            }
            let hi = x + y;
            if hi.is_infinite() {
                // Exact overflow handling is rare; fall back to halves.
                let lo = (x / 2.0 + y / 2.0) * 2.0;
                x = lo;
                continue;
            }
            let lo = y - (hi - x);
            if lo != 0.0 {
                partials[i] = lo;
                i += 1;
            }
            x = hi;
        }
        partials.truncate(i);
        partials.push(x);
    }
    if nan || (pos_inf && neg_inf) {
        return Ok(Value::NAN);
    }
    if pos_inf {
        return Ok(Value::number(f64::INFINITY));
    }
    if neg_inf {
        return Ok(Value::number(f64::NEG_INFINITY));
    }
    if !any || all_neg_zero {
        return Ok(Value::number(-0.0));
    }
    // Round the partials (Python's msum final step).
    let mut n = partials.len();
    let mut hi = 0.0;
    if n > 0 {
        n -= 1;
        hi = partials[n];
        let mut lo = 0.0;
        while n > 0 {
            let x = hi;
            n -= 1;
            let y = partials[n];
            hi = x + y;
            let yr = hi - x;
            lo = y - yr;
            if lo != 0.0 {
                break;
            }
        }
        if n > 0 && ((lo < 0.0 && partials[n - 1] < 0.0) || (lo > 0.0 && partials[n - 1] > 0.0)) {
            let y = lo * 2.0;
            let x = hi + y;
            let yr = x - hi;
            if y == yr {
                hi = x;
            }
        }
    }
    Ok(Value::number(if hi == 0.0 { 0.0 } else { hi }))
}
