//! Promise (ECMA-262 §27.2), the job queue hooks and await.

use alloc::boxed::Box;
use alloc::vec;
use alloc::vec::Vec;

use crate::object::{attr, Class, Key};
use crate::value::{ObjId, Value};
use crate::vm::gc::Trace;
use crate::vm::{GenState, GenStatus, Job, JsResult, Throw, Vm};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum State {
    Pending,
    Fulfilled,
    Rejected,
}

/// A reaction: what to do when a promise settles.
#[derive(Clone, Copy)]
pub struct Reaction {
    /// The derived promise capability (promise, resolve, reject), or
    /// undefined for internal reactions.
    pub cap: [Value; 3],
    /// The handler (or undefined: pass through).
    pub handler: Value,
}

pub struct PromiseData {
    pub state: State,
    pub result: Value,
    pub fulfill: Vec<Reaction>,
    pub reject: Vec<Reaction>,
    pub handled: bool,
}

impl Trace for PromiseData {
    fn trace(&self, out: &mut Vec<Value>) {
        out.push(self.result);
        for r in self.fulfill.iter().chain(self.reject.iter()) {
            out.extend_from_slice(&r.cap);
            out.push(r.handler);
        }
    }
}

fn data(vm: &Vm, p: ObjId) -> Option<&PromiseData> {
    match &vm.obj(p).class {
        Class::Promise(d) => Some(d),
        _ => None,
    }
}

fn data_mut(vm: &mut Vm, p: ObjId) -> &mut PromiseData {
    match &mut vm.obj_mut(p).class {
        Class::Promise(d) => d,
        _ => panic!("not a promise"),
    }
}

pub fn is_promise(vm: &Vm, v: Value) -> bool {
    v.as_object().is_some_and(|o| data(vm, o).is_some())
}

/// A new pending promise with the given prototype.
pub fn new_promise(vm: &mut Vm, proto: ObjId) -> ObjId {
    vm.alloc_object(
        Some(proto),
        Class::Promise(Box::new(PromiseData { state: State::Pending, result: Value::UNDEFINED, fulfill: Vec::new(), reject: Vec::new(), handled: false })),
    )
}

/// CreateResolvingFunctions: (resolve, reject) sharing an "already
/// resolved" flag (data slot 1 holds a shared Internal cell).
pub fn resolving_functions(vm: &mut Vm, p: ObjId) -> (Value, Value) {
    let flag = vm.alloc_object(None, Class::Internal(Box::new([Value::FALSE])));
    let resolve = vm.native_fn_data("", 1, resolve_fn, false, Box::new([Value::object(p), Value::object(flag)]));
    let reject = vm.native_fn_data("", 1, reject_fn, false, Box::new([Value::object(p), Value::object(flag)]));
    (Value::object(resolve), Value::object(reject))
}

fn take_flag(vm: &mut Vm) -> bool {
    let flag = vm.native_data(1).as_object().unwrap();
    if let Class::Internal(v) = &mut vm.obj_mut(flag).class {
        if v[0] == Value::TRUE {
            return false;
        }
        v[0] = Value::TRUE;
    }
    true
}

fn resolve_fn(vm: &mut Vm, _this: Value, args: &[Value], _nt: Value) -> JsResult<Value> {
    if !take_flag(vm) {
        return Ok(Value::UNDEFINED);
    }
    let p = vm.native_data(0);
    resolve_promise(vm, p, args.first().copied().unwrap_or(Value::UNDEFINED))?;
    Ok(Value::UNDEFINED)
}

fn reject_fn(vm: &mut Vm, _this: Value, args: &[Value], _nt: Value) -> JsResult<Value> {
    if !take_flag(vm) {
        return Ok(Value::UNDEFINED);
    }
    let p = vm.native_data(0);
    reject_promise(vm, p, args.first().copied().unwrap_or(Value::UNDEFINED));
    Ok(Value::UNDEFINED)
}

