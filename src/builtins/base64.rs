//! Uint8Array base64/hex: fromBase64, fromHex, toBase64, toHex,
//! setFromBase64, setFromHex.

use alloc::vec::Vec;

use super::buffer;
use super::object::arg;
use crate::object::Key;
use crate::value::{ObjId, Value};
use crate::vm::{JsResult, Vm};

const STD: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
const URL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

fn this_u8(vm: &mut Vm, v: Value) -> JsResult<(ObjId, usize)> {
    let (o, len) = buffer::validate(vm, v)?;
    if buffer::kind_of(vm, o) != 1 {
        return Err(vm.type_error("not a Uint8Array"));
    }
    Ok((o, len))
}

fn bytes_of(vm: &mut Vm, o: ObjId, len: usize) -> Vec<u8> {
    (0..len).map(|i| buffer::get_index(vm, o, i).as_int().unwrap_or(0) as u8).collect()
}

/// Reads `alphabet` and `lastChunkHandling` options.
fn options(vm: &mut Vm, opts: Value) -> JsResult<(bool, u8)> {
    if opts.is_undefined() {
        return Ok((false, 0));
    }
    let Some(o) = opts.as_object() else { return Err(vm.type_error("options must be an object")) };
    let ka = vm.key("alphabet");
    let a = vm.get(o, ka)?;
    let url = if a.is_undefined() {
        false
    } else {
        match a.as_string().map(|s| vm.str(s).to_rust_lossy()) {
            Some(s) if s == "base64" => false,
            Some(s) if s == "base64url" => true,
            _ => return Err(vm.type_error("invalid alphabet")),
        }
    };
    let kl = vm.key("lastChunkHandling");
    let l = vm.get(o, kl)?;
    let lch = if l.is_undefined() {
        0
    } else {
        match l.as_string().map(|s| vm.str(s).to_rust_lossy()) {
            Some(s) if s == "loose" => 0,
            Some(s) if s == "strict" => 1,
            Some(s) if s == "stop-before-partial" => 2,
            _ => return Err(vm.type_error("invalid lastChunkHandling")),
        }
    };
    Ok((url, lch))
}

/// FromBase64: (bytes, read count, error).
fn decode64(s: &[u16], url: bool, lch: u8, max: usize) -> (Vec<u8>, usize, bool) {
    let alpha = if url { URL } else { STD };
    let val = |c: u16| alpha.iter().position(|&a| a as u16 == c).map(|p| p as u8);
    let mut out = Vec::new();
    let mut chunk: Vec<u8> = Vec::new();
    let mut read = 0;
    let mut i = 0;
    let ws = |c: u16| matches!(c, 0x09 | 0x0A | 0x0C | 0x0D | 0x20);
    if max == 0 {
        return (out, 0, false);
    }
    loop {
        while i < s.len() && ws(s[i]) {
            i += 1;
        }
        if i == s.len() {
            if !chunk.is_empty() {
                if lch == 2 {
                    return (out, read, false);
                }
                if lch == 1 || chunk.len() == 1 {
                    return (out, read, true);
                }
                let bytes = decode_chunk(&chunk, false);
                match bytes {
                    Some(b) => out.extend(b),
                    None => return (out, read, true),
                }
            }
            return (out, s.len(), false);
        }
        let c = s[i];
        i += 1;
        if c == b'=' as u16 {
            if chunk.len() < 2 {
                return (out, read, true);
            }
            while i < s.len() && ws(s[i]) {
                i += 1;
            }
            if chunk.len() == 2 {
                if i == s.len() {
                    if lch == 2 {
                        return (out, read, false);
                    }
                    return (out, read, true);
                }
                if s[i] != b'=' as u16 {
                    return (out, read, true);
                }
                i += 1;
                while i < s.len() && ws(s[i]) {
                    i += 1;
                }
            }
            if i < s.len() {
                return (out, read, true);
            }
            match decode_chunk(&chunk, lch == 1) {
                Some(b) => out.extend(b),
                None => return (out, read, true),
            }
            return (out, s.len(), false);
        }
        let Some(v) = val(c) else { return (out, read, true) };
        let remaining = max - out.len();
        if (remaining == 1 && chunk.len() == 2) || (remaining == 2 && chunk.len() == 3) {
            return (out, read, false);
        }
        chunk.push(v);
        if chunk.len() == 4 {
            out.extend(decode_chunk(&chunk, false).unwrap());
            chunk.clear();
            read = i;
            if out.len() == max {
                return (out, read, false);
            }
        }
    }
}

fn decode_chunk(c: &[u8], strict: bool) -> Option<Vec<u8>> {
    let mut n: u32 = 0;
    for (i, v) in c.iter().enumerate() {
        n |= (*v as u32) << (18 - 6 * i);
    }
    let b = [(n >> 16) as u8, (n >> 8) as u8, n as u8];
    match c.len() {
        2 => {
            if strict && (n >> 8) & 0xFF != 0 {
                return None;
            }
            Some(b[..1].to_vec())
        }
        3 => {
            if strict && n & 0xFF != 0 {
                return None;
            }
            Some(b[..2].to_vec())
        }
        _ => Some(b.to_vec()),
    }
}

