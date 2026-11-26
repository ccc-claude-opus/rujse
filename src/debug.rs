//! The debugger: breakpoints, stepping, `debugger;`, pausing on
//! exceptions, and while paused the call stack, the variables of each
//! scope and evaluation in a frame (rubrowser maps this onto CDP
//! `Debugger`).
//!
//! Breakpoints and step locations patch an `Op::Break` over the
//! instruction (`Code::patches` keeps the original), so code runs at full
//! speed when nothing is set. Locations are the source positions the
//! compiler records (statement starts and some expressions). Lines and
//! columns are 1-based.

use alloc::boxed::Box;
use alloc::rc::Rc;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use crate::bytecode::{Code, Op, Patch};
use crate::object::Class;
use crate::value::Value;
use crate::vm::{JsResult, Throw, Vm};

/// Why execution paused.
#[derive(Clone, Debug, PartialEq)]
pub enum PauseReason {
    Breakpoint(u32),
    DebuggerStatement,
    Step,
    /// A thrown value (see `Debugger::set_pause_on_exceptions`).
    Exception(Value),
    /// `Vm::debug_request_pause`.
    Request,
}

/// What to do when the pause callback returns.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Resume {
    Continue,
    StepInto,
    StepOver,
    StepOut,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum PauseOnExceptions {
    #[default]
    None,
    /// Only exceptions no `try` in the JS stack will catch.
    Uncaught,
    All,
}

/// The embedder's side of the debugger. While `paused` runs, the VM can
/// be inspected (`debug_frames`, `debug_scopes`, `debug_evaluate`, the
/// inspector) and does not pause again.
pub trait DebugHandler {
    fn paused(&mut self, vm: &mut Vm, reason: &PauseReason) -> Resume;
    /// A script (or module, eval code, `Function` body) was compiled.
    fn script_parsed(&mut self, _vm: &mut Vm, _script: &ScriptInfo) {}
}

#[derive(Clone, Debug)]
pub struct ScriptInfo {
    pub id: u32,
    pub url: String,
    pub source: Rc<str>,
}

/// One frame of the JS call stack (index 0 = the innermost).
#[derive(Clone, Debug)]
pub struct CallFrameInfo {
    pub function_name: String,
    pub script_id: u32,
    pub url: String,
    pub line: u32,
    pub column: u32,
    pub this: Value,
    pub callee: Value,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScopeType {
    /// Variables of the paused function (registers and its own blocks).
    Local,
    /// Variables of an enclosing function captured by a closure.
    Closure,
    /// A `with` object.
    With,
    /// Script-level `let`/`const`/`class` and the global object.
    Global,
}

#[derive(Clone, Debug)]
pub struct ScopeInfo {
    pub kind: ScopeType,
    /// (name, value); `None` while the binding is in its TDZ.
    pub variables: Vec<(String, Option<Value>)>,
    /// The object of a `With` or `Global` scope.
    pub object: Option<Value>,
}

/// A breakpoint as set by the embedder, and where it resolved.
pub struct Breakpoint {
    pub id: u32,
    pub url: String,
    pub line: u32,
    pub column: Option<u32>,
    pub condition: Option<String>,
    /// Resolved locations: (code, pc, line, column).
    pub locations: Vec<(Rc<Code>, u32, u32, u32)>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Step {
    None,
    Into,
    /// Stop at a location in a frame at depth <= the value.
    Over(usize),
    /// Stop at a location in a frame at depth < the value.
    Out(usize),
}

pub struct Debugger {
    handler: Option<Box<dyn DebugHandler>>,
    pub breakpoints: Vec<Breakpoint>,
    next_id: u32,
    pub pause_on_exceptions: PauseOnExceptions,
    step: Step,
    /// Codes with step locations armed.
    armed: Vec<Rc<Code>>,
    /// Top-level codes compiled since the debugger was enabled.
    scripts: Vec<Rc<Code>>,
    pause_requested: bool,
    /// Compiling the debugger's own evaluations (not reported as scripts).
    internal: bool,
    /// A patched instruction temporarily restored to run it once.
    rearm: Option<(Rc<Code>, u32)>,
    /// The exception last reported (not reported again while it
    /// propagates through nested native calls).
    last_exception: Option<Value>,
}

impl Vm {
    // ----- control ----------------------------------------------------------

    /// Turns the debugger on. Scripts compiled before this call are not
    /// known to it.
    pub fn enable_debugger(&mut self, handler: Box<dyn DebugHandler>) {
        self.debugger = Some(Box::new(Debugger {
            handler: Some(handler),
            breakpoints: Vec::new(),
            next_id: 1,
            pause_on_exceptions: PauseOnExceptions::None,
            step: Step::None,
            armed: Vec::new(),
            scripts: Vec::new(),
            pause_requested: false,
            internal: false,
            rearm: None,
            last_exception: None,
        }));
    }

