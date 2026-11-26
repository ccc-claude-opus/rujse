//! ES modules (ECMA-262 §16.2): loading (the host may answer later),
//! linking, evaluation with top-level await, namespaces, `import()`,
//! `import.meta`, JSON modules.

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::format;
use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec::Vec;

use crate::builtins::promise;
use crate::compiler::ModuleInfo;
use crate::object::{attr, Class, Key};
use crate::value::{ObjId, Value};
use crate::vm::props::PropDesc;
use crate::vm::{EnvId, JsResult, Throw, Vm};

/// What the host returns when asked for a module's source.
pub enum ModuleSource {
    /// The source text.
    Ready(String),
    /// The host will call `Vm::finish_loading(url, …)` later.
    Pending,
    /// The module cannot be loaded (becomes a TypeError).
    Error(String),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Status {
    Unlinked,
    Linking,
    Linked,
    Evaluating,
    EvaluatingAsync,
    Evaluated,
}

/// Where an export resolves to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Res {
    Binding(u32, u16),
    Namespace(u32),
    Ambiguous,
}

pub struct Record {
    pub url: String,
    pub status: Status,
    pub info: ModuleInfo,
    pub loaded: Vec<Option<u32>>,
    pub env: Option<EnvId>,
    pub namespace: Option<ObjId>,
    pub ns_names: Vec<(String, Res)>,
    pub error: Option<Value>,
    pub dfs_index: u32,
    pub dfs_anc: u32,
    pub cycle_root: u32,
    pub async_eval: bool,
    pub async_order: u64,
    pub pending_async: u32,
    pub async_parents: Vec<u32>,
    pub top_promise: Option<Value>,
    pub meta: Option<ObjId>,
    /// A JSON module's value.
    pub json: Option<Value>,
}

struct Graph {
    promise: Value,
    pending: u32,
    visited: Vec<u32>,
    failed: bool,
    root: Option<u32>,
}

#[derive(Default)]
pub struct Modules {
    pub list: Vec<Record>,
    pub by_key: BTreeMap<String, u32>,
    graphs: Vec<Option<Graph>>,
    /// key -> waiters (graph, importing module and request index)
    waiting: BTreeMap<String, Vec<(usize, Option<(u32, usize)>)>>,
    async_counter: u64,
}

impl Modules {
    pub fn trace(&self, out: &mut Vec<Value>, envs: &mut Vec<EnvId>) {
        for r in &self.list {
            if let Some(e) = r.env {
                envs.push(e);
            }
            if let Some(n) = r.namespace {
                out.push(Value::object(n));
            }
            if let Some(m) = r.meta {
                out.push(Value::object(m));
            }
            out.extend(r.error);
            out.extend(r.top_promise);
            out.extend(r.json);
        }
        for g in self.graphs.iter().flatten() {
            out.push(g.promise);
        }
    }
}

fn key_of(url: &str, attrs: &[(String, String)]) -> String {
    match attrs.iter().find(|(k, _)| k == "type") {
        Some((_, t)) => format!("{url}#type={t}"),
        None => String::from(url),
    }
}

impl Vm {
    fn m(&self, i: u32) -> &Record {
        &self.modules.list[i as usize]
    }

    fn mm(&mut self, i: u32) -> &mut Record {
        &mut self.modules.list[i as usize]
    }

    fn new_promise_value(&mut self) -> Value {
        let pp = self.intrinsics.promise_prototype;
        Value::object(promise::new_promise(self, pp))
    }

