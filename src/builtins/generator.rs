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
    vm.method(agp, "next", 1, |vm, this, a, _| async_gen_enqueue(vm, this, 0, arg(a, 0)));
    vm.method(agp, "return", 1, |vm, this, a, _| async_gen_enqueue(vm, this, 2, arg(a, 0)));
    vm.method(agp, "throw", 1, |vm, this, a, _| async_gen_enqueue(vm, this, 1, arg(a, 0)));
    let p = vm.alloc_object(Some(aip), Class::Ordinary);
    vm.intrinsics.async_from_sync_iterator_prototype = p;
    vm.method(p, "next", 1, |vm, this, a, _| from_sync(vm, this, 0, a));
    vm.method(p, "return", 1, |vm, this, a, _| from_sync(vm, this, 2, a));
    vm.method(p, "throw", 1, |vm, this, a, _| from_sync(vm, this, 1, a));
}

fn resume(vm: &mut Vm, this: Value, kind: u8, v: Value) -> JsResult<Value> {
    match this.as_object() {
        Some(g) if matches!(vm.obj(g).class, Class::Generator(_)) => vm.generator_resume(g, kind, v),
        _ => Err(vm.type_error("generator method called on incompatible receiver")),
    }
}

/// An await inside an async generator settled.
pub fn async_gen_await_resume(vm: &mut Vm, g: ObjId, kind: u8, v: Value) -> JsResult<Value> {
    step(vm, g, kind, v)?;
    drain(vm, g)?;
    Ok(Value::UNDEFINED)
}

fn new_promise(vm: &mut Vm) -> Value {
    let pp = vm.intrinsics.promise_prototype;
    Value::object(super::promise::new_promise(vm, pp))
}

/// AsyncGenerator.prototype.next/throw/return.
fn async_gen_enqueue(vm: &mut Vm, this: Value, kind: u8, v: Value) -> JsResult<Value> {
    let p = new_promise(vm);
    vm.root(p);
    let g = match this.as_object() {
        Some(g) if matches!(vm.obj(g).class, Class::AsyncGenerator(_)) => g,
        _ => {
            let e = vm.type_error("AsyncGenerator method called on incompatible receiver");
            if let crate::vm::Throw::Value(e) = e {
                super::promise::reject_promise(vm, p, e);
            }
            return Ok(p);
        }
    };
    vm.gen_state_mut(g).queue.push((kind, v, p));
    drain(vm, g)?;
    Ok(p)
}

fn settle_head(vm: &mut Vm, g: ObjId, r: Result<(Value, bool), Value>) -> JsResult<()> {
    let st = vm.gen_state_mut(g);
    if st.queue.is_empty() {
        return Ok(());
    }
    let (_, _, p) = st.queue.remove(0);
    match r {
        Ok((v, done)) => {
            let res = vm.iter_result(v, done);
            super::promise::resolve_promise(vm, p, res)?;
        }
        Err(e) => super::promise::reject_promise(vm, p, e),
    }
    Ok(())
}

/// Runs the body one step with a completion and settles the head request
/// according to how the body stopped.
fn step(vm: &mut Vm, g: ObjId, kind: u8, v: Value) -> JsResult<()> {
    use crate::vm::interp::GenSignal;
    match vm.resume(g, kind, v) {
        Ok((v, GenSignal::Yield | GenSignal::YieldRaw)) => settle_head(vm, g, Ok((v, false))),
        Ok((_, GenSignal::Await)) => Ok(()),
        Ok((v, _)) => {
            vm.gen_state_mut(g).state = crate::vm::GenStatus::Completed;
            settle_head(vm, g, Ok((v, true)))
        }
        Err(crate::vm::Throw::Value(e)) => {
            vm.gen_state_mut(g).state = crate::vm::GenStatus::Completed;
            settle_head(vm, g, Err(e))
        }
        Err(t) => Err(t),
    }
}

/// AsyncGeneratorDrainQueue / resume-next: serves queued requests while
/// the generator is not running.
fn drain(vm: &mut Vm, g: ObjId) -> JsResult<()> {
    use crate::vm::GenStatus::*;
    loop {
        let st = vm.gen_state(g);
        let state = st.state;
        if matches!(state, Executing | AwaitingReturn) {
            return Ok(());
        }
        // A suspended-at-await body is Executing from our point of view.
        if st.awaiting_return {
            return Ok(());
        }
        let Some(&(kind, v, _)) = st.queue.first() else { return Ok(()) };
        if state == SuspendedYield && vm.async_gen_in_await(g) {
            return Ok(());
        }
        let completed = state == Completed || (state == SuspendedStart && kind != 0);
        if completed {
            vm.gen_state_mut(g).state = Completed;
            match kind {
                2 => {
                    // Await the value, then resolve {value, done: true}.
                    vm.gen_state_mut(g).state = AwaitingReturn;
                    let pc = Value::object(vm.intrinsics.promise);
                    let pr = match super::promise::promise_resolve(vm, pc, v) {
                        Ok(p) => p,
                        Err(crate::vm::Throw::Value(e)) => {
                            vm.gen_state_mut(g).state = Completed;
                            settle_head(vm, g, Err(e))?;
                            continue;
                        }
                        Err(t) => return Err(t),
                    };
                    let on_ful = vm.native_fn_data("", 1, await_return_settled, false, alloc::boxed::Box::new([Value::object(g), Value::FALSE]));
                    let on_rej = vm.native_fn_data("", 1, await_return_settled, false, alloc::boxed::Box::new([Value::object(g), Value::TRUE]));
                    super::promise::perform_then(vm, pr.as_object().unwrap(), Value::object(on_ful), Value::object(on_rej), [Value::UNDEFINED; 3]);
                    return Ok(());
                }
                1 => settle_head(vm, g, Err(v))?,
                _ => settle_head(vm, g, Ok((Value::UNDEFINED, true)))?,
            }
            continue;
        }
        step(vm, g, kind, v)?;
    }
}

