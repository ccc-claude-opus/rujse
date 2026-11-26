//! The virtual machine: heap, realm, frames and the interpreter.

pub mod conv;
pub mod gc;
pub mod generator;
pub mod interp;
pub mod props;
pub mod binary;
pub mod ic;
pub mod realm;
pub mod scope;

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec::Vec;

use crate::bytecode::{Code, Source};
use crate::heap::Arena;
use crate::object::{Class, Dict, Key, Object, Props, Shape, ShapeId, Symbol};
use crate::string::JsStr;
use crate::value::{BigId, Kind, ObjId, StrId, SymId, Value};

pub use realm::Intrinsics;

/// Why JS execution stopped abnormally.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Throw {
    /// A JS exception (catchable).
    Value(Value),
    /// The instruction budget ran out or the host interrupted: not
    /// catchable by JS; the VM stays usable.
    Terminate,
}

pub type JsResult<T> = Result<T, Throw>;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Hash)]
pub struct EnvId(pub u32);

/// A declarative environment (closure variables), a `with` object
/// environment, or a function's eval-extended variable environment.
pub struct Env {
    pub slots: Vec<Value>,
    pub parent: Option<EnvId>,
    /// Slot names (for `with`/eval lookups), an atom array.
    pub names: Option<Rc<[Value]>>,
    /// `with (obj)`.
    pub with: Option<ObjId>,
    /// Vars added by sloppy direct eval to a function scope.
    pub eval_vars: Option<Box<Dict>>,
    /// Module environments: the resolved import bindings (env, slot).
    pub imports: Option<Rc<[(EnvId, u16)]>>,
}

/// Arguments object mapped to a sloppy function's parameters.
pub struct MappedArgs {
    pub env: EnvId,
    /// Parameter index -> env slot (u16::MAX = unmapped).
    pub map: Vec<u16>,
}

/// Saved state of a suspended generator or async function.
pub struct GenState {
    pub state: GenStatus,
    pub code: Rc<Code>,
    pub pc: u32,
    pub regs: Vec<Value>,
    pub env: Option<EnvId>,
    pub env_depth: u16,
    pub this: Value,
    pub callee: Value,
    pub new_target: Value,
    pub args: Option<Box<[Value]>>,
    /// Async functions: the promise; async generators: the request queue.
    pub promise: Value,
    pub queue: Vec<(u8, Value, Value)>,
    /// Async generator: an await in progress inside the body.
    pub awaiting_return: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum GenStatus {
    SuspendedStart,
    SuspendedYield,
    Executing,
    AwaitingReturn,
    Completed,
}

/// For-in enumeration state: the keys collected up front, checked again
/// before each is returned (deleted keys are skipped).
pub struct ForInState {
    pub object: Value,
    pub keys: Vec<Value>,
    pub pos: usize,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FrameKind {
    /// Called from the interpreter: the result goes to the caller's acc.
    Normal,
    /// Called from Rust (`Vm::call`): returning ends `run`.
    Entry,
    /// `new`: a non-object result is replaced by `this`.
    Construct,
    /// Entry frame of `new` from Rust.
    ConstructEntry,
    /// A resumed generator/async function (entry from Rust).
    Resume,
}

pub struct Frame {
    pub code: Rc<Code>,
    pub pc: u32,
    pub base: u32,
    pub env: Option<EnvId>,
    pub env_depth: u16,
    pub this: Value,
    pub callee: Value,
    pub new_target: Value,
    pub args: Option<Box<[Value]>>,
    pub argc: u16,
    pub kind: FrameKind,
    /// The generator object this frame runs (generators, async).
    pub generator: Option<ObjId>,
}

/// Limits and the interrupt budget.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// Maximum JS call depth (frames).
    pub max_frames: usize,
    /// Maximum nested native→JS re-entries (Rust stack use).
    pub max_native_depth: u32,
    /// Heap size (bytes, approximate) that triggers a RangeError.
    pub max_heap_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Limits { max_frames: 10_000, max_native_depth: 400, max_heap_bytes: 512 << 20 }
    }
}

