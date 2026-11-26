//! Type conversions, equality and the operators of ECMA-262 §7.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use super::{JsResult, Vm};
use crate::object::{Class, Key};
use crate::string::JsStr;
use crate::value::{Kind, ObjId, StrId, Value};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Hint {
    Default,
    Number,
    String,
}

/// Number::toString(10).
pub fn number_to_string(x: f64) -> String {
    if x.is_nan() {
        return "NaN".into();
    }
    if x == 0.0 {
        return "0".into();
    }
    if x.is_infinite() {
        return if x > 0.0 { "Infinity".into() } else { "-Infinity".into() };
    }
    if x < 0.0 {
        let mut s = String::from("-");
        s.push_str(&number_to_string(-x));
        return s;
    }
    if crate::math::fract(x) == 0.0 && x < 1e21 && x < 9007199254740992.0 {
        return format!("{}", x as u64);
    }
    // Shortest round-trip digits and exponent.
    let e = format!("{x:e}");
    let (mant, exp) = e.split_once('e').unwrap();
    let digits: String = mant.chars().filter(|c| *c != '.').collect();
    let k = digits.len() as i32;
    let n = exp.parse::<i32>().unwrap() + 1;
    let mut out = String::new();
    if k <= n && n <= 21 {
        out.push_str(&digits);
        for _ in 0..(n - k) {
            out.push('0');
        }
    } else if 0 < n && n <= 21 {
        out.push_str(&digits[..n as usize]);
        out.push('.');
        out.push_str(&digits[n as usize..]);
    } else if -6 < n && n <= 0 {
        out.push_str("0.");
        for _ in 0..(-n) {
            out.push('0');
        }
        out.push_str(&digits);
    } else {
        out.push_str(&digits[..1]);
        if k > 1 {
            out.push('.');
            out.push_str(&digits[1..]);
        }
        out.push('e');
        out.push(if n - 1 >= 0 { '+' } else { '-' });
        out.push_str(&format!("{}", (n - 1).abs()));
    }
    out
}

pub fn is_js_space(c: u16) -> bool {
    matches!(c, 0x09 | 0x0A | 0x0B | 0x0C | 0x0D | 0x20 | 0xA0 | 0x1680 | 0x2000..=0x200A | 0x2028 | 0x2029 | 0x202F | 0x205F | 0x3000 | 0xFEFF)
}

/// StringToNumber.
pub fn string_to_number(s: &JsStr) -> f64 {
    let units: Vec<u16> = s.units().collect();
    let mut a = 0;
    let mut b = units.len();
    while a < b && is_js_space(units[a]) {
        a += 1;
    }
    while b > a && is_js_space(units[b - 1]) {
        b -= 1;
    }
    let t = &units[a..b];
    if t.is_empty() {
        return 0.0;
    }
    if t.iter().any(|&c| c > 0x7F) {
        return f64::NAN;
    }
    let st: String = t.iter().map(|&c| c as u8 as char).collect();
    parse_numeric_literal(&st).unwrap_or(f64::NAN)
}

