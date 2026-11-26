//! Compiling a module: its import/export entries, the environment layout,
//! an init unit (hoisted functions, TDZ) and the body.

use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec::Vec;

use super::*;

/// `import { name as local } from requests[request]`.
#[derive(Debug, Clone)]
pub struct ImportEntry {
    pub request: usize,
    /// The imported name; None for a namespace import (`* as ns`).
    pub name: Option<String>,
    /// Import-table index (named) or environment slot (namespace).
    pub index: u16,
}

#[derive(Debug, Default)]
pub struct ModuleInfo {
    pub init: Option<Rc<Code>>,
    pub body: Option<Rc<Code>>,
    /// (specifier, attributes)
    pub requests: Vec<(String, Vec<(String, String)>)>,
    pub imports: Vec<ImportEntry>,
    /// (export name, local slot)
    pub local_exports: Vec<(String, u16)>,
    /// (export name, request, imported name or None for `* as ns`)
    pub indirect_exports: Vec<(String, usize, Option<String>)>,
    pub star_exports: Vec<usize>,
    pub env_slots: u16,
    /// Number of import-table entries.
    pub import_count: u16,
    pub has_tla: bool,
}

fn s16(u: &[u16]) -> String {
    String::from_utf16_lossy(u)
}

impl<'a, 'v> Compiler<'a, 'v> {
    pub fn module(&mut self, prog: &'a Program) -> CResult<ModuleInfo> {
        let mut info = ModuleInfo::default();
        let req = |info: &mut ModuleInfo, spec: &[u16], attrs: &[(Str16, Str16)]| -> usize {
            let sp = s16(spec);
            let at: Vec<(String, String)> = attrs.iter().map(|(k, v)| (s16(k), s16(v))).collect();
            if let Some(i) = info.requests.iter().position(|(s, a)| *s == sp && *a == at) {
                return i;
            }
            info.requests.push((sp, at));
            info.requests.len() - 1
        };
        // Import bindings and local declarations.
        let mut import_bindings: Vec<(Name, usize, Option<String>)> = Vec::new();
        for s in &prog.body {
            if let StmtKind::Import(imp) = &s.kind {
                let r = req(&mut info, &imp.specifier, &imp.attributes);
                if let Some(d) = &imp.default {
                    import_bindings.push((d.clone(), r, Some(String::from("default"))));
                }
                if let Some(ns) = &imp.namespace {
                    import_bindings.push((ns.clone(), r, None));
                }
                for (imported, local) in &imp.named {
                    import_bindings.push((local.clone(), r, Some(s16(imported))));
                }
            }
        }
        for (i, (n, _, _)) in import_bindings.iter().enumerate() {
            if import_bindings[..i].iter().any(|(m, _, _)| m == n) {
                return err(&format!("Identifier '{n}' has already been declared"), Pos::default());
            }
        }
        // Unwrap export declarations for the scope analysis.
        let mut decls: Vec<&'a Stmt> = Vec::new();
        for s in &prog.body {
            match &s.kind {
                StmtKind::Export(Export::Decl(d)) => decls.push(d),
                StmtKind::Import(_) | StmtKind::Export(_) => {}
                _ => decls.push(s),
            }
        }
        let mut vars = Vec::new();
        for d in &decls {
            scan::var_declared_names(core::slice::from_ref(*d), &mut vars);
        }
        let mut lexicals: Vec<(Name, BKind)> = Vec::new();
        let mut functions: Vec<(Name, &'a Rc<Function>)> = Vec::new();
        for d in &decls {
            match &d.kind {
                StmtKind::Var(k @ (VarKind::Let | VarKind::Const | VarKind::Using | VarKind::AwaitUsing), ds) => {
                    for dd in ds {
                        let mut v = Vec::new();
                        crate::parser::bound_names(&dd.target, &mut v);
                        for (n, _) in v {
                            lexicals.push((n, if *k == VarKind::Let { BKind::Let } else { BKind::Const }));
                        }
                    }
                }
                StmtKind::Class(c) => lexicals.push((c.name.clone().unwrap(), BKind::Let)),
                StmtKind::Function(f) => functions.push((f.name.clone().unwrap(), f)),
                _ => {}
            }
        }
        for s in &prog.body {
            match &s.kind {
                StmtKind::Export(Export::DefaultFunction(f)) => {
                    let n = f.name.clone().unwrap_or_else(|| "*default*".into());
                    functions.push((n, f));
                }
                StmtKind::Export(Export::DefaultClass(c)) => {
                    let n = c.name.clone().unwrap_or_else(|| "*default*".into());
                    lexicals.push((n, BKind::Let));
                }
                StmtKind::Export(Export::DefaultExpr(_)) => lexicals.push(("*default*".into(), BKind::Const)),
                _ => {}
            }
        }
        // Early errors: duplicates among lexical, function, var and import
        // names; duplicate exports; exported names must be declared.
        let mut all: Vec<Name> = Vec::new();
        for n in lexicals.iter().map(|x| &x.0).chain(functions.iter().map(|x| &x.0)) {
            if all.contains(n) {
                return err(&format!("Identifier '{n}' has already been declared"), Pos::default());
            }
            all.push(n.clone());
        }
        for n in &vars {
            if all.contains(n) {
                return err(&format!("Identifier '{n}' has already been declared"), Pos::default());
            }
        }
        for (n, _, _) in &import_bindings {
            if all.contains(n) || vars.contains(n) {
                return err(&format!("Identifier '{n}' has already been declared"), Pos::default());
            }
        }
        self.check_lexical_conflicts_module(&decls)?;

        let has_tla = module_has_tla(&prog.body);
        info.has_tla = has_tla;
        // ----- init unit
        self.funcs.push(self.new_func(FuncKind::Module, true, false, false));
        self.declare_module_scope(&import_bindings, &vars, &lexicals, &functions);
        for (n, _) in &lexicals {
            self.set_uninit(n);
        }
        let mut seen: Vec<Name> = Vec::new();
        for (n, f) in functions.iter().rev() {
            if seen.contains(n) {
                continue;
            }
            seen.push(n.clone());
            let hint = if &**n == "*default*" { Some(self.atom("default")) } else { None };
            let idx = self.function_child(f, hint)?;
            self.emit(Op::Closure(idx));
            self.init_binding(n);
        }
        self.emit(Op::LdaUndef);
        self.emit(Op::Return);
        let slots = self.fr().scopes.last().unwrap().env_slots;
        self.pop_scope_no_emit();
        let name = Value::str(self.vm.names.empty);
        info.init = Some(self.finish(name, 0, 0, false, false, (0, 0)));
        // ----- body
        self.funcs.push(self.new_func(FuncKind::Module, true, has_tla, false));
        self.declare_module_scope(&import_bindings, &vars, &lexicals, &functions);
        // Exports.
        let slot_of = |c: &Self, n: &str| -> Option<u16> {
            c.fr().scopes.last().unwrap().bindings.iter().find(|b| &*b.name == n).and_then(|b| match b.loc {
                Loc::Env(s) => Some(s),
                _ => None,
            })
        };
        let mut exported: Vec<String> = Vec::new();
        let add_export = |name: String, exported: &mut Vec<String>| -> CResult<()> {
            if exported.contains(&name) {
                return err(&format!("Duplicate export of '{name}'"), Pos::default());
            }
            exported.push(name);
            Ok(())
        };
        for s in &prog.body {
            let StmtKind::Export(ex) = &s.kind else { continue };
            match ex {
                Export::Named { specs, from: None } => {
                    for (local, exp) in specs {
                        let local = s16(local);
                        add_export(s16(exp), &mut exported)?;
                        if let Some((_, r, imp)) = import_bindings.iter().find(|(n, _, _)| **n == *local.as_str()) {
                            match imp {
                                Some(i) => info.indirect_exports.push((s16(exp), *r, Some(i.clone()))),
                                None => match slot_of(self, &local) {
                                    Some(sl) => info.local_exports.push((s16(exp), sl)),
                                    None => return err(&format!("Export '{local}' is not defined"), s.pos),
                                },
                            }
                        } else {
                            match slot_of(self, &local) {
                                Some(sl) => info.local_exports.push((s16(exp), sl)),
                                None => return err(&format!("Export '{local}' is not defined in module"), s.pos),
                            }
                        }
                    }
                }
                Export::Named { specs, from: Some((m, attrs)) } => {
                    let r = req(&mut info, m, attrs);
                    for (imp, exp) in specs {
                        add_export(s16(exp), &mut exported)?;
                        info.indirect_exports.push((s16(exp), r, Some(s16(imp))));
                    }
                }
                Export::All { exported: e, from, attributes } => {
                    let r = req(&mut info, from, attributes);
                    match e {
                        Some(n) => {
                            add_export(s16(n), &mut exported)?;
                            info.indirect_exports.push((s16(n), r, None));
                        }
                        None => info.star_exports.push(r),
                    }
                }
                Export::Decl(d) => {
                    let mut names: Vec<Name> = Vec::new();
                    match &d.kind {
                        StmtKind::Var(_, ds) => {
                            for dd in ds {
                                let mut v = Vec::new();
                                crate::parser::bound_names(&dd.target, &mut v);
                                names.extend(v.into_iter().map(|x| x.0));
                            }
                        }
                        StmtKind::Function(f) => names.push(f.name.clone().unwrap()),
                        StmtKind::Class(c) => names.push(c.name.clone().unwrap()),
                        _ => {}
                    }
                    for n in names {
                        add_export(String::from(&*n), &mut exported)?;
                        let sl = slot_of(self, &n).unwrap();
                        info.local_exports.push((String::from(&*n), sl));
                    }
                }
                Export::DefaultExpr(_) => {
                    add_export(String::from("default"), &mut exported)?;
                    info.local_exports.push((String::from("default"), slot_of(self, "*default*").unwrap()));
                }
                Export::DefaultFunction(f) => {
                    add_export(String::from("default"), &mut exported)?;
                    let n = f.name.clone().unwrap_or_else(|| "*default*".into());
                    info.local_exports.push((String::from("default"), slot_of(self, &n).unwrap()));
                }
                Export::DefaultClass(c) => {
                    add_export(String::from("default"), &mut exported)?;
                    let n = c.name.clone().unwrap_or_else(|| "*default*".into());
                    info.local_exports.push((String::from("default"), slot_of(self, &n).unwrap()));
                }
            }
        }
        // Import entries.
        for (n, r, imp) in &import_bindings {
            let b = self.fr().scopes.last().unwrap().bindings.iter().find(|b| b.name == *n).unwrap();
            let index = match b.loc {
                Loc::Import(i) | Loc::Env(i) => i,
                Loc::Reg(r) => r,
            };
            info.imports.push(ImportEntry { request: *r, name: imp.clone(), index });
        }
        info.import_count = import_bindings.iter().filter(|x| x.2.is_some()).count() as u16;
        let result = self.alloc_reg();
        let _ = result;
        for s in &prog.body {
            match &s.kind {
                StmtKind::Function(_) | StmtKind::Import(_) => {}
                StmtKind::Export(Export::DefaultFunction(_)) => {}
                _ => self.stmt(s)?,
            }
        }
        self.emit(Op::LdaUndef);
        self.emit(Op::Return);
        info.env_slots = slots.max(self.fr().scopes.last().unwrap().env_slots);
        self.pop_scope_no_emit();
        let name = Value::str(self.vm.names.empty);
        info.body = Some(self.finish(name, 0, 0, false, false, (0, prog.end)));
        Ok(info)
    }

    fn declare_module_scope(&mut self, imports: &[(Name, usize, Option<String>)], vars: &[Name], lexicals: &[(Name, BKind)], functions: &[(Name, &'a Rc<Function>)]) {
        self.push_scope(ScopeKind::Function);
        // The module environment is created by the VM (no PushEnv).
        let mut next_import = 0u16;
        for (n, _, imp) in imports {
            if imp.is_some() {
                let s = self.f().scopes.last_mut().unwrap();
                s.bindings.push(Binding { name: n.clone(), kind: BKind::Const, loc: Loc::Import(next_import), tdz: false, init_end: 0, in_switch: false });
                next_import += 1;
            } else {
                self.declare(n.clone(), BKind::Const, true, false, 0);
            }
        }
        for n in vars {
            self.declare(n.clone(), BKind::Var, true, false, 0);
        }
        for (n, k) in lexicals {
            self.declare(n.clone(), *k, true, true, u32::MAX);
        }
        for (n, _) in functions {
            self.declare(n.clone(), BKind::Var, true, false, 0);
        }
        let s = self.f().scopes.last_mut().unwrap();
        s.has_env = true;
    }

    fn check_lexical_conflicts_module(&self, decls: &[&'a Stmt]) -> CResult<()> {
        let _ = decls;
        Ok(())
    }
}

/// A top-level `await` (or `for await`) outside any function.
fn module_has_tla(body: &[Stmt]) -> bool {
    struct V(bool);
    fn e(x: &Expr, v: &mut V) {
        if v.0 {
            return;
        }
        match &x.kind {
            ExprKind::Await(_) => v.0 = true,
            ExprKind::Function(_) => {}
            ExprKind::Class(c) => class(c, v),
            ExprKind::Unary(_, a) | ExprKind::Update(_, _, a) | ExprKind::Paren(a) | ExprKind::OptionalChain(a) => e(a, v),
            ExprKind::Binary(_, a, b) | ExprKind::Logical(_, a, b) => {
                e(a, v);
                e(b, v);
            }
            ExprKind::Assign(_, p, b) => {
                pat(p, v);
                e(b, v);
            }
            ExprKind::Cond(a, b, c) => {
                e(a, v);
                e(b, v);
                e(c, v);
            }
            ExprKind::Call { callee, args, .. } => {
                e(callee, v);
                for a in args {
                    match a {
                        Arg::Expr(x) | Arg::Spread(x) => e(x, v),
                    }
                }
            }
            ExprKind::New(c, args) => {
                e(c, v);
                for a in args {
                    match a {
                        Arg::Expr(x) | Arg::Spread(x) => e(x, v),
                    }
                }
            }
            ExprKind::Member { obj, prop, .. } => {
                e(obj, v);
                if let MemberProp::Computed(k) = prop {
                    e(k, v);
                }
            }
            ExprKind::Seq(xs) => xs.iter().for_each(|x| e(x, v)),
            ExprKind::Array(xs) => {
                for x in xs {
                    if let ArrayElem::Expr(x) | ArrayElem::Spread(x) = x {
                        e(x, v);
                    }
                }
            }
            ExprKind::Object(ps) => {
                for p in ps {
                    match p {
                        ObjProp::Value { key, value, .. } => {
                            if let PropKey::Computed(k) = key {
                                e(k, v);
                            }
                            e(value, v);
                        }
                        ObjProp::Spread(x) | ObjProp::Proto(x) => e(x, v),
                        ObjProp::Method { key: PropKey::Computed(k), .. } => e(k, v),
                        _ => {}
                    }
                }
            }
            ExprKind::Template { exprs, .. } | ExprKind::Tagged { exprs, .. } => exprs.iter().for_each(|x| e(x, v)),
            ExprKind::ImportCall(a, b) => {
                e(a, v);
                if let Some(b) = b {
                    e(b, v);
                }
            }
            _ => {}
        }
    }
    fn class(c: &Class, v: &mut V) {
        if let Some(h) = &c.extends {
            e(h, v);
        }
        for m in &c.members {
            if let ClassMember::Method { key: PropKey::Computed(k), .. } | ClassMember::Field { key: PropKey::Computed(k), .. } = m {
                e(k, v);
            }
        }
    }
    fn pat(p: &Pattern, v: &mut V) {
        match p {
            Pattern::Expr(x) => e(x, v),
            Pattern::Object { props, rest, .. } => {
                for pp in props {
                    if let PropKey::Computed(k) = &pp.key {
                        e(k, v);
                    }
                    pat(&pp.target, v);
                    if let Some(d) = &pp.default {
                        e(d, v);
                    }
                }
                if let Some(r) = rest {
                    pat(r, v);
                }
            }
            Pattern::Array { elems, rest, .. } => {
                for x in elems.iter().flatten() {
                    pat(&x.target, v);
                    if let Some(d) = &x.default {
                        e(d, v);
                    }
                }
                if let Some(r) = rest {
                    pat(r, v);
                }
            }
            Pattern::Ident(..) => {}
        }
    }
    fn st(s: &Stmt, v: &mut V) {
        if v.0 {
            return;
        }
        match &s.kind {
            StmtKind::Expr(x) | StmtKind::Throw(x) => e(x, v),
            StmtKind::Var(k, ds) => {
                if *k == VarKind::AwaitUsing {
                    v.0 = true;
                }
                for d in ds {
                    pat(&d.target, v);
                    if let Some(i) = &d.init {
                        e(i, v);
                    }
                }
            }
            StmtKind::Return(Some(x)) => e(x, v),
            StmtKind::If(t, a, b) => {
                e(t, v);
                st(a, v);
                if let Some(b) = b {
                    st(b, v);
                }
            }
            StmtKind::Block(b) => b.iter().for_each(|x| st(x, v)),
            StmtKind::For { init, test, update, body } => {
                match init {
                    Some(ForInit::Expr(x)) => e(x, v),
                    Some(ForInit::Var(k, ds)) => {
                        if *k == VarKind::AwaitUsing {
                            v.0 = true;
                        }
                        for d in ds {
                            if let Some(i) = &d.init {
                                e(i, v);
                            }
                        }
                    }
                    None => {}
                }
                if let Some(t) = test {
                    e(t, v);
                }
                if let Some(u) = update {
                    e(u, v);
                }
                st(body, v);
            }
            StmtKind::ForIn(_, x, b) => {
                e(x, v);
                st(b, v);
            }
            StmtKind::ForOf { iter, body, is_await, head } => {
                if *is_await {
                    v.0 = true;
                }
                if let ForHead::Var(VarKind::AwaitUsing, ..) = head {
                    v.0 = true;
                }
                e(iter, v);
                st(body, v);
            }
            StmtKind::While(t, b) | StmtKind::DoWhile(b, t) => {
                e(t, v);
                st(b, v);
            }
            StmtKind::Try { block, handler, finalizer } => {
                block.iter().for_each(|x| st(x, v));
                if let Some((_, b)) = handler {
                    b.iter().for_each(|x| st(x, v));
                }
                if let Some(f) = finalizer {
                    f.iter().for_each(|x| st(x, v));
                }
            }
            StmtKind::Switch(d, cases) => {
                e(d, v);
                for c in cases {
                    if let Some(t) = &c.test {
                        e(t, v);
                    }
                    c.body.iter().for_each(|x| st(x, v));
                }
            }
            StmtKind::Labeled(_, b) | StmtKind::With(_, b) => st(b, v),
            StmtKind::Export(Export::Decl(d)) => st(d, v),
            StmtKind::Export(Export::DefaultExpr(x)) => e(x, v),
            StmtKind::Class(c) | StmtKind::Export(Export::DefaultClass(c)) => class(c, v),
            _ => {}
        }
    }
    let mut v = V(false);
    for s in body {
        st(s, &mut v);
    }
    v.0
}
