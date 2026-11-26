//! ArrayBuffer, SharedArrayBuffer, typed arrays (integer-indexed exotic
//! objects), DataView and Atomics (ECMA-262 §25.1–25.4, §23.2).

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;

use super::object::arg;
use crate::object::{attr, Class, Key};
use crate::value::{ObjId, Value};
use crate::vm::gc::Trace;
use crate::vm::props::PropDesc;
use crate::vm::{JsResult, Vm};

pub struct BufferData {
    pub data: Vec<u8>,
    pub detached: bool,
    /// Resizable (ArrayBuffer) / growable (SharedArrayBuffer) maximum.
    pub max_len: Option<usize>,
    pub shared: bool,
}

pub struct TypedArrayData {
    pub buffer: ObjId,
    pub kind: u8,
    pub offset: usize,
    /// None: the length tracks a resizable buffer.
    pub length: Option<usize>,
}

impl Trace for TypedArrayData {
    fn trace(&self, out: &mut Vec<Value>) {
        out.push(Value::object(self.buffer));
    }
}

pub struct ViewData {
    pub buffer: ObjId,
    pub offset: usize,
    pub length: Option<usize>,
}

impl Trace for ViewData {
    fn trace(&self, out: &mut Vec<Value>) {
        out.push(Value::object(self.buffer));
    }
}

/// Element types: name, size.
pub const KINDS: [(&str, usize); 12] = [
    ("Int8Array", 1),
    ("Uint8Array", 1),
    ("Uint8ClampedArray", 1),
    ("Int16Array", 2),
    ("Uint16Array", 2),
    ("Int32Array", 4),
    ("Uint32Array", 4),
    ("Float16Array", 2),
    ("Float32Array", 4),
    ("Float64Array", 8),
    ("BigInt64Array", 8),
    ("BigUint64Array", 8),
];

pub fn is_bigint_kind(k: u8) -> bool {
    k >= 10
}

fn elem_size(k: u8) -> usize {
    KINDS[k as usize].1
}

// ----- element encoding ------------------------------------------------

pub fn f16_to_f64(h: u16) -> f64 {
    let sign = if h & 0x8000 != 0 { -1.0 } else { 1.0 };
    let e = ((h >> 10) & 0x1F) as i32;
    let m = (h & 0x3FF) as f64;
    if e == 0 {
        return sign * m * crate::math::powi(2.0, -24);
    }
    if e == 31 {
        return if m == 0.0 { sign * f64::INFINITY } else { f64::NAN };
    }
    sign * (1.0 + m / 1024.0) * crate::math::powi(2.0, e - 15)
}

pub fn f64_to_f16(x: f64) -> u16 {
    if x.is_nan() {
        return 0x7E00;
    }
    let sign: u16 = if x.is_sign_negative() { 0x8000 } else { 0 };
    let a = x.abs();
    if a == f64::INFINITY {
        return sign | 0x7C00;
    }
    if a == 0.0 {
        return sign;
    }
    // Round to the nearest representable value (ties to even).
    let e = crate::math::floor(crate::math::log2(a)) as i32;
    let e = e.max(-14);
    let step = crate::math::powi(2.0, e - 10);
    let q = crate::math::round_half_even(a / step);
    let v = q * step;
    if v >= 65520.0 {
        return sign | 0x7C00;
    }
    // Encode v.
    if v < crate::math::powi(2.0, -14) {
        return sign | (v / crate::math::powi(2.0, -24)) as u16;
    }
    let mut ex = crate::math::floor(crate::math::log2(v)) as i32;
    let mut mant = v / crate::math::powi(2.0, ex) - 1.0;
    if mant >= 1.0 {
        ex += 1;
        mant = v / crate::math::powi(2.0, ex) - 1.0;
    }
    sign | (((ex + 15) as u16) << 10) | (crate::math::round_half_even(mant * 1024.0) as u16 & 0x3FF)
}

/// RawBytesToNumeric.
fn decode(vm: &mut Vm, kind: u8, b: &[u8]) -> Value {
    let mut le = [0u8; 8];
    le[..b.len()].copy_from_slice(b);
    let u = u64::from_le_bytes(le);
    match kind {
        0 => Value::int(b[0] as i8 as i32),
        1 | 2 => Value::int(b[0] as i32),
        3 => Value::int(u as u16 as i16 as i32),
        4 => Value::int(u as u16 as i32),
        5 => Value::int(u as u32 as i32),
        6 => Value::from_u32(u as u32),
        7 => Value::number(f16_to_f64(u as u16)),
        8 => Value::number(f32::from_bits(u as u32) as f64),
        9 => Value::number(f64::from_bits(u)),
        10 => {
            let b = super::bigint::BigInt::from_i64(u as i64);
            super::bigint::alloc(vm, b)
        }
        _ => {
            let mut mag = Vec::new();
            if u != 0 {
                mag.push(u as u32);
                if u >> 32 != 0 {
                    mag.push((u >> 32) as u32);
                }
            }
            super::bigint::alloc(vm, super::bigint::BigInt { neg: false, mag })
        }
    }
}

/// NumericToRawBytes for an already converted value (Number or BigInt).
fn encode(vm: &Vm, kind: u8, v: Value, out: &mut [u8]) {
    let n = |v: Value| v.as_number().unwrap_or(f64::NAN);
    let bytes: [u8; 8] = match kind {
        0..=6 => {
            let x = n(v);
            let i = if kind == 2 {
                // Uint8Clamped: round half to even, clamp.
                if x.is_nan() || x <= 0.0 { 0 } else if x >= 255.0 { 255 } else { crate::math::round_half_even(x) as i64 }
            } else {
                crate::vm::conv::to_int32_f(x) as u32 as i64
            };
            let x2 = if kind <= 2 || kind >= 5 { i } else { i };
            (x2 as u64).to_le_bytes()
        }
        7 => (f64_to_f16(n(v)) as u64).to_le_bytes(),
        8 => ((n(v) as f32).to_bits() as u64).to_le_bytes(),
        9 => n(v).to_bits().to_le_bytes(),
        _ => {
            // BigInt modulo 2^64 (two's complement).
            let b = vm.bigint(v.as_bigint().unwrap());
            let mut x: u64 = 0;
            for (i, l) in b.mag.iter().enumerate().take(2) {
                x |= (*l as u64) << (32 * i);
            }
            if b.neg {
                x = x.wrapping_neg();
            }
            x.to_le_bytes()
        }
    };
    out.copy_from_slice(&bytes[..out.len()]);
}