/// StrNumericLiteral (already trimmed, ASCII).
pub fn parse_numeric_literal(st: &str) -> Option<f64> {
    let b = st.as_bytes();
    if b.len() > 2 && b[0] == b'0' {
        let radix = match b[1] {
            b'x' | b'X' => 16,
            b'o' | b'O' => 8,
            b'b' | b'B' => 2,
            _ => 0,
        };
        if radix != 0 {
            let mut v = 0f64;
            for &c in &b[2..] {
                let d = (c as char).to_digit(radix)?;
                v = v * radix as f64 + d as f64;
            }
            if v >= 9007199254740992.0 {
                return Some(parse_radix_exact(&st[2..], radix));
            }
            return Some(v);
        }
    }
    let (sign, rest) = match b[0] {
        b'+' => (1.0, &st[1..]),
        b'-' => (-1.0, &st[1..]),
        _ => (1.0, st),
    };
    if rest == "Infinity" {
        return Some(sign * f64::INFINITY);
    }
    // StrUnsignedDecimalLiteral: digits [. digits] [e[+-]digits] or . digits …
    let rb = rest.as_bytes();
    let mut i = 0;
    let mut int_digits = 0;
    while i < rb.len() && rb[i].is_ascii_digit() {
        i += 1;
        int_digits += 1;
    }
    let mut frac_digits = 0;
    if i < rb.len() && rb[i] == b'.' {
        i += 1;
        while i < rb.len() && rb[i].is_ascii_digit() {
            i += 1;
            frac_digits += 1;
        }
    }
    if int_digits + frac_digits == 0 {
        return None;
    }
    if i < rb.len() && (rb[i] == b'e' || rb[i] == b'E') {
        i += 1;
        if i < rb.len() && (rb[i] == b'+' || rb[i] == b'-') {
            i += 1;
        }
        let s = i;
        while i < rb.len() && rb[i].is_ascii_digit() {
            i += 1;
        }
        if i == s {
            return None;
        }
    }
    if i != rb.len() {
        return None;
    }
    let v: f64 = rest.parse().ok()?;
    Some(sign * v)
}

/// Exact (correctly rounded) value of a long radix-2^k digit string.
fn parse_radix_exact(digits: &str, radix: u32) -> f64 {
    let bits_per = match radix {
        16 => 4,
        8 => 3,
        _ => 1,
    };
    let mut bits: Vec<u8> = Vec::new();
    for c in digits.chars() {
        let d = c.to_digit(radix).unwrap();
        for j in (0..bits_per).rev() {
            bits.push(((d >> j) & 1) as u8);
        }
    }
    while bits.first() == Some(&0) {
        bits.remove(0);
    }
    if bits.len() <= 53 {
        return bits.iter().fold(0f64, |a, &b| a * 2.0 + b as f64);
    }
    let mut m: u64 = 0;
    for &b in &bits[..53] {
        m = m * 2 + b as u64;
    }
    let rest = &bits[53..];
    let half = rest[0] == 1;
    let sticky = rest[1..].iter().any(|&b| b == 1);
    if half && (sticky || m & 1 == 1) {
        m += 1;
    }
    let exp = rest.len() as i32;
    (m as f64) * crate::math::powi(2.0, exp)
}

pub fn to_int32_f(n: f64) -> i32 {
    if !n.is_finite() {
        return 0;
    }
    let t = crate::math::trunc(n);
    let m = crate::math::rem_euclid(t, 4294967296.0);
    m as u32 as i32
}

pub fn to_uint32_f(n: f64) -> u32 {
    to_int32_f(n) as u32
}

impl Vm {
    pub fn to_boolean(&self, v: Value) -> bool {
        match v.kind() {
            Kind::Undefined | Kind::Null | Kind::Hole | Kind::Uninit => false,
            Kind::Bool(b) => b,
            Kind::Int(i) => i != 0,
            Kind::Double(d) => !(d == 0.0 || d.is_nan()),
            Kind::Str(s) => !self.str(s).is_empty(),
            Kind::Symbol(_) | Kind::Object(_) => true,
            Kind::BigInt(b) => !self.bigint(b).is_zero(),
        }
    }

    pub fn to_primitive(&mut self, v: Value, hint: Hint) -> JsResult<Value> {
        let Some(o) = v.as_object() else { return Ok(v) };
        let k = Key::Sym(self.intrinsics.sym_to_primitive);
        if let Some(f) = self.get_method(v, k)? {
            let h = match hint {
                Hint::Default => self.names.default,
                Hint::Number => self.names.number,
                Hint::String => self.names.string,
            };
            let r = self.call(f, v, &[Value::str(h)])?;
            if r.is_object() {
                return Err(self.type_error("Cannot convert object to primitive value"));
            }
            return Ok(r);
        }
        self.ordinary_to_primitive(o, if hint == Hint::String { Hint::String } else { Hint::Number })
    }

