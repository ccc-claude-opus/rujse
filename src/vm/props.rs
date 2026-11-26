//! Object internal methods ([[GetOwnProperty]], [[DefineOwnProperty]],
//! [[Get]], [[Set]], [[Delete]], [[OwnPropertyKeys]], …) for ordinary
//! objects and the exotic ones (arrays, string objects, arguments, typed
//! arrays, proxies, module namespaces).

use alloc::boxed::Box;
use alloc::vec::Vec;

use super::{JsResult, Vm};
use crate::object::{attr, Class, Dict, Key, Props, DICT_THRESHOLD};
use crate::value::{ObjId, Value};

/// A property descriptor (fields absent = `None`).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PropDesc {
    pub value: Option<Value>,
    pub get: Option<Value>,
    pub set: Option<Value>,
    pub writable: Option<bool>,
    pub enumerable: Option<bool>,
    pub configurable: Option<bool>,
}

impl PropDesc {
    pub fn data(value: Value, attrs: u8) -> PropDesc {
        PropDesc {
            value: Some(value),
            get: None,
            set: None,
            writable: Some(attrs & attr::WRITABLE != 0),
            enumerable: Some(attrs & attr::ENUMERABLE != 0),
            configurable: Some(attrs & attr::CONFIGURABLE != 0),
        }
    }

    pub fn accessor(get: Value, set: Value, attrs: u8) -> PropDesc {
        PropDesc {
            value: None,
            get: Some(get),
            set: Some(set),
            writable: None,
            enumerable: Some(attrs & attr::ENUMERABLE != 0),
            configurable: Some(attrs & attr::CONFIGURABLE != 0),
        }
    }

    pub fn is_accessor(&self) -> bool {
        self.get.is_some() || self.set.is_some()
    }

    pub fn is_data(&self) -> bool {
        self.value.is_some() || self.writable.is_some()
    }

    pub fn is_generic(&self) -> bool {
        !self.is_accessor() && !self.is_data()
    }

    pub fn attrs(&self) -> u8 {
        let mut a = 0;
        if self.writable == Some(true) {
            a |= attr::WRITABLE;
        }
        if self.enumerable == Some(true) {
            a |= attr::ENUMERABLE;
        }
        if self.configurable == Some(true) {
            a |= attr::CONFIGURABLE;
        }
        if self.is_accessor() {
            a |= attr::ACCESSOR;
        }
        a
    }
}

/// A stored property: data value or accessor pair, with attributes.
#[derive(Clone, Copy, Debug)]
pub enum Slot {
    Data(Value, u8),
    Accessor(Value, Value, u8),
}

impl Vm {
    // ----- raw storage (ordinary properties, no exotic behavior) ---------

    fn element_attrs(&self, o: ObjId) -> u8 {
        match self.obj(o).frozen_elements {
            0 => attr::DEFAULT,
            1 => attr::WRITABLE | attr::ENUMERABLE,
            _ => attr::ENUMERABLE,
        }
    }

    /// The own property stored on the object itself (ignores exotics).
    pub fn raw_get_own(&self, o: ObjId, k: Key) -> Option<Slot> {
        let ob = self.obj(o);
        if let Key::Index(i) = k {
            if let Some(&v) = ob.elements.get(i as usize) {
                if !v.is_hole() {
                    return Some(Slot::Data(v, self.element_attrs(o)));
                }
            }
            if !ob.sparse {
                return None;
            }
        }
        let (v, a) = match &ob.props {
            Props::Shaped { shape, slots } => {
                let (slot, a) = self.shape_lookup(*shape, k)?;
                (slots[slot as usize], a)
            }
            Props::Dict(d) => d.get(k)?,
        };
        Some(self.to_slot(v, a))
    }

    fn to_slot(&self, v: Value, a: u8) -> Slot {
        if a & attr::ACCESSOR != 0 {
            if let Some(p) = v.as_object() {
                if let Class::AccessorPair { get, set } = self.obj(p).class {
                    return Slot::Accessor(get, set, a & !attr::ACCESSOR);
                }
            }
            Slot::Accessor(Value::UNDEFINED, Value::UNDEFINED, a & !attr::ACCESSOR)
        } else {
            Slot::Data(v, a)
        }
    }

    /// Stores a property (creating or replacing), ignoring exotics and
    /// extensibility.
    pub fn raw_put(&mut self, o: ObjId, k: Key, slot: Slot) {
        let (v, a) = match slot {
            Slot::Data(v, a) => (v, a),
            Slot::Accessor(g, s, a) => {
                let pair = self.alloc_object(None, Class::AccessorPair { get: g, set: s });
                (Value::object(pair), a | attr::ACCESSOR)
            }
        };
        if let Key::Index(i) = k {
            let ea = self.element_attrs(o);
            let ob = self.obj_mut(o);
            let i = i as usize;
            let dense_ok = a == ea && ob.frozen_elements == 0 || (a == ea && i < ob.elements.len());
            if dense_ok && (i < ob.elements.len() || i <= ob.elements.len() + 64 || i < ob.elements.len() * 2) && i < (1 << 26) {
                if i >= ob.elements.len() {
                    let cap = ob.elements.capacity();
                    ob.elements.resize(i + 1, Value::HOLE);
                    let grown = ob.elements.capacity() - cap;
                    self.alloc_bytes += grown * 8;
                    let ob = self.obj_mut(o);
                    ob.elements[i] = v;
                    if ob.sparse {
                        self.raw_remove_prop(o, k);
                    }
                    return;
                }
                ob.elements[i] = v;
                if ob.sparse {
                    // A stale entry in props must go.
                    self.raw_remove_prop(o, k);
                }
                return;
            }
            // Special attributes or very sparse: in props.
            if let Some(e) = ob.elements.get_mut(i) {
                *e = Value::HOLE;
            }
            ob.sparse = true;
        }
        self.raw_put_prop(o, k, v, a);
    }