fn await_return_settled(vm: &mut Vm, _: Value, a: &[Value], _: Value) -> JsResult<Value> {
    let g = vm.native_data(0).as_object().unwrap();
    let rejected = vm.native_data(1) == Value::TRUE;
    vm.gen_state_mut(g).state = crate::vm::GenStatus::Completed;
    let v = arg(a, 0);
    settle_head(vm, g, if rejected { Err(v) } else { Ok((v, true)) })?;
    drain(vm, g)?;
    Ok(Value::UNDEFINED)
}

/// %AsyncFromSyncIteratorPrototype%.next/return/throw.
fn from_sync(vm: &mut Vm, this: Value, kind: u8, a: &[Value]) -> JsResult<Value> {
    let p = new_promise(vm);
    vm.root(p);
    let (it, next) = match this.as_object().map(|o| &vm.obj(o).class) {
        Some(Class::AsyncFromSyncIterator(it, next)) => (*it, *next),
        _ => return Err(vm.type_error("not an async-from-sync iterator")),
    };
    let reject = |vm: &mut Vm, e: crate::vm::Throw| -> JsResult<Value> {
        match e {
            crate::vm::Throw::Value(e) => {
                super::promise::reject_promise(vm, p, e);
                Ok(p)
            }
            t => Err(t),
        }
    };
    let result = match kind {
        0 => {
            let r = if a.is_empty() { vm.call(next, it, &[]) } else { vm.call(next, it, &[a[0]]) };
            match r {
                Ok(r) => r,
                Err(e) => return reject(vm, e),
            }
        }
        _ => {
            let name = if kind == 2 { vm.names.return_ } else { vm.names.throw_ };
            let m = match vm.get_method(it, Key::Str(name)) {
                Ok(m) => m,
                Err(e) => return reject(vm, e),
            };
            let Some(m) = m else {
                if kind == 2 {
                    let res = vm.iter_result(arg(a, 0), true);
                    super::promise::resolve_promise(vm, p, res)?;
                    return Ok(p);
                }
                // No throw method: close the iterator, reject with TypeError.
                if let Err(e) = vm.iterator_close(it) {
                    return reject(vm, e);
                }
                let e = vm.type_error("The iterator does not provide a 'throw' method");
                return reject(vm, e);
            };
            let r = if a.is_empty() { vm.call(m, it, &[]) } else { vm.call(m, it, &[a[0]]) };
            match r {
                Ok(r) => r,
                Err(e) => return reject(vm, e),
            }
        }
    };
    let Some(ro) = result.as_object() else {
        let e = vm.type_error("iterator result is not an object");
        return reject(vm, e);
    };
    vm.root(result);
    // AsyncFromSyncIteratorContinuation.
    let kd = Key::Str(vm.names.done);
    let done = match vm.get(ro, kd) {
        Ok(d) => vm.to_boolean(d),
        Err(e) => return reject(vm, e),
    };
    let kv = Key::Str(vm.names.value);
    let value = match vm.get(ro, kv) {
        Ok(v) => v,
        Err(e) => return reject(vm, e),
    };
    let pc = Value::object(vm.intrinsics.promise);
    let wrapper = match super::promise::promise_resolve(vm, pc, value) {
        Ok(w) => w,
        Err(e) => {
            if !done && kind != 2 {
                let _ = vm.iterator_close(it);
            }
            return reject(vm, e);
        }
    };
    let unwrap = vm.native_fn_data("", 1, |vm, _, a, _| {
        let done = vm.native_data(0) == Value::TRUE;
        Ok(vm.iter_result(arg(a, 0), done))
    }, false, alloc::boxed::Box::new([Value::bool(done)]));
    // On rejection of a not-done value, close the sync iterator.
    let on_rej = if done || kind == 2 {
        Value::UNDEFINED
    } else {
        let f = vm.native_fn_data("", 1, |vm, _, a, _| {
            let it = vm.native_data(0);
            vm.iterator_close(it)?;
            Err(crate::vm::Throw::Value(arg(a, 0)))
        }, false, alloc::boxed::Box::new([it]));
        Value::object(f)
    };
    let (res, rej) = super::promise::resolving_functions(vm, p.as_object().unwrap());
    super::promise::perform_then(vm, wrapper.as_object().unwrap(), Value::object(unwrap), on_rej, [p, res, rej]);
    Ok(p)
}

