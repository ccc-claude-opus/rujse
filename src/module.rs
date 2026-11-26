//! ES modules: records, linking, evaluation, namespaces, import().

use alloc::vec::Vec;

use crate::object::Key;
use crate::value::{ObjId, Value};
use crate::vm::props::PropDesc;
use crate::vm::{JsResult, Vm};

#[derive(Default)]
pub struct Modules {}

impl Modules {
    pub fn trace(&self, _out: &mut Vec<Value>) {}
}

fn ns<T>(vm: &mut Vm) -> JsResult<T> {
    Err(vm.type_error("modules are not implemented yet"))
}

pub fn ns_get_own(vm: &mut Vm, _o: ObjId, _k: Key) -> JsResult<Option<PropDesc>> { ns(vm) }
pub fn ns_define_own(vm: &mut Vm, _o: ObjId, _k: Key, _d: PropDesc) -> JsResult<bool> { ns(vm) }
pub fn ns_has(vm: &mut Vm, _o: ObjId, _k: Key) -> JsResult<bool> { ns(vm) }
pub fn ns_get(vm: &mut Vm, _o: ObjId, _k: Key) -> JsResult<Value> { ns(vm) }
pub fn ns_delete(vm: &mut Vm, _o: ObjId, _k: Key) -> JsResult<bool> { ns(vm) }
pub fn ns_own_keys(vm: &mut Vm, _o: ObjId) -> JsResult<Vec<Key>> { ns(vm) }

/// import(specifier): a promise.
pub fn import_call(vm: &mut Vm, _fi: usize, _spec: Value, _opts: Value) -> Value {
    let pp = vm.intrinsics.promise_prototype;
    let p = crate::builtins::promise::new_promise(vm, pp);
    let e = vm.type_error("modules are not implemented yet");
    if let crate::vm::Throw::Value(e) = e {
        crate::builtins::promise::reject_promise(vm, Value::object(p), e);
    }
    Value::object(p)
}

pub fn import_meta(vm: &mut Vm, _fi: usize) -> JsResult<Value> { ns(vm) }