    pub fn ordinary_to_primitive(&mut self, o: ObjId, hint: Hint) -> JsResult<Value> {
        let order = if hint == Hint::String {
            [self.names.to_string, self.names.value_of]
        } else {
            [self.names.value_of, self.names.to_string]
        };
        for name in order {
            let f = self.get(o, Key::Str(name))?;
            if self.is_callable(f) {
                let r = self.call(f, Value::object(o), &[])?;
                if !r.is_object() {
                    return Ok(r);
                }
            }
        }
        Err(self.type_error("Cannot convert object to primitive value"))
    }

    pub fn to_number(&mut self, v: Value) -> JsResult<f64> {
        match v.kind() {
            Kind::Int(i) => Ok(i as f64),
            Kind::Double(d) => Ok(d),
            Kind::Undefined => Ok(f64::NAN),
            Kind::Null => Ok(0.0),
            Kind::Bool(b) => Ok(if b { 1.0 } else { 0.0 }),
            Kind::Str(s) => Ok(string_to_number(self.str(s))),
            Kind::Symbol(_) => Err(self.type_error("Cannot convert a Symbol value to a number")),
            Kind::BigInt(_) => Err(self.type_error("Cannot convert a BigInt value to a number")),
            Kind::Object(_) => {
                let p = self.to_primitive(v, Hint::Number)?;
                self.to_number(p)
            }
            Kind::Hole | Kind::Uninit => Ok(f64::NAN),
        }
    }

    /// ToNumeric: a Number value or a BigInt value.
    pub fn to_numeric(&mut self, v: Value) -> JsResult<Value> {
        if v.is_number() || v.is_bigint() {
            return Ok(v);
        }
        let p = self.to_primitive(v, Hint::Number)?;
        if p.is_bigint() {
            return Ok(p);
        }
        Ok(Value::number(self.to_number(p)?))
    }

    pub fn to_integer_or_infinity(&mut self, v: Value) -> JsResult<f64> {
        if let Some(i) = v.as_int() {
            return Ok(i as f64);
        }
        let n = self.to_number(v)?;
        if n.is_nan() || n == 0.0 {
            return Ok(0.0);
        }
        if n.is_infinite() {
            return Ok(n);
        }
        Ok(crate::math::trunc(n) + 0.0)
    }

    pub fn to_int32(&mut self, v: Value) -> JsResult<i32> {
        if let Some(i) = v.as_int() {
            return Ok(i);
        }
        Ok(to_int32_f(self.to_number(v)?))
    }

    pub fn to_uint32(&mut self, v: Value) -> JsResult<u32> {
        if let Some(i) = v.as_int() {
            return Ok(i as u32);
        }
        Ok(to_uint32_f(self.to_number(v)?))
    }

    pub fn to_length(&mut self, v: Value) -> JsResult<u64> {
        if let Some(i) = v.as_int() {
            return Ok(i.max(0) as u64);
        }
        let n = self.to_integer_or_infinity(v)?;
        if n <= 0.0 {
            return Ok(0);
        }
        Ok(n.min(9007199254740991.0) as u64)
    }

    /// ToIndex (ArrayBuffer sizes and offsets).
    pub fn to_index(&mut self, v: Value) -> JsResult<u64> {
        if v.is_undefined() {
            return Ok(0);
        }
        let n = self.to_integer_or_infinity(v)?;
        if !(0.0..=9007199254740991.0).contains(&n) {
            return Err(self.range_error("Invalid index"));
        }
        Ok(n as u64)
    }

    pub fn to_string(&mut self, v: Value) -> JsResult<StrId> {
        match v.kind() {
            Kind::Str(s) => Ok(s),
            Kind::Int(i) => {
                if (0..10).contains(&i) {
                    return Ok(self.names.digits[i as usize]);
                }
                let s = format!("{i}");
                Ok(self.new_string(&s).as_string().unwrap())
            }
            Kind::Double(d) => {
                let s = number_to_string(d);
                Ok(self.new_string(&s).as_string().unwrap())
            }
            Kind::Undefined => Ok(self.names.undefined),
            Kind::Null => Ok(self.names.null),
            Kind::Bool(true) => Ok(self.names.true_),
            Kind::Bool(false) => Ok(self.names.false_),
            Kind::Symbol(_) => Err(self.type_error("Cannot convert a Symbol value to a string")),
            Kind::BigInt(b) => {
                let s = self.bigint(b).to_string_radix(10);
                Ok(self.new_string(&s).as_string().unwrap())
            }
            Kind::Object(_) => {
                let p = self.to_primitive(v, Hint::String)?;
                self.to_string(p)
            }
            Kind::Hole | Kind::Uninit => Ok(self.names.undefined),
        }
    }

