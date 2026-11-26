//! Expressions, assignment targets, destructuring and classes.

use alloc::format;
use alloc::rc::Rc;
use alloc::vec::Vec;

use super::*;

/// Where a class field's key comes from.
enum FieldKey {
    Static(Value),
    Private(Name),
    Computed(u32),
}

impl<'a, 'v> Compiler<'a, 'v> {
    /// Compiles `e` with acc = its value.
    pub fn expr(&mut self, e: &'a Expr) -> CResult<()> {
        match &e.kind {
            ExprKind::Ident(n) => {
                if &**n == "undefined" {
                    if let Resolved::Global = self.resolve(n) {
                        self.emit(Op::LdaUndef);
                        return Ok(());
                    }
                }
                self.load_name(n, e.pos)?;
            }
            ExprKind::This => self.load_this()?,
            ExprKind::Null => {
                self.emit(Op::LdaNull);
            }
            ExprKind::Bool(b) => {
                self.emit(if *b { Op::LdaTrue } else { Op::LdaFalse });
            }
            ExprKind::Num(n) => self.load_number(*n),
            ExprKind::BigInt(s) => {
                let k = self.konst(Const::BigInt(s.clone()));
                self.emit(Op::LdaConst(k));
            }
            ExprKind::Str(s) => {
                let k = self.str16_const(s);
                self.emit(Op::LdaConst(k));
            }
            ExprKind::Regex { body, flags } => {
                let b = self.name_const(body);
                let f = self.name_const(flags);
                self.emit(Op::NewRegExp(b, f));
            }
            ExprKind::Template { quasis, exprs } => {
                let k = self.str16_const(quasis[0].cooked.as_ref().unwrap());
                self.emit(Op::LdaConst(k));
                let acc = self.alloc_reg();
                for (i, x) in exprs.iter().enumerate() {
                    self.emit(Op::Star(acc));
                    self.expr(x)?;
                    self.emit(Op::ToStringOp);
                    self.emit(Op::Add(acc));
                    let q = &quasis[i + 1];
                    let cooked = q.cooked.as_ref().unwrap();
                    if !cooked.is_empty() {
                        self.emit(Op::Star(acc));
                        let k = self.str16_const(cooked);
                        self.emit(Op::LdaConst(k));
                        self.emit(Op::Add(acc));
                    }
                }
                self.free_to(acc);
            }
            ExprKind::Tagged { tag, quasis, exprs } => {
                let site = self.template_site(quasis);
                // Callee and this as for a call.
                let base = self.alloc_regs(2);
                self.callee_and_this(tag, base)?;
                let args = self.alloc_regs(1 + exprs.len() as u16);
                self.emit(Op::TemplateObject(site));
                self.emit(Op::Star(args));
                for (i, x) in exprs.iter().enumerate() {
                    self.expr(x)?;
                    self.emit(Op::Star(args + 1 + i as u16));
                }
                self.pos(e.pos);
                self.emit(Op::Call(base, 1 + exprs.len() as u16));
                self.free_to(base);
            }
            ExprKind::Array(elems) => {
                let r = self.alloc_reg();
                self.emit(Op::NewArray(elems.len() as u32));
                self.emit(Op::Star(r));
                for el in elems {
                    match el {
                        ArrayElem::Hole => {
                            self.emit(Op::ArrayHole(r));
                        }
                        ArrayElem::Expr(x) => {
                            self.expr(x)?;
                            self.emit(Op::ArrayPush(r));
                        }
                        ArrayElem::Spread(x) => {
                            self.expr(x)?;
                            self.emit(Op::ArraySpread(r));
                        }
                    }
                }
                self.emit(Op::Ldar(r));
                self.free_to(r);
            }
            ExprKind::Object(props) => self.object_literal(props)?,
            ExprKind::Function(f) => {
                if f.kind != FnKind::Arrow {
                    self.fn_expr_flag.set(true);
                }
                let idx = self.function_child(f, None)?;
                self.emit(Op::Closure(idx));
            }
            ExprKind::Class(c) => self.class(c, None)?,
            ExprKind::Unary(op, x) => self.unary(*op, x, e.pos)?,
            ExprKind::Update(inc, prefix, target) => self.update(*inc, *prefix, target)?,
            ExprKind::Binary(op, a, b) => self.binary(*op, a, b)?,
            ExprKind::Logical(op, a, b) => {
                self.expr(a)?;
                let j = match op {
                    LogicalOp::And => self.emit(Op::JmpFalse(0)),
                    LogicalOp::Or => self.emit(Op::JmpTrue(0)),
                    LogicalOp::Nullish => self.emit(Op::JmpNotNullish(0)),
                };
                self.expr(b)?;
                self.patch_here(j);
            }
            ExprKind::Assign(op, target, value) => self.assign(*op, target, value, e.pos)?,
            ExprKind::Cond(t, a, b) => {
                self.expr(t)?;
                let j = self.emit(Op::JmpFalse(0));
                self.expr(a)?;
                let j2 = self.emit(Op::Jmp(0));
                self.patch_here(j);
                self.expr(b)?;
                self.patch_here(j2);
            }
            ExprKind::Call { callee, args, optional } => self.call(callee, args, *optional, e.pos)?,
            ExprKind::New(callee, args) => {
                let base = self.alloc_regs(2);
                self.expr(callee)?;
                self.emit(Op::Star(base));
                let spread = args.iter().any(|a| matches!(a, Arg::Spread(_)));
                if spread {
                    self.args_array(args, base + 2)?;
                    self.pos(e.pos);
                    self.emit(Op::NewSpread(base));
                } else {
                    let n = self.args_regs(args)?;
                    self.pos(e.pos);
                    self.emit(Op::New(base, n));
                }
                self.free_to(base);
            }
            ExprKind::Member { obj, prop, optional } => {
                self.expr(obj)?;
                if *optional {
                    self.chain_jump();
                }
                let r = self.alloc_reg();
                self.emit(Op::Star(r));
                self.member_get(r, prop, e.pos)?;
                self.free_to(r);
            }
            ExprKind::SuperMember(prop) => {
                match prop {
                    MemberProp::Name(n) => {
                        self.load_this()?;
                        let k = self.name_const(n);
                        self.pos(e.pos);
                        self.emit(Op::GetSuperNamed(k));
                    }
                    MemberProp::Computed(k) => {
                        let r = self.alloc_reg();
                        self.load_this()?;
                        self.emit(Op::Star(r));
                        self.expr(k)?;
                        self.emit(Op::ToPropertyKey);
                        self.pos(e.pos);
                        self.emit(Op::GetSuperKeyed(r));
                        self.free_to(r);
                    }
                    MemberProp::Private(_) => unreachable!(),
                }
            }
            ExprKind::SuperCall(args) => self.super_call(args, e.pos)?,
            ExprKind::OptionalChain(inner) => {
                self.chains.push(Vec::new());
                self.expr(inner)?;
                let jumps = self.chains.pop().unwrap();
                if !jumps.is_empty() {
                    let end = self.emit(Op::Jmp(0));
                    for j in jumps {
                        self.patch_here(j);
                    }
                    self.emit(Op::LdaUndef);
                    self.patch_here(end);
                }
            }
            ExprKind::Seq(xs) => {
                for x in xs {
                    self.expr(x)?;
                }
            }
            ExprKind::Paren(x) => self.expr(x)?,
            ExprKind::Yield(arg, delegate) => {
                if *delegate {
                    self.yield_star(arg.as_ref().unwrap())?;
                } else {
                    match arg {
                        Some(a) => self.expr(a)?,
                        None => {
                            self.emit(Op::LdaUndef);
                        }
                    }
                    self.yield_value()?;
                }
            }
            ExprKind::Await(x) => {
                self.expr(x)?;
                let k = self.alloc_reg();
                self.emit(Op::Await(k));
                self.emit_await_resume(k)?;
                self.free_to(k);
            }
            ExprKind::NewTarget => match self.resolve_pseudo("new.target") {
                Some(r) => self.load_resolved(r),
                None => {
                    self.emit(Op::LdaNewTarget);
                }
            },
            ExprKind::ImportMeta => {
                self.emit(Op::ImportMeta);
            }
            ExprKind::ImportCall(spec, opts) => {
                let r = self.alloc_reg();
                match opts {
                    Some(o) => self.expr(o)?,
                    None => {
                        self.emit(Op::LdaUndef);
                    }
                }
                self.emit(Op::Star(r));
                // The specifier is evaluated first per spec: evaluate it,
                // then options; keep order by evaluating spec first.
                self.free_to(r);
                let rs = self.alloc_reg();
                self.expr(spec)?;
                self.emit(Op::Star(rs));
                let ro = self.alloc_reg();
                match opts {
                    Some(o) => self.expr(o)?,
                    None => {
                        self.emit(Op::LdaUndef);
                    }
                }
                self.emit(Op::Star(ro));
                self.emit(Op::Ldar(rs));
                self.emit(Op::ImportCall(ro));
                self.free_to(rs);
            }
            ExprKind::PrivateIn(name, obj) => {
                let rk = self.alloc_reg();
                self.load_private_name(name)?;
                self.emit(Op::Star(rk));
                self.expr(obj)?;
                self.emit(Op::HasPrivate(rk));
                self.free_to(rk);
            }
        }
        Ok(())
    }

