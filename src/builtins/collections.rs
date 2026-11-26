//! Map, Set, WeakMap, WeakSet, WeakRef, FinalizationRegistry.

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use super::object::arg;
use crate::object::{attr, Class, Key};
use crate::value::{Kind, ObjId, Value};
use crate::vm::gc::Trace;
use crate::vm::{JsResult, Vm};

/// Insertion-ordered map with tombstones (iterators stay valid while
/// entries are added or removed); a hash index finds keys.
#[derive(Default)]
pub struct OrderedMap {
    pub entries: Vec<Option<(Value, Value)>>,
    pub live: usize,
    /// Hash of the key -> entry positions.
    index: BTreeMap<u64, Vec<u32>>,
}

impl Trace for OrderedMap {
    fn trace(&self, out: &mut Vec<Value>) {
        for (k, v) in self.entries.iter().flatten() {
            out.push(*k);
            out.push(*v);
        }
    }
}

/// A hash of a key consistent with SameValueZero.
fn key_hash(vm: &Vm, k: Value) -> u64 {
    match k.kind() {
        Kind::Str(s) => {
            let mut h = 0xcbf2_9ce4_8422_2325u64;
            for u in vm.str(s).units() {
                h ^= u as u64;
                h = h.wrapping_mul(0x0100_0000_01b3);
            }
            h | 1
        }
        Kind::Int(i) => (i as f64).to_bits(),
        Kind::Double(d) => {
            if d.is_nan() {
                f64::NAN.to_bits()
            } else if d == 0.0 {
                0f64.to_bits()
            } else {
                d.to_bits()
            }
        }
        Kind::BigInt(b) => {
            let x = vm.bigint(b);
            let mut h = x.neg as u64;
            for l in &x.mag {
                h = h.wrapping_mul(31).wrapping_add(*l as u64);
            }
            h ^ 0xb16
        }
        _ => k.raw(),
    }
}

fn normalize(k: Value) -> Value {
    if k.as_number() == Some(0.0) { Value::int(0) } else { k }
}

impl OrderedMap {
    pub fn find(&self, vm: &Vm, k: Value) -> Option<usize> {
        let h = key_hash(vm, k);
        let list = self.index.get(&h)?;
        for &i in list {
            if let Some((x, _)) = self.entries[i as usize] {
                if vm.same_value_zero(x, k) {
                    return Some(i as usize);
                }
            }
        }
        None
    }

    pub fn insert(&mut self, vm: &Vm, k: Value, v: Value) {
        let k = normalize(k);
        if let Some(i) = self.find(vm, k) {
            self.entries[i] = Some((k, v));
            return;
        }
        let h = key_hash(vm, k);
        self.index.entry(h).or_default().push(self.entries.len() as u32);
        self.entries.push(Some((k, v)));
        self.live += 1;
    }

    pub fn remove(&mut self, vm: &Vm, k: Value) -> bool {
        let Some(i) = self.find(vm, k) else { return false };
        let h = key_hash(vm, k);
        if let Some(list) = self.index.get_mut(&h) {
            list.retain(|&x| x as usize != i);
        }
        self.entries[i] = None;
        self.live -= 1;
        true
    }

    pub fn clear(&mut self) {
        for e in self.entries.iter_mut() {
            *e = None;
        }
        self.index.clear();
        self.live = 0;
    }
}

pub struct RegistryCell {
    pub target: Value,
    pub held: Value,
    pub token: Value,
}

pub struct Registry {
    pub cleanup: Value,
    pub cells: Vec<RegistryCell>,
}

impl Trace for Registry {
    fn trace(&self, out: &mut Vec<Value>) {
        out.push(self.cleanup);
        for c in &self.cells {
            out.push(c.held);
            // Tokens are held weakly but symbols/objects as tokens must stay
            // valid ids while registered: keep them (minor leak by spec
            // standards, never observable).
            out.push(c.token);
        }
    }
}

fn map_data(vm: &mut Vm, this: Value, set: bool) -> JsResult<ObjId> {
    if let Some(o) = this.as_object() {
        match (&vm.obj(o).class, set) {
            (Class::Map(_), false) | (Class::Set(_), true) => return Ok(o),
            _ => {}
        }
    }
    Err(vm.type_error(if set { "Set method called on incompatible receiver" } else { "Map method called on incompatible receiver" }))
}