    pub fn to_string_value(&mut self, v: Value) -> JsResult<Value> {
        Ok(Value::str(self.to_string(v)?))
    }

    /// ToString of a value for messages (never throws, no side effects).
    pub fn display(&self, v: Value) -> String {
        match v.kind() {
            Kind::Str(s) => self.str(s).to_rust_lossy(),
            Kind::Int(i) => format!("{i}"),
            Kind::Double(d) => number_to_string(d),
            Kind::Undefined => "undefined".into(),
            Kind::Null => "null".into(),
            Kind::Bool(b) => format!("{b}"),
            Kind::Symbol(s) => match self.symbols.get(s.0).description {
                Some(d) => format!("Symbol({})", self.str(d).to_rust_lossy()),
                None => "Symbol()".into(),
            },
            Kind::BigInt(b) => format!("{}n", self.bigint(b).to_string_radix(10)),
            Kind::Object(o) => {
                if self.obj(o).is_callable() {
                    "function".into()
                } else if self.obj(o).is_array() {
                    "[object Array]".into()
                } else {
                    "[object Object]".into()
                }
            }
            Kind::Hole | Kind::Uninit => "undefined".into(),
        }
    }

    pub fn to_object(&mut self, v: Value) -> JsResult<ObjId> {
        let (proto, class) = match v.kind() {
            Kind::Object(o) => return Ok(o),
            Kind::Undefined | Kind::Null | Kind::Hole | Kind::Uninit => {
                return Err(self.type_error("Cannot convert undefined or null to object"));
            }
            Kind::Bool(b) => (self.intrinsics.boolean_prototype, Class::Boolean(b)),
            Kind::Int(_) | Kind::Double(_) => (self.intrinsics.number_prototype, Class::Number(v.num())),
            Kind::Str(s) => (self.intrinsics.string_prototype, Class::String(s)),
            Kind::Symbol(s) => (self.intrinsics.symbol_prototype, Class::Symbol(s)),
            Kind::BigInt(b) => (self.intrinsics.bigint_prototype, Class::BigInt(b)),
        };
        Ok(self.alloc_object(Some(proto), class))
    }

    pub fn to_property_key(&mut self, v: Value) -> JsResult<Key> {
        match v.kind() {
            Kind::Int(i) if i >= 0 => Ok(Key::Index(i as u32)),
            Kind::Str(s) => Ok(self.key_from_str(s)),
            Kind::Symbol(s) => Ok(Key::Sym(s)),
            Kind::Object(_) => {
                let p = self.to_primitive(v, Hint::String)?;
                self.to_property_key(p)
            }
            _ => {
                let s = self.to_string(v)?;
                Ok(self.key_from_str(s))
            }
        }
    }

    /// A key back to a property-key value (strings and symbols).
    pub fn key_value(&mut self, k: Key) -> Value {
        self.key_to_value(k)
    }

    // ----- equality -----------------------------------------------------

    pub fn strict_equals(&self, a: Value, b: Value) -> bool {
        if a.raw() == b.raw() {
            return !(a == Value::NAN);
        }
        match (a.kind(), b.kind()) {
            (Kind::Str(x), Kind::Str(y)) => self.str(x).eq_units(self.str(y)),
            (Kind::Int(_) | Kind::Double(_), Kind::Int(_) | Kind::Double(_)) => a.num() == b.num(),
            (Kind::BigInt(x), Kind::BigInt(y)) => self.bigint(x) == self.bigint(y),
            _ => false,
        }
    }