/// A job queued for the microtask checkpoint (promise reactions).
pub struct Job {
    pub func: Value,
    pub this: Value,
    pub args: Vec<Value>,
}

pub struct Vm {
    pub objects: Arena<Object>,
    pub strings: Arena<JsStr>,
    pub symbols: Arena<Symbol>,
    pub bigints: Arena<crate::builtins::bigint::BigInt>,
    pub envs: Arena<Env>,
    pub shapes: Vec<Shape>,
    pub root_shape: ShapeId,
    /// Interned strings (property keys): hash -> ids.
    atoms: BTreeMap<u32, Vec<StrId>>,
    /// Atoms for common names.
    pub names: realm::Names,
    pub intrinsics: Intrinsics,
    pub global: ObjId,
    /// Script-level `let`/`const`/`class` bindings: name -> index into
    /// `global_cells` (value, const). Cells are never removed, so inline
    /// caches may keep their index.
    pub global_lex: BTreeMap<StrId, u32>,
    pub global_cells: Vec<(Value, bool)>,
    /// Bumped whenever a global lexical binding is added (it may shadow a
    /// global object property that an inline cache points to).
    pub global_epoch: u32,
    /// The debugger, when enabled (`enable_debugger`).
    pub debugger: Option<alloc::boxed::Box<crate::debug::Debugger>>,
    pub inspector: crate::inspect::Inspector,
    pub symbol_registry: BTreeMap<StrId, SymId>,
    pub stack: Vec<Value>,
    pub sp: usize,
    pub frames: Vec<Frame>,
    pub native_roots: Vec<Value>,
    pub native_depth: u32,
    pub jobs: alloc::collections::VecDeque<Job>,
    pub limits: Limits,
    /// Instructions left before `Terminate` (None = unlimited).
    pub budget: Option<u64>,
    /// Set by the host to stop execution at the next safepoint.
    pub interrupt: Rc<core::cell::Cell<bool>>,
    /// Allocation counter that triggers a GC at the next safepoint.
    pub alloc_bytes: usize,
    pub gc_threshold: usize,
    pub host: Option<Box<dyn crate::Host>>,
    /// Template objects per (code address, site).
    pub templates: BTreeMap<(usize, u32), Value>,
    pub sources: Vec<Rc<Source>>,
    /// Persistent handles held by the embedder.
    pub persistent: BTreeMap<u32, Value>,
    pub next_persistent: u32,
    pub modules: crate::module::Modules,
    /// The exception being propagated (for stack traces).
    pub last_error_stack: Option<String>,
    /// Random state for Math.random.
    pub rng: u64,
    /// Scratch vectors for native call arguments.
    pub arg_pool: Vec<Vec<Value>>,
    /// Weak collections and refs to sweep after marking.
    pub weak_objects: Vec<ObjId>,
    /// Finalization registries with pending cleanups.
    pub kept_alive: Vec<Value>,
    /// The native function running now (for its captured data).
    pub current_native: Value,
    /// Safepoint countdown.
    pub ticks: i32,
    /// Accumulator to resume a frame with (generators, async unwinding).
    pub pending_acc: Option<Value>,
    pub gen_signal: interp::GenSignal,
    /// `this` of global code (the global object).
    pub global_this: ObjId,
    /// Objects being joined (Array.prototype.join cycle detection).
    pub join_stack: Vec<ObjId>,
    /// Objects made by JSON.rawJSON.
    pub raw_json: Vec<ObjId>,
    /// Symbols used as weak collection keys -> their holder object.
    pub symbol_holders: BTreeMap<u32, ObjId>,
    /// Recently compiled regular expressions ((pattern, flags), program).
    pub regex_cache: Vec<((Vec<u16>, String), Rc<crate::regex::Regex>)>,
}

