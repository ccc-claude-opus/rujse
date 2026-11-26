//! RegExp objects (the matcher is in `regex/`).

use alloc::vec::Vec;

use crate::value::Value;
use crate::vm::gc::Trace;
use crate::vm::{JsResult, Vm};

pub struct RegExpData {
    pub source: Value,
    pub flags: Value,
}

impl Trace for RegExpData {
    fn trace(&self, out: &mut Vec<Value>) {
        out.push(self.source);
        out.push(self.flags);
    }
}

/// A regular expression literal.
pub fn create(vm: &mut Vm, pattern: Value, flags: Value) -> JsResult<Value> {
    let p = vm.intrinsics.regexp_prototype;
    let o = vm.alloc_object(Some(p), crate::object::Class::RegExp(alloc::boxed::Box::new(RegExpData { source: pattern, flags })));
    let k = crate::object::Key::Str(vm.names.last_index);
    vm.define_data(o, k, Value::int(0), crate::object::attr::WRITABLE);
    Ok(Value::object(o))
}

pub fn init(vm: &mut Vm) {
    let op = vm.intrinsics.object_prototype;
    let rp = vm.alloc_object(Some(op), crate::object::Class::Ordinary);
    vm.intrinsics.regexp_prototype = rp;
}