/// The resolve function's behavior (thenables are followed via a job).
pub fn resolve_promise(vm: &mut Vm, p: Value, resolution: Value) -> JsResult<()> {
    let po = p.as_object().unwrap();
    if data(vm, po).map(|d| d.state) != Some(State::Pending) {
        return Ok(());
    }
    if resolution == p {
        let e = vm.type_error("Chaining cycle detected for promise");
        let Throw::Value(e) = e else { unreachable!() };
        reject_promise(vm, p, e);
        return Ok(());
    }
    let Some(ro) = resolution.as_object() else {
        fulfill(vm, po, resolution);
        return Ok(());
    };
    let kt = Key::Str(vm.names.then);
    let then = match vm.get(ro, kt) {
        Ok(t) => t,
        Err(Throw::Value(e)) => {
            reject_promise(vm, p, e);
            return Ok(());
        }
        Err(t) => return Err(t),
    };
    if !vm.is_callable(then) {
        fulfill(vm, po, resolution);
        return Ok(());
    }
    // NewPromiseResolveThenableJob.
    let job = vm.native_fn_data("", 0, resolve_thenable_job, false, Box::new([p, resolution, then]));
    vm.jobs.push_back(Job { func: Value::object(job), this: Value::UNDEFINED, args: Vec::new() });
    Ok(())
}

fn resolve_thenable_job(vm: &mut Vm, _this: Value, _args: &[Value], _nt: Value) -> JsResult<Value> {
    let p = vm.native_data(0).as_object().unwrap();
    let thenable = vm.native_data(1);
    let then = vm.native_data(2);
    let (res, rej) = resolving_functions(vm, p);
    vm.root(res);
    vm.root(rej);
    if let Err(Throw::Value(e)) = vm.call(then, thenable, &[res, rej]) {
        vm.call(rej, Value::UNDEFINED, &[e])?;
    }
    Ok(Value::UNDEFINED)
}

fn fulfill(vm: &mut Vm, p: ObjId, v: Value) {
    let d = data_mut(vm, p);
    let reactions = core::mem::take(&mut d.fulfill);
    d.reject.clear();
    d.state = State::Fulfilled;
    d.result = v;
    for r in reactions {
        queue_reaction(vm, r, v, false);
    }
}

pub fn reject_promise(vm: &mut Vm, p: Value, reason: Value) {
    let po = p.as_object().unwrap();
    if data(vm, po).map(|d| d.state) != Some(State::Pending) {
        return;
    }
    let d = data_mut(vm, po);
    let reactions = core::mem::take(&mut d.reject);
    d.fulfill.clear();
    d.state = State::Rejected;
    d.result = reason;
    let handled = d.handled;
    if !handled {
        if let Some(h) = vm.host.as_mut() {
            h.promise_rejection(p, false);
        }
    }
    for r in reactions {
        queue_reaction(vm, r, reason, true);
    }
}

fn queue_reaction(vm: &mut Vm, r: Reaction, arg: Value, rejected: bool) {
    let job = vm.native_fn_data("", 0, reaction_job, false, Box::new([r.cap[0], r.cap[1], r.cap[2], r.handler, arg, Value::bool(rejected)]));
    vm.jobs.push_back(Job { func: Value::object(job), this: Value::UNDEFINED, args: Vec::new() });
}

/// NewPromiseReactionJob.
fn reaction_job(vm: &mut Vm, _this: Value, _args: &[Value], _nt: Value) -> JsResult<Value> {
    let cap = [vm.native_data(0), vm.native_data(1), vm.native_data(2)];
    let handler = vm.native_data(3);
    let arg = vm.native_data(4);
    let rejected = vm.native_data(5) == Value::TRUE;
    let result = if handler.is_undefined() {
        if rejected { Err(Throw::Value(arg)) } else { Ok(arg) }
    } else {
        vm.call(handler, Value::UNDEFINED, &[arg])
    };
    if cap[0].is_undefined() && cap[1].is_undefined() {
        // Internal reaction (await): nothing more.
        return match result {
            Err(Throw::Terminate) => Err(Throw::Terminate),
            _ => Ok(Value::UNDEFINED),
        };
    }
    match result {
        Ok(v) => {
            vm.call(cap[1], Value::UNDEFINED, &[v])?;
        }
        Err(Throw::Value(e)) => {
            vm.call(cap[2], Value::UNDEFINED, &[e])?;
        }
        Err(t) => return Err(t),
    }
    Ok(Value::UNDEFINED)
}