    /// Parses and compiles a module (or a JSON module) into a record.
    fn create_record(&mut self, url: &str, key: &str, src: &str, json: bool) -> Result<u32, Value> {
        let info = if json {
            let mut info = ModuleInfo::default();
            info.local_exports.push((String::from("default"), 0));
            info.env_slots = 1;
            info
        } else {
            let prog = match crate::parser::Parser::parse_module(src) {
                Ok(p) => p,
                Err(e) => {
                    let m = format!("{} ({}:{}:{})", e.message, url, e.line, e.col);
                    let Throw::Value(v) = self.syntax_error(&m) else { unreachable!() };
                    return Err(v);
                }
            };
            let source = self.add_source(url, src.into());
            let r = {
                let mut c = crate::compiler::Compiler::new(self, source);
                c.module(&prog)
            };
            match r {
                Ok(i) => i,
                Err(e) => {
                    let m = format!("{} ({}:{}:{})", e.message, url, e.line, e.col);
                    let Throw::Value(v) = self.syntax_error(&m) else { unreachable!() };
                    return Err(v);
                }
            }
        };
        let json_value = if json {
            let s = self.new_string(src);
            let parse = {
                let j = self.intrinsics.json;
                let k = self.key("parse");
                self.get(j, k)
            };
            match parse.and_then(|f| self.call(f, Value::UNDEFINED, &[s])) {
                Ok(v) => Some(v),
                Err(Throw::Value(e)) => return Err(e),
                Err(_) => return Err(Value::UNDEFINED),
            }
        } else {
            None
        };
        let n = info.requests.len();
        let idx = self.modules.list.len() as u32;
        self.modules.list.push(Record {
            url: url.into(),
            status: Status::Unlinked,
            info,
            loaded: alloc::vec![None; n],
            env: None,
            namespace: None,
            ns_names: Vec::new(),
            error: None,
            dfs_index: 0,
            dfs_anc: 0,
            cycle_root: idx,
            async_eval: false,
            async_order: 0,
            pending_async: 0,
            async_parents: Vec::new(),
            top_promise: None,
            meta: None,
            json: json_value,
        });
        self.modules.by_key.insert(key.into(), idx);
        Ok(idx)
    }

    // ----- loading -------------------------------------------------------

    /// Imports a module by specifier (like `import()`): returns a promise
    /// for its namespace. `referrer` is the importing script/module URL.
    pub fn import_module(&mut self, specifier: &str, referrer: Option<&str>) -> Value {
        let p = self.new_promise_value();
        let g = self.new_graph(p);
        self.host_load(g, None, referrer, specifier, &[]);
        p
    }

    fn new_graph(&mut self, promise: Value) -> usize {
        let g = Graph { promise, pending: 1, visited: Vec::new(), failed: false, root: None };
        match self.modules.graphs.iter().position(|x| x.is_none()) {
            Some(i) => {
                self.modules.graphs[i] = Some(g);
                i
            }
            None => {
                self.modules.graphs.push(Some(g));
                self.modules.graphs.len() - 1
            }
        }
    }

    /// HostLoadImportedModule.
    fn host_load(&mut self, g: usize, from: Option<(u32, usize)>, referrer: Option<&str>, spec: &str, attrs: &[(String, String)]) {
        for (k, v) in attrs {
            if k != "type" || (v != "json" && v != "javascript") {
                let Throw::Value(e) = self.syntax_error(&format!("Unsupported import attribute {k}: {v}")) else { return };
                return self.finish_load(g, from, Err(e));
            }
        }
        let resolved = match self.host.as_mut() {
            Some(h) => h.resolve_module(referrer, spec),
            None => Err(String::from("module loading is not supported")),
        };
        let url = match resolved {
            Ok(u) => u,
            Err(m) => {
                let Throw::Value(e) = self.type_error(&m) else { return };
                return self.finish_load(g, from, Err(e));
            }
        };
        let json = attrs.iter().any(|(k, v)| k == "type" && v == "json");
        let key = key_of(&url, attrs);
        if let Some(&m) = self.modules.by_key.get(&key) {
            return self.finish_load(g, from, Ok(m));
        }
        if let Some(w) = self.modules.waiting.get_mut(&key) {
            w.push((g, from));
            return;
        }
        let src = match self.host.as_mut() {
            Some(h) => h.fetch_module(&url),
            None => ModuleSource::Error(String::from("module loading is not supported")),
        };
        match src {
            ModuleSource::Ready(s) => {
                let r = self.create_record(&url, &key, &s, json);
                self.finish_load(g, from, r);
            }
            ModuleSource::Pending => {
                self.modules.waiting.insert(key, alloc::vec![(g, from)]);
            }
            ModuleSource::Error(m) => {
                let Throw::Value(e) = self.type_error(&m) else { return };
                self.finish_load(g, from, Err(e));
            }
        }
    }

