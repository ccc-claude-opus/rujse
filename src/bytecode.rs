//! Bytecode: a register machine with an accumulator (like V8's Ignition).
//! Most instructions read or write the accumulator; binary operators take
//! the left operand from a register (`acc = r OP acc`).

use alloc::rc::Rc;
use alloc::vec::Vec;
use core::cell::Cell;

use crate::value::Value;

pub type Reg = u16;
/// Index into `Code::consts`.
pub type K = u32;
/// Absolute instruction index.
pub type Label = u32;

/// Binary operators of the fused register instructions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BinKind {
    Add,
    Sub,
    Mul,
    Lt,
    Le,
    Gt,
    Ge,
    StrictEq,
    StrictNe,
    BitAnd,
    BitOr,
    BitXor,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Op {
    Nop,
    /// dst = acc = lhs OP rhs (registers).
    BinRR { kind: BinKind, dst: Reg, lhs: Reg, rhs: Reg },
    /// acc = lhs CMP rhs; jumps if acc == when.
    CmpJmp { kind: BinKind, lhs: Reg, rhs: Reg, target: Label, when: bool },
    /// r = acc = r + 1 / r - 1 (with ToNumeric).
    IncReg(Reg),
    DecReg(Reg),
    // ----- accumulator / registers
    LdaUndef,
    LdaNull,
    LdaTrue,
    LdaFalse,
    LdaHole,
    LdaUninit,
    LdaInt(i32),
    LdaConst(K),
    Ldar(Reg),
    Star(Reg),
    Mov(Reg, Reg),
    /// `this` (ReferenceError in a derived constructor before `super()`).
    LdaThis,
    LdaNewTarget,
    /// The running function object.
    LdaCallee,
    // ----- environments (closures)
    /// Pushes a declarative environment with `n` slots (all uninitialized
    /// when `tdz`, else undefined). `names` is a const index of the slot
    /// names (`u32::MAX` if none): needed for `with`/`eval` lookups.
    PushEnv { n: u16, names: K },
    PopEnv,
    /// Replaces the current environment by a copy (per-iteration `let`).
    CloneEnv,
    LdaEnv(u16, u16),
    StaEnv(u16, u16),
    /// Reads a binding that may be in its TDZ; `K` is its name.
    LdaEnvChecked(u16, u16, K),
    /// Assigns to a `let` binding that may be in its TDZ.
    StaEnvChecked(u16, u16, K),
    /// acc = an imported binding: (hops to the module environment, index
    /// in its import table); ReferenceError while uninitialized.
    LdaImport(u16, u16),
    /// Register binding TDZ check: ReferenceError if `r` is uninitialized.
    CheckInit(Reg, K),
    /// Throws TypeError "assignment to constant variable" (name `K`).
    ThrowConstAssign(K),
    // ----- globals and dynamic names
    LdaGlobal(K),
    /// `typeof x` for an unresolved name: no ReferenceError.
    LdaGlobalOrUndef(K),
    /// Assigns a global; `strict` throws on an unresolvable name.
    StaGlobal(K, bool),
    /// Creates a global lexical binding (script top-level let/const/class).
    InitGlobalLex(K),
    /// Script/eval prologue: declares the vars and functions listed in
    /// const `K` (GlobalDeclarationInstantiation).
    DeclareGlobals(K),
    /// Name lookup through `with` objects, eval-created vars and globals.
    LdaName(K),
    LdaNameOrUndef(K),
    StaName(K, bool),
    /// `delete name`.
    DeleteName(K),
    /// `name(…)` inside `with`: loads the function into acc and the `this`
    /// (the with object or undefined) into `r`.
    LdaNameWithThis(K, Reg),
    /// Declares a `var` created by sloppy direct eval in a function.
    DeclareEvalVar(K),
    // ----- properties
    GetNamed(Reg, K),
    SetNamed(Reg, K),
    /// `acc = r[acc]`.
    GetKeyed(Reg),
    /// `obj[key] = acc` (obj, key).
    SetKeyed(Reg, Reg),
    DeleteNamed(Reg, K),
    /// `delete r[acc]`.
    DeleteKeyed(Reg),
    /// `acc = acc in r`.
    In(Reg),
    /// acc = super.K, with acc = `this` on entry.
    GetSuperNamed(K),
    /// acc = super[acc] with `this` in r.
    GetSuperKeyed(Reg),
    /// super.K = acc with `this` in r (acc kept).
    SetSuperNamed(K, Reg),
    /// super[key] = acc: (this, key); acc kept.
    SetSuperKeyed(Reg, Reg),
    /// Private name access: `r.#x` where acc holds the private key (from
    /// the class environment).
    GetPrivate(Reg),
    /// `obj.#x = value`: (obj, key), value in acc.
    SetPrivate(Reg, Reg),
    /// `#x in obj`: acc = obj, r = key.
    HasPrivate(Reg),
    // ----- literals and definitions
    NewObject,
    NewArray(u32),
    /// Appends acc to the array in `r`.
    ArrayPush(Reg),
    /// Appends every element of the iterable in acc to the array in `r`.
    ArraySpread(Reg),
    /// Appends a hole to the array in `r`.
    ArrayHole(Reg),
    /// Defines data property `K` of `r` = acc (object literal).
    DefineNamed(Reg, K),
    /// Defines data property `r2` of `r1` = acc.
    DefineKeyed(Reg, Reg),
    /// Defines a getter/setter: (object, key reg), function in acc.
    DefineGetter(Reg, Reg),
    DefineSetter(Reg, Reg),
    /// Object literal method/class member: sets `[[HomeObject]]` of the
    /// function in acc to `r1` and defines it as `r2` (non-enumerable when
    /// `class`), naming it from the key.
    DefineMethod { obj: Reg, key: Reg, kind: u8, enumerable: bool },
    /// `{...acc}` into `r`.
    CopyDataProps(Reg),
    /// Object rest: copies acc's own enumerable props into the new object
    /// `r` except the keys in registers `from..from+n`.
    CopyDataPropsExcept { target: Reg, from: Reg, n: u16 },
    /// `__proto__: acc` in an object literal.
    SetProtoLiteral(Reg),
    NewRegExp(K, K),
    /// Creates a closure for child code `u32` in the current environment.
    Closure(u32),
    /// Sets the `name` of the function in acc from the key in `r`
    /// (NamedEvaluation for computed keys); `u8` = prefix (0 none, 1
    /// "get ", 2 "set ").
    SetFunctionName(Reg, u8),
    TemplateObject(u32),
    // ----- classes
    /// acc = superclass (or hole for no `extends`); creates the
    /// prototype in r and the constructor (child code `u32`) in acc.
    CreateClass { code: u32, proto: Reg, name: K },
    /// Registers the field initializer function in acc on the constructor
    /// in `r` (run on each new instance).
    SetClassFields(Reg),
    /// Creates a fresh private name (a private symbol) described by `K`.
    NewPrivateName(K),
    /// Defines private method/accessor `key` (reg) on the class's
    /// instance list or static object: (ctor reg, key reg, kind, static).
    AddPrivateMethod { ctor: Reg, key: Reg, kind: u8, is_static: bool },
    /// Defines private field `r2` = acc on object `r1`.
    DefinePrivate(Reg, Reg),
    /// Defines a public class field (CreateDataPropertyOrThrow): (obj, key).
    DefineField(Reg, Reg),
    /// Sets `[[HomeObject]]` of the function in acc to the object in r.
    SetHome(Reg),
    /// Runs the field initializers of the constructor in the register
    /// just below the current temporaries… see `InitFieldsOf`.
    InitFields,
    /// acc = this; runs the instance field initializers and private
    /// methods of constructor `r` on it.
    InitFieldsOf(Reg),
    /// `super(...)` in a default derived constructor: forwards the
    /// arguments of the current call (constructor in r, new.target r+1).
    SuperCallForward(Reg),
    // ----- operators: acc = r OP acc
    Add(Reg),
    Sub(Reg),
    Mul(Reg),
    Div(Reg),
    Mod(Reg),
    Exp(Reg),
    BitAnd(Reg),
    BitOr(Reg),
    BitXor(Reg),
    Shl(Reg),
    Shr(Reg),
    Ushr(Reg),
    Eq(Reg),
    Ne(Reg),
    StrictEq(Reg),
    StrictNe(Reg),
    Lt(Reg),
    Le(Reg),
    Gt(Reg),
    Ge(Reg),
    InstanceOf(Reg),
    /// acc = acc + int (fast path for `x + 1`, loop counters).
    AddInt(i32),
    Inc,
    Dec,
    Neg,
    BitNot,
    Not,
    Typeof,
    ToNumeric,
    /// ToNumber (unary `+`: BigInt throws).
    ToNumber,
    /// ToPropertyKey (computed keys are evaluated once).
    ToPropertyKey,
    /// ToString (template literals).
    ToStringOp,
    /// TypeError unless acc is an object (`K` = message).
    CheckObject(K),
    /// TypeError if acc is null/undefined (destructuring).
    RequireObjectCoercible,
    // ----- control flow
    Jmp(Label),
    JmpTrue(Label),
    JmpFalse(Label),
    JmpNullish(Label),
    JmpNotNullish(Label),
    JmpUndefined(Label),
    JmpNotUndefined(Label),
    /// Jumps if acc === r (switch cases).
    JmpStrictEq(Reg, Label),
    /// Jumps if r == int (finally dispatch).
    JmpIfIntEq(Reg, i32, Label),
    // ----- calls: callee in r, this in r+1, arguments in r+2…
    Call(Reg, u16),
    /// Arguments are the elements of the array in r+2.
    CallSpread(Reg),
    /// `new`: constructor in r, arguments in r+2… (r+1 unused).
    New(Reg, u16),
    NewSpread(Reg),
    SuperCall(Reg, u16),
    SuperCallSpread(Reg),
    /// Direct `eval(...)`: like Call, but evaluates in this scope.
    /// `u32` = const index of the scope description.
    CallEval(Reg, u16, K),
    Return,
    Throw,
    /// Throws a new error: kind 0 TypeError, 1 ReferenceError,
    /// 2 SyntaxError, 3 RangeError; message const `K`.
    ThrowError(u8, K),
    // ----- iteration
    /// acc = iterable; stores the iterator in r and its next method in r+1.
    GetIterator(Reg),
    GetAsyncIterator(Reg),
    /// Calls the iterator in (r, r+1); acc = the result object.
    IteratorNextRaw(Reg),
    /// acc = result.done of the result in acc / result.value.
    IterResultDone,
    IterResultValue,
    /// Steps the iterator: if done, jumps to the label (and marks r+2 done);
    /// else acc = value.
    IteratorStep(Reg, Label),
    /// IteratorClose on normal completion (calls `return`, checks result).
    IteratorClose(Reg),
    /// IteratorClose on a throw completion: calls `return`, ignores its
    /// errors; the original exception is in r2 and is rethrown after.
    IteratorCloseThrow(Reg, Reg),
    /// Collects the remaining values of iterator r into an array in acc.
    IteratorRest(Reg),
    /// acc = object; creates a for-in key iterator in r.
    ForInPrepare(Reg),
    /// acc = next key of the for-in iterator r, or jumps when done.
    ForInNext(Reg, Label),
    // ----- generators and async
    /// Generator start: suspends right away (the call returns the
    /// generator object).
    GeneratorStart,
    /// Suspends with acc as the value. On resume acc = the sent value and
    /// r = resume kind (0 next, 1 throw, 2 return).
    Yield(Reg),
    /// Suspends until the promise for acc settles; on resume acc = the
    /// value and r = 0 (fulfilled) or 1 (rejected).
    Await(Reg),
    /// Async generator yield (value already awaited).
    AsyncGenYield(Reg),
    /// Sync generator: yields the iterator result object in acc as is
    /// (`yield*`).
    YieldRaw(Reg),
    /// Wraps acc into an iterator result `{ value: acc, done: r }` with
    /// done from the register (bool).
    CreateIterResult(Reg),
    // ----- misc
    Debugger,
    /// `arguments` object of the current function into acc; `K` is the
    /// const list mapping parameters to env slots (mapped arguments) or
    /// `u32::MAX` (unmapped).
    CreateArguments(K),
    /// Rest parameter array from argument index `u16`.
    CreateRest(u16),
    /// Dynamic `import(acc)` (options in r).
    ImportCall(Reg),
    ImportMeta,
    /// Pushes a `with` environment for the object in acc.
    PushWith,
    /// Pops environments down to the given depth (break/continue out of
    /// blocks).
    PopEnvTo(u16),
}