    fn raw_put_prop(&mut self, o: ObjId, k: Key, v: Value, a: u8) {
        let lookup = match &self.obj(o).props {
            Props::Shaped { shape, .. } => self.shape_lookup(*shape, k),
            Props::Dict(_) => None,
        };
        match &self.obj(o).props {
            Props::Shaped { shape, .. } => {
                let shape = *shape;
                match lookup {
                    Some((slot, old)) if old == a => {
                        if let Props::Shaped { slots, .. } = &mut self.obj_mut(o).props {
                            slots[slot as usize] = v;
                        }
                    }
                    Some(_) => {
                        self.to_dict(o);
                        self.raw_put_prop(o, k, v, a);
                    }
                    None => {
                        if self.shape(shape).len >= DICT_THRESHOLD {
                            self.to_dict(o);
                            return self.raw_put_prop(o, k, v, a);
                        }
                        let ns = self.shape_add(shape, k, a);
                        if let Props::Shaped { shape, slots } = &mut self.obj_mut(o).props {
                            *shape = ns;
                            slots.push(v);
                        }
                    }
                }
            }
            Props::Dict(_) => {
                if let Props::Dict(d) = &mut self.obj_mut(o).props {
                    d.set(k, v, a);
                }
            }
        }
    }

    fn raw_remove_prop(&mut self, o: ObjId, k: Key) -> bool {
        let present = match &self.obj(o).props {
            Props::Shaped { shape, .. } => self.shape_lookup(*shape, k).is_some(),
            Props::Dict(d) => d.get(k).is_some(),
        };
        if !present {
            return false;
        }
        // Deleting the last-added property keeps the shape (common for
        // stack-like use); otherwise go to dictionary mode.
        if let Props::Shaped { shape, .. } = &self.obj(o).props {
            let s = self.shape(*shape);
            if s.key == Some(k) {
                let parent = s.parent.unwrap();
                if let Props::Shaped { shape, slots } = &mut self.obj_mut(o).props {
                    *shape = parent;
                    slots.pop();
                }
                return true;
            }
            self.to_dict(o);
        }
        if let Props::Dict(d) = &mut self.obj_mut(o).props {
            d.remove(k);
        }
        true
    }

    /// Removes an own property from storage.
    pub fn raw_delete(&mut self, o: ObjId, k: Key) -> bool {
        if let Key::Index(i) = k {
            let ob = self.obj_mut(o);
            if let Some(e) = ob.elements.get_mut(i as usize) {
                if !e.is_hole() {
                    *e = Value::HOLE;
                    if i as usize + 1 == ob.elements.len() {
                        while ob.elements.last().is_some_and(|v| v.is_hole()) {
                            ob.elements.pop();
                        }
                    }
                    return true;
                }
            }
            if !self.obj(o).sparse {
                return false;
            }
        }
        self.raw_remove_prop(o, k)
    }

    /// Defines a data property directly (built-in setup, object literals).
    pub fn define_data(&mut self, o: ObjId, k: Key, v: Value, attrs: u8) {
        self.raw_put(o, k, Slot::Data(v, attrs));
    }

    /// Defines an accessor directly (built-in setup).
    pub fn define_accessor(&mut self, o: ObjId, k: Key, get: Value, set: Value, attrs: u8) {
        self.raw_put(o, k, Slot::Accessor(get, set, attrs));
    }

    /// Own keys of the storage in spec order: indices ascending, then
    /// strings, then symbols (each in insertion order).
    pub fn raw_own_keys(&self, o: ObjId) -> Vec<Key> {
        let ob = self.obj(o);
        let mut idx: Vec<u32> = Vec::new();
        for (i, v) in ob.elements.iter().enumerate() {
            if !v.is_hole() {
                idx.push(i as u32);
            }
        }
        let named: Vec<Key> = match &ob.props {
            Props::Shaped { shape, .. } => self.shape_keys(*shape).into_iter().map(|(k, _, _)| k).collect(),
            Props::Dict(d) => d.entries.iter().flatten().map(|e| e.0).collect(),
        };
        if ob.sparse {
            for k in &named {
                if let Key::Index(i) = k {
                    idx.push(*i);
                }
            }
            idx.sort_unstable();
            idx.dedup();
        }
        let mut out: Vec<Key> = idx.into_iter().map(Key::Index).collect();
        out.extend(named.iter().filter(|k| matches!(k, Key::Str(_))));
        out.extend(named.iter().filter(|k| match k {
            Key::Sym(s) => !self.symbols.get(s.0).private,
            _ => false,
        }));
        out
    }

    // ----- [[GetOwnProperty]] ------------------------------------------

    pub fn get_own_property(&mut self, o: ObjId, k: Key) -> JsResult<Option<PropDesc>> {
        match &self.obj(o).class {
            Class::Proxy { .. } => return crate::builtins::proxy::get_own_property(self, o, k),
            Class::TypedArray(_) => {
                if let Some(r) = crate::builtins::buffer::ta_get_own(self, o, k) {
                    return Ok(r);
                }
            }
            Class::Namespace(_) => return crate::module::ns_get_own(self, o, k),
            _ => {}
        }
        Ok(self.ordinary_get_own(o, k))
    }