fn table(vm: &mut Vm, o: ObjId) -> &mut OrderedMap {
    match &mut vm.obj_mut(o).class {
        Class::Map(t) | Class::Set(t) => t,
        _ => unreachable!(),
    }
}

fn table_ref(vm: &Vm, o: ObjId) -> &OrderedMap {
    match &vm.obj(o).class {
        Class::Map(t) | Class::Set(t) => t,
        _ => unreachable!(),
    }
}

/// Map.prototype.set semantics on a Map object.
pub fn map_set(vm: &mut Vm, m: ObjId, k: Value, v: Value) {
    let mut t = core::mem::take(table(vm, m));
    let before = t.live;
    t.insert(vm, k, v);
    if t.live > before {
        // An entry and its index node (see heap_estimate).
        vm.alloc_bytes += 80;
    }
    *table(vm, m) = t;
}

fn tbl_insert(vm: &mut Vm, o: ObjId, k: Value, v: Value) {
    map_set(vm, o, k, v);
}

fn tbl_find(vm: &Vm, o: ObjId, k: Value) -> Option<usize> {
    table_ref(vm, o).find(vm, k)
}

fn tbl_remove(vm: &mut Vm, o: ObjId, k: Value) -> bool {
    let mut t = core::mem::take(table(vm, o));
    let r = t.remove(vm, k);
    *table(vm, o) = t;
    r
}

