//! Free-variable analysis: which names a function refers to from outside,
//! so the compiler knows which bindings closures capture (those live in
//! environments; the rest stay in registers).

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::rc::Rc;
use alloc::vec::Vec;
use core::cell::RefCell;

use crate::ast::*;

/// What a function (with everything nested in it) needs from outside.
#[derive(Default, Debug, Clone)]
pub struct FreeInfo {
    pub names: BTreeSet<Name>,
    /// A direct `eval` or a `with` inside: every visible binding may be
    /// used by name at run time.
    pub dynamic: bool,
    /// Arrows: lexical `this`, `arguments`, `new.target`, `super(…)`,
    /// `super.x` used inside.
    pub this: bool,
    pub arguments: bool,
    pub new_target: bool,
    pub super_call: bool,
    pub super_prop: bool,
}

/// What a region of code (a block in one function) needs: the union of
/// the free names of the functions nested in it, plus local facts.
#[derive(Default, Debug, Clone)]
pub struct Region {
    /// Names used by nested functions (captured if declared here).
    pub captured: BTreeSet<Name>,
    /// A nested function or this code uses eval/with.
    pub dynamic: bool,
    /// Pseudo-bindings needed by nested arrows.
    pub this: bool,
    pub arguments: bool,
    pub new_target: bool,
    pub super_call: bool,
    pub super_prop: bool,
    /// Names referenced directly in this code (not nested functions).
    pub direct: BTreeSet<Name>,
    /// `arguments` referenced directly (not via an arrow).
    pub uses_arguments: bool,
    pub uses_this: bool,
    pub has_direct_eval: bool,
}

#[derive(Default)]
pub struct Scanner {
    cache: RefCell<BTreeMap<usize, Rc<FreeInfo>>>,
    /// The raw region of each function (parameters and body), so the
    /// compiler does not walk the body again.
    fn_regions: RefCell<BTreeMap<usize, Rc<Region>>>,
    /// Regions of compound statements, keyed by address: each subtree is
    /// walked once even though the compiler asks about every block.
    stmt_cache: RefCell<BTreeMap<usize, Rc<Region>>>,
}

impl Region {
    /// Needs nothing at all.
    pub fn is_empty(&self) -> bool {
        self.captured.is_empty()
            && self.direct.is_empty()
            && !(self.dynamic || self.this || self.arguments || self.new_target || self.super_call || self.super_prop)
            && !(self.uses_arguments || self.uses_this || self.has_direct_eval)
    }

    /// Adds everything `o` needs to `self`.
    pub fn merge(&mut self, o: &Region) {
        self.captured.extend(o.captured.iter().cloned());
        self.direct.extend(o.direct.iter().cloned());
        self.dynamic |= o.dynamic;
        self.this |= o.this;
        self.arguments |= o.arguments;
        self.new_target |= o.new_target;
        self.super_call |= o.super_call;
        self.super_prop |= o.super_prop;
        self.uses_arguments |= o.uses_arguments;
        self.uses_this |= o.uses_this;
        self.has_direct_eval |= o.has_direct_eval;
    }
}

impl Scanner {
    pub fn function(&self, f: &Rc<Function>) -> Rc<FreeInfo> {
        let key = Rc::as_ptr(f) as usize;
        if let Some(i) = self.cache.borrow().get(&key) {
            return i.clone();
        }
        let r = self.function_region(f);
        let is_arrow = f.kind == FnKind::Arrow;
        // Names declared at the function's top level are its own.
        let mut own: BTreeSet<Name> = BTreeSet::new();
        for p in &f.params {
            let mut v = Vec::new();
            crate::parser::bound_names(&p.target, &mut v);
            own.extend(v.into_iter().map(|(n, _)| n));
        }
        if let Some(rest) = &f.rest {
            let mut v = Vec::new();
            crate::parser::bound_names(rest, &mut v);
            own.extend(v.into_iter().map(|(n, _)| n));
        }
        top_level_declared(&f.body, &mut own);
        if let Some(n) = &f.name {
            own.insert(n.clone());
        }
        let mut names: BTreeSet<Name> = r.direct.union(&r.captured).cloned().collect();
        names.retain(|n| !own.contains(n));
        if !is_arrow {
            names.remove("arguments");
        }
        let info = FreeInfo {
            names,
            dynamic: r.dynamic || r.has_direct_eval,
            this: is_arrow && (r.uses_this || r.this),
            arguments: is_arrow && (r.uses_arguments || r.arguments),
            new_target: is_arrow && r.new_target,
            super_call: is_arrow && r.super_call,
            super_prop: is_arrow && r.super_prop,
        };
        let info = Rc::new(info);
        self.cache.borrow_mut().insert(key, info.clone());
        info
    }