/// Converts a value for storing in an element of `kind`.
fn to_element(vm: &mut Vm, kind: u8, v: Value) -> JsResult<Value> {
    if is_bigint_kind(kind) {
        let b = super::bigint_impl::to_bigint(vm, v)?;
        Ok(super::bigint::alloc(vm, b))
    } else {
        Ok(Value::number(vm.to_number(v)?))
    }
}

// ----- buffers -----------------------------------------------------------

fn buf(vm: &Vm, o: ObjId) -> &BufferData {
    match &vm.obj(o).class {
        Class::ArrayBuffer(b) => b,
        _ => panic!("not a buffer"),
    }
}

fn buf_mut(vm: &mut Vm, o: ObjId) -> &mut BufferData {
    match &mut vm.obj_mut(o).class {
        Class::ArrayBuffer(b) => b,
        _ => panic!("not a buffer"),
    }
}

/// AllocateArrayBuffer.
pub fn allocate_buffer(vm: &mut Vm, proto: ObjId, len: usize, max: Option<usize>, shared: bool) -> JsResult<ObjId> {
    if len > (1 << 30) || max.is_some_and(|m| m > (1 << 30)) {
        return Err(vm.range_error("Array buffer allocation failed"));
    }
    vm.alloc_bytes += len;
    Ok(vm.alloc_object(Some(proto), Class::ArrayBuffer(Box::new(BufferData { data: alloc::vec![0; len], detached: false, max_len: max, shared }))))
}

/// An ArrayBuffer over bytes the host provides (no copy).
pub fn buffer_from_vec(vm: &mut Vm, data: Vec<u8>) -> ObjId {
    let p = vm.intrinsics.arraybuffer_prototype;
    vm.alloc_bytes += data.len();
    vm.alloc_object(Some(p), Class::ArrayBuffer(Box::new(BufferData { data, detached: false, max_len: None, shared: false })))
}

/// Borrows the bytes of an ArrayBuffer (None if not one or detached).
pub fn buffer_bytes(vm: &Vm, o: ObjId) -> Option<&[u8]> {
    match &vm.obj(o).class {
        Class::ArrayBuffer(b) if !b.detached => Some(&b.data),
        _ => None,
    }
}

/// DetachArrayBuffer (for $262.detachArrayBuffer).
pub fn detach(vm: &mut Vm, v: Value) -> JsResult<()> {
    match v.as_object().map(|o| &mut vm.obj_mut(o).class) {
        Some(Class::ArrayBuffer(b)) if !b.shared => {
            b.data = Vec::new();
            b.detached = true;
            Ok(())
        }
        _ => Err(vm.type_error("not an ArrayBuffer")),
    }
}

fn this_buffer(vm: &mut Vm, this: Value, shared: bool) -> JsResult<ObjId> {
    if let Some(o) = this.as_object() {
        if let Class::ArrayBuffer(b) = &vm.obj(o).class {
            if b.shared == shared {
                return Ok(o);
            }
        }
    }
    Err(vm.type_error(if shared { "Method called on incompatible receiver (not a SharedArrayBuffer)" } else { "Method called on incompatible receiver (not an ArrayBuffer)" }))
}

fn buffer_ctor(vm: &mut Vm, a: &[Value], nt: Value, shared: bool) -> JsResult<Value> {
    if nt.is_undefined() {
        return Err(vm.type_error("Constructor requires 'new'"));
    }
    let len = vm.to_index(arg(a, 0))? as usize;
    let mut max = None;
    if let Some(opts) = arg(a, 1).as_object() {
        let k = vm.key("maxByteLength");
        let m = vm.get(opts, k)?;
        if !m.is_undefined() {
            let m = vm.to_index(m)? as usize;
            if len > m {
                return Err(vm.range_error("byteLength exceeds maxByteLength"));
            }
            max = Some(m);
        }
    }
    let proto = if shared { vm.proto_from_ctor(nt, |i| i.shared_arraybuffer_prototype)? } else { vm.proto_from_ctor(nt, |i| i.arraybuffer_prototype)? };
    Ok(Value::object(allocate_buffer(vm, proto, len, max, shared)?))
}

// ----- typed arrays ------------------------------------------------------

fn ta(vm: &Vm, o: ObjId) -> &TypedArrayData {
    match &vm.obj(o).class {
        Class::TypedArray(t) => t,
        _ => panic!("not a typed array"),
    }
}

/// The current length, or None when out of bounds / detached.
fn ta_len(vm: &Vm, o: ObjId) -> Option<usize> {
    let t = ta(vm, o);
    let b = buf(vm, t.buffer);
    if b.detached {
        return None;
    }
    let size = elem_size(t.kind);
    let blen = b.data.len();
    match t.length {
        Some(n) => {
            if t.offset + n * size > blen {
                None
            } else {
                Some(n)
            }
        }
        None => {
            if t.offset > blen {
                None
            } else {
                Some((blen - t.offset) / size)
            }
        }
    }
}

/// The length for iteration/validation: TypeError if out of bounds.
pub fn ta_length_checked(vm: &mut Vm, o: ObjId) -> JsResult<usize> {
    match ta_len(vm, o) {
        Some(n) => Ok(n),
        None => Err(vm.type_error("TypedArray is detached or out of bounds")),
    }
}

/// CanonicalNumericIndexString for a key: Some(n) if numeric.
fn numeric_index(vm: &mut Vm, k: Key) -> Option<f64> {
    match k {
        Key::Index(i) => Some(i as f64),
        Key::Sym(_) => None,
        Key::Str(s) => {
            let st = vm.str(s);
            if st.eq_str("-0") {
                return Some(-0.0);
            }
            let n = crate::vm::conv::string_to_number(st);
            let back = crate::vm::conv::number_to_string(n);
            if vm.str(s).eq_str(&back) { Some(n) } else { None }
        }
    }
}

