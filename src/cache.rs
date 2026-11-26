//! Bytecode cache (plan §3.6): a compiled script serialized to bytes, so
//! a later run skips parsing and compiling. The bytes carry the source
//! (for `Function.prototype.toString` and error positions), a format
//! version and a hash of the source text.
//!
//! ```text
//! let bytes = vm.compile_to_cache(src, "prelude.js")?;   // at build time
//! vm.eval_cached(&bytes)?;                                // per page
//! ```
//!
//! Entries are trusted input (made by the embedder's own build): the
//! checks catch stale or truncated entries, not crafted ones.

use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec::Vec;
use core::cell::Cell;

use crate::bytecode::{Body, Code, CodeHeader, Const, DebugScope, EvalScope, GlobalDecls, Handler, Op, Source};
use crate::op_codec::*;
use crate::string::hash_units;
use crate::value::{Kind, Value};
use crate::vm::{JsResult, Vm};

const MAGIC: &[u8; 4] = b"RJBC";
/// Changes whenever the bytecode or this format changes.
pub const VERSION: u32 = 1;

// ----- byte streams ---------------------------------------------------------

#[derive(Default)]
pub struct Writer {
    pub out: Vec<u8>,
}

impl Writer {
    pub fn u8(&mut self, v: u8) {
        self.out.push(v);
    }
    pub fn bool(&mut self, v: bool) {
        self.out.push(v as u8);
    }
    /// LEB128.
    pub fn u32(&mut self, mut v: u32) {
        loop {
            let b = (v & 0x7f) as u8;
            v >>= 7;
            if v == 0 {
                self.out.push(b);
                return;
            }
            self.out.push(b | 0x80);
        }
    }
    pub fn u16(&mut self, v: u16) {
        self.u32(v as u32);
    }
    pub fn i32(&mut self, v: i32) {
        self.u32(((v << 1) ^ (v >> 31)) as u32);
    }
    pub fn u64(&mut self, v: u64) {
        self.out.extend_from_slice(&v.to_le_bytes());
    }
    pub fn bin_kind(&mut self, k: crate::bytecode::BinKind) {
        self.u8(bin_kind_to_u8(k));
    }
    pub fn bytes(&mut self, b: &[u8]) {
        self.u32(b.len() as u32);
        self.out.extend_from_slice(b);
    }
    pub fn str(&mut self, s: &str) {
        self.bytes(s.as_bytes());
    }
}

pub struct Reader<'a> {
    pub data: &'a [u8],
    pub pos: usize,
}

impl<'a> Reader<'a> {
    pub fn u8(&mut self) -> Option<u8> {
        let b = *self.data.get(self.pos)?;
        self.pos += 1;
        Some(b)
    }
    pub fn bool(&mut self) -> Option<bool> {
        Some(self.u8()? != 0)
    }
    pub fn u32(&mut self) -> Option<u32> {
        let mut v: u32 = 0;
        for shift in (0..35).step_by(7) {
            let b = self.u8()?;
            v |= ((b & 0x7f) as u32).checked_shl(shift)?;
            if b & 0x80 == 0 {
                return Some(v);
            }
        }
        None
    }
    pub fn u16(&mut self) -> Option<u16> {
        u16::try_from(self.u32()?).ok()
    }
    pub fn i32(&mut self) -> Option<i32> {
        let z = self.u32()?;
        Some(((z >> 1) as i32) ^ -((z & 1) as i32))
    }
    pub fn u64(&mut self) -> Option<u64> {
        let b = self.data.get(self.pos..self.pos + 8)?;
        self.pos += 8;
        Some(u64::from_le_bytes(b.try_into().ok()?))
    }
    pub fn bin_kind(&mut self) -> Option<crate::bytecode::BinKind> {
        bin_kind_from_u8(self.u8()?)
    }
    pub fn bytes(&mut self) -> Option<&'a [u8]> {
        let n = self.u32()? as usize;
        let b = self.data.get(self.pos..self.pos.checked_add(n)?)?;
        self.pos += n;
        Some(b)
    }
    pub fn str(&mut self) -> Option<String> {
        String::from_utf8(self.bytes()?.to_vec()).ok()
    }
    fn len(&mut self) -> Option<usize> {
        let n = self.u32()? as usize;
        // Every element takes at least a byte: reject absurd lengths.
        if n > self.data.len() - self.pos { None } else { Some(n) }
    }
}