    /// The host delivers a module it answered `Pending` for.
    pub fn finish_loading(&mut self, url: &str, result: Result<String, String>) {
        let keys: Vec<String> = self.modules.waiting.keys().filter(|k| *k == url || k.starts_with(&format!("{url}#type="))).cloned().collect();
        for key in keys {
            let waiters = self.modules.waiting.remove(&key).unwrap_or_default();
            let json = key.ends_with("#type=json");
            let r = match &result {
                Ok(src) => self.create_record(url, &key, src, json),
                Err(m) => {
                    let Throw::Value(e) = self.type_error(m) else { continue };
                    Err(e)
                }
            };
            for (g, from) in waiters {
                self.finish_load(g, from, r);
            }
        }
    }

    /// FinishLoadingImportedModule + ContinueModuleLoading.
    fn finish_load(&mut self, g: usize, from: Option<(u32, usize)>, r: Result<u32, Value>) {
        let Some(graph) = self.modules.graphs[g].as_ref() else { return };
        if graph.failed {
            return self.graph_done_one(g);
        }
        match r {
            Ok(m) => {
                match from {
                    Some((importer, i)) => self.mm(importer).loaded[i] = Some(m),
                    None => self.modules.graphs[g].as_mut().unwrap().root = Some(m),
                }
                self.inner_load(g, m);
            }
            Err(e) => {
                let gr = self.modules.graphs[g].as_mut().unwrap();
                gr.failed = true;
                let p = gr.promise;
                promise::reject_promise(self, p, e);
            }
        }
        self.graph_done_one(g);
    }

    fn inner_load(&mut self, g: usize, m: u32) {
        {
            let gr = self.modules.graphs[g].as_mut().unwrap();
            if gr.visited.contains(&m) {
                return;
            }
            gr.visited.push(m);
        }
        if self.m(m).status != Status::Unlinked {
            return;
        }
        let n = self.m(m).info.requests.len();
        for i in 0..n {
            if self.modules.graphs[g].as_ref().unwrap().failed {
                return;
            }
            match self.m(m).loaded[i] {
                Some(t) => self.inner_load(g, t),
                None => {
                    self.modules.graphs[g].as_mut().unwrap().pending += 1;
                    let (spec, attrs) = self.m(m).info.requests[i].clone();
                    let referrer = self.m(m).url.clone();
                    self.host_load(g, Some((m, i)), Some(&referrer), &spec, &attrs);
                }
            }
        }
    }

    fn graph_done_one(&mut self, g: usize) {
        let gr = self.modules.graphs[g].as_mut().unwrap();
        gr.pending -= 1;
        if gr.pending > 0 {
            return;
        }
        let gr = self.modules.graphs[g].take().unwrap();
        if gr.failed {
            return;
        }
        let root = gr.root.unwrap();
        // Link, evaluate, then resolve with the namespace.
        if let Err(e) = self.link(root) {
            if let Throw::Value(v) = e {
                promise::reject_promise(self, gr.promise, v);
            }
            return;
        }
        let ep = self.evaluate_module(root);
        let ns = match self.namespace_of(root) {
            Ok(n) => n,
            Err(Throw::Value(v)) => {
                promise::reject_promise(self, gr.promise, v);
                return;
            }
            Err(_) => return,
        };
        let on_ful = self.native_fn_data("", 1, |vm, _, _, _| {
            let p = vm.native_data(0);
            let ns = vm.native_data(1);
            promise::resolve_promise(vm, p, ns)?;
            Ok(Value::UNDEFINED)
        }, false, Box::new([gr.promise, Value::object(ns)]));
        let on_rej = self.native_fn_data("", 1, |vm, _, a, _| {
            let p = vm.native_data(0);
            promise::reject_promise(vm, p, a.first().copied().unwrap_or(Value::UNDEFINED));
            Ok(Value::UNDEFINED)
        }, false, Box::new([gr.promise]));
        promise::perform_then(self, ep.as_object().unwrap(), Value::object(on_ful), Value::object(on_rej), [Value::UNDEFINED; 3]);
    }

    // ----- linking -------------------------------------------------------

    fn link(&mut self, m: u32) -> JsResult<()> {
        let mut stack = Vec::new();
        if let Err(e) = self.inner_link(m, &mut stack, 0) {
            for s in stack {
                self.mm(s).status = Status::Unlinked;
                self.mm(s).env = None;
            }
            return Err(e);
        }
        Ok(())
    }