    /// [[GetOwnProperty]] of non-proxy objects (with array, string and
    /// arguments exotics).
    pub fn ordinary_get_own(&mut self, o: ObjId, k: Key) -> Option<PropDesc> {
        match &self.obj(o).class {
            Class::Array if k == Key::Str(self.names.length) => {
                let ob = self.obj(o);
                let a = if ob.length_readonly { attr::NONE } else { attr::WRITABLE };
                return Some(PropDesc::data(Value::from_u32(ob.length), a));
            }
            Class::String(s) => {
                let s = *s;
                if k == Key::Str(self.names.length) {
                    return Some(PropDesc::data(Value::from_usize(self.str(s).len()), attr::NONE));
                }
                if let Key::Index(i) = k {
                    if (i as usize) < self.str(s).len() {
                        let c = self.str(s).at(i as usize);
                        let v = self.char_string(c);
                        return Some(PropDesc::data(v, attr::ENUMERABLE));
                    }
                }
            }
            Class::Arguments(Some(m)) => {
                if let Key::Index(i) = k {
                    if let Some(&slot) = m.map.get(i as usize) {
                        if slot != u16::MAX {
                            let env = m.env;
                            let v = self.envs.get(env.0).slots[slot as usize];
                            return match self.raw_get_own(o, k) {
                                Some(Slot::Data(_, a)) => Some(PropDesc::data(v, a)),
                                _ => Some(PropDesc::data(v, attr::DEFAULT)),
                            };
                        }
                    }
                }
            }
            _ => {}
        }
        match self.raw_get_own(o, k)? {
            Slot::Data(v, a) => Some(PropDesc::data(v, a)),
            Slot::Accessor(g, s, a) => Some(PropDesc::accessor(g, s, a)),
        }
    }

    /// A one-unit string (cached for Latin-1).
    pub fn char_string(&mut self, c: u16) -> Value {
        if c < 0x80 {
            return Value::str(self.names.ascii[c as usize]);
        }
        self.new_string_units(&[c])
    }

    // ----- [[DefineOwnProperty]] ---------------------------------------

    pub fn define_own_property(&mut self, o: ObjId, k: Key, d: PropDesc) -> JsResult<bool> {
        self.poll_limits()?;
        match &self.obj(o).class {
            Class::Proxy { .. } => return crate::builtins::proxy::define_own_property(self, o, k, d),
            Class::TypedArray(_) => {
                if let Some(r) = crate::builtins::buffer::ta_define_own(self, o, k, d)? {
                    return Ok(r);
                }
            }
            Class::Namespace(_) => return crate::module::ns_define_own(self, o, k, d),
            Class::Array => {
                if k == Key::Str(self.names.length) {
                    return self.array_set_length(o, d);
                }
                if let Key::Index(i) = k {
                    let (len, ro) = (self.obj(o).length, self.obj(o).length_readonly);
                    if i >= len && ro {
                        return Ok(false);
                    }
                    let ok = self.ordinary_define(o, k, d)?;
                    if ok && i >= len {
                        self.obj_mut(o).length = i + 1;
                    }
                    return Ok(ok);
                }
            }
            Class::String(s) => {
                let s = *s;
                let len = self.str(s).len();
                if k == Key::Str(self.names.length) || matches!(k, Key::Index(i) if (i as usize) < len) {
                    let cur = self.ordinary_get_own(o, k);
                    return Ok(is_compatible(self, false, &d, cur.as_ref()));
                }
            }
            Class::Arguments(Some(m)) => {
                if let Key::Index(i) = k {
                    if let Some(&slot) = m.map.get(i as usize) {
                        if slot != u16::MAX {
                            return self.arguments_define(o, i, slot, d);
                        }
                    }
                }
            }
            _ => {}
        }
        self.ordinary_define(o, k, d)
    }

    fn arguments_define(&mut self, o: ObjId, i: u32, slot: u16, d: PropDesc) -> JsResult<bool> {
        let env = match &self.obj(o).class {
            Class::Arguments(Some(m)) => m.env,
            _ => unreachable!(),
        };
        let cur = self.envs.get(env.0).slots[slot as usize];
        // Keep the stored copy in sync before the generic validation.
        let k = Key::Index(i);
        let stored_attrs = match self.raw_get_own(o, k) {
            Some(Slot::Data(_, a)) => a,
            _ => attr::DEFAULT,
        };
        self.raw_put(o, k, Slot::Data(cur, stored_attrs));
        let mut d2 = d;
        if d.is_data() && d.value.is_none() && d.writable == Some(false) {
            d2.value = Some(cur);
        }
        if !self.ordinary_define(o, k, d2)? {
            return Ok(false);
        }
        let unmap = if d.is_accessor() {
            true
        } else {
            if let Some(v) = d.value {
                self.envs.get_mut(env.0).slots[slot as usize] = v;
            }
            d.writable == Some(false)
        };
        if unmap {
            if let Class::Arguments(Some(m)) = &mut self.obj_mut(o).class {
                m.map[i as usize] = u16::MAX;
            }
        }
        Ok(true)
    }