fn valid_index(vm: &Vm, o: ObjId, n: f64) -> Option<usize> {
    if n.is_nan() || crate::math::trunc(n) != n || (n == 0.0 && n.is_sign_negative()) || n < 0.0 {
        return None;
    }
    let len = ta_len(vm, o)?;
    if n >= len as f64 { None } else { Some(n as usize) }
}

fn get_element(vm: &mut Vm, o: ObjId, i: usize) -> Value {
    let (b, kind, off) = {
        let t = ta(vm, o);
        (t.buffer, t.kind, t.offset)
    };
    let size = elem_size(kind);
    let start = off + i * size;
    let bytes: Vec<u8> = buf(vm, b).data[start..start + size].to_vec();
    decode(vm, kind, &bytes)
}

fn set_element_raw(vm: &mut Vm, o: ObjId, i: usize, v: Value) {
    let (b, kind, off) = {
        let t = ta(vm, o);
        (t.buffer, t.kind, t.offset)
    };
    let size = elem_size(kind);
    let start = off + i * size;
    let mut tmp = [0u8; 8];
    encode(vm, kind, v, &mut tmp[..size]);
    buf_mut(vm, b).data[start..start + size].copy_from_slice(&tmp[..size]);
}

/// TypedArraySetElement: converts, then writes if the index is valid.
fn set_element(vm: &mut Vm, o: ObjId, n: f64, v: Value) -> JsResult<()> {
    let kind = ta(vm, o).kind;
    let nv = to_element(vm, kind, v)?;
    if let Some(i) = valid_index(vm, o, n) {
        set_element_raw(vm, o, i, nv);
    }
    Ok(())
}

pub fn ta_get_own(vm: &mut Vm, o: ObjId, k: Key) -> Option<Option<PropDesc>> {
    let n = numeric_index(vm, k)?;
    Some(valid_index(vm, o, n).map(|i| {
        let v = get_element(vm, o, i);
        PropDesc::data(v, attr::DEFAULT)
    }))
}

pub fn ta_has(vm: &mut Vm, o: ObjId, k: Key) -> Option<bool> {
    let n = numeric_index(vm, k)?;
    Some(valid_index(vm, o, n).is_some())
}

pub fn ta_get(vm: &mut Vm, o: ObjId, k: Key) -> Option<Value> {
    let n = numeric_index(vm, k)?;
    Some(match valid_index(vm, o, n) {
        Some(i) => get_element(vm, o, i),
        None => Value::UNDEFINED,
    })
}

pub fn ta_set(vm: &mut Vm, o: ObjId, k: Key, v: Value, receiver: Value) -> JsResult<Option<bool>> {
    let Some(n) = numeric_index(vm, k) else { return Ok(None) };
    if receiver == Value::object(o) {
        set_element(vm, o, n, v)?;
        return Ok(Some(true));
    }
    if valid_index(vm, o, n).is_none() {
        return Ok(Some(true));
    }
    Ok(None)
}

pub fn ta_define_own(vm: &mut Vm, o: ObjId, k: Key, d: PropDesc) -> JsResult<Option<bool>> {
    let Some(n) = numeric_index(vm, k) else { return Ok(None) };
    if valid_index(vm, o, n).is_none() {
        return Ok(Some(false));
    }
    if d.configurable == Some(false) || d.enumerable == Some(false) || d.is_accessor() || d.writable == Some(false) {
        return Ok(Some(false));
    }
    if let Some(v) = d.value {
        set_element(vm, o, n, v)?;
    }
    Ok(Some(true))
}

pub fn ta_delete(vm: &mut Vm, o: ObjId, k: Key) -> Option<bool> {
    let n = numeric_index(vm, k)?;
    Some(valid_index(vm, o, n).is_none())
}

pub fn ta_own_keys(vm: &mut Vm, o: ObjId) -> Vec<Key> {
    let len = ta_len(vm, o).unwrap_or(0);
    let mut keys: Vec<Key> = (0..len as u32).map(Key::Index).collect();
    keys.extend(vm.raw_own_keys(o).into_iter().filter(|k| !matches!(k, Key::Index(_))));
    keys
}

pub fn ta_can_prevent_extensions(vm: &mut Vm, o: ObjId) -> bool {
    // A length-tracking or resizable-backed view can change length.
    let t = ta(vm, o);
    let b = buf(vm, t.buffer);
    !(t.length.is_none() || b.max_len.is_some() && !b.shared)
}

fn this_ta(vm: &mut Vm, this: Value) -> JsResult<ObjId> {
    match this.as_object() {
        Some(o) if matches!(vm.obj(o).class, Class::TypedArray(_)) => Ok(o),
        _ => Err(vm.type_error("this is not a typed array.")),
    }
}

/// ValidateTypedArray: (object, length).
pub fn validate(vm: &mut Vm, v: Value) -> JsResult<(ObjId, usize)> {
    let o = this_ta(vm, v)?;
    let n = ta_length_checked(vm, o)?;
    Ok((o, n))
}

fn kind_proto(vm: &Vm, kind: u8) -> fn(&crate::vm::Intrinsics) -> ObjId {
    let _ = vm;
    match kind {
        0 => |i| i.int8_array_prototype,
        1 => |i| i.uint8_array_prototype,
        2 => |i| i.uint8_clamped_array_prototype,
        3 => |i| i.int16_array_prototype,
        4 => |i| i.uint16_array_prototype,
        5 => |i| i.int32_array_prototype,
        6 => |i| i.uint32_array_prototype,
        7 => |i| i.float16_array_prototype,
        8 => |i| i.float32_array_prototype,
        9 => |i| i.float64_array_prototype,
        10 => |i| i.bigint64_array_prototype,
        _ => |i| i.biguint64_array_prototype,
    }
}

