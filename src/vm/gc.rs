//! Mark-sweep garbage collection (precise; runs only at safepoints).

use alloc::vec::Vec;

use super::*;
use crate::object::{Class, Key, Props};

/// Something holding heap references.
pub trait Trace {
    fn trace(&self, out: &mut Vec<Value>);
}

struct Marker {
    values: Vec<Value>,
    envs: Vec<EnvId>,
    weakmaps: Vec<ObjId>,
}

impl Vm {
    pub fn collect_garbage(&mut self) {
        let mut m = Marker { values: Vec::with_capacity(4096), envs: Vec::new(), weakmaps: Vec::new() };
        self.roots(&mut m);
        self.drain(&mut m);
        // Ephemerons: WeakMap values live while their keys live.
        loop {
            let mut grew = false;
            for &wm in &m.weakmaps.clone() {
                let entries: Vec<(u32, Value)> = match &self.obj(wm).class {
                    Class::WeakMap(t) => t.iter().map(|(k, v)| (*k, *v)).collect(),
                    _ => continue,
                };
                for (k, v) in entries {
                    if self.objects.is_marked(k) && v.is_heap() && !self.is_value_marked(v) {
                        m.values.push(v);
                        grew = true;
                    }
                }
            }
            if !grew {
                break;
            }
            self.drain(&mut m);
        }
        self.process_weak();
        // Sweep.
        let freed_objects = self.objects.sweep(|_, _| {});
        let atoms_and_live = &self.strings;
        let _ = atoms_and_live;
        let freed_strings = self.strings.sweep(|_, _| {});
        self.symbols.sweep(|_, _| {});
        self.bigints.sweep(|_, _| {});
        self.envs.sweep(|_, _| {});
        let _ = (freed_objects, freed_strings);
        let live = self.heap_estimate();
        self.alloc_bytes = 0;
        self.gc_threshold = (live * 2).max(8 << 20).min(self.limits.max_heap_bytes / 2 + 1);
    }

    /// Approximate live heap size in bytes.
    pub fn heap_estimate(&self) -> usize {
        // An unflattened rope holds only its two children's ids.
        let strings: usize = self
            .strings
            .iter()
            .map(|(_, s)| match s.unflattened() {
                Some(_) => 48,
                None => 32 + s.len() * if s.is_latin1() { 1 } else { 2 },
            })
            .sum();
        let objects: usize = self.objects.iter().map(|(_, o)| 96 + o.elements.capacity() * 8).sum();
        objects + strings + self.envs.len() * 64 + self.stack.len() * 8
    }

    fn is_value_marked(&self, v: Value) -> bool {
        match v.kind() {
            Kind::Object(o) => self.objects.is_marked(o.0),
            Kind::Str(s) => self.strings.is_marked(s.0),
            Kind::Symbol(s) => self.symbols.is_marked(s.0),
            Kind::BigInt(b) => self.bigints.is_marked(b.0),
            _ => true,
        }
    }

    fn roots(&mut self, m: &mut Marker) {
        let sp = self.sp.min(self.stack.len());
        m.values.extend_from_slice(&self.stack[..sp]);
        for f in &self.frames {
            m.values.push(f.this);
            m.values.push(f.callee);
            m.values.push(f.new_target);
            if let Some(e) = f.env {
                m.envs.push(e);
            }
            if let Some(a) = &f.args {
                m.values.extend_from_slice(a);
            }
            if let Some(g) = f.generator {
                m.values.push(Value::object(g));
            }
            // Registers of every frame (the stack covers them up to sp).
            let base = f.base as usize;
            let end = (base + f.code.nregs as usize).min(self.stack.len());
            m.values.extend_from_slice(&self.stack[base..end]);
        }
        m.values.extend_from_slice(&self.native_roots);
        for o in self.intrinsics.all() {
            m.values.push(Value::object(o));
        }
        m.values.push(Value::object(self.global));
        m.values.push(Value::object(self.global_this));
        for k in self.global_lex.keys() {
            m.values.push(Value::str(*k));
        }
        for (v, _) in &self.global_cells {
            m.values.push(*v);
        }
        for (k, s) in &self.symbol_registry {
            m.values.push(Value::str(*k));
            m.values.push(Value::symbol(*s));
        }
        for j in &self.jobs {
            m.values.push(j.func);
            m.values.push(j.this);
            m.values.extend_from_slice(&j.args);
        }
        m.values.extend(self.templates.values().copied());
        m.values.extend(self.persistent.values().copied());
        if let Some(v) = self.pending_acc {
            m.values.push(v);
        }
        m.values.push(self.current_native);
        m.values.extend_from_slice(&self.kept_alive);
        m.values.extend(self.raw_json.iter().map(|o| Value::object(*o)));
        m.values.extend(self.symbol_holders.values().map(|o| Value::object(*o)));
        m.values.extend(self.join_stack.iter().map(|o| Value::object(*o)));
        self.modules.trace(&mut m.values, &mut m.envs);
        crate::debug::trace_roots(self, &mut m.values);
        // Atoms are permanent; so are the symbols used as shape keys.
        let atoms: Vec<StrId> = self.atoms().collect();
        for a in atoms {
            self.strings.mark(a.0);
        }
        let s = &self.intrinsics;
        for sym in [
            s.sym_iterator,
            s.sym_async_iterator,
            s.sym_has_instance,
            s.sym_is_concat_spreadable,
            s.sym_match,
            s.sym_match_all,
            s.sym_replace,
            s.sym_search,
            s.sym_species,
            s.sym_split,
            s.sym_to_primitive,
            s.sym_to_string_tag,
            s.sym_unscopables,
        ] {
            m.values.push(Value::symbol(sym));
        }
        for sh in &self.shapes {
            if let Some(Key::Sym(sym)) = sh.key {
                m.values.push(Value::symbol(sym));
            }
        }
        // Code constants (BigInt literals are materialized per use; atoms
        // are permanent) need nothing.
    }

