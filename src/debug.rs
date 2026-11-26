//! Debugger hooks (breakpoints, stepping) and the object inspector.

use alloc::vec::Vec;

use crate::value::Value;
use crate::vm::{JsResult, Vm};

/// A `debugger;` statement.
pub fn on_debugger_statement(_vm: &mut Vm) -> JsResult<()> {
    Ok(())
}

pub fn trace_roots(_vm: &Vm, _out: &mut Vec<Value>) {}