/// An exception handler: a throw at `pc` in `start..end` jumps to
/// `target` with the exception in `reg` and the environment depth reset
/// to `env_depth`.
#[derive(Clone, Copy, Debug)]
pub struct Handler {
    pub start: Label,
    pub end: Label,
    pub target: Label,
    pub reg: Reg,
    pub env_depth: u16,
}

/// Constants of a code unit.
#[derive(Clone, Debug)]
pub enum Const {
    /// A number or an atom string.
    Value(Value),
    BigInt(Rc<str>),
    /// Names (atoms) of environment slots or global declarations.
    Names(Rc<[Value]>),
    /// Global declarations of a script: (vars, functions, lexicals).
    Globals(Rc<GlobalDecls>),
    /// Scope chain description for a direct eval.
    EvalScope(Rc<EvalScope>),
    /// Cooked and raw strings of a template site (cooked may be undefined).
    Template(Rc<[(Value, Value)]>),
}

#[derive(Debug, Default)]
pub struct GlobalDecls {
    /// `var` names (atoms).
    pub vars: Vec<Value>,
    /// Function declarations: (name atom, child code index).
    pub functions: Vec<(Value, u32)>,
    /// let/const/class names: (name, is_const).
    pub lexicals: Vec<(Value, bool)>,
    /// Eval code: the vars go into the variable environment of the caller
    /// (function scope) instead of the global object.
    pub eval: bool,
    pub strict: bool,
    /// Sloppy direct eval in a function: hops from the eval frame's
    /// environment to the function's var environment (None: global).
    pub var_env_hops: Option<u16>,
}