    /// Turns the debugger off and removes every patch.
    pub fn disable_debugger(&mut self) {
        if let Some(d) = self.debugger.take() {
            for c in d.scripts.iter() {
                unpatch_all(c);
            }
            for c in d.armed.iter() {
                unpatch_all(c);
            }
        }
    }

    pub fn set_pause_on_exceptions(&mut self, p: PauseOnExceptions) {
        if let Some(d) = self.debugger.as_mut() {
            d.pause_on_exceptions = p;
        }
    }

    /// Pauses at the next location reached (any frame).
    pub fn debug_request_pause(&mut self) {
        if let Some(d) = self.debugger.as_mut() {
            d.pause_requested = true;
            self.ticks = 0;
        }
    }

    /// Sets a breakpoint at `line` (and the first location at or after
    /// `column`) of every script with this URL, now and when such
    /// scripts are compiled later. Returns its id.
    pub fn set_breakpoint(&mut self, url: &str, line: u32, column: Option<u32>, condition: Option<&str>) -> Option<u32> {
        let d = self.debugger.as_mut()?;
        let id = d.next_id;
        d.next_id += 1;
        let mut bp = Breakpoint { id, url: url.into(), line, column, condition: condition.map(String::from), locations: Vec::new() };
        let scripts: Vec<Rc<Code>> = d.scripts.iter().filter(|c| c.source.name == url).cloned().collect();
        for c in &scripts {
            resolve(&mut bp, c);
        }
        d.breakpoints.push(bp);
        Some(id)
    }

    /// Where a breakpoint resolved: (script id, line, column) per location.
    pub fn breakpoint_locations(&self, id: u32) -> Vec<(u32, u32, u32)> {
        let Some(d) = self.debugger.as_ref() else { return Vec::new() };
        d.breakpoints
            .iter()
            .filter(|b| b.id == id)
            .flat_map(|b| b.locations.iter().map(|(c, _, l, col)| (c.source.id, *l, *col)))
            .collect()
    }

    pub fn remove_breakpoint(&mut self, id: u32) {
        let Some(d) = self.debugger.as_mut() else { return };
        let Some(i) = d.breakpoints.iter().position(|b| b.id == id) else { return };
        let bp = d.breakpoints.remove(i);
        for (c, pc, _, _) in bp.locations {
            set_patch(&c, pc, |p| {
                if p.breakpoint == Some(id) {
                    p.breakpoint = None;
                }
            });
        }
    }

    /// The source of a script known to the debugger.
    pub fn debug_script_source(&self, id: u32) -> Option<Rc<str>> {
        self.sources.iter().find(|s| s.id == id).map(|s| s.text.clone())
    }

    // ----- while paused ---------------------------------------------------

    /// The JS call stack, innermost first.
    pub fn debug_frames(&self) -> Vec<CallFrameInfo> {
        let n = self.frames.len();
        (0..n)
            .rev()
            .map(|i| {
                let f = &self.frames[i];
                // Outer frames stand after their call instruction.
                let pc = if i + 1 == n { f.pc } else { f.pc.saturating_sub(1) };
                let (line, column) = f.code.position(pc);
                let function_name = match f.code.name.as_string() {
                    Some(s) => self.str(s).to_rust_lossy(),
                    None => String::new(),
                };
                CallFrameInfo {
                    function_name,
                    script_id: f.code.source.id,
                    url: f.code.source.name.clone(),
                    line,
                    column,
                    this: f.this,
                    callee: f.callee,
                }
            })
            .collect()
    }

