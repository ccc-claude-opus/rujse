# rujse 內部介面（寫 built-in 用）

這份文件說明寫內建物件（`src/builtins/*.rs`）時會用到的引擎 API。程式碼是 `#![no_std]` + `alloc`：用 `alloc::vec::Vec`、`alloc::string::String`、`alloc::format!`、`alloc::boxed::Box`，不能用 `std`。浮點數學函式用 `crate::math::{trunc, floor, ceil, sqrt, powf, exp, ln, sin, …}`（`f64` 本身沒有這些方法）。不能用 `unsafe`。

## 1. 值

```rust
use crate::value::{Value, Kind, ObjId, StrId, SymId, BigId};
Value::UNDEFINED, Value::NULL, Value::TRUE, Value::FALSE, Value::NAN
Value::int(i32), Value::number(f64) /* 整數自動存成 int32 */, Value::bool(b)
Value::from_u32(n), Value::from_usize(n)
Value::str(StrId), Value::object(ObjId), Value::symbol(SymId), Value::bigint(BigId)
v.kind() -> Kind  // Undefined | Null | Bool(b) | Int(i) | Double(f) | Str(id) | Symbol(id) | Object(id) | BigInt(id)
v.is_undefined() is_null() is_nullish() is_number() is_string() is_object() is_symbol() is_bigint() is_bool()
v.as_number() -> Option<f64>; v.num() -> f64 (已知是 Number)；v.as_int(); v.as_object(); v.as_string(); v.as_symbol(); v.as_bigint()
```

`Value` 是 `Copy`。比較 JS 值不要用 `==`，用 `vm.strict_equals(a, b)`、`vm.same_value(a, b)`、`vm.same_value_zero(a, b)`。

## 2. 原生函式

```rust
pub type NativeFn = fn(&mut Vm, Value /*this*/, &[Value] /*args*/, Value /*new.target, 呼叫時為 undefined*/) -> JsResult<Value>;
type JsResult<T> = Result<T, Throw>;   // Throw::Value(v) 是 JS 例外；Throw::Terminate 是中斷，只能往外傳
use crate::builtins::object::arg;  // arg(args, i) -> Value，缺少的參數是 undefined
```

- 丟例外：`return Err(vm.type_error("…"))`，另有 `range_error`、`reference_error`、`syntax_error`；或 `vm.throw_type("…")`。
- 呼叫 JS：`vm.call(f, this, &[args])`、`vm.construct(f, &[args], new_target)`。
- **GC 規則**：GC 只會在呼叫 JS（`vm.call`/`vm.construct`、getter、setter、Proxy trap、`to_primitive` 等）期間發生。若某個本地變數中的物件在呼叫 JS 之後還要用、而且沒有被其他地方參照（例如剛建立的結果陣列），先 `vm.root(v)`；函式返回時自動釋放。`args` 與 `this` 已經是 root。
- 閉包資料：`vm.native_fn_data(name, length, f, is_ctor, Box::new([v0, v1]))` 建立帶資料的原生函式，執行時用 `vm.native_data(i)` 讀、`vm.set_native_data(func_obj, i, v)` 寫；目前執行中的函式物件是 `vm.current_native`。

## 3. 定義內建物件（`src/vm/realm.rs`）

```rust
vm.method(obj, "name", length, f) -> ObjId          // writable、configurable、不可列舉
vm.method_sym(obj, sym, "[Symbol.iterator]", 0, f)
vm.getter(obj, "name", get_fn, Some(set_fn))       // 存取器，configurable
vm.getter_sym(obj, sym, "[Symbol.species]", get_fn)
vm.prop(obj, "NAME", value, attrs)                 // attrs: crate::object::attr::{NONE, WRITABLE, ENUMERABLE, CONFIGURABLE, HIDDEN(=W|C), DEFAULT(=W|E|C)}
vm.constructor("Name", length, f, proto) -> ObjId  // 建構子與 prototype 互相連結
vm.global_prop("Name", value)
vm.native_fn(name, length, f) -> ObjId
vm.alloc_object(Some(proto), Class::Ordinary) -> ObjId; vm.new_object(); vm.new_array(Vec<Value>) -> ObjId
vm.define_data(obj, key, value, attrs); vm.define_accessor(obj, key, get, set, attrs)   // 直接寫入，不檢查
```

內建原型與建構子在 `vm.intrinsics.*`（見 `src/vm/realm.rs` 的 `intrinsics!` 清單，例如 `object_prototype`、`array_prototype`、`string_prototype`、`number_prototype`、`map_prototype`、`iterator_prototype`、`type_error_prototype`），常用名稱的原子在 `vm.names.*`（`length`、`name`、`prototype`、`constructor`、`value`、`done`、`next`…），Well-known symbols 在 `vm.intrinsics.sym_iterator`、`sym_species`、`sym_to_string_tag`、`sym_to_primitive` 等。