    fn inner_link(&mut self, m: u32, stack: &mut Vec<u32>, mut index: u32) -> JsResult<u32> {
        match self.m(m).status {
            Status::Linking | Status::Linked | Status::EvaluatingAsync | Status::Evaluated | Status::Evaluating => return Ok(index),
            Status::Unlinked => {}
        }
        {
            let r = self.mm(m);
            r.status = Status::Linking;
            r.dfs_index = index;
            r.dfs_anc = index;
        }
        index += 1;
        stack.push(m);
        for i in 0..self.m(m).loaded.len() {
            let req = self.m(m).loaded[i].unwrap();
            index = self.inner_link(req, stack, index)?;
            if self.m(req).status == Status::Linking {
                let a = self.m(m).dfs_anc.min(self.m(req).dfs_anc);
                self.mm(m).dfs_anc = a;
            }
        }
        self.initialize_environment(m)?;
        if self.m(m).dfs_anc == self.m(m).dfs_index {
            while let Some(r) = stack.pop() {
                self.mm(r).status = Status::Linked;
                if r == m {
                    break;
                }
            }
        }
        Ok(index)
    }

    /// ResolveExport.
    fn resolve_export(&mut self, m: u32, name: &str, set: &mut Vec<(u32, String)>) -> Option<Res> {
        if set.iter().any(|(x, n)| *x == m && n == name) {
            return None;
        }
        set.push((m, name.into()));
        if let Some(&(_, slot)) = self.m(m).info.local_exports.iter().find(|(n, _)| n == name) {
            return Some(Res::Binding(m, slot));
        }
        let ind = self.m(m).info.indirect_exports.iter().find(|(n, _, _)| n == name).cloned();
        if let Some((_, r, imp)) = ind {
            let target = self.m(m).loaded[r]?;
            return match imp {
                None => Some(Res::Namespace(target)),
                Some(i) => self.resolve_export(target, &i, set),
            };
        }
        if name == "default" {
            return None;
        }
        let mut star: Option<Res> = None;
        for r in self.m(m).info.star_exports.clone() {
            let target = self.m(m).loaded[r]?;
            match self.resolve_export(target, name, set) {
                Some(Res::Ambiguous) => return Some(Res::Ambiguous),
                Some(res) => match star {
                    None => star = Some(res),
                    Some(s) if s != res => return Some(Res::Ambiguous),
                    _ => {}
                },
                None => {}
            }
        }
        star
    }

    fn initialize_environment(&mut self, m: u32) -> JsResult<()> {
        for (name, _, _) in self.m(m).info.indirect_exports.clone() {
            match self.resolve_export(m, &name, &mut Vec::new()) {
                Some(Res::Ambiguous) | None => {
                    return Err(self.syntax_error(&format!("The requested export '{name}' cannot be resolved")));
                }
                _ => {}
            }
        }
        let n = self.m(m).info.env_slots as usize;
        // An importer earlier in a cycle may have created it already.
        let env = match self.m(m).env {
            Some(e) => e,
            None => self.new_env(n, Value::UNDEFINED, None),
        };
        self.mm(m).env = Some(env);
        if let Some(v) = self.m(m).json {
            self.envs.get_mut(env.0).slots[0] = v;
            return Ok(());
        }
        let mut table: Vec<(EnvId, u16)> = alloc::vec![(env, 0); self.m(m).info.import_count as usize];
        for imp in self.m(m).info.imports.clone() {
            let target = self.m(m).loaded[imp.request].unwrap();
            match imp.name {
                None => {
                    let ns = self.namespace_of(target)?;
                    self.envs.get_mut(env.0).slots[imp.index as usize] = Value::object(ns);
                }
                Some(name) => match self.resolve_export(target, &name, &mut Vec::new()) {
                    Some(Res::Binding(tm, slot)) => {
                        let te = match self.m(tm).env {
                            Some(e) => e,
                            None => {
                                // Not yet linked (a cycle): link order makes
                                // its environment exist before evaluation;
                                // create it now.
                                let n = self.m(tm).info.env_slots as usize;
                                let e = self.new_env(n, Value::UNDEFINED, None);
                                self.mm(tm).env = Some(e);
                                e
                            }
                        };
                        table[imp.index as usize] = (te, slot);
                    }
                    Some(Res::Namespace(tm)) => {
                        let ns = self.namespace_of(tm)?;
                        let holder = self.new_env(1, Value::object(ns), None);
                        table[imp.index as usize] = (holder, 0);
                    }
                    _ => {
                        return Err(self.syntax_error(&format!("The requested module does not provide an export named '{name}'")));
                    }
                },
            }
        }
        self.envs.get_mut(env.0).imports = Some(table.into());
        // Hoisted functions and TDZ.
        let init = self.m(m).info.init.clone().unwrap();
        self.run_code(init, Some(env), Value::UNDEFINED, Value::UNDEFINED, Value::UNDEFINED)?;
        Ok(())
    }

