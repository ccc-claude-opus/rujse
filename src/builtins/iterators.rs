//! Built-in iterators (array, string, map, set, regexp) and async iteration.

use alloc::vec::Vec;

use crate::value::Value;
use crate::vm::gc::Trace;
use crate::vm::{JsResult, Vm};

pub struct IterState {
    /// 0 array keys, 1 array values, 2 array entries, 3 string, 4 map, 5 set, 6 regexp string, 7 helper.
    pub kind: u8,
    pub target: Value,
    pub pos: usize,
    pub extra: Vec<Value>,
    pub done: bool,
}

impl Trace for IterState {
    fn trace(&self, out: &mut Vec<Value>) {
        out.push(self.target);
        out.extend_from_slice(&self.extra);
    }
}

/// GetIterator(obj, async): (iterator, next).
pub fn get_async_iterator(vm: &mut Vm, v: Value) -> JsResult<(Value, Value)> {
    let k = crate::object::Key::Sym(vm.intrinsics.sym_async_iterator);
    match vm.get_method(v, k)? {
        Some(m) => {
            let it = vm.call(m, v, &[])?;
            if !it.is_object() {
                return Err(vm.type_error("Result of the Symbol.asyncIterator method is not an object"));
            }
            let kn = crate::object::Key::Str(vm.names.next);
            let next = vm.get_v(it, kn)?;
            Ok((it, next))
        }
        None => {
            let (it, next) = vm.get_iterator(v)?;
            crate::builtins::promise::async_from_sync(vm, it, next)
        }
    }
}

pub fn init(vm: &mut Vm) {
    let op = vm.intrinsics.object_prototype;
    let ip = vm.alloc_object(Some(op), crate::object::Class::Ordinary);
    vm.intrinsics.iterator_prototype = ip;
    let si = vm.intrinsics.sym_iterator;
    vm.method_sym(ip, si, "[Symbol.iterator]", 0, |_, this, _, _| Ok(this));
    let aip = vm.alloc_object(Some(op), crate::object::Class::Ordinary);
    vm.intrinsics.async_iterator_prototype = aip;
    let sai = vm.intrinsics.sym_async_iterator;
    vm.method_sym(aip, sai, "[Symbol.asyncIterator]", 0, |_, this, _, _| Ok(this));
}