// ----- encoding -------------------------------------------------------------

struct Enc {
    w: Writer,
    /// Strings by first use: StrId -> table index.
    table: alloc::collections::BTreeMap<crate::value::StrId, u32>,
    strings: Vec<crate::value::StrId>,
}

impl Enc {
    fn value(&mut self, v: Value) -> Option<()> {
        match v.kind() {
            Kind::Undefined => self.w.u8(0),
            Kind::Null => self.w.u8(1),
            Kind::Bool(b) => self.w.u8(2 + b as u8),
            Kind::Int(i) => {
                self.w.u8(4);
                self.w.i32(i);
            }
            Kind::Double(d) => {
                self.w.u8(5);
                self.w.u64(d.to_bits());
            }
            Kind::Str(s) => {
                let n = self.strings.len() as u32;
                let i = *self.table.entry(s).or_insert(n);
                if i == n {
                    self.strings.push(s);
                }
                self.w.u8(6);
                self.w.u32(i);
            }
            Kind::Hole => self.w.u8(8),
            Kind::Uninit => self.w.u8(9),
            // Objects, symbols and bigints never appear in compiled code.
            _ => return None,
        }
        Some(())
    }

    fn values(&mut self, vs: &[Value]) -> Option<()> {
        self.w.u32(vs.len() as u32);
        for v in vs {
            self.value(*v)?;
        }
        Some(())
    }

    /// A code unit: its header, then its body as a length-prefixed block
    /// (so loading can skip bodies until they are needed).
    fn code(&mut self, c: &Code) -> Option<()> {
        let w = &mut self.w;
        w.u16(c.nregs);
        w.u16(c.nparams);
        w.u16(c.length);
        self.value(c.name)?;
        let w = &mut self.w;
        w.u8(func_kind_to_u8(c.kind));
        w.u8(c.strict as u8 | (c.is_async as u8) << 1 | (c.is_generator as u8) << 2 | (c.keep_args as u8) << 3 | (c.has_fields as u8) << 4);
        w.u32(c.src_start);
        w.u32(c.src_end);
        let outer = core::mem::take(&mut self.w);
        self.body(c)?;
        let block = core::mem::replace(&mut self.w, outer);
        self.w.bytes(&block.out);
        Some(())
    }

    fn body(&mut self, c: &Code) -> Option<()> {
        let b = c.body();
        let w = &mut self.w;
        w.u32(b.ops.len() as u32);
        for pc in 0..b.ops.len() {
            put_op(w, c.op_at(pc));
        }
        w.u32(b.consts.len() as u32);
        for k in &b.consts {
            match k {
                Const::Value(v) => {
                    self.w.u8(0);
                    self.value(*v)?;
                }
                Const::BigInt(s) => {
                    self.w.u8(1);
                    self.w.str(s);
                }
                Const::Names(ns) => {
                    self.w.u8(2);
                    self.values(ns)?;
                }
                Const::Globals(g) => {
                    self.w.u8(3);
                    self.values(&g.vars)?;
                    self.w.u32(g.functions.len() as u32);
                    for (n, i) in &g.functions {
                        self.value(*n)?;
                        self.w.u32(*i);
                    }
                    self.w.u32(g.lexicals.len() as u32);
                    for (n, c) in &g.lexicals {
                        self.value(*n)?;
                        self.w.bool(*c);
                    }
                    self.w.bool(g.eval);
                    self.w.bool(g.strict);
                    opt_u16(&mut self.w, g.var_env_hops);
                }
                Const::EvalScope(e) => {
                    self.w.u8(4);
                    self.w.u32(e.levels.len() as u32);
                    for l in &e.levels {
                        match l {
                            None => self.w.u8(0),
                            Some(names) => {
                                self.w.u8(1);
                                self.w.u32(names.len() as u32);
                                for (n, slot, kind) in names.iter() {
                                    self.w.str(n);
                                    self.w.u16(*slot);
                                    self.w.u8(binding_kind_to_u8(*kind));
                                }
                            }
                        }
                    }
                    for b in [e.strict, e.in_function, e.this_mode_lexical, e.new_target, e.super_prop, e.super_call, e.in_class_field] {
                        self.w.bool(b);
                    }
                    opt_u16(&mut self.w, e.var_scope_level);
                }
                Const::Template(t) => {
                    self.w.u8(5);
                    self.w.u32(t.len() as u32);
                    for (a, b) in t.iter() {
                        self.value(*a)?;
                        self.value(*b)?;
                    }
                }
            }
        }
        self.w.u32(b.children.len() as u32);
        for ch in &b.children {
            self.code(ch)?;
        }
        let w = &mut self.w;
        w.u32(b.handlers.len() as u32);
        for h in &b.handlers {
            w.u32(h.start);
            w.u32(h.end);
            w.u32(h.target);
            w.u16(h.reg);
            w.u16(h.env_depth);
        }
        w.u32(b.lines.len() as u32);
        for &(pc, l, col) in &b.lines {
            w.u32(pc);
            w.u32(l);
            w.u32(col);
        }
        w.u32(b.scopes.len() as u32);
        for s in b.scopes.iter() {
            w.u32(s.start);
            w.u32(s.end);
            match s.env_at {
                Some(p) => {
                    w.u8(1);
                    w.u32(p);
                }
                None => w.u8(0),
            }
            w.bool(s.function);
            for list in [&s.regs, &s.slots] {
                w.u32(list.len() as u32);
                for (n, r) in list {
                    w.str(n);
                    w.u16(*r);
                }
            }
        }
        Some(())
    }
}