pub fn init(vm: &mut Vm) {
    let op = vm.intrinsics.object_prototype;
    let ip = vm.intrinsics.iterator_prototype;
    let kt = Key::Sym(vm.intrinsics.sym_to_string_tag);
    let sp = vm.intrinsics.sym_species;
    let si = vm.intrinsics.sym_iterator;

    // ----- Map
    let mp = vm.alloc_object(Some(op), Class::Ordinary);
    vm.intrinsics.map_prototype = mp;
    let mc = vm.constructor("Map", 0, |vm, _, a, nt| collection_ctor(vm, a, nt, false), mp);
    vm.global_prop("Map", Value::object(mc));
    vm.getter_sym(mc, sp, "[Symbol.species]", |_, this, _, _| Ok(this));
    vm.method(mc, "groupBy", 2, |vm, _, a, _| super::array::group_by(vm, arg(a, 0), arg(a, 1), true));
    vm.method(mp, "get", 1, |vm, this, a, _| {
        let o = map_data(vm, this, false)?;
        Ok(match tbl_find(vm, o, arg(a, 0)) {
            Some(i) => table_ref(vm, o).entries[i].unwrap().1,
            None => Value::UNDEFINED,
        })
    });
    vm.method(mp, "set", 2, |vm, this, a, _| {
        let o = map_data(vm, this, false)?;
        tbl_insert(vm, o, arg(a, 0), arg(a, 1));
        Ok(this)
    });
    vm.method(mp, "has", 1, |vm, this, a, _| {
        let o = map_data(vm, this, false)?;
        Ok(Value::bool(tbl_find(vm, o, arg(a, 0)).is_some()))
    });
    vm.method(mp, "delete", 1, |vm, this, a, _| {
        let o = map_data(vm, this, false)?;
        Ok(Value::bool(tbl_remove(vm, o, arg(a, 0))))
    });
    vm.method(mp, "clear", 0, |vm, this, _, _| {
        let o = map_data(vm, this, false)?;
        table(vm, o).clear();
        Ok(Value::UNDEFINED)
    });
    vm.method(mp, "forEach", 1, |vm, this, a, _| for_each(vm, this, a, false));
    vm.getter(mp, "size", |vm, this, _, _| {
        let o = map_data(vm, this, false)?;
        Ok(Value::from_usize(table_ref(vm, o).live))
    }, None);
    vm.method(mp, "keys", 0, |vm, this, _, _| make_iter(vm, this, false, 0));
    vm.method(mp, "values", 0, |vm, this, _, _| make_iter(vm, this, false, 1));
    let entries = vm.method(mp, "entries", 0, |vm, this, _, _| make_iter(vm, this, false, 2));
    vm.define_data(mp, Key::Sym(si), Value::object(entries), attr::HIDDEN);
    let tag = vm.new_string("Map");
    vm.define_data(mp, kt, tag, attr::CONFIGURABLE);
    let mip = vm.alloc_object(Some(ip), Class::Ordinary);
    vm.intrinsics.map_iterator_prototype = mip;
    vm.method(mip, "next", 0, |vm, this, _, _| iter_next(vm, this, false));
    let tag = vm.new_string("Map Iterator");
    vm.define_data(mip, kt, tag, attr::CONFIGURABLE);

    // ----- Set
    let stp = vm.alloc_object(Some(op), Class::Ordinary);
    vm.intrinsics.set_prototype = stp;
    let sc = vm.constructor("Set", 0, |vm, _, a, nt| collection_ctor(vm, a, nt, true), stp);
    vm.global_prop("Set", Value::object(sc));
    vm.getter_sym(sc, sp, "[Symbol.species]", |_, this, _, _| Ok(this));
    vm.method(stp, "add", 1, |vm, this, a, _| {
        let o = map_data(vm, this, true)?;
        let k = arg(a, 0);
        tbl_insert(vm, o, k, k);
        Ok(this)
    });
    vm.method(stp, "has", 1, |vm, this, a, _| {
        let o = map_data(vm, this, true)?;
        Ok(Value::bool(tbl_find(vm, o, arg(a, 0)).is_some()))
    });
    vm.method(stp, "delete", 1, |vm, this, a, _| {
        let o = map_data(vm, this, true)?;
        Ok(Value::bool(tbl_remove(vm, o, arg(a, 0))))
    });
    vm.method(stp, "clear", 0, |vm, this, _, _| {
        let o = map_data(vm, this, true)?;
        table(vm, o).clear();
        Ok(Value::UNDEFINED)
    });
    vm.method(stp, "forEach", 1, |vm, this, a, _| for_each(vm, this, a, true));
    vm.getter(stp, "size", |vm, this, _, _| {
        let o = map_data(vm, this, true)?;
        Ok(Value::from_usize(table_ref(vm, o).live))
    }, None);
    vm.method(stp, "entries", 0, |vm, this, _, _| make_iter(vm, this, true, 2));
    let values = vm.method(stp, "values", 0, |vm, this, _, _| make_iter(vm, this, true, 1));
    let kk = vm.key("keys");
    vm.define_data(stp, kk, Value::object(values), attr::HIDDEN);
    vm.define_data(stp, Key::Sym(si), Value::object(values), attr::HIDDEN);
    let tag = vm.new_string("Set");
    vm.define_data(stp, kt, tag, attr::CONFIGURABLE);
    for (name, op_kind) in [("union", 0u8), ("intersection", 1), ("difference", 2), ("symmetricDifference", 3), ("isSubsetOf", 4), ("isSupersetOf", 5), ("isDisjointFrom", 6)] {
        let f = vm.native_fn_data(name, 1, set_algebra, false, Box::new([Value::int(op_kind as i32)]));
        let k = vm.key(name);
        vm.define_data(stp, k, Value::object(f), attr::HIDDEN);
    }
    let sip = vm.alloc_object(Some(ip), Class::Ordinary);
    vm.intrinsics.set_iterator_prototype = sip;
    vm.method(sip, "next", 0, |vm, this, _, _| iter_next(vm, this, true));
    let tag = vm.new_string("Set Iterator");
    vm.define_data(sip, kt, tag, attr::CONFIGURABLE);

    // ----- WeakMap / WeakSet
    let wmp = vm.alloc_object(Some(op), Class::Ordinary);
    vm.intrinsics.weakmap_prototype = wmp;
    let wmc = vm.constructor("WeakMap", 0, |vm, _, a, nt| weak_ctor(vm, a, nt, false), wmp);
    vm.global_prop("WeakMap", Value::object(wmc));
    vm.method(wmp, "get", 1, |vm, this, a, _| {
        let o = weak_data(vm, this, false)?;
        let Some(k) = weak_key(vm, arg(a, 0)) else { return Ok(Value::UNDEFINED) };
        Ok(match &vm.obj(o).class {
            Class::WeakMap(t) => t.get(&k).copied().unwrap_or(Value::UNDEFINED),
            _ => Value::UNDEFINED,
        })
    });
    vm.method(wmp, "set", 2, |vm, this, a, _| {
        let o = weak_data(vm, this, false)?;
        let Some(k) = weak_key(vm, arg(a, 0)) else { return Err(vm.type_error("Invalid value used as weak map key")) };
        if let Class::WeakMap(t) = &mut vm.obj_mut(o).class {
            t.insert(k, arg(a, 1));
        }
        Ok(this)
    });
    vm.method(wmp, "has", 1, |vm, this, a, _| {
        let o = weak_data(vm, this, false)?;
        let Some(k) = weak_key(vm, arg(a, 0)) else { return Ok(Value::FALSE) };
        Ok(Value::bool(matches!(&vm.obj(o).class, Class::WeakMap(t) if t.contains_key(&k))))
    });
    vm.method(wmp, "delete", 1, |vm, this, a, _| {
        let o = weak_data(vm, this, false)?;
        let Some(k) = weak_key(vm, arg(a, 0)) else { return Ok(Value::FALSE) };
        Ok(Value::bool(match &mut vm.obj_mut(o).class {
            Class::WeakMap(t) => t.remove(&k).is_some(),
            _ => false,
        }))
    });
    let tag = vm.new_string("WeakMap");
    vm.define_data(wmp, kt, tag, attr::CONFIGURABLE);
    let wsp = vm.alloc_object(Some(op), Class::Ordinary);
    vm.intrinsics.weakset_prototype = wsp;
    let wsc = vm.constructor("WeakSet", 0, |vm, _, a, nt| weak_ctor(vm, a, nt, true), wsp);
    vm.global_prop("WeakSet", Value::object(wsc));
    vm.method(wsp, "add", 1, |vm, this, a, _| {
        let o = weak_data(vm, this, true)?;
        let Some(k) = weak_key(vm, arg(a, 0)) else { return Err(vm.type_error("Invalid value used in weak set")) };
        if let Class::WeakSet(t) = &mut vm.obj_mut(o).class {
            t.insert(k, ());
        }
        Ok(this)
    });
    vm.method(wsp, "has", 1, |vm, this, a, _| {
        let o = weak_data(vm, this, true)?;
        let Some(k) = weak_key(vm, arg(a, 0)) else { return Ok(Value::FALSE) };
        Ok(Value::bool(matches!(&vm.obj(o).class, Class::WeakSet(t) if t.contains_key(&k))))
    });
    vm.method(wsp, "delete", 1, |vm, this, a, _| {
        let o = weak_data(vm, this, true)?;
        let Some(k) = weak_key(vm, arg(a, 0)) else { return Ok(Value::FALSE) };
        Ok(Value::bool(match &mut vm.obj_mut(o).class {
            Class::WeakSet(t) => t.remove(&k).is_some(),
            _ => false,
        }))
    });
    let tag = vm.new_string("WeakSet");
    vm.define_data(wsp, kt, tag, attr::CONFIGURABLE);

    // ----- WeakRef
    let wrp = vm.alloc_object(Some(op), Class::Ordinary);
    vm.intrinsics.weakref_prototype = wrp;
    let wrc = vm.constructor("WeakRef", 1, |vm, _, a, nt| {
        if nt.is_undefined() {
            return Err(vm.type_error("Constructor WeakRef requires 'new'"));
        }
        let t = arg(a, 0);
        if !can_be_held_weakly(vm, t) {
            return Err(vm.type_error("WeakRef: target must be an object or non-registered symbol"));
        }
        let p = vm.proto_from_ctor(nt, |i| i.weakref_prototype)?;
        let o = vm.alloc_object(Some(p), Class::WeakRef(t));
        vm.kept_alive.push(t);
        Ok(Value::object(o))
    }, wrp);
    vm.global_prop("WeakRef", Value::object(wrc));
    vm.method(wrp, "deref", 0, |vm, this, _, _| match this.as_object().map(|o| &vm.obj(o).class) {
        Some(Class::WeakRef(t)) => {
            let t = *t;
            if !t.is_undefined() {
                vm.kept_alive.push(t);
            }
            Ok(t)
        }
        _ => Err(vm.type_error("WeakRef.prototype.deref called on incompatible receiver")),
    });
    let tag = vm.new_string("WeakRef");
    vm.define_data(wrp, kt, tag, attr::CONFIGURABLE);

    // ----- FinalizationRegistry
    let frp = vm.alloc_object(Some(op), Class::Ordinary);
    vm.intrinsics.finalization_registry_prototype = frp;
    let frc = vm.constructor("FinalizationRegistry", 1, |vm, _, a, nt| {
        if nt.is_undefined() {
            return Err(vm.type_error("Constructor FinalizationRegistry requires 'new'"));
        }
        let cb = arg(a, 0);
        if !vm.is_callable(cb) {
            return Err(vm.type_error("FinalizationRegistry: cleanup must be callable"));
        }
        let p = vm.proto_from_ctor(nt, |i| i.finalization_registry_prototype)?;
        let o = vm.alloc_object(Some(p), Class::FinalizationRegistry(Box::new(Registry { cleanup: cb, cells: Vec::new() })));
        Ok(Value::object(o))
    }, frp);
    vm.global_prop("FinalizationRegistry", Value::object(frc));
    vm.method(frp, "register", 2, |vm, this, a, _| {
        let o = registry(vm, this)?;
        let (t, held, token) = (arg(a, 0), arg(a, 1), arg(a, 2));
        if !can_be_held_weakly(vm, t) {
            return Err(vm.type_error("FinalizationRegistry.register: invalid target"));
        }
        if vm.same_value(t, held) {
            return Err(vm.type_error("FinalizationRegistry.register: target and holdings must not be the same"));
        }
        if !(token.is_undefined() || can_be_held_weakly(vm, token)) {
            return Err(vm.type_error("FinalizationRegistry.register: invalid unregister token"));
        }
        if let Class::FinalizationRegistry(r) = &mut vm.obj_mut(o).class {
            r.cells.push(RegistryCell { target: t, held, token });
        }
        Ok(Value::UNDEFINED)
    });
    vm.method(frp, "unregister", 1, |vm, this, a, _| {
        let o = registry(vm, this)?;
        let token = arg(a, 0);
        if !can_be_held_weakly(vm, token) {
            return Err(vm.type_error("FinalizationRegistry.unregister: invalid token"));
        }
        let mut removed = false;
        if let Class::FinalizationRegistry(r) = &mut vm.obj_mut(o).class {
            let before = r.cells.len();
            r.cells.retain(|c| c.token != token);
            removed = r.cells.len() != before;
        }
        Ok(Value::bool(removed))
    });
    vm.method(frp, "cleanupSome", 0, |vm, this, a, _| {
        registry(vm, this)?;
        let cb = arg(a, 0);
        if !cb.is_undefined() && !vm.is_callable(cb) {
            return Err(vm.type_error("cleanupSome: callback must be callable"));
        }
        Ok(Value::UNDEFINED)
    });
    let tag = vm.new_string("FinalizationRegistry");
    vm.define_data(frp, kt, tag, attr::CONFIGURABLE);
}

