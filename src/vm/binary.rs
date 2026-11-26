//! Binary data and strings for the embedder without copies (plan §5.2,
//! defects 8 and 9): host bytes become an ArrayBuffer by moving the
//! `Vec`, JS bytes are borrowed in place or moved out by detaching.

use alloc::vec::Vec;

use super::*;
use crate::object::Class;

impl Vm {
    /// An ArrayBuffer that owns `data` (moved in, not copied).
    pub fn array_buffer_from_vec(&mut self, data: Vec<u8>) -> Value {
        Value::object(crate::builtins::buffer::buffer_from_vec(self, data))
    }

    /// The bytes of an ArrayBuffer or SharedArrayBuffer (None if `v` is
    /// not one, or detached). Valid until JS runs again.
    pub fn array_buffer_bytes(&self, v: Value) -> Option<&[u8]> {
        match &self.obj(v.as_object()?).class {
            Class::ArrayBuffer(b) if !b.detached => Some(&b.data),
            _ => None,
        }
    }

    pub fn array_buffer_bytes_mut(&mut self, v: Value) -> Option<&mut [u8]> {
        match &mut self.obj_mut(v.as_object()?).class {
            Class::ArrayBuffer(b) if !b.detached => Some(&mut b.data),
            _ => None,
        }
    }

    /// Detaches an ArrayBuffer and returns its bytes (moved out, not
    /// copied), like `transfer()` to the host. None for shared, detached
    /// or non-buffers.
    pub fn array_buffer_take(&mut self, v: Value) -> Option<Vec<u8>> {
        match &mut self.obj_mut(v.as_object()?).class {
            Class::ArrayBuffer(b) if !b.detached && !b.shared => {
                b.detached = true;
                Some(core::mem::take(&mut b.data))
            }
            _ => None,
        }
    }

    /// The bytes a typed array or DataView covers (None if `v` is not a
    /// view, or it is detached / out of bounds).
    pub fn view_bytes(&mut self, v: Value) -> Option<&[u8]> {
        let o = v.as_object()?;
        let (buffer, start, len) = match &self.obj(o).class {
            Class::TypedArray(t) => {
                let (buffer, offset) = (t.buffer, t.offset);
                let n = crate::builtins::buffer::ta_length_checked(self, o).ok()?;
                let size = if n == 0 { 0 } else { crate::builtins::buffer::element_location(self, o, 0).2 };
                (buffer, offset, n * size)
            }
            Class::DataView(d) => {
                let (buffer, offset, length) = (d.buffer, d.offset, d.length);
                let total = self.array_buffer_bytes(Value::object(buffer))?.len();
                let len = match length {
                    Some(l) => l,
                    None => total.checked_sub(offset)?,
                };
                (buffer, offset, len)
            }
            _ => return None,
        };
        self.array_buffer_bytes(Value::object(buffer))?.get(start..start + len)
    }

    /// A string from UTF-8 text (ASCII and Latin-1 text is stored one
    /// byte per unit).
    pub fn string_from_utf8(&mut self, s: &str) -> Value {
        self.new_string(s)
    }

    /// The string as `&str` when it is ASCII (no conversion, no copy).
    pub fn string_as_ascii(&self, v: Value) -> Option<&str> {
        self.str(v.as_string()?).as_ascii()
    }
}