    /// acc = super[key] with `this` and the key in registers.
    fn emit_get_super_keyed(&mut self, this_reg: Reg, key_reg: Reg) {
        self.emit(Op::Ldar(key_reg));
        self.emit(Op::GetSuperKeyed(this_reg));
    }

    /// The short-circuit jump of an optional chain (acc nullish → end).
    fn chain_jump(&mut self) {
        let j = self.emit(Op::JmpNullish(0));
        match self.chains.last_mut() {
            Some(c) => c.push(j),
            None => {
                // Not inside an OptionalChain node: cannot happen.
            }
        }
    }

    /// acc = obj[prop] with obj in `r`.
    fn member_get(&mut self, r: Reg, prop: &'a MemberProp, pos: Pos) -> CResult<()> {
        match prop {
            MemberProp::Name(n) => {
                let k = self.name_const(n);
                self.pos(pos);
                self.emit(Op::GetNamed(r, k));
            }
            MemberProp::Computed(k) => {
                self.expr(k)?;
                self.pos(pos);
                self.emit(Op::GetKeyed(r));
            }
            MemberProp::Private(n) => {
                self.load_private_name(n)?;
                self.pos(pos);
                self.emit(Op::GetPrivate(r));
            }
        }
        Ok(())
    }

    pub fn load_private_name(&mut self, n: &str) -> CResult<()> {
        let name = format!("#{n}");
        self.load_name(&name, Pos::default())
    }

    /// Resolves a pseudo-binding (`this`, `new.target`, `%fn`, `%home`),
    /// ignoring `with` scopes.
    pub fn resolve_pseudo(&self, name: &str) -> Option<Resolved> {
        let mut hops: u16 = 0;
        let mut crossed = false;
        for f in self.funcs.iter().rev() {
            for s in f.scopes.iter().rev() {
                if let Some(b) = s.bindings.iter().find(|b| &*b.name == name) {
                    return Some(match b.loc {
                        Loc::Reg(r) if !crossed => Resolved::Reg(r, b.clone()),
                        Loc::Reg(_) | Loc::Import(_) => return None,
                        Loc::Env(slot) => Resolved::Env(hops, slot, b.clone(), crossed),
                    });
                }
                if s.has_env || s.kind == ScopeKind::With {
                    hops += 1;
                }
            }
            if !f.is_arrow && f.kind != FuncKind::Eval {
                return None;
            }
            crossed = true;
        }
        None
    }

    fn load_resolved(&mut self, r: Resolved) {
        match r {
            Resolved::Reg(r, _) => {
                self.emit(Op::Ldar(r));
            }
            Resolved::Env(h, s, _, _) => {
                self.emit(Op::LdaEnv(h, s));
            }
            _ => {}
        }
    }

    /// acc = this.
    pub fn load_this(&mut self) -> CResult<()> {
        match self.resolve_pseudo("this") {
            Some(Resolved::Reg(r, b)) => {
                self.emit(Op::Ldar(r));
                if b.tdz {
                    let k = self.name_const("this");
                    self.emit(Op::CheckInit(r, k));
                }
            }
            Some(Resolved::Env(h, s, b, _)) => {
                if b.tdz {
                    let k = self.name_const("this");
                    self.emit(Op::LdaEnvChecked(h, s, k));
                } else {
                    self.emit(Op::LdaEnv(h, s));
                }
            }
            _ => {
                self.emit(Op::LdaThis);
            }
        }
        Ok(())
    }

    fn unary(&mut self, op: UnaryOp, x: &'a Expr, pos: Pos) -> CResult<()> {
        match op {
            UnaryOp::Typeof => {
                if let ExprKind::Ident(n) = &x.unparen().kind {
                    match self.resolve(n) {
                        Resolved::Global => {
                            let k = self.name_const(n);
                            self.emit(Op::LdaGlobalOrUndef(k));
                        }
                        Resolved::Dynamic => {
                            let k = self.name_const(n);
                            self.emit(Op::LdaNameOrUndef(k));
                        }
                        _ => self.load_name(n, x.pos)?,
                    }
                } else {
                    self.expr(x)?;
                }
                self.emit(Op::Typeof);
            }
            UnaryOp::Delete => self.delete(x, pos)?,
            UnaryOp::Void => {
                self.expr(x)?;
                self.emit(Op::LdaUndef);
            }
            UnaryOp::Not => {
                self.expr(x)?;
                self.emit(Op::Not);
            }
            UnaryOp::Minus => {
                if let ExprKind::Num(n) = x.kind {
                    self.load_number(-n);
                } else {
                    self.expr(x)?;
                    self.emit(Op::Neg);
                }
            }
            UnaryOp::Plus => {
                self.expr(x)?;
                self.emit(Op::ToNumber);
            }
            UnaryOp::BitNot => {
                self.expr(x)?;
                self.emit(Op::BitNot);
            }
        }
        Ok(())
    }

    fn delete(&mut self, x: &'a Expr, pos: Pos) -> CResult<()> {
        let strict = self.fr().strict;
        match &x.unparen().kind {
            ExprKind::Member { obj, prop, optional } => {
                self.expr(obj)?;
                if *optional {
                    self.chain_jump();
                }
                let r = self.alloc_reg();
                self.emit(Op::Star(r));
                match prop {
                    MemberProp::Name(n) => {
                        let k = self.name_const(n);
                        self.pos(pos);
                        self.emit(Op::DeleteNamed(r, k));
                    }
                    MemberProp::Computed(k) => {
                        self.expr(k)?;
                        self.pos(pos);
                        self.emit(Op::DeleteKeyed(r));
                    }
                    MemberProp::Private(_) => unreachable!(),
                }
                self.free_to(r);
                let _ = strict;
            }
            ExprKind::OptionalChain(inner) => {
                self.chains.push(Vec::new());
                self.delete(inner, pos)?;
                let jumps = self.chains.pop().unwrap();
                if !jumps.is_empty() {
                    let end = self.emit(Op::Jmp(0));
                    for j in jumps {
                        self.patch_here(j);
                    }
                    self.emit(Op::LdaTrue);
                    self.patch_here(end);
                }
            }
            ExprKind::SuperMember(_) => {
                // ReferenceError after evaluating `this` (and the key).
                self.load_this()?;
                if let ExprKind::SuperMember(MemberProp::Computed(k)) = &x.unparen().kind {
                    self.expr(k)?;
                }
                let k = self.name_const("Unsupported reference to 'super'");
                self.emit(Op::ThrowError(1, k));
            }
            ExprKind::Ident(n) => match self.resolve(n) {
                Resolved::Global | Resolved::Dynamic => {
                    let k = self.name_const(n);
                    self.emit(Op::DeleteName(k));
                }
                _ => {
                    self.emit(Op::LdaFalse);
                }
            },
            _ => {
                self.expr(x)?;
                self.emit(Op::LdaTrue);
            }
        }
        Ok(())
    }