fn opt_u16(w: &mut Writer, v: Option<u16>) {
    match v {
        Some(x) => {
            w.u8(1);
            w.u16(x);
        }
        None => w.u8(0),
    }
}

// ----- decoding -------------------------------------------------------------

/// A code body not decoded yet: where it is in the cache entry.
pub struct LazyBody {
    data: Rc<[u8]>,
    start: usize,
    end: usize,
    strings: Rc<[Value]>,
    source: Rc<Source>,
}

struct Dec<'a> {
    r: Reader<'a>,
    data: &'a Rc<[u8]>,
    strings: &'a Rc<[Value]>,
    source: &'a Rc<Source>,
}

impl Dec<'_> {
    fn value(&mut self) -> Option<Value> {
        Some(match self.r.u8()? {
            0 => Value::UNDEFINED,
            1 => Value::NULL,
            2 => Value::FALSE,
            3 => Value::TRUE,
            4 => Value::int(self.r.i32()?),
            5 => Value::number(f64::from_bits(self.r.u64()?)),
            6 => *self.strings.get(self.r.u32()? as usize)?,
            8 => Value::HOLE,
            9 => Value::UNINIT,
            _ => return None,
        })
    }

    fn values(&mut self) -> Option<Vec<Value>> {
        let n = self.r.len()?;
        (0..n).map(|_| self.value()).collect()
    }

    fn opt_u16(&mut self) -> Option<Option<u16>> {
        Some(match self.r.u8()? {
            0 => None,
            _ => Some(self.r.u16()?),
        })
    }

    /// A code unit's header; its body stays encoded.
    fn code(&mut self) -> Option<Rc<Code>> {
        let (nregs, nparams, length) = (self.r.u16()?, self.r.u16()?, self.r.u16()?);
        let name = self.value()?;
        let kind = func_kind_from_u8(self.r.u8()?)?;
        let flags = self.r.u8()?;
        let (src_start, src_end) = (self.r.u32()?, self.r.u32()?);
        let n = self.r.len()?;
        let start = self.r.pos;
        self.r.pos += n;
        let header = CodeHeader {
            nregs,
            nparams,
            length,
            name,
            kind,
            strict: flags & 1 != 0,
            is_async: flags & 2 != 0,
            is_generator: flags & 4 != 0,
            keep_args: flags & 8 != 0,
            has_fields: flags & 16 != 0,
            source: self.source.clone(),
            src_start,
            src_end,
        };
        let lazy = LazyBody { data: self.data.clone(), start, end: start + n, strings: self.strings.clone(), source: self.source.clone() };
        Some(Rc::new(Code::new_lazy(header, lazy)))
    }

    fn body(&mut self) -> Option<Body> {
        let n = self.r.len()?;
        let mut ops = Vec::with_capacity(n);
        for _ in 0..n {
            let op = get_op(&mut self.r)?;
            if let Op::Break(_) = op {
                return None;
            }
            ops.push(Cell::new(op));
        }
        let n = self.r.len()?;
        let mut consts = Vec::with_capacity(n);
        for _ in 0..n {
            consts.push(match self.r.u8()? {
                0 => Const::Value(self.value()?),
                1 => Const::BigInt(self.r.str()?.into()),
                2 => Const::Names(self.values()?.into()),
                3 => {
                    let vars = self.values()?;
                    let mut functions = Vec::new();
                    for _ in 0..self.r.len()? {
                        functions.push((self.value()?, self.r.u32()?));
                    }
                    let mut lexicals = Vec::new();
                    for _ in 0..self.r.len()? {
                        lexicals.push((self.value()?, self.r.bool()?));
                    }
                    let (eval, strict) = (self.r.bool()?, self.r.bool()?);
                    let var_env_hops = self.opt_u16()?;
                    Const::Globals(Rc::new(GlobalDecls { vars, functions, lexicals, eval, strict, var_env_hops }))
                }
                4 => {
                    let mut levels = Vec::new();
                    for _ in 0..self.r.len()? {
                        levels.push(match self.r.u8()? {
                            0 => None,
                            _ => {
                                let mut names = Vec::new();
                                for _ in 0..self.r.len()? {
                                    names.push((self.r.str()?, self.r.u16()?, binding_kind_from_u8(self.r.u8()?)?));
                                }
                                Some(names.into())
                            }
                        });
                    }
                    let mut b = [false; 7];
                    for x in b.iter_mut() {
                        *x = self.r.bool()?;
                    }
                    let var_scope_level = self.opt_u16()?;
                    Const::EvalScope(Rc::new(EvalScope {
                        levels,
                        strict: b[0],
                        in_function: b[1],
                        this_mode_lexical: b[2],
                        new_target: b[3],
                        super_prop: b[4],
                        super_call: b[5],
                        in_class_field: b[6],
                        var_scope_level,
                    }))
                }
                5 => {
                    let mut t = Vec::new();
                    for _ in 0..self.r.len()? {
                        t.push((self.value()?, self.value()?));
                    }
                    Const::Template(t.into())
                }
                _ => return None,
            });
        }
        let mut children = Vec::new();
        for _ in 0..self.r.len()? {
            children.push(self.code()?);
        }
        let mut handlers = Vec::new();
        for _ in 0..self.r.len()? {
            handlers.push(Handler { start: self.r.u32()?, end: self.r.u32()?, target: self.r.u32()?, reg: self.r.u16()?, env_depth: self.r.u16()? });
        }
        let mut lines = Vec::new();
        for _ in 0..self.r.len()? {
            lines.push((self.r.u32()?, self.r.u32()?, self.r.u32()?));
        }
        let mut scopes = Vec::new();
        for _ in 0..self.r.len()? {
            let (start, end) = (self.r.u32()?, self.r.u32()?);
            let env_at = match self.r.u8()? {
                0 => None,
                _ => Some(self.r.u32()?),
            };
            let function = self.r.bool()?;
            let mut lists: [Vec<(Rc<str>, u16)>; 2] = Default::default();
            for list in lists.iter_mut() {
                for _ in 0..self.r.len()? {
                    list.push((self.r.str()?.into(), self.r.u16()?));
                }
            }
            let [regs, slots] = lists;
            scopes.push(DebugScope { start, end, env_at, function, regs, slots });
        }
        let ics = crate::compiler::assign_ics(&mut ops, consts.len());
        Some(Body {
            ops: ops.into_boxed_slice(),
            consts,
            children,
            handlers,
            lines,
            ics,
            patches: Default::default(),
            scopes: scopes.into_boxed_slice(),
        })
    }
}

