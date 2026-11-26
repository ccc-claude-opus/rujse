//! Bytecode compiler: scope resolution, functions and statements
//! (expressions, patterns and classes are in `expr.rs`).
//!
//! Bindings live in registers unless a nested function captures them (or
//! `eval`/`with` may reach them by name); captured ones live in
//! environment slots. `try/finally` exits run the finally block inline.

mod expr;
pub mod scan;

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::format;
use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::cell::Cell;

use crate::ast::*;
use crate::bytecode::*;
use crate::value::Value;
use crate::vm::Vm;
use scan::{Region, Scanner};

pub type CResult<T> = Result<T, CompileError>;

#[derive(Debug, Clone)]
pub struct CompileError {
    pub message: String,
    pub line: u32,
    pub col: u32,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BKind {
    Var,
    Let,
    Const,
    /// A class's inner name binding (immutable).
    ClassName,
    /// A named function expression's own name (immutable; assignment is
    /// ignored in sloppy mode).
    FnName,
    Param,
    /// `this`, `new.target`, `%fn`, `arguments`, private names.
    Pseudo,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Loc {
    Reg(Reg),
    Env(u16),
}

#[derive(Clone, Debug)]
pub struct Binding {
    pub name: Name,
    pub kind: BKind,
    pub loc: Loc,
    /// May be read before initialization (needs a TDZ check).
    pub tdz: bool,
    /// Source offset where the binding is initialized.
    pub init_end: u32,
    pub in_switch: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ScopeKind {
    Function,
    Block,
    With,
    Script,
    /// A scope of the code calling a direct eval (resolved by name/slot).
    Outer,
}

pub struct Scope {
    pub kind: ScopeKind,
    pub bindings: Vec<Binding>,
    /// This scope pushed an environment at run time.
    pub has_env: bool,
    pub env_slots: u16,
    /// Index of the PushEnv op (patched with the final slot count).
    pub push_at: Option<usize>,
    pub names: Vec<Value>,
    /// Register high-water mark to restore when the scope ends.
    pub reg_mark: Reg,
    /// A sloppy function's var scope that direct eval may extend.
    pub dynamic: bool,
    /// The environment records slot names (eval/with lookups by name).
    pub named: bool,
}

/// A context crossed by `break`/`continue`/`return`.
enum Ctx<'a> {
    Breakable {
        labels: Vec<Name>,
        is_loop: bool,
        is_switch: bool,
        /// for-of: a `break` to this loop closes the iterator (reg, try
        /// region, async).
        iter: Option<(Reg, usize, bool)>,
        breaks: Vec<usize>,
        continues: Vec<usize>,
        env_depth: u16,
        /// `continue` target known (do-while/for update) — patched later.
        continue_target: Option<Label>,
    },
    Finally {
        body: &'a [Stmt],
        try_index: usize,
    },
    IterClose {
        reg: Reg,
        try_index: usize,
        is_async: bool,
    },
    /// An active try region (catch handler) without cleanup.
    Catch {
        try_index: usize,
    },
}

/// An exception region being built (may be split around inline code).
struct TryRegion {
    ranges: Vec<(Label, Label)>,
    open: Option<Label>,
    target: Label,
    reg: Reg,
    env_depth: u16,
}

pub struct FuncState<'a> {
    ops: Vec<Op>,
    consts: Vec<Const>,
    children: Vec<Rc<Code>>,
    lines: Vec<(u32, u32, u32)>,
    pub next_reg: Reg,
    nregs: Reg,
    pub scopes: Vec<Scope>,
    env_depth: u16,
    ctx: Vec<Ctx<'a>>,
    tries: Vec<TryRegion>,
    handlers: Vec<Handler>,
    const_cache: BTreeMap<u64, K>,
    pub strict: bool,
    kind: FuncKind,
    is_async: bool,
    is_generator: bool,
    is_arrow: bool,
    /// Labels waiting for the next loop statement.
    pending_labels: Vec<Name>,
    /// The function has its own `this` binding (pseudo-var) or uses frame
    /// `this` directly.
    derived: bool,
    template_sites: u32,
    /// Every environment of this function records its slot names.
    pub named_envs: bool,
}

pub struct Compiler<'a, 'v> {
    pub vm: &'v mut Vm,
    pub scan: Scanner,
    pub source: Rc<Source>,
    pub funcs: Vec<FuncState<'a>>,
    /// Template sites: unique per tagged template in the source.
    next_site: u32,
    /// Set by the expression compiler: the next function compiled is a
    /// function expression (gets its own name scope).
    pub fn_expr_flag: Cell<bool>,
    /// Annex B.3.3 block function declarations (by address).
    pub annex_b_sites: core::cell::RefCell<Vec<usize>>,
    /// Short-circuit jumps of the optional chains being compiled.
    pub chains: Vec<Vec<usize>>,
}

fn err<T>(msg: &str, pos: Pos) -> CResult<T> {
    Err(CompileError { message: msg.into(), line: pos.line, col: pos.col })
}

impl<'a, 'v> Compiler<'a, 'v> {
    pub fn new(vm: &'v mut Vm, source: Rc<Source>) -> Self {
        Compiler { vm, scan: Scanner::default(), source, funcs: Vec::new(), next_site: 0, fn_expr_flag: Cell::new(false), annex_b_sites: Default::default(), chains: Vec::new() }
    }

    // ----- emission helpers ------------------------------------------

    fn f(&mut self) -> &mut FuncState<'a> {
        self.funcs.last_mut().unwrap()
    }