    /// The scope chain of frame `index` (0 = innermost), innermost first.
    pub fn debug_scopes(&self, index: usize) -> Vec<ScopeInfo> {
        let n = self.frames.len();
        if index >= n {
            return Vec::new();
        }
        let fi = n - 1 - index;
        let f = &self.frames[fi];
        let pc = if index == 0 { f.pc } else { f.pc.saturating_sub(1) };
        let mut out = Vec::new();
        // Local: register variables of the scopes live at pc (innermost
        // first: later-starting scopes are nested in earlier ones).
        let mut live: Vec<&crate::bytecode::DebugScope> = f.code.body().scopes.iter().filter(|s| s.start <= pc && pc < s.end).collect();
        live.sort_by(|a, b| b.start.cmp(&a.start).then(a.end.cmp(&b.end)));
        let mut local = ScopeInfo { kind: ScopeType::Local, variables: Vec::new(), object: None };
        let mut seen: Vec<Rc<str>> = Vec::new();
        for s in &live {
            for (name, r) in &s.regs {
                if seen.contains(name) || !visible(name) {
                    continue;
                }
                seen.push(name.clone());
                let v = self.stack.get(f.base as usize + *r as usize).copied().unwrap_or(Value::UNDEFINED);
                local.variables.push((name.to_string(), (!v.is_uninit()).then_some(v)));
            }
        }
        // Environments: the frame's own (env_depth of them), then those
        // of enclosing functions.
        let mut e = f.env;
        let mut own = f.env_depth as usize;
        let mut closure: Option<ScopeInfo> = None;
        while let Some(id) = e {
            let env = self.envs.get(id.0);
            if let Some(o) = env.with {
                if let Some(c) = closure.take() {
                    out.push(c);
                }
                out.push(ScopeInfo { kind: ScopeType::With, variables: Vec::new(), object: Some(Value::object(o)) });
            } else if let Some(names) = &env.names {
                let vars: Vec<(String, Option<Value>)> = names
                    .chunks(2)
                    .zip(env.slots.iter())
                    .filter_map(|(p, v)| {
                        if p[1] == Value::int(2) {
                            return None; // a function expression's own name
                        }
                        let name = self.str(p[0].as_string()?).to_rust_lossy();
                        visible(&name).then(|| (name, (!v.is_uninit()).then_some(*v)))
                    })
                    .collect();
                if own > 0 {
                    for (n, v) in vars {
                        if !local.variables.iter().any(|(m, _)| *m == n) {
                            local.variables.push((n, v));
                        }
                    }
                } else {
                    // Envs of one enclosing function show as one scope;
                    // a new Closure scope starts at each function's
                    // outermost env (we cannot tell where functions end,
                    // so each env is its own scope).
                    if let Some(c) = closure.take() {
                        out.push(c);
                    }
                    closure = Some(ScopeInfo { kind: ScopeType::Closure, variables: vars, object: None });
                }
            }
            own = own.saturating_sub(1);
            e = env.parent;
        }
        if let Some(c) = closure.take() {
            out.push(c);
        }
        if !local.variables.is_empty() || out.is_empty() {
            out.insert(0, local);
        }
        // Global: script-level lexical bindings, then the global object.
        let mut vars = Vec::new();
        for (k, i) in &self.global_lex {
            let v = self.global_cells[*i as usize].0;
            vars.push((self.str(*k).to_rust_lossy(), (!v.is_uninit()).then_some(v)));
        }
        out.push(ScopeInfo { kind: ScopeType::Global, variables: vars, object: Some(Value::object(self.global)) });
        out
    }

    /// Evaluates `src` in frame `index`: its variables are visible (as
    /// copies: assignments to them are not written back), `this` is the
    /// frame's. Runs without pausing.
    pub fn debug_evaluate(&mut self, index: usize, src: &str) -> JsResult<Value> {
        let n = self.frames.len();
        if index >= n {
            return self.global_eval(src);
        }
        let this = self.frames[n - 1 - index].this;
        let scopes = self.debug_scopes(index);
        let scope = self.alloc_object(None, Class::Ordinary);
        let sv = Value::object(scope);
        let roots = self.native_roots.len();
        self.native_roots.push(sv);
        // Outermost first, so inner bindings overwrite outer ones.
        for s in scopes.iter().rev() {
            if s.kind == ScopeType::Global {
                continue;
            }
            for (name, v) in &s.variables {
                if let Some(v) = v {
                    let k = self.key(name);
                    self.define_data(scope, k, *v, crate::object::attr::DEFAULT);
                }
            }
        }
        let paused = self.debugger.as_mut().and_then(|d| {
            d.internal = true;
            d.handler.take()
        });
        let r = self
            .global_eval("(function (__rujse_debug_scope__, __rujse_debug_src__) { with (__rujse_debug_scope__) { return eval(__rujse_debug_src__); } })")
            .and_then(|f| {
                let s = self.new_string(src);
                self.call(f, this, &[sv, s])
            });
        if let Some(d) = self.debugger.as_mut() {
            d.internal = false;
            if paused.is_some() {
                d.handler = paused;
            }
        }
        self.native_roots.truncate(roots);
        r
    }

    // ----- hooks called by the engine -------------------------------------