fn decode_hex(s: &[u16], max: usize) -> (Vec<u8>, usize, bool) {
    let mut out = Vec::new();
    if s.len() % 2 != 0 {
        return (out, 0, true);
    }
    let mut read = 0;
    while read < s.len() && out.len() < max {
        let h = |c: u16| char::from_u32(c as u32).and_then(|x| x.to_digit(16));
        match (h(s[read]), h(s[read + 1])) {
            (Some(a), Some(b)) => out.push((a * 16 + b) as u8),
            _ => return (out, read, true),
        }
        read += 2;
    }
    (out, read, false)
}

pub fn init(vm: &mut Vm) {
    let c = vm.intrinsics.uint8_array;
    let p = vm.intrinsics.uint8_array_prototype;
    vm.method(c, "fromBase64", 1, |vm, _, a, _| {
        let Some(s) = arg(a, 0).as_string() else { return Err(vm.type_error("fromBase64 requires a string")) };
        let (url, lch) = options(vm, arg(a, 1))?;
        let u = vm.str(s).to_units();
        let (bytes, _, err) = decode64(&u, url, lch, usize::MAX);
        if err {
            return Err(vm.syntax_error("invalid base64 string"));
        }
        let p = vm.intrinsics.uint8_array_prototype;
        let o = buffer::create_ta(vm, p, 1, bytes.len())?;
        for (i, b) in bytes.into_iter().enumerate() {
            buffer::set_index(vm, o, i, Value::int(b as i32))?;
        }
        Ok(Value::object(o))
    });
    vm.method(c, "fromHex", 1, |vm, _, a, _| {
        let Some(s) = arg(a, 0).as_string() else { return Err(vm.type_error("fromHex requires a string")) };
        let u = vm.str(s).to_units();
        let (bytes, _, err) = decode_hex(&u, usize::MAX);
        if err {
            return Err(vm.syntax_error("invalid hex string"));
        }
        let p = vm.intrinsics.uint8_array_prototype;
        let o = buffer::create_ta(vm, p, 1, bytes.len())?;
        for (i, b) in bytes.into_iter().enumerate() {
            buffer::set_index(vm, o, i, Value::int(b as i32))?;
        }
        Ok(Value::object(o))
    });
    vm.method(p, "toBase64", 0, |vm, this, a, _| {
        let (o, _) = this_u8(vm, this)?;
        let opts = arg(a, 0);
        let (url, _) = options(vm, opts)?;
        let mut omit = false;
        if let Some(oo) = opts.as_object() {
            let k = vm.key("omitPadding");
            let v = vm.get(oo, k)?;
            omit = vm.to_boolean(v);
        }
        let len = buffer::ta_length_checked(vm, o)?;
        let bytes = bytes_of(vm, o, len);
        let alpha = if url { URL } else { STD };
        let mut out = alloc::string::String::new();
        for ch in bytes.chunks(3) {
            let n = (ch[0] as u32) << 16 | (*ch.get(1).unwrap_or(&0) as u32) << 8 | *ch.get(2).unwrap_or(&0) as u32;
            for k in 0..4 {
                if k <= ch.len() {
                    out.push(alpha[((n >> (18 - 6 * k)) & 63) as usize] as char);
                } else if !omit {
                    out.push('=');
                }
            }
        }
        Ok(vm.new_string(&out))
    });
    vm.method(p, "toHex", 0, |vm, this, _, _| {
        let (o, len) = this_u8(vm, this)?;
        let bytes = bytes_of(vm, o, len);
        let s: alloc::string::String = bytes.iter().map(|b| alloc::format!("{b:02x}")).collect();
        Ok(vm.new_string(&s))
    });
    vm.method(p, "setFromBase64", 1, |vm, this, a, _| {
        let (o, _) = this_u8(vm, this)?;
        let Some(s) = arg(a, 0).as_string() else { return Err(vm.type_error("setFromBase64 requires a string")) };
        let (url, lch) = options(vm, arg(a, 1))?;
        let len = buffer::ta_length_checked(vm, o)?;
        let u = vm.str(s).to_units();
        let (bytes, read, err) = decode64(&u, url, lch, len);
        for (i, b) in bytes.iter().enumerate() {
            buffer::set_index(vm, o, i, Value::int(*b as i32))?;
        }
        if err {
            return Err(vm.syntax_error("invalid base64 string"));
        }
        result(vm, read, bytes.len())
    });
    vm.method(p, "setFromHex", 1, |vm, this, a, _| {
        let (o, _) = this_u8(vm, this)?;
        let Some(s) = arg(a, 0).as_string() else { return Err(vm.type_error("setFromHex requires a string")) };
        let len = buffer::ta_length_checked(vm, o)?;
        let u = vm.str(s).to_units();
        let (bytes, read, err) = decode_hex(&u, len);
        for (i, b) in bytes.iter().enumerate() {
            buffer::set_index(vm, o, i, Value::int(*b as i32))?;
        }
        if err {
            return Err(vm.syntax_error("invalid hex string"));
        }
        result(vm, read, bytes.len())
    });
}

fn result(vm: &mut Vm, read: usize, written: usize) -> JsResult<Value> {
    let r = vm.new_object();
    let kr = vm.key("read");
    vm.create_data_property_or_throw(r, kr, Value::from_usize(read))?;
    let kw = vm.key("written");
    vm.create_data_property_or_throw(r, kw, Value::from_usize(written))?;
    let _ = Key::Index(0);
    Ok(Value::object(r))
}