impl Vm {
    pub fn new() -> Box<Vm> {
        let mut vm = Box::new(Vm {
            objects: Arena::default(),
            strings: Arena::default(),
            symbols: Arena::default(),
            bigints: Arena::default(),
            envs: Arena::default(),
            shapes: Vec::new(),
            root_shape: ShapeId(0),
            atoms: BTreeMap::new(),
            names: realm::Names::default(),
            intrinsics: Intrinsics::default(),
            global: ObjId(0),
            global_lex: BTreeMap::new(),
            global_cells: Vec::new(),
            global_epoch: 0,
            debugger: None,
            inspector: Default::default(),
            symbol_registry: BTreeMap::new(),
            stack: Vec::new(),
            sp: 0,
            frames: Vec::new(),
            native_roots: Vec::new(),
            native_depth: 0,
            jobs: alloc::collections::VecDeque::new(),
            limits: Limits::default(),
            budget: None,
            interrupt: Rc::new(core::cell::Cell::new(false)),
            alloc_bytes: 0,
            gc_threshold: 8 << 20,
            host: None,
            templates: BTreeMap::new(),
            sources: Vec::new(),
            persistent: BTreeMap::new(),
            next_persistent: 1,
            modules: Default::default(),
            last_error_stack: None,
            rng: 0x2545_F491_4F6C_DD1D,
            arg_pool: Vec::new(),
            weak_objects: Vec::new(),
            kept_alive: Vec::new(),
            current_native: Value::UNDEFINED,
            ticks: 1024,
            pending_acc: None,
            gen_signal: interp::GenSignal::None,
            global_this: ObjId(0),
            join_stack: Vec::new(),
            raw_json: Vec::new(),
            symbol_holders: BTreeMap::new(),
            regex_cache: Vec::new(),
        });
        vm.shapes.push(Shape { parent: None, key: None, attrs: 0, len: 0, transitions: Vec::new(), table: None });
        realm::init(&mut vm);
        vm
    }

    // ----- allocation -----------------------------------------------------

    pub fn alloc_object(&mut self, proto: Option<ObjId>, class: Class) -> ObjId {
        self.alloc_bytes += 96;
        ObjId(self.objects.alloc(Object::new(proto, self.root_shape, class)))
    }

    /// An ordinary object with `Object.prototype`.
    pub fn new_object(&mut self) -> ObjId {
        let p = self.intrinsics.object_prototype;
        self.alloc_object(Some(p), Class::Ordinary)
    }

    pub fn new_array(&mut self, elements: Vec<Value>) -> ObjId {
        let p = self.intrinsics.array_prototype;
        self.alloc_bytes += elements.len() * 8;
        let id = self.alloc_object(Some(p), Class::Array);
        let o = self.objects.get_mut(id.0);
        o.length = elements.len() as u32;
        o.elements = elements;
        id
    }

    pub fn alloc_string(&mut self, s: JsStr) -> StrId {
        self.alloc_bytes += match s.repr {
            crate::string::Repr::Rope(_) => 48,
            _ => 32 + s.len() * if s.is_latin1() { 1 } else { 2 },
        };
        StrId(self.strings.alloc(s))
    }

    pub fn new_string(&mut self, s: &str) -> Value {
        if s.is_empty() {
            return Value::str(self.names.empty);
        }
        Value::str(self.alloc_string(JsStr::from_str(s)))
    }

    pub fn new_string_units(&mut self, u: &[u16]) -> Value {
        if u.is_empty() {
            return Value::str(self.names.empty);
        }
        Value::str(self.alloc_string(JsStr::from_units(u)))
    }

    pub fn new_env(&mut self, n: usize, init: Value, parent: Option<EnvId>) -> EnvId {
        self.alloc_bytes += 48 + n * 8;
        EnvId(self.envs.alloc(Env { slots: alloc::vec![init; n], parent, names: None, with: None, eval_vars: None, imports: None }))
    }

    pub fn new_symbol(&mut self, description: Option<StrId>) -> SymId {
        self.alloc_bytes += 24;
        SymId(self.symbols.alloc(Symbol { description, registered: false, private: false }))
    }