    /// The region of a function's parameters and body (what the code
    /// itself and its nested functions use, before removing its own names).
    pub fn function_region(&self, f: &Rc<Function>) -> Rc<Region> {
        let key = Rc::as_ptr(f) as usize;
        if let Some(r) = self.fn_regions.borrow().get(&key) {
            return r.clone();
        }
        let mut r = Region::default();
        for p in &f.params {
            self.pattern(&p.target, &mut r);
            if let Some(d) = &p.default {
                self.expr(d, &mut r);
            }
        }
        if let Some(rest) = &f.rest {
            self.pattern(rest, &mut r);
        }
        self.stmts(&f.body, &mut r);
        let r = Rc::new(r);
        self.fn_regions.borrow_mut().insert(key, r.clone());
        r
    }

    fn nested(&self, f: &Rc<Function>, r: &mut Region) {
        let i = self.function(f);
        r.captured.extend(i.names.iter().cloned());
        r.dynamic |= i.dynamic;
        r.this |= i.this;
        r.arguments |= i.arguments;
        r.new_target |= i.new_target;
        r.super_call |= i.super_call;
        r.super_prop |= i.super_prop;
        if i.arguments {
            r.captured.insert("arguments".into());
        }
    }

    pub fn region_stmts(&self, s: &[Stmt]) -> Region {
        let mut r = Region::default();
        self.stmts(s, &mut r);
        r
    }

    pub fn region_stmt(&self, s: &Stmt) -> Region {
        let mut r = Region::default();
        self.stmt(s, &mut r);
        r
    }

    pub fn region_expr(&self, e: &Expr) -> Region {
        let mut r = Region::default();
        self.expr(e, &mut r);
        r
    }

    pub fn stmts(&self, s: &[Stmt], r: &mut Region) {
        for st in s {
            self.stmt(st, r);
        }
    }

    pub fn stmt(&self, s: &Stmt, r: &mut Region) {
        let compound = matches!(
            s.kind,
            StmtKind::If(..)
                | StmtKind::Block(_)
                | StmtKind::For { .. }
                | StmtKind::ForIn(..)
                | StmtKind::ForOf { .. }
                | StmtKind::While(..)
                | StmtKind::DoWhile(..)
                | StmtKind::Try { .. }
                | StmtKind::Switch(..)
                | StmtKind::Labeled(..)
                | StmtKind::With(..)
        );
        if !compound {
            return self.stmt_uncached(s, r);
        }
        let key = s as *const Stmt as usize;
        let cached = self.stmt_cache.borrow().get(&key).cloned();
        let sub = match cached {
            Some(c) => c,
            None => {
                let mut sub = Region::default();
                self.stmt_uncached(s, &mut sub);
                if sub.is_empty() {
                    // Nothing to merge: not worth a cache entry (a script
                    // of millions of `{}` must not hold one per block).
                    return;
                }
                let sub = Rc::new(sub);
                self.stmt_cache.borrow_mut().insert(key, sub.clone());
                sub
            }
        };
        r.merge(&sub);
    }