    fn binary(&mut self, op: BinaryOp, a: &'a Expr, b: &'a Expr) -> CResult<()> {
        // Fast paths: `x + 1`, `x - 1`.
        if let (BinaryOp::Add | BinaryOp::Sub, ExprKind::Num(n)) = (op, &b.kind) {
            let v = if op == BinaryOp::Add { *n } else { -*n };
            if v == (v as i32) as f64 && v.abs() < 1e9 && !(op == BinaryOp::Sub && *n == 0.0) {
                self.expr(a)?;
                self.emit(Op::AddInt(v as i32));
                return Ok(());
            }
        }
        self.expr(a)?;
        let r = self.alloc_reg();
        self.emit(Op::Star(r));
        self.expr(b)?;
        let o = match op {
            BinaryOp::Add => Op::Add(r),
            BinaryOp::Sub => Op::Sub(r),
            BinaryOp::Mul => Op::Mul(r),
            BinaryOp::Div => Op::Div(r),
            BinaryOp::Mod => Op::Mod(r),
            BinaryOp::Exp => Op::Exp(r),
            BinaryOp::Shl => Op::Shl(r),
            BinaryOp::Shr => Op::Shr(r),
            BinaryOp::Ushr => Op::Ushr(r),
            BinaryOp::BitAnd => Op::BitAnd(r),
            BinaryOp::BitOr => Op::BitOr(r),
            BinaryOp::BitXor => Op::BitXor(r),
            BinaryOp::Eq => Op::Eq(r),
            BinaryOp::Ne => Op::Ne(r),
            BinaryOp::StrictEq => Op::StrictEq(r),
            BinaryOp::StrictNe => Op::StrictNe(r),
            BinaryOp::Lt => Op::Lt(r),
            BinaryOp::Gt => Op::Gt(r),
            BinaryOp::Le => Op::Le(r),
            BinaryOp::Ge => Op::Ge(r),
            BinaryOp::In => Op::In(r),
            BinaryOp::InstanceOf => Op::InstanceOf(r),
        };
        self.pos(a.pos);
        self.emit(o);
        self.free_to(r);
        Ok(())
    }

    fn update(&mut self, inc: bool, prefix: bool, target: &'a Expr) -> CResult<()> {
        let step = if inc { Op::Inc } else { Op::Dec };
        match &target.unparen().kind {
            ExprKind::Ident(n) => {
                self.load_name(n, target.pos)?;
                self.emit(Op::ToNumeric);
                if prefix {
                    self.emit(step);
                    self.store_name(n, target.pos)?;
                } else {
                    let old = self.alloc_reg();
                    self.emit(Op::Star(old));
                    self.emit(step);
                    self.store_name(n, target.pos)?;
                    self.emit(Op::Ldar(old));
                    self.free_to(old);
                }
            }
            ExprKind::Member { obj, prop, .. } => {
                let r = self.alloc_reg();
                self.expr(obj)?;
                self.emit(Op::Star(r));
                let rk = self.alloc_reg();
                match prop {
                    MemberProp::Name(n) => {
                        let k = self.name_const(n);
                        self.emit(Op::GetNamed(r, k));
                    }
                    MemberProp::Computed(k) => {
                        self.expr(k)?;
                        self.emit(Op::ToPropertyKey);
                        self.emit(Op::Star(rk));
                        self.emit(Op::GetKeyed(r));
                    }
                    MemberProp::Private(n) => {
                        self.load_private_name(n)?;
                        self.emit(Op::Star(rk));
                        self.emit(Op::GetPrivate(r));
                    }
                }
                self.emit(Op::ToNumeric);
                let old = self.alloc_reg();
                self.emit(Op::Star(old));
                self.emit(step);
                match prop {
                    MemberProp::Name(n) => {
                        let k = self.name_const(n);
                        self.emit(Op::SetNamed(r, k));
                    }
                    MemberProp::Computed(_) => {
                        self.emit(Op::SetKeyed(r, rk));
                    }
                    MemberProp::Private(_) => {
                        self.emit(Op::SetPrivate(r, rk));
                    }
                }
                if !prefix {
                    self.emit(Op::Ldar(old));
                }
                self.free_to(r);
            }
            ExprKind::SuperMember(prop) => {
                let r = self.alloc_regs(2);
                self.load_this()?;
                self.emit(Op::Star(r));
                let rk = self.alloc_reg();
                match prop {
                    MemberProp::Name(n) => {
                        let k = self.name_const(n);
                        self.emit(Op::Ldar(r));
                        self.emit(Op::GetSuperNamed(k));
                    }
                    MemberProp::Computed(k) => {
                        self.expr(k)?;
                        self.emit(Op::ToPropertyKey);
                        self.emit(Op::Star(rk));
                        self.emit_get_super_keyed(r, rk);
                    }
                    _ => unreachable!(),
                }
                self.emit(Op::ToNumeric);
                let old = self.alloc_reg();
                self.emit(Op::Star(old));
                self.emit(step);
                self.super_set(prop, r, rk)?;
                if !prefix {
                    self.emit(Op::Ldar(old));
                }
                self.free_to(r);
            }
            _ => {
                // `f()++` in sloppy mode: evaluate, then ReferenceError.
                self.expr(target)?;
                let k = self.name_const("Invalid left-hand side expression in postfix operation");
                self.emit(Op::ThrowError(1, k));
            }
        }
        Ok(())
    }

    /// `super[key] = acc` / `super.name = acc` with this in `this_reg`
    /// (and the key in `key_reg`); acc is kept.
    fn super_set(&mut self, prop: &'a MemberProp, this_reg: Reg, key_reg: Reg) -> CResult<()> {
        match prop {
            MemberProp::Name(n) => {
                let k = self.name_const(n);
                self.emit(Op::SetSuperNamed(k, this_reg));
            }
            MemberProp::Computed(_) => {
                self.emit(Op::SetSuperKeyed(this_reg, key_reg));
            }
            _ => unreachable!(),
        }
        Ok(())
    }

    fn assign(&mut self, op: AssignOp, target: &'a Pattern, value: &'a Expr, pos: Pos) -> CResult<()> {
        match op {
            AssignOp::Assign => match target {
                Pattern::Ident(n, p) => {
                    // Unresolvable strict refs throw after evaluating rhs.
                    self.named_expr(value, target)?;
                    self.store_name(n, *p)?;
                }
                Pattern::Expr(e) => self.assign_to_member(e, value, None)?,
                _ => {
                    self.expr(value)?;
                    let r = self.alloc_reg();
                    self.emit(Op::Star(r));
                    self.assign_pattern(target)?;
                    self.emit(Op::Ldar(r));
                    self.free_to(r);
                }
            },
            AssignOp::Op(bop) => match target {
                Pattern::Ident(n, p) => {
                    self.load_name(n, *p)?;
                    let r = self.alloc_reg();
                    self.emit(Op::Star(r));
                    self.expr(value)?;
                    self.emit(binop(bop, r));
                    self.free_to(r);
                    self.store_name(n, *p)?;
                }
                Pattern::Expr(e) => self.assign_to_member(e, value, Some(AssignOp::Op(bop)))?,
                _ => unreachable!(),
            },
            AssignOp::Logical(lop) => match target {
                Pattern::Ident(n, p) => {
                    self.load_name(n, *p)?;
                    let j = match lop {
                        LogicalOp::And => self.emit(Op::JmpFalse(0)),
                        LogicalOp::Or => self.emit(Op::JmpTrue(0)),
                        LogicalOp::Nullish => self.emit(Op::JmpNotNullish(0)),
                    };
                    self.named_expr(value, target)?;
                    self.store_name(n, *p)?;
                    self.patch_here(j);
                }
                Pattern::Expr(e) => self.assign_to_member(e, value, Some(op))?,
                _ => unreachable!(),
            },
        }
        let _ = pos;
        Ok(())
    }