    /// GetExportedNames.
    fn exported_names(&mut self, m: u32, set: &mut Vec<u32>) -> Vec<String> {
        if set.contains(&m) {
            return Vec::new();
        }
        set.push(m);
        let mut out: Vec<String> = self.m(m).info.local_exports.iter().map(|x| x.0.clone()).collect();
        out.extend(self.m(m).info.indirect_exports.iter().map(|x| x.0.clone()));
        for r in self.m(m).info.star_exports.clone() {
            let Some(t) = self.m(m).loaded[r] else { continue };
            for n in self.exported_names(t, set) {
                if n != "default" && !out.contains(&n) {
                    out.push(n);
                }
            }
        }
        out
    }

    /// GetModuleNamespace.
    pub fn namespace_of(&mut self, m: u32) -> JsResult<ObjId> {
        if let Some(n) = self.m(m).namespace {
            return Ok(n);
        }
        let names = self.exported_names(m, &mut Vec::new());
        let mut list: Vec<(String, Res)> = Vec::new();
        for n in names {
            if let Some(r) = self.resolve_export(m, &n, &mut Vec::new()) {
                if r != Res::Ambiguous {
                    list.push((n, r));
                }
            }
        }
        list.sort_by(|a, b| a.0.encode_utf16().cmp(b.0.encode_utf16()));
        let o = self.alloc_object(None, Class::Namespace(m));
        let tag = self.new_string("Module");
        let kt = Key::Sym(self.intrinsics.sym_to_string_tag);
        self.define_data(o, kt, tag, attr::NONE);
        self.obj_mut(o).extensible = false;
        self.mm(m).ns_names = list;
        self.mm(m).namespace = Some(o);
        Ok(o)
    }

    // ----- evaluation ----------------------------------------------------

    /// Evaluate(): a promise for the module's evaluation.
    pub fn evaluate_module(&mut self, mut m: u32) -> Value {
        if matches!(self.m(m).status, Status::EvaluatingAsync | Status::Evaluated) {
            m = self.m(m).cycle_root;
        }
        if let Some(p) = self.m(m).top_promise {
            return p;
        }
        let p = self.new_promise_value();
        self.mm(m).top_promise = Some(p);
        let mut stack = Vec::new();
        match self.inner_eval(m, &mut stack, 0) {
            Err(Throw::Value(e)) => {
                for s in stack {
                    let r = self.mm(s);
                    r.status = Status::Evaluated;
                    r.error = Some(e);
                }
                promise::reject_promise(self, p, e);
            }
            Err(Throw::Terminate) => {}
            Ok(_) => {
                if !self.m(m).async_eval {
                    let _ = promise::resolve_promise(self, p, Value::UNDEFINED);
                }
            }
        }
        p
    }