    fn stmt_uncached(&self, s: &Stmt, r: &mut Region) {
        match &s.kind {
            StmtKind::Expr(e) | StmtKind::Throw(e) => self.expr(e, r),
            StmtKind::Var(_, decls) => {
                for d in decls {
                    self.pattern(&d.target, r);
                    if let Some(i) = &d.init {
                        self.expr(i, r);
                    }
                }
            }
            StmtKind::Function(f) => self.nested(f, r),
            StmtKind::Class(c) => self.class(c, r),
            StmtKind::Return(e) => {
                if let Some(e) = e {
                    self.expr(e, r);
                }
            }
            StmtKind::If(t, a, b) => {
                self.expr(t, r);
                self.stmt(a, r);
                if let Some(b) = b {
                    self.stmt(b, r);
                }
            }
            StmtKind::Block(b) => self.stmts(b, r),
            StmtKind::For { init, test, update, body } => {
                match init.as_deref() {
                    Some(ForInit::Var(_, decls)) => {
                        for d in decls {
                            self.pattern(&d.target, r);
                            if let Some(i) = &d.init {
                                self.expr(i, r);
                            }
                        }
                    }
                    Some(ForInit::Expr(e)) => self.expr(e, r),
                    None => {}
                }
                if let Some(t) = test {
                    self.expr(t, r);
                }
                if let Some(u) = update {
                    self.expr(u, r);
                }
                self.stmt(body, r);
            }
            StmtKind::ForIn(h, e, body) => {
                self.for_head(h, r);
                self.expr(e, r);
                self.stmt(body, r);
            }
            StmtKind::ForOf { head, iter, body, .. } => {
                self.for_head(head, r);
                self.expr(iter, r);
                self.stmt(body, r);
            }
            StmtKind::While(t, b) | StmtKind::DoWhile(b, t) => {
                self.expr(t, r);
                self.stmt(b, r);
            }
            StmtKind::Try { block, handler, finalizer } => {
                self.stmts(block, r);
                if let Some((p, b)) = handler.as_deref() {
                    if let Some(p) = p {
                        self.pattern(p, r);
                    }
                    self.stmts(b, r);
                }
                if let Some(f) = finalizer {
                    self.stmts(f, r);
                }
            }
            StmtKind::Switch(d, cases) => {
                self.expr(d, r);
                for c in cases {
                    if let Some(t) = &c.test {
                        self.expr(t, r);
                    }
                    self.stmts(&c.body, r);
                }
            }
            StmtKind::Labeled(_, b) => self.stmt(b, r),
            StmtKind::With(e, b) => {
                self.expr(e, r);
                r.dynamic = true;
                self.stmt(b, r);
            }
            StmtKind::Export(Export::Decl(s)) => self.stmt(s, r),
            StmtKind::Export(Export::DefaultExpr(e)) => self.expr(e, r),
            StmtKind::Export(Export::DefaultFunction(f)) => self.nested(f, r),
            StmtKind::Export(Export::DefaultClass(c)) => self.class(c, r),
            StmtKind::Export(Export::Named { specs, from: None }) => {
                for (local, _) in specs {
                    let n: Name = alloc::string::String::from_utf16_lossy(local).into();
                    r.direct.insert(n);
                }
            }
            _ => {}
        }
    }

    fn for_head(&self, h: &ForHead, r: &mut Region) {
        match h {
            ForHead::Var(_, p, init) => {
                self.pattern(p, r);
                if let Some(i) = init {
                    self.expr(i, r);
                }
            }
            ForHead::Target(p) => self.pattern(p, r),
        }
    }

    pub fn class(&self, c: &Rc<Class>, r: &mut Region) {
        if let Some(e) = &c.extends {
            self.expr(e, r);
        }
        if let Some(f) = &c.constructor {
            self.nested(f, r);
        }
        for m in &c.members {
            match m {
                ClassMember::Method { key, func, .. } => {
                    self.key(key, r);
                    self.nested(func, r);
                }
                ClassMember::Field { key, value, .. } => {
                    self.key(key, r);
                    if let Some(v) = value {
                        self.nested(v, r);
                    }
                }
                ClassMember::StaticBlock(f) => self.nested(f, r),
            }
        }
    }

    fn key(&self, k: &PropKey, r: &mut Region) {
        if let PropKey::Computed(e) = k {
            self.expr(e, r);
        }
    }