    /// `obj.p = v`, `obj[k] op= v`, `super.p = v`, `f() = v` (sloppy).
    fn assign_to_member(&mut self, target: &'a Expr, value: &'a Expr, op: Option<AssignOp>) -> CResult<()> {
        match &target.unparen().kind {
            ExprKind::Member { obj, prop, .. } => {
                let r = self.alloc_reg();
                self.expr(obj)?;
                self.emit(Op::Star(r));
                let rk = self.alloc_reg();
                match prop {
                    MemberProp::Computed(k) => {
                        self.expr(k)?;
                        if op.is_some() {
                            self.emit(Op::ToPropertyKey);
                        }
                        self.emit(Op::Star(rk));
                    }
                    MemberProp::Private(n) => {
                        self.load_private_name(n)?;
                        self.emit(Op::Star(rk));
                    }
                    MemberProp::Name(_) => {}
                }
                let mut skip = None;
                if let Some(op) = op {
                    // Load current value.
                    match prop {
                        MemberProp::Name(n) => {
                            let k = self.name_const(n);
                            self.emit(Op::GetNamed(r, k));
                        }
                        MemberProp::Computed(_) => {
                            self.emit(Op::Ldar(rk));
                            self.emit(Op::GetKeyed(r));
                        }
                        MemberProp::Private(_) => {
                            self.emit(Op::Ldar(rk));
                            self.emit(Op::GetPrivate(r));
                        }
                    }
                    match op {
                        AssignOp::Op(bop) => {
                            let t = self.alloc_reg();
                            self.emit(Op::Star(t));
                            self.expr(value)?;
                            self.emit(binop(bop, t));
                            self.free_to(t);
                        }
                        AssignOp::Logical(l) => {
                            skip = Some(match l {
                                LogicalOp::And => self.emit(Op::JmpFalse(0)),
                                LogicalOp::Or => self.emit(Op::JmpTrue(0)),
                                LogicalOp::Nullish => self.emit(Op::JmpNotNullish(0)),
                            });
                            self.expr(value)?;
                        }
                        AssignOp::Assign => unreachable!(),
                    }
                } else {
                    self.expr(value)?;
                }
                self.pos(target.pos);
                match prop {
                    MemberProp::Name(n) => {
                        let k = self.name_const(n);
                        self.emit(Op::SetNamed(r, k));
                    }
                    MemberProp::Computed(_) => {
                        self.emit(Op::SetKeyed(r, rk));
                    }
                    MemberProp::Private(_) => {
                        self.emit(Op::SetPrivate(r, rk));
                    }
                }
                if let Some(j) = skip {
                    self.patch_here(j);
                }
                self.free_to(r);
            }
            ExprKind::SuperMember(prop) => {
                let r = self.alloc_regs(2);
                self.load_this()?;
                self.emit(Op::Star(r));
                let rk = self.alloc_reg();
                if let MemberProp::Computed(k) = prop {
                    self.expr(k)?;
                    self.emit(Op::ToPropertyKey);
                    self.emit(Op::Star(rk));
                }
                let mut skip = None;
                if let Some(op) = op {
                    match prop {
                        MemberProp::Name(n) => {
                            self.emit(Op::Ldar(r));
                            let k = self.name_const(n);
                            self.emit(Op::GetSuperNamed(k));
                        }
                        _ => self.emit_get_super_keyed(r, rk),
                    }
                    match op {
                        AssignOp::Op(bop) => {
                            let t = self.alloc_reg();
                            self.emit(Op::Star(t));
                            self.expr(value)?;
                            self.emit(binop(bop, t));
                            self.free_to(t);
                        }
                        AssignOp::Logical(l) => {
                            skip = Some(match l {
                                LogicalOp::And => self.emit(Op::JmpFalse(0)),
                                LogicalOp::Or => self.emit(Op::JmpTrue(0)),
                                LogicalOp::Nullish => self.emit(Op::JmpNotNullish(0)),
                            });
                            self.expr(value)?;
                        }
                        _ => unreachable!(),
                    }
                } else {
                    self.expr(value)?;
                }
                self.super_set(prop, r, rk)?;
                if let Some(j) = skip {
                    self.patch_here(j);
                }
                self.free_to(r);
            }
            _ => {
                // Call expression target (sloppy web compat): evaluate the
                // call, then throw ReferenceError.
                self.expr(target)?;
                let k = self.name_const("Invalid left-hand side in assignment");
                self.emit(Op::ThrowError(1, k));
            }
        }
        Ok(())
    }

    /// Compiles `e`, naming an anonymous function/class after a binding
    /// target (NamedEvaluation).
    pub fn named_expr(&mut self, e: &'a Expr, target: &Pattern) -> CResult<()> {
        if let Pattern::Ident(n, _) = target {
            if e.is_anonymous_function_definition() {
                let v = self.atom(n);
                return self.expr_named(e, v);
            }
        }
        self.expr(e)
    }

    /// An anonymous function/class definition named `name`.
    pub fn expr_named(&mut self, e: &'a Expr, name: Value) -> CResult<()> {
        match &e.unparen().kind {
            ExprKind::Function(f) => {
                let idx = self.function_child(f, Some(name))?;
                self.emit(Op::Closure(idx));
                Ok(())
            }
            ExprKind::Class(c) => self.class(c, Some(name)),
            _ => self.expr(e),
        }
    }

    // ----- calls ---------------------------------------------------------

    /// Loads the callee into `base` and its `this` into `base + 1`.
    fn callee_and_this(&mut self, callee: &'a Expr, base: Reg) -> CResult<()> {
        match &callee.unparen().kind {
            ExprKind::Member { obj, prop, optional } if !matches!(callee.kind, ExprKind::Paren(_)) || true => {
                self.expr(obj)?;
                if *optional {
                    self.chain_jump();
                }
                self.emit(Op::Star(base + 1));
                self.member_get(base + 1, prop, callee.pos)?;
                self.emit(Op::Star(base));
            }
            ExprKind::SuperMember(prop) => {
                self.load_this()?;
                self.emit(Op::Star(base + 1));
                match prop {
                    MemberProp::Name(n) => {
                        let k = self.name_const(n);
                        self.emit(Op::GetSuperNamed(k));
                    }
                    MemberProp::Computed(k) => {
                        self.expr(k)?;
                        self.emit(Op::ToPropertyKey);
                        self.emit(Op::GetSuperKeyed(base + 1));
                    }
                    _ => unreachable!(),
                }
                self.emit(Op::Star(base));
            }
            ExprKind::Ident(n) if matches!(self.resolve(n), Resolved::Dynamic) => {
                let k = self.name_const(n);
                self.emit(Op::LdaNameWithThis(k, base + 1));
                self.emit(Op::Star(base));
            }
            _ => {
                self.expr(callee)?;
                self.emit(Op::Star(base));
                self.emit(Op::LdaUndef);
                self.emit(Op::Star(base + 1));
            }
        }
        Ok(())
    }

    fn call(&mut self, callee: &'a Expr, args: &'a [Arg], optional: bool, pos: Pos) -> CResult<()> {
        let base = self.alloc_regs(2);
        let is_eval = matches!(&callee.kind, ExprKind::Ident(n) if &**n == "eval");
        self.callee_and_this(callee, base)?;
        if optional {
            self.emit(Op::Ldar(base));
            self.chain_jump();
        }
        let spread = args.iter().any(|a| matches!(a, Arg::Spread(_)));
        if spread {
            self.args_array(args, base + 2)?;
            self.pos(pos);
            self.emit(Op::CallSpread(base));
        } else {
            let n = self.args_regs(args)?;
            self.pos(pos);
            if is_eval {
                let k = self.eval_scope_const();
                self.emit(Op::CallEval(base, n, k));
            } else {
                self.emit(Op::Call(base, n));
            }
        }
        self.free_to(base);
        Ok(())
    }

    /// Evaluates arguments into consecutive registers; returns the count.
    fn args_regs(&mut self, args: &'a [Arg]) -> CResult<u16> {
        for a in args {
            if let Arg::Expr(x) = a {
                let r = self.alloc_reg();
                self.expr(x)?;
                self.emit(Op::Star(r));
            }
        }
        Ok(args.len() as u16)
    }

    /// Builds an array of the (spread) arguments in register `r`.
    fn args_array(&mut self, args: &'a [Arg], r: Reg) -> CResult<()> {
        let rr = self.alloc_reg();
        debug_assert_eq!(rr, r);
        self.emit(Op::NewArray(args.len() as u32));
        self.emit(Op::Star(r));
        for a in args {
            match a {
                Arg::Expr(x) => {
                    self.expr(x)?;
                    self.emit(Op::ArrayPush(r));
                }
                Arg::Spread(x) => {
                    self.expr(x)?;
                    self.emit(Op::ArraySpread(r));
                }
            }
        }
        Ok(())
    }

