//! The realm: well-known names (atoms), intrinsic objects and the global
//! object, created at VM start.

use alloc::boxed::Box;
use alloc::vec::Vec;

use super::Vm;
use crate::object::{attr, Class, Key, Native, NativeFn};
use crate::value::{ObjId, StrId, SymId, Value};

macro_rules! names {
    ($($field:ident = $text:expr),* $(,)?) => {
        /// Atoms for names the engine uses itself.
        #[derive(Default)]
        pub struct Names {
            pub empty: StrId,
            pub ascii: Vec<StrId>,
            pub digits: Vec<StrId>,
            $(pub $field: StrId,)*
        }

        fn init_names(vm: &mut Vm) {
            vm.names.empty = vm.intern("");
            let mut ascii = Vec::new();
            for c in 0u8..128 {
                let s = alloc::string::String::from(c as char);
                ascii.push(vm.intern(&s));
            }
            vm.names.digits = (0..10).map(|i| ascii[b'0' as usize + i]).collect();
            vm.names.ascii = ascii;
            $(vm.names.$field = vm.intern($text);)*
        }
    };
}

names! {
    length = "length", name = "name", prototype = "prototype", constructor = "constructor",
    message = "message", stack = "stack", cause = "cause", errors = "errors",
    value = "value", done = "done", next = "next", return_ = "return", throw_ = "throw",
    get = "get", set = "set", writable = "writable", enumerable = "enumerable", configurable = "configurable",
    to_string = "toString", value_of = "valueOf", callee = "callee", caller = "caller", arguments = "arguments",
    undefined = "undefined", null = "null", true_ = "true", false_ = "false",
    object = "object", function = "function", number = "number", string = "string", boolean = "boolean",
    symbol = "symbol", bigint = "bigint", default = "default",
    then = "then", index = "index", input = "input", groups = "groups", indices = "indices",
    last_index = "lastIndex", flags = "flags", source = "source", raw = "raw",
    proto = "__proto__", join = "join", exec = "exec", global_this = "globalThis",
    nan = "NaN", infinity = "Infinity", neg_infinity = "-Infinity", resolve = "resolve",
}

macro_rules! intrinsics {
    ($($field:ident),* $(,)?) => {
        /// Intrinsic objects of the realm.
        #[derive(Default)]
        pub struct Intrinsics {
            $(pub $field: ObjId,)*
            pub sym_iterator: SymId,
            pub sym_async_iterator: SymId,
            pub sym_has_instance: SymId,
            pub sym_is_concat_spreadable: SymId,
            pub sym_match: SymId,
            pub sym_match_all: SymId,
            pub sym_replace: SymId,
            pub sym_search: SymId,
            pub sym_species: SymId,
            pub sym_split: SymId,
            pub sym_to_primitive: SymId,
            pub sym_to_string_tag: SymId,
            pub sym_unscopables: SymId,
        }

        impl Intrinsics {
            /// Every intrinsic object (GC roots).
            pub fn all(&self) -> Vec<ObjId> {
                alloc::vec![$(self.$field,)*]
            }
        }
    };
}

intrinsics! {
    object_prototype, function_prototype, array_prototype, string_prototype, number_prototype,
    boolean_prototype, symbol_prototype, bigint_prototype, error_prototype,
    type_error_prototype, range_error_prototype, reference_error_prototype, syntax_error_prototype,
    eval_error_prototype, uri_error_prototype, aggregate_error_prototype,
    iterator_prototype, async_iterator_prototype, array_iterator_prototype, string_iterator_prototype,
    map_iterator_prototype, set_iterator_prototype, regexp_string_iterator_prototype,
    generator_prototype, async_generator_prototype, generator_function_prototype,
    async_function_prototype, async_generator_function_prototype, async_from_sync_iterator_prototype,
    iterator_helper_prototype, wrap_for_valid_iterator_prototype,
    promise_prototype, promise, regexp_prototype, regexp, date_prototype, map_prototype, set_prototype,
    weakmap_prototype, weakset_prototype, weakref_prototype, finalization_registry_prototype,
    arraybuffer_prototype, shared_arraybuffer_prototype, dataview_prototype, typed_array_prototype, typed_array,
    proxy, object, function, array, array_values, eval, throw_type_error,
    int8_array_prototype, uint8_array_prototype, uint8_clamped_array_prototype, int16_array_prototype,
    uint16_array_prototype, int32_array_prototype, uint32_array_prototype, float16_array_prototype,
    float32_array_prototype, float64_array_prototype, bigint64_array_prototype, biguint64_array_prototype,
    int8_array, uint8_array, uint8_clamped_array, int16_array, uint16_array, int32_array, uint32_array,
    float16_array, float32_array, float64_array, bigint64_array, biguint64_array,
    arraybuffer, shared_arraybuffer, error, type_error, range_error, syntax_error, reference_error,
    async_function, generator_function, async_generator_function, json, math, reflect, atomics, iterator,
}