    pub fn pattern(&self, p: &Pattern, r: &mut Region) {
        match p {
            Pattern::Ident(n, _) => {
                r.direct.insert(n.clone());
                if &**n == "arguments" {
                    r.uses_arguments = true;
                }
            }
            Pattern::Expr(e) => self.expr(e, r),
            Pattern::Object { props, rest, .. } => {
                for pp in props {
                    self.key(&pp.key, r);
                    self.pattern(&pp.target, r);
                    if let Some(d) = &pp.default {
                        self.expr(d, r);
                    }
                }
                if let Some(rest) = rest {
                    self.pattern(rest, r);
                }
            }
            Pattern::Array { elems, rest, .. } => {
                for e in elems.iter().flatten() {
                    self.pattern(&e.target, r);
                    if let Some(d) = &e.default {
                        self.expr(d, r);
                    }
                }
                if let Some(rest) = rest {
                    self.pattern(rest, r);
                }
            }
        }
    }

    pub fn expr(&self, e: &Expr, r: &mut Region) {
        match &e.kind {
            ExprKind::Ident(n) => {
                if &**n == "arguments" {
                    r.uses_arguments = true;
                }
                r.direct.insert(n.clone());
            }
            ExprKind::This => r.uses_this = true,
            ExprKind::NewTarget => r.new_target = true,
            ExprKind::Template { exprs, .. } => {
                for x in exprs {
                    self.expr(x, r);
                }
            }
            ExprKind::Tagged { tag, exprs, .. } => {
                self.expr(tag, r);
                for x in exprs {
                    self.expr(x, r);
                }
            }
            ExprKind::Array(el) => {
                for x in el {
                    match x {
                        ArrayElem::Expr(x) | ArrayElem::Spread(x) => self.expr(x, r),
                        ArrayElem::Hole => {}
                    }
                }
            }
            ExprKind::Object(props) => {
                for p in props {
                    match p {
                        ObjProp::Value { key, value, .. } => {
                            self.key(key, r);
                            self.expr(value, r);
                        }
                        ObjProp::Method { key, func, .. } => {
                            self.key(key, r);
                            self.nested(func, r);
                        }
                        ObjProp::Spread(x) | ObjProp::Proto(x) => self.expr(x, r),
                        ObjProp::CoverInit { name, init } => {
                            r.direct.insert(name.clone());
                            self.expr(init, r);
                        }
                    }
                }
            }
            ExprKind::Function(f) => self.nested(f, r),
            ExprKind::Class(c) => self.class(c, r),
            ExprKind::Unary(_, x) | ExprKind::Update(_, _, x) | ExprKind::Await(x) | ExprKind::Paren(x) | ExprKind::OptionalChain(x) => {
                self.expr(x, r)
            }
            ExprKind::Binary(_, a, b) | ExprKind::Logical(_, a, b) => {
                self.expr(a, r);
                self.expr(b, r);
            }
            ExprKind::Assign(_, p, v) => {
                self.pattern(p, r);
                self.expr(v, r);
            }
            ExprKind::Cond(a, b, c) => {
                self.expr(a, r);
                self.expr(b, r);
                self.expr(c, r);
            }
            ExprKind::Call { callee, args, .. } => {
                if let ExprKind::Ident(n) = &callee.kind {
                    if &**n == "eval" {
                        r.has_direct_eval = true;
                        r.uses_this = true;
                        r.uses_arguments = true;
                        r.new_target = true;
                        r.super_prop = true;
                    }
                }
                self.expr(callee, r);
                self.args(args, r);
            }
            ExprKind::New(c, args) => {
                self.expr(c, r);
                self.args(args, r);
            }
            ExprKind::Member { obj, prop, .. } => {
                self.expr(obj, r);
                if let MemberProp::Computed(k) = prop {
                    self.expr(k, r);
                }
            }
            ExprKind::SuperMember(prop) => {
                r.super_prop = true;
                r.uses_this = true;
                if let MemberProp::Computed(k) = prop {
                    self.expr(k, r);
                }
            }
            ExprKind::SuperCall(args) => {
                r.super_call = true;
                r.uses_this = true;
                r.new_target = true;
                self.args(args, r);
            }
            ExprKind::Seq(xs) => {
                for x in xs {
                    self.expr(x, r);
                }
            }
            ExprKind::Yield(x, _) => {
                if let Some(x) = x {
                    self.expr(x, r);
                }
            }
            ExprKind::ImportCall(a, b) => {
                self.expr(a, r);
                if let Some(b) = b {
                    self.expr(b, r);
                }
            }
            ExprKind::PrivateIn(n, x) => {
                let mut s = alloc::string::String::from("#");
                s.push_str(n);
                r.direct.insert(s.into());
                self.expr(x, r);
            }
            _ => {}
        }
        if let ExprKind::Member { prop: MemberProp::Private(n), .. } = &e.kind {
            let mut s = alloc::string::String::from("#");
            s.push_str(n);
            r.direct.insert(s.into());
        }
    }