    /// The scope description for a direct eval at this point.
    fn eval_scope_const(&mut self) -> K {
        let mut levels = Vec::new();
        let mut var_scope_level = None;
        let mut in_function = false;
        let mut this_mode_lexical = false;
        let mut new_target = false;
        let mut super_prop = false;
        let mut super_call = false;
        let mut first_fn = true;
        for f in self.funcs.iter().rev() {
            if first_fn {
                in_function = !matches!(f.kind, FuncKind::Script | FuncKind::Eval | FuncKind::Module);
                this_mode_lexical = f.is_arrow;
                new_target = in_function;
                super_prop = matches!(f.kind, FuncKind::Method | FuncKind::ClassConstructor | FuncKind::DerivedConstructor);
                super_call = f.kind == FuncKind::DerivedConstructor;
            }
            for s in f.scopes.iter().rev() {
                if s.kind == ScopeKind::With {
                    levels.push(None);
                    continue;
                }
                if !s.has_env {
                    continue;
                }
                if s.dynamic && var_scope_level.is_none() && first_fn {
                    var_scope_level = Some(levels.len() as u16);
                }
                let names: Vec<(String, u16, BindingKind)> = s
                    .bindings
                    .iter()
                    .filter_map(|b| match b.loc {
                        Loc::Env(slot) => Some((
                            String::from(&*b.name),
                            slot,
                            match b.kind {
                                BKind::Let => BindingKind::Let,
                                BKind::Const | BKind::ClassName => BindingKind::Const,
                                BKind::FnName => BindingKind::FnName,
                                _ => BindingKind::Var,
                            },
                        )),
                        _ => None,
                    })
                    .collect();
                levels.push(Some(names.into()));
            }
            if !f.is_arrow {
                first_fn = false;
            }
        }
        let scope = EvalScope {
            levels,
            strict: self.fr().strict,
            in_function,
            this_mode_lexical,
            new_target,
            super_prop,
            super_call,
            in_class_field: false,
            var_scope_level,
        };
        self.konst(Const::EvalScope(Rc::new(scope)))
    }

    fn super_call(&mut self, args: &'a [Arg], pos: Pos) -> CResult<()> {
        let base = self.alloc_regs(2);
        match self.resolve_pseudo("%fn") {
            Some(r) => self.load_resolved(r),
            None => {
                self.emit(Op::LdaCallee);
            }
        }
        self.emit(Op::Star(base));
        match self.resolve_pseudo("new.target") {
            Some(r) => self.load_resolved(r),
            None => {
                self.emit(Op::LdaNewTarget);
            }
        }
        self.emit(Op::Star(base + 1));
        let spread = args.iter().any(|a| matches!(a, Arg::Spread(_)));
        if spread {
            self.args_array(args, base + 2)?;
            self.pos(pos);
            self.emit(Op::SuperCallSpread(base));
        } else {
            let n = self.args_regs(args)?;
            self.pos(pos);
            self.emit(Op::SuperCall(base, n));
        }
        self.after_super_call(base)?;
        self.free_to(base);
        Ok(())
    }

    /// acc = the new `this`: binds it (ReferenceError if already bound)
    /// and runs the field initializers of the constructor in `ctor`.
    pub fn after_super_call(&mut self, ctor: Reg) -> CResult<()> {
        let t = self.alloc_regs(2);
        self.emit(Op::Star(t));
        // Already initialized?
        self.emit(Op::LdaUninit);
        self.emit(Op::Star(t + 1));
        match self.resolve_pseudo("this") {
            Some(r) => self.load_resolved(r),
            None => {
                self.emit(Op::LdaUninit);
            }
        }
        let ok = self.emit(Op::JmpStrictEq(t + 1, 0));
        let k = self.name_const("Super constructor may only be called once");
        self.emit(Op::ThrowError(1, k));
        self.patch_here(ok);
        self.emit(Op::Ldar(t));
        match self.resolve_pseudo("this") {
            Some(Resolved::Reg(r, _)) => {
                self.emit(Op::Star(r));
            }
            Some(Resolved::Env(h, s, _, _)) => {
                self.emit(Op::StaEnv(h, s));
            }
            _ => {}
        }
        self.emit(Op::InitFieldsOf(ctor));
        self.emit(Op::Ldar(t));
        self.free_to(t);
        Ok(())
    }

    // ----- generators ------------------------------------------------------

    /// `yield acc`: suspends; dispatches the resumption.
    fn yield_value(&mut self) -> CResult<()> {
        let is_async = self.fr().is_async;
        let k = self.alloc_reg();
        if is_async {
            // Async generators await the operand first.
            self.emit(Op::Await(k));
            self.emit_await_resume(k)?;
        }
        self.emit(Op::Yield(k));
        self.resume_dispatch(k)?;
        self.free_to(k);
        Ok(())
    }

    /// After a Yield: kind 0 continues with acc, 1 throws acc, 2 returns acc.
    fn resume_dispatch(&mut self, k: Reg) -> CResult<()> {
        let next = self.emit(Op::JmpIfIntEq(k, 0, 0));
        let throw = self.emit(Op::JmpIfIntEq(k, 1, 0));
        // return
        self.emit_return(None)?;
        self.patch_here(throw);
        self.emit(Op::Throw);
        self.patch_here(next);
        Ok(())
    }

    /// `yield* iterable`.
    fn yield_star(&mut self, arg: &'a Expr) -> CResult<()> {
        let is_async = self.fr().is_async;
        self.expr(arg)?;
        let it = self.alloc_regs(3);
        if is_async {
            self.emit(Op::GetAsyncIterator(it));
        } else {
            self.emit(Op::GetIterator(it));
        }
        let kind = self.alloc_reg();
        let val = self.alloc_reg();
        let res = self.alloc_reg();
        self.emit(Op::LdaInt(0));
        self.emit(Op::Star(kind));
        self.emit(Op::LdaUndef);
        self.emit(Op::Star(val));
        let top = self.here();
        let call = self.alloc_regs(3);
        // kind 0: next(val)
        let not_next = self.emit(Op::JmpIfIntEq(kind, 0, 0));
        let to_throw = self.emit(Op::JmpIfIntEq(kind, 1, 0));
        // kind 2: return
        let kret = self.name_const("return");
        self.emit(Op::GetNamed(it, kret));
        let has_ret = self.emit(Op::JmpNotNullish(0));
        // No return method: return val (awaited in async generators).
        self.emit(Op::Ldar(val));
        if is_async {
            let k = self.alloc_reg();
            self.emit(Op::Await(k));
            self.emit_await_resume(k)?;
            self.free_to(k);
        }
        self.emit_return_direct()?;
        self.patch_here(has_ret);
        let after_method = self.emit(Op::Jmp(0));
        // throw
        self.patch_here(to_throw);
        let kthrow = self.name_const("throw");
        self.emit(Op::GetNamed(it, kthrow));
        let has_throw = self.emit(Op::JmpNotNullish(0));
        // No throw method: close the iterator, then TypeError.
        if is_async {
            self.async_iterator_close_for_ystar(it)?;
        } else {
            self.emit(Op::IteratorClose(it));
        }
        let kmsg = self.name_const("The iterator does not provide a 'throw' method");
        self.emit(Op::ThrowError(0, kmsg));
        self.patch_here(has_throw);
        let after_method2 = self.emit(Op::Jmp(0));
        // next
        self.patch_here(not_next);
        self.emit(Op::Ldar(it + 1));
        self.patch_here(after_method);
        self.patch_here(after_method2);
        // acc = method; call it with (it, val)
        self.emit(Op::Star(call));
        self.emit(Op::Mov(it, call + 1));
        self.emit(Op::Mov(val, call + 2));
        self.emit(Op::Call(call, 1));
        if is_async {
            let k = self.alloc_reg();
            self.emit(Op::Await(k));
            self.emit_await_resume(k)?;
            self.free_to(k);
        }
        let kobj = self.name_const("iterator result is not an object");
        self.emit(Op::CheckObject(kobj));
        self.emit(Op::Star(res));
        self.emit(Op::IterResultDone);
        let not_done = self.emit(Op::JmpFalse(0));
        // done: value is the result (or the return value for kind 2).
        self.emit(Op::Ldar(res));
        self.emit(Op::IterResultValue);
        let normal_done = self.emit(Op::JmpIfIntEq(kind, 2, 0));
        let exit = self.emit(Op::Jmp(0));
        self.patch_here(normal_done);
        self.emit_return_direct_maybe_async()?;
        self.patch_here(not_done);
        // Not done: yield the result (sync: the object itself).
        if is_async {
            self.emit(Op::Ldar(res));
            self.emit(Op::IterResultValue);
            self.emit(Op::AsyncGenYield(kind));
        } else {
            self.emit(Op::Ldar(res));
            self.emit(Op::YieldRaw(kind));
        }
        self.emit(Op::Star(val));
        self.emit(Op::Jmp(top));
        self.patch_here(exit);
        self.free_to(it);
        Ok(())
    }