    /// OrdinaryDefineOwnProperty (ValidateAndApplyPropertyDescriptor).
    pub fn ordinary_define(&mut self, o: ObjId, k: Key, d: PropDesc) -> JsResult<bool> {
        let current = self.raw_get_own(o, k);
        let extensible = self.obj(o).extensible;
        let Some(cur) = current else {
            if !extensible {
                return Ok(false);
            }
            let a = d.attrs();
            if d.is_accessor() {
                self.raw_put(o, k, Slot::Accessor(d.get.unwrap_or(Value::UNDEFINED), d.set.unwrap_or(Value::UNDEFINED), a));
            } else {
                self.raw_put(o, k, Slot::Data(d.value.unwrap_or(Value::UNDEFINED), a & !attr::ACCESSOR));
            }
            return Ok(true);
        };
        let cur_desc = match cur {
            Slot::Data(v, a) => PropDesc::data(v, a),
            Slot::Accessor(g, s, a) => PropDesc::accessor(g, s, a),
        };
        if !is_compatible(self, extensible, &d, Some(&cur_desc)) {
            return Ok(false);
        }
        // Apply.
        let enumerable = d.enumerable.unwrap_or(cur_desc.enumerable == Some(true));
        let configurable = d.configurable.unwrap_or(cur_desc.configurable == Some(true));
        let mut a = 0;
        if enumerable {
            a |= attr::ENUMERABLE;
        }
        if configurable {
            a |= attr::CONFIGURABLE;
        }
        let new = if d.is_accessor() {
            let (g0, s0) = match cur {
                Slot::Accessor(g, s, _) => (g, s),
                _ => (Value::UNDEFINED, Value::UNDEFINED),
            };
            Slot::Accessor(d.get.unwrap_or(g0), d.set.unwrap_or(s0), a)
        } else if d.is_data() || matches!(cur, Slot::Data(..)) {
            let (v0, w0) = match cur {
                Slot::Data(v, ca) => (v, ca & attr::WRITABLE != 0),
                _ => (Value::UNDEFINED, false),
            };
            if d.writable.unwrap_or(w0) {
                a |= attr::WRITABLE;
            }
            Slot::Data(d.value.unwrap_or(v0), a)
        } else {
            match cur {
                Slot::Accessor(g, s, _) => Slot::Accessor(g, s, a),
                Slot::Data(v, _) => Slot::Data(v, a),
            }
        };
        // Frozen/sealed elements store attrs implicitly: an element whose
        // attrs differ must move out of the dense storage.
        self.raw_put(o, k, new);
        Ok(true)
    }

    /// ArraySetLength.
    fn array_set_length(&mut self, o: ObjId, d: PropDesc) -> JsResult<bool> {
        let Some(v) = d.value else {
            // Only attributes change.
            if d.configurable == Some(true) || d.enumerable == Some(true) || d.is_accessor() {
                return Ok(false);
            }
            if d.writable == Some(true) && self.obj(o).length_readonly {
                return Ok(false);
            }
            if d.writable == Some(false) {
                self.obj_mut(o).length_readonly = true;
            }
            return Ok(true);
        };
        let new_len = self.to_uint32(v)?;
        let num = self.to_number(v)?;
        if new_len as f64 != num {
            return Err(self.range_error("Invalid array length"));
        }
        if d.configurable == Some(true) || d.enumerable == Some(true) || d.is_accessor() {
            return Ok(false);
        }
        let old = self.obj(o).length;
        if self.obj(o).length_readonly {
            return Ok(new_len == old && d.writable != Some(true));
        }
        if new_len >= old {
            self.obj_mut(o).length = new_len;
            if d.writable == Some(false) {
                self.obj_mut(o).length_readonly = true;
            }
            return Ok(true);
        }
        // Shrink: delete elements from the end; stop at a non-configurable one.
        let ok = self.array_truncate(o, new_len);
        if d.writable == Some(false) {
            self.obj_mut(o).length_readonly = true;
        }
        Ok(ok)
    }

    /// Deletes indices >= `new_len` (from the top); returns false (and
    /// leaves `length` just above it) at a non-configurable element.
    pub fn array_truncate(&mut self, o: ObjId, new_len: u32) -> bool {
        let ob = self.obj(o);
        if ob.frozen_elements != 0 && ob.elements.iter().skip(new_len as usize).any(|v| !v.is_hole()) {
            // The highest present element blocks.
            let top = ob.elements.iter().rposition(|v| !v.is_hole()).unwrap() as u32;
            self.obj_mut(o).length = top + 1;
            return false;
        }
        if self.obj(o).sparse {
            let mut idx: Vec<u32> = self
                .raw_own_keys(o)
                .into_iter()
                .filter_map(|k| if let Key::Index(i) = k { Some(i) } else { None })
                .filter(|&i| i >= new_len)
                .collect();
            idx.sort_unstable_by(|a, b| b.cmp(a));
            for i in idx {
                if let Some(Slot::Data(_, a) | Slot::Accessor(_, _, a)) = self.raw_get_own(o, Key::Index(i)) {
                    if a & attr::CONFIGURABLE == 0 {
                        self.obj_mut(o).length = i + 1;
                        return false;
                    }
                }
                self.raw_delete(o, Key::Index(i));
            }
        }
        let ob = self.obj_mut(o);
        if ob.elements.len() > new_len as usize {
            ob.elements.truncate(new_len as usize);
        }
        ob.length = new_len;
        true
    }

    // ----- [[HasProperty]] / [[Get]] / [[Set]] / [[Delete]] -------------

    pub fn has_property(&mut self, o: ObjId, k: Key) -> JsResult<bool> {
        self.poll_limits()?;
        let mut cur = o;
        loop {
            match &self.obj(cur).class {
                Class::Proxy { .. } => return crate::builtins::proxy::has(self, cur, k),
                Class::TypedArray(_) => {
                    if let Some(r) = crate::builtins::buffer::ta_has(self, cur, k) {
                        return Ok(r);
                    }
                }
                Class::Namespace(_) => return crate::module::ns_has(self, cur, k),
                _ => {}
            }
            if self.ordinary_get_own(cur, k).is_some() {
                return Ok(true);
            }
            match self.obj(cur).proto {
                Some(p) => cur = p,
                None => return Ok(false),
            }
        }
    }

