//! Function constructor and Function.prototype.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;

use super::object::arg;
use crate::object::{attr, Class, Key};
use crate::value::Value;
use crate::vm::{JsResult, Vm};

pub fn init(vm: &mut Vm) {
    let fp = vm.intrinsics.function_prototype;
    let kl = Key::Str(vm.names.length);
    vm.define_data(fp, kl, Value::int(0), attr::CONFIGURABLE);
    let kn = Key::Str(vm.names.name);
    let e = Value::str(vm.names.empty);
    vm.define_data(fp, kn, e, attr::CONFIGURABLE);
    let c = vm.constructor("Function", 1, |vm, _, a, nt| dynamic_function(vm, a, nt, false, false), fp);
    vm.intrinsics.function = c;
    vm.global_prop("Function", Value::object(c));
    vm.method(fp, "call", 1, |vm, this, a, _| {
        if !vm.is_callable(this) {
            return Err(vm.type_error("Function.prototype.call called on non-callable"));
        }
        let rest = if a.len() > 1 { &a[1..] } else { &[] };
        vm.call(this, arg(a, 0), rest)
    });
    vm.method(fp, "apply", 2, |vm, this, a, _| {
        if !vm.is_callable(this) {
            return Err(vm.type_error("Function.prototype.apply was called on a non-callable"));
        }
        let list = arg(a, 1);
        if list.is_nullish() {
            return vm.call(this, arg(a, 0), &[]);
        }
        let args = create_list_from_array_like(vm, list)?;
        vm.call(this, arg(a, 0), &args)
    });
    vm.method(fp, "bind", 1, bind);
    vm.method(fp, "toString", 0, function_to_string);
    let hi = vm.intrinsics.sym_has_instance;
    let f = vm.native_fn("[Symbol.hasInstance]", 1, |vm, this, a, _| Ok(Value::bool(vm.ordinary_has_instance(this, arg(a, 0))?)));
    vm.define_data(fp, Key::Sym(hi), Value::object(f), attr::NONE);
    // %ThrowTypeError%
    let tte = vm.native_fn("", 0, |vm, _, _, _| Err(vm.type_error("'caller', 'callee', and 'arguments' properties may not be accessed on strict mode functions or the arguments objects for calls to them")));
    vm.obj_mut(tte).extensible = false;
    let kl = Key::Str(vm.names.length);
    let kn = Key::Str(vm.names.name);
    vm.define_data(tte, kl, Value::int(0), attr::NONE);
    vm.define_data(tte, kn, e, attr::NONE);
    vm.intrinsics.throw_type_error = tte;
    let t = Value::object(tte);
    let kc = Key::Str(vm.names.caller);
    let ka = Key::Str(vm.names.arguments);
    vm.define_accessor(fp, kc, t, t, attr::CONFIGURABLE);
    vm.define_accessor(fp, ka, t, t, attr::CONFIGURABLE);
}

/// CreateListFromArrayLike.
pub fn create_list_from_array_like(vm: &mut Vm, v: Value) -> JsResult<Vec<Value>> {
    let Some(o) = v.as_object() else {
        return Err(vm.type_error("CreateListFromArrayLike called on non-object"));
    };
    if let Class::Array = vm.obj(o).class {
        let ob = vm.obj(o);
        if ob.elements.len() == ob.length as usize && !ob.sparse && ob.elements.iter().all(|v| !v.is_hole()) {
            return Ok(ob.elements.clone());
        }
    }
    let n = vm.length_of(o)?;
    let mut out = Vec::with_capacity(n.min(1 << 16) as usize);
    for i in 0..n {
        let k = vm.index_key(i);
        out.push(vm.get(o, k)?);
    }
    Ok(out)
}

