//! Global bindings, dynamic name lookup (`with`, sloppy eval) and direct
//! `eval`.

use alloc::rc::Rc;
use alloc::vec::Vec;

use super::props::PropDesc;
use super::*;
use crate::bytecode::{Const, GlobalDecls};
use crate::object::{attr, Key};

impl Vm {
    pub fn get_global(&mut self, name: StrId, throw: bool) -> JsResult<Value> {
        if let Some(&(v, _)) = self.global_lex.get(&name) {
            if v.is_uninit() {
                return Err(self.tdz_error(Value::str(name)));
            }
            return Ok(v);
        }
        let k = self.key_from_str(name);
        let g = self.global;
        if let Some(super::props::Slot::Data(v, _)) = self.raw_get_own(g, k) {
            return Ok(v);
        }
        if self.has_property(g, k)? {
            return self.get(g, k);
        }
        if throw {
            let n = self.str(name).to_rust_lossy();
            return Err(self.reference_error(&alloc::format!("{n} is not defined")));
        }
        Ok(Value::UNDEFINED)
    }

    pub fn set_global(&mut self, name: StrId, v: Value, strict: bool) -> JsResult<()> {
        if let Some(&(cur, is_const)) = self.global_lex.get(&name) {
            if cur.is_uninit() {
                return Err(self.tdz_error(Value::str(name)));
            }
            if is_const {
                let n = self.str(name).to_rust_lossy();
                return Err(self.type_error(&alloc::format!("Assignment to constant variable '{n}'.")));
            }
            self.global_lex.insert(name, (v, false));
            return Ok(());
        }
        let k = self.key_from_str(name);
        let g = self.global;
        if !self.has_property(g, k)? && strict {
            let n = self.str(name).to_rust_lossy();
            return Err(self.reference_error(&alloc::format!("{n} is not defined")));
        }
        if !self.set(g, k, v)? && strict {
            return Err(self.cannot_assign(k));
        }
        Ok(())
    }

    /// GlobalDeclarationInstantiation / EvalDeclarationInstantiation.
    pub fn declare_globals(&mut self, d: &Rc<GlobalDecls>, fi: usize) -> JsResult<()> {
        let g = self.global;
        let configurable = d.eval;
        let var_env = d.var_env_hops.map(|h| {
            let mut e = self.frames[fi].env;
            for _ in 0..h {
                e = self.envs.get(e.unwrap().0).parent;
            }
            e.unwrap()
        });
        if var_env.is_none() {
            // Checks against the global scope.
            for &(n, _) in &d.lexicals {
                let s = n.as_string().unwrap();
                if self.global_lex.contains_key(&s) {
                    return Err(self.redeclared(s));
                }
                let k = self.key_from_str(s);
                if let Some(p) = self.get_own_property(g, k)? {
                    if p.configurable == Some(false) {
                        return Err(self.redeclared(s));
                    }
                }
            }
            for n in d.vars.iter().chain(d.functions.iter().map(|f| &f.0)) {
                let s = n.as_string().unwrap();
                if self.global_lex.contains_key(&s) {
                    return Err(self.redeclared(s));
                }
            }
            for &(n, _) in &d.functions {
                let s = n.as_string().unwrap();
                let k = self.key_from_str(s);
                let ok = match self.get_own_property(g, k)? {
                    None => self.is_extensible(g)?,
                    Some(p) => p.configurable == Some(true) || (p.is_data() && p.writable == Some(true) && p.enumerable == Some(true)),
                };
                if !ok {
                    let name = self.str(s).to_rust_lossy();
                    return Err(self.type_error(&alloc::format!("cannot declare global function {name}")));
                }
            }
            for &n in &d.vars {
                let s = n.as_string().unwrap();
                let k = self.key_from_str(s);
                if self.get_own_property(g, k)?.is_none() && !self.is_extensible(g)? {
                    let name = self.str(s).to_rust_lossy();
                    return Err(self.type_error(&alloc::format!("cannot declare global variable {name}")));
                }
            }
        }
        // Functions.
        let code = self.frames[fi].code.clone();
        let env = self.frames[fi].env;
        for &(n, idx) in &d.functions {
            let child = code.children[idx as usize].clone();
            let f = Value::object(self.make_closure(child, env, None));
            let s = n.as_string().unwrap();
            match var_env {
                Some(ve) => self.declare_eval_var(ve, s, Some(f)),
                None => {
                    let k = self.key_from_str(s);
                    let existing = self.get_own_property(g, k)?;
                    let desc = if existing.is_none() || existing.unwrap().configurable == Some(true) {
                        PropDesc::data(f, attr::WRITABLE | attr::ENUMERABLE | if configurable { attr::CONFIGURABLE } else { 0 })
                    } else {
                        PropDesc { value: Some(f), ..Default::default() }
                    };
                    self.define_property_or_throw(g, k, desc)?;
                }
            }
        }
        for &n in &d.vars {
            let s = n.as_string().unwrap();
            match var_env {
                Some(ve) => self.declare_eval_var(ve, s, None),
                None => {
                    let k = self.key_from_str(s);
                    if self.get_own_property(g, k)?.is_none() {
                        self.define_property_or_throw(g, k, PropDesc::data(Value::UNDEFINED, attr::WRITABLE | attr::ENUMERABLE | if configurable { attr::CONFIGURABLE } else { 0 }))?;
                    }
                }
            }
        }
        for &(n, c) in &d.lexicals {
            self.global_lex.insert(n.as_string().unwrap(), (Value::UNINIT, c));
        }
        Ok(())
    }

