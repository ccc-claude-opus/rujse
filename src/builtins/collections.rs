//! Map, Set, WeakMap, WeakSet, WeakRef, FinalizationRegistry storage.

use alloc::vec::Vec;

use crate::value::Value;
use crate::vm::gc::Trace;

/// Insertion-ordered map with tombstones (iterators stay valid while
/// entries are added or removed).
#[derive(Default)]
pub struct OrderedMap {
    pub entries: Vec<Option<(Value, Value)>>,
    pub live: usize,
}

impl Trace for OrderedMap {
    fn trace(&self, out: &mut Vec<Value>) {
        for (k, v) in self.entries.iter().flatten() {
            out.push(*k);
            out.push(*v);
        }
    }
}

pub struct RegistryCell {
    pub target: Value,
    pub held: Value,
    pub token: Value,
}

pub struct Registry {
    pub cleanup: Value,
    pub cells: Vec<RegistryCell>,
}

impl Trace for Registry {
    fn trace(&self, out: &mut Vec<Value>) {
        out.push(self.cleanup);
        for c in &self.cells {
            out.push(c.held);
        }
    }
}

pub fn init(vm: &mut crate::vm::Vm) {
    let op = vm.intrinsics.object_prototype;
    let mp = vm.alloc_object(Some(op), crate::object::Class::Ordinary);
    vm.intrinsics.map_prototype = mp;
}

/// Map.prototype.set semantics on a Map object.
pub fn map_set(vm: &mut crate::vm::Vm, m: crate::value::ObjId, k: Value, v: Value) {
    let k = if k.as_number() == Some(0.0) { Value::int(0) } else { k };
    let pos = match &vm.obj(m).class {
        crate::object::Class::Map(t) => t.entries.iter().position(|e| e.is_some_and(|(x, _)| vm.same_value_zero(x, k))),
        _ => return,
    };
    if let crate::object::Class::Map(t) = &mut vm.obj_mut(m).class {
        match pos {
            Some(i) => t.entries[i] = Some((k, v)),
            None => {
                t.entries.push(Some((k, v)));
                t.live += 1;
            }
        }
    }
}