fn registry(vm: &mut Vm, this: Value) -> JsResult<ObjId> {
    match this.as_object() {
        Some(o) if matches!(vm.obj(o).class, Class::FinalizationRegistry(_)) => Ok(o),
        _ => Err(vm.type_error("FinalizationRegistry method called on incompatible receiver")),
    }
}

/// CanBeHeldWeakly: objects and symbols not from Symbol.for.
pub fn can_be_held_weakly(vm: &Vm, v: Value) -> bool {
    match v.kind() {
        Kind::Object(_) => true,
        Kind::Symbol(s) => !vm.symbols.get(s.0).registered,
        _ => false,
    }
}

/// Weak collections key by object id; symbols as keys are kept by id in
/// the high range (they are never collected while used as keys).
fn weak_key(vm: &mut Vm, v: Value) -> Option<u32> {
    if !can_be_held_weakly(vm, v) {
        return None;
    }
    match v.kind() {
        Kind::Object(o) => Some(o.0),
        Kind::Symbol(s) => {
            // Map the symbol to a stable holder object.
            let holder = match vm.symbol_holders.get(&s.0) {
                Some(&h) => h,
                None => {
                    let h = vm.alloc_object(None, Class::Internal(Box::new([v])));
                    vm.symbol_holders.insert(s.0, h);
                    h
                }
            };
            Some(holder.0)
        }
        _ => None,
    }
}