    /// An `Op::Break` was reached at `pc` of frame `fi`.
    pub(crate) fn debug_break(&mut self, fi: usize, pc: usize, patch: u32, _acc: Value) -> JsResult<()> {
        let code = self.frames[fi].code.clone();
        let p = code.body().patches.borrow()[patch as usize];
        let Some(d) = self.debugger.as_ref() else { return Ok(()) };
        if d.handler.is_none() {
            return Ok(()); // inside a pause (evaluating)
        }
        if let Some(id) = p.breakpoint {
            let cond = d.breakpoints.iter().find(|b| b.id == id).and_then(|b| b.condition.clone());
            let hit = match cond {
                None => true,
                Some(c) => match self.debug_evaluate(0, &c) {
                    Ok(v) => self.to_boolean(v),
                    Err(Throw::Terminate) => return Err(Throw::Terminate),
                    Err(_) => false,
                },
            };
            if hit {
                return self.debug_pause(PauseReason::Breakpoint(id));
            }
        }
        if p.step {
            let depth = self.frames.len();
            let stop = match self.debugger.as_ref().map(|d| d.step) {
                Some(Step::Into) => true,
                Some(Step::Over(d)) => depth <= d,
                Some(Step::Out(d)) => depth < d,
                _ => false,
            };
            if stop {
                let reason = if self.debugger.as_mut().is_some_and(|d| core::mem::take(&mut d.pause_requested)) {
                    PauseReason::Request
                } else {
                    PauseReason::Step
                };
                return self.debug_pause(reason);
            }
        }
        let _ = pc;
        Ok(())
    }

    /// Calls the handler and applies its answer.
    pub fn debug_pause(&mut self, reason: PauseReason) -> JsResult<()> {
        let Some(d) = self.debugger.as_mut() else { return Ok(()) };
        let Some(mut h) = d.handler.take() else { return Ok(()) };
        // Stepping ends at a pause.
        d.step = Step::None;
        for c in core::mem::take(&mut d.armed) {
            disarm(&c);
        }
        let resume = h.paused(self, &reason);
        let Some(d) = self.debugger.as_mut() else { return Ok(()) };
        d.handler = Some(h);
        let depth = self.frames.len();
        d.step = match resume {
            Resume::Continue => Step::None,
            Resume::StepInto => Step::Into,
            Resume::StepOver => Step::Over(depth),
            Resume::StepOut => Step::Out(depth),
        };
        if d.step != Step::None {
            self.arm_stack();
        }
        Ok(())
    }

    /// Arms step locations in every code on the stack.
    fn arm_stack(&mut self) {
        let codes: Vec<Rc<Code>> = self.frames.iter().map(|f| f.code.clone()).collect();
        for c in codes {
            self.arm(&c);
        }
    }

    fn arm(&mut self, c: &Rc<Code>) {
        let Some(d) = self.debugger.as_mut() else { return };
        if d.armed.iter().any(|a| Rc::ptr_eq(a, c)) {
            return;
        }
        for &(pc, _, _) in c.body().lines.iter() {
            if (pc as usize) < c.body().ops.len() {
                set_patch(c, pc, |p| p.step = true);
            }
        }
        d.armed.push(c.clone());
    }

    /// After a `Break`: the original op at `pc` runs now; patch it again
    /// at the next safepoint.
    pub(crate) fn debug_rearm(&mut self, code: &Rc<Code>, pc: u32) {
        if let Some(d) = self.debugger.as_mut() {
            d.rearm = Some((code.clone(), pc));
            self.ticks = 1;
        } else {
            // Debugger turned off while paused: the patch is gone.
        }
    }

    fn do_rearm(&mut self) {
        if let Some((c, pc)) = self.debugger.as_mut().and_then(|d| d.rearm.take()) {
            set_patch(&c, pc, |_| {});
        }
    }

    /// A frame was pushed (stepping into it arms its code).
    #[inline]
    pub(crate) fn debug_on_enter(&mut self) {
        self.do_rearm();
        if let Some(d) = self.debugger.as_ref() {
            if d.step == Step::Into {
                let c = self.frames.last().unwrap().code.clone();
                self.arm(&c);
            }
        }
    }

    /// A safepoint with the debugger on: a requested pause becomes a step
    /// into the next location.
    pub(crate) fn debug_safepoint(&mut self) {
        self.do_rearm();
        if let Some(d) = self.debugger.as_mut() {
            if d.pause_requested && d.handler.is_some() {
                d.step = Step::Into;
                self.arm_stack();
            }
        }
    }