## 4. 物件操作（`src/vm/props.rs`、`src/vm/conv.rs`）

```rust
use crate::object::{Key, Class, attr};
vm.key("name") -> Key;  vm.index_key(u64) -> Key;  vm.to_property_key(v)? -> Key;  vm.key_to_value(k) -> Value
vm.get(o, k)?; vm.get_with(o, k, receiver)?; vm.get_v(value, k)?   // get_v 也接受原始型別
vm.set(o, k, v)? -> bool;  vm.set_or_throw(o, k, v)?  // Set(O, P, V, true)
vm.has_property(o, k)?; vm.has_own_property(o, k)?; vm.delete(o, k)? -> bool; vm.delete_or_throw(o, k)?
vm.get_own_property(o, k)? -> Option<PropDesc>; vm.define_own_property(o, k, desc)? -> bool; vm.define_property_or_throw(o, k, desc)?
vm.create_data_property(o, k, v)? -> bool; vm.create_data_property_or_throw(o, k, v)?
vm.own_keys(o)? -> Vec<Key>;  vm.get_prototype_of(o)?;  vm.set_prototype_of(o, p)?; vm.is_extensible(o)?
vm.length_of(o)? -> u64            // LengthOfArrayLike
vm.get_method(v, k)? -> Option<Value>
vm.is_callable(v); vm.is_constructor(v)
vm.get_iterator(v)? -> (iter, next); vm.iterator_step(iter, next)? -> Option<Value>; vm.iterator_close(iter)?
vm.iter_result(value, done) -> Value
vm.to_boolean(v); vm.to_number(v)? -> f64; vm.to_numeric(v)?; vm.to_string(v)? -> StrId; vm.to_object(v)? -> ObjId
vm.to_integer_or_infinity(v)? -> f64; vm.to_int32(v)?; vm.to_uint32(v)?; vm.to_length(v)? -> u64; vm.to_index(v)? -> u64
vm.to_primitive(v, Hint::{Default, Number, String})?
vm.proto_from_ctor(new_target, |i| i.xxx_prototype)? -> ObjId   // GetPrototypeFromConstructor
crate::builtins::array::{is_array, species_create, array_create, iterable_to_list}
crate::builtins::promise::species_constructor(vm, o, default)?
crate::vm::conv::{number_to_string(f64) -> String, string_to_number(&JsStr) -> f64, to_int32_f, to_uint32_f, js_rem}
```

`PropDesc { value, get, set, writable, enumerable, configurable }`（每個欄位都是 `Option`），`PropDesc::data(v, attrs)`、`PropDesc::accessor(g, s, attrs)`。

## 5. 字串

字串是 UTF-16 code unit 序列（`crate::string::JsStr`），存成 Latin-1 或 UTF-16。

```rust
vm.new_string("rust str") -> Value; vm.new_string_units(&[u16]) -> Value
vm.str(id) -> &JsStr;  s.len(); s.at(i) -> u16; s.units() (迭代器); s.to_units() -> Vec<u16>;
s.to_rust_lossy() -> String; s.eq_str("…"); s.cmp_units(&other); s.slice(a, b) -> JsStr; s.concat(&other)
vm.alloc_string(JsStr) -> StrId;  vm.char_string(u16) -> Value;  Value::str(vm.names.empty)
vm.intern("…") -> StrId (屬性鍵用的原子)
```

## 6. 內部 class

物件的內部狀態在 `vm.obj(o).class`（`crate::object::Class`）：`Array`、`String(StrId)`、`Number(f64)`、`Boolean(bool)`、`Date(f64)`、`Map(Box<OrderedMap>)`、`Set(…)`、`TypedArray(…)` 等。陣列的快速路徑：`vm.obj(o).elements`（`Value::HOLE` 代表空洞）與 `vm.obj(o).length`，只有 `Class::Array` 才能直接用，且要先確認沒有 `sparse`、`frozen_elements == 0`。拿不準就用一般的 `get`/`set`。

## 7. 測試

- 單元測試：`cargo test --lib`。
- 執行腳本：`cargo build --release --bin rujse && ./target/release/rujse -e 'print([1,2].map(x => x * 2).join())'`。
- test262：`cargo build --release -p test262 && ./target/release/test262 built-ins/Array --failures target/f.txt`，失敗原因在 `target/f.txt`（加 `-v` 印在畫面）。