    pub fn str(&self, id: StrId) -> &JsStr {
        let s = self.strings.get(id.0);
        if let Some(r) = s.unflattened() {
            let _ = r.flat.set(self.flatten(r));
        }
        s
    }

    /// Copies a rope's units (iteratively: ropes built by `s += x` in a
    /// loop are as deep as the loop is long).
    fn flatten(&self, r: &crate::string::Rope) -> crate::string::StrData {
        use crate::string::StrData;
        let mut stack = alloc::vec![r.right, r.left];
        if r.latin1 {
            let mut out: Vec<u8> = Vec::with_capacity(r.len as usize);
            while let Some(id) = stack.pop() {
                let s = self.strings.get(id.0);
                match s.unflattened() {
                    Some(c) => stack.extend([c.right, c.left]),
                    None => s.push_latin1(&mut out),
                }
            }
            StrData::Latin1(out.into_boxed_slice())
        } else {
            let mut out: Vec<u16> = Vec::with_capacity(r.len as usize);
            while let Some(id) = stack.pop() {
                let s = self.strings.get(id.0);
                match s.unflattened() {
                    Some(c) => stack.extend([c.right, c.left]),
                    None => s.push_units(&mut out),
                }
            }
            StrData::Utf16(out.into_boxed_slice())
        }
    }

    pub fn obj(&self, id: ObjId) -> &Object {
        self.objects.get(id.0)
    }

    pub fn obj_mut(&mut self, id: ObjId) -> &mut Object {
        self.objects.get_mut(id.0)
    }

    // ----- atoms ----------------------------------------------------------

    /// Interns a string (atoms are never collected).
    pub fn intern_id(&mut self, id: StrId) -> StrId {
        if self.str(id).atom {
            return id;
        }
        let h = self.strings.get_mut(id.0).hash();
        if let Some(list) = self.atoms.get(&h) {
            for &a in list {
                if self.strings.get(a.0).eq_units(self.strings.get(id.0)) {
                    return a;
                }
            }
        }
        self.strings.get_mut(id.0).atom = true;
        self.atoms.entry(h).or_default().push(id);
        id
    }

    pub fn intern(&mut self, s: &str) -> StrId {
        let h = crate::string::hash_units(s.encode_utf16());
        if let Some(list) = self.atoms.get(&h) {
            for &a in list {
                if self.strings.get(a.0).eq_str(s) {
                    return a;
                }
            }
        }
        let mut js = JsStr::from_str(s);
        js.hash = h;
        js.atom = true;
        let id = StrId(self.strings.alloc(js));
        self.atoms.entry(h).or_default().push(id);
        id
    }

    pub fn intern_units(&mut self, u: &[u16]) -> StrId {
        let h = crate::string::hash_units(u.iter().copied());
        if let Some(list) = self.atoms.get(&h) {
            for &a in list {
                if self.strings.get(a.0).units().eq(u.iter().copied()) {
                    return a;
                }
            }
        }
        let mut js = JsStr::from_units(u);
        js.hash = h;
        js.atom = true;
        let id = StrId(self.strings.alloc(js));
        self.atoms.entry(h).or_default().push(id);
        id
    }

