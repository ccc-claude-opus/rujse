//! `Value`: a JavaScript value in 64 bits (NaN-boxing).
//!
//! A double is stored as itself (every NaN canonicalized to one bit
//! pattern). Other values live in the negative quiet-NaN space
//! `0xFFF8_…`: bits 48–50 are a tag, the low 32 bits the payload (an int32
//! or a heap index). Integers that fit in i32 (except -0) are always stored
//! as int32, so `===` on such numbers is a bit comparison.

use core::fmt;

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Value(u64);

const BOX: u64 = 0xFFF8_0000_0000_0000;
const TAG_MASK: u64 = 0x0007_0000_0000_0000;
const CANON_NAN: u64 = 0x7FF8_0000_0000_0000;

const T_SPECIAL: u64 = 0;
const T_INT: u64 = 1;
const T_STR: u64 = 2;
const T_SYM: u64 = 3;
const T_OBJ: u64 = 4;
const T_BIG: u64 = 5;

const fn boxed(tag: u64, payload: u32) -> u64 {
    BOX | (tag << 48) | payload as u64
}

/// Heap indices.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default)]
pub struct ObjId(pub u32);
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default)]
pub struct StrId(pub u32);
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default)]
pub struct SymId(pub u32);
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default)]
pub struct BigId(pub u32);

/// The kind of a value, for `match`.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Kind {
    Undefined,
    Null,
    Bool(bool),
    Int(i32),
    Double(f64),
    Str(StrId),
    Symbol(SymId),
    Object(ObjId),
    BigInt(BigId),
    /// Internal markers that never reach JS: an array hole, an
    /// uninitialized binding (TDZ).
    Hole,
    Uninit,
}

impl Value {
    pub const UNDEFINED: Value = Value(boxed(T_SPECIAL, 0));
    pub const NULL: Value = Value(boxed(T_SPECIAL, 1));
    pub const FALSE: Value = Value(boxed(T_SPECIAL, 2));
    pub const TRUE: Value = Value(boxed(T_SPECIAL, 3));
    /// An array hole (missing element).
    pub const HOLE: Value = Value(boxed(T_SPECIAL, 4));
    /// A `let`/`const`/class binding before initialization.
    pub const UNINIT: Value = Value(boxed(T_SPECIAL, 5));
    pub const NAN: Value = Value(CANON_NAN);
    pub const ZERO: Value = Value(boxed(T_INT, 0));

    #[inline]
    pub fn raw(self) -> u64 {
        self.0
    }

    #[inline]
    pub fn from_raw(bits: u64) -> Value {
        Value(bits)
    }

    #[inline]
    pub fn bool(b: bool) -> Value {
        if b { Value::TRUE } else { Value::FALSE }
    }

    #[inline]
    pub fn int(i: i32) -> Value {
        Value(boxed(T_INT, i as u32))
    }

    /// A number; integral values in i32 range (not -0) become int32.
    #[inline]
    pub fn number(f: f64) -> Value {
        let i = f as i32;
        if i as f64 == f && (i != 0 || f.to_bits() == 0) {
            return Value::int(i);
        }
        if f.is_nan() {
            return Value::NAN;
        }
        Value(f.to_bits())
    }

    #[inline]
    pub fn from_u32(n: u32) -> Value {
        if n <= i32::MAX as u32 { Value::int(n as i32) } else { Value(f64::from(n).to_bits()) }
    }

    #[inline]
    pub fn from_usize(n: usize) -> Value {
        if n <= i32::MAX as usize { Value::int(n as i32) } else { Value::number(n as f64) }
    }

    #[inline]
    pub fn str(id: StrId) -> Value {
        Value(boxed(T_STR, id.0))
    }

    #[inline]
    pub fn symbol(id: SymId) -> Value {
        Value(boxed(T_SYM, id.0))
    }

    #[inline]
    pub fn object(id: ObjId) -> Value {
        Value(boxed(T_OBJ, id.0))
    }

    #[inline]
    pub fn bigint(id: BigId) -> Value {
        Value(boxed(T_BIG, id.0))
    }

    #[inline]
    fn is_boxed(self) -> bool {
        self.0 & BOX == BOX
    }

    #[inline]
    fn tag(self) -> u64 {
        (self.0 & TAG_MASK) >> 48
    }

    #[inline]
    fn payload(self) -> u32 {
        self.0 as u32
    }

    #[inline]
    pub fn kind(self) -> Kind {
        if !self.is_boxed() {
            return Kind::Double(f64::from_bits(self.0));
        }
        match self.tag() {
            T_INT => Kind::Int(self.payload() as i32),
            T_STR => Kind::Str(StrId(self.payload())),
            T_SYM => Kind::Symbol(SymId(self.payload())),
            T_OBJ => Kind::Object(ObjId(self.payload())),
            T_BIG => Kind::BigInt(BigId(self.payload())),
            _ => match self.payload() {
                0 => Kind::Undefined,
                1 => Kind::Null,
                2 => Kind::Bool(false),
                3 => Kind::Bool(true),
                4 => Kind::Hole,
                _ => Kind::Uninit,
            },
        }
    }

