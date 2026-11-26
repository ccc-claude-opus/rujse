//! Inline caches for named property access (`o.x`, `o.x = v`).
//!
//! An entry records where a property was found for objects of one shape.
//! Only objects whose named properties have no exotic behavior are cached,
//! and every hit is checked against the current shapes before it is used.

use super::*;
use crate::bytecode::{Code, Ic};
use crate::object::{attr, Class, Key, Props};

/// Classes whose named (non-index) properties are plain storage.
fn plain(c: &Class) -> bool {
    matches!(c, Class::Ordinary | Class::Function(_) | Class::Native(_) | Class::Array | Class::Error)
}

impl Vm {
    /// `o.x` through a cache entry, if it applies.
    #[inline]
    pub fn prop_ic_load(&self, base: Value, ic: Ic) -> Option<Value> {
        let o = base.as_object()?;
        let ob = self.obj(o);
        match ic {
            Ic::Slot { shape, slot, .. } => match &ob.props {
                Props::Shaped { shape: s, slots } if *s == shape => slots.get(slot as usize).copied(),
                _ => None,
            },
            Ic::Proto { shape, proto, proto_shape, slot } => {
                match &ob.props {
                    Props::Shaped { shape: s, .. } if *s == shape && ob.proto == Some(proto) => {}
                    _ => return None,
                }
                match &self.obj(proto).props {
                    Props::Shaped { shape: s, slots } if *s == proto_shape => slots.get(slot as usize).copied(),
                    _ => None,
                }
            }
            _ => None,
        }
    }

    /// `o.x = v` through a cache entry (existing writable data property).
    #[inline]
    pub fn prop_ic_store(&mut self, base: Value, ic: Ic, v: Value) -> bool {
        let Ic::Slot { shape, slot, writable: true, .. } = ic else { return false };
        let Some(o) = base.as_object() else { return false };
        match &mut self.obj_mut(o).props {
            Props::Shaped { shape: s, slots } if *s == shape => match slots.get_mut(slot as usize) {
                Some(x) => {
                    *x = v;
                    true
                }
                None => false,
            },
            _ => false,
        }
    }

    /// Where `key` is found on `base` now, as a cache entry.
    fn prop_ic_entry(&self, base: Value, key: Key) -> Ic {
        let Some(o) = base.as_object() else { return Ic::Empty };
        if let Key::Index(_) = key {
            return Ic::Empty;
        }
        let ob = self.obj(o);
        if !plain(&ob.class) {
            return Ic::Empty;
        }
        if ob.is_array() && key == Key::Str(self.names.length) {
            return Ic::Empty;
        }
        let Props::Shaped { shape, .. } = &ob.props else { return Ic::Empty };
        match self.shape_lookup(*shape, key) {
            Some((slot, a)) if a & attr::ACCESSOR == 0 => {
                Ic::Slot { shape: *shape, slot, epoch: 0, writable: a & attr::WRITABLE != 0 }
            }
            Some(_) => Ic::Empty,
            None => {
                let Some(p) = ob.proto else { return Ic::Empty };
                let pb = self.obj(p);
                if !plain(&pb.class) || (pb.is_array() && key == Key::Str(self.names.length)) {
                    return Ic::Empty;
                }
                let Props::Shaped { shape: ps, .. } = &pb.props else { return Ic::Empty };
                match self.shape_lookup(*ps, key) {
                    Some((slot, a)) if a & attr::ACCESSOR == 0 => Ic::Proto { shape: *shape, proto: p, proto_shape: *ps, slot },
                    _ => Ic::Empty,
                }
            }
        }
    }

    /// `GetNamed` slow path: the full lookup, then cache where it was.
    pub fn get_named_cached(&mut self, code: &Code, k: usize, base: Value, key: Key) -> JsResult<Value> {
        let v = self.get_value_prop(base, key)?;
        let e = self.prop_ic_entry(base, key);
        if e != Ic::Empty {
            code.ics[k].set(e);
        }
        Ok(v)
    }

    /// `SetNamed` slow path.
    pub fn set_named_cached(&mut self, code: &Code, k: usize, base: Value, key: Key, v: Value) -> JsResult<()> {
        self.put_value_prop(base, key, v, code.strict)?;
        if let e @ Ic::Slot { writable: true, .. } = self.prop_ic_entry(base, key) {
            code.ics[k].set(e);
        }
        Ok(())
    }

    /// `DefineNamed` (object literals, class fields) through an `Add`
    /// entry: a new data property with default attributes.
    #[inline]
    pub fn define_ic(&mut self, o: ObjId, ic: Ic, v: Value) -> bool {
        let Ic::Add { from, to } = ic else { return false };
        let ob = self.obj_mut(o);
        if !matches!(ob.class, Class::Ordinary) || !ob.extensible {
            return false;
        }
        match &mut ob.props {
            Props::Shaped { shape, slots } if *shape == from => {
                *shape = to;
                slots.push(v);
                true
            }
            _ => false,
        }
    }

    /// `DefineNamed` slow path: CreateDataPropertyOrThrow, then cache the
    /// shape transition it made.
    pub fn define_named_cached(&mut self, code: &Code, k: usize, o: ObjId, key: Key, v: Value) -> JsResult<()> {
        let before = match &self.obj(o).props {
            Props::Shaped { shape, .. } if matches!(self.obj(o).class, Class::Ordinary) && self.obj(o).extensible => Some(*shape),
            _ => None,
        };
        self.create_data_property_or_throw(o, key, v)?;
        if let (Some(from), Props::Shaped { shape: to, .. }) = (before, &self.obj(o).props) {
            let to = *to;
            let s = self.shape(to);
            if s.parent == Some(from) && s.key == Some(key) && s.attrs == attr::DEFAULT {
                code.ics[k].set(Ic::Add { from, to });
            }
        }
        Ok(())
    }
}