    pub fn same_value(&self, a: Value, b: Value) -> bool {
        if a.is_number() && b.is_number() {
            let (x, y) = (a.num(), b.num());
            if x.is_nan() && y.is_nan() {
                return true;
            }
            return x.to_bits() == y.to_bits() || (x == y && x != 0.0);
        }
        self.strict_equals(a, b)
    }

    pub fn same_value_zero(&self, a: Value, b: Value) -> bool {
        if a.is_number() && b.is_number() {
            let (x, y) = (a.num(), b.num());
            return x == y || (x.is_nan() && y.is_nan());
        }
        self.strict_equals(a, b)
    }

    /// IsLooselyEqual (`==`).
    pub fn loose_equals(&mut self, a: Value, b: Value) -> JsResult<bool> {
        use Kind::*;
        match (a.kind(), b.kind()) {
            (Undefined | Null, Undefined | Null) => Ok(true),
            (Int(_) | Double(_), Int(_) | Double(_)) => Ok(a.num() == b.num()),
            (Str(_), Str(_)) | (Bool(_), Bool(_)) | (Symbol(_), Symbol(_)) | (Object(_), Object(_)) | (BigInt(_), BigInt(_)) => {
                Ok(self.strict_equals(a, b))
            }
            (Int(_) | Double(_), Str(s)) => Ok(a.num() == string_to_number(self.str(s))),
            (Str(s), Int(_) | Double(_)) => Ok(string_to_number(self.str(s)) == b.num()),
            (BigInt(x), Str(s)) | (Str(s), BigInt(x)) => {
                let t = self.str(s).to_rust_lossy();
                Ok(match crate::builtins::bigint::BigInt::parse_string(&t) {
                    Some(y) => *self.bigint(x) == y,
                    None => false,
                })
            }
            (Bool(x), _) => {
                let n = Value::int(x as i32);
                self.loose_equals(n, b)
            }
            (_, Bool(y)) => {
                let n = Value::int(y as i32);
                self.loose_equals(a, n)
            }
            (Int(_) | Double(_) | Str(_) | BigInt(_) | Symbol(_), Object(_)) => {
                let p = self.to_primitive(b, Hint::Default)?;
                self.loose_equals(a, p)
            }
            (Object(_), Int(_) | Double(_) | Str(_) | BigInt(_) | Symbol(_)) => {
                let p = self.to_primitive(a, Hint::Default)?;
                self.loose_equals(p, b)
            }
            (BigInt(x), Int(_) | Double(_)) => Ok(self.bigint(x).cmp_f64(b.num()) == Some(core::cmp::Ordering::Equal)),
            (Int(_) | Double(_), BigInt(y)) => Ok(self.bigint(y).cmp_f64(a.num()) == Some(core::cmp::Ordering::Equal)),
            _ => Ok(false),
        }
    }

    // ----- operators ----------------------------------------------------

    /// `a + b`.
    pub fn op_add(&mut self, a: Value, b: Value) -> JsResult<Value> {
        if let (Some(x), Some(y)) = (a.as_int(), b.as_int()) {
            return Ok(match x.checked_add(y) {
                Some(r) => Value::int(r),
                None => Value::number(x as f64 + y as f64),
            });
        }
        if a.is_number() && b.is_number() {
            return Ok(Value::number(a.num() + b.num()));
        }
        if let (Some(x), Some(y)) = (a.as_string(), b.as_string()) {
            return self.concat(x, y);
        }
        let pa = self.to_primitive(a, Hint::Default)?;
        let pb = self.to_primitive(b, Hint::Default)?;
        if pa.is_string() || pb.is_string() {
            let x = self.to_string(pa)?;
            let y = self.to_string(pb)?;
            return self.concat(x, y);
        }
        let na = self.to_numeric(pa)?;
        let nb = self.to_numeric(pb)?;
        self.numeric_op(crate::ast::BinaryOp::Add, na, nb)
    }