    fn emit_return_direct(&mut self) -> CResult<()> {
        // Like emit_return, without awaiting again (already awaited).
        let saved = self.fr().is_generator;
        let is_async = self.fr().is_async;
        if is_async && saved {
            self.f().is_generator = false;
            let r = self.emit_return(None);
            self.f().is_generator = true;
            return r;
        }
        self.emit_return(None)
    }

    fn emit_return_direct_maybe_async(&mut self) -> CResult<()> {
        self.emit_return_direct()
    }

    fn async_iterator_close_for_ystar(&mut self, it: Reg) -> CResult<()> {
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
        self.patch_here(skip);
        self.free_to(ret);
        Ok(())
    }

    // ----- object literals ------------------------------------------------

    fn object_literal(&mut self, props: &'a [ObjProp]) -> CResult<()> {
        let r = self.alloc_reg();
        self.emit(Op::NewObject);
        self.emit(Op::Star(r));
        for p in props {
            match p {
                ObjProp::Value { key, value, .. } => match key {
                    PropKey::Computed(k) => {
                        let rk = self.alloc_reg();
                        self.expr(k)?;
                        self.emit(Op::ToPropertyKey);
                        self.emit(Op::Star(rk));
                        self.expr(value)?;
                        if value.is_anonymous_function_definition() {
                            self.emit(Op::SetFunctionName(rk, 0));
                        }
                        self.emit(Op::DefineKeyed(r, rk));
                        self.free_to(rk);
                    }
                    _ => {
                        let kv = self.static_key(key);
                        if value.is_anonymous_function_definition() {
                            self.expr_named(value, kv)?;
                        } else {
                            self.expr(value)?;
                        }
                        let k = self.value_const(kv);
                        self.emit(Op::DefineNamed(r, k));
                    }
                },
                ObjProp::Method { key, kind, func } => {
                    let rk = self.alloc_reg();
                    let static_name = self.load_key(key, rk)?;
                    let name = static_name.map(|n| self.method_name(n, *kind));
                    let idx = self.function_child(func, name)?;
                    self.emit(Op::Closure(idx));
                    if static_name.is_none() {
                        self.emit(Op::SetFunctionName(rk, prefix_of(*kind)));
                    }
                    self.emit(Op::DefineMethod { obj: r, key: rk, kind: method_kind(*kind), enumerable: true });
                    self.free_to(rk);
                }
                ObjProp::Spread(x) => {
                    self.expr(x)?;
                    self.emit(Op::CopyDataProps(r));
                }
                ObjProp::Proto(x) => {
                    self.expr(x)?;
                    self.emit(Op::SetProtoLiteral(r));
                }
                ObjProp::CoverInit { .. } => unreachable!("rejected by the parser"),
            }
        }
        self.emit(Op::Ldar(r));
        self.free_to(r);
        Ok(())
    }

    /// The property-key value of a non-computed key.
    pub fn static_key(&mut self, key: &PropKey) -> Value {
        match key {
            PropKey::Name(s) => self.atom16(s),
            PropKey::Num(n) => {
                let s = crate::vm::conv::number_to_string(*n);
                self.atom(&s)
            }
            PropKey::BigInt(s) => self.atom(s),
            PropKey::Computed(_) | PropKey::Private(_) => unreachable!(),
        }
    }

    /// Loads a key into register `rk`; returns the static key value if not
    /// computed.
    fn load_key(&mut self, key: &'a PropKey, rk: Reg) -> CResult<Option<Value>> {
        match key {
            PropKey::Computed(k) => {
                self.expr(k)?;
                self.emit(Op::ToPropertyKey);
                self.emit(Op::Star(rk));
                Ok(None)
            }
            PropKey::Private(n) => {
                self.load_private_name(n)?;
                self.emit(Op::Star(rk));
                Ok(None)
            }
            _ => {
                let v = self.static_key(key);
                let k = self.value_const(v);
                self.emit(Op::LdaConst(k));
                self.emit(Op::Star(rk));
                Ok(Some(v))
            }
        }
    }

    fn method_name(&mut self, key: Value, kind: MethodKind) -> Value {
        match kind {
            MethodKind::Method => key,
            MethodKind::Get | MethodKind::Set => {
                let s = self.vm.str(key.as_string().unwrap()).to_rust_lossy();
                let full = format!("{} {}", if kind == MethodKind::Get { "get" } else { "set" }, s);
                self.atom(&full)
            }
        }
    }

    fn template_site(&mut self, quasis: &Rc<Vec<TemplatePiece>>) -> u32 {
        let mut items = Vec::new();
        for q in quasis.iter() {
            let cooked = match &q.cooked {
                Some(c) => self.atom16(c),
                None => Value::UNDEFINED,
            };
            let raw = self.atom16(&q.raw);
            items.push((cooked, raw));
        }
        let k = self.konst(Const::Template(items.into()));
        self.next_site += 1;
        k
    }

    // ----- destructuring --------------------------------------------------

    /// Assigns acc to an assignment target/pattern (`x = …`, `for (x of …)`).
    pub fn assign_pattern(&mut self, p: &'a Pattern) -> CResult<()> {
        self.destructure(p, false)
    }

    /// Initializes the bindings of a binding pattern with acc.
    pub fn bind_pattern(&mut self, p: &'a Pattern, _init: bool) -> CResult<()> {
        self.destructure(p, true)
    }

    fn destructure(&mut self, p: &'a Pattern, init: bool) -> CResult<()> {
        match p {
            Pattern::Ident(n, pos) => {
                if init {
                    self.init_binding(n);
                } else {
                    self.store_name(n, *pos)?;
                }
            }
            Pattern::Expr(e) => {
                let v = self.alloc_reg();
                self.emit(Op::Star(v));
                self.store_to_target(e, v)?;
                self.free_to(v);
            }
            Pattern::Object { props, rest, .. } => {
                self.emit(Op::RequireObjectCoercible);
                let src = self.alloc_reg();
                self.emit(Op::Star(src));
                let keys = if rest.is_some() { self.alloc_regs(props.len() as u16) } else { 0 };
                for (i, pp) in props.iter().enumerate() {
                    match &pp.key {
                        PropKey::Computed(k) => {
                            let rk = if rest.is_some() { keys + i as u16 } else { self.alloc_reg() };
                            self.expr(k)?;
                            self.emit(Op::ToPropertyKey);
                            self.emit(Op::Star(rk));
                            self.emit(Op::GetKeyed(src));
                            if rest.is_none() {
                                // keep rk allocated until after the element
                            }
                            self.element(&pp.target, pp.default.as_ref(), init)?;
                            if rest.is_none() {
                                self.free_to(rk);
                            }
                        }
                        key => {
                            let kv = self.static_key(key);
                            let k = self.value_const(kv);
                            if rest.is_some() {
                                self.emit(Op::LdaConst(k));
                                self.emit(Op::Star(keys + i as u16));
                            }
                            self.emit(Op::GetNamed(src, k));
                            self.element(&pp.target, pp.default.as_ref(), init)?;
                        }
                    }
                }
                if let Some(r) = rest {
                    let t = self.alloc_reg();
                    self.emit(Op::NewObject);
                    self.emit(Op::Star(t));
                    self.emit(Op::Ldar(src));
                    self.emit(Op::CopyDataPropsExcept { target: t, from: keys, n: props.len() as u16 });
                    self.emit(Op::Ldar(t));
                    self.destructure(r, init)?;
                }
                self.free_to(src);
            }
            Pattern::Array { elems, rest, .. } => {
                let it = self.alloc_regs(3);
                self.emit(Op::GetIterator(it));
                let exc = self.alloc_reg();
                let ti = self.begin_try(exc);
                for el in elems {
                    let done = self.emit(Op::IteratorStep(it, 0));
                    match el {
                        None => {
                            self.patch_here(done);
                        }
                        Some(PatElem { target, default }) => {
                            let have = self.emit(Op::Jmp(0));
                            self.patch_here(done);
                            self.emit(Op::LdaUndef);
                            self.patch_here(have);
                            self.element(target, default.as_ref(), init)?;
                        }
                    }
                }
                if let Some(r) = rest {
                    if let Pattern::Expr(e) = &**r {
                        // Evaluate the target reference before collecting.
                        let _ = e;
                    }
                    self.emit(Op::IteratorRest(it));
                    self.destructure(r, init)?;
                }
                self.end_try(ti);
                self.emit(Op::IteratorClose(it));
                let end = self.emit(Op::Jmp(0));
                self.handler_here(ti);
                self.emit(Op::IteratorCloseThrow(it, exc));
                self.emit(Op::Ldar(exc));
                self.emit(Op::Throw);
                self.patch_here(end);
                self.free_to(it);
            }
        }
        Ok(())
    }