fn kind_ctor(vm: &Vm, kind: u8) -> ObjId {
    let i = &vm.intrinsics;
    [i.int8_array, i.uint8_array, i.uint8_clamped_array, i.int16_array, i.uint16_array, i.int32_array, i.uint32_array, i.float16_array, i.float32_array, i.float64_array, i.bigint64_array, i.biguint64_array][kind as usize]
}

/// A new typed array of `kind` with `len` elements (fresh buffer).
pub fn create_ta(vm: &mut Vm, proto: ObjId, kind: u8, len: usize) -> JsResult<ObjId> {
    let size = elem_size(kind);
    if len.checked_mul(size).is_none_or(|b| b > (1 << 30)) {
        return Err(vm.range_error("Invalid typed array length"));
    }
    let bp = vm.intrinsics.arraybuffer_prototype;
    let b = allocate_buffer(vm, bp, len * size, None, false)?;
    Ok(vm.alloc_object(Some(proto), Class::TypedArray(Box::new(TypedArrayData { buffer: b, kind, offset: 0, length: Some(len) }))))
}

/// The concrete typed array constructors.
fn ta_ctor(vm: &mut Vm, _: Value, a: &[Value], nt: Value) -> JsResult<Value> {
    let kind = vm.native_data(0).as_int().unwrap() as u8;
    if nt.is_undefined() {
        return Err(vm.type_error("Constructor requires 'new'"));
    }
    let proto = vm.proto_from_ctor(nt, kind_proto(vm, kind))?;
    let first = arg(a, 0);
    let Some(fo) = first.as_object() else {
        let len = vm.to_index(first)? as usize;
        return Ok(Value::object(create_ta(vm, proto, kind, len)?));
    };
    match &vm.obj(fo).class {
        Class::TypedArray(_) => {
            let src_kind = ta(vm, fo).kind;
            let len = ta_length_checked(vm, fo)?;
            if is_bigint_kind(src_kind) != is_bigint_kind(kind) {
                return Err(vm.type_error("Content types differ"));
            }
            let o = create_ta(vm, proto, kind, len)?;
            for i in 0..len {
                let v = get_element(vm, fo, i);
                set_element_raw(vm, o, i, v);
            }
            Ok(Value::object(o))
        }
        Class::ArrayBuffer(_) => {
            let size = elem_size(kind);
            let offset = vm.to_index(arg(a, 1))? as usize;
            if offset % size != 0 {
                return Err(vm.range_error("start offset must be a multiple of the element size"));
            }
            let len_v = arg(a, 2);
            let new_len = if len_v.is_undefined() { None } else { Some(vm.to_index(len_v)? as usize) };
            let b = buf(vm, fo);
            if b.detached {
                return Err(vm.type_error("ArrayBuffer is detached"));
            }
            let blen = b.data.len();
            let tracking = new_len.is_none() && b.max_len.is_some() && !b.shared || (new_len.is_none() && b.shared && b.max_len.is_some());
            let length = match new_len {
                Some(n) => {
                    if offset + n * size > blen {
                        return Err(vm.range_error("Invalid typed array length"));
                    }
                    Some(n)
                }
                None => {
                    if tracking {
                        if offset > blen {
                            return Err(vm.range_error("Start offset is outside the bounds of the buffer"));
                        }
                        None
                    } else {
                        if blen % size != 0 {
                            return Err(vm.range_error("byte length must be a multiple of the element size"));
                        }
                        if offset > blen {
                            return Err(vm.range_error("Start offset is outside the bounds of the buffer"));
                        }
                        Some((blen - offset) / size)
                    }
                }
            };
            let o = vm.alloc_object(Some(proto), Class::TypedArray(Box::new(TypedArrayData { buffer: fo, kind, offset, length })));
            Ok(Value::object(o))
        }
        _ => {
            // An iterable or array-like object.
            let ki = crate::object::Key::Sym(vm.intrinsics.sym_iterator);
            let using = vm.get_method(first, ki)?;
            let values: Vec<Value> = match using {
                Some(f) => {
                    let it = vm.call(f, first, &[])?;
                    if !it.is_object() {
                        return Err(vm.type_error("iterator is not an object"));
                    }
                    vm.root(it);
                    let kn = Key::Str(vm.names.next);
                    let next = vm.get_v(it, kn)?;
                    let mut out = Vec::new();
                    while let Some(v) = vm.iterator_step(it, next)? {
                        vm.root(v);
                        out.push(v);
                    }
                    out
                }
                None => {
                    let len = vm.length_of(fo)?;
                    let o = create_ta(vm, proto, kind, len as usize)?;
                    vm.root(Value::object(o));
                    for i in 0..len {
                        let k = vm.index_key(i);
                        let v = vm.get(fo, k)?;
                        set_element(vm, o, i as f64, v)?;
                    }
                    return Ok(Value::object(o));
                }
            };
            let o = create_ta(vm, proto, kind, values.len())?;
            vm.root(Value::object(o));
            for (i, v) in values.into_iter().enumerate() {
                set_element(vm, o, i as f64, v)?;
            }
            Ok(Value::object(o))
        }
    }
}

/// TypedArraySpeciesCreate / TypedArrayCreateFromConstructor with a length.
pub fn species_create(vm: &mut Vm, exemplar: ObjId, len: usize) -> JsResult<ObjId> {
    let kind = ta(vm, exemplar).kind;
    let default = Value::object(kind_ctor(vm, kind));
    let c = super::promise::species_constructor(vm, exemplar, default)?;
    let r = create_from_ctor(vm, c, &[Value::number(len as f64)], Some(len))?;
    if is_bigint_kind(ta(vm, r).kind) != is_bigint_kind(kind) {
        return Err(vm.type_error("Content types differ"));
    }
    Ok(r)
}

/// TypedArrayCreateFromConstructor.
pub fn create_from_ctor(vm: &mut Vm, c: Value, args: &[Value], min_len: Option<usize>) -> JsResult<ObjId> {
    let r = vm.construct(c, args, c)?;
    let (o, len) = validate(vm, r)?;
    if let Some(m) = min_len {
        if len < m {
            return Err(vm.type_error("Derived TypedArray constructor created an array which was too small"));
        }
    }
    Ok(o)
}