    /// `x + y` for strings (a RangeError past the maximum length).
    pub fn concat(&mut self, x: StrId, y: StrId) -> JsResult<Value> {
        // Lengths without flattening: `s += x` must not copy `s`.
        let (a, b) = (self.strings.get(x.0), self.strings.get(y.0));
        if a.is_empty() {
            return Ok(Value::str(y));
        }
        if b.is_empty() {
            return Ok(Value::str(x));
        }
        let len = a.len() + b.len();
        if len > crate::string::MAX_LEN {
            return Err(self.range_error("Invalid string length"));
        }
        if len >= crate::string::ROPE_MIN {
            let latin1 = a.is_latin1() && b.is_latin1();
            return Ok(Value::str(self.alloc_string(crate::string::JsStr::rope(x, y, len, latin1))));
        }
        let s = self.str(x).concat(self.str(y));
        Ok(Value::str(self.alloc_string(s)))
    }

    /// Arithmetic on two numeric values (both Numbers or both BigInts).
    pub fn numeric_op(&mut self, op: crate::ast::BinaryOp, a: Value, b: Value) -> JsResult<Value> {
        use crate::ast::BinaryOp::*;
        if a.is_bigint() || b.is_bigint() {
            if !(a.is_bigint() && b.is_bigint()) {
                return Err(self.type_error("Cannot mix BigInt and other types, use explicit conversions"));
            }
            return crate::builtins::bigint::binary(self, op, a, b);
        }
        let (x, y) = (a.num(), b.num());
        Ok(match op {
            Add => Value::number(x + y),
            Sub => Value::number(x - y),
            Mul => Value::number(x * y),
            Div => Value::number(x / y),
            Mod => Value::number(js_rem(x, y)),
            Exp => Value::number(js_pow(x, y)),
            Shl => Value::int(to_int32_f(x).wrapping_shl(to_uint32_f(y) & 31)),
            Shr => Value::int(to_int32_f(x) >> (to_uint32_f(y) & 31)),
            Ushr => Value::from_u32(to_uint32_f(x) >> (to_uint32_f(y) & 31)),
            BitAnd => Value::int(to_int32_f(x) & to_int32_f(y)),
            BitOr => Value::int(to_int32_f(x) | to_int32_f(y)),
            BitXor => Value::int(to_int32_f(x) ^ to_int32_f(y)),
            _ => unreachable!(),
        })
    }

    /// A binary arithmetic/bitwise operator on any values.
    pub fn binary_arith(&mut self, op: crate::ast::BinaryOp, a: Value, b: Value) -> JsResult<Value> {
        if op == crate::ast::BinaryOp::Add {
            return self.op_add(a, b);
        }
        if a.is_number() && b.is_number() {
            return self.numeric_op(op, a, b);
        }
        let na = self.to_numeric(a)?;
        let nb = self.to_numeric(b)?;
        self.numeric_op(op, na, nb)
    }

    /// IsLessThan: Some(true/false) or None (undefined: NaN involved).
    pub fn less_than(&mut self, a: Value, b: Value, left_first: bool) -> JsResult<Option<bool>> {
        if a.is_number() && b.is_number() {
            let (x, y) = (a.num(), b.num());
            if x.is_nan() || y.is_nan() {
                return Ok(None);
            }
            return Ok(Some(x < y));
        }
        let (pa, pb) = if left_first {
            let pa = self.to_primitive(a, Hint::Number)?;
            let pb = self.to_primitive(b, Hint::Number)?;
            (pa, pb)
        } else {
            let pb = self.to_primitive(b, Hint::Number)?;
            let pa = self.to_primitive(a, Hint::Number)?;
            (pa, pb)
        };
        if let (Some(x), Some(y)) = (pa.as_string(), pb.as_string()) {
            return Ok(Some(self.str(x).cmp_units(self.str(y)) == core::cmp::Ordering::Less));
        }
        if let (Some(x), Some(s)) = (pa.as_bigint(), pb.as_string()) {
            let t = self.str(s).to_rust_lossy();
            return Ok(crate::builtins::bigint::BigInt::parse_string(&t).map(|y| *self.bigint(x) < y));
        }
        if let (Some(s), Some(y)) = (pa.as_string(), pb.as_bigint()) {
            let t = self.str(s).to_rust_lossy();
            return Ok(crate::builtins::bigint::BigInt::parse_string(&t).map(|x| x < *self.bigint(y)));
        }
        let na = self.to_numeric(pa)?;
        let nb = self.to_numeric(pb)?;
        match (na.as_bigint(), nb.as_bigint()) {
            (Some(x), Some(y)) => Ok(Some(self.bigint(x) < self.bigint(y))),
            (Some(x), None) => {
                let y = nb.num();
                if y.is_nan() {
                    return Ok(None);
                }
                Ok(Some(self.bigint(x).cmp_f64(y) == Some(core::cmp::Ordering::Less)))
            }
            (None, Some(y)) => {
                let x = na.num();
                if x.is_nan() {
                    return Ok(None);
                }
                Ok(Some(self.bigint(y).cmp_f64(x) == Some(core::cmp::Ordering::Greater)))
            }
            (None, None) => {
                let (x, y) = (na.num(), nb.num());
                if x.is_nan() || y.is_nan() {
                    return Ok(None);
                }
                Ok(Some(x < y))
            }
        }
    }