    fn inner_eval(&mut self, m: u32, stack: &mut Vec<u32>, mut index: u32) -> JsResult<u32> {
        match self.m(m).status {
            Status::EvaluatingAsync | Status::Evaluated => {
                return match self.m(m).error {
                    Some(e) => Err(Throw::Value(e)),
                    None => Ok(index),
                };
            }
            Status::Evaluating => return Ok(index),
            _ => {}
        }
        {
            let r = self.mm(m);
            r.status = Status::Evaluating;
            r.dfs_index = index;
            r.dfs_anc = index;
            r.pending_async = 0;
        }
        index += 1;
        stack.push(m);
        for i in 0..self.m(m).loaded.len() {
            let mut req = self.m(m).loaded[i].unwrap();
            index = self.inner_eval(req, stack, index)?;
            if self.m(req).status == Status::Evaluating {
                let a = self.m(m).dfs_anc.min(self.m(req).dfs_anc);
                self.mm(m).dfs_anc = a;
            } else {
                req = self.m(req).cycle_root;
                if let Some(e) = self.m(req).error {
                    return Err(Throw::Value(e));
                }
            }
            if self.m(req).async_eval {
                self.mm(m).pending_async += 1;
                self.mm(req).async_parents.push(m);
            }
        }
        if self.m(m).pending_async > 0 || self.m(m).info.has_tla {
            let order = self.modules.async_counter;
            self.modules.async_counter += 1;
            let r = self.mm(m);
            r.async_eval = true;
            r.async_order = order;
            if r.pending_async == 0 {
                self.execute_async(m)?;
            }
        } else {
            self.execute_sync(m)?;
        }
        if self.m(m).dfs_anc == self.m(m).dfs_index {
            while let Some(r) = stack.pop() {
                let st = if self.m(r).async_eval { Status::EvaluatingAsync } else { Status::Evaluated };
                let rr = self.mm(r);
                rr.status = st;
                rr.cycle_root = m;
                if r == m {
                    break;
                }
            }
        }
        Ok(index)
    }

    fn execute_sync(&mut self, m: u32) -> JsResult<()> {
        if self.m(m).json.is_some() {
            return Ok(());
        }
        let body = self.m(m).info.body.clone().unwrap();
        let env = self.m(m).env;
        self.run_code(body, env, Value::UNDEFINED, Value::UNDEFINED, Value::UNDEFINED)?;
        Ok(())
    }

    /// ExecuteAsyncModule: runs a module body with top-level await.
    fn execute_async(&mut self, m: u32) -> JsResult<()> {
        let body = self.m(m).info.body.clone().unwrap();
        let env = self.m(m).env;
        let p = self.run_async_code(body, env)?;
        let on_ful = self.native_fn_data("", 1, |vm, _, _, _| {
            let m = vm.native_data(0).as_int().unwrap() as u32;
            vm.async_module_fulfilled(m);
            Ok(Value::UNDEFINED)
        }, false, Box::new([Value::int(m as i32)]));
        let on_rej = self.native_fn_data("", 1, |vm, _, a, _| {
            let m = vm.native_data(0).as_int().unwrap() as u32;
            vm.async_module_rejected(m, a.first().copied().unwrap_or(Value::UNDEFINED));
            Ok(Value::UNDEFINED)
        }, false, Box::new([Value::int(m as i32)]));
        promise::perform_then(self, p.as_object().unwrap(), Value::object(on_ful), Value::object(on_rej), [Value::UNDEFINED; 3]);
        Ok(())
    }

    fn gather_ancestors(&mut self, m: u32, list: &mut Vec<u32>) {
        for p in self.m(m).async_parents.clone() {
            if list.contains(&p) {
                continue;
            }
            let root = self.m(p).cycle_root;
            if self.m(root).error.is_some() {
                continue;
            }
            let r = self.mm(p);
            r.pending_async = r.pending_async.saturating_sub(1);
            if r.pending_async == 0 {
                list.push(p);
                if !self.m(p).info.has_tla {
                    self.gather_ancestors(p, list);
                }
            }
        }
    }

    fn async_module_fulfilled(&mut self, m: u32) {
        if self.m(m).status == Status::Evaluated {
            return;
        }
        {
            let r = self.mm(m);
            r.async_eval = false;
            r.status = Status::Evaluated;
        }
        if let Some(p) = self.m(m).top_promise {
            let _ = promise::resolve_promise(self, p, Value::UNDEFINED);
        }
        let mut list = Vec::new();
        self.gather_ancestors(m, &mut list);
        list.sort_by_key(|&x| self.m(x).async_order);
        for p in list {
            if self.m(p).error.is_some() {
                continue;
            }
            if self.m(p).info.has_tla {
                if let Err(Throw::Value(e)) = self.execute_async(p) {
                    self.async_module_rejected(p, e);
                }
            } else {
                match self.execute_sync(p) {
                    Err(Throw::Value(e)) => self.async_module_rejected(p, e),
                    Err(_) => {}
                    Ok(()) => {
                        let r = self.mm(p);
                        r.status = Status::Evaluated;
                        r.async_eval = false;
                        if let Some(pr) = self.m(p).top_promise {
                            let _ = promise::resolve_promise(self, pr, Value::UNDEFINED);
                        }
                    }
                }
            }
        }
    }