// ----- DataView ------------------------------------------------------------

fn view(vm: &mut Vm, this: Value) -> JsResult<(ObjId, usize, Option<usize>)> {
    match this.as_object().map(|o| &vm.obj(o).class) {
        Some(Class::DataView(d)) => Ok((d.buffer, d.offset, d.length)),
        _ => Err(vm.type_error("Receiver is not a DataView")),
    }
}

/// The view's byte length, or TypeError if out of bounds/detached.
fn view_len(vm: &mut Vm, b: ObjId, off: usize, len: Option<usize>) -> JsResult<usize> {
    let bd = buf(vm, b);
    if bd.detached {
        return Err(vm.type_error("DataView buffer is detached"));
    }
    let blen = bd.data.len();
    match len {
        Some(n) if off + n <= blen => Ok(n),
        None if off <= blen => Ok(blen - off),
        _ => Err(vm.type_error("DataView is out of bounds")),
    }
}

fn dataview_ctor(vm: &mut Vm, _: Value, a: &[Value], nt: Value) -> JsResult<Value> {
    if nt.is_undefined() {
        return Err(vm.type_error("Constructor DataView requires 'new'"));
    }
    let b = match arg(a, 0).as_object() {
        Some(o) if matches!(vm.obj(o).class, Class::ArrayBuffer(_)) => o,
        _ => return Err(vm.type_error("First argument to DataView constructor must be an ArrayBuffer")),
    };
    let offset = vm.to_index(arg(a, 1))? as usize;
    if buf(vm, b).detached {
        return Err(vm.type_error("ArrayBuffer is detached"));
    }
    let blen = buf(vm, b).data.len();
    if offset > blen {
        return Err(vm.range_error("Start offset is outside the bounds of the buffer"));
    }
    let tracking = arg(a, 2).is_undefined() && buf(vm, b).max_len.is_some();
    let length = if arg(a, 2).is_undefined() {
        if tracking { None } else { Some(blen - offset) }
    } else {
        let l = vm.to_index(arg(a, 2))? as usize;
        if offset + l > blen {
            return Err(vm.range_error("Invalid DataView length"));
        }
        Some(l)
    };
    let p = vm.proto_from_ctor(nt, |i| i.dataview_prototype)?;
    if buf(vm, b).detached {
        return Err(vm.type_error("ArrayBuffer is detached"));
    }
    let blen = buf(vm, b).data.len();
    if offset > blen || length.is_some_and(|l| offset + l > blen) {
        return Err(vm.range_error("Invalid DataView length"));
    }
    Ok(Value::object(vm.alloc_object(Some(p), Class::DataView(Box::new(ViewData { buffer: b, offset, length })))))
}

/// DataView get/set: data = [kind].
fn view_get(vm: &mut Vm, this: Value, a: &[Value], _: Value) -> JsResult<Value> {
    let kind = vm.native_data(0).as_int().unwrap() as u8;
    let (b, off, len) = view(vm, this)?;
    let idx = vm.to_index(arg(a, 0))? as usize;
    let little = vm.to_boolean(arg(a, 1));
    let vlen = view_len(vm, b, off, len)?;
    let size = elem_size(kind);
    if idx + size > vlen {
        return Err(vm.range_error("Offset is outside the bounds of the DataView"));
    }
    let start = off + idx;
    let mut bytes: Vec<u8> = buf(vm, b).data[start..start + size].to_vec();
    if !little {
        bytes.reverse();
    }
    Ok(decode(vm, kind, &bytes))
}

fn view_set(vm: &mut Vm, this: Value, a: &[Value], _: Value) -> JsResult<Value> {
    let kind = vm.native_data(0).as_int().unwrap() as u8;
    let (b, off, len) = view(vm, this)?;
    let idx = vm.to_index(arg(a, 0))? as usize;
    let v = to_element(vm, kind, arg(a, 1))?;
    let little = vm.to_boolean(arg(a, 2));
    let vlen = view_len(vm, b, off, len)?;
    let size = elem_size(kind);
    if idx + size > vlen {
        return Err(vm.range_error("Offset is outside the bounds of the DataView"));
    }
    let mut tmp = [0u8; 8];
    encode(vm, kind, v, &mut tmp[..size]);
    if !little {
        tmp[..size].reverse();
    }
    let start = off + idx;
    buf_mut(vm, b).data[start..start + size].copy_from_slice(&tmp[..size]);
    Ok(Value::UNDEFINED)
}

// ----- init ----------------------------------------------------------------

