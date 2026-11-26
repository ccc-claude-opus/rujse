//! Proxy exotic objects.

use alloc::vec::Vec;

use crate::object::Key;
use crate::value::{ObjId, Value};
use crate::vm::props::PropDesc;
use crate::vm::{JsResult, Vm};

pub fn init(_vm: &mut Vm) {}

fn revoked<T>(vm: &mut Vm) -> JsResult<T> {
    Err(vm.type_error("Proxy support is not implemented yet"))
}

pub fn get_own_property(vm: &mut Vm, _o: ObjId, _k: Key) -> JsResult<Option<PropDesc>> { revoked(vm) }
pub fn define_own_property(vm: &mut Vm, _o: ObjId, _k: Key, _d: PropDesc) -> JsResult<bool> { revoked(vm) }
pub fn has(vm: &mut Vm, _o: ObjId, _k: Key) -> JsResult<bool> { revoked(vm) }
pub fn get(vm: &mut Vm, _o: ObjId, _k: Key, _r: Value) -> JsResult<Value> { revoked(vm) }
pub fn set(vm: &mut Vm, _o: ObjId, _k: Key, _v: Value, _r: Value) -> JsResult<bool> { revoked(vm) }
pub fn delete(vm: &mut Vm, _o: ObjId, _k: Key) -> JsResult<bool> { revoked(vm) }
pub fn own_keys(vm: &mut Vm, _o: ObjId) -> JsResult<Vec<Key>> { revoked(vm) }
pub fn get_prototype_of(vm: &mut Vm, _o: ObjId) -> JsResult<Option<ObjId>> { revoked(vm) }
pub fn set_prototype_of(vm: &mut Vm, _o: ObjId, _p: Option<ObjId>) -> JsResult<bool> { revoked(vm) }
pub fn is_extensible(vm: &mut Vm, _o: ObjId) -> JsResult<bool> { revoked(vm) }
pub fn prevent_extensions(vm: &mut Vm, _o: ObjId) -> JsResult<bool> { revoked(vm) }
pub fn call(vm: &mut Vm, _o: ObjId, _this: Value, _a: &[Value]) -> JsResult<Value> { revoked(vm) }
pub fn construct(vm: &mut Vm, _o: ObjId, _a: &[Value], _nt: Value) -> JsResult<Value> { revoked(vm) }