    pub fn has_own_property(&mut self, o: ObjId, k: Key) -> JsResult<bool> {
        Ok(self.get_own_property(o, k)?.is_some())
    }

    /// [[Get]] with a receiver.
    pub fn get_with(&mut self, o: ObjId, k: Key, receiver: Value) -> JsResult<Value> {
        self.poll_limits()?;
        let mut cur = o;
        loop {
            // Fast path: plain storage hit.
            let ob = self.obj(cur);
            match &ob.class {
                Class::Ordinary | Class::Function(_) | Class::Native(_) | Class::Error => {
                    if let Some(s) = self.raw_get_own(cur, k) {
                        return match s {
                            Slot::Data(v, _) => Ok(v),
                            Slot::Accessor(g, _, _) => {
                                if g.is_undefined() {
                                    Ok(Value::UNDEFINED)
                                } else {
                                    self.call(g, receiver, &[])
                                }
                            }
                        };
                    }
                }
                Class::Proxy { .. } => return crate::builtins::proxy::get(self, cur, k, receiver),
                Class::TypedArray(_) => {
                    if let Some(r) = crate::builtins::buffer::ta_get(self, cur, k) {
                        return Ok(r);
                    }
                    if let Some(s) = self.raw_get_own(cur, k) {
                        return self.slot_value(s, receiver);
                    }
                }
                Class::Namespace(_) => return crate::module::ns_get(self, cur, k),
                _ => {
                    if let Some(d) = self.ordinary_get_own(cur, k) {
                        return match d.get {
                            Some(g) if d.is_accessor() => {
                                if g.is_undefined() {
                                    Ok(Value::UNDEFINED)
                                } else {
                                    self.call(g, receiver, &[])
                                }
                            }
                            _ if d.is_accessor() => Ok(Value::UNDEFINED),
                            _ => Ok(d.value.unwrap_or(Value::UNDEFINED)),
                        };
                    }
                }
            }
            match self.obj(cur).proto {
                Some(p) => cur = p,
                None => return Ok(Value::UNDEFINED),
            }
        }
    }

    fn slot_value(&mut self, s: Slot, receiver: Value) -> JsResult<Value> {
        match s {
            Slot::Data(v, _) => Ok(v),
            Slot::Accessor(g, _, _) => {
                if g.is_undefined() {
                    Ok(Value::UNDEFINED)
                } else {
                    self.call(g, receiver, &[])
                }
            }
        }
    }

    pub fn get(&mut self, o: ObjId, k: Key) -> JsResult<Value> {
        self.get_with(o, k, Value::object(o))
    }

    /// GetV: property of any value (primitives use their prototype).
    pub fn get_v(&mut self, v: Value, k: Key) -> JsResult<Value> {
        if let Some(o) = v.as_object() {
            return self.get_with(o, k, v);
        }
        if let Some(s) = v.as_string() {
            if k == Key::Str(self.names.length) {
                // No flattening: a rope knows its length.
                return Ok(Value::from_usize(self.strings.get(s.0).len()));
            }
            if let Key::Index(i) = k {
                if (i as usize) < self.str(s).len() {
                    let c = self.str(s).at(i as usize);
                    return Ok(self.char_string(c));
                }
            }
        }
        let proto = self.primitive_proto(v)?;
        self.get_with(proto, k, v)
    }

    /// The prototype used for property lookups on a primitive.
    pub fn primitive_proto(&mut self, v: Value) -> JsResult<ObjId> {
        use crate::value::Kind;
        Ok(match v.kind() {
            Kind::Str(_) => self.intrinsics.string_prototype,
            Kind::Int(_) | Kind::Double(_) => self.intrinsics.number_prototype,
            Kind::Bool(_) => self.intrinsics.boolean_prototype,
            Kind::Symbol(_) => self.intrinsics.symbol_prototype,
            Kind::BigInt(_) => self.intrinsics.bigint_prototype,
            Kind::Object(o) => o,
            _ => {
                return Err(self.type_error("cannot read properties of null or undefined"));
            }
        })
    }