    #[inline]
    pub fn is_undefined(self) -> bool {
        self == Value::UNDEFINED
    }

    #[inline]
    pub fn is_null(self) -> bool {
        self == Value::NULL
    }

    #[inline]
    pub fn is_nullish(self) -> bool {
        self == Value::UNDEFINED || self == Value::NULL
    }

    #[inline]
    pub fn is_int(self) -> bool {
        self.is_boxed() && self.tag() == T_INT
    }

    #[inline]
    pub fn as_int(self) -> Option<i32> {
        if self.is_int() { Some(self.payload() as i32) } else { None }
    }

    #[inline]
    pub fn is_number(self) -> bool {
        !self.is_boxed() || self.tag() == T_INT
    }

    /// The numeric value of a Number (int32 or double).
    #[inline]
    pub fn as_number(self) -> Option<f64> {
        if !self.is_boxed() {
            Some(f64::from_bits(self.0))
        } else if self.tag() == T_INT {
            Some(self.payload() as i32 as f64)
        } else {
            None
        }
    }

    /// The number of a value known to be a Number.
    #[inline]
    pub fn num(self) -> f64 {
        if self.is_boxed() { self.payload() as i32 as f64 } else { f64::from_bits(self.0) }
    }

    #[inline]
    pub fn is_string(self) -> bool {
        self.is_boxed() && self.tag() == T_STR
    }

    #[inline]
    pub fn as_string(self) -> Option<StrId> {
        if self.is_string() { Some(StrId(self.payload())) } else { None }
    }

    #[inline]
    pub fn is_object(self) -> bool {
        self.is_boxed() && self.tag() == T_OBJ
    }

    #[inline]
    pub fn as_object(self) -> Option<ObjId> {
        if self.is_object() { Some(ObjId(self.payload())) } else { None }
    }

    #[inline]
    pub fn is_symbol(self) -> bool {
        self.is_boxed() && self.tag() == T_SYM
    }

    #[inline]
    pub fn as_symbol(self) -> Option<SymId> {
        if self.is_symbol() { Some(SymId(self.payload())) } else { None }
    }

    #[inline]
    pub fn is_bigint(self) -> bool {
        self.is_boxed() && self.tag() == T_BIG
    }

    #[inline]
    pub fn as_bigint(self) -> Option<BigId> {
        if self.is_bigint() { Some(BigId(self.payload())) } else { None }
    }

    #[inline]
    pub fn is_bool(self) -> bool {
        self == Value::TRUE || self == Value::FALSE
    }

    #[inline]
    pub fn is_hole(self) -> bool {
        self == Value::HOLE
    }

    #[inline]
    pub fn is_uninit(self) -> bool {
        self == Value::UNINIT
    }

    /// A value that refers to the heap (traced by the GC).
    #[inline]
    pub fn is_heap(self) -> bool {
        self.is_boxed() && self.tag() >= T_STR
    }
}

impl Default for Value {
    fn default() -> Value {
        Value::UNDEFINED
    }
}

impl fmt::Debug for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.kind() {
            Kind::Undefined => write!(f, "undefined"),
            Kind::Null => write!(f, "null"),
            Kind::Bool(b) => write!(f, "{b}"),
            Kind::Int(i) => write!(f, "{i}"),
            Kind::Double(d) => write!(f, "{d}"),
            Kind::Str(s) => write!(f, "str#{}", s.0),
            Kind::Symbol(s) => write!(f, "sym#{}", s.0),
            Kind::Object(o) => write!(f, "obj#{}", o.0),
            Kind::BigInt(b) => write!(f, "big#{}", b.0),
            Kind::Hole => write!(f, "<hole>"),
            Kind::Uninit => write!(f, "<uninit>"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers() {
        assert_eq!(Value::number(3.0), Value::int(3));
        assert!(Value::number(-0.0) != Value::int(0));
        assert_eq!(Value::number(-0.0).num().to_bits(), (-0.0f64).to_bits());
        assert_eq!(Value::number(f64::NAN), Value::NAN);
        assert_eq!(Value::number(-f64::NAN), Value::NAN);
        assert_eq!(Value::number(1.5).kind(), Kind::Double(1.5));
        assert_eq!(Value::number(4294967296.0).num(), 4294967296.0);
        assert_eq!(Value::int(-7).as_int(), Some(-7));
        assert_eq!(Value::from_u32(u32::MAX).num(), 4294967295.0);
        assert!(Value::number(f64::INFINITY).is_number());
        assert!(Value::number(f64::NEG_INFINITY).is_number());
    }

    #[test]
    fn tags() {
        assert_eq!(Value::object(ObjId(5)).as_object(), Some(ObjId(5)));
        assert_eq!(Value::str(StrId(9)).kind(), Kind::Str(StrId(9)));
        assert!(Value::NULL.is_nullish() && Value::UNDEFINED.is_nullish());
        assert!(!Value::FALSE.is_nullish());
        assert!(Value::symbol(SymId(1)).is_heap());
        assert!(!Value::int(1).is_heap());
        assert_eq!(Value::HOLE.kind(), Kind::Hole);
        assert_eq!(Value::UNINIT.kind(), Kind::Uninit);
    }
}
