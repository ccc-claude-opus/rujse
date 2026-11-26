//! The interpreter loop, calls, construction, exceptions, generators.

use alloc::boxed::Box;
use alloc::rc::Rc;
use alloc::vec::Vec;

use super::props::PropDesc;
use super::*;
use crate::bytecode::{BinKind, Const, FuncKind, Op};
use crate::object::{attr, Class, Closure, Key};

/// How a generator frame left the interpreter.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum GenSignal {
    None,
    Yield,
    YieldRaw,
    Return,
    Await,
}

/// Safepoint interval (ticks between checks).
const TICKS: i32 = 1024;

impl Vm {
    // ----- calls from Rust --------------------------------------------

    /// Calls `f` with `this` and arguments.
    pub fn call(&mut self, f: Value, this: Value, args: &[Value]) -> JsResult<Value> {
        let Some(fo) = f.as_object() else {
            let s = self.display(f);
            return Err(self.type_error(&alloc::format!("{s} is not a function")));
        };
        match &self.obj(fo).class {
            Class::Native(n) => {
                let fp = n.f;
                return self.native_call(fp, f, this, args, Value::UNDEFINED);
            }
            Class::Function(c) => {
                let c = (**c).clone();
                if matches!(c.code.kind, FuncKind::ClassConstructor | FuncKind::DerivedConstructor) {
                    return Err(self.type_error("Class constructor cannot be invoked without 'new'"));
                }
                let base = self.push_args(f, this, args);
                return self.enter(fo, &c, this, Value::UNDEFINED, base, args.len(), FrameKind::Entry);
            }
            Class::Bound { target, this: bt, args: bargs } => {
                let (target, bt) = (*target, *bt);
                let mut all: Vec<Value> = bargs.to_vec();
                all.extend_from_slice(args);
                return self.call(Value::object(target), bt, &all);
            }
            Class::Proxy { callable: true, .. } => return crate::builtins::proxy::call(self, fo, this, args),
            _ => {}
        }
        let s = self.display(f);
        Err(self.type_error(&alloc::format!("{s} is not a function")))
    }

    /// `new f(...args)` with a new.target.
    pub fn construct(&mut self, f: Value, args: &[Value], new_target: Value) -> JsResult<Value> {
        if !self.is_constructor(f) {
            let s = self.display(f);
            return Err(self.type_error(&alloc::format!("{s} is not a constructor")));
        }
        let fo = f.as_object().unwrap();
        match &self.obj(fo).class {
            Class::Native(n) => {
                let fp = n.f;
                self.native_call(fp, f, Value::UNDEFINED, args, new_target)
            }
            Class::Function(c) => {
                let c = (**c).clone();
                let this = if c.code.kind == FuncKind::DerivedConstructor {
                    Value::UNINIT
                } else {
                    let p = self.proto_from_ctor(new_target, |i| i.object_prototype)?;
                    Value::object(self.alloc_object(Some(p), Class::Ordinary))
                };
                let base = self.push_args(f, this, args);
                self.enter(fo, &c, this, new_target, base, args.len(), FrameKind::ConstructEntry)
            }
            Class::Bound { target, args: bargs, .. } => {
                let target = *target;
                let mut all: Vec<Value> = bargs.to_vec();
                all.extend_from_slice(args);
                let nt = if new_target == f { Value::object(target) } else { new_target };
                self.construct(Value::object(target), &all, nt)
            }
            Class::Proxy { .. } => crate::builtins::proxy::construct(self, fo, args, new_target),
            _ => unreachable!(),
        }
    }

    /// GetPrototypeFromConstructor.
    pub fn proto_from_ctor(&mut self, ctor: Value, fallback: fn(&Intrinsics) -> ObjId) -> JsResult<ObjId> {
        if let Some(c) = ctor.as_object() {
            let k = Key::Str(self.names.prototype);
            let p = self.get(c, k)?;
            if let Some(p) = p.as_object() {
                return Ok(p);
            }
            // The fallback comes from the constructor's realm (one realm here).
        }
        Ok(fallback(&self.intrinsics))
    }

    /// Pushes callee, this and the arguments at the top of the stack;
    /// returns the index of the first argument.
    fn push_args(&mut self, f: Value, this: Value, args: &[Value]) -> usize {
        let base = self.sp + 2;
        let need = base + args.len();
        if self.stack.len() < need {
            self.stack.resize(need, Value::UNDEFINED);
        }
        self.stack[base - 2] = f;
        self.stack[base - 1] = this;
        self.stack[base..need].copy_from_slice(args);
        self.sp = need;
        base
    }

    fn native_call(&mut self, f: crate::object::NativeFn, callee: Value, this: Value, args: &[Value], nt: Value) -> JsResult<Value> {
        if self.native_depth >= self.limits.max_native_depth {
            return Err(self.range_error("Maximum call stack size exceeded"));
        }
        self.native_depth += 1;
        let roots = self.native_roots.len();
        self.native_roots.push(callee);
        self.native_roots.push(this);
        self.native_roots.extend_from_slice(args);
        let saved = self.current_native;
        self.current_native = callee;
        let r = f(self, this, args, nt);
        self.current_native = saved;
        self.native_roots.truncate(roots);
        self.native_depth -= 1;
        r
    }

    /// Pushes a frame for a closure and runs it to completion.
    #[allow(clippy::too_many_arguments)]
    fn enter(&mut self, fo: ObjId, c: &Closure, this: Value, nt: Value, base: usize, argc: usize, kind: FrameKind) -> JsResult<Value> {
        if self.native_depth >= self.limits.max_native_depth {
            self.sp = base - 2;
            return Err(self.range_error("Maximum call stack size exceeded"));
        }
        if let Err(e) = self.poll_limits() {
            self.sp = base - 2;
            return Err(e);
        }
        self.native_depth += 1;
        let depth = self.frames.len();
        let r = match self.push_frame(fo, c.code.clone(), c.env, this, nt, base, argc, kind) {
            Ok(()) => self.run(depth),
            Err(e) => Err(e),
        };
        self.native_depth -= 1;
        r
    }

    /// Sets up a frame for closure `c` whose arguments start at `base`.
    #[allow(clippy::too_many_arguments)]
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn push_frame(&mut self, fo: ObjId, code: Rc<Code>, env: Option<EnvId>, this: Value, nt: Value, base: usize, argc: usize, kind: FrameKind) -> JsResult<()> {
        if self.frames.len() >= self.limits.max_frames {
            return Err(self.range_error("Maximum call stack size exceeded"));
        }
        let nregs = code.nregs as usize;
        let nparams = code.nparams as usize;
        let args = if code.keep_args { Some(self.stack[base..base + argc].to_vec().into_boxed_slice()) } else { None };
        let top = base + nregs.max(argc);
        if self.stack.len() < top {
            self.stack.resize(top + 16, Value::UNDEFINED);
        }
        let from = argc.min(nparams);
        for v in &mut self.stack[base + from..base + nregs] {
            *v = Value::UNDEFINED;
        }
        let this = if code.strict || code.kind == FuncKind::Arrow || matches!(kind, FrameKind::Construct | FrameKind::ConstructEntry) {
            this
        } else if this.is_nullish() {
            Value::object(self.global_this)
        } else if this.is_object() {
            this
        } else {
            Value::object(self.to_object(this)?)
        };
        code.calls.set(code.calls.get().wrapping_add(1));
        self.sp = base + nregs;
        self.frames.push(Frame {
            code,
            pc: 0,
            base: base as u32,
            env,
            env_depth: 0,
            this,
            callee: Value::object(fo),
            new_target: nt,
            args,
            argc: argc as u16,
            kind,
            generator: None,
        });
        if self.debugger.is_some() {
            self.debug_on_enter();
        }
        // Async functions: a promise and a state object from the start.
        let f = self.frames.last().unwrap();
        if f.code.is_async && !f.code.is_generator {
            let g = crate::builtins::promise::start_async(self)?;
            self.frames.last_mut().unwrap().generator = Some(g);
        }
        Ok(())
    }

    /// Runs code (a script, module or eval body) with the given scope.
    pub fn run_code(&mut self, code: Rc<Code>, env: Option<EnvId>, this: Value, callee: Value, nt: Value) -> JsResult<Value> {
        let base = self.sp + 2;
        let nregs = code.nregs as usize;
        if self.stack.len() < base + nregs {
            self.stack.resize(base + nregs + 16, Value::UNDEFINED);
        }
        for v in &mut self.stack[base..base + nregs] {
            *v = Value::UNDEFINED;
        }
        self.sp = base + nregs;
        let depth = self.frames.len();
        self.frames.push(Frame {
            code,
            pc: 0,
            base: base as u32,
            env,
            env_depth: 0,
            this,
            callee,
            new_target: nt,
            args: None,
            argc: 0,
            kind: FrameKind::Entry,
            generator: None,
        });
        self.native_depth += 1;
        let r = self.run(depth);
        self.native_depth -= 1;
        r
    }

    // ----- environments ---------------------------------------------------

    #[inline]
    fn env_at(&self, mut e: Option<EnvId>, hops: u16) -> EnvId {
        for _ in 0..hops {
            e = self.envs.get(e.unwrap().0).parent;
        }
        e.expect("environment chain")
    }

    // ----- the interpreter loop -------------------------------------------

