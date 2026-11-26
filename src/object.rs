//! Objects: a prototype, named properties (a shape plus slots, or a
//! dictionary), dense indexed elements and an internal class.

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::rc::Rc;
use alloc::vec::Vec;

use crate::bytecode::Code;
use crate::value::{BigId, ObjId, StrId, SymId, Value};

/// A property key: an array index, an atom string or a symbol.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum Key {
    Index(u32),
    Str(StrId),
    Sym(SymId),
}

impl Key {
    pub fn to_value(self) -> Option<Value> {
        match self {
            Key::Str(s) => Some(Value::str(s)),
            Key::Sym(s) => Some(Value::symbol(s)),
            Key::Index(_) => None,
        }
    }
}

/// Property attributes.
pub mod attr {
    pub const WRITABLE: u8 = 1;
    pub const ENUMERABLE: u8 = 2;
    pub const CONFIGURABLE: u8 = 4;
    /// The slot holds an `AccessorPair` object.
    pub const ACCESSOR: u8 = 8;
    pub const DEFAULT: u8 = WRITABLE | ENUMERABLE | CONFIGURABLE;
    /// Built-in methods: writable, configurable, not enumerable.
    pub const HIDDEN: u8 = WRITABLE | CONFIGURABLE;
    pub const NONE: u8 = 0;
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ShapeId(pub u32);

/// A hidden class: the ordered keys and attributes of named properties.
/// Shapes form a transition tree; objects built the same way share one.
pub struct Shape {
    pub parent: Option<ShapeId>,
    pub key: Option<Key>,
    pub attrs: u8,
    /// Number of properties (= slots).
    pub len: u32,
    pub transitions: Vec<(Key, u8, ShapeId)>,
    /// Key -> (slot, attrs) for shapes with more than a few keys.
    pub table: Option<Rc<BTreeMap<Key, (u32, u8)>>>,
}

pub const TABLE_MIN: u32 = 8;
/// Objects with more named properties than this become dictionaries.
pub const DICT_THRESHOLD: u32 = 64;

/// Named properties.
pub enum Props {
    Shaped { shape: ShapeId, slots: Vec<Value> },
    Dict(Box<Dict>),
}

/// Insertion-ordered dictionary of properties (removals leave holes that
/// are compacted when there are many).
#[derive(Default)]
pub struct Dict {
    pub entries: Vec<Option<(Key, Value, u8)>>,
    pub index: BTreeMap<Key, u32>,
    pub holes: u32,
}

impl Dict {
    pub fn get(&self, k: Key) -> Option<(Value, u8)> {
        let i = *self.index.get(&k)?;
        self.entries[i as usize].map(|(_, v, a)| (v, a))
    }

    pub fn set(&mut self, k: Key, v: Value, a: u8) {
        match self.index.get(&k) {
            Some(&i) => self.entries[i as usize] = Some((k, v, a)),
            None => {
                self.index.insert(k, self.entries.len() as u32);
                self.entries.push(Some((k, v, a)));
            }
        }
    }

    pub fn remove(&mut self, k: Key) -> bool {
        let Some(i) = self.index.remove(&k) else { return false };
        self.entries[i as usize] = None;
        self.holes += 1;
        if self.holes > 16 && self.holes as usize * 2 > self.entries.len() {
            self.entries.retain(|e| e.is_some());
            self.holes = 0;
            self.index.clear();
            for (j, e) in self.entries.iter().enumerate() {
                self.index.insert(e.unwrap().0, j as u32);
            }
        }
        true
    }

    pub fn len(&self) -> usize {
        self.index.len()
    }