pub fn init(vm: &mut Vm) {
    let op = vm.intrinsics.object_prototype;
    let kt = Key::Sym(vm.intrinsics.sym_to_string_tag);
    let sp = vm.intrinsics.sym_species;
    // ArrayBuffer
    let abp = vm.alloc_object(Some(op), Class::Ordinary);
    vm.intrinsics.arraybuffer_prototype = abp;
    let abc = vm.constructor("ArrayBuffer", 1, |vm, _, a, nt| buffer_ctor(vm, a, nt, false), abp);
    vm.intrinsics.arraybuffer = abc;
    vm.global_prop("ArrayBuffer", Value::object(abc));
    vm.getter_sym(abc, sp, "[Symbol.species]", |_, this, _, _| Ok(this));
    vm.method(abc, "isView", 1, |vm, _, a, _| Ok(Value::bool(matches!(arg(a, 0).as_object().map(|o| &vm.obj(o).class), Some(Class::TypedArray(_) | Class::DataView(_))))));
    vm.getter(abp, "byteLength", |vm, this, _, _| {
        let o = this_buffer(vm, this, false)?;
        Ok(Value::from_usize(buf(vm, o).data.len()))
    }, None);
    vm.getter(abp, "maxByteLength", |vm, this, _, _| {
        let o = this_buffer(vm, this, false)?;
        let b = buf(vm, o);
        Ok(Value::from_usize(if b.detached { 0 } else { b.max_len.unwrap_or(b.data.len()) }))
    }, None);
    vm.getter(abp, "resizable", |vm, this, _, _| {
        let o = this_buffer(vm, this, false)?;
        Ok(Value::bool(buf(vm, o).max_len.is_some()))
    }, None);
    vm.getter(abp, "detached", |vm, this, _, _| {
        let o = this_buffer(vm, this, false)?;
        Ok(Value::bool(buf(vm, o).detached))
    }, None);
    vm.method(abp, "slice", 2, |vm, this, a, _| buffer_slice(vm, this, a, false));
    vm.method(abp, "resize", 1, |vm, this, a, _| {
        let o = this_buffer(vm, this, false)?;
        let Some(max) = buf(vm, o).max_len else { return Err(vm.type_error("ArrayBuffer is not resizable")) };
        let n = vm.to_index(arg(a, 0))? as usize;
        if buf(vm, o).detached {
            return Err(vm.type_error("ArrayBuffer is detached"));
        }
        if n > max {
            return Err(vm.range_error("Invalid length for resize"));
        }
        buf_mut(vm, o).data.resize(n, 0);
        Ok(Value::UNDEFINED)
    });
    vm.method(abp, "transfer", 0, |vm, this, a, _| transfer(vm, this, a, true));
    vm.method(abp, "transferToFixedLength", 0, |vm, this, a, _| transfer(vm, this, a, false));
    let tag = vm.new_string("ArrayBuffer");
    vm.define_data(abp, kt, tag, attr::CONFIGURABLE);
    // SharedArrayBuffer
    let sabp = vm.alloc_object(Some(op), Class::Ordinary);
    vm.intrinsics.shared_arraybuffer_prototype = sabp;
    let sabc = vm.constructor("SharedArrayBuffer", 1, |vm, _, a, nt| buffer_ctor(vm, a, nt, true), sabp);
    vm.intrinsics.shared_arraybuffer = sabc;
    vm.global_prop("SharedArrayBuffer", Value::object(sabc));
    vm.getter_sym(sabc, sp, "[Symbol.species]", |_, this, _, _| Ok(this));
    vm.getter(sabp, "byteLength", |vm, this, _, _| {
        let o = this_buffer(vm, this, true)?;
        Ok(Value::from_usize(buf(vm, o).data.len()))
    }, None);
    vm.getter(sabp, "maxByteLength", |vm, this, _, _| {
        let o = this_buffer(vm, this, true)?;
        let b = buf(vm, o);
        Ok(Value::from_usize(b.max_len.unwrap_or(b.data.len())))
    }, None);
    vm.getter(sabp, "growable", |vm, this, _, _| {
        let o = this_buffer(vm, this, true)?;
        Ok(Value::bool(buf(vm, o).max_len.is_some()))
    }, None);
    vm.method(sabp, "grow", 1, |vm, this, a, _| {
        let o = this_buffer(vm, this, true)?;
        let Some(max) = buf(vm, o).max_len else { return Err(vm.type_error("SharedArrayBuffer is not growable")) };
        let n = vm.to_index(arg(a, 0))? as usize;
        if n > max || n < buf(vm, o).data.len() {
            return Err(vm.range_error("Invalid length for grow"));
        }
        buf_mut(vm, o).data.resize(n, 0);
        Ok(Value::UNDEFINED)
    });
    vm.method(sabp, "slice", 2, |vm, this, a, _| buffer_slice(vm, this, a, true));
    let tag = vm.new_string("SharedArrayBuffer");
    vm.define_data(sabp, kt, tag, attr::CONFIGURABLE);

    // %TypedArray%
    let fp = vm.intrinsics.function_prototype;
    let tap = vm.alloc_object(Some(op), Class::Ordinary);
    vm.intrinsics.typed_array_prototype = tap;
    let tac = vm.constructor("TypedArray", 0, |vm, _, _, _| Err(vm.type_error("Abstract class TypedArray not directly constructable")), tap);
    vm.obj_mut(tac).proto = Some(fp);
    vm.intrinsics.typed_array = tac;
    vm.getter_sym(tac, sp, "[Symbol.species]", |_, this, _, _| Ok(this));
    vm.getter(tap, "buffer", |vm, this, _, _| {
        let o = this_ta(vm, this)?;
        Ok(Value::object(ta(vm, o).buffer))
    }, None);
    vm.getter(tap, "byteLength", |vm, this, _, _| {
        let o = this_ta(vm, this)?;
        let n = ta_len(vm, o).unwrap_or(0);
        Ok(Value::from_usize(n * elem_size(ta(vm, o).kind)))
    }, None);
    vm.getter(tap, "byteOffset", |vm, this, _, _| {
        let o = this_ta(vm, this)?;
        Ok(Value::from_usize(if ta_len(vm, o).is_none() { 0 } else { ta(vm, o).offset }))
    }, None);
    vm.getter(tap, "length", |vm, this, _, _| {
        let o = this_ta(vm, this)?;
        Ok(Value::from_usize(ta_len(vm, o).unwrap_or(0)))
    }, None);
    let gt = vm.native_fn("get [Symbol.toStringTag]", 0, |vm, this, _, _| {
        match this.as_object().map(|o| &vm.obj(o).class) {
            Some(Class::TypedArray(t)) => {
                let k = t.kind;
                Ok(vm.new_string(KINDS[k as usize].0))
            }
            _ => Ok(Value::UNDEFINED),
        }
    });
    vm.define_accessor(tap, kt, Value::object(gt), Value::UNDEFINED, attr::CONFIGURABLE);
    // Iterators: share the array iterator.
    vm.method(tap, "keys", 0, |vm, this, _, _| {
        let (o, _) = validate(vm, this)?;
        Ok(super::array::make_array_iterator(vm, Value::object(o), 0))
    });
    vm.method(tap, "entries", 0, |vm, this, _, _| {
        let (o, _) = validate(vm, this)?;
        Ok(super::array::make_array_iterator(vm, Value::object(o), 2))
    });
    let values = vm.method(tap, "values", 0, |vm, this, _, _| {
        let (o, _) = validate(vm, this)?;
        Ok(super::array::make_array_iterator(vm, Value::object(o), 1))
    });
    let si = vm.intrinsics.sym_iterator;
    vm.define_data(tap, Key::Sym(si), Value::object(values), attr::HIDDEN);
    vm.method(tap, "set", 1, ta_set_method);
    vm.method(tap, "subarray", 2, ta_subarray);
    super::typed_array_methods::init(vm, tap, tac);

    // Concrete constructors.
    for (k, (name, size)) in KINDS.iter().enumerate() {
        let p = vm.alloc_object(Some(tap), Class::Ordinary);
        let c = vm.native_fn_data(name, 3, ta_ctor, true, Box::new([Value::int(k as i32)]));
        let kp = Key::Str(vm.names.prototype);
        vm.define_data(c, kp, Value::object(p), attr::NONE);
        let kc = Key::Str(vm.names.constructor);
        vm.define_data(p, kc, Value::object(c), attr::HIDDEN);
        vm.obj_mut(c).proto = Some(tac);
        vm.prop(c, "BYTES_PER_ELEMENT", Value::from_usize(*size), attr::NONE);
        vm.prop(p, "BYTES_PER_ELEMENT", Value::from_usize(*size), attr::NONE);
        vm.global_prop(name, Value::object(c));
        let i = &mut vm.intrinsics;
        match k {
            0 => (i.int8_array, i.int8_array_prototype) = (c, p),
            1 => (i.uint8_array, i.uint8_array_prototype) = (c, p),
            2 => (i.uint8_clamped_array, i.uint8_clamped_array_prototype) = (c, p),
            3 => (i.int16_array, i.int16_array_prototype) = (c, p),
            4 => (i.uint16_array, i.uint16_array_prototype) = (c, p),
            5 => (i.int32_array, i.int32_array_prototype) = (c, p),
            6 => (i.uint32_array, i.uint32_array_prototype) = (c, p),
            7 => (i.float16_array, i.float16_array_prototype) = (c, p),
            8 => (i.float32_array, i.float32_array_prototype) = (c, p),
            9 => (i.float64_array, i.float64_array_prototype) = (c, p),
            10 => (i.bigint64_array, i.bigint64_array_prototype) = (c, p),
            _ => (i.biguint64_array, i.biguint64_array_prototype) = (c, p),
        }
    }
    super::base64::init(vm);

    // DataView
    let dvp = vm.alloc_object(Some(op), Class::Ordinary);
    vm.intrinsics.dataview_prototype = dvp;
    let dvc = vm.constructor("DataView", 1, dataview_ctor, dvp);
    vm.global_prop("DataView", Value::object(dvc));
    vm.getter(dvp, "buffer", |vm, this, _, _| Ok(Value::object(view(vm, this)?.0)), None);
    vm.getter(dvp, "byteLength", |vm, this, _, _| {
        let (b, off, len) = view(vm, this)?;
        Ok(Value::from_usize(view_len(vm, b, off, len)?))
    }, None);
    vm.getter(dvp, "byteOffset", |vm, this, _, _| {
        let (b, off, len) = view(vm, this)?;
        view_len(vm, b, off, len)?;
        Ok(Value::from_usize(off))
    }, None);
    let tag = vm.new_string("DataView");
    vm.define_data(dvp, kt, tag, attr::CONFIGURABLE);
    for (k, (name, _)) in KINDS.iter().enumerate() {
        if k == 2 {
            continue;
        }
        let base = name.trim_end_matches("Array");
        let gname = alloc::format!("get{base}");
        let sname = alloc::format!("set{base}");
        let g = vm.native_fn_data(&gname, 1, view_get, false, Box::new([Value::int(k as i32)]));
        let s = vm.native_fn_data(&sname, 2, view_set, false, Box::new([Value::int(k as i32)]));
        let kg = vm.key(&gname);
        vm.define_data(dvp, kg, Value::object(g), attr::HIDDEN);
        let ks = vm.key(&sname);
        vm.define_data(dvp, ks, Value::object(s), attr::HIDDEN);
    }
    super::atomics::init(vm);
}