    fn redeclared(&mut self, s: StrId) -> Throw {
        let n = self.str(s).to_rust_lossy();
        self.syntax_error(&alloc::format!("Identifier '{n}' has already been declared"))
    }

    /// A var created by sloppy direct eval in a function scope.
    fn declare_eval_var(&mut self, env: EnvId, name: StrId, value: Option<Value>) {
        let e = self.envs.get(env.0);
        if let Some(names) = &e.names {
            if let Some(i) = names.chunks(2).position(|p| p[0] == Value::str(name)) {
                if let Some(v) = value {
                    self.envs.get_mut(env.0).slots[i] = v;
                }
                return;
            }
        }
        let e = self.envs.get_mut(env.0);
        let d = e.eval_vars.get_or_insert_with(Default::default);
        let k = Key::Str(name);
        match (d.get(k), value) {
            (Some(_), None) => {}
            (_, v) => d.set(k, v.unwrap_or(Value::UNDEFINED), attr::DEFAULT),
        }
    }

    /// Looks a name up through the environment chain: (value, this).
    pub fn get_name(&mut self, fi: usize, name: StrId, throw: bool) -> JsResult<(Value, Value)> {
        let nv = Value::str(name);
        let mut e = self.frames[fi].env;
        while let Some(id) = e {
            if let Some(o) = self.envs.get(id.0).with {
                if self.with_has(o, name)? {
                    let k = self.key_from_str(name);
                    let v = self.get(o, k)?;
                    return Ok((v, Value::object(o)));
                }
            }
            let env = self.envs.get(id.0);
            if let Some(names) = &env.names {
                if let Some(i) = names.chunks(2).position(|p| p[0] == nv) {
                    let v = env.slots[i];
                    if v.is_uninit() {
                        return Err(self.tdz_error(nv));
                    }
                    return Ok((v, Value::UNDEFINED));
                }
            }
            if let Some(d) = &env.eval_vars {
                if let Some((v, _)) = d.get(Key::Str(name)) {
                    return Ok((v, Value::UNDEFINED));
                }
            }
            e = env.parent;
        }
        Ok((self.get_global(name, throw)?, Value::UNDEFINED))
    }

    /// HasBinding of an object environment record (with @@unscopables).
    fn with_has(&mut self, o: ObjId, name: StrId) -> JsResult<bool> {
        let k = self.key_from_str(name);
        if !self.has_property(o, k)? {
            return Ok(false);
        }
        let ku = Key::Sym(self.intrinsics.sym_unscopables);
        let u = self.get(o, ku)?;
        if let Some(uo) = u.as_object() {
            let b = self.get(uo, k)?;
            if self.to_boolean(b) {
                return Ok(false);
            }
        }
        Ok(true)
    }

