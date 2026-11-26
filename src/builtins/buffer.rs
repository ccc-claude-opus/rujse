//! ArrayBuffer, SharedArrayBuffer, typed arrays and DataView.

use alloc::vec::Vec;

use crate::object::Key;
use crate::value::{ObjId, Value};
use crate::vm::gc::Trace;
use crate::vm::props::PropDesc;
use crate::vm::{JsResult, Vm};

pub struct BufferData {
    pub data: Vec<u8>,
    pub detached: bool,
    pub max_len: Option<usize>,
    pub shared: bool,
}

pub struct TypedArrayData {
    pub buffer: ObjId,
    pub kind: u8,
    pub offset: usize,
    /// None: length tracks a resizable buffer.
    pub length: Option<usize>,
}

impl Trace for TypedArrayData {
    fn trace(&self, out: &mut Vec<Value>) {
        out.push(Value::object(self.buffer));
    }
}

pub struct ViewData {
    pub buffer: ObjId,
    pub offset: usize,
    pub length: Option<usize>,
}

impl Trace for ViewData {
    fn trace(&self, out: &mut Vec<Value>) {
        out.push(Value::object(self.buffer));
    }
}

pub fn init(_vm: &mut Vm) {}

pub fn ta_get_own(_vm: &mut Vm, _o: ObjId, _k: Key) -> Option<Option<PropDesc>> { None }
pub fn ta_define_own(_vm: &mut Vm, _o: ObjId, _k: Key, _d: PropDesc) -> JsResult<Option<bool>> { Ok(None) }
pub fn ta_has(_vm: &mut Vm, _o: ObjId, _k: Key) -> Option<bool> { None }
pub fn ta_get(_vm: &mut Vm, _o: ObjId, _k: Key) -> Option<Value> { None }
pub fn ta_set(_vm: &mut Vm, _o: ObjId, _k: Key, _v: Value, _r: Value) -> JsResult<Option<bool>> { Ok(None) }
pub fn ta_delete(_vm: &mut Vm, _o: ObjId, _k: Key) -> Option<bool> { None }
pub fn ta_own_keys(vm: &mut Vm, o: ObjId) -> Vec<Key> { vm.raw_own_keys(o) }
pub fn ta_can_prevent_extensions(_vm: &mut Vm, _o: ObjId) -> bool { true }
pub fn ta_length_checked(_vm: &mut Vm, _o: ObjId) -> JsResult<usize> { Ok(0) }