/// PerformPromiseThen.
pub fn perform_then(vm: &mut Vm, p: ObjId, on_ful: Value, on_rej: Value, cap: [Value; 3]) {
    let on_ful = if vm.is_callable(on_ful) { on_ful } else { Value::UNDEFINED };
    let on_rej = if vm.is_callable(on_rej) { on_rej } else { Value::UNDEFINED };
    let fr = Reaction { cap, handler: on_ful };
    let rr = Reaction { cap, handler: on_rej };
    let (state, result, handled) = {
        let d = data(vm, p).unwrap();
        (d.state, d.result, d.handled)
    };
    match state {
        State::Pending => {
            let d = data_mut(vm, p);
            d.fulfill.push(fr);
            d.reject.push(rr);
        }
        State::Fulfilled => queue_reaction(vm, fr, result, false),
        State::Rejected => {
            if !handled {
                if let Some(h) = vm.host.as_mut() {
                    h.promise_rejection(Value::object(p), true);
                }
            }
            queue_reaction(vm, rr, result, true);
        }
    }
    data_mut(vm, p).handled = true;
}

/// NewPromiseCapability(C): [promise, resolve, reject].
pub fn new_capability(vm: &mut Vm, c: Value) -> JsResult<[Value; 3]> {
    if c == Value::object(vm.intrinsics.promise) {
        let pp = vm.intrinsics.promise_prototype;
        let p = new_promise(vm, pp);
        let (res, rej) = resolving_functions(vm, p);
        return Ok([Value::object(p), res, rej]);
    }
    if !vm.is_constructor(c) {
        return Err(vm.type_error("Promise capability constructor is not a constructor"));
    }
    let cell = vm.alloc_object(None, Class::Internal(Box::new([Value::UNDEFINED, Value::UNDEFINED])));
    let exec = vm.native_fn_data("", 2, capability_executor, false, Box::new([Value::object(cell)]));
    vm.root(Value::object(cell));
    let p = vm.construct(c, &[Value::object(exec)], c)?;
    let (res, rej) = match &vm.obj(cell).class {
        Class::Internal(v) => (v[0], v[1]),
        _ => unreachable!(),
    };
    if !vm.is_callable(res) || !vm.is_callable(rej) {
        return Err(vm.type_error("Promise resolve or reject function is not callable"));
    }
    Ok([p, res, rej])
}

fn capability_executor(vm: &mut Vm, _this: Value, args: &[Value], _nt: Value) -> JsResult<Value> {
    let cell = vm.native_data(0).as_object().unwrap();
    let (r0, r1) = match &vm.obj(cell).class {
        Class::Internal(v) => (v[0], v[1]),
        _ => unreachable!(),
    };
    if !r0.is_undefined() || !r1.is_undefined() {
        return Err(vm.type_error("Promise executor has already been invoked"));
    }
    let a = args.first().copied().unwrap_or(Value::UNDEFINED);
    let b = args.get(1).copied().unwrap_or(Value::UNDEFINED);
    if let Class::Internal(v) = &mut vm.obj_mut(cell).class {
        v[0] = a;
        v[1] = b;
    }
    Ok(Value::UNDEFINED)
}

/// PromiseResolve(C, x).
pub fn promise_resolve(vm: &mut Vm, c: Value, x: Value) -> JsResult<Value> {
    if is_promise(vm, x) {
        let kc = Key::Str(vm.names.constructor);
        let xc = vm.get(x.as_object().unwrap(), kc)?;
        if vm.same_value(xc, c) {
            return Ok(x);
        }
    }
    let cap = new_capability(vm, c)?;
    vm.call(cap[1], Value::UNDEFINED, &[x])?;
    Ok(cap[0])
}

// ----- async functions -------------------------------------------------

/// State object for an async function call (its promise).
pub fn start_async(vm: &mut Vm) -> JsResult<ObjId> {
    let pp = vm.intrinsics.promise_prototype;
    let p = new_promise(vm, pp);
    let f = vm.frames.last().unwrap();
    let st = GenState {
        state: GenStatus::Executing,
        code: f.code.clone(),
        pc: 0,
        regs: Vec::new(),
        env: None,
        env_depth: 0,
        this: Value::UNDEFINED,
        callee: Value::UNDEFINED,
        new_target: Value::UNDEFINED,
        args: None,
        promise: Value::object(p),
        queue: Vec::new(),
        awaiting_return: false,
    };
    Ok(vm.alloc_object(None, Class::Generator(Box::new(st))))
}