/// Decodes a body left encoded by `load_cached`.
pub fn decode_body(l: LazyBody) -> Option<Body> {
    let mut d = Dec { r: Reader { data: l.data.get(..l.end)?, pos: l.start }, data: &l.data, strings: &l.strings, source: &l.source };
    let b = d.body()?;
    if d.r.pos == l.end { Some(b) } else { None }
}

impl Vm {
    /// Compiles a script and returns its bytecode cache entry.
    pub fn compile_to_cache(&mut self, src: &str, name: &str) -> JsResult<Vec<u8>> {
        let code = self.compile_script(src, name)?;
        let mut e = Enc { w: Writer::default(), table: Default::default(), strings: Vec::new() };
        if e.code(&code).is_none() {
            return Err(self.type_error("script cannot be cached"));
        }
        let mut w = Writer::default();
        w.out.extend_from_slice(MAGIC);
        w.u32(VERSION);
        w.u32(hash_units(src.encode_utf16()));
        w.str(name);
        w.str(src);
        // The string table: each string once (atoms on load).
        w.u32(e.strings.len() as u32);
        for &id in &e.strings {
            let s = self.str(id);
            if s.is_latin1() {
                w.u8(0);
                w.u32(s.len() as u32);
                w.out.extend(s.units().map(|u| u as u8));
            } else {
                w.u8(1);
                w.u32(s.len() as u32);
                for u in s.units() {
                    w.out.extend_from_slice(&u.to_le_bytes());
                }
            }
        }
        w.out.extend_from_slice(&e.w.out);
        Ok(w.out)
    }