    fn fr(&self) -> &FuncState<'a> {
        self.funcs.last().unwrap()
    }

    pub fn emit(&mut self, op: Op) -> usize {
        let f = self.f();
        f.ops.push(op);
        f.ops.len() - 1
    }

    pub fn here(&self) -> Label {
        self.fr().ops.len() as Label
    }

    pub fn patch(&mut self, at: usize, target: Label) {
        let op = &mut self.f().ops[at];
        *op = match *op {
            Op::Jmp(_) => Op::Jmp(target),
            Op::JmpTrue(_) => Op::JmpTrue(target),
            Op::JmpFalse(_) => Op::JmpFalse(target),
            Op::JmpNullish(_) => Op::JmpNullish(target),
            Op::JmpNotNullish(_) => Op::JmpNotNullish(target),
            Op::JmpUndefined(_) => Op::JmpUndefined(target),
            Op::JmpNotUndefined(_) => Op::JmpNotUndefined(target),
            Op::JmpStrictEq(r, _) => Op::JmpStrictEq(r, target),
            Op::JmpIfIntEq(r, i, _) => Op::JmpIfIntEq(r, i, target),
            Op::IteratorStep(r, _) => Op::IteratorStep(r, target),
            Op::ForInNext(r, _) => Op::ForInNext(r, target),
            other => panic!("patch of non-jump {other:?}"),
        };
    }

    pub fn patch_here(&mut self, at: usize) {
        let h = self.here();
        self.patch(at, h);
    }

    pub fn pos(&mut self, p: Pos) {
        let pc = self.here();
        let f = self.f();
        if f.lines.last().is_some_and(|l| l.0 == pc) {
            f.lines.pop();
        }
        if f.lines.last().is_some_and(|l| l.1 == p.line && l.2 == p.col) {
            return;
        }
        f.lines.push((pc, p.line, p.col));
    }

    pub fn alloc_reg(&mut self) -> Reg {
        let f = self.f();
        let r = f.next_reg;
        f.next_reg += 1;
        if f.next_reg > f.nregs {
            f.nregs = f.next_reg;
        }
        r
    }

    /// `n` contiguous registers.
    pub fn alloc_regs(&mut self, n: u16) -> Reg {
        let f = self.f();
        let r = f.next_reg;
        f.next_reg += n;
        if f.next_reg > f.nregs {
            f.nregs = f.next_reg;
        }
        r
    }

    /// Frees registers down to `r` (LIFO).
    pub fn free_to(&mut self, r: Reg) {
        self.f().next_reg = r;
    }

    pub fn konst(&mut self, c: Const) -> K {
        let f = self.f();
        f.consts.push(c);
        (f.consts.len() - 1) as K
    }

    pub fn value_const(&mut self, v: Value) -> K {
        if let Some(&k) = self.fr().const_cache.get(&v.raw()) {
            return k;
        }
        let k = self.konst(Const::Value(v));
        self.f().const_cache.insert(v.raw(), k);
        k
    }

    pub fn atom(&mut self, s: &str) -> Value {
        Value::str(self.vm.intern(s))
    }

    pub fn atom16(&mut self, s: &[u16]) -> Value {
        Value::str(self.vm.intern_units(s))
    }

    pub fn name_const(&mut self, s: &str) -> K {
        let v = self.atom(s);
        self.value_const(v)
    }

    pub fn str16_const(&mut self, s: &[u16]) -> K {
        let v = self.atom16(s);
        self.value_const(v)
    }

    pub fn load_number(&mut self, n: f64) {
        let v = Value::number(n);
        if let Some(i) = v.as_int() {
            self.emit(Op::LdaInt(i));
        } else {
            let k = self.value_const(v);
            self.emit(Op::LdaConst(k));
        }
    }

    // ----- scopes -----------------------------------------------------

    pub fn push_scope(&mut self, kind: ScopeKind) {
        let mark = self.fr().next_reg;
        let named = self.fr().named_envs;
        self.f().scopes.push(Scope {
            kind,
            bindings: Vec::new(),
            has_env: false,
            env_slots: 0,
            push_at: None,
            names: Vec::new(),
            reg_mark: mark,
            dynamic: false,
            named,
        });
    }

    /// Declares a binding in the innermost scope; `captured` puts it in
    /// the scope's environment (created on first use).
    pub fn declare(&mut self, name: Name, kind: BKind, captured: bool, tdz: bool, init_end: u32) -> Loc {
        let in_switch = false;
        if let Some(b) = self.f().scopes.last().unwrap().bindings.iter().find(|b| b.name == name) {
            return b.loc;
        }
        let loc = if captured {
            let atom = self.atom(&name);
            let kv = Value::int(match kind {
                BKind::Const | BKind::ClassName => 1,
                BKind::FnName => 2,
                _ => 0,
            });
            let s = self.f().scopes.last_mut().unwrap();
            let slot = s.env_slots;
            s.env_slots += 1;
            s.names.push(atom);
            s.names.push(kv);
            Loc::Env(slot)
        } else {
            Loc::Reg(self.alloc_reg())
        };
        let s = self.f().scopes.last_mut().unwrap();
        s.bindings.push(Binding { name, kind, loc, tdz, init_end, in_switch });
        if captured && !s.has_env {
            // The PushEnv is emitted by the caller via `open_env`.
        }
        loc
    }

    /// Emits the PushEnv of the innermost scope if it has captured
    /// bindings (call after declaring them, before using them).
    pub fn open_env(&mut self) {
        let s = self.f().scopes.last().unwrap();
        if s.env_slots == 0 || s.has_env {
            return;
        }
        let names = s.names.clone();
        let dynamic = s.dynamic || s.named;
        let names_k = if dynamic { self.konst(Const::Names(names.into())) } else { u32::MAX };
        let n = self.fr().scopes.last().unwrap().env_slots;
        let at = self.emit(Op::PushEnv { n, names: names_k });
        let f = self.f();
        let s = f.scopes.last_mut().unwrap();
        s.has_env = true;
        s.push_at = Some(at);
        f.env_depth += 1;
    }

    /// Ends the innermost scope (pops its environment).
    pub fn pop_scope(&mut self) {
        let s = self.f().scopes.pop().unwrap();
        if s.has_env {
            // Patch the slot count (bindings may have been added later).
            if let Some(at) = s.push_at {
                let names_k = match self.fr().ops[at] {
                    Op::PushEnv { names, .. } => names,
                    _ => u32::MAX,
                };
                if names_k != u32::MAX {
                    self.f().consts[names_k as usize] = Const::Names(s.names.clone().into());
                }
                self.f().ops[at] = Op::PushEnv { n: s.env_slots, names: names_k };
            }
            self.emit(Op::PopEnv);
            self.f().env_depth -= 1;
        }
        self.free_to(s.reg_mark.max(self.lowest_live_reg()));
    }

    fn lowest_live_reg(&self) -> Reg {
        // Bindings of enclosing scopes keep their registers.
        0
    }

    /// Resolves a name to its binding location.
    pub fn resolve(&self, name: &str) -> Resolved {
        let mut hops: u16 = 0;
        let mut crossed_function = false;
        let mut crossed_dynamic = false;
        for f in self.funcs.iter().rev() {
            for s in f.scopes.iter().rev() {
                if s.kind == ScopeKind::With {
                    return Resolved::Dynamic;
                }
                if let Some(b) = s.bindings.iter().find(|b| &*b.name == name) {
                    if crossed_dynamic {
                        return Resolved::Dynamic;
                    }
                    return match b.loc {
                        Loc::Reg(r) if !crossed_function => Resolved::Reg(r, b.clone()),
                        Loc::Reg(_) => Resolved::Dynamic,
                        Loc::Env(slot) => Resolved::Env(hops, slot, b.clone(), crossed_function),
                    };
                }
                // A sloppy var scope that eval may have extended: names
                // outside it could be shadowed at run time.
                if s.dynamic {
                    crossed_dynamic = true;
                }
                if s.has_env {
                    hops += 1;
                }
            }
            crossed_function = true;
        }
        if crossed_dynamic {
            return Resolved::Dynamic;
        }
        Resolved::Global
    }

    fn has_sloppy_eval_outer(&self) -> bool {
        self.funcs.iter().any(|f| f.scopes.iter().any(|s| s.kind == ScopeKind::Outer && s.dynamic))
    }

    // ----- loading and storing bindings ------------------------------

    /// acc = the value of `name` (with TDZ check if needed).
    pub fn load_name(&mut self, name: &str, pos: Pos) -> CResult<()> {
        match self.resolve(name) {
            Resolved::Reg(r, b) => {
                self.emit(Op::Ldar(r));
                if self.needs_tdz(&b, pos, false) {
                    let k = self.name_const(display_name(&b.name));
                    self.emit(Op::CheckInit(r, k));
                }
            }
            Resolved::Env(h, slot, b, crossed) => {
                if self.needs_tdz(&b, pos, crossed) {
                    let k = self.name_const(display_name(&b.name));
                    self.emit(Op::LdaEnvChecked(h, slot, k));
                } else {
                    self.emit(Op::LdaEnv(h, slot));
                }
            }
            Resolved::Global => {
                let k = self.name_const(name);
                self.emit(Op::LdaGlobal(k));
            }
            Resolved::Dynamic => {
                let k = self.name_const(name);
                self.emit(Op::LdaName(k));
            }
        }
        Ok(())
    }

    fn needs_tdz(&self, b: &Binding, pos: Pos, crossed: bool) -> bool {
        if !b.tdz {
            return false;
        }
        crossed || b.in_switch || pos.offset < b.init_end || self.in_loop_body_before(b)
    }

    fn in_loop_body_before(&self, _b: &Binding) -> bool {
        false
    }

    /// Stores acc into `name` (assignment: const/TDZ checks). acc is kept.
    pub fn store_name(&mut self, name: &str, pos: Pos) -> CResult<()> {
        let strict = self.fr().strict;
        match self.resolve(name) {
            Resolved::Reg(r, b) => match b.kind {
                BKind::Const | BKind::ClassName => {
                    if self.needs_tdz(&b, pos, false) {
                        let k = self.name_const(display_name(&b.name));
                        self.emit(Op::CheckInit(r, k));
                    }
                    let k = self.name_const(&b.name);
                    self.emit(Op::ThrowConstAssign(k));
                }
                BKind::FnName => {
                    if strict {
                        let k = self.name_const(&b.name);
                        self.emit(Op::ThrowConstAssign(k));
                    }
                }
                _ => {
                    if self.needs_tdz(&b, pos, false) {
                        let k = self.name_const(display_name(&b.name));
                        self.emit(Op::CheckInit(r, k));
                    }
                    self.emit(Op::Star(r));
                }
            },
            Resolved::Env(h, slot, b, crossed) => match b.kind {
                BKind::Const | BKind::ClassName => {
                    if self.needs_tdz(&b, pos, crossed) {
                        let r = self.alloc_reg();
                        self.emit(Op::Star(r));
                        let k = self.name_const(display_name(&b.name));
                        self.emit(Op::LdaEnvChecked(h, slot, k));
                        self.emit(Op::Ldar(r));
                        self.free_to(r);
                    }
                    let k = self.name_const(&b.name);
                    self.emit(Op::ThrowConstAssign(k));
                }
                BKind::FnName => {
                    if strict {
                        let k = self.name_const(&b.name);
                        self.emit(Op::ThrowConstAssign(k));
                    }
                }
                _ => {
                    if self.needs_tdz(&b, pos, crossed) {
                        let k = self.name_const(display_name(&b.name));
                        self.emit(Op::StaEnvChecked(h, slot, k));
                    } else {
                        self.emit(Op::StaEnv(h, slot));
                    }
                }
            },
            Resolved::Global => {
                let k = self.name_const(name);
                self.emit(Op::StaGlobal(k, strict));
            }
            Resolved::Dynamic => {
                let k = self.name_const(name);
                self.emit(Op::StaName(k, strict));
            }
        }
        Ok(())
    }

    /// Initializes a binding declared in the current function (no checks).
    pub fn init_binding(&mut self, name: &str) {
        match self.resolve(name) {
            Resolved::Reg(r, _) => {
                self.emit(Op::Star(r));
            }
            Resolved::Env(h, slot, _, _) => {
                self.emit(Op::StaEnv(h, slot));
            }
            Resolved::Global => {
                let k = self.name_const(name);
                // Script top-level lexical or var.
                self.emit(Op::InitGlobalLex(k));
            }
            Resolved::Dynamic => {
                let k = self.name_const(name);
                self.emit(Op::StaName(k, false));
            }
        }
    }

    /// Sets a lexical binding to "uninitialized" (TDZ start).
    pub fn set_uninit(&mut self, name: &str) {
        match self.resolve(name) {
            Resolved::Reg(r, _) => {
                self.emit(Op::LdaUninit);
                self.emit(Op::Star(r));
            }
            Resolved::Env(h, slot, _, _) => {
                self.emit(Op::LdaUninit);
                self.emit(Op::StaEnv(h, slot));
            }
            _ => {}
        }
    }

    /// Mark a binding initialized from here on (for TDZ elision).
    pub fn set_init_end(&mut self, name: &str, end: u32) {
        let f = self.f();
        for s in f.scopes.iter_mut().rev() {
            if let Some(b) = s.bindings.iter_mut().find(|b| &*b.name == name) {
                b.init_end = end;
                return;
            }
        }
    }

    // ----- entry points ----------------------------------------------

    fn new_func(&self, kind: FuncKind, strict: bool, is_async: bool, is_generator: bool) -> FuncState<'a> {
        FuncState {
            ops: Vec::new(),
            consts: Vec::new(),
            children: Vec::new(),
            lines: Vec::new(),
            next_reg: 0,
            nregs: 0,
            scopes: Vec::new(),
            env_depth: 0,
            ctx: Vec::new(),
            tries: Vec::new(),
            handlers: Vec::new(),
            const_cache: BTreeMap::new(),
            strict,
            kind,
            is_async,
            is_generator,
            is_arrow: kind == FuncKind::Arrow,
            pending_labels: Vec::new(),
            derived: kind == FuncKind::DerivedConstructor,
            template_sites: 0,
            named_envs: false,
        }
    }

    fn finish(&mut self, name: Value, nparams: u16, length: u16, keep_args: bool, has_fields: bool, src: (u32, u32)) -> Rc<Code> {
        let f = self.funcs.pop().unwrap();
        // Handlers from the try regions, innermost first (they were
        // finished inner-first).
        Rc::new(Code {
            ops: f.ops,
            consts: f.consts,
            children: f.children,
            handlers: f.handlers,
            nregs: f.nregs.max(nparams),
            nparams,
            length,
            name,
            kind: f.kind,
            strict: f.strict,
            is_async: f.is_async,
            is_generator: f.is_generator,
            keep_args,
            has_fields,
            lines: f.lines,
            source: self.source.clone(),
            src_start: src.0,
            src_end: src.1,
            calls: Cell::new(0),
        })
    }

    /// Compiles a script (or indirect eval).
    pub fn script(&mut self, prog: &'a Program, eval: bool) -> CResult<Rc<Code>> {
        let kind = if eval { FuncKind::Eval } else { FuncKind::Script };
        self.funcs.push(self.new_func(kind, prog.strict, false, false));
        self.push_scope(ScopeKind::Script);
        let region = self.scan.region_stmts(&prog.body);
        self.check_lexical_conflicts(&prog.body, &[])?;
        // Global declarations.
        let mut decls = GlobalDecls { eval, strict: prog.strict, ..Default::default() };
        let mut vars = Vec::new();
        scan::var_declared_names(&prog.body, &mut vars);
        let mut fn_decls: Vec<&'a Rc<Function>> = Vec::new();
        let mut lexicals: Vec<(Name, bool)> = Vec::new();
        for s in &prog.body {
            match &s.kind {
                StmtKind::Function(f) => fn_decls.push(f),
                StmtKind::Var(k @ (VarKind::Let | VarKind::Const), ds) => {
                    for d in ds {
                        let mut v = Vec::new();
                        crate::parser::bound_names(&d.target, &mut v);
                        for (n, _) in v {
                            lexicals.push((n, *k == VarKind::Const));
                        }
                    }
                }
                StmtKind::Class(c) => lexicals.push((c.name.clone().unwrap(), false)),
                _ => {}
            }
        }
        // Annex B: block-level functions in sloppy scripts also create vars.
        let annex_b = if prog.strict { Vec::new() } else { self.annex_b_functions(&prog.body, &lexicals.iter().map(|l| l.0.clone()).collect::<Vec<_>>()) };
        collect_annex_b_sites(&prog.body, &annex_b, &mut self.annex_b_sites.borrow_mut());
        for n in &annex_b {
            if !vars.contains(n) && !fn_decls.iter().any(|f| f.name.as_ref() == Some(n)) {
                vars.push(n.clone());
            }
        }
        // Strict eval: its vars and functions are local.
        let local_vars = eval && prog.strict;
        if local_vars {
            for n in &vars {
                self.declare(n.clone(), BKind::Var, true, false, 0);
            }
            for f in &fn_decls {
                self.declare(f.name.clone().unwrap(), BKind::Var, true, false, 0);
            }
        }
        // Eval code: lexical declarations are local to the eval.
        if eval {
            for (n, c) in &lexicals {
                let kind = if *c { BKind::Const } else { BKind::Let };
                self.declare(n.clone(), kind, true, true, u32::MAX);
            }
        }
        self.declare_pseudo_script(&region);
        self.open_env();
        if !local_vars {
            for n in &vars {
                decls.vars.push(self.atom(n));
            }
        }
        if !eval {
            for (n, c) in &lexicals {
                let a = self.atom(n);
                decls.lexicals.push((a, *c));
            }
        }
        // Function declarations are created by DeclareGlobals (they close
        // over the script environment).
        let mut fn_children = Vec::new();
        for f in fn_decls.iter().rev() {
            let n = f.name.clone().unwrap();
            if fn_children.iter().any(|(m, _): &(Name, u32)| *m == n) {
                continue;
            }
            let idx = self.function_child(f, None)?;
            fn_children.push((n, idx));
        }
        fn_children.reverse();
        if local_vars {
            for (n, idx) in &fn_children {
                self.emit(Op::Closure(*idx));
                self.init_binding(n);
            }
        } else {
            for (n, idx) in &fn_children {
                let a = self.atom(n);
                decls.functions.push((a, *idx));
            }
        }
        let k = self.konst(Const::Globals(Rc::new(decls)));
        self.emit(Op::DeclareGlobals(k));
        if eval {
            for (n, _) in &lexicals {
                self.set_uninit(n);
            }
        }
        // The completion value of the script is kept in a register.
        let result = self.alloc_reg();
        self.emit(Op::LdaUndef);
        self.emit(Op::Star(result));
        self.f().ctx.clear();
        for s in &prog.body {
            self.stmt_completion(s, Some(result))?;
        }
        self.emit(Op::Ldar(result));
        self.emit(Op::Return);
        self.pop_scope_no_emit();
        let name = Value::str(self.vm.names.empty);
        Ok(self.finish(name, 0, 0, false, false, (0, prog.end)))
    }

    /// Ops were inserted at index 0: shift labels by one.
    fn shift_for_insert(&mut self) {
        let f = self.f();
        for op in f.ops.iter_mut().skip(1) {
            *op = match *op {
                Op::Jmp(l) => Op::Jmp(l + 1),
                Op::JmpTrue(l) => Op::JmpTrue(l + 1),
                Op::JmpFalse(l) => Op::JmpFalse(l + 1),
                Op::JmpNullish(l) => Op::JmpNullish(l + 1),
                Op::JmpNotNullish(l) => Op::JmpNotNullish(l + 1),
                Op::JmpUndefined(l) => Op::JmpUndefined(l + 1),
                Op::JmpNotUndefined(l) => Op::JmpNotUndefined(l + 1),
                Op::JmpStrictEq(r, l) => Op::JmpStrictEq(r, l + 1),
                Op::JmpIfIntEq(r, i, l) => Op::JmpIfIntEq(r, i, l + 1),
                Op::IteratorStep(r, l) => Op::IteratorStep(r, l + 1),
                Op::ForInNext(r, l) => Op::ForInNext(r, l + 1),
                other => other,
            };
        }
        for s in f.scopes.iter_mut() {
            if let Some(p) = s.push_at.as_mut() {
                *p += 1;
            }
        }
        for h in f.handlers.iter_mut() {
            h.start += 1;
            h.end += 1;
            h.target += 1;
        }
        for l in f.lines.iter_mut() {
            l.0 += 1;
        }
    }

    fn pop_scope_no_emit(&mut self) {
        let s = self.f().scopes.pop().unwrap();
        if let Some(at) = s.push_at {
            let names_k = match self.fr().ops[at] {
                Op::PushEnv { names, .. } => names,
                _ => u32::MAX,
            };
            if names_k != u32::MAX {
                self.f().consts[names_k as usize] = Const::Names(s.names.clone().into());
            }
            self.f().ops[at] = Op::PushEnv { n: s.env_slots, names: names_k };
        }
    }

    /// Pseudo-bindings of the script level used by arrows (`this`).
    fn declare_pseudo_script(&mut self, region: &Region) {
        if region.this || region.dynamic {
            self.declare("this".into(), BKind::Pseudo, true, false, 0);
            self.open_env();
            self.emit(Op::LdaThis);
            self.init_binding("this");
        }
    }

    /// Compiles a direct eval inside a caller described by `scope`.
    pub fn direct_eval(&mut self, prog: &'a Program, scope: &EvalScope) -> CResult<Rc<Code>> {
        // Outer scopes from the description, outermost first.
        self.funcs.push(self.new_func(FuncKind::Eval, prog.strict || scope.strict, false, false));
        for (i, level) in scope.levels.iter().enumerate().rev() {
            match level {
                None => self.push_scope(ScopeKind::With),
                Some(names) => {
                    self.push_scope(ScopeKind::Outer);
                    let s = self.f().scopes.last_mut().unwrap();
                    s.has_env = true;
                    s.dynamic = scope.var_scope_level == Some(i as u16);
                    for (n, slot, kind) in names.iter() {
                        let kind = match kind {
                            BindingKind::Var => BKind::Var,
                            BindingKind::Let => BKind::Let,
                            BindingKind::Const => BKind::Const,
                            BindingKind::FnName => BKind::FnName,
                        };
                        let tdz = matches!(kind, BKind::Let | BKind::Const);
                        s.bindings.push(Binding { name: n.as_str().into(), kind, loc: Loc::Env(*slot), tdz, init_end: u32::MAX, in_switch: true });
                    }
                }
            }
        }
        let f = self.f();
        f.env_depth = 0;
        let strict = prog.strict || scope.strict;
        self.f().strict = strict;
        // Then like a script, but vars go to the caller's var scope.
        let code = self.eval_body(prog, scope, strict)?;
        Ok(code)
    }

    fn eval_body(&mut self, prog: &'a Program, scope: &EvalScope, strict: bool) -> CResult<Rc<Code>> {
        self.push_scope(ScopeKind::Script);
        let mut vars = Vec::new();
        scan::var_declared_names(&prog.body, &mut vars);
        let mut fn_decls: Vec<&'a Rc<Function>> = Vec::new();
        let mut lexicals: Vec<(Name, bool)> = Vec::new();
        for s in &prog.body {
            match &s.kind {
                StmtKind::Function(f) => fn_decls.push(f),
                StmtKind::Var(k @ (VarKind::Let | VarKind::Const), ds) => {
                    for d in ds {
                        let mut v = Vec::new();
                        crate::parser::bound_names(&d.target, &mut v);
                        for (n, _) in v {
                            lexicals.push((n, *k == VarKind::Const));
                        }
                    }
                }
                StmtKind::Class(c) => lexicals.push((c.name.clone().unwrap(), false)),
                _ => {}
            }
        }
        if !strict {
            let annex = self.annex_b_functions(&prog.body, &lexicals.iter().map(|l| l.0.clone()).collect::<Vec<_>>());
            collect_annex_b_sites(&prog.body, &annex, &mut self.annex_b_sites.borrow_mut());
            for n in annex {
                if !vars.contains(&n) {
                    vars.push(n);
                }
            }
        }
        // Strict eval: everything local.
        for (n, c) in &lexicals {
            let kind = if *c { BKind::Const } else { BKind::Let };
            self.declare(n.clone(), kind, true, true, u32::MAX);
        }
        if strict {
            for n in &vars {
                self.declare(n.clone(), BKind::Var, true, false, 0);
            }
            for f in &fn_decls {
                self.declare(f.name.clone().unwrap(), BKind::Var, true, false, 0);
            }
        }
        self.f().scopes.last_mut().unwrap().dynamic = true;
        self.open_env();
        let own_env = self.fr().scopes.last().unwrap().has_env as u16;
        let mut decls = GlobalDecls { eval: true, strict, ..Default::default() };
        if !strict {
            decls.var_env_hops = scope.var_scope_level.map(|l| l + own_env);
            // A var may not hoist over a lexical binding of the same name.
            for n in vars.iter().chain(fn_decls.iter().map(|f| f.name.as_ref().unwrap())) {
                for sc in self.fr().scopes.iter().rev().skip(1) {
                    if let Some(b) = sc.bindings.iter().find(|b| b.name == *n) {
                        if matches!(b.kind, BKind::Let | BKind::Const | BKind::ClassName) {
                            return err(&format!("Identifier '{n}' has already been declared"), Pos::default());
                        }
                    }
                    if sc.dynamic {
                        break;
                    }
                }
            }
        }
        if !strict {
            for n in &vars {
                decls.vars.push(self.atom(n));
            }
        }
        let mut fn_children = Vec::new();
        for f in fn_decls.iter().rev() {
            let n = f.name.clone().unwrap();
            if fn_children.iter().any(|(m, _): &(Name, u32)| *m == n) {
                continue;
            }
            let idx = self.function_child(f, None)?;
            fn_children.push((n, idx));
        }
        fn_children.reverse();
        if strict {
            for (n, idx) in &fn_children {
                self.emit(Op::Closure(*idx));
                self.init_binding(n);
            }
        } else {
            for (n, idx) in &fn_children {
                let a = self.atom(n);
                decls.functions.push((a, *idx));
            }
        }
        let _ = scope;
        let k = self.konst(Const::Globals(Rc::new(decls)));
        self.emit(Op::DeclareGlobals(k));
        for (n, _) in &lexicals {
            self.set_uninit(n);
        }
        let result = self.alloc_reg();
        self.emit(Op::LdaUndef);
        self.emit(Op::Star(result));
        for s in &prog.body {
            self.stmt_completion(s, Some(result))?;
        }
        self.emit(Op::Ldar(result));
        self.emit(Op::Return);
        self.pop_scope_no_emit();
        let name = Value::str(self.vm.names.empty);
        Ok(self.finish(name, 0, 0, false, false, (0, prog.end)))
    }

    /// Function names declared in blocks that Annex B.3.3 also binds as
    /// vars (sloppy mode), unless a lexical binding would conflict.
    fn annex_b_functions(&self, body: &[Stmt], top_lexical: &[Name]) -> Vec<Name> {
        let mut out = Vec::new();
        let mut blocked: Vec<Name> = top_lexical.to_vec();
        for s in body {
            annex_b_stmt(s, &mut blocked, &mut out, false);
        }
        out
    }

    /// A function created by the Function constructor (global scope).
    pub fn toplevel_function(&mut self, f: &'a Rc<Function>) -> CResult<Rc<Code>> {
        self.funcs.push(self.new_func(FuncKind::Script, false, false, false));
        self.push_scope(ScopeKind::Script);
        self.fn_expr_flag.set(false);
        let anon = self.atom("anonymous");
        let r = self.function(f, Some(anon), None);
        self.funcs.pop();
        r
    }

    /// A child function: compiles it and returns its index.
    pub fn function_child(&mut self, f: &'a Rc<Function>, name_hint: Option<Value>) -> CResult<u32> {
        let code = self.function(f, name_hint, None)?;
        let fs = self.f();
        fs.children.push(code);
        Ok((fs.children.len() - 1) as u32)
    }

    /// Compiles a function body (FunctionDeclarationInstantiation + body).
    pub fn function(&mut self, f: &'a Rc<Function>, name_hint: Option<Value>, class_info: Option<&ClassCtorInfo>) -> CResult<Rc<Code>> {
        let kind = match f.kind {
            FnKind::Arrow => FuncKind::Arrow,
            FnKind::Constructor { derived: false } => FuncKind::ClassConstructor,
            FnKind::Constructor { derived: true } => FuncKind::DerivedConstructor,
            FnKind::Method | FnKind::Getter | FnKind::Setter | FnKind::ClassInit => FuncKind::Method,
            FnKind::Normal => FuncKind::Normal,
        };
        let is_expr = self.fn_expr_flag.replace(false);
        let info = self.scan.function(f);
        let mut region = Region::default();
        for p in &f.params {
            self.scan.pattern(&p.target, &mut region);
            if let Some(d) = &p.default {
                self.scan.expr(d, &mut region);
            }
        }
        if let Some(r) = &f.rest {
            self.scan.pattern(r, &mut region);
        }
        self.scan.stmts(&f.body, &mut region);
        let dynamic = region.dynamic || region.has_direct_eval;
        let sloppy_eval = region.has_direct_eval && !f.strict;
        let mut fs = self.new_func(kind, f.strict, f.is_async, f.is_generator);
        fs.derived = kind == FuncKind::DerivedConstructor;
        fs.named_envs = dynamic;
        self.funcs.push(fs);
        let is_arrow = kind == FuncKind::Arrow;
        let captured = |n: &str| dynamic || region.captured.contains(n);

        // Parameters arrive in registers 0..nparams.
        let nparams = f.params.len() as u16;
        let r0 = self.alloc_regs(nparams);
        debug_assert_eq!(r0, 0);
        let length = f.params.iter().take_while(|p| p.default.is_none()).count() as u16;
        let length = if f.rest.is_some() { f.params.iter().take_while(|p| p.default.is_none()).count() as u16 } else { length };
        let simple = f.simple_params;

        // Function-expression name scope (an outer scope of the function).
        let has_name_scope = matches!(f.kind, FnKind::Normal) && f.name.is_some() && is_expr;
        if has_name_scope {
            self.push_scope(ScopeKind::Block);
            let n = f.name.clone().unwrap();
            let cap = captured(&n);
            self.declare(n.clone(), BKind::FnName, cap, false, 0);
            self.open_env();
            self.emit(Op::LdaCallee);
            self.init_binding(&n);
        }

        self.push_scope(ScopeKind::Function);
        self.f().scopes.last_mut().unwrap().dynamic = sloppy_eval;
        // Parameter names.
        let mut param_names: Vec<Name> = Vec::new();
        for p in &f.params {
            let mut v = Vec::new();
            crate::parser::bound_names(&p.target, &mut v);
            param_names.extend(v.into_iter().map(|(n, _)| n));
        }
        if let Some(r) = &f.rest {
            let mut v = Vec::new();
            crate::parser::bound_names(r, &mut v);
            param_names.extend(v.into_iter().map(|(n, _)| n));
        }
        let mut var_names = Vec::new();
        scan::var_declared_names(&f.body, &mut var_names);
        let mut top_fns: Vec<&'a Rc<Function>> = Vec::new();
        let mut top_lex: Vec<(Name, VarKind)> = Vec::new();
        for s in &f.body {
            match &s.kind {
                StmtKind::Function(g) => top_fns.push(g),
                StmtKind::Var(k @ (VarKind::Let | VarKind::Const | VarKind::Using | VarKind::AwaitUsing), ds) => {
                    for d in ds {
                        let mut v = Vec::new();
                        crate::parser::bound_names(&d.target, &mut v);
                        for (n, _) in v {
                            top_lex.push((n, *k));
                        }
                    }
                }
                StmtKind::Class(c) => top_lex.push((c.name.clone().unwrap(), VarKind::Let)),
                _ => {}
            }
        }
        self.check_lexical_conflicts(&f.body, &param_names)?;
        let lex_names: Vec<Name> = top_lex.iter().map(|l| l.0.clone()).collect();
        let annex_b: Vec<Name> = if f.strict {
            Vec::new()
        } else {
            self.annex_b_functions(&f.body, &lex_names).into_iter().filter(|n| !param_names.contains(n) && &**n != "arguments" || var_names.contains(n)).collect()
        };
        collect_annex_b_sites(&f.body, &annex_b, &mut self.annex_b_sites.borrow_mut());
        let fn_names: Vec<Name> = top_fns.iter().map(|g| g.name.clone().unwrap()).collect();
        let arguments_needed = !is_arrow
            && (region.uses_arguments || region.arguments || dynamic)
            && !param_names.iter().any(|n| &**n == "arguments")
            && !(fn_names.iter().any(|n| &**n == "arguments") || lex_names.iter().any(|n| &**n == "arguments"));
        let mapped = arguments_needed && !f.strict && simple;
        // Params.
        let params_captured = mapped;
        for (i, n) in param_names.iter().enumerate() {
            let cap = params_captured || captured(n);
            let tdz = !simple;
            if !cap && simple && i < nparams as usize {
                // The argument register itself (last duplicate wins).
                let s = self.f().scopes.last_mut().unwrap();
                if let Some(b) = s.bindings.iter_mut().find(|b| b.name == *n) {
                    b.loc = Loc::Reg(i as Reg);
                } else {
                    s.bindings.push(Binding { name: n.clone(), kind: BKind::Param, loc: Loc::Reg(i as Reg), tdz: false, init_end: 0, in_switch: false });
                }
            } else {
                self.declare(n.clone(), BKind::Param, cap, tdz, 0);
            }
        }
        // Pseudo-bindings.
        let this_binding = !is_arrow && (kind == FuncKind::DerivedConstructor || region.this || dynamic);
        if this_binding {
            let cap = region.this || dynamic;
            self.declare("this".into(), BKind::Pseudo, cap, kind == FuncKind::DerivedConstructor, if kind == FuncKind::DerivedConstructor { u32::MAX } else { 0 });
        }
        if !is_arrow && (region.new_target || dynamic) {
            self.declare("new.target".into(), BKind::Pseudo, true, false, 0);
        }
        if !is_arrow && kind == FuncKind::DerivedConstructor && (region.super_call || dynamic) {
            self.declare("%fn".into(), BKind::Pseudo, true, false, 0);
        }
        if !is_arrow && (region.super_prop || dynamic) && matches!(kind, FuncKind::Method | FuncKind::ClassConstructor | FuncKind::DerivedConstructor) {
            self.declare("%home".into(), BKind::Pseudo, true, false, 0);
        }
        if arguments_needed {
            let cap = captured("arguments");
            self.declare("arguments".into(), BKind::Var, cap, false, 0);
        }
        // Body var scope: separate when parameters have expressions.
        let separate_vars = !simple;
        if !separate_vars {
            for n in var_names.iter().chain(fn_names.iter()).chain(annex_b.iter()) {
                if param_names.contains(n) || (&**n == "arguments" && arguments_needed) {
                    continue;
                }
                let cap = captured(n);
                self.declare(n.clone(), BKind::Var, cap, false, 0);
            }
            for (n, k) in &top_lex {
                let cap = captured(n);
                let bk = if matches!(k, VarKind::Const | VarKind::Using | VarKind::AwaitUsing) { BKind::Const } else { BKind::Let };
                self.declare(n.clone(), bk, cap, true, u32::MAX);
            }
        }
        self.open_env();
        // Initialize pseudo-bindings.
        if this_binding {
            if kind == FuncKind::DerivedConstructor {
                self.emit(Op::LdaUninit);
            } else {
                self.emit(Op::LdaThis);
            }
            self.init_binding("this");
        }
        if !is_arrow && (region.new_target || dynamic) {
            self.emit(Op::LdaNewTarget);
            self.init_binding("new.target");
        }
        if !is_arrow && kind == FuncKind::DerivedConstructor && (region.super_call || dynamic) {
            self.emit(Op::LdaCallee);
            self.init_binding("%fn");
        }
        if !is_arrow && (region.super_prop || dynamic) && matches!(kind, FuncKind::Method | FuncKind::ClassConstructor | FuncKind::DerivedConstructor) {
            self.emit(Op::LdaCallee);
            self.init_binding("%home");
        }
        let keep_args = arguments_needed || f.rest.is_some();
        // Simple captured params: copy from the argument registers.
        if simple {
            for (i, n) in param_names.iter().enumerate() {
                if let Resolved::Env(..) = self.resolve(n) {
                    // Last duplicate wins: copy only from its last position.
                    if param_names[i + 1..].contains(n) {
                        continue;
                    }
                    self.emit(Op::Ldar(i as Reg));
                    self.init_binding(n);
                }
            }
        }
        if arguments_needed {
            let map_k = if mapped {
                let mut map: Vec<Value> = Vec::new();
                for (i, n) in param_names.iter().enumerate() {
                    let last = !param_names[i + 1..].contains(n);
                    match self.resolve(n) {
                        Resolved::Env(0, slot, _, _) if last => map.push(Value::int(slot as i32)),
                        _ => map.push(Value::int(-1)),
                    }
                }
                self.konst(Const::Names(map.into()))
            } else {
                u32::MAX
            };
            self.emit(Op::CreateArguments(map_k));
            self.init_binding("arguments");
        }
        // Non-simple parameters: evaluate in order.
        if !simple {
            for (n, _) in param_names.iter().enumerate().map(|(i, n)| (n, i)) {
                self.set_uninit(n);
            }
            for (i, p) in f.params.iter().enumerate() {
                self.emit(Op::Ldar(i as Reg));
                if let Some(d) = &p.default {
                    let skip = self.emit(Op::JmpNotUndefined(0));
                    self.named_expr(d, &p.target)?;
                    self.patch_here(skip);
                }
                self.bind_pattern(&p.target, true)?;
            }
            if let Some(r) = &f.rest {
                self.emit(Op::CreateRest(nparams));
                self.bind_pattern(r, true)?;
            }
        } else if let Some(r) = &f.rest {
            self.emit(Op::CreateRest(nparams));
            self.bind_pattern(r, true)?;
        }
        if separate_vars {
            self.push_scope(ScopeKind::Block);
            self.f().scopes.last_mut().unwrap().dynamic = sloppy_eval;
            for n in var_names.iter().chain(fn_names.iter()).chain(annex_b.iter()) {
                if &**n == "arguments" && arguments_needed && !var_names.contains(n) {
                    continue;
                }
                let cap = captured(n);
                self.declare(n.clone(), BKind::Var, cap, false, 0);
            }
            for (n, k) in &top_lex {
                let cap = captured(n);
                let bk = if matches!(k, VarKind::Const | VarKind::Using | VarKind::AwaitUsing) { BKind::Const } else { BKind::Let };
                self.declare(n.clone(), bk, cap, true, u32::MAX);
            }
            self.open_env();
            // Vars named like params start with the param's value.
            for n in &var_names {
                if param_names.contains(n) || &**n == "arguments" {
                    self.load_outer_binding(n)?;
                    self.init_binding(n);
                }
            }
        }
        // TDZ for top-level lexicals.
        for (n, _) in &top_lex {
            self.set_uninit(n);
        }
        // Hoisted function declarations (last one wins).
        let mut seen: Vec<Name> = Vec::new();
        for g in top_fns.iter().rev() {
            let n = g.name.clone().unwrap();
            if seen.contains(&n) {
                continue;
            }
            seen.push(n.clone());
            let idx = self.function_child(g, None)?;
            self.emit(Op::Closure(idx));
            self.init_binding(&n);
        }
        // Base class constructor: fields at entry.
        let mut has_fields = false;
        if let Some(ci) = class_info {
            if !ci.derived && ci.has_instance_fields {
                has_fields = true;
                let r = self.alloc_reg();
                self.emit(Op::LdaCallee);
                self.emit(Op::Star(r));
                self.emit(Op::LdaThis);
                self.emit(Op::InitFieldsOf(r));
                self.free_to(r);
            }
        }
        if f.is_generator {
            self.emit(Op::GeneratorStart);
        }
        // Body.
        for s in &f.body {
            self.stmt(s)?;
        }
        // Implicit return.
        self.emit_return_undefined()?;
        if separate_vars {
            self.pop_scope_no_emit();
        }
        self.pop_scope_no_emit();
        if has_name_scope {
            self.pop_scope_no_emit();
        }
        let name = match name_hint {
            Some(n) => n,
            None => match &f.name {
                Some(n) => self.atom(n),
                None => Value::str(self.vm.names.empty),
            },
        };
        let _ = info;
        Ok(self.finish(name, nparams, length, keep_args, has_fields, (f.start, f.end)))
    }

    fn is_fn_expression(&self, _f: &Rc<Function>) -> bool {
        self.fn_expr_flag.replace(false)
    }

    /// Loads a binding from the scope outside the innermost one.
    fn load_outer_binding(&mut self, name: &str) -> CResult<()> {
        let f = self.f();
        let inner = f.scopes.pop().unwrap();
        let r = self.load_name(name, Pos::default());
        self.f().scopes.push(inner);
        // Hop count is off by one if the inner scope has an env: fix up.
        if self.fr().scopes.last().unwrap().has_env {
            let at = self.fr().ops.len() - 1;
            if let Op::LdaEnv(h, s) = self.fr().ops[at] {
                self.f().ops[at] = Op::LdaEnv(h + 1, s);
            }
        }
        r
    }

    /// `return undefined` (or `this` in a derived constructor).
    fn emit_return_undefined(&mut self) -> CResult<()> {
        if self.fr().derived {
            self.load_this_checked()?;
        } else {
            self.emit(Op::LdaUndef);
        }
        self.emit(Op::Return);
        Ok(())
    }

    /// acc = `this` (with the derived-constructor check).
    pub fn load_this_checked(&mut self) -> CResult<()> {
        match self.resolve("this") {
            Resolved::Reg(r, b) => {
                self.emit(Op::Ldar(r));
                if b.tdz {
                    let k = self.name_const("this");
                    self.emit(Op::CheckInit(r, k));
                }
            }
            Resolved::Env(h, slot, b, _) => {
                if b.tdz {
                    let k = self.name_const("this");
                    self.emit(Op::LdaEnvChecked(h, slot, k));
                } else {
                    self.emit(Op::LdaEnv(h, slot));
                }
            }
            _ => {
                self.emit(Op::LdaThis);
            }
        }
        Ok(())
    }

    // ----- early errors needing scopes --------------------------------

    /// Duplicate lexical declarations and lexical/var conflicts in a
    /// statement list (function body, script or block).
    pub fn check_lexical_conflicts(&self, body: &[Stmt], params: &[Name]) -> CResult<()> {
        let mut lex: Vec<(Name, Pos)> = Vec::new();
        let strict = self.funcs.last().is_some_and(|f| f.strict);
        let mut fn_names: Vec<Name> = Vec::new();
        for s in body {
            match &s.kind {
                StmtKind::Var(VarKind::Let | VarKind::Const | VarKind::Using | VarKind::AwaitUsing, ds) => {
                    for d in ds {
                        crate::parser::bound_names(&d.target, &mut lex);
                    }
                }
                StmtKind::Class(c) => lex.push((c.name.clone().unwrap(), s.pos)),
                StmtKind::Export(Export::Decl(d)) => match &d.kind {
                    StmtKind::Var(VarKind::Let | VarKind::Const, ds) => {
                        for dd in ds {
                            crate::parser::bound_names(&dd.target, &mut lex);
                        }
                    }
                    StmtKind::Class(c) => lex.push((c.name.clone().unwrap(), s.pos)),
                    _ => {}
                },
                StmtKind::Export(Export::DefaultClass(c)) => {
                    if let Some(n) = &c.name {
                        lex.push((n.clone(), s.pos));
                    }
                }
                _ => {}
            }
            if let StmtKind::Function(f) = &s.kind {
                fn_names.push(f.name.clone().unwrap());
            }
        }
        for (i, (n, p)) in lex.iter().enumerate() {
            if lex[..i].iter().any(|(m, _)| m == n) || fn_names.contains(n) {
                return err(&format!("Identifier '{n}' has already been declared"), *p);
            }
            if params.contains(n) {
                return err(&format!("Identifier '{n}' has already been declared"), *p);
            }
        }
        let mut vars = Vec::new();
        scan::var_declared_names(body, &mut vars);
        for (n, p) in &lex {
            if vars.contains(n) {
                return err(&format!("Identifier '{n}' has already been declared"), *p);
            }
        }
        let _ = strict;
        Ok(())
    }

    /// Block-level checks: duplicate lexical names (functions count as
    /// lexical in blocks; sloppy duplicates of plain functions are allowed).
    pub fn check_block_conflicts(&self, body: &[Stmt], extra: &[Name]) -> CResult<()> {
        let strict = self.fr().strict;
        let mut lex: Vec<(Name, Pos, bool)> = Vec::new();
        for s in body {
            match &s.kind {
                StmtKind::Var(VarKind::Let | VarKind::Const | VarKind::Using | VarKind::AwaitUsing, ds) => {
                    let mut v = Vec::new();
                    for d in ds {
                        crate::parser::bound_names(&d.target, &mut v);
                    }
                    lex.extend(v.into_iter().map(|(n, p)| (n, p, false)));
                }
                StmtKind::Class(c) => lex.push((c.name.clone().unwrap(), s.pos, false)),
                StmtKind::Function(f) => {
                    let plain = !f.is_async && !f.is_generator;
                    lex.push((f.name.clone().unwrap(), s.pos, plain));
                }
                _ => {}
            }
        }
        for (i, (n, p, plain)) in lex.iter().enumerate() {
            for (m, _, plain2) in &lex[..i] {
                if m == n && !(!strict && *plain && *plain2) {
                    return err(&format!("Identifier '{n}' has already been declared"), *p);
                }
            }
            if extra.contains(n) {
                return err(&format!("Identifier '{n}' has already been declared"), *p);
            }
        }
        let mut vars = Vec::new();
        scan::var_declared_names(body, &mut vars);
        for (n, p, _) in &lex {
            if vars.contains(n) {
                return err(&format!("Identifier '{n}' has already been declared"), *p);
            }
        }
        Ok(())
    }

    // ----- statements ---------------------------------------------------

    pub fn stmt(&mut self, s: &'a Stmt) -> CResult<()> {
        self.stmt_completion(s, None)
    }

    /// Compiles a statement; with `result`, expression statement values are
    /// stored there (script completion value).
    pub fn stmt_completion(&mut self, s: &'a Stmt, result: Option<Reg>) -> CResult<()> {
        self.pos(s.pos);
        match &s.kind {
            StmtKind::Expr(e) => {
                self.expr(e)?;
                if let Some(r) = result {
                    self.emit(Op::Star(r));
                }
            }
            StmtKind::Var(kind, decls) => self.var_decl(*kind, decls, s.pos)?,
            StmtKind::Function(f) => {
                // Hoisted; Annex B: copy a block function to its var.
                if !self.fr().strict && self.is_annex_b_site(f) {
                    let n = f.name.clone().unwrap();
                    self.load_name(&n, s.pos)?;
                    self.store_annex_b_var(&n)?;
                }
            }
            StmtKind::Class(c) => {
                self.class(c, None)?;
                let n = c.name.clone().unwrap();
                self.init_binding(&n);
                self.set_init_end(&n, 0);
            }
            StmtKind::Return(e) => {
                match e {
                    Some(e) => self.expr(e)?,
                    None => {
                        self.emit(Op::LdaUndef);
                    }
                }
                self.emit_return(e.as_ref().map(|e| e.pos))?;
            }
            StmtKind::If(t, a, b) => {
                if let Some(r) = result {
                    self.emit(Op::LdaUndef);
                    self.emit(Op::Star(r));
                }
                self.expr(t)?;
                let j = self.emit(Op::JmpFalse(0));
                self.stmt_completion(a, result)?;
                if let Some(b) = b {
                    let j2 = self.emit(Op::Jmp(0));
                    self.patch_here(j);
                    self.stmt_completion(b, result)?;
                    self.patch_here(j2);
                } else {
                    self.patch_here(j);
                }
            }
            StmtKind::Block(body) => {
                self.block(body, result)?;
            }
            StmtKind::Empty => {}
            StmtKind::Debugger => {
                self.emit(Op::Debugger);
            }
            StmtKind::Throw(e) => {
                self.expr(e)?;
                self.pos(s.pos);
                self.emit(Op::Throw);
            }
            StmtKind::While(t, body) => {
                if let Some(r) = result {
                    self.emit(Op::LdaUndef);
                    self.emit(Op::Star(r));
                }
                let labels = core::mem::take(&mut self.f().pending_labels);
                let top = self.here();
                self.expr(t)?;
                let exit = self.emit(Op::JmpFalse(0));
                self.push_breakable(labels, true);
                self.stmt_completion(body, result)?;
                self.emit(Op::Jmp(top));
                self.patch_here(exit);
                self.pop_breakable(top);
            }
            StmtKind::DoWhile(body, t) => {
                if let Some(r) = result {
                    self.emit(Op::LdaUndef);
                    self.emit(Op::Star(r));
                }
                let labels = core::mem::take(&mut self.f().pending_labels);
                let top = self.here();
                self.push_breakable(labels, true);
                self.stmt_completion(body, result)?;
                let cont = self.here();
                self.patch_continues(cont);
                self.expr(t)?;
                self.emit(Op::JmpTrue(top));
                self.pop_breakable(cont);
            }
            StmtKind::For { init, test, update, body } => self.for_stmt(init, test, update, body, result)?,
            StmtKind::ForIn(head, obj, body) => self.for_in(head, obj, body, result)?,
            StmtKind::ForOf { head, iter, body, is_await } => self.for_of(head, iter, body, *is_await, result)?,
            StmtKind::Break(label) => self.jump_out(label.as_ref(), false, s.pos)?,
            StmtKind::Continue(label) => self.jump_out(label.as_ref(), true, s.pos)?,
            StmtKind::Labeled(name, body) => {
                let is_loop = matches!(body.kind, StmtKind::For { .. } | StmtKind::ForIn(..) | StmtKind::ForOf { .. } | StmtKind::While(..) | StmtKind::DoWhile(..) | StmtKind::Labeled(..));
                if is_loop {
                    self.f().pending_labels.push(name.clone());
                    if let StmtKind::Labeled(..) = body.kind {
                        // Nested label: keep collecting.
                    }
                    self.stmt_completion(body, result)?;
                    self.f().pending_labels.retain(|l| l != name);
                } else {
                    let mut labels = core::mem::take(&mut self.f().pending_labels);
                    labels.push(name.clone());
                    self.push_breakable(labels, false);
                    self.stmt_completion(body, result)?;
                    let end = self.here();
                    self.pop_breakable(end);
                }
            }
            StmtKind::Switch(d, cases) => self.switch(d, cases, result)?,
            StmtKind::Try { block, handler, finalizer } => self.try_stmt(block, handler, finalizer, result)?,
            StmtKind::With(obj, body) => {
                if let Some(r) = result {
                    self.emit(Op::LdaUndef);
                    self.emit(Op::Star(r));
                }
                self.expr(obj)?;
                self.emit(Op::PushWith);
                self.f().env_depth += 1;
                self.push_scope(ScopeKind::With);
                self.stmt_completion(body, result)?;
                self.f().scopes.pop();
                self.emit(Op::PopEnv);
                self.f().env_depth -= 1;
            }
            StmtKind::Import(_) => {}
            StmtKind::Export(ex) => self.export_stmt(ex, s.pos)?,
        }
        Ok(())
    }

    fn export_stmt(&mut self, ex: &'a Export, pos: Pos) -> CResult<()> {
        match ex {
            Export::Decl(s) => self.stmt(s),
            Export::DefaultExpr(e) => {
                let hint = self.atom("default");
                if e.is_anonymous_function_definition() {
                    self.expr_named(e, hint)?;
                } else {
                    self.expr(e)?;
                }
                self.init_binding("*default*");
                Ok(())
            }
            Export::DefaultFunction(_) => Ok(()),
            Export::DefaultClass(c) => {
                let hint = if c.name.is_none() { Some(self.atom("default")) } else { None };
                self.class(c, hint)?;
                let n = c.name.clone().unwrap_or_else(|| "*default*".into());
                self.init_binding(&n);
                let _ = pos;
                Ok(())
            }
            _ => Ok(()),
        }
    }

    fn is_annex_b_site(&self, f: &Rc<Function>) -> bool {
        self.annex_b_sites.borrow().contains(&(Rc::as_ptr(f) as usize))
    }

    /// Annex B: assigns acc to the var-scoped binding of a block function.
    fn store_annex_b_var(&mut self, name: &str) -> CResult<()> {
        // Find the binding in the nearest Function/Script scope.
        let mut hops = 0u16;
        let f = self.fr();
        for s in f.scopes.iter().rev() {
            if matches!(s.kind, ScopeKind::Function) || (s.kind == ScopeKind::Block && s.bindings.iter().any(|b| &*b.name == name && b.kind == BKind::Var)) {
                if let Some(b) = s.bindings.iter().find(|b| &*b.name == name) {
                    match b.loc {
                        Loc::Reg(r) => {
                            self.emit(Op::Star(r));
                        }
                        Loc::Env(slot) => {
                            self.emit(Op::StaEnv(hops, slot));
                        }
                    }
                    return Ok(());
                }
            }
            if s.kind == ScopeKind::Script {
                break;
            }
            if s.has_env {
                hops += 1;
            }
        }
        // Script / eval: a global var.
        let is_eval = f.kind == FuncKind::Eval;
        let k = self.name_const(name);
        if is_eval {
            self.emit(Op::StaName(k, false));
        } else {
            self.emit(Op::StaGlobal(k, false));
        }
        Ok(())
    }

    fn var_decl(&mut self, kind: VarKind, decls: &'a [VarDecl], _pos: Pos) -> CResult<()> {
        for d in decls {
            match &d.init {
                Some(init) => {
                    self.named_expr(init, &d.target)?;
                    if kind == VarKind::Var {
                        self.assign_pattern(&d.target)?;
                    } else {
                        self.bind_pattern(&d.target, true)?;
                        self.mark_initialized(&d.target, init.pos.offset);
                    }
                }
                None => {
                    if matches!(kind, VarKind::Const | VarKind::Using | VarKind::AwaitUsing) {
                        return err("Missing initializer in const declaration", _pos);
                    }
                    if kind != VarKind::Var {
                        self.emit(Op::LdaUndef);
                        self.bind_pattern(&d.target, true)?;
                        self.mark_initialized(&d.target, 0);
                    }
                }
            }
        }
        Ok(())
    }

    fn mark_initialized(&mut self, _p: &Pattern, _end: u32) {}

    /// A block statement with its lexical scope.
    pub fn block(&mut self, body: &'a [Stmt], result: Option<Reg>) -> CResult<()> {
        self.check_block_conflicts(body, &[])?;
        let opened = self.enter_lexical(body)?;
        for s in body {
            self.stmt_completion(s, result)?;
        }
        if opened {
            self.pop_scope();
        }
        Ok(())
    }

    /// Declares the lexical bindings and functions of a statement list in
    /// a new block scope (if any); hoists its functions. Returns whether a
    /// scope was pushed.
    pub fn enter_lexical(&mut self, body: &'a [Stmt]) -> CResult<bool> {
        let mut names: Vec<(Name, BKind)> = Vec::new();
        let mut fns: Vec<&'a Rc<Function>> = Vec::new();
        for s in body {
            match &s.kind {
                StmtKind::Var(k @ (VarKind::Let | VarKind::Const | VarKind::Using | VarKind::AwaitUsing), ds) => {
                    for d in ds {
                        let mut v = Vec::new();
                        crate::parser::bound_names(&d.target, &mut v);
                        for (n, _) in v {
                            let bk = if *k == VarKind::Let { BKind::Let } else { BKind::Const };
                            names.push((n, bk));
                        }
                    }
                }
                StmtKind::Class(c) => names.push((c.name.clone().unwrap(), BKind::Let)),
                StmtKind::Function(f) => fns.push(f),
                _ => {}
            }
        }
        if names.is_empty() && fns.is_empty() {
            return Ok(false);
        }
        let region = self.scan.region_stmts(body);
        let dynamic = region.dynamic || region.has_direct_eval;
        self.push_scope(ScopeKind::Block);
        for (n, k) in &names {
            let cap = dynamic || region.captured.contains(n);
            self.declare(n.clone(), *k, cap, true, u32::MAX);
        }
        for f in &fns {
            let n = f.name.clone().unwrap();
            let cap = dynamic || region.captured.contains(&n);
            self.declare(n, BKind::Var, cap, false, 0);
        }
        self.open_env();
        for (n, _) in &names {
            self.set_uninit(n);
        }
        let mut seen: Vec<Name> = Vec::new();
        for f in fns.iter().rev() {
            let n = f.name.clone().unwrap();
            if seen.contains(&n) {
                continue;
            }
            seen.push(n.clone());
            let idx = self.function_child(f, None)?;
            self.emit(Op::Closure(idx));
            self.init_binding(&n);
        }
        Ok(true)
    }

    fn for_stmt(&mut self, init: &'a Option<ForInit>, test: &'a Option<Expr>, update: &'a Option<Expr>, body: &'a Stmt, result: Option<Reg>) -> CResult<()> {
        if let Some(r) = result {
            self.emit(Op::LdaUndef);
            self.emit(Op::Star(r));
        }
        let labels = core::mem::take(&mut self.f().pending_labels);
        let mut scoped = false;
        let mut per_iteration = false;
        match init {
            Some(ForInit::Var(kind @ (VarKind::Let | VarKind::Const), decls)) => {
                let mut names = Vec::new();
                for d in decls {
                    crate::parser::bound_names(&d.target, &mut names);
                }
                for (i, (n, p)) in names.iter().enumerate() {
                    if names[..i].iter().any(|(m, _)| m == n) {
                        return err(&format!("Identifier '{n}' has already been declared"), *p);
                    }
                }
                let mut r = Region::default();
                for d in decls {
                    if let Some(i) = &d.init {
                        self.scan.expr(i, &mut r);
                    }
                }
                if let Some(t) = test {
                    self.scan.expr(t, &mut r);
                }
                if let Some(u) = update {
                    self.scan.expr(u, &mut r);
                }
                self.scan.stmt(body, &mut r);
                let dynamic = r.dynamic || r.has_direct_eval;
                self.push_scope(ScopeKind::Block);
                scoped = true;
                for (n, _) in &names {
                    let cap = dynamic || r.captured.contains(n);
                    let bk = if *kind == VarKind::Let { BKind::Let } else { BKind::Const };
                    self.declare(n.clone(), bk, cap, true, u32::MAX);
                }
                self.open_env();
                per_iteration = self.fr().scopes.last().unwrap().has_env && *kind == VarKind::Let;
                for (n, _) in &names {
                    self.set_uninit(n);
                }
                self.var_decl(*kind, decls, Pos::default())?;
                for (n, _) in &names {
                    self.set_init_end(n, 0);
                }
            }
            Some(ForInit::Var(kind, decls)) => self.var_decl(*kind, decls, Pos::default())?,
            Some(ForInit::Expr(e)) => {
                self.expr(e)?;
            }
            None => {}
        }
        if per_iteration {
            self.emit(Op::CloneEnv);
        }
        let top = self.here();
        let exit = match test {
            Some(t) => {
                self.expr(t)?;
                Some(self.emit(Op::JmpFalse(0)))
            }
            None => None,
        };
        self.push_breakable(labels, true);
        self.stmt_completion(body, result)?;
        let cont = self.here();
        self.patch_continues(cont);
        if per_iteration {
            self.emit(Op::CloneEnv);
        }
        if let Some(u) = update {
            self.expr(u)?;
        }
        self.emit(Op::Jmp(top));
        if let Some(x) = exit {
            self.patch_here(x);
        }
        self.pop_breakable(cont);
        if scoped {
            self.pop_scope();
        }
        Ok(())
    }

    /// Declares the per-iteration binding scope of `for (let/const x …)`
    /// and binds acc to the head.
    fn for_head_bind(&mut self, head: &'a ForHead, body: &'a Stmt) -> CResult<bool> {
        match head {
            ForHead::Var(VarKind::Var, p, _) => {
                self.assign_pattern(p)?;
                Ok(false)
            }
            ForHead::Var(kind, p, _) => {
                let mut names = Vec::new();
                crate::parser::bound_names(p, &mut names);
                for (i, (n, pp)) in names.iter().enumerate() {
                    if names[..i].iter().any(|(m, _)| m == n) {
                        return err(&format!("Identifier '{n}' has already been declared"), *pp);
                    }
                }
                let r = self.scan.region_stmt(body);
                let mut r2 = Region::default();
                self.scan.pattern(p, &mut r2);
                let dynamic = r.dynamic || r.has_direct_eval || r2.dynamic;
                let tmp = self.alloc_reg();
                self.emit(Op::Star(tmp));
                self.push_scope(ScopeKind::Block);
                for (n, _) in &names {
                    let cap = dynamic || r.captured.contains(n) || r2.captured.contains(n);
                    let bk = if *kind == VarKind::Let { BKind::Let } else { BKind::Const };
                    self.declare(n.clone(), bk, cap, true, u32::MAX);
                }
                self.open_env();
                for (n, _) in &names {
                    self.set_uninit(n);
                }
                self.emit(Op::Ldar(tmp));
                self.bind_pattern(p, true)?;
                for (n, _) in &names {
                    self.set_init_end(n, 0);
                }
                Ok(true)
            }
            ForHead::Target(p) => {
                self.assign_pattern(p)?;
                Ok(false)
            }
        }
    }

    /// Scope for the TDZ of `for (let x of <expr>)` while evaluating expr.
    fn for_tdz_scope(&mut self, head: &'a ForHead) -> bool {
        if let ForHead::Var(VarKind::Let | VarKind::Const, p, _) = head {
            let mut names = Vec::new();
            crate::parser::bound_names(p, &mut names);
            self.push_scope(ScopeKind::Block);
            for (n, _) in &names {
                self.declare(n.clone(), BKind::Let, false, true, u32::MAX);
            }
            for (n, _) in &names {
                self.set_uninit(n);
            }
            return true;
        }
        false
    }

    fn for_in(&mut self, head: &'a ForHead, obj: &'a Expr, body: &'a Stmt, result: Option<Reg>) -> CResult<()> {
        if let Some(r) = result {
            self.emit(Op::LdaUndef);
            self.emit(Op::Star(r));
        }
        let labels = core::mem::take(&mut self.f().pending_labels);
        // Annex B initializer: `for (var x = init in o)`.
        if let ForHead::Var(VarKind::Var, p, Some(init)) = head {
            self.named_expr(init, p)?;
            self.assign_pattern(p)?;
        }
        let tdz = self.for_tdz_scope(head);
        self.expr(obj)?;
        if tdz {
            self.f().scopes.pop();
        }
        let it = self.alloc_reg();
        self.emit(Op::ForInPrepare(it));
        self.push_breakable(labels, true);
        let top = self.here();
        let next = self.emit(Op::ForInNext(it, 0));
        let scoped = self.for_head_bind(head, body)?;
        self.stmt_completion(body, result)?;
        if scoped {
            self.pop_scope();
        }
        let cont = self.here();
        self.patch_continues(cont);
        self.emit(Op::Jmp(top));
        self.patch_here(next);
        self.pop_breakable(cont);
        self.free_to(it);
        Ok(())
    }

    fn for_of(&mut self, head: &'a ForHead, iter: &'a Expr, body: &'a Stmt, is_await: bool, result: Option<Reg>) -> CResult<()> {
        if let Some(r) = result {
            self.emit(Op::LdaUndef);
            self.emit(Op::Star(r));
        }
        let labels = core::mem::take(&mut self.f().pending_labels);
        let tdz = self.for_tdz_scope(head);
        self.expr(iter)?;
        if tdz {
            self.f().scopes.pop();
        }
        let it = self.alloc_regs(3);
        if is_await {
            self.emit(Op::GetAsyncIterator(it));
        } else {
            self.emit(Op::GetIterator(it));
        }
        let exc = self.alloc_reg();
        let top = self.here();
        let step = if is_await {
            self.emit(Op::IteratorNextRaw(it));
            let k = self.alloc_reg();
            self.emit(Op::Await(k));
            self.emit_await_resume(k)?;
            self.free_to(k);
            let kmsg = self.name_const("iterator result is not an object");
            self.emit(Op::CheckObject(kmsg));
            let res = self.alloc_reg();
            self.emit(Op::Star(res));
            self.emit(Op::IterResultDone);
            let j = self.emit(Op::JmpTrue(0));
            self.emit(Op::Ldar(res));
            self.emit(Op::IterResultValue);
            self.free_to(res);
            j
        } else {
            self.emit(Op::IteratorStep(it, 0))
        };
        // The head binding and body are protected: an exception closes the
        // iterator; `break` closes it too, `continue` does not.
        let ti = self.begin_try(exc);
        self.push_breakable_ext(labels, true, false, Some((it, ti, is_await)));
        let scoped = self.for_head_bind(head, body)?;
        self.stmt_completion(body, result)?;
        if scoped {
            self.pop_scope();
        }
        let cont = self.here();
        self.patch_continues(cont);
        self.emit(Op::Jmp(top));
        let brk = self.pop_breakable_collect();
        self.end_try(ti);
        // Handler: close and rethrow.
        self.handler_here(ti);
        if is_await {
            self.async_iterator_close_throw(it, exc)?;
        } else {
            self.emit(Op::IteratorCloseThrow(it, exc));
        }
        self.emit(Op::Ldar(exc));
        self.emit(Op::Throw);
        self.patch_here(step);
        for b in brk {
            self.patch_here(b);
        }
        self.free_to(it);
        Ok(())
    }

    fn switch(&mut self, d: &'a Expr, cases: &'a [Case], result: Option<Reg>) -> CResult<()> {
        if let Some(r) = result {
            self.emit(Op::LdaUndef);
            self.emit(Op::Star(r));
        }
        let labels = core::mem::take(&mut self.f().pending_labels);
        self.expr(d)?;
        let disc = self.alloc_reg();
        self.emit(Op::Star(disc));
        // One lexical scope for all cases.
        let all: Vec<&'a Stmt> = cases.iter().flat_map(|c| c.body.iter()).collect();
        let flat: Vec<Stmt> = Vec::new();
        let _ = flat;
        self.check_switch_conflicts(cases)?;
        let opened = self.enter_switch_lexical(cases)?;
        let mut jumps = Vec::new();
        for c in cases {
            if let Some(t) = &c.test {
                self.expr(t)?;
                jumps.push(Some(self.emit(Op::JmpStrictEq(disc, 0))));
            } else {
                jumps.push(None);
            }
        }
        let default_jump = self.emit(Op::Jmp(0));
        self.push_breakable_ext(labels, false, true, None);
        let mut default_target = None;
        for (i, c) in cases.iter().enumerate() {
            let here = self.here();
            match jumps[i] {
                Some(j) => self.patch(j, here),
                None => default_target = Some(here),
            }
            for s in &c.body {
                self.stmt_completion(s, result)?;
            }
        }
        let end = self.here();
        self.patch(default_jump, default_target.unwrap_or(end));
        self.pop_breakable(end);
        let _ = all;
        if opened {
            self.pop_scope();
        }
        Ok(())
    }

    fn check_switch_conflicts(&self, cases: &'a [Case]) -> CResult<()> {
        let mut stmts: Vec<&Stmt> = Vec::new();
        for c in cases {
            stmts.extend(c.body.iter());
        }
        // Reuse the block check on a flattened view.
        let strict = self.fr().strict;
        let mut lex: Vec<(Name, Pos, bool)> = Vec::new();
        let mut vars = Vec::new();
        for s in &stmts {
            match &s.kind {
                StmtKind::Var(VarKind::Let | VarKind::Const, ds) => {
                    let mut v = Vec::new();
                    for d in ds {
                        crate::parser::bound_names(&d.target, &mut v);
                    }
                    lex.extend(v.into_iter().map(|(n, p)| (n, p, false)));
                }
                StmtKind::Class(c) => lex.push((c.name.clone().unwrap(), s.pos, false)),
                StmtKind::Function(f) => lex.push((f.name.clone().unwrap(), s.pos, !f.is_async && !f.is_generator)),
                _ => {}
            }
            scan::var_declared_names(core::slice::from_ref(*s), &mut vars);
        }
        for (i, (n, p, plain)) in lex.iter().enumerate() {
            for (m, _, plain2) in &lex[..i] {
                if m == n && !(!strict && *plain && *plain2) {
                    return err(&format!("Identifier '{n}' has already been declared"), *p);
                }
            }
            if vars.contains(n) {
                return err(&format!("Identifier '{n}' has already been declared"), *p);
            }
        }
        Ok(())
    }

    fn enter_switch_lexical(&mut self, cases: &'a [Case]) -> CResult<bool> {
        let mut names: Vec<(Name, BKind)> = Vec::new();
        let mut fns: Vec<&'a Rc<Function>> = Vec::new();
        let mut region = Region::default();
        for c in cases {
            for s in &c.body {
                self.scan.stmt(s, &mut region);
                match &s.kind {
                    StmtKind::Var(k @ (VarKind::Let | VarKind::Const), ds) => {
                        for d in ds {
                            let mut v = Vec::new();
                            crate::parser::bound_names(&d.target, &mut v);
                            for (n, _) in v {
                                names.push((n, if *k == VarKind::Let { BKind::Let } else { BKind::Const }));
                            }
                        }
                    }
                    StmtKind::Class(c) => names.push((c.name.clone().unwrap(), BKind::Let)),
                    StmtKind::Function(f) => fns.push(f),
                    _ => {}
                }
            }
        }
        if names.is_empty() && fns.is_empty() {
            return Ok(false);
        }
        let dynamic = region.dynamic || region.has_direct_eval;
        self.push_scope(ScopeKind::Block);
        for (n, k) in &names {
            let cap = dynamic || region.captured.contains(n);
            self.declare(n.clone(), *k, cap, true, u32::MAX);
            if let Some(b) = self.f().scopes.last_mut().unwrap().bindings.last_mut() {
                b.in_switch = true;
            }
        }
        for f in &fns {
            let n = f.name.clone().unwrap();
            let cap = dynamic || region.captured.contains(&n);
            self.declare(n, BKind::Var, cap, false, 0);
        }
        self.open_env();
        for (n, _) in &names {
            self.set_uninit(n);
        }
        for f in &fns {
            let idx = self.function_child(f, None)?;
            self.emit(Op::Closure(idx));
            let n = f.name.clone().unwrap();
            self.init_binding(&n);
        }
        Ok(true)
    }

    fn try_stmt(&mut self, block: &'a [Stmt], handler: &'a Option<(Option<Pattern>, Vec<Stmt>)>, finalizer: &'a Option<Vec<Stmt>>, result: Option<Reg>) -> CResult<()> {
        if let Some(r) = result {
            self.emit(Op::LdaUndef);
            self.emit(Op::Star(r));
        }
        let exc_f = self.alloc_reg();
        let fin_try = if let Some(fb) = finalizer {
            let ti = self.begin_try(exc_f);
            self.f().ctx.push(Ctx::Finally { body: fb, try_index: ti });
            Some(ti)
        } else {
            None
        };
        let mut done_jumps = Vec::new();
        if let Some((param, hbody)) = handler {
            let exc = self.alloc_reg();
            let ti = self.begin_try(exc);
            self.f().ctx.push(Ctx::Catch { try_index: ti });
            self.block(block, result)?;
            self.f().ctx.pop();
            self.end_try(ti);
            done_jumps.push(self.emit(Op::Jmp(0)));
            self.handler_here(ti);
            // Catch scope: the parameter, then the body block.
            self.push_scope(ScopeKind::Block);
            if let Some(p) = param {
                let mut names = Vec::new();
                crate::parser::bound_names(p, &mut names);
                for (i, (n, pp)) in names.iter().enumerate() {
                    if names[..i].iter().any(|(m, _)| m == n) {
                        return err(&format!("Identifier '{n}' has already been declared"), *pp);
                    }
                }
                // Catch param vs lexical declarations of the body.
                let pnames: Vec<Name> = names.iter().map(|(n, _)| n.clone()).collect();
                self.check_block_conflicts(hbody, &pnames)?;
                let region = {
                    let mut r = self.scan.region_stmts(hbody);
                    self.scan.pattern(p, &mut r);
                    r
                };
                let dynamic = region.dynamic || region.has_direct_eval;
                for (n, _) in &names {
                    let cap = dynamic || region.captured.contains(n);
                    self.declare(n.clone(), BKind::Let, cap, false, 0);
                }
                self.open_env();
                self.emit(Op::Ldar(exc));
                self.bind_pattern(p, true)?;
            }
            self.block(hbody, result)?;
            self.pop_scope();
            self.free_to(exc);
        } else {
            self.block(block, result)?;
        }
        if let Some(ti) = fin_try {
            let fb = finalizer.as_ref().unwrap();
            self.f().ctx.pop();
            self.end_try(ti);
            for j in done_jumps.drain(..) {
                self.patch_here(j);
            }
            // Normal completion: run finally.
            let saved = result.map(|r| {
                let t = self.alloc_reg();
                self.emit(Op::Ldar(r));
                self.emit(Op::Star(t));
                (r, t)
            });
            self.block(fb, None)?;
            if let Some((r, t)) = saved {
                self.emit(Op::Ldar(t));
                self.emit(Op::Star(r));
            }
            let end = self.emit(Op::Jmp(0));
            self.handler_here(ti);
            self.block(fb, None)?;
            self.emit(Op::Ldar(exc_f));
            self.emit(Op::Throw);
            self.patch_here(end);
        } else {
            for j in done_jumps {
                self.patch_here(j);
            }
        }
        self.free_to(exc_f);
        Ok(())
    }

    // ----- try regions ---------------------------------------------------

    pub fn begin_try(&mut self, reg: Reg) -> usize {
        let start = self.here();
        let depth = self.fr().env_depth;
        let f = self.f();
        f.tries.push(TryRegion { ranges: Vec::new(), open: Some(start), target: 0, reg, env_depth: depth });
        f.tries.len() - 1
    }

    fn suspend_try(&mut self, i: usize) {
        let here = self.here();
        let t = &mut self.f().tries[i];
        if let Some(s) = t.open.take() {
            if s < here {
                t.ranges.push((s, here));
            }
        }
    }

    fn resume_try(&mut self, i: usize) {
        let here = self.here();
        self.f().tries[i].open = Some(here);
    }

    pub fn end_try(&mut self, i: usize) {
        self.suspend_try(i);
    }

    /// The handler of try region `i` starts here.
    pub fn handler_here(&mut self, i: usize) {
        let here = self.here();
        let f = self.f();
        f.tries[i].target = here;
        let t = &f.tries[i];
        for &(s, e) in &t.ranges {
            f.handlers.push(Handler { start: s, end: e, target: here, reg: t.reg, env_depth: t.env_depth });
        }
    }

    // ----- break / continue / return ------------------------------------

    fn push_breakable(&mut self, labels: Vec<Name>, is_loop: bool) {
        self.push_breakable_ext(labels, is_loop, false, None);
    }

    fn push_breakable_ext(&mut self, labels: Vec<Name>, is_loop: bool, is_switch: bool, iter: Option<(Reg, usize, bool)>) {
        let depth = self.fr().env_depth;
        self.f().ctx.push(Ctx::Breakable { labels, is_loop, is_switch, iter, breaks: Vec::new(), continues: Vec::new(), env_depth: depth, continue_target: None });
    }

    fn patch_continues(&mut self, target: Label) {
        let conts = match self.f().ctx.last_mut() {
            Some(Ctx::Breakable { continues, continue_target, .. }) => {
                *continue_target = Some(target);
                core::mem::take(continues)
            }
            _ => Vec::new(),
        };
        for c in conts {
            self.patch(c, target);
        }
    }

    fn patch_continues_to(&mut self, target: Label) {
        self.patch_continues(target);
    }

    fn pop_breakable(&mut self, cont: Label) {
        let end = self.here();
        if let Some(Ctx::Breakable { breaks, continues, .. }) = self.f().ctx.pop() {
            for b in breaks {
                self.patch(b, end);
            }
            for c in continues {
                self.patch(c, cont);
            }
        }
    }

    fn pop_breakable_at(&mut self, cont: Label) {
        self.pop_breakable(cont);
    }

    /// Pops a breakable context, returning its break jumps (to patch).
    fn pop_breakable_collect(&mut self) -> Vec<usize> {
        match self.f().ctx.pop() {
            Some(Ctx::Breakable { breaks, continues, continue_target, .. }) => {
                if let Some(t) = continue_target {
                    for c in continues {
                        self.patch(c, t);
                    }
                }
                breaks
            }
            _ => Vec::new(),
        }
    }

    /// `break`/`continue` (optionally labeled): runs the cleanups of the
    /// contexts it leaves, then jumps.
    fn jump_out(&mut self, label: Option<&Name>, is_continue: bool, pos: Pos) -> CResult<()> {
        let n = self.fr().ctx.len();
        let mut target = None;
        for i in (0..n).rev() {
            if let Ctx::Breakable { labels, is_loop, is_switch, .. } = &self.fr().ctx[i] {
                let ok = match label {
                    Some(l) => labels.contains(l),
                    None => *is_loop || (*is_switch && !is_continue),
                };
                if ok && (!is_continue || *is_loop) {
                    target = Some(i);
                    break;
                }
            }
        }
        let Some(t) = target else { return err("illegal break/continue", pos) };
        let (depth, iter) = match &self.fr().ctx[t] {
            Ctx::Breakable { env_depth, iter, .. } => (*env_depth, *iter),
            _ => (0, None),
        };
        let mut suspended = self.run_cleanups(t + 1)?;
        if !is_continue {
            if let Some((reg, ti, is_async)) = iter {
                self.suspend_try(ti);
                suspended.push(ti);
                if is_async {
                    self.async_iterator_close(reg)?;
                } else {
                    self.emit(Op::IteratorClose(reg));
                }
            }
        }
        if self.fr().env_depth > depth {
            self.emit(Op::PopEnvTo(depth));
        }
        let j = self.emit(Op::Jmp(0));
        let mut patch_now = None;
        if let Ctx::Breakable { breaks, continues, continue_target, .. } = &mut self.f().ctx[t] {
            if is_continue {
                match continue_target {
                    Some(c) => patch_now = Some(*c),
                    None => continues.push(j),
                }
            } else {
                breaks.push(j);
            }
        }
        if let Some(c) = patch_now {
            self.patch(j, c);
        }
        for i in suspended {
            self.resume_try(i);
        }
        Ok(())
    }

    /// Emits the cleanups of contexts `from..` (innermost first), with their
    /// try regions suspended. Returns the suspended regions.
    fn run_cleanups(&mut self, from: usize) -> CResult<Vec<usize>> {
        let mut suspended = Vec::new();
        let n = self.fr().ctx.len();
        // Suspend every try region inside the crossed contexts first.
        for i in (from..n).rev() {
            match &self.fr().ctx[i] {
                Ctx::Finally { try_index, .. } | Ctx::IterClose { try_index, .. } | Ctx::Catch { try_index } => {
                    let ti = *try_index;
                    self.suspend_try(ti);
                    suspended.push(ti);
                }
                _ => {}
            }
        }
        for i in (from..n).rev() {
            match &self.fr().ctx[i] {
                Ctx::Finally { body, .. } => {
                    let body: &'a [Stmt] = body;
                    // The finally body runs with the contexts outside it.
                    let saved: Vec<Ctx<'a>> = self.f().ctx.drain(i..).collect();
                    let r = self.block(body, None);
                    self.f().ctx.extend(saved);
                    r?;
                }
                Ctx::IterClose { reg, is_async, .. } => {
                    let (reg, is_async) = (*reg, *is_async);
                    if is_async {
                        self.async_iterator_close(reg)?;
                    } else {
                        self.emit(Op::IteratorClose(reg));
                    }
                }
                Ctx::Breakable { iter: Some((reg, ti, is_async)), .. } => {
                    let (reg, ti, is_async) = (*reg, *ti, *is_async);
                    if !suspended.contains(&ti) {
                        self.suspend_try(ti);
                        suspended.push(ti);
                    }
                    if is_async {
                        self.async_iterator_close(reg)?;
                    } else {
                        self.emit(Op::IteratorClose(reg));
                    }
                }
                _ => {}
            }
        }
        Ok(suspended)
    }

    /// `return` with the value in acc, running finally blocks.
    pub fn emit_return(&mut self, _pos: Option<Pos>) -> CResult<()> {
        let has_cleanup = self.fr().ctx.iter().any(|c| matches!(c, Ctx::Finally { .. } | Ctx::IterClose { .. } | Ctx::Breakable { iter: Some(_), .. }));
        let derived = self.fr().derived;
        if self.fr().is_async && self.fr().is_generator {
            // Async generator: `return x` awaits x.
            let k = self.alloc_reg();
            self.emit(Op::Await(k));
            self.emit_await_resume(k)?;
            self.free_to(k);
        }
        if has_cleanup {
            let t = self.alloc_reg();
            self.emit(Op::Star(t));
            let suspended = self.run_cleanups(0)?;
            self.emit(Op::Ldar(t));
            self.free_to(t);
            self.return_value(derived)?;
            for i in suspended {
                self.resume_try(i);
            }
        } else {
            self.return_value(derived)?;
        }
        Ok(())
    }

    fn return_value(&mut self, derived: bool) -> CResult<()> {
        if derived {
            // Object → return it; undefined → this; else TypeError.
            let j = self.emit(Op::JmpUndefined(0));
            let k = self.name_const("Derived constructors may only return object or undefined");
            self.emit(Op::CheckObject(k));
            self.emit(Op::Return);
            self.patch_here(j);
            self.load_this_checked()?;
        }
        self.emit(Op::Return);
        Ok(())
    }

    /// After `Await(k)`: a throw resumption rethrows.
    pub fn emit_await_resume(&mut self, k: Reg) -> CResult<()> {
        let j = self.emit(Op::JmpIfIntEq(k, 1, 0));
        let done = self.emit(Op::Jmp(0));
        self.patch_here(j);
        self.emit(Op::Throw);
        self.patch_here(done);
        Ok(())
    }

    fn async_iterator_close(&mut self, it: Reg) -> CResult<()> {
        // return = iterator.return; if present: await call; check object.
        let ret = self.alloc_reg();
        let k = self.name_const("return");
        self.emit(Op::GetNamed(it, k));
        let skip = self.emit(Op::JmpNullish(0));
        self.emit(Op::Star(ret));
        let call = self.alloc_regs(2);
        self.emit(Op::Mov(ret, call));
        self.emit(Op::Mov(it, call + 1));
        self.emit(Op::Call(call, 0));
        let kr = self.alloc_reg();
        self.emit(Op::Await(kr));
        self.emit_await_resume(kr)?;
        let km = self.name_const("iterator result is not an object");
        self.emit(Op::CheckObject(km));
        self.patch_here(skip);
        self.free_to(ret);
        Ok(())
    }

    fn async_iterator_close_throw(&mut self, it: Reg, exc: Reg) -> CResult<()> {
        // Errors from return() are ignored; the original exception wins.
        let inner = self.alloc_reg();
        let ti = self.begin_try(inner);
        let ret = self.alloc_reg();
        let k = self.name_const("return");
        self.emit(Op::GetNamed(it, k));
        let skip = self.emit(Op::JmpNullish(0));
        self.emit(Op::Star(ret));
        let call = self.alloc_regs(2);
        self.emit(Op::Mov(ret, call));
        self.emit(Op::Mov(it, call + 1));
        self.emit(Op::Call(call, 0));
        let kr = self.alloc_reg();
        self.emit(Op::Await(kr));
        self.patch_here(skip);
        self.end_try(ti);
        let end = self.emit(Op::Jmp(0));
        self.handler_here(ti);
        self.patch_here(end);
        let _ = exc;
        self.free_to(inner);
        Ok(())
    }
}

