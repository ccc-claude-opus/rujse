//! Generator and async generator objects and their prototypes.

use crate::object::{attr, Class, Key};
use crate::value::{ObjId, Value};
use crate::vm::{JsResult, Vm};

use super::object::arg;

pub fn init(vm: &mut Vm) {
    let fp = vm.intrinsics.function_prototype;
    let ip = vm.intrinsics.iterator_prototype;
    // %GeneratorFunction.prototype% and %GeneratorPrototype%.
    let gfp = vm.alloc_object(Some(fp), Class::Ordinary);
    vm.intrinsics.generator_function_prototype = gfp;
    let gp = vm.alloc_object(Some(ip), Class::Ordinary);
    vm.intrinsics.generator_prototype = gp;
    vm.prop(gfp, "prototype", Value::object(gp), attr::CONFIGURABLE);
    vm.prop(gp, "constructor", Value::object(gfp), attr::CONFIGURABLE);
    let tag = vm.new_string("Generator");
    let kt = Key::Sym(vm.intrinsics.sym_to_string_tag);
    vm.define_data(gp, kt, tag, attr::CONFIGURABLE);
    let tag = vm.new_string("GeneratorFunction");
    vm.define_data(gfp, kt, tag, attr::CONFIGURABLE);
    vm.method(gp, "next", 1, |vm, this, a, _| resume(vm, this, 0, arg(a, 0)));
    vm.method(gp, "return", 1, |vm, this, a, _| resume(vm, this, 2, arg(a, 0)));
    vm.method(gp, "throw", 1, |vm, this, a, _| resume(vm, this, 1, arg(a, 0)));
    let gf = vm.constructor("GeneratorFunction", 1, |vm, _, a, nt| super::function::dynamic_function(vm, a, nt, false, true), gfp);
    vm.obj_mut(gf).proto = Some(vm.intrinsics.function);
    vm.intrinsics.generator_function = gf;
    // Async functions.
    let afp = vm.alloc_object(Some(fp), Class::Ordinary);
    vm.intrinsics.async_function_prototype = afp;
    let tag = vm.new_string("AsyncFunction");
    vm.define_data(afp, kt, tag, attr::CONFIGURABLE);
    let af = vm.constructor("AsyncFunction", 1, |vm, _, a, nt| super::function::dynamic_function(vm, a, nt, true, false), afp);
    vm.obj_mut(af).proto = Some(vm.intrinsics.function);
    vm.intrinsics.async_function = af;
    // Async generators.
    let aip = vm.intrinsics.async_iterator_prototype;
    let agfp = vm.alloc_object(Some(fp), Class::Ordinary);
    vm.intrinsics.async_generator_function_prototype = agfp;
    let agp = vm.alloc_object(Some(aip), Class::Ordinary);
    vm.intrinsics.async_generator_prototype = agp;
    vm.prop(agfp, "prototype", Value::object(agp), attr::CONFIGURABLE);
    vm.prop(agp, "constructor", Value::object(agfp), attr::CONFIGURABLE);
    let tag = vm.new_string("AsyncGenerator");
    vm.define_data(agp, kt, tag, attr::CONFIGURABLE);
    let tag = vm.new_string("AsyncGeneratorFunction");
    vm.define_data(agfp, kt, tag, attr::CONFIGURABLE);
    let agf = vm.constructor("AsyncGeneratorFunction", 1, |vm, _, a, nt| super::function::dynamic_function(vm, a, nt, true, true), agfp);
    vm.obj_mut(agf).proto = Some(vm.intrinsics.function);
    vm.intrinsics.async_generator_function = agf;
    let p = vm.alloc_object(Some(aip), Class::Ordinary);
    vm.intrinsics.async_from_sync_iterator_prototype = p;
}

fn resume(vm: &mut Vm, this: Value, kind: u8, v: Value) -> JsResult<Value> {
    match this.as_object() {
        Some(g) if matches!(vm.obj(g).class, Class::Generator(_)) => vm.generator_resume(g, kind, v),
        _ => Err(vm.type_error("generator method called on incompatible receiver")),
    }
}

/// An await inside an async generator settled.
pub fn async_gen_await_resume(vm: &mut Vm, g: ObjId, kind: u8, v: Value) -> JsResult<Value> {
    vm.resume(g, kind, v)?;
    Ok(Value::UNDEFINED)
}