pub fn init(vm: &mut Vm) {
    init_names(vm);
    let wk = |vm: &mut Vm, d: &str| {
        let s = vm.intern(d);
        vm.new_symbol(Some(s))
    };
    vm.intrinsics.sym_iterator = wk(vm, "Symbol.iterator");
    vm.intrinsics.sym_async_iterator = wk(vm, "Symbol.asyncIterator");
    vm.intrinsics.sym_has_instance = wk(vm, "Symbol.hasInstance");
    vm.intrinsics.sym_is_concat_spreadable = wk(vm, "Symbol.isConcatSpreadable");
    vm.intrinsics.sym_match = wk(vm, "Symbol.match");
    vm.intrinsics.sym_match_all = wk(vm, "Symbol.matchAll");
    vm.intrinsics.sym_replace = wk(vm, "Symbol.replace");
    vm.intrinsics.sym_search = wk(vm, "Symbol.search");
    vm.intrinsics.sym_species = wk(vm, "Symbol.species");
    vm.intrinsics.sym_split = wk(vm, "Symbol.split");
    vm.intrinsics.sym_to_primitive = wk(vm, "Symbol.toPrimitive");
    vm.intrinsics.sym_to_string_tag = wk(vm, "Symbol.toStringTag");
    vm.intrinsics.sym_unscopables = wk(vm, "Symbol.unscopables");

    // The two roots of everything.
    let op = vm.alloc_object(None, Class::Ordinary);
    vm.intrinsics.object_prototype = op;
    let fp = vm.alloc_object(Some(op), Class::Native(Box::new(Native { f: |_, _, _, _| Ok(Value::UNDEFINED), ctor: false, data: Box::new([]) })));
    vm.intrinsics.function_prototype = fp;
    let global = vm.alloc_object(Some(op), Class::Ordinary);
    vm.global = global;
    vm.global_this = global;
    crate::builtins::init(vm);
}

impl Vm {
    /// Creates a native function object.
    pub fn native_fn(&mut self, name: &str, length: u32, f: NativeFn) -> ObjId {
        self.native_fn_data(name, length, f, false, Box::new([]))
    }

    pub fn native_fn_data(&mut self, name: &str, length: u32, f: NativeFn, ctor: bool, data: Box<[Value]>) -> ObjId {
        let p = self.intrinsics.function_prototype;
        let o = self.alloc_object(Some(p), Class::Native(Box::new(Native { f, ctor, data })));
        let kl = Key::Str(self.names.length);
        self.define_data(o, kl, Value::from_u32(length), attr::CONFIGURABLE);
        let n = self.new_string(name);
        let kn = Key::Str(self.names.name);
        self.define_data(o, kn, n, attr::CONFIGURABLE);
        o
    }

    /// Defines a built-in method on `o`.
    pub fn method(&mut self, o: ObjId, name: &str, length: u32, f: NativeFn) -> ObjId {
        let func = self.native_fn(name, length, f);
        let k = self.key(name);
        self.define_data(o, k, Value::object(func), attr::HIDDEN);
        func
    }

    /// Defines a built-in method keyed by a symbol; `name` like "[Symbol.iterator]".
    pub fn method_sym(&mut self, o: ObjId, sym: SymId, name: &str, length: u32, f: NativeFn) -> ObjId {
        let func = self.native_fn(name, length, f);
        self.define_data(o, Key::Sym(sym), Value::object(func), attr::HIDDEN);
        func
    }

    /// Defines a built-in getter (and optional setter).
    pub fn getter(&mut self, o: ObjId, name: &str, get: NativeFn, set: Option<NativeFn>) {
        let gname = alloc::format!("get {name}");
        let g = self.native_fn(&gname, 0, get);
        let s = match set {
            Some(sf) => {
                let sname = alloc::format!("set {name}");
                Value::object(self.native_fn(&sname, 1, sf))
            }
            None => Value::UNDEFINED,
        };
        let k = self.key(name);
        self.define_accessor(o, k, Value::object(g), s, attr::CONFIGURABLE);
    }

    pub fn getter_sym(&mut self, o: ObjId, sym: SymId, name: &str, get: NativeFn) {
        let gname = alloc::format!("get {name}");
        let g = self.native_fn(&gname, 0, get);
        self.define_accessor(o, Key::Sym(sym), Value::object(g), Value::UNDEFINED, attr::CONFIGURABLE);
    }

    /// Defines a constant data property (non-writable etc. per `attrs`).
    pub fn prop(&mut self, o: ObjId, name: &str, v: Value, attrs: u8) {
        let k = self.key(name);
        self.define_data(o, k, v, attrs);
    }

    /// A constructor function with its prototype object linked both ways.
    pub fn constructor(&mut self, name: &str, length: u32, f: NativeFn, proto: ObjId) -> ObjId {
        let c = self.native_fn_data(name, length, f, true, Box::new([]));
        let kp = Key::Str(self.names.prototype);
        self.define_data(c, kp, Value::object(proto), attr::NONE);
        let kc = Key::Str(self.names.constructor);
        self.define_data(proto, kc, Value::object(c), attr::HIDDEN);
        c
    }

    /// Puts a value on the global object (writable, configurable).
    pub fn global_prop(&mut self, name: &str, v: Value) {
        let g = self.global;
        self.prop(g, name, v, attr::HIDDEN);
    }

    /// The data captured by the running native function.
    pub fn native_data(&self, i: usize) -> Value {
        match self.current_native.as_object().map(|o| &self.obj(o).class) {
            Some(Class::Native(n)) => n.data.get(i).copied().unwrap_or(Value::UNDEFINED),
            _ => Value::UNDEFINED,
        }
    }

    pub fn set_native_data(&mut self, f: ObjId, i: usize, v: Value) {
        if let Class::Native(n) = &mut self.obj_mut(f).class {
            n.data[i] = v;
        }
    }
}