    /// [[Set]] (OrdinarySet with a receiver).
    pub fn set_with(&mut self, o: ObjId, k: Key, v: Value, receiver: Value) -> JsResult<bool> {
        self.poll_limits()?;
        // Fast path: own writable data property on an ordinary receiver.
        if receiver == Value::object(o) {
            if let Class::Ordinary | Class::Function(_) | Class::Error = self.obj(o).class {
                if let Key::Index(i) = k {
                    let ob = self.obj_mut(o);
                    if ob.frozen_elements == 0 {
                        if let Some(e) = ob.elements.get_mut(i as usize) {
                            if !e.is_hole() {
                                *e = v;
                                return Ok(true);
                            }
                        }
                    }
                } else if let Props::Shaped { shape, .. } = &self.obj(o).props {
                    if let Some((slot, a)) = self.shape_lookup(*shape, k) {
                        if a & (attr::WRITABLE | attr::ACCESSOR) == attr::WRITABLE {
                            if let Props::Shaped { slots, .. } = &mut self.obj_mut(o).props {
                                slots[slot as usize] = v;
                            }
                            return Ok(true);
                        }
                    }
                }
            }
            if let Class::Array = self.obj(o).class {
                if let Key::Index(i) = k {
                    let ob = self.obj_mut(o);
                    if ob.frozen_elements == 0 {
                        let i = i as usize;
                        if let Some(e) = ob.elements.get_mut(i) {
                            if !e.is_hole() {
                                *e = v;
                                return Ok(true);
                            }
                        }
                        // Appending to a dense array whose prototypes have
                        // no indexed properties.
                        if i == ob.elements.len() && i as u32 == ob.length && ob.extensible && !ob.length_readonly && !ob.sparse && self.protos_have_no_elements(o) {
                            let ob = self.obj_mut(o);
                            let cap = ob.elements.capacity();
                            ob.elements.push(v);
                            ob.length += 1;
                            let grown = ob.elements.capacity() - cap;
                            self.alloc_bytes += grown * 8;
                            return Ok(true);
                        }
                    }
                }
            }
        }
        // OrdinarySetWithOwnDescriptor, walking the prototype chain.
        let mut cur = o;
        let own_desc = loop {
            match &self.obj(cur).class {
                Class::Proxy { .. } => return crate::builtins::proxy::set(self, cur, k, v, receiver),
                Class::TypedArray(_) => {
                    if let Some(r) = crate::builtins::buffer::ta_set(self, cur, k, v, receiver)? {
                        return Ok(r);
                    }
                }
                Class::Namespace(_) => return Ok(false),
                _ => {}
            }
            if let Some(d) = self.ordinary_get_own(cur, k) {
                break Some(d);
            }
            match self.obj(cur).proto {
                Some(p) => cur = p,
                None => break None,
            }
        };
        let d = own_desc.unwrap_or(PropDesc::data(Value::UNDEFINED, attr::DEFAULT));
        if d.is_accessor() {
            let s = d.set.unwrap_or(Value::UNDEFINED);
            if s.is_undefined() {
                return Ok(false);
            }
            self.call(s, receiver, &[v])?;
            return Ok(true);
        }
        if d.writable != Some(true) {
            return Ok(false);
        }
        let Some(r) = receiver.as_object() else { return Ok(false) };
        match self.get_own_property(r, k)? {
            Some(ex) => {
                if ex.is_accessor() || ex.writable != Some(true) {
                    return Ok(false);
                }
                self.define_own_property(r, k, PropDesc { value: Some(v), ..Default::default() })
            }
            None => self.define_own_property(r, k, PropDesc::data(v, attr::DEFAULT)),
        }
    }

    fn protos_have_no_elements(&self, o: ObjId) -> bool {
        let mut cur = self.obj(o).proto;
        while let Some(p) = cur {
            let ob = self.obj(p);
            if !ob.elements.is_empty() || ob.sparse || !matches!(ob.class, Class::Ordinary | Class::Array) {
                return false;
            }
            cur = ob.proto;
        }
        true
    }

    /// Stops a single native call (filling a huge array, say) from going
    /// far past the heap limit between two safepoints: more than the whole
    /// limit allocated since the last collection is a RangeError. (No
    /// collection here: natives hold unrooted values.)
    #[inline]
    pub fn check_allocation(&mut self) -> JsResult<()> {
        if self.alloc_bytes > self.limits.max_heap_bytes {
            return Err(self.range_error("Out of memory: the heap limit was reached"));
        }
        Ok(())
    }

    pub fn set(&mut self, o: ObjId, k: Key, v: Value) -> JsResult<bool> {
        self.set_with(o, k, v, Value::object(o))
    }

    /// Set(O, P, V, true): TypeError on failure.
    pub fn set_or_throw(&mut self, o: ObjId, k: Key, v: Value) -> JsResult<()> {
        if !self.set(o, k, v)? {
            return Err(self.cannot_assign(k));
        }
        Ok(())
    }

    pub fn cannot_assign(&mut self, k: Key) -> super::Throw {
        let name = self.key_display(k);
        self.type_error(&alloc::format!("cannot assign to read only property '{name}'"))
    }

    pub fn key_display(&self, k: Key) -> alloc::string::String {
        match k {
            Key::Index(i) => alloc::format!("{i}"),
            Key::Str(s) => self.str(s).to_rust_lossy(),
            Key::Sym(s) => match self.symbols.get(s.0).description {
                Some(d) => alloc::format!("Symbol({})", self.str(d).to_rust_lossy()),
                None => alloc::string::String::from("Symbol()"),
            },
        }
    }

    pub fn delete(&mut self, o: ObjId, k: Key) -> JsResult<bool> {
        match &self.obj(o).class {
            Class::Proxy { .. } => return crate::builtins::proxy::delete(self, o, k),
            Class::TypedArray(_) => {
                if let Some(r) = crate::builtins::buffer::ta_delete(self, o, k) {
                    return Ok(r);
                }
            }
            Class::Namespace(_) => return crate::module::ns_delete(self, o, k),
            Class::Array if k == Key::Str(self.names.length) => return Ok(false),
            Class::String(s) => {
                let s = *s;
                if k == Key::Str(self.names.length) || matches!(k, Key::Index(i) if (i as usize) < self.str(s).len()) {
                    return Ok(false);
                }
            }
            Class::Arguments(Some(m)) => {
                if let Key::Index(i) = k {
                    if m.map.get(i as usize).is_some_and(|&s| s != u16::MAX) {
                        if let Some(Slot::Data(_, a)) = self.raw_get_own(o, k) {
                            if a & attr::CONFIGURABLE == 0 {
                                return Ok(false);
                            }
                        }
                        if let Class::Arguments(Some(m)) = &mut self.obj_mut(o).class {
                            m.map[i as usize] = u16::MAX;
                        }
                        self.raw_delete(o, k);
                        return Ok(true);
                    }
                }
            }
            _ => {}
        }
        match self.raw_get_own(o, k) {
            None => Ok(true),
            Some(Slot::Data(_, a) | Slot::Accessor(_, _, a)) => {
                if a & attr::CONFIGURABLE == 0 {
                    return Ok(false);
                }
                self.raw_delete(o, k);
                Ok(true)
            }
        }
    }