    pub fn typeof_value(&self, v: Value) -> StrId {
        match v.kind() {
            Kind::Undefined | Kind::Hole | Kind::Uninit => self.names.undefined,
            Kind::Null => self.names.object,
            Kind::Bool(_) => self.names.boolean,
            Kind::Int(_) | Kind::Double(_) => self.names.number,
            Kind::Str(_) => self.names.string,
            Kind::Symbol(_) => self.names.symbol,
            Kind::BigInt(_) => self.names.bigint,
            Kind::Object(o) => {
                if self.obj(o).is_callable() {
                    self.names.function
                } else {
                    self.names.object
                }
            }
        }
    }

    /// InstanceofOperator.
    pub fn instance_of(&mut self, v: Value, target: Value) -> JsResult<bool> {
        if !target.is_object() {
            return Err(self.type_error("Right-hand side of 'instanceof' is not an object"));
        }
        let k = Key::Sym(self.intrinsics.sym_has_instance);
        if let Some(h) = self.get_method(target, k)? {
            let r = self.call(h, target, &[v])?;
            return Ok(self.to_boolean(r));
        }
        if !self.is_callable(target) {
            return Err(self.type_error("Right-hand side of 'instanceof' is not callable"));
        }
        self.ordinary_has_instance(target, v)
    }

    pub fn ordinary_has_instance(&mut self, c: Value, v: Value) -> JsResult<bool> {
        if !self.is_callable(c) {
            return Ok(false);
        }
        let co = c.as_object().unwrap();
        if let Class::Bound { target, .. } = self.obj(co).class {
            return self.instance_of(v, Value::object(target));
        }
        let Some(mut o) = v.as_object() else { return Ok(false) };
        let k = Key::Str(self.names.prototype);
        let p = self.get(co, k)?;
        let Some(p) = p.as_object() else {
            return Err(self.type_error("Function has non-object prototype in instanceof check"));
        };
        loop {
            match self.get_prototype_of(o)? {
                Some(x) if x == p => return Ok(true),
                Some(x) => o = x,
                None => return Ok(false),
            }
        }
    }

    pub fn op_in(&mut self, key: Value, obj: Value) -> JsResult<bool> {
        let Some(o) = obj.as_object() else {
            return Err(self.type_error("Cannot use 'in' operator to search for a key in a non-object"));
        };
        let k = self.to_property_key(key)?;
        self.has_property(o, k)
    }
}

/// `%` on doubles (sign of the dividend; C fmod).
pub fn js_rem(x: f64, y: f64) -> f64 {
    if y.is_infinite() && x.is_finite() {
        return x;
    }
    let r = x % y;
    if r == 0.0 { if x.is_sign_negative() { -0.0 } else { 0.0 } } else { r }
}

/// `**` (Number::exponentiate).
pub fn js_pow(x: f64, y: f64) -> f64 {
    if y.is_nan() {
        return f64::NAN;
    }
    if y == 0.0 {
        return 1.0;
    }
    if (x == 1.0 || x == -1.0) && y.is_infinite() {
        return f64::NAN;
    }
    crate::builtins::math::pow(x, y)
}