/// Information the class compiler passes to a constructor.
pub struct ClassCtorInfo {
    pub derived: bool,
    pub has_instance_fields: bool,
}

pub enum Resolved {
    Reg(Reg, Binding),
    /// (hops, slot, binding, crossed a function boundary)
    Env(u16, u16, Binding, bool),
    Global,
    Dynamic,
}

fn display_name(n: &str) -> &str {
    n
}

/// Collects Annex B.3.3 candidate function names in nested blocks.
fn annex_b_stmt(s: &Stmt, blocked: &mut Vec<Name>, out: &mut Vec<Name>, in_block: bool) {
    match &s.kind {
        StmtKind::Function(f) if in_block => {
            if !f.is_async && !f.is_generator {
                let n = f.name.clone().unwrap();
                if !blocked.contains(&n) && !out.contains(&n) {
                    out.push(n);
                }
            }
        }
        StmtKind::Block(b) => annex_b_block(b, blocked, out),
        StmtKind::If(_, a, b) => {
            annex_b_stmt(a, blocked, out, false);
            if let Some(b) = b {
                annex_b_stmt(b, blocked, out, false);
            }
        }
        StmtKind::For { body, init, .. } => {
            let n = blocked.len();
            if let Some(ForInit::Var(VarKind::Let | VarKind::Const, ds)) = init {
                for d in ds {
                    let mut v = Vec::new();
                    crate::parser::bound_names(&d.target, &mut v);
                    blocked.extend(v.into_iter().map(|x| x.0));
                }
            }
            annex_b_stmt(body, blocked, out, false);
            blocked.truncate(n);
        }
        StmtKind::ForIn(h, _, body) | StmtKind::ForOf { head: h, body, .. } => {
            let n = blocked.len();
            if let ForHead::Var(VarKind::Let | VarKind::Const, p, _) = h {
                let mut v = Vec::new();
                crate::parser::bound_names(p, &mut v);
                blocked.extend(v.into_iter().map(|x| x.0));
            }
            annex_b_stmt(body, blocked, out, false);
            blocked.truncate(n);
        }
        StmtKind::While(_, b) | StmtKind::DoWhile(b, _) | StmtKind::Labeled(_, b) | StmtKind::With(_, b) => annex_b_stmt(b, blocked, out, false),
        StmtKind::Try { block, handler, finalizer } => {
            annex_b_block(block, blocked, out);
            if let Some((p, b)) = handler {
                let n = blocked.len();
                if let Some(Pattern::Object { .. } | Pattern::Array { .. }) = p {
                    let mut v = Vec::new();
                    crate::parser::bound_names(p.as_ref().unwrap(), &mut v);
                    blocked.extend(v.into_iter().map(|x| x.0));
                }
                annex_b_block(b, blocked, out);
                blocked.truncate(n);
            }
            if let Some(f) = finalizer {
                annex_b_block(f, blocked, out);
            }
        }
        StmtKind::Switch(_, cases) => {
            let n = blocked.len();
            for c in cases {
                for s in &c.body {
                    lexical_of(s, blocked);
                }
            }
            for c in cases {
                for s in &c.body {
                    annex_b_stmt(s, blocked, out, true);
                }
            }
            blocked.truncate(n);
        }
        _ => {}
    }
}