fn weak_data(vm: &mut Vm, this: Value, set: bool) -> JsResult<ObjId> {
    if let Some(o) = this.as_object() {
        match (&vm.obj(o).class, set) {
            (Class::WeakMap(_), false) | (Class::WeakSet(_), true) => return Ok(o),
            _ => {}
        }
    }
    Err(vm.type_error("WeakMap/WeakSet method called on incompatible receiver"))
}

fn collection_ctor(vm: &mut Vm, a: &[Value], nt: Value, set: bool) -> JsResult<Value> {
    if nt.is_undefined() {
        return Err(vm.type_error(if set { "Constructor Set requires 'new'" } else { "Constructor Map requires 'new'" }));
    }
    let p = if set { vm.proto_from_ctor(nt, |i| i.set_prototype)? } else { vm.proto_from_ctor(nt, |i| i.map_prototype)? };
    let o = vm.alloc_object(Some(p), if set { Class::Set(Box::default()) } else { Class::Map(Box::default()) });
    vm.root(Value::object(o));
    let it = arg(a, 0);
    if it.is_nullish() {
        return Ok(Value::object(o));
    }
    let adder_name = if set { "add" } else { "set" };
    let ka = vm.key(adder_name);
    let adder = vm.get(o, ka)?;
    if !vm.is_callable(adder) {
        return Err(vm.type_error("adder is not callable"));
    }
    vm.root(adder);
    add_entries(vm, o, it, adder, set)?;
    Ok(Value::object(o))
}

