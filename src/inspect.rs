//! The object inspector (rubrowser maps it onto CDP `Runtime`): stable
//! ids for values handed to the front end, released by group, and
//! property listings that never run JS (no getters, no Proxy traps).

use alloc::collections::BTreeMap;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use crate::object::{attr, Class, Key};
use crate::value::Value;
use crate::vm::Vm;

/// Values registered for the front end: id -> (value, group). They stay
/// alive until released.
#[derive(Default)]
pub struct Inspector {
    handles: BTreeMap<u32, (Value, u32)>,
    next: u32,
}

impl Inspector {
    pub fn values(&self) -> impl Iterator<Item = Value> + '_ {
        self.handles.values().map(|(v, _)| *v)
    }
}

/// An own property as stored: a data value or the accessor functions
/// (not called).
#[derive(Clone, Debug)]
pub struct PropertyInfo {
    /// A string or a symbol.
    pub name: Value,
    pub value: Option<Value>,
    pub get: Option<Value>,
    pub set: Option<Value>,
    pub writable: bool,
    pub enumerable: bool,
    pub configurable: bool,
    /// Found on the object itself (else on its prototype chain).
    pub own: bool,
}

impl Vm {
    /// Registers a value; the id stays valid until released.
    pub fn inspect_register(&mut self, v: Value, group: u32) -> u32 {
        let i = &mut self.inspector;
        i.next += 1;
        i.handles.insert(i.next, (v, group));
        i.next
    }

    pub fn inspect_get(&self, id: u32) -> Option<Value> {
        self.inspector.handles.get(&id).map(|(v, _)| *v)
    }

    pub fn inspect_release(&mut self, id: u32) {
        self.inspector.handles.remove(&id);
    }

    pub fn inspect_release_group(&mut self, group: u32) {
        self.inspector.handles.retain(|_, (_, g)| *g != group);
    }

    /// The properties of `v` without running JS. With `chain`, also those
    /// of its prototypes (not shadowed ones). A Proxy shows none (see
    /// `internal_properties`).
    pub fn inspect_properties(&mut self, v: Value, chain: bool) -> Vec<PropertyInfo> {
        let mut out: Vec<PropertyInfo> = Vec::new();
        let Some(mut o) = v.as_object() else { return out };
        let mut own = true;
        loop {
            if matches!(self.obj(o).class, Class::Proxy { .. }) {
                break;
            }
            // Own keys and descriptors of non-proxy objects never call JS.
            let keys = self.own_keys(o).unwrap_or_default();
            for k in keys {
                let name = self.key_to_value(k);
                if !own && out.iter().any(|p| p.name == name) {
                    continue;
                }
                let Ok(Some(d)) = self.get_own_property(o, k) else { continue };
                let a = d.attrs();
                out.push(PropertyInfo {
                    name,
                    value: d.value,
                    get: d.get,
                    set: d.set,
                    writable: a & attr::WRITABLE != 0,
                    enumerable: a & attr::ENUMERABLE != 0,
                    configurable: a & attr::CONFIGURABLE != 0,
                    own,
                });
            }
            if !chain {
                break;
            }
            match self.obj(o).proto {
                Some(p) => o = p,
                None => break,
            }
            own = false;
        }
        out
    }