    /// Runs frames until the frame count drops back to `entry`.
    pub(crate) fn run(&mut self, entry: usize) -> JsResult<Value> {
        let mut acc = Value::UNDEFINED;
        'frame: loop {
            if let Some(v) = self.pending_acc.take() {
                acc = v;
            }
            let fi = self.frames.len() - 1;
            let code = self.frames[fi].code.clone();
            let base = self.frames[fi].base as usize;
            let mut pc = self.frames[fi].pc as usize;
            let body = code.body();
            let ops: &[core::cell::Cell<Op>] = &body.ops;
            #[allow(unused_assignments)]
            let mut error: Throw = Throw::Terminate;
            macro_rules! r {
                ($r:expr) => {
                    self.stack[base + $r as usize]
                };
            }
            macro_rules! konst {
                ($k:expr) => {
                    match &body.consts[$k as usize] {
                        Const::Value(v) => *v,
                        _ => Value::UNDEFINED,
                    }
                };
            }
            macro_rules! name_key {
                ($k:expr) => {{
                    let s = konst!($k).as_string().unwrap();
                    self.key_from_str(s)
                }};
            }
            'ops: loop {
                macro_rules! tri {
                    ($e:expr) => {
                        match $e {
                            Ok(v) => v,
                            Err(t) => {
                                error = t;
                                self.frames[fi].pc = pc as u32;
                                break 'ops;
                            }
                        }
                    };
                }
                macro_rules! safepoint {
                    () => {
                        self.ticks -= 1;
                        if self.ticks <= 0 {
                            self.frames[fi].pc = pc as u32;
                            tri!(self.safepoint(acc));
                        }
                    };
                }
                let op = ops[pc].get();
                pc += 1;
                match op {
                    Op::Nop => {}
                    Op::Break(i) => {
                        pc -= 1;
                        self.frames[fi].pc = pc as u32;
                        tri!(self.debug_break(fi, pc, i, acc));
                        // Run the original instruction: put it back until
                        // the next safepoint (any way back to this pc
                        // passes one: a backward jump or a call).
                        let orig = body.patches.borrow()[i as usize].op;
                        ops[pc].set(orig);
                        self.debug_rearm(&code, pc as u32);
                    }
                    Op::BinRR { kind, dst, lhs, rhs } => {
                        let (a, b) = (r!(lhs), r!(rhs));
                        let fast = match (a.as_int(), b.as_int()) {
                            (Some(x), Some(y)) => int_binop(kind, x, y),
                            _ if a.is_number() && b.is_number() => num_binop(kind, a.num(), b.num()),
                            _ => None,
                        };
                        acc = match fast {
                            Some(v) => v,
                            None => {
                                self.frames[fi].pc = pc as u32;
                                tri!(self.bin_generic(kind, a, b))
                            }
                        };
                        r!(dst) = acc;
                    }
                    Op::CmpJmp { kind, lhs, rhs, target, when } => {
                        let (a, b) = (r!(lhs), r!(rhs));
                        let t = match (a.as_int(), b.as_int()) {
                            (Some(x), Some(y)) => int_compare(kind, x, y),
                            _ if a.is_number() && b.is_number() => num_compare(kind, a.num(), b.num()),
                            _ => {
                                self.frames[fi].pc = pc as u32;
                                tri!(self.bin_generic(kind, a, b)) == Value::TRUE
                            }
                        };
                        acc = Value::bool(t);
                        if t == when {
                            if (target as usize) < pc {
                                safepoint!();
                            }
                            pc = target as usize;
                        }
                    }
                    Op::IncReg(x) => {
                        let v = r!(x);
                        acc = match v.as_int() {
                            Some(i) if i < i32::MAX => Value::int(i + 1),
                            _ => tri!(self.inc_dec_slow(v, true)),
                        };
                        r!(x) = acc;
                    }
                    Op::DecReg(x) => {
                        let v = r!(x);
                        acc = match v.as_int() {
                            Some(i) if i > i32::MIN => Value::int(i - 1),
                            _ => tri!(self.inc_dec_slow(v, false)),
                        };
                        r!(x) = acc;
                    }
                    Op::LdaUndef => acc = Value::UNDEFINED,
                    Op::LdaNull => acc = Value::NULL,
                    Op::LdaTrue => acc = Value::TRUE,
                    Op::LdaFalse => acc = Value::FALSE,
                    Op::LdaHole => acc = Value::HOLE,
                    Op::LdaUninit => acc = Value::UNINIT,
                    Op::LdaInt(i) => acc = Value::int(i),
                    Op::LdaConst(k) => {
                        acc = match &body.consts[k as usize] {
                            Const::Value(v) => *v,
                            Const::BigInt(s) => tri!(crate::builtins::bigint::from_literal(self, s)),
                            _ => Value::UNDEFINED,
                        }
                    }
                    Op::Ldar(x) => acc = r!(x),
                    Op::Star(x) => r!(x) = acc,
                    Op::Mov(a, b) => r!(b) = r!(a),
                    Op::LdaThis => {
                        acc = self.frames[fi].this;
                        if acc.is_uninit() {
                            tri!(Err(self.reference_error("Must call super constructor in derived class before accessing 'this' or returning from derived constructor")));
                        }
                    }
                    Op::LdaNewTarget => acc = self.frames[fi].new_target,
                    Op::LdaCallee => acc = self.frames[fi].callee,
                    Op::PushEnv { n, names } => {
                        let parent = self.frames[fi].env;
                        let e = self.new_env(n as usize, Value::UNDEFINED, parent);
                        if names != u32::MAX {
                            if let Const::Names(ns) = &body.consts[names as usize] {
                                self.envs.get_mut(e.0).names = Some(ns.clone());
                            }
                        }
                        let f = &mut self.frames[fi];
                        f.env = Some(e);
                        f.env_depth += 1;
                    }
                    Op::PushWith => {
                        let o = tri!(self.to_object(acc));
                        let parent = self.frames[fi].env;
                        let e = self.new_env(0, Value::UNDEFINED, parent);
                        self.envs.get_mut(e.0).with = Some(o);
                        let f = &mut self.frames[fi];
                        f.env = Some(e);
                        f.env_depth += 1;
                    }
                    Op::PopEnv => {
                        let f = &mut self.frames[fi];
                        let e = f.env.unwrap();
                        f.env = self.envs.get(e.0).parent;
                        f.env_depth -= 1;
                    }
                    Op::PopEnvTo(d) => {
                        while self.frames[fi].env_depth > d {
                            let e = self.frames[fi].env.unwrap();
                            self.frames[fi].env = self.envs.get(e.0).parent;
                            self.frames[fi].env_depth -= 1;
                        }
                    }
                    Op::CloneEnv => {
                        let e = self.frames[fi].env.unwrap();
                        let (slots, parent, names) = {
                            let en = self.envs.get(e.0);
                            (en.slots.clone(), en.parent, en.names.clone())
                        };
                        let n = self.new_env(0, Value::UNDEFINED, parent);
                        let en = self.envs.get_mut(n.0);
                        en.slots = slots;
                        en.names = names;
                        self.frames[fi].env = Some(n);
                    }
                    Op::LdaEnv(h, s) => {
                        let e = self.env_at(self.frames[fi].env, h);
                        acc = self.envs.get(e.0).slots[s as usize];
                    }
                    Op::StaEnv(h, s) => {
                        let e = self.env_at(self.frames[fi].env, h);
                        self.envs.get_mut(e.0).slots[s as usize] = acc;
                    }
                    Op::LdaEnvChecked(h, s, k) => {
                        let e = self.env_at(self.frames[fi].env, h);
                        acc = self.envs.get(e.0).slots[s as usize];
                        if acc.is_uninit() {
                            tri!(Err(self.tdz_error(konst!(k))));
                        }
                    }
                    Op::StaEnvChecked(h, s, k) => {
                        let e = self.env_at(self.frames[fi].env, h);
                        if self.envs.get(e.0).slots[s as usize].is_uninit() {
                            tri!(Err(self.tdz_error(konst!(k))));
                        }
                        self.envs.get_mut(e.0).slots[s as usize] = acc;
                    }
                    Op::LdaImport(h, i) => {
                        let e = self.env_at(self.frames[fi].env, h);
                        let (te, ts) = self.envs.get(e.0).imports.as_ref().unwrap()[i as usize];
                        acc = self.envs.get(te.0).slots[ts as usize];
                        if acc.is_uninit() {
                            tri!(Err(self.reference_error("Cannot access an imported binding before its module is initialized")));
                        }
                    }
                    Op::CheckInit(x, k) => {
                        if r!(x).is_uninit() {
                            tri!(Err(self.tdz_error(konst!(k))));
                        }
                    }
                    Op::ThrowConstAssign(k) => {
                        let n = self.display(konst!(k));
                        tri!(Err(self.type_error(&alloc::format!("Assignment to constant variable '{n}'."))));
                    }
                    Op::LdaGlobal(k) => {
                        acc = match self.global_ic_load(body.ics[k as usize].get()) {
                            Some(v) => v,
                            None => tri!(self.get_global_cached(&code, k as usize, konst!(k).as_string().unwrap(), true)),
                        }
                    }
                    Op::LdaGlobalOrUndef(k) => {
                        acc = match self.global_ic_load(body.ics[k as usize].get()) {
                            Some(v) => v,
                            None => tri!(self.get_global_cached(&code, k as usize, konst!(k).as_string().unwrap(), false)),
                        }
                    }
                    Op::StaGlobal(k, strict) => {
                        if !self.global_ic_store(body.ics[k as usize].get(), acc) {
                            tri!(self.set_global_cached(&code, k as usize, konst!(k).as_string().unwrap(), acc, strict));
                        }
                    }
                    Op::InitGlobalLex(k) => {
                        let n = konst!(k).as_string().unwrap();
                        if let Some(&i) = self.global_lex.get(&n) {
                            self.global_cells[i as usize].0 = acc;
                        } else {
                            // A var/function at script level (Annex B copy).
                            let key = self.key_from_str(n);
                            tri!(self.set(self.global, key, acc));
                        }
                    }
                    Op::DeclareGlobals(k) => {
                        let Const::Globals(d) = &body.consts[k as usize] else { unreachable!() };
                        let d = d.clone();
                        self.frames[fi].pc = pc as u32;
                        tri!(self.declare_globals(&d, fi));
                    }
                    Op::LdaName(k) => acc = tri!(self.get_name(fi, konst!(k).as_string().unwrap(), true).map(|x| x.0)),
                    Op::LdaNameOrUndef(k) => acc = tri!(self.get_name(fi, konst!(k).as_string().unwrap(), false).map(|x| x.0)),
                    Op::LdaNameWithThis(k, t) => {
                        let (v, this) = tri!(self.get_name(fi, konst!(k).as_string().unwrap(), true));
                        acc = v;
                        r!(t) = this;
                    }
                    Op::StaName(k, strict) => tri!(self.set_name(fi, konst!(k).as_string().unwrap(), acc, strict)),
                    Op::DeleteName(k) => acc = Value::bool(tri!(self.delete_name(fi, konst!(k).as_string().unwrap()))),
                    Op::DeclareEvalVar(_) => {}
                    Op::GetNamed(o, k, ic) => {
                        let obj = r!(o);
                        acc = match self.prop_ic_load(obj, body.ics[ic as usize].get()) {
                            Some(v) => v,
                            None => {
                                let key = name_key!(k);
                                tri!(self.get_named_cached(&code, ic as usize, obj, key))
                            }
                        };
                    }
                    Op::SetNamed(o, k, ic) => {
                        let obj = r!(o);
                        if !self.prop_ic_store(obj, body.ics[ic as usize].get(), acc) {
                            let key = name_key!(k);
                            tri!(self.set_named_cached(&code, ic as usize, obj, key, acc));
                        }
                    }
                    Op::GetKeyed(o) => {
                        let obj = r!(o);
                        acc = tri!(self.get_keyed(obj, acc));
                    }
                    Op::SetKeyed(o, kr) => {
                        let obj = r!(o);
                        let kv = r!(kr);
                        tri!(self.set_keyed(obj, kv, acc, code.strict));
                    }
                    Op::DeleteNamed(o, k) => {
                        let obj = r!(o);
                        let key = name_key!(k);
                        acc = Value::bool(tri!(self.delete_prop(obj, key, code.strict)));
                    }
                    Op::DeleteKeyed(o) => {
                        let obj = r!(o);
                        let key = tri!(self.to_property_key(acc));
                        acc = Value::bool(tri!(self.delete_prop(obj, key, code.strict)));
                    }
                    Op::In(o) => {
                        let key = r!(o);
                        acc = Value::bool(tri!(self.op_in(key, acc)));
                    }
                    Op::GetSuperNamed(k) => {
                        let key = name_key!(k);
                        acc = tri!(self.super_get(fi, acc, key));
                    }
                    Op::GetSuperKeyed(t) => {
                        let key = tri!(self.to_property_key(acc));
                        let this = r!(t);
                        acc = tri!(self.super_get(fi, this, key));
                    }
                    Op::SetSuperNamed(k, t) => {
                        let key = name_key!(k);
                        let this = r!(t);
                        tri!(self.super_set(fi, this, key, acc, code.strict));
                    }
                    Op::SetSuperKeyed(t, kr) => {
                        let key = tri!(self.to_property_key(r!(kr)));
                        let this = r!(t);
                        tri!(self.super_set(fi, this, key, acc, code.strict));
                    }
                    Op::GetPrivate(o) => {
                        let obj = r!(o);
                        acc = tri!(self.private_get(obj, acc));
                    }
                    Op::SetPrivate(o, kr) => {
                        let obj = r!(o);
                        let key = r!(kr);
                        tri!(self.private_set(obj, key, acc));
                    }
                    Op::HasPrivate(kr) => {
                        let o = match acc.as_object() {
                            Some(o) => o,
                            None => tri!(Err(self.type_error("Cannot use 'in' operator to search for a private field in a non-object"))),
                        };
                        let sym = r!(kr).as_symbol().unwrap();
                        acc = Value::bool(self.raw_get_own(o, Key::Sym(sym)).is_some());
                    }
                    Op::DefinePrivate(o, kr) => {
                        let obj = r!(o);
                        let key = r!(kr);
                        tri!(self.private_define(obj, key, acc));
                    }
                    Op::DefineField(o, kr) => {
                        let obj = r!(o).as_object().unwrap();
                        let key = tri!(self.to_property_key(r!(kr)));
                        tri!(self.create_data_property_or_throw(obj, key, acc));
                    }
                    Op::NewObject => acc = Value::object(self.new_object()),
                    Op::NewArray(n) => {
                        let a = self.new_array(Vec::with_capacity(n as usize));
                        acc = Value::object(a);
                    }
                    Op::ArrayPush(a) => {
                        let o = r!(a).as_object().unwrap();
                        let ob = self.obj_mut(o);
                        ob.elements.push(acc);
                        ob.length += 1;
                    }
                    Op::ArrayHole(a) => {
                        let o = r!(a).as_object().unwrap();
                        let ob = self.obj_mut(o);
                        ob.elements.push(Value::HOLE);
                        ob.length += 1;
                    }
                    Op::ArraySpread(a) => {
                        let o = r!(a).as_object().unwrap();
                        self.frames[fi].pc = pc as u32;
                        tri!(self.spread_into(o, acc));
                    }
                    Op::DefineNamed(o, k, ic) => {
                        let obj = r!(o).as_object().unwrap();
                        if !self.define_ic(obj, body.ics[ic as usize].get(), acc) {
                            let key = name_key!(k);
                            tri!(self.define_named_cached(&code, ic as usize, obj, key, acc));
                        }
                    }
                    Op::DefineKeyed(o, kr) => {
                        let obj = r!(o).as_object().unwrap();
                        let key = tri!(self.to_property_key(r!(kr)));
                        tri!(self.create_data_property_or_throw(obj, key, acc));
                    }
                    Op::DefineGetter(o, kr) | Op::DefineSetter(o, kr) => {
                        let obj = r!(o).as_object().unwrap();
                        let key = tri!(self.to_property_key(r!(kr)));
                        let is_get = matches!(op, Op::DefineGetter(..));
                        tri!(self.define_accessor_half(obj, key, acc, is_get, true));
                    }
                    Op::DefineMethod { obj, key, kind, enumerable } => {
                        let o = r!(obj).as_object().unwrap();
                        let k = tri!(self.to_property_key(r!(key)));
                        let f = acc.as_object().unwrap();
                        if let Class::Function(c) = &mut self.obj_mut(f).class {
                            c.home = Some(o);
                        }
                        match kind {
                            0 => {
                                let a = if enumerable { attr::DEFAULT } else { attr::HIDDEN };
                                tri!(self.define_property_or_throw(o, k, PropDesc::data(acc, a)));
                            }
                            1 => tri!(self.define_accessor_half(o, k, acc, true, enumerable)),
                            _ => tri!(self.define_accessor_half(o, k, acc, false, enumerable)),
                        }
                    }
                    Op::CopyDataProps(t) => {
                        let target = r!(t).as_object().unwrap();
                        tri!(self.copy_data_properties(target, acc, &[]));
                    }
                    Op::CopyDataPropsExcept { target, from, n } => {
                        let t = r!(target).as_object().unwrap();
                        let mut ex = Vec::with_capacity(n as usize);
                        for i in 0..n {
                            let kv = r!(from + i);
                            ex.push(tri!(self.to_property_key(kv)));
                        }
                        tri!(self.copy_data_properties(t, acc, &ex));
                    }
                    Op::SetProtoLiteral(o) => {
                        let obj = r!(o).as_object().unwrap();
                        if acc.is_object() || acc.is_null() {
                            self.obj_mut(obj).proto = acc.as_object();
                        }
                    }
                    Op::NewRegExp(p, f) => {
                        let (p, f) = (konst!(p), konst!(f));
                        acc = tri!(crate::builtins::regexp::create(self, p, f));
                    }
                    Op::Closure(i) => {
                        let child = body.children[i as usize].clone();
                        let env = self.frames[fi].env;
                        let home = if child.kind == FuncKind::Arrow { self.callee_home(fi) } else { None };
                        acc = Value::object(self.make_closure(child, env, home));
                    }
                    Op::SetFunctionName(kr, prefix) => {
                        let f = acc.as_object().unwrap();
                        let key = r!(kr);
                        tri!(self.set_function_name(f, key, prefix));
                    }
                    Op::SetHome(o) => {
                        let h = r!(o).as_object();
                        if let Class::Function(c) = &mut self.obj_mut(acc.as_object().unwrap()).class {
                            c.home = h;
                        }
                    }
                    Op::TemplateObject(i) => acc = self.template_object(&code, i),
                    Op::CreateClass { code: ci, proto, name } => {
                        let child = body.children[ci as usize].clone();
                        let env = self.frames[fi].env;
                        let name = konst!(name);
                        let (f, p) = tri!(self.create_class(child, env, acc, name));
                        r!(proto) = Value::object(p);
                        acc = Value::object(f);
                    }
                    Op::SetClassFields(c) => {
                        let f = r!(c).as_object().unwrap();
                        if let Class::Function(cl) = &mut self.obj_mut(f).class {
                            cl.fields = Some(acc);
                        }
                    }
                    Op::NewPrivateName(k) => {
                        let d = konst!(k).as_string();
                        let s = self.new_symbol(d);
                        self.symbols.get_mut(s.0).private = true;
                        acc = Value::symbol(s);
                    }
                    Op::AddPrivateMethod { ctor, key, kind, is_static } => {
                        let c = r!(ctor).as_object().unwrap();
                        let k = r!(key);
                        tri!(self.add_private_method(c, k, acc, kind, is_static));
                    }
                    Op::InitFields => {}
                    Op::InitFieldsOf(c) => {
                        let f = r!(c);
                        self.frames[fi].pc = pc as u32;
                        tri!(self.init_fields(f, acc));
                    }
                    // ----- operators
                    Op::Add(x) => {
                        let a = r!(x);
                        acc = if let (Some(i), Some(j)) = (a.as_int(), acc.as_int()) {
                            match i.checked_add(j) {
                                Some(s) => Value::int(s),
                                None => Value::number(i as f64 + j as f64),
                            }
                        } else if a.is_number() && acc.is_number() {
                            Value::number(a.num() + acc.num())
                        } else {
                            self.frames[fi].pc = pc as u32;
                            tri!(self.op_add(a, acc))
                        };
                    }
                    Op::AddInt(i) => {
                        acc = if let Some(a) = acc.as_int() {
                            match a.checked_add(i) {
                                Some(s) => Value::int(s),
                                None => Value::number(a as f64 + i as f64),
                            }
                        } else if acc.is_number() {
                            Value::number(acc.num() + i as f64)
                        } else {
                            tri!(self.op_add(acc, Value::int(i)))
                        };
                    }
                    Op::Sub(x) => {
                        let a = r!(x);
                        acc = if let (Some(i), Some(j)) = (a.as_int(), acc.as_int()) {
                            match i.checked_sub(j) {
                                Some(s) => Value::int(s),
                                None => Value::number(i as f64 - j as f64),
                            }
                        } else if a.is_number() && acc.is_number() {
                            Value::number(a.num() - acc.num())
                        } else {
                            tri!(self.binary_arith(crate::ast::BinaryOp::Sub, a, acc))
                        };
                    }
                    Op::Mul(x) => {
                        let a = r!(x);
                        acc = if a.is_number() && acc.is_number() {
                            Value::number(a.num() * acc.num())
                        } else {
                            tri!(self.binary_arith(crate::ast::BinaryOp::Mul, a, acc))
                        };
                    }
                    Op::Div(x) => {
                        let a = r!(x);
                        acc = if a.is_number() && acc.is_number() {
                            Value::number(a.num() / acc.num())
                        } else {
                            tri!(self.binary_arith(crate::ast::BinaryOp::Div, a, acc))
                        };
                    }
                    Op::Mod(x) => {
                        let a = r!(x);
                        acc = match (a.as_int(), acc.as_int()) {
                            (Some(i), Some(j)) if j > 0 && i >= 0 => Value::int(i % j),
                            _ => tri!(self.binary_arith(crate::ast::BinaryOp::Mod, a, acc)),
                        };
                    }
                    Op::Exp(x) => acc = tri!(self.binary_arith(crate::ast::BinaryOp::Exp, r!(x), acc)),
                    Op::BitAnd(x) => {
                        let a = r!(x);
                        acc = match (a.as_int(), acc.as_int()) {
                            (Some(i), Some(j)) => Value::int(i & j),
                            _ => tri!(self.binary_arith(crate::ast::BinaryOp::BitAnd, a, acc)),
                        };
                    }
                    Op::BitOr(x) => {
                        let a = r!(x);
                        acc = match (a.as_int(), acc.as_int()) {
                            (Some(i), Some(j)) => Value::int(i | j),
                            _ => tri!(self.binary_arith(crate::ast::BinaryOp::BitOr, a, acc)),
                        };
                    }
                    Op::BitXor(x) => {
                        let a = r!(x);
                        acc = match (a.as_int(), acc.as_int()) {
                            (Some(i), Some(j)) => Value::int(i ^ j),
                            _ => tri!(self.binary_arith(crate::ast::BinaryOp::BitXor, a, acc)),
                        };
                    }
                    Op::Shl(x) => acc = tri!(self.binary_arith(crate::ast::BinaryOp::Shl, r!(x), acc)),
                    Op::Shr(x) => acc = tri!(self.binary_arith(crate::ast::BinaryOp::Shr, r!(x), acc)),
                    Op::Ushr(x) => acc = tri!(self.binary_arith(crate::ast::BinaryOp::Ushr, r!(x), acc)),
                    Op::Eq(x) => acc = Value::bool(tri!(self.loose_equals(r!(x), acc))),
                    Op::Ne(x) => acc = Value::bool(!tri!(self.loose_equals(r!(x), acc))),
                    Op::StrictEq(x) => acc = Value::bool(self.strict_equals(r!(x), acc)),
                    Op::StrictNe(x) => acc = Value::bool(!self.strict_equals(r!(x), acc)),
                    Op::Lt(x) => {
                        let a = r!(x);
                        acc = match (a.as_int(), acc.as_int()) {
                            (Some(i), Some(j)) => Value::bool(i < j),
                            _ => Value::bool(tri!(self.less_than(a, acc, true)) == Some(true)),
                        };
                    }
                    Op::Gt(x) => {
                        let a = r!(x);
                        acc = match (a.as_int(), acc.as_int()) {
                            (Some(i), Some(j)) => Value::bool(i > j),
                            _ => Value::bool(tri!(self.less_than(acc, a, false)) == Some(true)),
                        };
                    }
                    Op::Le(x) => {
                        let a = r!(x);
                        acc = match (a.as_int(), acc.as_int()) {
                            (Some(i), Some(j)) => Value::bool(i <= j),
                            _ => Value::bool(tri!(self.less_than(acc, a, false)) == Some(false)),
                        };
                    }
                    Op::Ge(x) => {
                        let a = r!(x);
                        acc = match (a.as_int(), acc.as_int()) {
                            (Some(i), Some(j)) => Value::bool(i >= j),
                            _ => Value::bool(tri!(self.less_than(a, acc, true)) == Some(false)),
                        };
                    }
                    Op::InstanceOf(x) => acc = Value::bool(tri!(self.instance_of(r!(x), acc))),
                    Op::Inc => {
                        if !acc.is_number() && !acc.is_bigint() {
                            acc = tri!(self.to_numeric(acc));
                        }
                        acc = match acc.as_int() {
                            Some(i) if i < i32::MAX => Value::int(i + 1),
                            _ => {
                                if acc.is_bigint() {
                                    tri!(crate::builtins::bigint::add_int(self, acc, 1))
                                } else {
                                    Value::number(acc.num() + 1.0)
                                }
                            }
                        }
                    }
                    Op::Dec => {
                        if !acc.is_number() && !acc.is_bigint() {
                            acc = tri!(self.to_numeric(acc));
                        }
                        acc = match acc.as_int() {
                            Some(i) if i > i32::MIN => Value::int(i - 1),
                            _ => {
                                if acc.is_bigint() {
                                    tri!(crate::builtins::bigint::add_int(self, acc, -1))
                                } else {
                                    Value::number(acc.num() - 1.0)
                                }
                            }
                        }
                    }
                    Op::Neg => {
                        let n = tri!(self.to_numeric(acc));
                        acc = if n.is_bigint() { tri!(crate::builtins::bigint::negate(self, n)) } else { Value::number(-n.num()) };
                    }
                    Op::BitNot => {
                        let n = tri!(self.to_numeric(acc));
                        acc = if n.is_bigint() { tri!(crate::builtins::bigint::bitnot(self, n)) } else { Value::int(!conv::to_int32_f(n.num())) };
                    }
                    Op::Not => acc = Value::bool(!self.to_boolean(acc)),
                    Op::Typeof => acc = Value::str(self.typeof_value(acc)),
                    Op::ToNumeric => {
                        if !acc.is_number() {
                            acc = tri!(self.to_numeric(acc));
                        }
                    }
                    Op::ToNumber => {
                        if !acc.is_number() {
                            acc = Value::number(tri!(self.to_number(acc)));
                        }
                    }
                    Op::ToPropertyKey => {
                        if !(acc.is_string() || acc.is_symbol() || acc.is_int()) {
                            let k = tri!(self.to_property_key(acc));
                            acc = self.key_to_value(k);
                        }
                    }
                    Op::ToStringOp => {
                        if !acc.is_string() {
                            acc = Value::str(tri!(self.to_string(acc)));
                        }
                    }
                    Op::CheckObject(k) => {
                        if !acc.is_object() {
                            let m = self.display(konst!(k));
                            tri!(Err(self.type_error(&m)));
                        }
                    }
                    Op::RequireObjectCoercible => {
                        if acc.is_nullish() {
                            let m = alloc::format!("Cannot destructure '{}' as it is {}.", self.display(acc), self.display(acc));
                            tri!(Err(self.type_error(&m)));
                        }
                    }
                    // ----- control flow
                    Op::Jmp(t) => {
                        if (t as usize) < pc {
                            safepoint!();
                        }
                        pc = t as usize;
                    }
                    Op::JmpTrue(t) => {
                        if self.to_boolean(acc) {
                            if (t as usize) < pc {
                                safepoint!();
                            }
                            pc = t as usize;
                        }
                    }
                    Op::JmpFalse(t) => {
                        if !self.to_boolean(acc) {
                            pc = t as usize;
                        }
                    }
                    Op::JmpNullish(t) => {
                        if acc.is_nullish() {
                            pc = t as usize;
                        }
                    }
                    Op::JmpNotNullish(t) => {
                        if !acc.is_nullish() {
                            pc = t as usize;
                        }
                    }
                    Op::JmpUndefined(t) => {
                        if acc.is_undefined() {
                            pc = t as usize;
                        }
                    }
                    Op::JmpNotUndefined(t) => {
                        if !acc.is_undefined() {
                            pc = t as usize;
                        }
                    }
                    Op::JmpStrictEq(x, t) => {
                        if self.strict_equals(r!(x), acc) || (acc.is_uninit() && r!(x).is_uninit()) {
                            pc = t as usize;
                        }
                    }
                    Op::JmpIfIntEq(x, i, t) => {
                        if r!(x) == Value::int(i) {
                            pc = t as usize;
                        }
                    }
                    // ----- calls
                    Op::Call(x, argc) | Op::CallEval(x, argc, _) => {
                        safepoint!();
                        self.frames[fi].pc = pc as u32;
                        let callee = r!(x);
                        let this = r!(x + 1);
                        if let Op::CallEval(_, _, k) = op {
                            if callee == Value::object(self.intrinsics.eval) {
                                let arg = if argc > 0 { r!(x + 2) } else { Value::UNDEFINED };
                                acc = tri!(self.direct_eval(fi, arg, k));
                                continue 'ops;
                            }
                        }
                        let abase = base + x as usize + 2;
                        match tri!(self.call_from_frame(callee, this, abase, argc as usize, Value::UNDEFINED)) {
                            Some(v) => acc = v,
                            None => continue 'frame,
                        }
                    }
                    Op::CallSpread(x) => {
                        safepoint!();
                        self.frames[fi].pc = pc as u32;
                        let callee = r!(x);
                        let this = r!(x + 1);
                        let abase = base + x as usize + 2;
                        let argc = self.unpack_spread(abase);
                        match tri!(self.call_from_frame(callee, this, abase, argc, Value::UNDEFINED)) {
                            Some(v) => acc = v,
                            None => continue 'frame,
                        }
                    }
                    Op::New(x, _) | Op::NewSpread(x) => {
                        safepoint!();
                        self.frames[fi].pc = pc as u32;
                        let callee = r!(x);
                        let abase = base + x as usize + 2;
                        let argc = if let Op::New(_, n) = op { n as usize } else { self.unpack_spread(abase) };
                        if !self.is_constructor(callee) {
                            let s = self.display(callee);
                            tri!(Err(self.type_error(&alloc::format!("{s} is not a constructor"))));
                        }
                        match tri!(self.construct_from_frame(callee, abase, argc, callee)) {
                            Some(v) => acc = v,
                            None => continue 'frame,
                        }
                    }
                    Op::SuperCall(x, _) | Op::SuperCallSpread(x) => {
                        safepoint!();
                        self.frames[fi].pc = pc as u32;
                        let f = r!(x).as_object().unwrap();
                        let nt = r!(x + 1);
                        let abase = base + x as usize + 2;
                        let argc = if let Op::SuperCall(_, n) = op { n as usize } else { self.unpack_spread(abase) };
                        let parent = tri!(self.get_prototype_of(f));
                        let pv = parent.map(Value::object).unwrap_or(Value::NULL);
                        if !self.is_constructor(pv) {
                            tri!(Err(self.type_error("Super constructor is not a constructor")));
                        }
                        let args: Vec<Value> = self.stack[abase..abase + argc].to_vec();
                        acc = tri!(self.construct(pv, &args, nt));
                    }
                    Op::SuperCallForward(x) => {
                        self.frames[fi].pc = pc as u32;
                        let f = r!(x).as_object().unwrap();
                        let nt = r!(x + 1);
                        let parent = tri!(self.get_prototype_of(f));
                        let pv = parent.map(Value::object).unwrap_or(Value::NULL);
                        if !self.is_constructor(pv) {
                            tri!(Err(self.type_error("Super constructor is not a constructor")));
                        }
                        let args: Vec<Value> = self.frames[fi].args.as_deref().unwrap_or(&[]).to_vec();
                        acc = tri!(self.construct(pv, &args, nt));
                        self.frames[fi].this = acc;
                    }
                    Op::Return => {
                        match self.do_return(acc, entry) {
                            Ok((v, true)) => return Ok(v),
                            Ok((v, false)) => {
                                acc = v;
                                continue 'frame;
                            }
                            Err(t) => {
                                error = t;
                                break 'ops;
                            }
                        }
                    }
                    Op::Throw => {
                        self.frames[fi].pc = pc as u32;
                        error = Throw::Value(acc);
                        break 'ops;
                    }
                    Op::ThrowError(kind, k) => {
                        let m = self.display(konst!(k));
                        let e = match kind {
                            0 => self.type_error(&m),
                            1 => self.reference_error(&m),
                            2 => self.syntax_error(&m),
                            _ => self.range_error(&m),
                        };
                        tri!(Err(e));
                    }
                    // ----- iteration
                    Op::GetIterator(x) => {
                        self.frames[fi].pc = pc as u32;
                        let (it, next) = tri!(self.get_iterator(acc));
                        r!(x) = it;
                        r!(x + 1) = next;
                        r!(x + 2) = Value::FALSE;
                    }
                    Op::GetAsyncIterator(x) => {
                        self.frames[fi].pc = pc as u32;
                        let (it, next) = tri!(crate::builtins::iterators::get_async_iterator(self, acc));
                        r!(x) = it;
                        r!(x + 1) = next;
                        r!(x + 2) = Value::FALSE;
                    }
                    Op::IteratorNextRaw(x) => {
                        self.frames[fi].pc = pc as u32;
                        let (it, next) = (r!(x), r!(x + 1));
                        acc = tri!(self.call(next, it, &[]));
                    }
                    Op::IterResultDone => {
                        let o = acc.as_object().unwrap();
                        let k = Key::Str(self.names.done);
                        let d = tri!(self.get(o, k));
                        acc = Value::bool(self.to_boolean(d));
                    }
                    Op::IterResultValue => {
                        let o = acc.as_object().unwrap();
                        let k = Key::Str(self.names.value);
                        acc = tri!(self.get(o, k));
                    }
                    Op::IteratorStep(x, t) => {
                        if r!(x + 2) == Value::TRUE {
                            pc = t as usize;
                            continue 'ops;
                        }
                        self.frames[fi].pc = pc as u32;
                        let (it, next) = (r!(x), r!(x + 1));
                        match self.iterator_step(it, next) {
                            Ok(Some(v)) => acc = v,
                            Ok(None) => {
                                self.stack[base + x as usize + 2] = Value::TRUE;
                                pc = t as usize;
                            }
                            Err(e) => {
                                self.stack[base + x as usize + 2] = Value::TRUE;
                                tri!(Err(e));
                            }
                        }
                    }
                    Op::IteratorClose(x) => {
                        if r!(x + 2) != Value::TRUE {
                            r!(x + 2) = Value::TRUE;
                            self.frames[fi].pc = pc as u32;
                            let it = r!(x);
                            let saved = acc;
                            tri!(self.iterator_close(it));
                            acc = saved;
                        }
                    }
                    Op::IteratorCloseThrow(x, _) => {
                        if r!(x + 2) != Value::TRUE {
                            r!(x + 2) = Value::TRUE;
                            self.frames[fi].pc = pc as u32;
                            let it = r!(x);
                            if let Err(Throw::Terminate) = self.iterator_close(it) {
                                tri!(Err(Throw::Terminate));
                            }
                        }
                    }
                    Op::IteratorRest(x) => {
                        self.frames[fi].pc = pc as u32;
                        let mut items = Vec::new();
                        if r!(x + 2) != Value::TRUE {
                            let (it, next) = (r!(x), r!(x + 1));
                            loop {
                                match self.iterator_step(it, next) {
                                    Ok(Some(v)) => {
                                        self.native_roots.push(v);
                                        items.push(v);
                                    }
                                    Ok(None) => break,
                                    Err(e) => {
                                        self.stack[base + x as usize + 2] = Value::TRUE;
                                        tri!(Err(e));
                                    }
                                }
                            }
                            let n = self.native_roots.len() - items.len();
                            self.native_roots.truncate(n);
                            self.stack[base + x as usize + 2] = Value::TRUE;
                        }
                        acc = Value::object(self.new_array(items));
                    }
                    Op::CreateIterResult(x) => {
                        let done = r!(x) == Value::TRUE;
                        acc = self.iter_result(acc, done);
                    }
                    Op::ForInPrepare(x) => {
                        self.frames[fi].pc = pc as u32;
                        r!(x) = tri!(self.for_in_prepare(acc));
                    }
                    Op::ForInNext(x, t) => {
                        self.frames[fi].pc = pc as u32;
                        match tri!(self.for_in_next(r!(x))) {
                            Some(k) => acc = k,
                            None => pc = t as usize,
                        }
                    }
                    // ----- generators and async
                    Op::GeneratorStart => {
                        self.frames[fi].pc = pc as u32;
                        let g = tri!(self.generator_start(fi));
                        match self.leave_frame(g, entry) {
                            Some(v) => return Ok(v),
                            None => {
                                acc = g;
                                continue 'frame;
                            }
                        }
                    }
                    Op::Yield(_) | Op::YieldRaw(_) | Op::AsyncGenYield(_) => {
                        self.frames[fi].pc = pc as u32;
                        let sig = if let Op::YieldRaw(_) = op { GenSignal::YieldRaw } else { GenSignal::Yield };
                        let v = self.suspend(fi, acc, sig);
                        match self.leave_frame(v, entry) {
                            Some(v) => return Ok(v),
                            None => {
                                acc = v;
                                continue 'frame;
                            }
                        }
                    }
                    Op::Await(_) => {
                        self.frames[fi].pc = pc as u32;
                        let g = self.frames[fi].generator.unwrap();
                        tri!(crate::builtins::promise::await_value(self, g, acc));
                        let v = self.suspend(fi, Value::UNDEFINED, GenSignal::Await);
                        match self.leave_frame(v, entry) {
                            Some(v) => return Ok(v),
                            None => {
                                acc = v;
                                continue 'frame;
                            }
                        }
                    }
                    // ----- misc
                    Op::Debugger => {
                        self.frames[fi].pc = pc as u32;
                        tri!(self.debugger_statement());
                    }
                    Op::CreateArguments(k) => {
                        let map = if k == u32::MAX {
                            None
                        } else if let Const::Names(m) = &body.consts[k as usize] {
                            Some(m.clone())
                        } else {
                            None
                        };
                        acc = self.create_arguments(fi, map);
                    }
                    Op::CreateRest(from) => {
                        let args: Vec<Value> = self.frames[fi].args.as_deref().unwrap_or(&[]).iter().skip(from as usize).copied().collect();
                        acc = Value::object(self.new_array(args));
                    }
                    Op::ImportCall(o) => {
                        self.frames[fi].pc = pc as u32;
                        let opts = r!(o);
                        acc = crate::module::import_call(self, fi, acc, opts);
                    }
                    Op::ImportMeta => {
                        acc = tri!(crate::module::import_meta(self, fi));
                    }
                }
            }
            // An exception: find a handler.
            if let (Throw::Value(v), true) = (&error, self.debugger.is_some()) {
                let v = *v;
                if let Err(t) = self.debug_on_throw(v) {
                    error = t;
                }
            }
            match self.unwind(error, entry) {
                Ok(Some(v)) => {
                    // Exception caught by an async frame that ends here.
                    return Ok(v);
                }
                Ok(None) => {
                    acc = Value::UNDEFINED;
                    continue 'frame;
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// The generic path of a fused binary instruction.
    /// `x + 1` / `x - 1` for values that are not small ints.
    #[inline(never)]
    fn inc_dec_slow(&mut self, v: Value, up: bool) -> JsResult<Value> {
        let n = if v.is_number() || v.is_bigint() { v } else { self.to_numeric(v)? };
        if n.is_bigint() {
            crate::builtins::bigint::add_int(self, n, if up { 1 } else { -1 })
        } else {
            Ok(Value::number(n.num() + if up { 1.0 } else { -1.0 }))
        }
    }

    #[inline(never)]
    fn bin_generic(&mut self, kind: BinKind, a: Value, b: Value) -> JsResult<Value> {
        use crate::ast::BinaryOp as B;
        Ok(match kind {
            BinKind::Add => self.op_add(a, b)?,
            BinKind::Sub => self.binary_arith(B::Sub, a, b)?,
            BinKind::Mul => self.binary_arith(B::Mul, a, b)?,
            BinKind::BitAnd => self.binary_arith(B::BitAnd, a, b)?,
            BinKind::BitOr => self.binary_arith(B::BitOr, a, b)?,
            BinKind::BitXor => self.binary_arith(B::BitXor, a, b)?,
            BinKind::Lt => Value::bool(self.less_than(a, b, true)? == Some(true)),
            BinKind::Gt => Value::bool(self.less_than(b, a, false)? == Some(true)),
            BinKind::Le => Value::bool(self.less_than(b, a, false)? == Some(false)),
            BinKind::Ge => Value::bool(self.less_than(a, b, true)? == Some(false)),
            BinKind::StrictEq => Value::bool(self.strict_equals(a, b)),
            BinKind::StrictNe => Value::bool(!self.strict_equals(a, b)),
        })
    }

    // ----- calls inside the interpreter -------------------------------

    /// Calls from the interpreter: a closure gets a new frame (returns
    /// None, the loop continues in it); natives run now.
    fn call_from_frame(&mut self, callee: Value, this: Value, abase: usize, argc: usize, nt: Value) -> JsResult<Option<Value>> {
        let _ = nt;
        let Some(fo) = callee.as_object() else {
            let s = self.display(callee);
            return Err(self.type_error(&alloc::format!("{s} is not a function")));
        };
        match &self.obj(fo).class {
            Class::Function(c) => {
                if matches!(c.code.kind, FuncKind::ClassConstructor | FuncKind::DerivedConstructor) {
                    return Err(self.type_error("Class constructor cannot be invoked without 'new'"));
                }
                let (code, env) = (c.code.clone(), c.env);
                self.push_frame(fo, code, env, this, Value::UNDEFINED, abase, argc, FrameKind::Normal)?;
                Ok(None)
            }
            Class::Native(n) => {
                let f = n.f;
                // The arguments are copied out of the stack (the native gets
                // `&mut Vm`) into a reused buffer.
                let mut args = self.take_args_vec();
                args.extend_from_slice(&self.stack[abase..abase + argc]);
                let saved_sp = self.sp;
                self.sp = abase + argc;
                let r = self.native_call(f, callee, this, &args, Value::UNDEFINED);
                self.sp = saved_sp;
                self.give_args_vec(args);
                r.map(Some)
            }
            _ => {
                let args: Vec<Value> = self.stack[abase..abase + argc].to_vec();
                let saved_sp = self.sp;
                self.sp = abase + argc;
                let r = self.call(callee, this, &args);
                self.sp = saved_sp;
                r.map(Some)
            }
        }
    }

    fn construct_from_frame(&mut self, callee: Value, abase: usize, argc: usize, nt: Value) -> JsResult<Option<Value>> {
        let fo = callee.as_object().unwrap();
        if let Class::Function(c) = &self.obj(fo).class {
            let (code, env) = (c.code.clone(), c.env);
            let this = if code.kind == FuncKind::DerivedConstructor {
                Value::UNINIT
            } else {
                let saved_sp = self.sp;
                self.sp = abase + argc;
                let p = self.proto_from_ctor(nt, |i| i.object_prototype);
                self.sp = saved_sp;
                Value::object(self.alloc_object(Some(p?), Class::Ordinary))
            };
            self.push_frame(fo, code, env, this, nt, abase, argc, FrameKind::Construct)?;
            return Ok(None);
        }
        let args: Vec<Value> = self.stack[abase..abase + argc].to_vec();
        let saved_sp = self.sp;
        self.sp = abase + argc;
        let r = self.construct(callee, &args, nt);
        self.sp = saved_sp;
        r.map(Some)
    }

    /// Replaces the spread array at `abase` by its elements; returns argc.
    fn unpack_spread(&mut self, abase: usize) -> usize {
        let a = self.stack[abase].as_object().unwrap();
        let items: Vec<Value> = self.obj(a).elements.clone();
        let n = items.len();
        if self.stack.len() < abase + n + 1 {
            self.stack.resize(abase + n + 16, Value::UNDEFINED);
        }
        for (i, v) in items.into_iter().enumerate() {
            self.stack[abase + i] = if v.is_hole() { Value::UNDEFINED } else { v };
        }
        if self.sp < abase + n {
            self.sp = abase + n;
        }
        n
    }

    /// Pops the current frame for a `return`; returns (value, done) where
    /// done means `run` must return.
    fn do_return(&mut self, mut v: Value, entry: usize) -> JsResult<(Value, bool)> {
        let f = self.frames.pop().unwrap();
        match f.kind {
            FrameKind::Construct | FrameKind::ConstructEntry => {
                if f.code.kind != FuncKind::DerivedConstructor && !v.is_object() {
                    v = f.this;
                }
            }
            _ => {}
        }
        if let Some(g) = f.generator {
            v = self.finish_generator_frame(g, &f, Ok(v))?;
        }
        self.restore_sp();
        let done = self.frames.len() <= entry;
        Ok((v, done))
    }

    fn restore_sp(&mut self) {
        self.sp = match self.frames.last() {
            Some(f) => f.base as usize + f.code.nregs as usize,
            None => 0,
        };
    }

    /// Leaves the current (suspended) frame with `v` as its result.
    fn leave_frame(&mut self, v: Value, entry: usize) -> Option<Value> {
        self.frames.pop();
        self.restore_sp();
        if self.frames.len() <= entry { Some(v) } else { None }
    }

    /// Unwinds to a handler. Ok(None): continue in the frame with the
    /// handler; Ok(Some(v)): `run` returns v (an async function caught it);
    /// Err: propagate out of `run`.
    fn unwind(&mut self, mut error: Throw, entry: usize) -> JsResult<Option<Value>> {
        loop {
            let fi = self.frames.len() - 1;
            if let Throw::Value(_) = error {
                let f = &self.frames[fi];
                let pc = f.pc.saturating_sub(1);
                let found = f.code.body().handlers.iter().find(|h| h.start <= pc && pc < h.end).copied();
                if let Some(h) = found {
                    while self.frames[fi].env_depth > h.env_depth {
                        let e = self.frames[fi].env.unwrap();
                        self.frames[fi].env = self.envs.get(e.0).parent;
                        self.frames[fi].env_depth -= 1;
                    }
                    let Throw::Value(v) = error else { unreachable!() };
                    let base = self.frames[fi].base as usize;
                    self.stack[base + h.reg as usize] = v;
                    self.frames[fi].pc = h.target;
                    self.sp = base + self.frames[fi].code.nregs as usize;
                    return Ok(None);
                }
            }
            let f = self.frames.pop().unwrap();
            if let Some(g) = f.generator {
                match self.finish_generator_frame(g, &f, Err(error)) {
                    Ok(v) => {
                        // An async function turned the error into a
                        // rejected promise: return normally.
                        self.restore_sp();
                        if self.frames.len() <= entry {
                            return Ok(Some(v));
                        }
                        // Continue in the caller with v as the call result:
                        // place it as acc by resuming at the caller's pc.
                        self.pending_acc = Some(v);
                        return Ok(None);
                    }
                    Err(e) => error = e,
                }
            }
            self.restore_sp();
            if self.frames.len() <= entry {
                return Err(error);
            }
        }
    }

    // ----- safepoints -----------------------------------------------------

    fn safepoint(&mut self, _acc: Value) -> JsResult<()> {
        self.ticks = TICKS;
        self.check_limits()?;
        if self.alloc_bytes > self.gc_threshold {
            self.collect_garbage();
            if self.heap_estimate() > self.limits.max_heap_bytes {
                return Err(self.range_error("Out of memory: the heap limit was reached"));
            }
        }
        Ok(())
    }

    /// Counts one tick where native code drives JS (a call from Rust, an
    /// iterator step): a loop in a native over a JS iterator or callback
    /// must stop at the budget, an interrupt or the heap limit like a JS
    /// loop. No collection here (natives hold unrooted values).
    #[inline]
    pub(crate) fn poll_limits(&mut self) -> JsResult<()> {
        self.ticks -= 1;
        if self.ticks <= 0 {
            self.ticks = TICKS;
            self.check_limits()?;
        }
        self.check_allocation()
    }

    /// The budget, interrupt and debugger part of a safepoint.
    fn check_limits(&mut self) -> JsResult<()> {
        if self.debugger.is_some() {
            self.debug_safepoint();
        }
        if self.interrupt.get() {
            self.interrupt.set(false);
            return Err(Throw::Terminate);
        }
        if self.host.as_mut().is_some_and(|h| h.interrupt_requested()) {
            return Err(Throw::Terminate);
        }
        if let Some(b) = self.budget.as_mut() {
            if *b <= TICKS as u64 {
                *b = 0;
                return Err(Throw::Terminate);
            }
            *b -= TICKS as u64;
        }
        Ok(())
    }

    fn debugger_statement(&mut self) -> JsResult<()> {
        crate::debug::on_debugger_statement(self)
    }

    // ----- helpers used by ops ----------------------------------------------

    pub fn tdz_error(&mut self, name: Value) -> Throw {
        let n = self.display(name);
        if n == "this" {
            return self.reference_error("Must call super constructor in derived class before accessing 'this' or returning from derived constructor");
        }
        self.reference_error(&alloc::format!("Cannot access '{n}' before initialization"))
    }

    /// GetValue on a property reference (base may be a primitive).
    pub fn get_value_prop(&mut self, base: Value, key: Key) -> JsResult<Value> {
        if let Some(o) = base.as_object() {
            return self.get_with(o, key, base);
        }
        if base.is_nullish() {
            let k = self.key_display(key);
            let b = self.display(base);
            return Err(self.type_error(&alloc::format!("Cannot read properties of {b} (reading '{k}')")));
        }
        self.get_v(base, key)
    }

    pub fn get_keyed(&mut self, base: Value, key: Value) -> JsResult<Value> {
        if let (Some(o), Some(i)) = (base.as_object(), key.as_int()) {
            if i >= 0 {
                let ob = self.obj(o);
                if let Class::Array | Class::Ordinary = ob.class {
                    if let Some(&v) = ob.elements.get(i as usize) {
                        if !v.is_hole() {
                            return Ok(v);
                        }
                    }
                }
            }
        }
        if base.is_nullish() {
            let b = self.display(base);
            let k = self.display(key);
            return Err(self.type_error(&alloc::format!("Cannot read properties of {b} (reading '{k}')")));
        }
        let k = self.to_property_key(key)?;
        self.get_value_prop(base, k)
    }

    /// PutValue on a property reference.
    pub fn put_value_prop(&mut self, base: Value, key: Key, v: Value, strict: bool) -> JsResult<()> {
        let ok = match base.as_object() {
            Some(o) => self.set_with(o, key, v, base)?,
            None => {
                if base.is_nullish() {
                    let k = self.key_display(key);
                    let b = self.display(base);
                    return Err(self.type_error(&alloc::format!("Cannot set properties of {b} (setting '{k}')")));
                }
                let p = self.primitive_proto(base)?;
                self.set_with(p, key, v, base)?
            }
        };
        if !ok && strict {
            return Err(self.cannot_assign(key));
        }
        Ok(())
    }

    pub fn set_keyed(&mut self, base: Value, key: Value, v: Value, strict: bool) -> JsResult<()> {
        if let (Some(o), Some(i)) = (base.as_object(), key.as_int()) {
            if i >= 0 {
                let ob = self.obj_mut(o);
                if let Class::Array | Class::Ordinary = ob.class {
                    if ob.frozen_elements == 0 {
                        if let Some(e) = ob.elements.get_mut(i as usize) {
                            if !e.is_hole() {
                                *e = v;
                                return Ok(());
                            }
                        }
                    }
                }
            }
        }
        if base.is_nullish() {
            let b = self.display(base);
            let k = self.display(key);
            return Err(self.type_error(&alloc::format!("Cannot set properties of {b} (setting '{k}')")));
        }
        let k = self.to_property_key(key)?;
        self.put_value_prop(base, k, v, strict)
    }

    fn delete_prop(&mut self, base: Value, key: Key, strict: bool) -> JsResult<bool> {
        let o = self.to_object(base)?;
        let ok = self.delete(o, key)?;
        if !ok && strict {
            let k = self.key_display(key);
            return Err(self.type_error(&alloc::format!("Cannot delete property '{k}'")));
        }
        Ok(ok)
    }

    /// The `[[HomeObject]]` of the running function (for arrows created here).
    fn callee_home(&self, fi: usize) -> Option<ObjId> {
        let c = self.frames[fi].callee.as_object()?;
        match &self.obj(c).class {
            Class::Function(cl) => cl.home,
            _ => None,
        }
    }

    fn super_base(&mut self, fi: usize) -> JsResult<Option<ObjId>> {
        let Some(home) = self.callee_home(fi) else {
            return Err(self.syntax_error("'super' keyword unexpected here"));
        };
        self.get_prototype_of(home)
    }

    fn super_get(&mut self, fi: usize, this: Value, key: Key) -> JsResult<Value> {
        match self.super_base(fi)? {
            Some(p) => self.get_with(p, key, this),
            None => {
                let k = self.key_display(key);
                Err(self.type_error(&alloc::format!("Cannot read properties of null (reading '{k}')")))
            }
        }
    }

    fn super_set(&mut self, fi: usize, this: Value, key: Key, v: Value, strict: bool) -> JsResult<()> {
        let Some(p) = self.super_base(fi)? else {
            return Err(self.type_error("Cannot set properties of null"));
        };
        let ok = self.set_with(p, key, v, this)?;
        if !ok && strict {
            return Err(self.cannot_assign(key));
        }
        Ok(())
    }

    /// Creates a function object for code.
    pub fn make_closure(&mut self, code: Rc<Code>, env: Option<EnvId>, home: Option<ObjId>) -> ObjId {
        let proto = match (code.is_async, code.is_generator) {
            (false, false) => self.intrinsics.function_prototype,
            (false, true) => self.intrinsics.generator_function_prototype,
            (true, false) => self.intrinsics.async_function_prototype,
            (true, true) => self.intrinsics.async_generator_function_prototype,
        };
        let kind = code.kind;
        let (is_async, is_gen) = (code.is_async, code.is_generator);
        let length = code.length;
        let name = code.name;
        let f = self.alloc_object(Some(proto), Class::Function(Box::new(Closure { code, env, home, fields: None, private_methods: None })));
        let kl = Key::Str(self.names.length);
        self.define_data(f, kl, Value::int(length as i32), attr::CONFIGURABLE);
        let kn = Key::Str(self.names.name);
        self.define_data(f, kn, name, attr::CONFIGURABLE);
        if is_gen {
            let pp = if is_async { self.intrinsics.async_generator_prototype } else { self.intrinsics.generator_prototype };
            let p = self.alloc_object(Some(pp), Class::Ordinary);
            let kp = Key::Str(self.names.prototype);
            self.define_data(f, kp, Value::object(p), attr::WRITABLE);
        } else if kind == FuncKind::Normal && !is_async {
            let p = self.new_object();
            let kc = Key::Str(self.names.constructor);
            self.define_data(p, kc, Value::object(f), attr::HIDDEN);
            let kp = Key::Str(self.names.prototype);
            self.define_data(f, kp, Value::object(p), attr::WRITABLE);
        }
        f
    }

    /// SetFunctionName from a property key value.
    pub fn set_function_name(&mut self, f: ObjId, key: Value, prefix: u8) -> JsResult<()> {
        let base = if let Some(s) = key.as_symbol() {
            match self.symbols.get(s.0).description {
                Some(d) => {
                    if self.symbols.get(s.0).private {
                        Value::str(d)
                    } else {
                        let ds = self.str(d).to_rust_lossy();
                        self.new_string(&alloc::format!("[{ds}]"))
                    }
                }
                None => Value::str(self.names.empty),
            }
        } else {
            Value::str(self.to_string(key)?)
        };
        let name = match prefix {
            1 | 2 => {
                let b = self.display(base);
                let p = if prefix == 1 { "get" } else { "set" };
                self.new_string(&alloc::format!("{p} {b}"))
            }
            _ => base,
        };
        let kn = Key::Str(self.names.name);
        // Classes with a static `name` member keep it.
        if let Some(crate::vm::props::Slot::Data(..) | crate::vm::props::Slot::Accessor(..)) = self.raw_get_own(f, kn) {
            if let Class::Function(c) = &self.obj(f).class {
                if matches!(c.code.kind, FuncKind::ClassConstructor | FuncKind::DerivedConstructor) {
                    let cur = self.raw_get_own(f, kn);
                    if let Some(crate::vm::props::Slot::Data(v, _)) = cur {
                        if !(v.as_string().is_some_and(|s| self.str(s).is_empty())) {
                            return Ok(());
                        }
                    } else {
                        return Ok(());
                    }
                }
            }
        }
        self.define_data(f, kn, name, attr::CONFIGURABLE);
        Ok(())
    }

    fn define_accessor_half(&mut self, o: ObjId, k: Key, f: Value, is_get: bool, enumerable: bool) -> JsResult<()> {
        let mut d = PropDesc { configurable: Some(true), enumerable: Some(enumerable), ..Default::default() };
        if is_get {
            d.get = Some(f);
        } else {
            d.set = Some(f);
        }
        self.define_property_or_throw(o, k, d)
    }

    /// CopyDataProperties(target, source, excluded).
    pub fn copy_data_properties(&mut self, target: ObjId, source: Value, excluded: &[Key]) -> JsResult<()> {
        if source.is_nullish() {
            return Ok(());
        }
        let from = self.to_object(source)?;
        self.native_roots.push(Value::object(from));
        let keys = self.own_keys(from)?;
        for k in keys {
            if excluded.contains(&k) {
                continue;
            }
            if let Some(d) = self.get_own_property(from, k)? {
                if d.enumerable == Some(true) {
                    let v = self.get(from, k)?;
                    self.create_data_property_or_throw(target, k, v)?;
                }
            }
        }
        self.native_roots.pop();
        Ok(())
    }

    fn spread_into(&mut self, arr: ObjId, iterable: Value) -> JsResult<()> {
        let (it, next) = self.get_iterator(iterable)?;
        self.native_roots.push(it);
        self.native_roots.push(next);
        while let Some(v) = self.iterator_step(it, next)? {
            let ob = self.obj_mut(arr);
            ob.elements.push(v);
            ob.length += 1;
        }
        self.native_roots.truncate(self.native_roots.len() - 2);
        Ok(())
    }

    fn template_object(&mut self, code: &Rc<Code>, i: u32) -> Value {
        let key = (Rc::as_ptr(code) as usize, i);
        if let Some(&v) = self.templates.get(&key) {
            return v;
        }
        let Const::Template(items) = &code.body().consts[i as usize] else { unreachable!() };
        let items = items.clone();
        let cooked: Vec<Value> = items.iter().map(|x| x.0).collect();
        let raw: Vec<Value> = items.iter().map(|x| x.1).collect();
        let r = self.new_array(raw);
        let c = self.new_array(cooked);
        let kr = self.key("raw");
        self.define_data(c, kr, Value::object(r), attr::NONE);
        let _ = self.set_integrity(r, true);
        let _ = self.set_integrity(c, true);
        let v = Value::object(c);
        self.templates.insert(key, v);
        v
    }

    fn create_class(&mut self, code: Rc<Code>, env: Option<EnvId>, superclass: Value, name: Value) -> JsResult<(ObjId, ObjId)> {
        let (proto_parent, ctor_parent) = if superclass.is_hole() {
            (Some(self.intrinsics.object_prototype), self.intrinsics.function_prototype)
        } else if superclass.is_null() {
            (None, self.intrinsics.function_prototype)
        } else {
            if !self.is_constructor(superclass) {
                return Err(self.type_error("Class extends value is not a constructor or null"));
            }
            let so = superclass.as_object().unwrap();
            let k = Key::Str(self.names.prototype);
            let pp = self.get(so, k)?;
            if !(pp.is_object() || pp.is_null()) {
                return Err(self.type_error("Class extends value does not have valid prototype property"));
            }
            (pp.as_object(), so)
        };
        let proto = self.alloc_object(proto_parent, Class::Ordinary);
        let length = code.length;
        let f = self.alloc_object(Some(ctor_parent), Class::Function(Box::new(Closure { code, env, home: Some(proto), fields: None, private_methods: None })));
        let kl = Key::Str(self.names.length);
        self.define_data(f, kl, Value::int(length as i32), attr::CONFIGURABLE);
        let kn = Key::Str(self.names.name);
        self.define_data(f, kn, name, attr::CONFIGURABLE);
        let kp = Key::Str(self.names.prototype);
        self.define_data(f, kp, Value::object(proto), attr::NONE);
        let kc = Key::Str(self.names.constructor);
        self.define_data(proto, kc, Value::object(f), attr::HIDDEN);
        Ok((f, proto))
    }

    fn add_private_method(&mut self, ctor: ObjId, key: Value, f: Value, kind: u8, is_static: bool) -> JsResult<()> {
        let fo = f.as_object().unwrap();
        let home = if is_static {
            Some(ctor)
        } else {
            let kp = Key::Str(self.names.prototype);
            match self.raw_get_own(ctor, kp) {
                Some(crate::vm::props::Slot::Data(p, _)) => p.as_object(),
                _ => None,
            }
        };
        if let Class::Function(c) = &mut self.obj_mut(fo).class {
            c.home = home;
        }
        if is_static {
            let sym = key.as_symbol().unwrap();
            let k = Key::Sym(sym);
            match kind {
                0 => self.define_data(ctor, k, f, attr::NONE),
                _ => {
                    let (g, s) = match self.raw_get_own(ctor, k) {
                        Some(crate::vm::props::Slot::Accessor(g, s, _)) => (g, s),
                        _ => (Value::UNDEFINED, Value::UNDEFINED),
                    };
                    let (g, s) = if kind == 1 { (f, s) } else { (g, f) };
                    self.define_accessor(ctor, k, g, s, attr::NONE);
                }
            }
            return Ok(());
        }
        if let Class::Function(c) = &mut self.obj_mut(ctor).class {
            let mut list: Vec<(Value, Value, u8)> = c.private_methods.as_deref().cloned().unwrap_or_default();
            list.push((key, f, kind));
            c.private_methods = Some(Rc::new(list));
        }
        Ok(())
    }

    /// Installs private methods and runs the field initializer of class
    /// constructor `f` on `this`.
    fn init_fields(&mut self, f: Value, this: Value) -> JsResult<()> {
        let Some(fo) = f.as_object() else { return Ok(()) };
        let (fields, methods) = match &self.obj(fo).class {
            Class::Function(c) => (c.fields, c.private_methods.clone()),
            _ => return Ok(()),
        };
        let o = this.as_object().unwrap();
        if let Some(ms) = methods {
            // Brand: every private method (accessor pairs merged).
            let mut i = 0;
            while i < ms.len() {
                let (k, m, kind) = ms[i];
                let sym = k.as_symbol().unwrap();
                if self.raw_get_own(o, Key::Sym(sym)).is_some() && (kind == 0 || !ms[..i].iter().any(|x| x.0 == k)) {
                    return Err(self.type_error("Cannot initialize private methods of class twice on the same object"));
                }
                match kind {
                    0 => self.raw_put(o, Key::Sym(sym), crate::vm::props::Slot::Data(m, attr::NONE)),
                    _ => {
                        let (g, s) = match self.raw_get_own(o, Key::Sym(sym)) {
                            Some(crate::vm::props::Slot::Accessor(g, s, _)) => (g, s),
                            _ => (Value::UNDEFINED, Value::UNDEFINED),
                        };
                        let (g, s) = if kind == 1 { (m, s) } else { (g, m) };
                        self.raw_put(o, Key::Sym(sym), crate::vm::props::Slot::Accessor(g, s, attr::NONE));
                    }
                }
                i += 1;
            }
        }
        if let Some(init) = fields {
            self.call(init, this, &[])?;
        }
        Ok(())
    }

    fn private_lookup(&mut self, obj: Value, key: Value) -> JsResult<(ObjId, crate::vm::props::Slot)> {
        let sym = key.as_symbol().unwrap();
        let Some(o) = obj.as_object() else {
            return Err(self.type_error("Cannot read private member from a non-object"));
        };
        match self.raw_get_own(o, Key::Sym(sym)) {
            Some(s) => Ok((o, s)),
            None => {
                let d = self.symbols.get(sym.0).description.map(|d| self.str(d).to_rust_lossy()).unwrap_or_default();
                Err(self.type_error(&alloc::format!("Cannot read private member {d} from an object whose class did not declare it")))
            }
        }
    }

    fn private_get(&mut self, obj: Value, key: Value) -> JsResult<Value> {
        match self.private_lookup(obj, key)? {
            (_, crate::vm::props::Slot::Data(v, _)) => Ok(v),
            (_, crate::vm::props::Slot::Accessor(g, _, _)) => {
                if g.is_undefined() {
                    return Err(self.type_error("'#x' was defined without a getter"));
                }
                self.call(g, obj, &[])
            }
        }
    }

    fn private_set(&mut self, obj: Value, key: Value, v: Value) -> JsResult<()> {
        match self.private_lookup(obj, key)? {
            (o, crate::vm::props::Slot::Data(_, a)) => {
                if a & attr::WRITABLE == 0 {
                    return Err(self.type_error("Private method is not writable"));
                }
                let sym = key.as_symbol().unwrap();
                self.raw_put(o, Key::Sym(sym), crate::vm::props::Slot::Data(v, a));
                Ok(())
            }
            (_, crate::vm::props::Slot::Accessor(_, s, _)) => {
                if s.is_undefined() {
                    return Err(self.type_error("'#x' was defined without a setter"));
                }
                self.call(s, obj, &[v])?;
                Ok(())
            }
        }
    }

    fn private_define(&mut self, obj: Value, key: Value, v: Value) -> JsResult<()> {
        let sym = key.as_symbol().unwrap();
        let o = obj.as_object().unwrap();
        if self.raw_get_own(o, Key::Sym(sym)).is_some() {
            return Err(self.type_error("Cannot initialize private field twice on the same object"));
        }
        if let Class::Proxy { .. } = self.obj(o).class {
            // Private fields on proxies are stored on the proxy itself.
        }
        self.raw_put(o, Key::Sym(sym), crate::vm::props::Slot::Data(v, attr::WRITABLE));
        Ok(())
    }

    fn create_arguments(&mut self, fi: usize, map: Option<Rc<[Value]>>) -> Value {
        let args: Vec<Value> = self.frames[fi].args.as_deref().unwrap_or(&[]).to_vec();
        let n = args.len();
        let callee = self.frames[fi].callee;
        let p = self.intrinsics.object_prototype;
        let mapped = map.map(|m| {
            let env = match self.frames[fi].env {
                Some(e) => e,
                // No parameters live in an environment: nothing is mapped.
                None => self.new_env(0, Value::UNDEFINED, None),
            };
            let mut slots = Vec::new();
            for i in 0..n {
                let s = m.get(i).and_then(|v| v.as_int()).unwrap_or(-1);
                slots.push(if s < 0 { u16::MAX } else { s as u16 });
            }
            Box::new(MappedArgs { env, map: slots })
        });
        let is_mapped = mapped.is_some();
        let o = self.alloc_object(Some(p), Class::Arguments(mapped));
        for (i, v) in args.into_iter().enumerate() {
            self.define_data(o, Key::Index(i as u32), v, attr::DEFAULT);
        }
        let kl = Key::Str(self.names.length);
        self.define_data(o, kl, Value::from_usize(n), attr::HIDDEN);
        let ki = Key::Sym(self.intrinsics.sym_iterator);
        let values = self.intrinsics.array_values;
        self.define_data(o, ki, Value::object(values), attr::HIDDEN);
        let kc = Key::Str(self.names.callee);
        if is_mapped {
            self.define_data(o, kc, callee, attr::HIDDEN);
        } else {
            let t = Value::object(self.intrinsics.throw_type_error);
            self.define_accessor(o, kc, t, t, attr::NONE);
        }
        Value::object(o)
    }

    // ----- iterators ----------------------------------------------------

    /// GetIterator(obj, sync): (iterator, next method).
    pub fn get_iterator(&mut self, v: Value) -> JsResult<(Value, Value)> {
        let k = Key::Sym(self.intrinsics.sym_iterator);
        let m = self.get_v(v, k)?;
        if !self.is_callable(m) {
            let s = self.display(v);
            return Err(self.type_error(&alloc::format!("{s} is not iterable")));
        }
        let it = self.call(m, v, &[])?;
        if !it.is_object() {
            return Err(self.type_error("Result of the Symbol.iterator method is not an object"));
        }
        let kn = Key::Str(self.names.next);
        let next = self.get_v(it, kn)?;
        Ok((it, next))
    }

    /// IteratorStepValue: Some(value) or None when done.
    pub fn iterator_step(&mut self, it: Value, next: Value) -> JsResult<Option<Value>> {
        // Callers usually collect the values: count them, and the step.
        self.alloc_bytes += 16;
        self.poll_limits()?;
        let r = self.call(next, it, &[])?;
        let Some(ro) = r.as_object() else {
            return Err(self.type_error("Iterator result is not an object"));
        };
        let kd = Key::Str(self.names.done);
        let d = self.get(ro, kd)?;
        if self.to_boolean(d) {
            return Ok(None);
        }
        let kv = Key::Str(self.names.value);
        Ok(Some(self.get(ro, kv)?))
    }

    /// IteratorClose with a normal completion.
    pub fn iterator_close(&mut self, it: Value) -> JsResult<()> {
        let kr = Key::Str(self.names.return_);
        let m = self.get_method(it, kr)?;
        if let Some(m) = m {
            let r = self.call(m, it, &[])?;
            if !r.is_object() {
                return Err(self.type_error("Iterator result is not an object"));
            }
        }
        Ok(())
    }

    pub fn iter_result(&mut self, value: Value, done: bool) -> Value {
        let o = self.new_object();
        let kv = Key::Str(self.names.value);
        self.define_data(o, kv, value, attr::DEFAULT);
        let kd = Key::Str(self.names.done);
        self.define_data(o, kd, Value::bool(done), attr::DEFAULT);
        Value::object(o)
    }

    fn for_in_prepare(&mut self, v: Value) -> JsResult<Value> {
        let mut keys: Vec<Value> = Vec::new();
        let object = if v.is_nullish() { Value::UNDEFINED } else { Value::object(self.to_object(v)?) };
        if let Some(mut o) = object.as_object() {
            let mut seen: Vec<Key> = Vec::new();
            loop {
                let ks = self.own_keys(o)?;
                for k in ks {
                    if let Key::Sym(_) = k {
                        continue;
                    }
                    if seen.contains(&k) {
                        continue;
                    }
                    seen.push(k);
                    if let Some(d) = self.get_own_property(o, k)? {
                        if d.enumerable == Some(true) {
                            let kv = self.key_to_value(k);
                            keys.push(kv);
                        }
                    }
                }
                match self.get_prototype_of(o)? {
                    Some(p) => o = p,
                    None => break,
                }
            }
        }
        let st = self.alloc_object(None, Class::ForIn(Box::new(ForInState { object, keys, pos: 0 })));
        Ok(Value::object(st))
    }

    fn for_in_next(&mut self, it: Value) -> JsResult<Option<Value>> {
        let o = it.as_object().unwrap();
        loop {
            let (obj, key) = match &mut self.obj_mut(o).class {
                Class::ForIn(s) => {
                    if s.pos >= s.keys.len() {
                        return Ok(None);
                    }
                    s.pos += 1;
                    (s.object, s.keys[s.pos - 1])
                }
                _ => return Ok(None),
            };
            // Skip keys deleted during the loop.
            let k = self.to_property_key(key)?;
            if let Some(ob) = obj.as_object() {
                if !self.has_property(ob, k)? {
                    continue;
                }
            }
            return Ok(Some(key));
        }
    }
}

/// A fused binary op on two int32 operands, when the result needs no
/// conversion (None: overflow or an op without an int fast path).
#[inline(always)]
fn int_binop(kind: BinKind, x: i32, y: i32) -> Option<Value> {
    Some(match kind {
        BinKind::Add => Value::int(x.checked_add(y)?),
        BinKind::Sub => Value::int(x.checked_sub(y)?),
        BinKind::Mul => {
            let p = x.checked_mul(y)?;
            if p == 0 && (x < 0 || y < 0) {
                return None; // -0
            }
            Value::int(p)
        }
        BinKind::StrictEq => Value::bool(x == y),
        BinKind::StrictNe => Value::bool(x != y),
        BinKind::Lt => Value::bool(x < y),
        BinKind::Le => Value::bool(x <= y),
        BinKind::Gt => Value::bool(x > y),
        BinKind::Ge => Value::bool(x >= y),
        BinKind::BitAnd => Value::int(x & y),
        BinKind::BitOr => Value::int(x | y),
        BinKind::BitXor => Value::int(x ^ y),
    })
}

/// A comparison of two int32 operands (CmpJmp kinds only).
#[inline(always)]
fn int_compare(kind: BinKind, x: i32, y: i32) -> bool {
    match kind {
        BinKind::Lt => x < y,
        BinKind::Le => x <= y,
        BinKind::Gt => x > y,
        BinKind::Ge => x >= y,
        BinKind::StrictEq => x == y,
        _ => x != y,
    }
}

/// A fused binary op on two numbers (None: needs the generic path).
#[inline(always)]
fn num_binop(kind: BinKind, x: f64, y: f64) -> Option<Value> {
    Some(match kind {
        BinKind::Add => Value::number(x + y),
        BinKind::Sub => Value::number(x - y),
        BinKind::Mul => Value::number(x * y),
        BinKind::Lt => Value::bool(x < y),
        BinKind::Le => Value::bool(x <= y),
        BinKind::Gt => Value::bool(x > y),
        BinKind::Ge => Value::bool(x >= y),
        BinKind::StrictEq => Value::bool(x == y),
        BinKind::StrictNe => Value::bool(x != y),
        _ => return None,
    })
}

/// A comparison of two numbers (CmpJmp kinds only).
#[inline(always)]
fn num_compare(kind: BinKind, x: f64, y: f64) -> bool {
    match kind {
        BinKind::Lt => x < y,
        BinKind::Le => x <= y,
        BinKind::Gt => x > y,
        BinKind::Ge => x >= y,
        BinKind::StrictEq => x == y,
        _ => x != y,
    }
}