/// Await(value) for the suspended state object `g`: resumes it when the
/// promise settles.
pub fn await_value(vm: &mut Vm, g: ObjId, v: Value) -> JsResult<()> {
    let pc = Value::object(vm.intrinsics.promise);
    let p = promise_resolve(vm, pc, v)?;
    let on_ful = vm.native_fn_data("", 1, await_resume, false, Box::new([Value::object(g), Value::int(0)]));
    let on_rej = vm.native_fn_data("", 1, await_resume, false, Box::new([Value::object(g), Value::int(1)]));
    perform_then(vm, p.as_object().unwrap(), Value::object(on_ful), Value::object(on_rej), [Value::UNDEFINED; 3]);
    Ok(())
}

fn await_resume(vm: &mut Vm, _this: Value, args: &[Value], _nt: Value) -> JsResult<Value> {
    let g = vm.native_data(0).as_object().unwrap();
    let kind = vm.native_data(1).as_int().unwrap() as u8;
    let v = args.first().copied().unwrap_or(Value::UNDEFINED);
    if let Class::AsyncGenerator(_) = vm.obj(g).class {
        return crate::builtins::generator::async_gen_await_resume(vm, g, kind, v);
    }
    vm.resume(g, kind, v)?;
    Ok(Value::UNDEFINED)
}

/// CreateAsyncFromSyncIterator.
pub fn async_from_sync(vm: &mut Vm, it: Value, next: Value) -> JsResult<(Value, Value)> {
    let p = vm.intrinsics.async_from_sync_iterator_prototype;
    let o = vm.alloc_object(Some(p), Class::AsyncFromSyncIterator(it, next));
    let kn = Key::Str(vm.names.next);
    let n = vm.get(o, kn)?;
    Ok((Value::object(o), n))
}

// ----- the Promise constructor and prototype ---------------------------

pub fn init(vm: &mut Vm) {
    let op = vm.intrinsics.object_prototype;
    let proto = vm.alloc_object(Some(op), Class::Ordinary);
    vm.intrinsics.promise_prototype = proto;
    let c = vm.constructor("Promise", 1, promise_ctor, proto);
    vm.intrinsics.promise = c;
    vm.global_prop("Promise", Value::object(c));
    vm.method(proto, "then", 2, then);
    vm.method(proto, "catch", 1, catch);
    vm.method(proto, "finally", 1, finally);
    let tag = vm.new_string("Promise");
    let kt = Key::Sym(vm.intrinsics.sym_to_string_tag);
    vm.define_data(proto, kt, tag, attr::CONFIGURABLE);
    vm.method(c, "resolve", 1, static_resolve);
    vm.method(c, "reject", 1, static_reject);
    vm.method(c, "withResolvers", 0, with_resolvers);
    vm.method(c, "try", 1, static_try);
    vm.method(c, "all", 1, |vm, this, a, _| combinator(vm, this, a, 0));
    vm.method(c, "allSettled", 1, |vm, this, a, _| combinator(vm, this, a, 1));
    vm.method(c, "any", 1, |vm, this, a, _| combinator(vm, this, a, 2));
    vm.method(c, "race", 1, race);
    let sp = vm.intrinsics.sym_species;
    vm.getter_sym(c, sp, "[Symbol.species]", |_, this, _, _| Ok(this));
}

fn promise_ctor(vm: &mut Vm, _this: Value, args: &[Value], nt: Value) -> JsResult<Value> {
    if nt.is_undefined() {
        return Err(vm.type_error("Promise constructor cannot be invoked without 'new'"));
    }
    let exec = args.first().copied().unwrap_or(Value::UNDEFINED);
    if !vm.is_callable(exec) {
        return Err(vm.type_error("Promise resolver is not a function"));
    }
    let proto = vm.proto_from_ctor(nt, |i| i.promise_prototype)?;
    let p = new_promise(vm, proto);
    vm.root(Value::object(p));
    let (res, rej) = resolving_functions(vm, p);
    vm.root(res);
    vm.root(rej);
    if let Err(Throw::Value(e)) = vm.call(exec, Value::UNDEFINED, &[res, rej]) {
        vm.call(rej, Value::UNDEFINED, &[e])?;
    }
    Ok(Value::object(p))
}

fn this_promise(vm: &mut Vm, this: Value) -> JsResult<ObjId> {
    match this.as_object() {
        Some(o) if data(vm, o).is_some() => Ok(o),
        _ => Err(vm.type_error("Method Promise.prototype.then called on incompatible receiver")),
    }
}

