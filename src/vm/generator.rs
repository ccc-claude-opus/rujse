//! Generators and the suspension of frames (yield / await).

use alloc::boxed::Box;
use alloc::vec::Vec;

use super::interp::GenSignal;
use super::*;
use crate::bytecode::Op;
use crate::object::{Class, Key};

impl Vm {
    /// GeneratorStart: the generator object with the frame's state.
    pub(crate) fn generator_start(&mut self, fi: usize) -> JsResult<Value> {
        let f = &self.frames[fi];
        let is_async = f.code.is_async;
        let callee = f.callee;
        let fallback = if is_async { self.intrinsics.async_generator_prototype } else { self.intrinsics.generator_prototype };
        let proto = match callee.as_object() {
            Some(c) => {
                let k = Key::Str(self.names.prototype);
                let p = self.get(c, k)?;
                p.as_object().unwrap_or(fallback)
            }
            None => fallback,
        };
        let st = self.save_state(fi, GenStatus::SuspendedStart);
        let class = if is_async { Class::AsyncGenerator(Box::new(st)) } else { Class::Generator(Box::new(st)) };
        let g = self.alloc_object(Some(proto), class);
        Ok(Value::object(g))
    }

    fn save_state(&mut self, fi: usize, status: GenStatus) -> GenState {
        let f = &mut self.frames[fi];
        let base = f.base as usize;
        let n = f.code.nregs as usize;
        GenState {
            state: status,
            code: f.code.clone(),
            pc: f.pc,
            regs: self.stack[base..base + n].to_vec(),
            env: f.env,
            env_depth: f.env_depth,
            this: f.this,
            callee: f.callee,
            new_target: f.new_target,
            args: f.args.take(),
            promise: Value::UNDEFINED,
            queue: Vec::new(),
            awaiting_return: false,
        }
    }

    pub fn gen_state_mut(&mut self, g: ObjId) -> &mut GenState {
        match &mut self.obj_mut(g).class {
            Class::Generator(s) | Class::AsyncGenerator(s) => s,
            _ => panic!("not a generator"),
        }
    }

    pub fn gen_state(&self, g: ObjId) -> &GenState {
        match &self.obj(g).class {
            Class::Generator(s) | Class::AsyncGenerator(s) => s,
            _ => panic!("not a generator"),
        }
    }

    /// Saves the running frame into its generator/async state object and
    /// returns what the frame's caller receives.
    pub(crate) fn suspend(&mut self, fi: usize, value: Value, sig: GenSignal) -> Value {
        let g = self.frames[fi].generator.expect("suspend outside a generator");
        let kind = self.frames[fi].kind;
        let st = self.save_state(fi, GenStatus::SuspendedYield);
        {
            let s = self.gen_state_mut(g);
            let promise = s.promise;
            let queue = core::mem::take(&mut s.queue);
            *s = st;
            s.promise = promise;
            s.queue = queue;
        }
        self.gen_signal = sig;
        if sig == GenSignal::Await && kind != FrameKind::Resume {
            // First await of an async function: the caller gets the promise.
            return self.gen_state(g).promise;
        }
        value
    }

    /// The frame of generator/async object `g` completed (normally or by
    /// an exception). Returns the value for the frame's caller.
    pub(crate) fn finish_generator_frame(&mut self, g: ObjId, f: &Frame, r: JsResult<Value>) -> JsResult<Value> {
        let is_async_fn = f.code.is_async && !f.code.is_generator;
        self.gen_state_mut(g).state = GenStatus::Completed;
        if is_async_fn {
            let promise = self.gen_state(g).promise;
            match r {
                Ok(v) => crate::builtins::promise::resolve_promise(self, promise, v)?,
                Err(Throw::Value(e)) => crate::builtins::promise::reject_promise(self, promise, e),
                Err(Throw::Terminate) => return Err(Throw::Terminate),
            }
            self.gen_signal = GenSignal::Return;
            return Ok(if f.kind == FrameKind::Resume { Value::UNDEFINED } else { promise });
        }
        self.gen_signal = GenSignal::Return;
        r
    }