/// What a direct eval needs to compile code in the caller's scope: the
/// names of every visible binding and where it lives.
#[derive(Debug, Default)]
pub struct EvalScope {
    /// Innermost first: each environment level's slot names; a level is
    /// `None` for a `with` object environment.
    pub levels: Vec<Option<Rc<[(alloc::string::String, u16, BindingKind)]>>>,
    pub strict: bool,
    pub in_function: bool,
    pub this_mode_lexical: bool,
    pub new_target: bool,
    pub super_prop: bool,
    pub super_call: bool,
    pub in_class_field: bool,
    /// The function's own vars may be extended by sloppy eval.
    pub var_scope_level: Option<u16>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BindingKind {
    Var,
    Let,
    Const,
    /// A function's own name (immutable, silently in sloppy).
    FnName,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FuncKind {
    Normal,
    Arrow,
    Method,
    ClassConstructor,
    DerivedConstructor,
    Script,
    Module,
    Eval,
}

/// A compiled function, script, module or eval body.
#[derive(Debug)]
pub struct Code {
    pub ops: Vec<Op>,
    pub consts: Vec<Const>,
    pub children: Vec<Rc<Code>>,
    pub handlers: Vec<Handler>,
    pub nregs: u16,
    pub nparams: u16,
    /// Value of `length`.
    pub length: u16,
    pub name: Value,
    pub kind: FuncKind,
    pub strict: bool,
    pub is_async: bool,
    pub is_generator: bool,
    /// Copies the arguments at entry (uses `arguments` or a rest param).
    pub keep_args: bool,
    /// Has `InitFields` at entry (base class constructor with fields).
    pub has_fields: bool,
    /// Source positions: (pc, line, column), sorted by pc.
    pub lines: Vec<(u32, u32, u32)>,
    pub source: Rc<Source>,
    /// Byte range of the function source (for `toString`).
    pub src_start: u32,
    pub src_end: u32,
    /// Times called; hot code becomes a JIT candidate (later).
    pub calls: Cell<u32>,
}

/// A script's source text and name (URL), shared by all its code.
#[derive(Debug)]
pub struct Source {
    pub name: alloc::string::String,
    pub text: Rc<str>,
    pub id: u32,
}

impl Code {
    /// The source position of the instruction at `pc`.
    pub fn position(&self, pc: u32) -> (u32, u32) {
        match self.lines.binary_search_by(|&(p, _, _)| p.cmp(&pc)) {
            Ok(i) => (self.lines[i].1, self.lines[i].2),
            Err(0) => (0, 0),
            Err(i) => (self.lines[i - 1].1, self.lines[i - 1].2),
        }
    }
}