    pub fn own_keys(&mut self, o: ObjId) -> JsResult<Vec<Key>> {
        match &self.obj(o).class {
            Class::Proxy { .. } => return crate::builtins::proxy::own_keys(self, o),
            Class::TypedArray(_) => return Ok(crate::builtins::buffer::ta_own_keys(self, o)),
            Class::Namespace(_) => return crate::module::ns_own_keys(self, o),
            Class::String(s) => {
                let n = self.str(*s).len() as u32;
                let mut keys: Vec<Key> = (0..n).map(Key::Index).collect();
                let rest = self.raw_own_keys(o);
                let (idx, other): (Vec<Key>, Vec<Key>) = rest.into_iter().partition(|k| matches!(k, Key::Index(_)));
                keys.extend(idx.into_iter().filter(|k| matches!(k, Key::Index(i) if *i >= n)));
                let len = Key::Str(self.names.length);
                let mut strs: Vec<Key> = alloc::vec![len];
                strs.extend(other.iter().filter(|k| matches!(k, Key::Str(_))));
                keys.extend(strs);
                keys.extend(other.iter().filter(|k| matches!(k, Key::Sym(_))));
                return Ok(keys);
            }
            Class::Array => {
                let mut keys = self.raw_own_keys(o);
                let first_non_index = keys.iter().position(|k| !matches!(k, Key::Index(_))).unwrap_or(keys.len());
                keys.insert(first_non_index, Key::Str(self.names.length));
                return Ok(keys);
            }
            _ => {}
        }
        Ok(self.raw_own_keys(o))
    }

    pub fn get_prototype_of(&mut self, o: ObjId) -> JsResult<Option<ObjId>> {
        if let Class::Proxy { .. } = self.obj(o).class {
            return crate::builtins::proxy::get_prototype_of(self, o);
        }
        Ok(self.obj(o).proto)
    }

    pub fn set_prototype_of(&mut self, o: ObjId, p: Option<ObjId>) -> JsResult<bool> {
        match self.obj(o).class {
            Class::Proxy { .. } => return crate::builtins::proxy::set_prototype_of(self, o, p),
            Class::Namespace(_) => return Ok(p == self.obj(o).proto),
            _ => {}
        }
        if p == self.obj(o).proto {
            return Ok(true);
        }
        if !self.obj(o).extensible || self.is_immutable_proto(o) {
            return Ok(false);
        }
        // No cycles (stop at proxies).
        let mut cur = p;
        while let Some(c) = cur {
            if c == o {
                return Ok(false);
            }
            if let Class::Proxy { .. } = self.obj(c).class {
                break;
            }
            cur = self.obj(c).proto;
        }
        self.obj_mut(o).proto = p;
        Ok(true)
    }

    fn is_immutable_proto(&self, o: ObjId) -> bool {
        o == self.intrinsics.object_prototype
    }

    pub fn is_extensible(&mut self, o: ObjId) -> JsResult<bool> {
        if let Class::Proxy { .. } = self.obj(o).class {
            return crate::builtins::proxy::is_extensible(self, o);
        }
        Ok(self.obj(o).extensible)
    }

    pub fn prevent_extensions(&mut self, o: ObjId) -> JsResult<bool> {
        match self.obj(o).class {
            Class::Proxy { .. } => return crate::builtins::proxy::prevent_extensions(self, o),
            Class::TypedArray(_) => {
                if !crate::builtins::buffer::ta_can_prevent_extensions(self, o) {
                    return Ok(false);
                }
            }
            _ => {}
        }
        self.obj_mut(o).extensible = false;
        Ok(true)
    }

    /// SetIntegrityLevel: `frozen` (else sealed).
    pub fn set_integrity(&mut self, o: ObjId, frozen: bool) -> JsResult<bool> {
        if !self.prevent_extensions(o)? {
            return Ok(false);
        }
        let ordinary_storage = !matches!(self.obj(o).class, Class::Proxy { .. } | Class::TypedArray(_) | Class::Namespace(_) | Class::Arguments(Some(_)) | Class::String(_));
        if ordinary_storage {
            // Elements: flag; named properties: per key.
            let level = if frozen { 2 } else { 1 };
            let ob = self.obj_mut(o);
            ob.frozen_elements = ob.frozen_elements.max(level);
            if frozen && ob.is_array() {
                ob.length_readonly = true;
            }
            let keys = self.raw_own_keys(o);
            for k in keys {
                if let Key::Index(i) = k {
                    if self.obj(o).elements.get(i as usize).is_some_and(|v| !v.is_hole()) {
                        continue;
                    }
                }
                let Some(s) = self.raw_get_own(o, k) else { continue };
                let ns = match s {
                    Slot::Data(v, a) => Slot::Data(v, a & !attr::CONFIGURABLE & if frozen { !attr::WRITABLE } else { 0xFF }),
                    Slot::Accessor(g, st, a) => Slot::Accessor(g, st, a & !attr::CONFIGURABLE),
                };
                self.raw_put_named_attrs(o, k, ns);
            }
            return Ok(true);
        }
        let keys = self.own_keys(o)?;
        for k in keys {
            let d = if frozen {
                match self.get_own_property(o, k)? {
                    Some(cur) if cur.is_accessor() => PropDesc { configurable: Some(false), ..Default::default() },
                    Some(_) => PropDesc { configurable: Some(false), writable: Some(false), ..Default::default() },
                    None => continue,
                }
            } else {
                PropDesc { configurable: Some(false), ..Default::default() }
            };
            if !self.define_own_property(o, k, d)? {
                let name = self.key_display(k);
                return Err(self.type_error(&alloc::format!("cannot redefine property: {name}")));
            }
        }
        Ok(true)
    }