fn add_entries(vm: &mut Vm, o: ObjId, it: Value, adder: Value, set: bool) -> JsResult<()> {
    let (iter, next) = vm.get_iterator(it)?;
    vm.root(iter);
    vm.root(next);
    while let Some(v) = vm.iterator_step(iter, next)? {
        let r = if set {
            vm.call(adder, Value::object(o), &[v]).map(|_| ())
        } else {
            (|| {
                let Some(e) = v.as_object() else {
                    return Err(vm.type_error("Iterator value is not an entry object"));
                };
                let k = vm.get(e, Key::Index(0))?;
                let val = vm.get(e, Key::Index(1))?;
                vm.call(adder, Value::object(o), &[k, val]).map(|_| ())
            })()
        };
        if let Err(e) = r {
            let _ = vm.iterator_close(iter);
            return Err(e);
        }
    }
    Ok(())
}

fn weak_ctor(vm: &mut Vm, a: &[Value], nt: Value, set: bool) -> JsResult<Value> {
    if nt.is_undefined() {
        return Err(vm.type_error("Constructor requires 'new'"));
    }
    let p = if set { vm.proto_from_ctor(nt, |i| i.weakset_prototype)? } else { vm.proto_from_ctor(nt, |i| i.weakmap_prototype)? };
    let o = vm.alloc_object(Some(p), if set { Class::WeakSet(Box::default()) } else { Class::WeakMap(Box::default()) });
    vm.root(Value::object(o));
    let it = arg(a, 0);
    if it.is_nullish() {
        return Ok(Value::object(o));
    }
    let ka = vm.key(if set { "add" } else { "set" });
    let adder = vm.get(o, ka)?;
    if !vm.is_callable(adder) {
        return Err(vm.type_error("adder is not callable"));
    }
    vm.root(adder);
    add_entries(vm, o, it, adder, set)?;
    Ok(Value::object(o))
}

fn for_each(vm: &mut Vm, this: Value, a: &[Value], set: bool) -> JsResult<Value> {
    let o = map_data(vm, this, set)?;
    let f = arg(a, 0);
    if !vm.is_callable(f) {
        return Err(vm.type_error("forEach callback is not a function"));
    }
    let this_arg = arg(a, 1);
    let mut i = 0;
    loop {
        let e = {
            let t = table_ref(vm, o);
            if i >= t.entries.len() {
                break;
            }
            t.entries[i]
        };
        i += 1;
        if let Some((k, v)) = e {
            vm.call(f, this_arg, &[v, k, this])?;
        }
    }
    Ok(Value::UNDEFINED)
}