    /// acc = the value for one element: applies the default, then binds.
    fn element(&mut self, target: &'a Pattern, default: Option<&'a Expr>, init: bool) -> CResult<()> {
        if let Some(d) = default {
            let skip = self.emit(Op::JmpNotUndefined(0));
            self.named_expr(d, target)?;
            self.patch_here(skip);
        }
        self.destructure(target, init)
    }

    /// Stores register `v` into a member expression target.
    fn store_to_target(&mut self, e: &'a Expr, v: Reg) -> CResult<()> {
        match &e.unparen().kind {
            ExprKind::Member { obj, prop, .. } => {
                let r = self.alloc_reg();
                self.expr(obj)?;
                self.emit(Op::Star(r));
                match prop {
                    MemberProp::Name(n) => {
                        let k = self.name_const(n);
                        self.emit(Op::Ldar(v));
                        self.emit(Op::SetNamed(r, k));
                    }
                    MemberProp::Computed(k) => {
                        let rk = self.alloc_reg();
                        self.expr(k)?;
                        self.emit(Op::Star(rk));
                        self.emit(Op::Ldar(v));
                        self.emit(Op::SetKeyed(r, rk));
                    }
                    MemberProp::Private(n) => {
                        let rk = self.alloc_reg();
                        self.load_private_name(n)?;
                        self.emit(Op::Star(rk));
                        self.emit(Op::Ldar(v));
                        self.emit(Op::SetPrivate(r, rk));
                    }
                }
                self.free_to(r);
            }
            ExprKind::SuperMember(prop) => {
                let r = self.alloc_regs(2);
                self.load_this()?;
                self.emit(Op::Star(r));
                let rk = self.alloc_reg();
                if let MemberProp::Computed(k) = prop {
                    self.expr(k)?;
                    self.emit(Op::ToPropertyKey);
                    self.emit(Op::Star(rk));
                }
                self.emit(Op::Ldar(v));
                self.super_set(prop, r, rk)?;
                self.free_to(r);
            }
            _ => {
                self.expr(e)?;
                let k = self.name_const("Invalid destructuring assignment target");
                self.emit(Op::ThrowError(1, k));
            }
        }
        Ok(())
    }

    // ----- classes --------------------------------------------------------