fn buffer_slice(vm: &mut Vm, this: Value, a: &[Value], shared: bool) -> JsResult<Value> {
    let o = this_buffer(vm, this, shared)?;
    if buf(vm, o).detached {
        return Err(vm.type_error("ArrayBuffer is detached"));
    }
    let len = buf(vm, o).data.len() as f64;
    let rel = |vm: &mut Vm, v: Value, def: f64| -> JsResult<f64> {
        if v.is_undefined() {
            return Ok(def);
        }
        let r = vm.to_integer_or_infinity(v)?;
        Ok(if r < 0.0 { (len + r).max(0.0) } else { r.min(len) })
    };
    let first = rel(vm, arg(a, 0), 0.0)?;
    let fin = rel(vm, arg(a, 1), len)?;
    let new_len = (fin - first).max(0.0) as usize;
    let default = if shared { Value::object(vm.intrinsics.shared_arraybuffer) } else { Value::object(vm.intrinsics.arraybuffer) };
    let c = super::promise::species_constructor(vm, o, default)?;
    let r = vm.construct(c, &[Value::from_usize(new_len)], c)?;
    let ro = match r.as_object() {
        Some(x) if matches!(&vm.obj(x).class, Class::ArrayBuffer(b) if b.shared == shared) => x,
        _ => return Err(vm.type_error("species constructor did not return an ArrayBuffer")),
    };
    if buf(vm, ro).detached {
        return Err(vm.type_error("new ArrayBuffer is detached"));
    }
    if ro == o {
        return Err(vm.type_error("species constructor returned the same buffer"));
    }
    if buf(vm, ro).data.len() < new_len {
        return Err(vm.type_error("new ArrayBuffer is too small"));
    }
    if buf(vm, o).detached {
        return Err(vm.type_error("ArrayBuffer is detached"));
    }
    let cur = buf(vm, o).data.len();
    let first = (first as usize).min(cur);
    let end = (first + new_len).min(cur);
    let bytes: Vec<u8> = buf(vm, o).data[first..end].to_vec();
    buf_mut(vm, ro).data[..bytes.len()].copy_from_slice(&bytes);
    Ok(r)
}