fn make_iter(vm: &mut Vm, this: Value, set: bool, kind: u8) -> JsResult<Value> {
    let o = map_data(vm, this, set)?;
    let p = if set { vm.intrinsics.set_iterator_prototype } else { vm.intrinsics.map_iterator_prototype };
    let st = super::iterators::IterState { kind: if set { 5 } else { 4 }, target: Value::object(o), pos: 0, extra: alloc::vec![Value::int(kind as i32)], done: false };
    Ok(Value::object(vm.alloc_object(Some(p), Class::Iter(Box::new(st)))))
}

fn iter_next(vm: &mut Vm, this: Value, set: bool) -> JsResult<Value> {
    let want = if set { 5 } else { 4 };
    let Some(it) = this.as_object() else {
        return Err(vm.type_error("next called on incompatible receiver"));
    };
    let (target, mut pos, kind, done) = match &vm.obj(it).class {
        Class::Iter(s) if s.kind == want => (s.target, s.pos, s.extra[0].as_int().unwrap(), s.done),
        _ => return Err(vm.type_error("next called on incompatible receiver")),
    };
    if done {
        return Ok(vm.iter_result(Value::UNDEFINED, true));
    }
    let o = target.as_object().unwrap();
    loop {
        let e = {
            let t = table_ref(vm, o);
            if pos >= t.entries.len() {
                None
            } else {
                Some(t.entries[pos])
            }
        };
        match e {
            None => {
                if let Class::Iter(s) = &mut vm.obj_mut(it).class {
                    s.done = true;
                    s.pos = pos;
                }
                return Ok(vm.iter_result(Value::UNDEFINED, true));
            }
            Some(None) => pos += 1,
            Some(Some((k, v))) => {
                pos += 1;
                if let Class::Iter(s) = &mut vm.obj_mut(it).class {
                    s.pos = pos;
                }
                let r = match kind {
                    0 => k,
                    1 => v,
                    _ => Value::object(vm.new_array(alloc::vec![k, v])),
                };
                return Ok(vm.iter_result(r, false));
            }
        }
    }
}

/// GetSetRecord(other): (size, has, keys).
fn set_record(vm: &mut Vm, other: Value) -> JsResult<(f64, Value, Value)> {
    let Some(o) = other.as_object() else {
        return Err(vm.type_error("Set operation argument must be an object"));
    };
    let ks = vm.key("size");
    let raw = vm.get(o, ks)?;
    let n = vm.to_number(raw)?;
    if n.is_nan() {
        return Err(vm.type_error("size is not a number"));
    }
    let size = vm.to_integer_or_infinity(Value::number(n))?;
    if size < 0.0 {
        return Err(vm.range_error("size must not be negative"));
    }
    let kh = vm.key("has");
    let has = vm.get(o, kh)?;
    if !vm.is_callable(has) {
        return Err(vm.type_error("has is not a function"));
    }
    let kk = vm.key("keys");
    let keys = vm.get(o, kk)?;
    if !vm.is_callable(keys) {
        return Err(vm.type_error("keys is not a function"));
    }
    Ok((size, has, keys))
}

fn this_set_values(vm: &Vm, o: ObjId) -> Vec<Value> {
    table_ref(vm, o).entries.iter().flatten().map(|e| e.0).collect()
}

