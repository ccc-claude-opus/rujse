//! The abstract syntax tree produced by the parser and consumed by the
//! compiler. Names are `Rc<str>` (identifiers cannot contain lone
//! surrogates); string values are UTF-16 (`Str16`) because they can.

use alloc::boxed::Box;
use alloc::rc::Rc;
use alloc::vec::Vec;

pub type Name = Rc<str>;
pub type Str16 = Rc<[u16]>;

/// Source position of a node: byte offset, 1-based line and column.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Pos {
    pub offset: u32,
    pub line: u32,
    pub col: u32,
}

#[derive(Debug)]
pub struct Program {
    pub module: bool,
    pub strict: bool,
    pub body: Vec<Stmt>,
    /// Byte length of the source.
    pub end: u32,
}

#[derive(Debug)]
pub struct Stmt {
    pub kind: StmtKind,
    pub pos: Pos,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VarKind {
    Var,
    Let,
    Const,
    Using,
    AwaitUsing,
}

#[derive(Debug)]
pub struct VarDecl {
    pub target: Pattern,
    pub init: Option<Expr>,
    /// Byte offset just after the declarator (the binding is initialized
    /// from here on).
    pub end: u32,
}

#[derive(Debug)]
pub enum ForInit {
    Var(VarKind, Vec<VarDecl>),
    Expr(Expr),
}

/// The left side of `for (… in/of …)`.
#[derive(Debug)]
pub enum ForHead {
    /// `for (var/let/const x …)`; a `var` may carry an Annex B initializer
    /// (`for (var x = 1 in o)`).
    Var(VarKind, Pattern, Option<Expr>),
    /// `for (x.y of …)`, `for ([a, b] of …)`.
    Target(Pattern),
}

#[derive(Debug)]
pub struct Case {
    /// `None` for `default:`.
    pub test: Option<Expr>,
    pub body: Vec<Stmt>,
}

#[derive(Debug)]
pub enum StmtKind {
    Expr(Expr),
    Var(VarKind, Vec<VarDecl>),
    Function(Rc<Function>),
    Class(Rc<Class>),
    Return(Option<Expr>),
    If(Expr, Box<Stmt>, Option<Box<Stmt>>),
    Block(Vec<Stmt>),
    // Large parts are boxed: statements are many (2,000,000 `{}` must
    // not take gigabytes).
    For {
        init: Option<Box<ForInit>>,
        test: Option<Box<Expr>>,
        update: Option<Box<Expr>>,
        body: Box<Stmt>,
    },
    ForIn(Box<ForHead>, Expr, Box<Stmt>),
    ForOf {
        head: Box<ForHead>,
        iter: Expr,
        body: Box<Stmt>,
        is_await: bool,
    },
    While(Expr, Box<Stmt>),
    DoWhile(Box<Stmt>, Expr),
    Break(Option<Name>),
    Continue(Option<Name>),
    Throw(Expr),
    Try {
        block: Vec<Stmt>,
        /// `catch (param) { … }`; the param is optional (`catch { }`).
        handler: Option<Box<(Option<Pattern>, Vec<Stmt>)>>,
        finalizer: Option<Vec<Stmt>>,
    },
    Switch(Expr, Vec<Case>),
    Labeled(Name, Box<Stmt>),
    Empty,
    Debugger,
    With(Expr, Box<Stmt>),
    Import(Box<Import>),
    Export(Export),
}

#[derive(Debug)]
pub struct Import {
    pub specifier: Str16,
    pub default: Option<Name>,
    pub namespace: Option<Name>,
    /// `import { a as b }`: (imported name, local name).
    pub named: Vec<(Str16, Name)>,
    pub attributes: Vec<(Str16, Str16)>,
    /// `import defer * as ns` / `import source x` are not supported.
    pub pos: Pos,
}

#[derive(Debug)]
pub enum Export {
    /// `export { a as b }` or `export { a as b } from "m"`: (local or
    /// imported name, exported name).
    Named {
        specs: Vec<(Str16, Str16)>,
        from: Option<(Str16, Vec<(Str16, Str16)>)>,
    },
    /// `export * from "m"` / `export * as ns from "m"`.
    All {
        exported: Option<Str16>,
        from: Str16,
        attributes: Vec<(Str16, Str16)>,
    },
    /// `export default <expr>;`
    DefaultExpr(Expr),
    /// `export default function …` / `export default class …` (possibly
    /// anonymous; then bound as `*default*`).
    DefaultFunction(Rc<Function>),
    DefaultClass(Rc<Class>),
    /// `export var/let/const/function/class …`
    Decl(Box<Stmt>),
}

#[derive(Debug)]
pub struct Expr {
    pub kind: ExprKind,
    pub pos: Pos,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnaryOp {
    Minus,
    Plus,
    Not,
    BitNot,
    Typeof,
    Void,
    Delete,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BinaryOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Exp,
    Shl,
    Shr,
    Ushr,
    BitAnd,
    BitOr,
    BitXor,
    Eq,
    Ne,
    StrictEq,
    StrictNe,
    Lt,
    Gt,
    Le,
    Ge,
    In,
    InstanceOf,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogicalOp {
    And,
    Or,
    Nullish,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AssignOp {
    Assign,
    /// `a op= b` for an arithmetic/bitwise op.
    Op(BinaryOp),
    /// `&&=`, `||=`, `??=`.
    Logical(LogicalOp),
}

#[derive(Debug)]
pub enum ArrayElem {
    Hole,
    Expr(Expr),
    Spread(Expr),
}

#[derive(Debug)]
pub enum Arg {
    Expr(Expr),
    Spread(Expr),
}

/// A property name in an object literal, class or pattern.
#[derive(Debug)]
pub enum PropKey {
    /// Identifier or string literal key (UTF-16).
    Name(Str16),
    /// Numeric literal key; converted with Number::toString.
    Num(f64),
    /// BigInt literal key (decimal digits).
    BigInt(Name),
    Computed(Box<Expr>),
    /// `#x` (classes only).
    Private(Name),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MethodKind {
    Method,
    Get,
    Set,
}

#[derive(Debug)]
pub enum ObjProp {
    /// `key: value`; `shorthand` for `{ a }`.
    Value {
        key: PropKey,
        value: Expr,
        shorthand: bool,
    },
    Method {
        key: PropKey,
        kind: MethodKind,
        func: Rc<Function>,
    },
    Spread(Expr),
    /// `__proto__: value` (non-computed, non-shorthand): sets the prototype.
    Proto(Expr),
    /// `{ a = 1 }`: only valid as an assignment pattern (cover grammar);
    /// the parser rejects it elsewhere.
    CoverInit { name: Name, init: Expr },
}

#[derive(Debug)]
pub enum MemberProp {
    Name(Name),
    Computed(Box<Expr>),
    Private(Name),
}

#[derive(Debug)]
pub struct TemplatePiece {
    pub cooked: Option<Str16>,
    pub raw: Str16,
}

#[derive(Debug)]
pub enum ExprKind {
    Ident(Name),
    This,
    Null,
    Bool(bool),
    Num(f64),
    BigInt(Name),
    Str(Str16),
    Regex {
        body: Name,
        flags: Name,
    },
    /// `` `a${x}b` ``: `quasis.len() == exprs.len() + 1`.
    Template {
        quasis: Vec<TemplatePiece>,
        exprs: Vec<Expr>,
    },
    /// `` tag`…` ``: the quasis are shared so each site has one template object.
    Tagged {
        tag: Box<Expr>,
        quasis: Rc<Vec<TemplatePiece>>,
        exprs: Vec<Expr>,
    },
    Array(Vec<ArrayElem>),
    Object(Vec<ObjProp>),
    Function(Rc<Function>),
    Class(Rc<Class>),
    Unary(UnaryOp, Box<Expr>),
    /// `++x` / `x--`: (increment?, prefix?, target).
    Update(bool, bool, Box<Expr>),
    Binary(BinaryOp, Box<Expr>, Box<Expr>),
    Logical(LogicalOp, Box<Expr>, Box<Expr>),
    Assign(AssignOp, Box<Pattern>, Box<Expr>),
    Cond(Box<Expr>, Box<Expr>, Box<Expr>),
    /// `f(args)`; `optional` for `f?.(args)`.
    Call {
        callee: Box<Expr>,
        args: Vec<Arg>,
        optional: bool,
    },
    New(Box<Expr>, Vec<Arg>),
    /// `o.p`, `o[p]`, `o.#p`; `optional` for `o?.p`.
    Member {
        obj: Box<Expr>,
        prop: MemberProp,
        optional: bool,
    },
    /// `super.p` / `super[p]`.
    SuperMember(MemberProp),
    /// `super(args)`.
    SuperCall(Vec<Arg>),
    /// The whole of an optional chain `a?.b.c`: where a short circuit lands.
    OptionalChain(Box<Expr>),
    Seq(Vec<Expr>),
    /// `yield x` / `yield* x`.
    Yield(Option<Box<Expr>>, bool),
    Await(Box<Expr>),
    NewTarget,
    ImportMeta,
    /// `import(spec, options)`.
    ImportCall(Box<Expr>, Option<Box<Expr>>),
    /// `#x in obj`.
    PrivateIn(Name, Box<Expr>),
    /// `(expr)`: kept because some early errors depend on it (`({a}) = 1`
    /// is invalid, `(a) = 1` is valid); otherwise transparent.
    Paren(Box<Expr>),
}

/// A binding or assignment target.
#[derive(Debug)]
pub enum Pattern {
    Ident(Name, Pos),
    /// An assignment target that is not a binding (`o.p`, `o[k]`), only in
    /// assignment patterns.
    Expr(Box<Expr>),
    Object {
        props: Vec<PatProp>,
        rest: Option<Box<Pattern>>,
        pos: Pos,
    },
    Array {
        elems: Vec<Option<PatElem>>,
        rest: Option<Box<Pattern>>,
        pos: Pos,
    },
}

#[derive(Debug)]
pub struct PatElem {
    pub target: Pattern,
    pub default: Option<Expr>,
}

#[derive(Debug)]
pub struct PatProp {
    pub key: PropKey,
    pub target: Pattern,
    pub default: Option<Expr>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FnKind {
    Normal,
    Arrow,
    Method,
    Getter,
    Setter,
    /// A class constructor (`derived` when the class has `extends`).
    Constructor { derived: bool },
    /// The synthetic function holding a class's field initializers and
    /// static blocks.
    ClassInit,
}

#[derive(Debug)]
pub struct Param {
    pub target: Pattern,
    pub default: Option<Expr>,
}

#[derive(Debug)]
pub struct Function {
    pub name: Option<Name>,
    pub kind: FnKind,
    pub is_async: bool,
    pub is_generator: bool,
    pub params: Vec<Param>,
    pub rest: Option<Pattern>,
    pub body: Vec<Stmt>,
    /// An arrow function with an expression body (`x => x + 1`): `body` is
    /// a single `return`.
    pub expression_body: bool,
    /// The body (or an enclosing scope) is strict.
    pub strict: bool,
    /// The parameter list has no defaults, patterns or rest.
    pub simple_params: bool,
    /// Byte range of the source text (for `Function.prototype.toString`).
    pub start: u32,
    pub end: u32,
    pub pos: Pos,
}

#[derive(Debug)]
pub enum ClassMember {
    Method {
        key: PropKey,
        kind: MethodKind,
        is_static: bool,
        func: Rc<Function>,
    },
    Field {
        key: PropKey,
        is_static: bool,
        /// The initializer, wrapped as the body of a synthetic method so it
        /// gets its own `this` and scope.
        value: Option<Rc<Function>>,
        /// `accessor x = 1` (auto-accessor).
        accessor: bool,
    },
    StaticBlock(Rc<Function>),
}

#[derive(Debug)]
pub struct Class {
    pub name: Option<Name>,
    pub extends: Option<Box<Expr>>,
    pub constructor: Option<Rc<Function>>,
    pub members: Vec<ClassMember>,
    pub start: u32,
    pub end: u32,
    pub pos: Pos,
}

impl Expr {
    pub fn new(kind: ExprKind, pos: Pos) -> Expr {
        Expr { kind, pos }
    }

    /// Strips parentheses.
    pub fn unparen(&self) -> &Expr {
        let mut e = self;
        while let ExprKind::Paren(inner) = &e.kind {
            e = inner;
        }
        e
    }

    /// An anonymous function or class definition: gets its name from the
    /// binding it is assigned to (NamedEvaluation).
    pub fn is_anonymous_function_definition(&self) -> bool {
        match &self.unparen().kind {
            ExprKind::Function(f) => f.name.is_none(),
            ExprKind::Class(c) => c.name.is_none(),
            _ => false,
        }
    }
}

/// UTF-16 of a Rust string.
pub fn utf16(s: &str) -> Str16 {
    s.encode_utf16().collect::<Vec<u16>>().into()
}

/// Whether a UTF-16 string equals an ASCII/Rust string.
pub fn eq16(a: &[u16], b: &str) -> bool {
    a.iter().copied().eq(b.encode_utf16())
}