    /// The source text stored in an entry (for an embedder to check that
    /// the entry matches its current script without hashing it).
    pub fn cache_source(bytes: &[u8]) -> Option<&str> {
        if !bytes.starts_with(MAGIC) {
            return None;
        }
        let mut r = Reader { data: bytes, pos: 4 };
        if r.u32()? != VERSION {
            return None;
        }
        let _hash = r.u32()?;
        let _name = r.bytes()?;
        core::str::from_utf8(r.bytes()?).ok()
    }

    /// The entry was made from exactly this source text.
    pub fn cache_matches(bytes: &[u8], src: &str) -> bool {
        let mut r = Reader { data: bytes, pos: 4 };
        bytes.starts_with(MAGIC) && r.u32() == Some(VERSION) && r.u32() == Some(hash_units(src.encode_utf16()))
    }

    /// Loads a cache entry made by `compile_to_cache` (same engine
    /// version) without parsing or compiling. None if the bytes are not a
    /// valid entry for this version.
    pub fn load_cached(&mut self, bytes: &[u8]) -> Option<Rc<Code>> {
        if bytes.get(..4)? != MAGIC {
            return None;
        }
        let mut r = Reader { data: bytes, pos: 4 };
        if r.u32()? != VERSION {
            return None;
        }
        let _hash = r.u32()?; // see `cache_matches`
        let name = r.str()?;
        let src = r.str()?;
        let source = self.add_source(&name, src.into());
        let n = r.len()?;
        let mut strings = Vec::with_capacity(n);
        let mut units: Vec<u16> = Vec::new();
        for _ in 0..n {
            let wide = r.u8()? != 0;
            let len = r.len()?;
            let b = r.data.get(r.pos..r.pos.checked_add(if wide { len.checked_mul(2)? } else { len })?)?;
            units.clear();
            if wide {
                r.pos += len * 2;
                units.extend(b.chunks(2).map(|p| u16::from_le_bytes([p[0], p[1]])));
            } else {
                r.pos += len;
                units.extend(b.iter().map(|&c| c as u16));
            }
            strings.push(Value::str(self.intern_units(&units)));
        }
        // Function bodies decode on first use, from a shared copy of the entry.
        let data: Rc<[u8]> = bytes.into();
        let strings: Rc<[Value]> = strings.into();
        let mut d = Dec { r: Reader { data: &data, pos: r.pos }, data: &data, strings: &strings, source: &source };
        let code = d.code()?;
        if d.r.pos != bytes.len() {
            return None;
        }
        crate::debug::code_compiled(self, &code);
        Some(code)
    }

    /// Runs a cached script (see `load_cached`).
    pub fn eval_cached(&mut self, bytes: &[u8]) -> JsResult<Value> {
        match self.load_cached(bytes) {
            Some(code) => self.run_script_code(code),
            None => Err(self.type_error("invalid bytecode cache entry")),
        }
    }
}