    fn drain(&mut self, m: &mut Marker) {
        loop {
            if let Some(v) = m.values.pop() {
                self.mark_value(v, m);
                continue;
            }
            if let Some(e) = m.envs.pop() {
                if !self.envs.mark(e.0) {
                    continue;
                }
                let env = self.envs.get(e.0);
                m.values.extend_from_slice(&env.slots);
                if let Some(p) = env.parent {
                    m.envs.push(p);
                }
                if let Some(w) = env.with {
                    m.values.push(Value::object(w));
                }
                if let Some(im) = &env.imports {
                    m.envs.extend(im.iter().map(|x| x.0));
                }
                if let Some(ns) = &env.names {
                    m.values.extend_from_slice(ns);
                }
                if let Some(d) = &env.eval_vars {
                    for (k, v, _) in d.entries.iter().flatten() {
                        if let Some(kv) = k.to_value() {
                            m.values.push(kv);
                        }
                        m.values.push(*v);
                    }
                }
                continue;
            }
            break;
        }
    }

    /// Marks a string; an unflattened rope keeps its children alive.
    fn mark_str(&mut self, s: StrId, m: &mut Marker) {
        if self.strings.mark(s.0) {
            if let Some(r) = self.strings.get(s.0).unflattened() {
                let (l, rt) = (r.left, r.right);
                m.values.push(Value::str(l));
                m.values.push(Value::str(rt));
            }
        }
    }

    fn mark_value(&mut self, v: Value, m: &mut Marker) {
        match v.kind() {
            Kind::Str(s) => self.mark_str(s, m),
            Kind::Symbol(s) => {
                if self.symbols.mark(s.0) {
                    if let Some(d) = self.symbols.get(s.0).description {
                        self.mark_str(d, m);
                    }
                }
            }
            Kind::BigInt(b) => {
                self.bigints.mark(b.0);
            }
            Kind::Object(o) => {
                if self.objects.mark(o.0) {
                    self.trace_object(o, m);
                }
            }
            _ => {}
        }
    }