    fn raw_put_named_attrs(&mut self, o: ObjId, k: Key, s: Slot) {
        // Bypass the dense-element path (attrs of named/sparse props).
        let (v, a) = match s {
            Slot::Data(v, a) => (v, a),
            Slot::Accessor(g, st, a) => {
                let pair = self.alloc_object(None, Class::AccessorPair { get: g, set: st });
                (Value::object(pair), a | attr::ACCESSOR)
            }
        };
        self.raw_put_prop(o, k, v, a);
    }

    /// TestIntegrityLevel.
    pub fn test_integrity(&mut self, o: ObjId, frozen: bool) -> JsResult<bool> {
        if self.is_extensible(o)? {
            return Ok(false);
        }
        let keys = self.own_keys(o)?;
        for k in keys {
            if let Some(d) = self.get_own_property(o, k)? {
                if d.configurable == Some(true) {
                    return Ok(false);
                }
                if frozen && d.is_data() && d.writable == Some(true) {
                    return Ok(false);
                }
            }
        }
        Ok(true)
    }

    // ----- helpers ------------------------------------------------------

    pub fn create_data_property(&mut self, o: ObjId, k: Key, v: Value) -> JsResult<bool> {
        self.define_own_property(o, k, PropDesc::data(v, attr::DEFAULT))
    }

    pub fn create_data_property_or_throw(&mut self, o: ObjId, k: Key, v: Value) -> JsResult<()> {
        if !self.create_data_property(o, k, v)? {
            let name = self.key_display(k);
            return Err(self.type_error(&alloc::format!("cannot define property {name}")));
        }
        Ok(())
    }

    pub fn define_property_or_throw(&mut self, o: ObjId, k: Key, d: PropDesc) -> JsResult<()> {
        if !self.define_own_property(o, k, d)? {
            let name = self.key_display(k);
            return Err(self.type_error(&alloc::format!("cannot redefine property: {name}")));
        }
        Ok(())
    }

    pub fn delete_or_throw(&mut self, o: ObjId, k: Key) -> JsResult<()> {
        if !self.delete(o, k)? {
            let name = self.key_display(k);
            return Err(self.type_error(&alloc::format!("cannot delete property '{name}'")));
        }
        Ok(())
    }

    /// GetMethod: undefined/null -> None; non-callable -> TypeError.
    pub fn get_method(&mut self, v: Value, k: Key) -> JsResult<Option<Value>> {
        let f = self.get_v(v, k)?;
        if f.is_nullish() {
            return Ok(None);
        }
        if !self.is_callable(f) {
            let name = self.key_display(k);
            return Err(self.type_error(&alloc::format!("{name} is not a function")));
        }
        Ok(Some(f))
    }

    /// LengthOfArrayLike.
    pub fn length_of(&mut self, o: ObjId) -> JsResult<u64> {
        if let Class::Array = self.obj(o).class {
            return Ok(self.obj(o).length as u64);
        }
        let k = Key::Str(self.names.length);
        let v = self.get(o, k)?;
        self.to_length(v)
    }

    /// The key for an integer (index or string for large ones).
    pub fn index_key(&mut self, i: u64) -> Key {
        if i < u32::MAX as u64 {
            Key::Index(i as u32)
        } else {
            let s = alloc::format!("{i}");
            Key::Str(self.intern(&s))
        }
    }

    /// Empties a dictionary-mode object (used by realm setup helpers).
    pub fn clear_props(&mut self, o: ObjId) {
        self.obj_mut(o).props = Props::Dict(Box::new(Dict::default()));
    }
}

/// ValidateAndApplyPropertyDescriptor's checks (without applying).
pub fn is_compatible(vm: &Vm, extensible: bool, d: &PropDesc, cur: Option<&PropDesc>) -> bool {
    let Some(c) = cur else { return extensible };
    if d.is_generic() && d.enumerable.is_none() && d.configurable.is_none() {
        return true;
    }
    if c.configurable == Some(false) {
        if d.configurable == Some(true) {
            return false;
        }
        if d.enumerable.is_some() && d.enumerable != c.enumerable {
            return false;
        }
        if !d.is_generic() && d.is_accessor() != c.is_accessor() {
            return false;
        }
        if c.is_accessor() {
            if let Some(g) = d.get {
                if !vm.same_value(g, c.get.unwrap_or(Value::UNDEFINED)) {
                    return false;
                }
            }
            if let Some(s) = d.set {
                if !vm.same_value(s, c.set.unwrap_or(Value::UNDEFINED)) {
                    return false;
                }
            }
        } else if c.writable == Some(false) {
            if d.writable == Some(true) {
                return false;
            }
            if let Some(v) = d.value {
                if !vm.same_value(v, c.value.unwrap_or(Value::UNDEFINED)) {
                    return false;
                }
            }
        }
    }
    true
}