    /// Engine-internal state, named like Chrome's `[[…]]` properties.
    pub fn internal_properties(&mut self, v: Value) -> Vec<(&'static str, Value)> {
        let mut out = Vec::new();
        let Some(o) = v.as_object() else { return out };
        if let Some(p) = self.obj(o).proto {
            out.push(("[[Prototype]]", Value::object(p)));
        }
        enum Extra {
            None,
            Entries(Vec<(Value, Value)>, bool),
            Location(u32, u32, u32),
        }
        let mut extra = Extra::None;
        match &self.obj(o).class {
            Class::Promise(p) => {
                use crate::builtins::promise::State;
                let s = match p.state {
                    State::Pending => "pending",
                    State::Fulfilled => "fulfilled",
                    State::Rejected => "rejected",
                };
                let r = p.result;
                let sv = self.new_string(s);
                out.push(("[[PromiseState]]", sv));
                out.push(("[[PromiseResult]]", r));
            }
            Class::Map(m) => extra = Extra::Entries(m.entries.iter().flatten().copied().collect(), true),
            Class::Set(m) => extra = Extra::Entries(m.entries.iter().flatten().copied().collect(), false),
            Class::Bound { target, this, args } => {
                let (t, th, a) = (*target, *this, args.to_vec());
                out.push(("[[TargetFunction]]", Value::object(t)));
                out.push(("[[BoundThis]]", th));
                let arr = self.new_array(a);
                out.push(("[[BoundArgs]]", Value::object(arr)));
            }
            Class::Proxy { target, handler, .. } => {
                out.push(("[[Target]]", target.map(Value::object).unwrap_or(Value::NULL)));
                out.push(("[[Handler]]", handler.map(Value::object).unwrap_or(Value::NULL)));
                out.push(("[[IsRevoked]]", Value::bool(target.is_none())));
            }
            Class::Function(c) => {
                let (l, col) = c.code.position(0);
                extra = Extra::Location(c.code.source.id, l, col);
            }
            Class::Generator(g) | Class::AsyncGenerator(g) => {
                use crate::vm::GenStatus;
                let s = match g.state {
                    GenStatus::SuspendedStart | GenStatus::SuspendedYield => "suspended",
                    GenStatus::Executing | GenStatus::AwaitingReturn => "running",
                    GenStatus::Completed => "closed",
                };
                let f = g.callee;
                let sv = self.new_string(s);
                out.push(("[[GeneratorState]]", sv));
                out.push(("[[GeneratorFunction]]", f));
            }
            Class::Boolean(b) => out.push(("[[PrimitiveValue]]", Value::bool(*b))),
            Class::Number(n) => out.push(("[[PrimitiveValue]]", Value::number(*n))),
            Class::String(s) => out.push(("[[PrimitiveValue]]", Value::str(*s))),
            Class::Symbol(s) => out.push(("[[PrimitiveValue]]", Value::symbol(*s))),
            Class::BigInt(b) => out.push(("[[PrimitiveValue]]", Value::bigint(*b))),
            Class::Date(t) => out.push(("[[DateValue]]", Value::number(*t))),
            Class::WeakRef(t) => out.push(("[[WeakRefTarget]]", *t)),
            Class::TypedArray(t) => out.push(("[[Buffer]]", Value::object(t.buffer))),
            Class::DataView(d) => out.push(("[[Buffer]]", Value::object(d.buffer))),
            Class::ArrayBuffer(b) => {
                let (n, det) = (b.data.len(), b.detached);
                out.push(("[[ArrayBufferByteLength]]", Value::from_usize(n)));
                out.push(("[[IsDetached]]", Value::bool(det)));
            }
            _ => {}
        }
        match extra {
            Extra::Entries(list, is_map) => {
                let items: Vec<Value> = list
                    .into_iter()
                    .map(|(k, v)| {
                        if is_map {
                            Value::object(self.new_array(alloc::vec![k, v]))
                        } else {
                            k
                        }
                    })
                    .collect();
                let arr = self.new_array(items);
                out.push(("[[Entries]]", Value::object(arr)));
            }
            Extra::Location(script, line, col) => {
                let loc = self.alloc_object(Some(self.intrinsics.object_prototype), Class::Ordinary);
                for (k, v) in [("scriptId", script), ("lineNumber", line), ("columnNumber", col)] {
                    let key = self.key(k);
                    self.define_data(loc, key, Value::from_u32(v), attr::DEFAULT);
                }
                out.push(("[[FunctionLocation]]", Value::object(loc)));
            }
            _ => {}
        }
        out
    }

    /// The class name Chrome shows for an object (`Array(3)`, `Map(2)`,
    /// `Foo` for instances of class Foo), without running JS.
    pub fn inspect_class_name(&mut self, v: Value) -> String {
        let Some(o) = v.as_object() else {
            return String::from(match v.kind() {
                crate::value::Kind::Str(_) => "string",
                crate::value::Kind::Int(_) | crate::value::Kind::Double(_) => "number",
                crate::value::Kind::Bool(_) => "boolean",
                crate::value::Kind::Symbol(_) => "symbol",
                crate::value::Kind::BigInt(_) => "bigint",
                crate::value::Kind::Null => "null",
                _ => "undefined",
            });
        };
        let base = match &self.obj(o).class {
            Class::Array => return alloc::format!("Array({})", self.obj(o).length),
            Class::Map(m) => return alloc::format!("Map({})", m.live),
            Class::Set(m) => return alloc::format!("Set({})", m.live),
            Class::Function(_) | Class::Native(_) | Class::Bound { .. } => "Function",
            Class::Proxy { .. } => "Proxy",
            Class::Promise(_) => "Promise",
            Class::Error => "Error",
            Class::RegExp(_) => "RegExp",
            Class::Date(_) => "Date",
            Class::WeakMap(_) => "WeakMap",
            Class::WeakSet(_) => "WeakSet",
            Class::ArrayBuffer(_) => "ArrayBuffer",
            Class::DataView(_) => "DataView",
            _ => "Object",
        };
        // `constructor.name` as plain data on the prototype, if present.
        if base == "Object" || base == "Error" {
            if let Some(p) = self.obj(o).proto {
                let ck = Key::Str(self.names.constructor);
                if let Some(crate::vm::props::Slot::Data(c, _)) = self.raw_get_own(p, ck) {
                    if let Some(co) = c.as_object() {
                        let nk = Key::Str(self.names.name);
                        if let Some(crate::vm::props::Slot::Data(n, _)) = self.raw_get_own(co, nk) {
                            if let Some(s) = n.as_string() {
                                let s = self.str(s).to_rust_lossy();
                                if !s.is_empty() {
                                    return s;
                                }
                            }
                        }
                    }
                }
            }
        }
        base.to_string()
    }
}