fn then(vm: &mut Vm, this: Value, args: &[Value], _nt: Value) -> JsResult<Value> {
    let p = this_promise(vm, this)?;
    let pc = Value::object(vm.intrinsics.promise);
    let c = species_constructor(vm, p, pc)?;
    let cap = new_capability(vm, c)?;
    let a = args.first().copied().unwrap_or(Value::UNDEFINED);
    let b = args.get(1).copied().unwrap_or(Value::UNDEFINED);
    perform_then(vm, p, a, b, cap);
    Ok(cap[0])
}

/// SpeciesConstructor(O, default).
pub fn species_constructor(vm: &mut Vm, o: ObjId, default: Value) -> JsResult<Value> {
    let kc = Key::Str(vm.names.constructor);
    let c = vm.get(o, kc)?;
    if c.is_undefined() {
        return Ok(default);
    }
    let Some(co) = c.as_object() else {
        return Err(vm.type_error("object.constructor is not an object"));
    };
    let ks = Key::Sym(vm.intrinsics.sym_species);
    let s = vm.get(co, ks)?;
    if s.is_nullish() {
        return Ok(default);
    }
    if vm.is_constructor(s) {
        return Ok(s);
    }
    Err(vm.type_error("object.constructor[Symbol.species] is not a constructor"))
}

fn invoke_then(vm: &mut Vm, p: Value, a: Value, b: Value) -> JsResult<Value> {
    let kt = Key::Str(vm.names.then);
    let t = vm.get_v(p, kt)?;
    vm.call(t, p, &[a, b])
}

fn catch(vm: &mut Vm, this: Value, args: &[Value], _nt: Value) -> JsResult<Value> {
    let a = args.first().copied().unwrap_or(Value::UNDEFINED);
    invoke_then(vm, this, Value::UNDEFINED, a)
}

fn finally(vm: &mut Vm, this: Value, args: &[Value], _nt: Value) -> JsResult<Value> {
    let Some(p) = this.as_object() else {
        return Err(vm.type_error("Promise.prototype.finally called on a non-object"));
    };
    let pc = Value::object(vm.intrinsics.promise);
    let c = species_constructor(vm, p, pc)?;
    let f = args.first().copied().unwrap_or(Value::UNDEFINED);
    if !vm.is_callable(f) {
        return invoke_then(vm, this, f, f);
    }
    let then_finally = vm.native_fn_data("", 1, finally_then, false, Box::new([f, c]));
    let catch_finally = vm.native_fn_data("", 1, finally_catch, false, Box::new([f, c]));
    invoke_then(vm, this, Value::object(then_finally), Value::object(catch_finally))
}

fn finally_then(vm: &mut Vm, _this: Value, args: &[Value], _nt: Value) -> JsResult<Value> {
    let f = vm.native_data(0);
    let c = vm.native_data(1);
    let v = args.first().copied().unwrap_or(Value::UNDEFINED);
    let r = vm.call(f, Value::UNDEFINED, &[])?;
    let p = promise_resolve(vm, c, r)?;
    let value_thunk = vm.native_fn_data("", 0, |vm, _, _, _| Ok(vm.native_data(0)), false, Box::new([v]));
    invoke_then(vm, p, Value::object(value_thunk), Value::UNDEFINED)
}

fn finally_catch(vm: &mut Vm, _this: Value, args: &[Value], _nt: Value) -> JsResult<Value> {
    let f = vm.native_data(0);
    let c = vm.native_data(1);
    let v = args.first().copied().unwrap_or(Value::UNDEFINED);
    let r = vm.call(f, Value::UNDEFINED, &[])?;
    let p = promise_resolve(vm, c, r)?;
    let thrower = vm.native_fn_data("", 0, |vm, _, _, _| Err(Throw::Value(vm.native_data(0))), false, Box::new([v]));
    invoke_then(vm, p, Value::object(thrower), Value::UNDEFINED)
}

fn static_resolve(vm: &mut Vm, this: Value, args: &[Value], _nt: Value) -> JsResult<Value> {
    if !this.is_object() {
        return Err(vm.type_error("Promise.resolve called on non-object"));
    }
    promise_resolve(vm, this, args.first().copied().unwrap_or(Value::UNDEFINED))
}