    pub fn set_name(&mut self, fi: usize, name: StrId, v: Value, strict: bool) -> JsResult<()> {
        let nv = Value::str(name);
        let mut e = self.frames[fi].env;
        while let Some(id) = e {
            if let Some(o) = self.envs.get(id.0).with {
                if self.with_has(o, name)? {
                    let k = self.key_from_str(name);
                    // Re-check existence (it may have been deleted).
                    if !self.has_property(o, k)? && strict {
                        let n = self.str(name).to_rust_lossy();
                        return Err(self.reference_error(&alloc::format!("{n} is not defined")));
                    }
                    if !self.set(o, k, v)? && strict {
                        return Err(self.cannot_assign(k));
                    }
                    return Ok(());
                }
            }
            let env = self.envs.get(id.0);
            if let Some(names) = &env.names {
                if let Some(i) = names.chunks(2).position(|p| p[0] == nv) {
                    let kind = names[i * 2 + 1].as_int().unwrap_or(0);
                    if env.slots[i].is_uninit() {
                        return Err(self.tdz_error(nv));
                    }
                    match kind {
                        1 => {
                            let n = self.str(name).to_rust_lossy();
                            return Err(self.type_error(&alloc::format!("Assignment to constant variable '{n}'.")));
                        }
                        2 => {
                            if strict {
                                let n = self.str(name).to_rust_lossy();
                                return Err(self.type_error(&alloc::format!("Assignment to constant variable '{n}'.")));
                            }
                            return Ok(());
                        }
                        _ => {}
                    }
                    self.envs.get_mut(id.0).slots[i] = v;
                    return Ok(());
                }
            }
            if let Some(d) = &env.eval_vars {
                if let Some((_, a)) = d.get(Key::Str(name)) {
                    self.envs.get_mut(id.0).eval_vars.as_mut().unwrap().set(Key::Str(name), v, a);
                    return Ok(());
                }
            }
            e = env.parent;
        }
        self.set_global(name, v, strict)
    }

    pub fn delete_name(&mut self, fi: usize, name: StrId) -> JsResult<bool> {
        let nv = Value::str(name);
        let mut e = self.frames[fi].env;
        while let Some(id) = e {
            if let Some(o) = self.envs.get(id.0).with {
                if self.with_has(o, name)? {
                    let k = self.key_from_str(name);
                    return self.delete(o, k);
                }
            }
            let env = self.envs.get(id.0);
            if let Some(names) = &env.names {
                if names.chunks(2).any(|p| p[0] == nv) {
                    return Ok(false);
                }
            }
            if let Some(d) = &env.eval_vars {
                if d.get(Key::Str(name)).is_some() {
                    self.envs.get_mut(id.0).eval_vars.as_mut().unwrap().remove(Key::Str(name));
                    return Ok(true);
                }
            }
            e = env.parent;
        }
        if self.global_lex.contains_key(&name) {
            return Ok(false);
        }
        let k = self.key_from_str(name);
        let g = self.global;
        self.delete(g, k)
    }

    /// Direct `eval(arg)` from frame `fi`.
    pub fn direct_eval(&mut self, fi: usize, arg: Value, k: u32) -> JsResult<Value> {
        let Some(s) = arg.as_string() else { return Ok(arg) };
        let code = self.frames[fi].code.clone();
        let Const::EvalScope(scope) = &code.consts[k as usize] else { unreachable!() };
        let scope = scope.clone();
        let src = self.str(s).to_rust_lossy();
        let ctx = crate::parser::EvalCtx {
            strict: scope.strict,
            in_function: scope.in_function,
            super_prop: scope.super_prop,
            super_call: scope.super_call,
            new_target: scope.new_target,
            in_class_field: scope.in_class_field,
        };
        let prog = match crate::parser::Parser::parse_eval(&src, ctx) {
            Ok(p) => p,
            Err(e) => return Err(self.syntax_error(&e.message)),
        };
        let source = self.add_source("eval", src.as_str().into());
        let compiled = {
            let mut c = crate::compiler::Compiler::new(self, source);
            c.direct_eval(&prog, &scope)
        };
        let compiled = match compiled {
            Ok(c) => c,
            Err(e) => return Err(self.syntax_error(&e.message)),
        };
        let f = &self.frames[fi];
        let (env, this, callee, nt) = (f.env, f.this, f.callee, f.new_target);
        self.run_code(compiled, env, this, callee, nt)
    }