    /// A value was thrown in frame `fi`.
    pub(crate) fn debug_on_throw(&mut self, v: Value) -> JsResult<()> {
        let Some(d) = self.debugger.as_mut() else { return Ok(()) };
        if d.handler.is_none() || d.pause_on_exceptions == PauseOnExceptions::None || d.last_exception == Some(v) {
            return Ok(());
        }
        d.last_exception = Some(v);
        if d.pause_on_exceptions == PauseOnExceptions::Uncaught && self.exception_is_caught() {
            return Ok(());
        }
        self.debug_pause(PauseReason::Exception(v))
    }

    /// Some frame on the JS stack has a handler around its current pc.
    fn exception_is_caught(&self) -> bool {
        self.frames.iter().any(|f| {
            // pc stands after the throwing (or calling) instruction.
            let pc = f.pc.saturating_sub(1);
            f.code.body().handlers.iter().any(|h| h.start <= pc && pc < h.end)
        })
    }
}

/// `debugger;`
pub fn on_debugger_statement(vm: &mut Vm) -> JsResult<()> {
    if vm.debugger.as_ref().is_some_and(|d| d.handler.is_some()) {
        return vm.debug_pause(PauseReason::DebuggerStatement);
    }
    Ok(())
}

/// A top-level code was compiled: register it, resolve breakpoints set
/// by URL, and tell the handler.
pub fn code_compiled(vm: &mut Vm, code: &Rc<Code>) {
    let Some(d) = vm.debugger.as_mut() else { return };
    if d.internal {
        return;
    }
    d.scripts.push(code.clone());
    let url = code.source.name.clone();
    for bp in d.breakpoints.iter_mut().filter(|b| b.url == url) {
        resolve(bp, code);
    }
    if let Some(mut h) = d.handler.take() {
        let info = ScriptInfo { id: code.source.id, url, source: code.source.text.clone() };
        h.script_parsed(vm, &info);
        if let Some(d) = vm.debugger.as_mut() {
            d.handler = Some(h);
        }
    }
}

/// Values the debugger and inspector keep alive.
pub fn trace_roots(vm: &Vm, out: &mut Vec<Value>) {
    out.extend(vm.inspector.values());
    if let Some(d) = vm.debugger.as_ref() {
        out.extend(d.last_exception);
    }
}

fn visible(name: &str) -> bool {
    !(name.starts_with('%') || name.starts_with('#') || name == "this" || name == "new.target")
}


/// Resolves a breakpoint in `code` and its nested functions: the
/// location on `line` with the smallest column at or after `column`.
fn resolve(bp: &mut Breakpoint, code: &Rc<Code>) {
    let mut best: Option<(Rc<Code>, u32, u32)> = None;
    let mut stack = alloc::vec![code.clone()];
    while let Some(c) = stack.pop() {
        for &(pc, line, col) in c.body().lines.iter() {
            if line != bp.line || (pc as usize) >= c.body().ops.len() || bp.column.is_some_and(|m| col < m) {
                continue;
            }
            if best.as_ref().is_none_or(|b| col < b.2) {
                best = Some((c.clone(), pc, col));
            }
        }
        stack.extend(c.body().children.iter().cloned());
    }
    if let Some((c, pc, col)) = best {
        let id = bp.id;
        set_patch(&c, pc, |p| p.breakpoint = Some(id));
        bp.locations.push((c, pc, bp.line, col));
    }
}

/// Changes the patch at `pc` (creating it), and restores the original
/// instruction when nothing is left on it.
fn set_patch(c: &Code, pc: u32, f: impl FnOnce(&mut Patch)) {
    let cell = &c.body().ops[pc as usize];
    let mut patches = c.body().patches.borrow_mut();
    let i = match cell.get() {
        Op::Break(i) => i as usize,
        op => {
            // Reuse a dead patch entry of this pc, else add one.
            match patches.iter().position(|p| p.pc == pc) {
                Some(i) => {
                    patches[i].op = op;
                    i
                }
                None => {
                    patches.push(Patch { pc, op, breakpoint: None, step: false });
                    patches.len() - 1
                }
            }
        }
    };
    f(&mut patches[i]);
    let p = patches[i];
    cell.set(if p.breakpoint.is_some() || p.step { Op::Break(i as u32) } else { p.op });
}

fn disarm(c: &Code) {
    let pcs: Vec<u32> = c.body().patches.borrow().iter().filter(|p| p.step).map(|p| p.pc).collect();
    for pc in pcs {
        set_patch(c, pc, |p| p.step = false);
    }
}

fn unpatch_all(c: &Rc<Code>) {
    let mut stack = alloc::vec![c.clone()];
    while let Some(c) = stack.pop() {
        for p in c.body().patches.borrow().iter() {
            c.body().ops[p.pc as usize].set(p.op);
        }
        stack.extend(c.body().children.iter().cloned());
    }
}