fn transfer(vm: &mut Vm, this: Value, a: &[Value], preserve: bool) -> JsResult<Value> {
    let o = this_buffer(vm, this, false)?;
    let new_len = if arg(a, 0).is_undefined() { buf(vm, o).data.len() } else { vm.to_index(arg(a, 0))? as usize };
    if buf(vm, o).detached {
        return Err(vm.type_error("ArrayBuffer is detached"));
    }
    let max = if preserve { buf(vm, o).max_len } else { None };
    if max.is_some_and(|m| new_len > m) {
        return Err(vm.range_error("Invalid length"));
    }
    let mut data = core::mem::take(&mut buf_mut(vm, o).data);
    data.resize(new_len, 0);
    buf_mut(vm, o).detached = true;
    let p = vm.intrinsics.arraybuffer_prototype;
    Ok(Value::object(vm.alloc_object(Some(p), Class::ArrayBuffer(Box::new(BufferData { data, detached: false, max_len: max, shared: false })))))
}

/// %TypedArray%.prototype.set.
fn ta_set_method(vm: &mut Vm, this: Value, a: &[Value], _: Value) -> JsResult<Value> {
    let o = this_ta(vm, this)?;
    let off = vm.to_integer_or_infinity(arg(a, 1))?;
    if off < 0.0 {
        return Err(vm.range_error("offset is out of bounds"));
    }
    let target_len = ta_length_checked(vm, o)?;
    let src = arg(a, 0);
    if let Some(so) = src.as_object() {
        if let Class::TypedArray(_) = vm.obj(so).class {
            let src_len = ta_length_checked(vm, so)?;
            if is_bigint_kind(ta(vm, so).kind) != is_bigint_kind(ta(vm, o).kind) {
                return Err(vm.type_error("Content types differ"));
            }
            if off == f64::INFINITY || src_len as f64 + off > target_len as f64 {
                return Err(vm.range_error("offset is out of bounds"));
            }
            let vals: Vec<Value> = (0..src_len).map(|i| get_element(vm, so, i)).collect();
            for (i, v) in vals.into_iter().enumerate() {
                set_element_raw(vm, o, off as usize + i, v);
            }
            return Ok(Value::UNDEFINED);
        }
    }
    let so = vm.to_object(src)?;
    vm.root(Value::object(so));
    let src_len = vm.length_of(so)?;
    if off == f64::INFINITY || src_len as f64 + off > target_len as f64 {
        return Err(vm.range_error("offset is out of bounds"));
    }
    for i in 0..src_len {
        let k = vm.index_key(i);
        let v = vm.get(so, k)?;
        set_element(vm, o, off + i as f64, v)?;
    }
    Ok(Value::UNDEFINED)
}

fn ta_subarray(vm: &mut Vm, this: Value, a: &[Value], _: Value) -> JsResult<Value> {
    let o = this_ta(vm, this)?;
    let (b, kind, base_off, tracking) = {
        let t = ta(vm, o);
        (t.buffer, t.kind, t.offset, t.length.is_none())
    };
    let src_len = ta_len(vm, o).unwrap_or(0) as f64;
    let r = vm.to_integer_or_infinity(arg(a, 0))?;
    let begin = if r < 0.0 { (src_len + r).max(0.0) } else { r.min(src_len) };
    let end_v = arg(a, 1);
    let size = elem_size(kind);
    let begin_byte = base_off + begin as usize * size;
    let args: Vec<Value> = if tracking && end_v.is_undefined() {
        alloc::vec![Value::object(b), Value::from_usize(begin_byte)]
    } else {
        let e = if end_v.is_undefined() { src_len } else { vm.to_integer_or_infinity(end_v)? };
        let end = if e < 0.0 { (src_len + e).max(0.0) } else { e.min(src_len) };
        let n = (end - begin).max(0.0);
        alloc::vec![Value::object(b), Value::from_usize(begin_byte), Value::number(n)]
    };
    let default = Value::object(kind_ctor(vm, kind));
    let c = super::promise::species_constructor(vm, o, default)?;
    let r = create_from_ctor(vm, c, &args, None)?;
    if is_bigint_kind(ta(vm, r).kind) != is_bigint_kind(kind) {
        return Err(vm.type_error("Content types differ"));
    }
    Ok(Value::object(r))
}

/// Element access for the %TypedArray%.prototype methods.
pub fn get_index(vm: &mut Vm, o: ObjId, i: usize) -> Value {
    if valid_index(vm, o, i as f64).is_some() { get_element(vm, o, i) } else { Value::UNDEFINED }
}

pub fn set_index(vm: &mut Vm, o: ObjId, i: usize, v: Value) -> JsResult<()> {
    set_element(vm, o, i as f64, v)
}

pub fn kind_of(vm: &Vm, o: ObjId) -> u8 {
    ta(vm, o).kind
}

pub fn ctor_of_kind(vm: &Vm, k: u8) -> ObjId {
    kind_ctor(vm, k)
}

pub fn proto_of_kind(vm: &Vm, k: u8) -> ObjId {
    (kind_proto(vm, k))(&vm.intrinsics)
}

/// Raw byte access for Atomics: (buffer, byte index) of element i.
pub fn element_location(vm: &Vm, o: ObjId, i: usize) -> (ObjId, usize, usize) {
    let t = ta(vm, o);
    (t.buffer, t.offset + i * elem_size(t.kind), elem_size(t.kind))
}

pub fn read_raw(vm: &mut Vm, o: ObjId, i: usize) -> Value {
    get_element(vm, o, i)
}

pub fn write_raw(vm: &mut Vm, o: ObjId, i: usize, v: Value) {
    set_element_raw(vm, o, i, v)
}

pub fn convert_for(vm: &mut Vm, o: ObjId, v: Value) -> JsResult<Value> {
    let k = ta(vm, o).kind;
    to_element(vm, k, v)
}

pub fn is_shared(vm: &Vm, o: ObjId) -> bool {
    buf(vm, ta(vm, o).buffer).shared
}

pub fn _unused(_: String) {}