    /// acc = the class constructor.
    pub fn class(&mut self, c: &'a Rc<Class>, name_hint: Option<Value>) -> CResult<()> {
        let mut region = Region::default();
        self.scan.class(c, &mut region);
        let dynamic = region.dynamic || region.has_direct_eval;
        // Class scope: name, private names, computed field keys.
        self.push_scope(ScopeKind::Block);
        if let Some(n) = &c.name {
            let cap = dynamic || region.captured.contains(n);
            self.declare(n.clone(), BKind::ClassName, cap, true, u32::MAX);
        }
        let mut privates: Vec<Name> = Vec::new();
        for m in &c.members {
            let key = match m {
                ClassMember::Method { key, .. } | ClassMember::Field { key, .. } => key,
                _ => continue,
            };
            if let PropKey::Private(n) = key {
                if !privates.contains(n) {
                    privates.push(n.clone());
                }
            }
        }
        for n in &privates {
            self.declare(format!("#{n}").into(), BKind::Pseudo, true, false, 0);
        }
        let mut computed_fields = 0u32;
        for m in &c.members {
            if let ClassMember::Field { key: PropKey::Computed(_), .. } = m {
                self.declare(format!("%fk{computed_fields}").into(), BKind::Pseudo, true, false, 0);
                computed_fields += 1;
            }
        }
        self.open_env();
        if let Some(n) = &c.name {
            self.set_uninit(n);
        }
        for n in &privates {
            let k = self.name_const(&format!("#{n}"));
            self.emit(Op::NewPrivateName(k));
            self.init_binding(&format!("#{n}"));
        }
        // Heritage.
        match &c.extends {
            Some(e) => {
                self.expr(e)?;
            }
            None => {
                self.emit(Op::LdaHole);
            }
        }
        let proto = self.alloc_reg();
        let ctor = self.alloc_reg();
        let derived = c.extends.is_some();
        let has_instance_fields = c.members.iter().any(|m| match m {
            ClassMember::Field { is_static: false, .. } => true,
            ClassMember::Method { is_static: false, key: PropKey::Private(_), .. } => true,
            _ => false,
        });
        let info = ClassCtorInfo { derived, has_instance_fields };
        let class_name = match (&c.name, name_hint) {
            (Some(n), _) => self.atom(n),
            (None, Some(h)) => h,
            (None, None) => Value::str(self.vm.names.empty),
        };
        let ctor_code = match &c.constructor {
            Some(f) => self.function(f, Some(class_name), Some(&info))?,
            None => self.default_constructor(derived, has_instance_fields, class_name, c)?,
        };
        let idx = {
            let fs = self.f();
            fs.children.push(ctor_code);
            (fs.children.len() - 1) as u32
        };
        let nk = self.value_const(class_name);
        self.pos(c.pos);
        self.emit(Op::CreateClass { code: idx, proto, name: nk });
        self.emit(Op::Star(ctor));
        // Members in order.
        let mut instance_fields: Vec<(&'a ClassMember, FieldKey)> = Vec::new();
        let mut static_elems: Vec<(&'a ClassMember, Option<FieldKey>)> = Vec::new();
        let mut ci = 0u32;
        for m in &c.members {
            match m {
                ClassMember::Method { key, kind, is_static, func } => {
                    let target = if *is_static { ctor } else { proto };
                    let rk = self.alloc_reg();
                    if let PropKey::Private(n) = key {
                        self.load_private_name(n)?;
                        self.emit(Op::Star(rk));
                        let pname = self.atom(&format!("#{n}"));
                        let name = self.method_name(pname, *kind);
                        let cidx = self.function_child(func, Some(name))?;
                        self.emit(Op::Closure(cidx));
                        self.emit(Op::AddPrivateMethod { ctor, key: rk, kind: method_kind(*kind), is_static: *is_static });
                    } else {
                        let static_name = self.load_key(key, rk)?;
                        let name = static_name.map(|n| self.method_name(n, *kind));
                        let cidx = self.function_child(func, name)?;
                        self.emit(Op::Closure(cidx));
                        if static_name.is_none() {
                            self.emit(Op::SetFunctionName(rk, prefix_of(*kind)));
                        }
                        self.emit(Op::DefineMethod { obj: target, key: rk, kind: method_kind(*kind), enumerable: false });
                    }
                    self.free_to(rk);
                }
                ClassMember::Field { key, is_static, .. } => {
                    let fk = match key {
                        PropKey::Computed(k) => {
                            self.expr(k)?;
                            self.emit(Op::ToPropertyKey);
                            self.init_binding(&format!("%fk{ci}"));
                            ci += 1;
                            FieldKey::Computed(ci - 1)
                        }
                        PropKey::Private(n) => FieldKey::Private(n.clone()),
                        other => FieldKey::Static(self.static_key(other)),
                    };
                    if *is_static {
                        static_elems.push((m, Some(fk)));
                    } else {
                        instance_fields.push((m, fk));
                    }
                }
                ClassMember::StaticBlock(_) => static_elems.push((m, None)),
            }
        }
        if !instance_fields.is_empty() {
            let idx = self.field_initializer(&instance_fields, false)?;
            self.emit(Op::Closure(idx));
            self.emit(Op::SetHome(proto));
            self.emit(Op::SetClassFields(ctor));
        }
        // The class binding is initialized before static elements run.
        if let Some(n) = &c.name {
            self.emit(Op::Ldar(ctor));
            self.init_binding(n);
        }
        if !static_elems.is_empty() {
            let list: Vec<(&'a ClassMember, FieldKey)> = Vec::new();
            let _ = list;
            let idx = self.static_initializer(&static_elems)?;
            let call = self.alloc_regs(2);
            self.emit(Op::Closure(idx));
            self.emit(Op::SetHome(ctor));
            self.emit(Op::Star(call));
            self.emit(Op::Mov(ctor, call + 1));
            self.emit(Op::Call(call, 0));
            self.free_to(call);
        }
        self.emit(Op::Ldar(ctor));
        self.pop_scope();
        Ok(())
    }

    fn default_constructor(&mut self, derived: bool, has_fields: bool, name: Value, c: &Class) -> CResult<Rc<Code>> {
        let kind = if derived { FuncKind::DerivedConstructor } else { FuncKind::ClassConstructor };
        let fs = self.new_func(kind, true, false, false);
        self.funcs.push(fs);
        self.push_scope(ScopeKind::Function);
        if derived {
            let base = self.alloc_regs(2);
            self.emit(Op::LdaCallee);
            self.emit(Op::Star(base));
            self.emit(Op::LdaNewTarget);
            self.emit(Op::Star(base + 1));
            self.emit(Op::SuperCallForward(base));
            // this = result; fields
            self.emit(Op::InitFieldsOf(base));
            self.emit(Op::Return);
        } else {
            if has_fields {
                let r = self.alloc_reg();
                self.emit(Op::LdaCallee);
                self.emit(Op::Star(r));
                self.emit(Op::LdaThis);
                self.emit(Op::InitFieldsOf(r));
            }
            self.emit(Op::LdaUndef);
            self.emit(Op::Return);
        }
        self.pop_scope_no_emit();
        Ok(self.finish(name, 0, 0, false, has_fields && !derived, (c.start, c.end)))
    }

    /// The synthetic method that defines the instance fields on `this`.
    fn field_initializer(&mut self, fields: &[(&'a ClassMember, FieldKey)], is_static: bool) -> CResult<u32> {
        let mut region = Region::default();
        for (m, _) in fields {
            if let ClassMember::Field { value: Some(v), .. } = m {
                self.scan.stmts(&v.body, &mut region);
            }
        }
        let fs = self.new_func(FuncKind::Method, true, false, false);
        self.funcs.push(fs);
        self.f().named_envs = region.dynamic;
        self.push_scope(ScopeKind::Function);
        self.declare_method_pseudos(&region);
        let this = self.alloc_reg();
        self.emit(Op::LdaThis);
        self.emit(Op::Star(this));
        for (m, fk) in fields {
            self.define_field(m, fk, this)?;
        }
        let _ = is_static;
        self.emit(Op::LdaUndef);
        self.emit(Op::Return);
        self.pop_scope_no_emit();
        let name = Value::str(self.vm.names.empty);
        let code = self.finish(name, 0, 0, false, false, (0, 0));
        let fs = self.f();
        fs.children.push(code);
        Ok((fs.children.len() - 1) as u32)
    }

    fn declare_method_pseudos(&mut self, region: &Region) {
        let dynamic = region.dynamic || region.has_direct_eval;
        if region.this || dynamic {
            self.declare("this".into(), BKind::Pseudo, true, false, 0);
        }
        if region.new_target || dynamic {
            self.declare("new.target".into(), BKind::Pseudo, true, false, 0);
        }
        if region.super_prop || dynamic {
            self.declare("%home".into(), BKind::Pseudo, true, false, 0);
        }
        self.open_env();
        if region.this || dynamic {
            self.emit(Op::LdaThis);
            self.init_binding("this");
        }
        if region.new_target || dynamic {
            self.emit(Op::LdaUndef);
            self.init_binding("new.target");
        }
        if region.super_prop || dynamic {
            self.emit(Op::LdaCallee);
            self.init_binding("%home");
        }
    }

    /// Defines one field on the object in `this` (acc clobbered).
    fn define_field(&mut self, m: &'a ClassMember, fk: &FieldKey, this: Reg) -> CResult<()> {
        let ClassMember::Field { value, .. } = m else { return Ok(()) };
        let rk = self.alloc_reg();
        let name: Option<Value> = match fk {
            FieldKey::Static(v) => {
                let k = self.value_const(*v);
                self.emit(Op::LdaConst(k));
                Some(*v)
            }
            FieldKey::Private(n) => {
                self.load_private_name(n)?;
                Some(self.atom(&format!("#{n}")))
            }
            FieldKey::Computed(i) => {
                self.load_name(&format!("%fk{i}"), Pos::default())?;
                None
            }
        };
        self.emit(Op::Star(rk));
        match value {
            Some(f) => {
                let StmtKind::Return(Some(e)) = &f.body[0].kind else { unreachable!() };
                if e.is_anonymous_function_definition() {
                    match name {
                        Some(n) => self.expr_named(e, n)?,
                        None => {
                            self.expr(e)?;
                            self.emit(Op::SetFunctionName(rk, 0));
                        }
                    }
                } else {
                    self.expr(e)?;
                }
            }
            None => {
                self.emit(Op::LdaUndef);
            }
        }
        if let FieldKey::Private(_) = fk {
            self.emit(Op::DefinePrivate(this, rk));
        } else {
            self.emit(Op::DefineField(this, rk));
        }
        self.free_to(rk);
        Ok(())
    }

    /// The synthetic method running static fields and blocks in order.
    fn static_initializer(&mut self, elems: &[(&'a ClassMember, Option<FieldKey>)]) -> CResult<u32> {
        let mut region = Region::default();
        for (m, _) in elems {
            match m {
                ClassMember::Field { value: Some(v), .. } => self.scan.stmts(&v.body, &mut region),
                ClassMember::StaticBlock(f) => {
                    let i = self.scan.function(f);
                    region.captured.extend(i.names.iter().cloned());
                    region.dynamic |= i.dynamic;
                }
                _ => {}
            }
        }
        let fs = self.new_func(FuncKind::Method, true, false, false);
        self.funcs.push(fs);
        self.f().named_envs = region.dynamic;
        self.push_scope(ScopeKind::Function);
        self.declare_method_pseudos(&region);
        let this = self.alloc_reg();
        self.emit(Op::LdaThis);
        self.emit(Op::Star(this));
        for (m, fk) in elems {
            match (m, fk) {
                (ClassMember::Field { .. }, Some(fk)) => self.define_field(m, fk, this)?,
                (ClassMember::StaticBlock(f), _) => {
                    let idx = self.function_child(f, None)?;
                    let call = self.alloc_regs(2);
                    self.emit(Op::Closure(idx));
                    self.emit(Op::SetHome(this));
                    self.emit(Op::Star(call));
                    self.emit(Op::Mov(this, call + 1));
                    self.emit(Op::Call(call, 0));
                    self.free_to(call);
                }
                _ => {}
            }
        }
        self.emit(Op::LdaUndef);
        self.emit(Op::Return);
        self.pop_scope_no_emit();
        let name = Value::str(self.vm.names.empty);
        let code = self.finish(name, 0, 0, false, false, (0, 0));
        let fs = self.f();
        fs.children.push(code);
        Ok((fs.children.len() - 1) as u32)
    }
}

fn binop(op: BinaryOp, r: Reg) -> Op {
    match op {
        BinaryOp::Add => Op::Add(r),
        BinaryOp::Sub => Op::Sub(r),
        BinaryOp::Mul => Op::Mul(r),
        BinaryOp::Div => Op::Div(r),
        BinaryOp::Mod => Op::Mod(r),
        BinaryOp::Exp => Op::Exp(r),
        BinaryOp::Shl => Op::Shl(r),
        BinaryOp::Shr => Op::Shr(r),
        BinaryOp::Ushr => Op::Ushr(r),
        BinaryOp::BitAnd => Op::BitAnd(r),
        BinaryOp::BitOr => Op::BitOr(r),
        BinaryOp::BitXor => Op::BitXor(r),
        _ => unreachable!(),
    }
}

fn method_kind(k: MethodKind) -> u8 {
    match k {
        MethodKind::Method => 0,
        MethodKind::Get => 1,
        MethodKind::Set => 2,
    }
}

fn prefix_of(k: MethodKind) -> u8 {
    method_kind(k)
}