    pub fn atoms(&self) -> impl Iterator<Item = StrId> + '_ {
        self.atoms.values().flatten().copied()
    }

    /// The property key for a string (array index or atom).
    pub fn key_from_str(&mut self, id: StrId) -> Key {
        if let Some(i) = self.str(id).array_index() {
            return Key::Index(i);
        }
        Key::Str(self.intern_id(id))
    }

    pub fn key(&mut self, s: &str) -> Key {
        let id = self.intern(s);
        self.key_from_str(id)
    }

    /// A key back to a JS value (index keys become strings).
    pub fn key_to_value(&mut self, k: Key) -> Value {
        match k {
            Key::Index(i) => {
                let s = alloc::format!("{i}");
                self.new_string(&s)
            }
            Key::Str(s) => Value::str(s),
            Key::Sym(s) => Value::symbol(s),
        }
    }

    // ----- shapes ---------------------------------------------------------

    pub fn shape(&self, id: ShapeId) -> &Shape {
        &self.shapes[id.0 as usize]
    }

    /// Finds `key` in a shape: (slot, attrs).
    pub fn shape_lookup(&self, id: ShapeId, key: Key) -> Option<(u32, u8)> {
        let s = &self.shapes[id.0 as usize];
        if let Some(t) = &s.table {
            return t.get(&key).copied();
        }
        let mut cur = Some(id);
        while let Some(c) = cur {
            let s = &self.shapes[c.0 as usize];
            if s.key == Some(key) {
                return Some((s.len - 1, s.attrs));
            }
            cur = s.parent;
        }
        None
    }

    /// The shape with `key` added (cached transition).
    pub fn shape_add(&mut self, id: ShapeId, key: Key, attrs: u8) -> ShapeId {
        if let Some(&(_, _, t)) = self.shapes[id.0 as usize].transitions.iter().find(|(k, a, _)| *k == key && *a == attrs) {
            return t;
        }
        let len = self.shapes[id.0 as usize].len + 1;
        let table = if len > crate::object::TABLE_MIN {
            let mut t = match &self.shapes[id.0 as usize].table {
                Some(t) => (**t).clone(),
                None => {
                    let mut t = BTreeMap::new();
                    for (k, slot, a) in self.shape_keys(id) {
                        t.insert(k, (slot, a));
                    }
                    t
                }
            };
            t.insert(key, (len - 1, attrs));
            Some(Rc::new(t))
        } else {
            None
        };
        let new = ShapeId(self.shapes.len() as u32);
        self.shapes.push(Shape { parent: Some(id), key: Some(key), attrs, len, transitions: Vec::new(), table });
        self.shapes[id.0 as usize].transitions.push((key, attrs, new));
        new
    }

    /// Keys of a shape in insertion order: (key, slot, attrs).
    pub fn shape_keys(&self, id: ShapeId) -> Vec<(Key, u32, u8)> {
        let mut out = Vec::with_capacity(self.shapes[id.0 as usize].len as usize);
        let mut cur = Some(id);
        while let Some(c) = cur {
            let s = &self.shapes[c.0 as usize];
            if let Some(k) = s.key {
                out.push((k, s.len - 1, s.attrs));
            }
            cur = s.parent;
        }
        out.reverse();
        out
    }

    /// Converts an object's named properties to a dictionary.
    pub fn to_dict(&mut self, id: ObjId) {
        let Props::Shaped { shape, slots } = &self.objects.get(id.0).props else { return };
        let keys = self.shape_keys(*shape);
        let mut d = Dict::default();
        for (k, slot, a) in keys {
            d.set(k, slots[slot as usize], a);
        }
        self.objects.get_mut(id.0).props = Props::Dict(Box::new(d));
    }

    // ----- errors ---------------------------------------------------------

    pub fn make_error(&mut self, proto: ObjId, msg: &str) -> Value {
        let o = self.alloc_object(Some(proto), Class::Error);
        let m = self.new_string(msg);
        let k = Key::Str(self.names.message);
        self.define_data(o, k, m, crate::object::attr::HIDDEN);
        self.attach_stack(o);
        Value::object(o)
    }

    pub fn type_error(&mut self, msg: &str) -> Throw {
        let p = self.intrinsics.type_error_prototype;
        Throw::Value(self.make_error(p, msg))
    }

    pub fn range_error(&mut self, msg: &str) -> Throw {
        let p = self.intrinsics.range_error_prototype;
        Throw::Value(self.make_error(p, msg))
    }

    pub fn reference_error(&mut self, msg: &str) -> Throw {
        let p = self.intrinsics.reference_error_prototype;
        Throw::Value(self.make_error(p, msg))
    }

    pub fn syntax_error(&mut self, msg: &str) -> Throw {
        let p = self.intrinsics.syntax_error_prototype;
        Throw::Value(self.make_error(p, msg))
    }

    pub fn throw_type<T>(&mut self, msg: &str) -> JsResult<T> {
        Err(self.type_error(msg))
    }

    pub fn throw_range<T>(&mut self, msg: &str) -> JsResult<T> {
        Err(self.range_error(msg))
    }

    /// Adds a `stack` property with the current JS call stack.
    pub fn attach_stack(&mut self, o: ObjId) {
        let s = self.stack_trace();
        let v = self.new_string(&s);
        let k = Key::Str(self.names.stack);
        self.define_data(o, k, v, crate::object::attr::HIDDEN);
    }

    /// "    at f (file:line:col)" lines for the current frames.
    pub fn stack_trace(&self) -> String {
        let mut out = String::new();
        for f in self.frames.iter().rev().take(20) {
            let (line, col) = f.code.position(f.pc.saturating_sub(1));
            let name = match f.code.name.as_string() {
                Some(s) if !self.str(s).is_empty() => self.str(s).to_rust_lossy(),
                _ => String::from("<anonymous>"),
            };
            out.push_str(&alloc::format!("    at {} ({}:{}:{})\n", name, f.code.source.name, line, col));
        }
        if out.ends_with('\n') {
            out.pop();
        }
        out
    }

    // ----- handles for the embedder --------------------------------------

    pub fn persist(&mut self, v: Value) -> u32 {
        let id = self.next_persistent;
        self.next_persistent += 1;
        self.persistent.insert(id, v);
        id
    }

    pub fn release(&mut self, id: u32) {
        self.persistent.remove(&id);
    }

    /// Keeps a value alive while a native function runs (released when it
    /// returns). Needed only across calls back into JS.
    pub fn root(&mut self, v: Value) {
        self.native_roots.push(v);
    }

    // ----- misc -----------------------------------------------------------

    pub fn is_callable(&self, v: Value) -> bool {
        match v.as_object() {
            Some(o) => self.obj(o).is_callable(),
            None => false,
        }
    }

    pub fn is_constructor(&self, v: Value) -> bool {
        let Some(o) = v.as_object() else { return false };
        match &self.obj(o).class {
            Class::Function(c) => {
                use crate::bytecode::FuncKind::*;
                matches!(c.code.kind, Normal | ClassConstructor | DerivedConstructor) && !c.code.is_async && !c.code.is_generator
            }
            Class::Native(n) => n.ctor,
            Class::Bound { target, .. } => self.is_constructor(Value::object(*target)),
            Class::Proxy { ctor, .. } => *ctor,
            _ => false,
        }
    }

    pub fn random(&mut self) -> f64 {
        // xorshift64*
        let mut x = self.rng;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.rng = x;
        (x.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 11) as f64 / (1u64 << 53) as f64
    }

    pub fn kind(&self, v: Value) -> Kind {
        v.kind()
    }

    /// The longest string this VM makes (code units): the engine maximum,
    /// or less when the heap limit could not hold it (as UTF-16).
    pub fn max_string_len(&self) -> usize {
        crate::string::MAX_LEN.min(self.limits.max_heap_bytes / 2)
    }

    /// Makes the VM usable again after the host abandoned a call into it
    /// midway (a trap or fuel exhaustion: the call never returned). The
    /// abandoned frames are dropped; the heap stays consistent. Call it
    /// before the next call when the previous one did not return.
    pub fn recover(&mut self) {
        self.frames.clear();
        self.sp = 0;
        self.native_depth = 0;
        self.native_roots.clear();
        self.pending_acc = None;
        self.gen_signal = interp::GenSignal::None;
        self.current_native = Value::UNDEFINED;
        self.join_stack.clear();
        self.ticks = 1024;
    }

    pub fn take_args_vec(&mut self) -> Vec<Value> {
        self.arg_pool.pop().unwrap_or_default()
    }

    pub fn give_args_vec(&mut self, mut v: Vec<Value>) {
        v.clear();
        if self.arg_pool.len() < 64 {
            self.arg_pool.push(v);
        }
    }

    pub fn bigint(&self, id: BigId) -> &crate::builtins::bigint::BigInt {
        self.bigints.get(id.0)
    }
}