fn bind(vm: &mut Vm, this: Value, a: &[Value], _: Value) -> JsResult<Value> {
    let Some(target) = this.as_object() else {
        return Err(vm.type_error("Bind must be called on a function"));
    };
    if !vm.is_callable(this) {
        return Err(vm.type_error("Bind must be called on a function"));
    }
    let bargs: Box<[Value]> = a.iter().skip(1).copied().collect();
    let nb = bargs.len() as f64;
    let proto = vm.get_prototype_of(target)?;
    let f = vm.alloc_object(proto, Class::Bound { target, this: arg(a, 0), args: bargs });
    // length
    let kl = Key::Str(vm.names.length);
    let mut len = 0.0;
    if vm.has_own_property(target, kl)? {
        let l = vm.get(target, kl)?;
        if let Some(n) = l.as_number() {
            if n == f64::INFINITY {
                len = f64::INFINITY;
            } else if n.is_finite() {
                len = (crate::math::trunc(n) - nb).max(0.0);
            }
        }
    }
    vm.define_data(f, kl, Value::number(len), attr::CONFIGURABLE);
    let kn = Key::Str(vm.names.name);
    let n = vm.get(target, kn)?;
    let ns = match n.as_string() {
        Some(s) => vm.str(s).to_rust_lossy(),
        None => String::new(),
    };
    let name = vm.new_string(&alloc::format!("bound {ns}"));
    vm.define_data(f, kn, name, attr::CONFIGURABLE);
    Ok(Value::object(f))
}

fn function_to_string(vm: &mut Vm, this: Value, _: &[Value], _: Value) -> JsResult<Value> {
    let Some(o) = this.as_object() else {
        return Err(vm.type_error("Function.prototype.toString requires that 'this' be a Function"));
    };
    let text = match &vm.obj(o).class {
        Class::Function(c) => {
            let src = &c.code.source.text;
            let (s, e) = (c.code.src_start as usize, c.code.src_end as usize);
            if e > s && e <= src.len() {
                String::from(&src[s..e])
            } else {
                String::from("function () { [native code] }")
            }
        }
        Class::Native(_) | Class::Bound { .. } => {
            let kn = Key::Str(vm.names.name);
            let n = match vm.raw_get_own(o, kn) {
                Some(crate::vm::props::Slot::Data(v, _)) if v.is_string() => vm.display(v),
                _ => String::new(),
            };
            alloc::format!("function {n}() {{ [native code] }}")
        }
        Class::Proxy { callable: true, .. } => String::from("function () { [native code] }"),
        _ => return Err(vm.type_error("Function.prototype.toString requires that 'this' be a Function")),
    };
    Ok(vm.new_string(&text))
}

/// CreateDynamicFunction (Function, AsyncFunction, GeneratorFunction…).
pub fn dynamic_function(vm: &mut Vm, a: &[Value], nt: Value, is_async: bool, is_gen: bool) -> JsResult<Value> {
    let mut params = String::new();
    let n = a.len();
    for (i, &v) in a.iter().enumerate().take(n.saturating_sub(1)) {
        if i > 0 {
            params.push(',');
        }
        let s = vm.to_string(v)?;
        params.push_str(&vm.str(s).to_rust_lossy());
    }
    let body = if n > 0 {
        let s = vm.to_string(a[n - 1])?;
        vm.str(s).to_rust_lossy()
    } else {
        String::new()
    };
    let f = match crate::parser::Parser::parse_function_parts(&params, &body, is_async, is_gen) {
        Ok(f) => f,
        Err(e) => return Err(vm.syntax_error(&e.message)),
    };
    let kw = match (is_async, is_gen) {
        (false, false) => "function",
        (false, true) => "function*",
        (true, false) => "async function",
        (true, true) => "async function*",
    };
    let src_text = alloc::format!("({kw} anonymous({params}\n) {{\n{body}\n}})");
    let source = vm.add_source("anonymous", src_text.as_str().into());
    let code = {
        let mut c = crate::compiler::Compiler::new(vm, source);
        c.toplevel_function(&f)
    };
    let code = match code {
        Ok(c) => c,
        Err(e) => return Err(vm.syntax_error(&e.message)),
    };
    let fo = vm.make_closure(code, None, None);
    let fallback: fn(&crate::vm::Intrinsics) -> crate::value::ObjId = match (is_async, is_gen) {
        (false, false) => |i| i.function_prototype,
        (false, true) => |i| i.generator_function_prototype,
        (true, false) => |i| i.async_function_prototype,
        (true, true) => |i| i.async_generator_function_prototype,
    };
    if !nt.is_undefined() {
        let p = vm.proto_from_ctor(nt, fallback)?;
        vm.obj_mut(fo).proto = Some(p);
    }
    Ok(Value::object(fo))
}