fn static_reject(vm: &mut Vm, this: Value, args: &[Value], _nt: Value) -> JsResult<Value> {
    let cap = new_capability(vm, this)?;
    vm.call(cap[2], Value::UNDEFINED, &[args.first().copied().unwrap_or(Value::UNDEFINED)])?;
    Ok(cap[0])
}

fn with_resolvers(vm: &mut Vm, this: Value, _args: &[Value], _nt: Value) -> JsResult<Value> {
    let cap = new_capability(vm, this)?;
    let o = vm.new_object();
    let kp = vm.key("promise");
    vm.create_data_property_or_throw(o, kp, cap[0])?;
    let kr = vm.key("resolve");
    vm.create_data_property_or_throw(o, kr, cap[1])?;
    let kj = vm.key("reject");
    vm.create_data_property_or_throw(o, kj, cap[2])?;
    Ok(Value::object(o))
}

fn static_try(vm: &mut Vm, this: Value, args: &[Value], _nt: Value) -> JsResult<Value> {
    if !this.is_object() {
        return Err(vm.type_error("Promise.try called on non-object"));
    }
    let cap = new_capability(vm, this)?;
    let f = args.first().copied().unwrap_or(Value::UNDEFINED);
    let rest = if args.len() > 1 { &args[1..] } else { &[] };
    match vm.call(f, Value::UNDEFINED, rest) {
        Ok(v) => {
            vm.call(cap[1], Value::UNDEFINED, &[v])?;
        }
        Err(Throw::Value(e)) => {
            vm.call(cap[2], Value::UNDEFINED, &[e])?;
        }
        Err(t) => return Err(t),
    }
    Ok(cap[0])
}

/// Promise.all (0), allSettled (1), any (2).
fn combinator(vm: &mut Vm, c: Value, args: &[Value], kind: u8) -> JsResult<Value> {
    let cap = new_capability(vm, c)?;
    for v in cap {
        vm.root(v);
    }
    let r = combinator_inner(vm, c, args.first().copied().unwrap_or(Value::UNDEFINED), kind, cap);
    match r {
        Ok(v) => Ok(v),
        Err(Throw::Value(e)) => {
            vm.call(cap[2], Value::UNDEFINED, &[e])?;
            Ok(cap[0])
        }
        Err(t) => Err(t),
    }
}

fn get_resolve(vm: &mut Vm, c: Value) -> JsResult<Value> {
    let kr = Key::Str(vm.names.resolve);
    let r = vm.get_v(c, kr)?;
    if !vm.is_callable(r) {
        return Err(vm.type_error("Promise resolve is not a function"));
    }
    Ok(r)
}

fn combinator_inner(vm: &mut Vm, c: Value, iterable: Value, kind: u8, cap: [Value; 3]) -> JsResult<Value> {
    let resolve = get_resolve(vm, c)?;
    vm.root(resolve);
    let (it, next) = vm.get_iterator(iterable)?;
    vm.root(it);
    vm.root(next);
    // Shared state: [values array, remaining count].
    let values = vm.new_array(Vec::new());
    let state = vm.alloc_object(None, Class::Internal(Box::new([Value::object(values), Value::int(1)])));
    vm.root(Value::object(state));
    let mut index = 0u32;
    loop {
        let v = match vm.iterator_step(it, next) {
            Ok(Some(v)) => v,
            Ok(None) => break,
            Err(e) => return Err(e),
        };
        let ob = vm.obj_mut(values);
        ob.elements.push(Value::UNDEFINED);
        ob.length += 1;
        let r = (|| -> JsResult<()> {
            let next_p = vm.call(resolve, c, &[v])?;
            vm.root(next_p);
            if let Class::Internal(s) = &mut vm.obj_mut(state).class {
                s[1] = Value::int(s[1].as_int().unwrap() + 1);
            }
            let mk = |vm: &mut Vm, which: u8| -> Value {
                let f = vm.native_fn_data("", 1, element_fn, false, Box::new([Value::object(state), Value::from_u32(index), Value::int(which as i32), cap[1], cap[2], Value::FALSE]));
                Value::object(f)
            };
            let (on_ful, on_rej) = match kind {
                0 => (mk(vm, 0), cap[2]),
                1 => (mk(vm, 1), mk(vm, 2)),
                _ => (cap[1], mk(vm, 3)),
            };
            invoke_then(vm, next_p, on_ful, on_rej)?;
            Ok(())
        })();
        if let Err(e) = r {
            if let Throw::Value(_) = e {
                let _ = vm.iterator_close(it);
            }
            return Err(e);
        }
        index += 1;
    }
    // Finish: remaining -= 1; if 0, settle.
    let done = if let Class::Internal(s) = &mut vm.obj_mut(state).class {
        let n = s[1].as_int().unwrap() - 1;
        s[1] = Value::int(n);
        n == 0
    } else {
        false
    };
    if done {
        settle_combinator(vm, kind, values, cap[1], cap[2])?;
    }
    Ok(cap[0])
}