    pub fn is_empty(&self) -> bool {
        self.index.is_empty()
    }
}

/// A native function: `(vm, this, args, new_target) -> result`;
/// `new_target` is undefined for a plain call.
pub type NativeFn = fn(&mut crate::vm::Vm, Value, &[Value], Value) -> crate::vm::JsResult<Value>;

#[derive(Clone)]
pub struct Closure {
    pub code: Rc<Code>,
    pub env: Option<crate::vm::EnvId>,
    /// `[[HomeObject]]` for `super` property access.
    pub home: Option<ObjId>,
    /// Lexical `this`/new.target of arrows are captured through the
    /// environment; class constructors keep their fields here.
    pub fields: Option<Value>,
    /// The class constructor's `[[PrivateMethods]]` (functions and accessor
    /// pairs stored with their private key).
    pub private_methods: Option<Rc<Vec<(Value, Value, u8)>>>,
}

pub struct Native {
    pub f: NativeFn,
    pub ctor: bool,
    /// Values captured by the native (e.g. a promise's resolve functions).
    pub data: Box<[Value]>,
}

/// Internal class of an object (its "exotic" behavior and internal slots).
pub enum Class {
    Ordinary,
    Array,
    Function(Box<Closure>),
    Native(Box<Native>),
    Bound {
        target: ObjId,
        this: Value,
        args: Box<[Value]>,
    },
    Error,
    Boolean(bool),
    Number(f64),
    String(StrId),
    Symbol(SymId),
    BigInt(BigId),
    Date(f64),
    RegExp(Box<crate::builtins::regexp::RegExpData>),
    Map(Box<crate::builtins::collections::OrderedMap>),
    Set(Box<crate::builtins::collections::OrderedMap>),
    WeakMap(Box<BTreeMap<u32, Value>>),
    WeakSet(Box<BTreeMap<u32, ()>>),
    WeakRef(Value),
    FinalizationRegistry(Box<crate::builtins::collections::Registry>),
    Promise(Box<crate::builtins::promise::PromiseData>),
    Proxy {
        target: Option<ObjId>,
        handler: Option<ObjId>,
        callable: bool,
        ctor: bool,
    },
    ArrayBuffer(Box<crate::builtins::buffer::BufferData>),
    TypedArray(Box<crate::builtins::buffer::TypedArrayData>),
    DataView(Box<crate::builtins::buffer::ViewData>),
    /// Mapped (sloppy, simple params) or unmapped arguments.
    Arguments(Option<Box<crate::vm::MappedArgs>>),
    Generator(Box<crate::vm::GenState>),
    AsyncGenerator(Box<crate::vm::GenState>),
    AsyncFromSyncIterator(Value, Value),
    /// Built-in iterators: (kind, iterated object, position, extra).
    Iter(Box<crate::builtins::iterators::IterState>),
    ForIn(Box<crate::vm::ForInState>),
    AccessorPair {
        get: Value,
        set: Value,
    },
    /// Module namespace exotic object.
    Namespace(u32),
    /// A plain object whose internal state is a list of values (realm
    /// records, promise capabilities, …).
    Internal(Box<[Value]>),
}

pub struct Object {
    pub proto: Option<ObjId>,
    pub props: Props,
    /// Dense indexed elements with default attributes (`Value::HOLE` for
    /// missing ones). Other indexed properties live in `props`.
    pub elements: Vec<Value>,
    /// Arrays: the `length` (may exceed `elements.len()`).
    pub length: u32,
    pub extensible: bool,
    /// `length` of an array is non-writable.
    pub length_readonly: bool,
    /// Some indexed property lives in `props` (sparse or special).
    pub sparse: bool,
    /// Every element is frozen (Object.freeze/seal on elements).
    pub frozen_elements: u8,
    pub class: Class,
}

impl Object {
    pub fn new(proto: Option<ObjId>, shape: ShapeId, class: Class) -> Object {
        Object {
            proto,
            props: Props::Shaped { shape, slots: Vec::new() },
            elements: Vec::new(),
            length: 0,
            extensible: true,
            length_readonly: false,
            sparse: false,
            frozen_elements: 0,
            class,
        }
    }

    pub fn is_array(&self) -> bool {
        matches!(self.class, Class::Array)
    }

    pub fn is_callable(&self) -> bool {
        match &self.class {
            Class::Function(_) | Class::Native(_) | Class::Bound { .. } => true,
            Class::Proxy { callable, .. } => *callable,
            _ => false,
        }
    }
}

/// A symbol: description and whether it is a private name.
pub struct Symbol {
    pub description: Option<StrId>,
    /// `Symbol.for` registry key.
    pub registered: bool,
    /// A private name (`#x`): never exposed as a property key.
    pub private: bool,
}