    fn trace_object(&mut self, id: ObjId, m: &mut Marker) {
        let o = self.objects.get(id.0);
        if let Some(p) = o.proto {
            m.values.push(Value::object(p));
        }
        match &o.props {
            Props::Shaped { slots, .. } => m.values.extend_from_slice(slots),
            Props::Dict(d) => {
                for (k, v, _) in d.entries.iter().flatten() {
                    if let Some(kv) = k.to_value() {
                        m.values.push(kv);
                    }
                    m.values.push(*v);
                }
            }
        }
        m.values.extend_from_slice(&o.elements);
        let out = &mut m.values;
        match &o.class {
            Class::Ordinary | Class::Array | Class::Error | Class::Boolean(_) | Class::Number(_) | Class::Date(_) => {}
            Class::Function(c) => {
                if let Some(e) = c.env {
                    m.envs.push(e);
                }
                if let Some(h) = c.home {
                    out.push(Value::object(h));
                }
                if let Some(f) = c.fields {
                    out.push(f);
                }
                if let Some(pm) = &c.private_methods {
                    for (k, f, _) in pm.iter() {
                        out.push(*k);
                        out.push(*f);
                    }
                }
                c.code.trace(out);
            }
            Class::Native(n) => out.extend_from_slice(&n.data),
            Class::Bound { target, this, args } => {
                out.push(Value::object(*target));
                out.push(*this);
                out.extend_from_slice(args);
            }
            Class::String(s) => out.push(Value::str(*s)),
            Class::Symbol(s) => out.push(Value::symbol(*s)),
            Class::BigInt(b) => out.push(Value::bigint(*b)),
            Class::RegExp(r) => r.trace(out),
            Class::Map(t) | Class::Set(t) => t.trace(out),
            Class::WeakMap(_) => m.weakmaps.push(id),
            Class::WeakSet(_) => {}
            Class::WeakRef(_) => {}
            Class::FinalizationRegistry(r) => r.trace(out),
            Class::Promise(p) => p.trace(out),
            Class::Proxy { target, handler, .. } => {
                if let Some(t) = target {
                    out.push(Value::object(*t));
                }
                if let Some(h) = handler {
                    out.push(Value::object(*h));
                }
            }
            Class::ArrayBuffer(_) => {}
            Class::TypedArray(t) => t.trace(out),
            Class::DataView(t) => t.trace(out),
            Class::Arguments(a) => {
                if let Some(a) = a {
                    m.envs.push(a.env);
                }
            }
            Class::Generator(g) | Class::AsyncGenerator(g) => {
                out.extend_from_slice(&g.regs);
                out.push(g.this);
                out.push(g.callee);
                out.push(g.new_target);
                out.push(g.promise);
                if let Some(a) = &g.args {
                    out.extend_from_slice(a);
                }
                for (_, a, b) in &g.queue {
                    out.push(*a);
                    out.push(*b);
                }
                if let Some(e) = g.env {
                    m.envs.push(e);
                }
                g.code.trace(out);
            }
            Class::AsyncFromSyncIterator(a, b) => {
                out.push(*a);
                out.push(*b);
            }
            Class::Iter(s) => s.trace(out),
            Class::ForIn(s) => {
                out.push(s.object);
                out.extend_from_slice(&s.keys);
            }
            Class::AccessorPair { get, set } => {
                out.push(*get);
                out.push(*set);
            }
            Class::Namespace(_) => {}
            Class::Internal(vs) => out.extend_from_slice(vs),
        }
    }

    /// Clears dead entries of weak collections and refs before sweeping.
    fn process_weak(&mut self) {
        let mut cleanups: Vec<(Value, Value)> = Vec::new();
        let ids: Vec<u32> = self.objects.iter().filter(|(i, _)| self.objects.is_marked(*i)).map(|(i, _)| i).collect();
        for i in ids {
            let marked_target = |vm: &Vm, v: Value| vm.is_value_marked(v);
            let (dead_keys, kind): (Vec<u32>, u8) = match &self.objects.get(i).class {
                Class::WeakMap(t) => (t.keys().copied().filter(|k| !self.objects.is_marked(*k)).collect(), 0),
                Class::WeakSet(t) => (t.keys().copied().filter(|k| !self.objects.is_marked(*k)).collect(), 1),
                Class::WeakRef(v) => {
                    if !marked_target(self, *v) {
                        (alloc::vec![0], 2)
                    } else {
                        (Vec::new(), 2)
                    }
                }
                Class::FinalizationRegistry(r) => {
                    let mut dead = Vec::new();
                    for (n, cell) in r.cells.iter().enumerate() {
                        if !marked_target(self, cell.target) {
                            dead.push(n as u32);
                            cleanups.push((r.cleanup, cell.held));
                        }
                    }
                    (dead, 3)
                }
                _ => continue,
            };
            if dead_keys.is_empty() {
                continue;
            }
            match &mut self.objects.get_mut(i).class {
                Class::WeakMap(t) => {
                    for k in dead_keys {
                        t.remove(&k);
                    }
                }
                Class::WeakSet(t) => {
                    for k in dead_keys {
                        t.remove(&k);
                    }
                }
                Class::WeakRef(v) => *v = Value::UNDEFINED,
                Class::FinalizationRegistry(r) => {
                    let mut n = 0u32;
                    r.cells.retain(|_| {
                        let keep = !dead_keys.contains(&n);
                        n += 1;
                        keep
                    });
                }
                _ => {}
            }
            let _ = kind;
        }
        // Cleanup callbacks run as jobs (their values are live: queued).
        for (f, held) in cleanups {
            self.jobs.push_back(Job { func: f, this: Value::UNDEFINED, args: alloc::vec![held] });
        }
    }
}

impl Trace for crate::bytecode::Code {
    fn trace(&self, out: &mut Vec<Value>) {
        // Constants are atoms (permanent) or numbers; children likewise.
        let _ = out;
    }
}