fn lexical_of(s: &Stmt, blocked: &mut Vec<Name>) {
    match &s.kind {
        StmtKind::Var(VarKind::Let | VarKind::Const, ds) => {
            for d in ds {
                let mut v = Vec::new();
                crate::parser::bound_names(&d.target, &mut v);
                blocked.extend(v.into_iter().map(|x| x.0));
            }
        }
        StmtKind::Class(c) => blocked.push(c.name.clone().unwrap()),
        _ => {}
    }
}

fn annex_b_block(b: &[Stmt], blocked: &mut Vec<Name>, out: &mut Vec<Name>) {
    let n = blocked.len();
    for s in b {
        lexical_of(s, blocked);
    }
    for s in b {
        annex_b_stmt(s, blocked, out, true);
    }
    blocked.truncate(n);
}

/// Records the Annex B function declaration sites (function statements in
/// blocks whose name also becomes a var).
pub fn collect_annex_b_sites(body: &[Stmt], names: &[Name], out: &mut Vec<usize>) {
    fn walk(s: &Stmt, names: &[Name], out: &mut Vec<usize>, in_block: bool) {
        match &s.kind {
            StmtKind::Function(f) if in_block => {
                if names.contains(f.name.as_ref().unwrap()) && !f.is_async && !f.is_generator {
                    out.push(Rc::as_ptr(f) as usize);
                }
            }
            StmtKind::Block(b) => b.iter().for_each(|x| walk(x, names, out, true)),
            StmtKind::If(_, a, b) => {
                walk(a, names, out, false);
                if let Some(b) = b {
                    walk(b, names, out, false);
                }
            }
            StmtKind::For { body, .. } | StmtKind::ForIn(_, _, body) | StmtKind::ForOf { body, .. } | StmtKind::While(_, body) | StmtKind::DoWhile(body, _) | StmtKind::Labeled(_, body) | StmtKind::With(_, body) => {
                walk(body, names, out, false)
            }
            StmtKind::Try { block, handler, finalizer } => {
                block.iter().for_each(|x| walk(x, names, out, true));
                if let Some((_, b)) = handler {
                    b.iter().for_each(|x| walk(x, names, out, true));
                }
                if let Some(f) = finalizer {
                    f.iter().for_each(|x| walk(x, names, out, true));
                }
            }
            StmtKind::Switch(_, cases) => {
                for c in cases {
                    c.body.iter().for_each(|x| walk(x, names, out, true));
                }
            }
            _ => {}
        }
    }
    for s in body {
        walk(s, names, out, false);
    }
}

pub fn _unused() -> Box<()> {
    let _ = vec![0u8];
    Box::new(())
}