    /// Resumes a suspended generator or async function with a completion
    /// (kind 0 next/fulfilled, 1 throw/rejected, 2 return). Returns the
    /// value it yielded/returned and how it stopped.
    pub fn resume(&mut self, g: ObjId, kind: u8, value: Value) -> JsResult<(Value, GenSignal)> {
        let (st, was_start) = {
            let s = self.gen_state_mut(g);
            let was_start = s.state == GenStatus::SuspendedStart;
            s.state = GenStatus::Executing;
            let st = GenState {
                state: GenStatus::Executing,
                code: s.code.clone(),
                pc: s.pc,
                regs: core::mem::take(&mut s.regs),
                env: s.env,
                env_depth: s.env_depth,
                this: s.this,
                callee: s.callee,
                new_target: s.new_target,
                args: s.args.take(),
                promise: s.promise,
                queue: Vec::new(),
                awaiting_return: false,
            };
            (st, was_start)
        };
        let base = self.sp + 2;
        let n = st.regs.len();
        if self.stack.len() < base + n {
            self.stack.resize(base + n + 16, Value::UNDEFINED);
        }
        self.stack[base..base + n].copy_from_slice(&st.regs);
        self.sp = base + n;
        if !was_start {
            // The register named by the suspending op gets the kind.
            let pc = st.pc as usize;
            if pc > 0 {
                if let Op::Yield(r) | Op::YieldRaw(r) | Op::AsyncGenYield(r) | Op::Await(r) = st.code.op_at(pc - 1) {
                    self.stack[base + r as usize] = Value::int(kind as i32);
                }
            }
        }
        let depth = self.frames.len();
        self.frames.push(Frame {
            code: st.code,
            pc: st.pc,
            base: base as u32,
            env: st.env,
            env_depth: st.env_depth,
            this: st.this,
            callee: st.callee,
            new_target: st.new_target,
            args: st.args,
            argc: 0,
            kind: FrameKind::Resume,
            generator: Some(g),
        });
        self.pending_acc = Some(value);
        self.gen_signal = GenSignal::None;
        self.native_depth += 1;
        let r = self.run(depth);
        self.native_depth -= 1;
        let sig = self.gen_signal;
        self.gen_signal = GenSignal::None;
        match r {
            Ok(v) => {
                if sig == GenSignal::Return {
                    self.gen_state_mut(g).state = GenStatus::Completed;
                }
                Ok((v, sig))
            }
            Err(e) => {
                self.gen_state_mut(g).state = GenStatus::Completed;
                Err(e)
            }
        }
    }

    /// generator.next/throw/return for sync generators: the iterator result.
    pub fn generator_resume(&mut self, g: ObjId, kind: u8, value: Value) -> JsResult<Value> {
        let status = self.gen_state(g).state;
        match status {
            GenStatus::Executing => return Err(self.type_error("Generator is already running")),
            GenStatus::Completed => {
                return match kind {
                    0 => Ok(self.iter_result(Value::UNDEFINED, true)),
                    1 => Err(Throw::Value(value)),
                    _ => Ok(self.iter_result(value, true)),
                };
            }
            GenStatus::SuspendedStart if kind != 0 => {
                self.gen_state_mut(g).state = GenStatus::Completed;
                return match kind {
                    1 => Err(Throw::Value(value)),
                    _ => Ok(self.iter_result(value, true)),
                };
            }
            _ => {}
        }
        let (v, sig) = self.resume(g, kind, value)?;
        Ok(match sig {
            GenSignal::YieldRaw => v,
            GenSignal::Yield => self.iter_result(v, false),
            _ => self.iter_result(v, true),
        })
    }
}

impl Vm {
    /// The async generator is suspended at an `await` (not a `yield`).
    pub fn async_gen_in_await(&self, g: ObjId) -> bool {
        let st = self.gen_state(g);
        let pc = st.pc as usize;
        pc > 0 && matches!(st.code.op_at(pc - 1), Op::Await(_))
    }
}