fn settle_combinator(vm: &mut Vm, kind: u8, values: ObjId, resolve: Value, reject: Value) -> JsResult<()> {
    if kind == 2 {
        let p = vm.intrinsics.aggregate_error_prototype;
        let e = vm.make_error(p, "All promises were rejected");
        let ke = Key::Str(vm.names.errors);
        vm.define_data(e.as_object().unwrap(), ke, Value::object(values), attr::HIDDEN);
        vm.call(reject, Value::UNDEFINED, &[e])?;
    } else {
        vm.call(resolve, Value::UNDEFINED, &[Value::object(values)])?;
    }
    Ok(())
}

/// Resolve/reject element functions of the combinators.
fn element_fn(vm: &mut Vm, _this: Value, args: &[Value], _nt: Value) -> JsResult<Value> {
    // Called at most once.
    let me = vm.current_native.as_object().unwrap();
    if vm.native_data(5) == Value::TRUE {
        return Ok(Value::UNDEFINED);
    }
    vm.set_native_data(me, 5, Value::TRUE);
    let state = vm.native_data(0).as_object().unwrap();
    let index = vm.native_data(1).as_int().unwrap() as usize;
    let which = vm.native_data(2).as_int().unwrap();
    let (resolve, reject) = (vm.native_data(3), vm.native_data(4));
    let x = args.first().copied().unwrap_or(Value::UNDEFINED);
    let values = match &vm.obj(state).class {
        Class::Internal(s) => s[0].as_object().unwrap(),
        _ => unreachable!(),
    };
    let stored = match which {
        1 | 2 => {
            let o = vm.new_object();
            let ks = vm.key("status");
            let st = vm.new_string(if which == 1 { "fulfilled" } else { "rejected" });
            vm.create_data_property_or_throw(o, ks, st)?;
            let k = if which == 1 { Key::Str(vm.names.value) } else { vm.key("reason") };
            vm.create_data_property_or_throw(o, k, x)?;
            Value::object(o)
        }
        _ => x,
    };
    vm.obj_mut(values).elements[index] = stored;
    let done = if let Class::Internal(s) = &mut vm.obj_mut(state).class {
        let n = s[1].as_int().unwrap() - 1;
        s[1] = Value::int(n);
        n == 0
    } else {
        false
    };
    if done {
        let kind = match which {
            0 => 0,
            1 | 2 => 1,
            _ => 2,
        };
        settle_combinator(vm, kind, values, resolve, reject)?;
    }
    Ok(Value::UNDEFINED)
}

fn race(vm: &mut Vm, c: Value, args: &[Value], _nt: Value) -> JsResult<Value> {
    let cap = new_capability(vm, c)?;
    for v in cap {
        vm.root(v);
    }
    let r = (|| -> JsResult<Value> {
        let resolve = get_resolve(vm, c)?;
        vm.root(resolve);
        let (it, next) = vm.get_iterator(args.first().copied().unwrap_or(Value::UNDEFINED))?;
        vm.root(it);
        vm.root(next);
        loop {
            let Some(v) = vm.iterator_step(it, next)? else { break };
            let r = (|| -> JsResult<()> {
                let p = vm.call(resolve, c, &[v])?;
                invoke_then(vm, p, cap[1], cap[2])?;
                Ok(())
            })();
            if let Err(e) = r {
                if let Throw::Value(_) = e {
                    let _ = vm.iterator_close(it);
                }
                return Err(e);
            }
        }
        Ok(cap[0])
    })();
    match r {
        Ok(v) => Ok(v),
        Err(Throw::Value(e)) => {
            vm.call(cap[2], Value::UNDEFINED, &[e])?;
            Ok(cap[0])
        }
        Err(t) => Err(t),
    }
}

pub fn _v() -> Vec<u8> {
    vec![]
}