    /// Indirect eval / `eval` called as a function: global scope.
    pub fn global_eval(&mut self, src: &str) -> JsResult<Value> {
        let prog = match crate::parser::Parser::parse_script(src, false) {
            Ok(p) => p,
            Err(e) => return Err(self.syntax_error(&e.message)),
        };
        let source = self.add_source("eval", src.into());
        let compiled = {
            let mut c = crate::compiler::Compiler::new(self, source);
            c.script(&prog, true)
        };
        let compiled = match compiled {
            Ok(c) => c,
            Err(e) => return Err(self.syntax_error(&e.message)),
        };
        let this = Value::object(self.global_this);
        self.run_code(compiled, None, this, Value::UNDEFINED, Value::UNDEFINED)
    }

    pub fn add_source(&mut self, name: &str, text: Rc<str>) -> Rc<Source> {
        let id = self.sources.len() as u32;
        let s = Rc::new(Source { name: name.into(), text, id });
        if name != "eval" {
            self.sources.push(s.clone());
        }
        s
    }

    /// Parses and runs a script in the global scope.
    pub fn eval_script(&mut self, src: &str, name: &str) -> JsResult<Value> {
        let prog = match crate::parser::Parser::parse_script(src, false) {
            Ok(p) => p,
            Err(e) => {
                let m = alloc::format!("{} ({}:{}:{})", e.message, name, e.line, e.col);
                return Err(self.syntax_error(&m));
            }
        };
        let source = self.add_source(name, src.into());
        let compiled = {
            let mut c = crate::compiler::Compiler::new(self, source);
            c.script(&prog, false)
        };
        let compiled = match compiled {
            Ok(c) => c,
            Err(e) => {
                let m = alloc::format!("{} ({}:{}:{})", e.message, name, e.line, e.col);
                return Err(self.syntax_error(&m));
            }
        };
        let this = Value::object(self.global_this);
        self.run_code(compiled, None, this, Value::UNDEFINED, Value::UNDEFINED)
    }

    /// Runs queued promise jobs until the queue is empty.
    pub fn run_jobs(&mut self) -> JsResult<()> {
        while let Some(job) = self.jobs.pop_front() {
            self.native_roots.push(job.func);
            let r = self.call(job.func, job.this, &job.args);
            self.native_roots.pop();
            // ClearKeptObjects (WeakRef targets kept alive during a job).
            self.kept_alive.clear();
            match r {
                Ok(_) => {}
                Err(Throw::Terminate) => return Err(Throw::Terminate),
                Err(Throw::Value(e)) => {
                    // A job's exception is reported, not propagated.
                    self.report_error(e);
                }
            }
        }
        Ok(())
    }

    pub fn report_error(&mut self, e: Value) {
        let s = self.error_to_string(e);
        if let Some(h) = self.host.as_mut() {
            h.uncaught_error(&s);
        }
    }

    /// "Name: message\n    at …" for an error value.
    pub fn error_to_string(&mut self, e: Value) -> alloc::string::String {
        let mut out = alloc::string::String::new();
        if let Some(o) = e.as_object() {
            let kn = Key::Str(self.names.name);
            let km = Key::Str(self.names.message);
            let n = self.get(o, kn).ok().map(|v| self.display(v)).unwrap_or_default();
            let m = self.get(o, km).ok().map(|v| self.display(v)).unwrap_or_default();
            if n.is_empty() {
                out.push_str(&m);
            } else if m.is_empty() {
                out.push_str(&n);
            } else {
                out.push_str(&alloc::format!("{n}: {m}"));
            }
            let ks = Key::Str(self.names.stack);
            if let Some(super::props::Slot::Data(st, _)) = self.raw_get_own(o, ks) {
                let s = self.display(st);
                if !s.is_empty() {
                    out.push('\n');
                    out.push_str(&s);
                }
            }
        } else {
            out.push_str(&self.display(e));
        }
        out
    }

    pub fn _unused_vec(&self) -> Vec<u8> {
        Vec::new()
    }
}