    fn async_module_rejected(&mut self, m: u32, e: Value) {
        if self.m(m).status == Status::Evaluated {
            return;
        }
        {
            let r = self.mm(m);
            r.error = Some(e);
            r.status = Status::Evaluated;
            r.async_eval = false;
        }
        for p in self.m(m).async_parents.clone() {
            self.async_module_rejected(p, e);
        }
        if let Some(p) = self.m(m).top_promise {
            promise::reject_promise(self, p, e);
        }
    }

    /// The module whose code runs in frame `fi` (by its environment chain).
    pub fn module_of_frame(&self, fi: usize) -> Option<u32> {
        let mut e = self.frames[fi].env?;
        while let Some(p) = self.envs.get(e.0).parent {
            e = p;
        }
        self.modules.list.iter().position(|r| r.env == Some(e)).map(|i| i as u32)
    }

    /// Resolves a binding of a namespace (ReferenceError while in TDZ).
    fn ns_binding_value(&mut self, r: Res) -> JsResult<Value> {
        match r {
            Res::Binding(m, slot) => {
                let Some(env) = self.m(m).env else {
                    return Err(self.reference_error("Cannot access a module binding before its module is linked"));
                };
                let v = self.envs.get(env.0).slots[slot as usize];
                if v.is_uninit() {
                    return Err(self.reference_error("Cannot access a module binding before initialization"));
                }
                Ok(v)
            }
            Res::Namespace(m) => Ok(Value::object(self.namespace_of(m)?)),
            Res::Ambiguous => Err(self.syntax_error("ambiguous export")),
        }
    }
}

/// Runs async module code: like an async function body; returns its promise.
impl Vm {
    pub fn run_async_code(&mut self, code: Rc<crate::bytecode::Code>, env: Option<EnvId>) -> JsResult<Value> {
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
        self.frames.push(crate::vm::Frame {
            code,
            pc: 0,
            base: base as u32,
            env,
            env_depth: 0,
            this: Value::UNDEFINED,
            callee: Value::UNDEFINED,
            new_target: Value::UNDEFINED,
            args: None,
            argc: 0,
            kind: crate::vm::FrameKind::Entry,
            generator: None,
        });
        let g = promise::start_async(self)?;
        self.frames.last_mut().unwrap().generator = Some(g);
        self.native_depth += 1;
        let r = self.run(depth);
        self.native_depth -= 1;
        r
    }
}

fn ns_index(vm: &Vm, o: ObjId) -> u32 {
    match vm.obj(o).class {
        Class::Namespace(m) => m,
        _ => unreachable!(),
    }
}

fn ns_lookup(vm: &mut Vm, o: ObjId, k: Key) -> Option<Res> {
    let m = ns_index(vm, o);
    let name = match k {
        Key::Str(s) => vm.str(s).to_rust_lossy(),
        Key::Index(i) => format!("{i}"),
        Key::Sym(_) => return None,
    };
    vm.m(m).ns_names.iter().find(|(n, _)| *n == name).map(|x| x.1)
}

pub fn ns_get_own(vm: &mut Vm, o: ObjId, k: Key) -> JsResult<Option<PropDesc>> {
    if let Key::Sym(_) = k {
        return Ok(vm.ordinary_get_own(o, k));
    }
    match ns_lookup(vm, o, k) {
        Some(r) => {
            let v = vm.ns_binding_value(r)?;
            Ok(Some(PropDesc::data(v, attr::WRITABLE | attr::ENUMERABLE)))
        }
        None => Ok(None),
    }
}

pub fn ns_define_own(vm: &mut Vm, o: ObjId, k: Key, d: PropDesc) -> JsResult<bool> {
    if let Key::Sym(_) = k {
        return vm.ordinary_define(o, k, d);
    }
    let Some(cur) = ns_get_own(vm, o, k)? else { return Ok(false) };
    if d.configurable == Some(true) || d.enumerable == Some(false) || d.is_accessor() || d.writable == Some(false) {
        return Ok(false);
    }
    if let Some(v) = d.value {
        return Ok(vm.same_value(v, cur.value.unwrap()));
    }
    Ok(true)
}

pub fn ns_has(vm: &mut Vm, o: ObjId, k: Key) -> JsResult<bool> {
    if let Key::Sym(_) = k {
        return Ok(vm.ordinary_get_own(o, k).is_some());
    }
    Ok(ns_lookup(vm, o, k).is_some())
}