/// union, intersection, difference, symmetricDifference, isSubsetOf,
/// isSupersetOf, isDisjointFrom.
fn set_algebra(vm: &mut Vm, this: Value, a: &[Value], _: Value) -> JsResult<Value> {
    let op = vm.native_data(0).as_int().unwrap();
    let o = map_data(vm, this, true)?;
    let other = arg(a, 0);
    let (size, has, keys) = set_record(vm, other)?;
    vm.root(has);
    vm.root(keys);
    let new_set = |vm: &mut Vm, vals: Vec<Value>| -> Value {
        let p = vm.intrinsics.set_prototype;
        let r = vm.alloc_object(Some(p), Class::Set(Box::default()));
        for v in vals {
            tbl_insert(vm, r, v, v);
        }
        Value::object(r)
    };
    let other_keys = |vm: &mut Vm| -> JsResult<Vec<Value>> {
        let it = vm.call(keys, other, &[])?;
        if !it.is_object() {
            return Err(vm.type_error("keys() result is not an object"));
        }
        vm.root(it);
        let kn = Key::Str(vm.names.next);
        let next = vm.get_v(it, kn)?;
        vm.root(next);
        let mut out = Vec::new();
        while let Some(v) = vm.iterator_step(it, next)? {
            let v = normalize(v);
            vm.root(v);
            out.push(v);
        }
        Ok(out)
    };
    let in_other = |vm: &mut Vm, v: Value| -> JsResult<bool> {
        let r = vm.call(has, other, &[v])?;
        Ok(vm.to_boolean(r))
    };
    let this_size = table_ref(vm, o).live as f64;
    match op {
        0 => {
            let mut vals = this_set_values(vm, o);
            for v in other_keys(vm)? {
                if !vals.iter().any(|x| vm.same_value_zero(*x, v)) {
                    vals.push(v);
                }
            }
            Ok(new_set(vm, vals))
        }
        1 => {
            let mut out: Vec<Value> = Vec::new();
            if this_size <= size {
                let mut i = 0;
                loop {
                    let e = {
                        let t = table_ref(vm, o);
                        if i >= t.entries.len() {
                            break;
                        }
                        t.entries[i]
                    };
                    i += 1;
                    if let Some((v, _)) = e {
                        if in_other(vm, v)? && !out.iter().any(|x| vm.same_value_zero(*x, v)) {
                            out.push(v);
                        }
                    }
                }
            } else {
                for v in other_keys(vm)? {
                    if tbl_find(vm, o, v).is_some() && !out.iter().any(|x| vm.same_value_zero(*x, v)) {
                        out.push(v);
                    }
                }
            }
            Ok(new_set(vm, out))
        }
        2 => {
            let mut vals = this_set_values(vm, o);
            if this_size <= size {
                let mut keep = Vec::new();
                for v in vals {
                    if !in_other(vm, v)? {
                        keep.push(v);
                    }
                }
                vals = keep;
            } else {
                for v in other_keys(vm)? {
                    vals.retain(|x| !vm.same_value_zero(*x, v));
                }
            }
            Ok(new_set(vm, vals))
        }
        3 => {
            let mut vals = this_set_values(vm, o);
            let original = vals.clone();
            for v in other_keys(vm)? {
                let in_this = original.iter().any(|x| vm.same_value_zero(*x, v));
                if in_this {
                    vals.retain(|x| !vm.same_value_zero(*x, v));
                } else if !vals.iter().any(|x| vm.same_value_zero(*x, v)) {
                    vals.push(v);
                }
            }
            Ok(new_set(vm, vals))
        }
        4 => {
            if this_size > size {
                return Ok(Value::FALSE);
            }
            for v in this_set_values(vm, o) {
                if !in_other(vm, v)? {
                    return Ok(Value::FALSE);
                }
            }
            Ok(Value::TRUE)
        }
        5 => {
            if this_size < size {
                return Ok(Value::FALSE);
            }
            let it = vm.call(keys, other, &[])?;
            if !it.is_object() {
                return Err(vm.type_error("keys() result is not an object"));
            }
            vm.root(it);
            let kn = Key::Str(vm.names.next);
            let next = vm.get_v(it, kn)?;
            while let Some(v) = vm.iterator_step(it, next)? {
                if tbl_find(vm, o, v).is_none() {
                    vm.iterator_close(it)?;
                    return Ok(Value::FALSE);
                }
            }
            Ok(Value::TRUE)
        }
        _ => {
            if this_size <= size {
                for v in this_set_values(vm, o) {
                    if in_other(vm, v)? {
                        return Ok(Value::FALSE);
                    }
                }
            } else {
                let it = vm.call(keys, other, &[])?;
                if !it.is_object() {
                    return Err(vm.type_error("keys() result is not an object"));
                }
                vm.root(it);
                let kn = Key::Str(vm.names.next);
                let next = vm.get_v(it, kn)?;
                while let Some(v) = vm.iterator_step(it, next)? {
                    if tbl_find(vm, o, v).is_some() {
                        vm.iterator_close(it)?;
                        return Ok(Value::FALSE);
                    }
                }
            }
            Ok(Value::TRUE)
        }
    }
}