    fn args(&self, args: &[Arg], r: &mut Region) {
        for a in args {
            match a {
                Arg::Expr(x) | Arg::Spread(x) => self.expr(x, r),
            }
        }
    }
}

/// Names declared at a function body's top level (vars anywhere, plus
/// top-level lexical declarations and functions).
pub fn top_level_declared(body: &[Stmt], out: &mut BTreeSet<Name>) {
    let mut vars = Vec::new();
    var_declared_names(body, &mut vars);
    out.extend(vars);
    for s in body {
        match &s.kind {
            StmtKind::Var(_, decls) => {
                for d in decls {
                    let mut v = Vec::new();
                    crate::parser::bound_names(&d.target, &mut v);
                    out.extend(v.into_iter().map(|(n, _)| n));
                }
            }
            StmtKind::Function(f) => {
                if let Some(n) = &f.name {
                    out.insert(n.clone());
                }
            }
            StmtKind::Class(c) => {
                if let Some(n) = &c.name {
                    out.insert(n.clone());
                }
            }
            _ => {}
        }
    }
}

/// VarDeclaredNames: `var` bindings anywhere in the statements (not in
/// nested functions).
pub fn var_declared_names(body: &[Stmt], out: &mut Vec<Name>) {
    for s in body {
        var_names_stmt(s, out);
    }
}

fn var_names_stmt(s: &Stmt, out: &mut Vec<Name>) {
    let push = |p: &Pattern, out: &mut Vec<Name>| {
        let mut v = Vec::new();
        crate::parser::bound_names(p, &mut v);
        for (n, _) in v {
            if !out.contains(&n) {
                out.push(n);
            }
        }
    };
    match &s.kind {
        StmtKind::Var(VarKind::Var, decls) => {
            for d in decls {
                push(&d.target, out);
            }
        }
        StmtKind::If(_, a, b) => {
            var_names_stmt(a, out);
            if let Some(b) = b {
                var_names_stmt(b, out);
            }
        }
        StmtKind::Block(b) => var_declared_names(b, out),
        StmtKind::For { init, body, .. } => {
            if let Some(ForInit::Var(VarKind::Var, decls)) = init.as_deref() {
                for d in decls {
                    push(&d.target, out);
                }
            }
            var_names_stmt(body, out);
        }
        StmtKind::ForIn(h, _, body) | StmtKind::ForOf { head: h, body, .. } => {
            if let ForHead::Var(VarKind::Var, p, _) = &**h {
                push(p, out);
            }
            var_names_stmt(body, out);
        }
        StmtKind::While(_, b) | StmtKind::DoWhile(b, _) | StmtKind::Labeled(_, b) | StmtKind::With(_, b) => var_names_stmt(b, out),
        StmtKind::Try { block, handler, finalizer } => {
            var_declared_names(block, out);
            if let Some((_, b)) = handler.as_deref() {
                var_declared_names(b, out);
            }
            if let Some(f) = finalizer {
                var_declared_names(f, out);
            }
        }
        StmtKind::Switch(_, cases) => {
            for c in cases {
                var_declared_names(&c.body, out);
            }
        }
        StmtKind::Export(Export::Decl(s)) => var_names_stmt(s, out),
        _ => {}
    }
}