pub fn ns_get(vm: &mut Vm, o: ObjId, k: Key) -> JsResult<Value> {
    if let Key::Sym(_) = k {
        return Ok(vm.ordinary_get_own(o, k).and_then(|d| d.value).unwrap_or(Value::UNDEFINED));
    }
    match ns_lookup(vm, o, k) {
        Some(r) => vm.ns_binding_value(r),
        None => Ok(Value::UNDEFINED),
    }
}

pub fn ns_delete(vm: &mut Vm, o: ObjId, k: Key) -> JsResult<bool> {
    if let Key::Sym(_) = k {
        return match vm.ordinary_get_own(o, k) {
            Some(d) if d.configurable == Some(false) => Ok(false),
            Some(_) => {
                vm.raw_delete(o, k);
                Ok(true)
            }
            None => Ok(true),
        };
    }
    Ok(ns_lookup(vm, o, k).is_none())
}

pub fn ns_own_keys(vm: &mut Vm, o: ObjId) -> JsResult<Vec<Key>> {
    let m = ns_index(vm, o);
    let names: Vec<String> = vm.m(m).ns_names.iter().map(|x| x.0.clone()).collect();
    let mut keys: Vec<Key> = names.iter().map(|n| Key::Str(vm.intern(n))).collect();
    keys.extend(vm.raw_own_keys(o).into_iter().filter(|k| matches!(k, Key::Sym(_))));
    Ok(keys)
}

/// `import(specifier, options)` from frame `fi`.
pub fn import_call(vm: &mut Vm, fi: usize, spec: Value, opts: Value) -> Value {
    let p = vm.new_promise_value();
    vm.root(p);
    let reject = |vm: &mut Vm, t: Throw| {
        if let Throw::Value(e) = t {
            promise::reject_promise(vm, p, e);
        }
    };
    let s = match vm.to_string(spec) {
        Ok(s) => vm.str(s).to_rust_lossy(),
        Err(t) => {
            reject(vm, t);
            return p;
        }
    };
    let mut attrs: Vec<(String, String)> = Vec::new();
    if !opts.is_undefined() {
        let r = (|| -> JsResult<()> {
            let Some(oo) = opts.as_object() else {
                return Err(vm.type_error("The second argument of import() must be an object"));
            };
            let kw = vm.key("with");
            let w = vm.get(oo, kw)?;
            if w.is_undefined() {
                return Ok(());
            }
            let Some(wo) = w.as_object() else {
                return Err(vm.type_error("The 'with' option of import() must be an object"));
            };
            for k in vm.own_keys(wo)? {
                let Key::Str(ks) = k else { continue };
                if !vm.get_own_property(wo, k)?.is_some_and(|d| d.enumerable == Some(true)) {
                    continue;
                }
                let v = vm.get(wo, k)?;
                let Some(vs) = v.as_string() else {
                    return Err(vm.type_error("Import attribute values must be strings"));
                };
                attrs.push((vm.str(ks).to_rust_lossy(), vm.str(vs).to_rust_lossy()));
            }
            Ok(())
        })();
        if let Err(t) = r {
            reject(vm, t);
            return p;
        }
    }
    let referrer: Option<String> = match vm.module_of_frame(fi) {
        Some(m) => Some(vm.m(m).url.clone()),
        None => Some(vm.frames[fi].code.source.name.clone()),
    };
    let g = vm.new_graph(p);
    vm.host_load(g, None, referrer.as_deref(), &s, &attrs);
    p
}

/// `import.meta` of the module running in frame `fi`.
pub fn import_meta(vm: &mut Vm, fi: usize) -> JsResult<Value> {
    let Some(m) = vm.module_of_frame(fi) else {
        return Err(vm.syntax_error("import.meta outside a module"));
    };
    if let Some(o) = vm.m(m).meta {
        return Ok(Value::object(o));
    }
    let o = vm.alloc_object(None, Class::Ordinary);
    let url = vm.m(m).url.clone();
    let u = vm.new_string(&url);
    let k = vm.key("url");
    vm.define_data(o, k, u, attr::DEFAULT);
    vm.mm(m).meta = Some(o);
    Ok(Value::object(o))
}
