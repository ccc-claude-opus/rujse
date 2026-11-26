//! Recursive-descent parser for ECMAScript 2026 (scripts and modules),
//! with the syntactic early errors. Arrow parameters and destructuring
//! assignment use the cover grammar: parsed as expressions, then converted.

use alloc::boxed::Box;
use alloc::format;
use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use crate::ast::*;
use crate::lexer::{Lexer, SyntaxError, Tok, Token, P};

pub type PResult<T> = Result<T, SyntaxError>;

/// What the code around the current point allows.
#[derive(Clone, Copy, Default)]
struct Ctx {
    strict: bool,
    /// Inside a generator: `yield` is an operator.
    yield_: bool,
    /// Inside an async function or module top level: `await` is an operator.
    await_: bool,
    /// `return` allowed.
    ret: bool,
    super_call: bool,
    super_prop: bool,
    new_target: bool,
    /// Class field initializer or static block: `arguments` is an error.
    no_arguments: bool,
    /// Static block: `await` is reserved as an identifier.
    static_block: bool,
    /// Parameters of the function being parsed: `yield`/`await`
    /// expressions are errors there.
    in_params: bool,
}

struct ClassScope {
    declared: Vec<(Name, u8)>,
    used: Vec<(Name, Pos)>,
}

/// Parameters collected from a parenthesized list or call arguments,
/// before we know whether they become arrow parameters.
struct CoverList {
    items: Vec<Expr>,
    rest: Option<(Expr, Pos)>,
    trailing_comma: bool,
    /// Position of the first `{a = 1}` cover initializer, if any.
    cover_init: Option<Pos>,
    /// A `yield`/`await` expression appeared (invalid in arrow params).
    has_yield_await: Option<Pos>,
}

pub struct Parser<'a> {
    lx: Lexer<'a>,
    tok: Token,
    prev_end: u32,
    prev_line: u32,
    ctx: Ctx,
    module: bool,
    labels: Vec<(Name, bool)>,
    loop_depth: u32,
    switch_depth: u32,
    classes: Vec<ClassScope>,
    /// First `{a = 1}` seen while parsing an expression that may still turn
    /// into a pattern.
    pending_cover_init: Option<Pos>,
    /// A `yield`/`await` expression was parsed (for arrow parameter checks).
    saw_yield_await: Option<Pos>,
    /// Depth of array/object literals and parenthesized lists being parsed
    /// (where `{a = 1}` may still turn into a pattern).
    cover_depth: u32,
    /// A duplicate `__proto__` in an object literal (an error unless the
    /// literal becomes a pattern).
    dup_proto: Option<Pos>,
    /// Code given to `eval` inside a class may use its private names.
    private_ok_outside_class: bool,
}

const RESERVED: &[&str] = &[
    "break", "case", "catch", "class", "const", "continue", "debugger", "default", "delete", "do", "else", "enum",
    "export", "extends", "false", "finally", "for", "function", "if", "import", "in", "instanceof", "new", "null",
    "return", "super", "switch", "this", "throw", "true", "try", "typeof", "var", "void", "while", "with",
];
const STRICT_RESERVED: &[&str] =
    &["implements", "interface", "let", "package", "private", "protected", "public", "static", "yield"];

fn pos_of(t: &Token) -> Pos {
    Pos { offset: t.start, line: t.line, col: t.col }
}

/// Options for parsing code given to `eval` or `Function`.
#[derive(Clone, Copy, Default)]
pub struct EvalCtx {
    pub strict: bool,
    pub in_function: bool,
    pub super_prop: bool,
    pub super_call: bool,
    pub new_target: bool,
    pub in_class_field: bool,
}

impl<'a> Parser<'a> {
    pub fn new(src: &'a str) -> PResult<Parser<'a>> {
        let mut lx = Lexer::new(src);
        let tok = lx.next()?;
        Ok(Parser {
            lx,
            tok,
            prev_end: 0,
            prev_line: 1,
            ctx: Ctx::default(),
            module: false,
            labels: Vec::new(),
            loop_depth: 0,
            switch_depth: 0,
            classes: Vec::new(),
            pending_cover_init: None,
            saw_yield_await: None,
            cover_depth: 0,
            dup_proto: None,
            private_ok_outside_class: false,
        })
    }

    pub fn parse_script(src: &'a str, strict: bool) -> PResult<Program> {
        let mut p = Parser::new(src)?;
        p.ctx.strict = strict;
        p.program(false)
    }

    pub fn parse_module(src: &'a str) -> PResult<Program> {
        let mut p = Parser::new(src)?;
        p.ctx.strict = true;
        p.ctx.await_ = true;
        p.module = true;
        p.program(true)
    }

    /// Code for `eval`: a script with the context of the caller.
    pub fn parse_eval(src: &'a str, e: EvalCtx) -> PResult<Program> {
        let mut p = Parser::new(src)?;
        p.ctx.strict = e.strict;
        p.ctx.super_prop = e.super_prop;
        p.ctx.super_call = e.super_call;
        p.ctx.new_target = e.new_target || e.in_function;
        p.ctx.no_arguments = e.in_class_field;
        p.private_ok_outside_class = e.super_prop;
        p.program(false)
    }

    /// `new Function(params, body)`: parses the pieces as one function.
    pub fn parse_function_parts(
        params: &str,
        body: &str,
        is_async: bool,
        is_generator: bool,
    ) -> PResult<Rc<Function>> {
        // Parse each part alone first so `/*` in params cannot swallow `)`.
        {
            let mut p = Parser::new(params)?;
            p.ctx.yield_ = is_generator;
            p.ctx.await_ = is_async;
            p.ctx.in_params = true;
            let mut params_out = Vec::new();
            let mut rest = None;
            p.formal_params_until_eof(&mut params_out, &mut rest)?;
        }
        {
            let mut p = Parser::new(body)?;
            p.ctx.yield_ = is_generator;
            p.ctx.await_ = is_async;
            p.ctx.ret = true;
            p.ctx.new_target = true;
            p.ctx.super_prop = false;
            let mut out = Vec::new();
            while p.tok.tok != Tok::Eof {
                out.push(p.statement_list_item()?);
            }
        }
        let kw = match (is_async, is_generator) {
            (false, false) => "function",
            (false, true) => "function*",
            (true, false) => "async function",
            (true, true) => "async function*",
        };
        let src = format!("({kw} anonymous({params}\n) {{\n{body}\n}})");
        // Leak-free: parse from an owned string.
        let src: Rc<str> = src.into();
        let prog = {
            let mut p = Parser::new(&src)?;
            p.program(false)?
        };
        match prog.body.into_iter().next().map(|s| s.kind) {
            Some(StmtKind::Expr(Expr { kind: ExprKind::Paren(e), .. })) => match e.kind {
                ExprKind::Function(f) => Ok(f),
                _ => Err(SyntaxError { message: "bad function".into(), line: 1, col: 1 }),
            },
            _ => Err(SyntaxError { message: "bad function".into(), line: 1, col: 1 }),
        }
    }

    // ----- tokens --------------------------------------------------------

    fn next(&mut self) -> PResult<Token> {
        let t = self.lx.next()?;
        self.prev_end = self.tok.end;
        self.prev_line = self.tok.line;
        Ok(core::mem::replace(&mut self.tok, t))
    }

    fn err<T>(&self, msg: &str) -> PResult<T> {
        Err(SyntaxError { message: msg.into(), line: self.tok.line, col: self.tok.col })
    }

    fn err_at<T>(&self, pos: Pos, msg: &str) -> PResult<T> {
        Err(SyntaxError { message: msg.into(), line: pos.line, col: pos.col })
    }

    fn unexpected<T>(&self) -> PResult<T> {
        let what = match &self.tok.tok {
            Tok::Eof => String::from("end of input"),
            Tok::Ident(n) => format!("identifier '{n}'"),
            Tok::Punct(p) => format!("token {p:?}"),
            Tok::Num(_) | Tok::BigInt(_) => String::from("number"),
            Tok::Str(_) => String::from("string"),
            Tok::Template { .. } => String::from("template"),
            Tok::Regex { .. } => String::from("regular expression"),
            Tok::Private(n) => format!("#{n}"),
        };
        self.err(&format!("unexpected {what}"))
    }

    fn pos(&self) -> Pos {
        pos_of(&self.tok)
    }

    fn at(&self, p: P) -> bool {
        self.tok.tok == Tok::Punct(p)
    }

    fn eat(&mut self, p: P) -> PResult<bool> {
        if self.at(p) {
            self.next()?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    fn expect(&mut self, p: P) -> PResult<Token> {
        if self.at(p) { self.next() } else { self.unexpected() }
    }

    /// The current token is the keyword `kw` (spelled without escapes).
    fn at_kw(&self, kw: &str) -> bool {
        matches!(&self.tok.tok, Tok::Ident(n) if &**n == kw) && !self.tok.escaped
    }

    fn eat_kw(&mut self, kw: &str) -> PResult<bool> {
        if self.at_kw(kw) {
            self.next()?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    fn expect_kw(&mut self, kw: &str) -> PResult<()> {
        if self.at_kw(kw) {
            self.next()?;
            Ok(())
        } else {
            self.unexpected()
        }
    }

    /// The token after the current one (without consuming).
    fn peek(&self) -> PResult<Token> {
        let mut lx = self.lx.clone();
        lx.next()
    }

    fn semicolon(&mut self) -> PResult<()> {
        if self.eat(P::Semi)? {
            return Ok(());
        }
        if self.at(P::RBrace) || self.tok.tok == Tok::Eof || self.tok.nl_before {
            return Ok(());
        }
        self.unexpected()
    }

    fn ident_name(&self) -> Option<Name> {
        match &self.tok.tok {
            Tok::Ident(n) => Some(n.clone()),
            _ => None,
        }
    }

    // ----- identifiers ---------------------------------------------------

    /// Checks that `name` may be used as an IdentifierReference or binding.
    fn check_identifier(&self, name: &str, binding: bool, pos: Pos) -> PResult<()> {
        if RESERVED.contains(&name) {
            return self.err_at(pos, &format!("unexpected reserved word '{name}'"));
        }
        if self.ctx.strict && STRICT_RESERVED.contains(&name) {
            return self.err_at(pos, &format!("'{name}' is reserved in strict mode"));
        }
        if name == "yield" && (self.ctx.yield_ || self.ctx.strict) {
            return self.err_at(pos, "unexpected 'yield'");
        }
        if name == "await" && (self.ctx.await_ || self.module || self.ctx.static_block) {
            return self.err_at(pos, "unexpected 'await'");
        }
        if binding && self.ctx.strict && (name == "eval" || name == "arguments") {
            return self.err_at(pos, &format!("cannot bind '{name}' in strict mode"));
        }
        if name == "arguments" && self.ctx.no_arguments {
            return self.err_at(pos, "'arguments' is not allowed in class field initializers or static blocks");
        }
        Ok(())
    }

    fn binding_identifier(&mut self) -> PResult<(Name, Pos)> {
        let pos = self.pos();
        match self.ident_name() {
            Some(n) => {
                self.check_identifier(&n, true, pos)?;
                self.next()?;
                Ok((n, pos))
            }
            None => self.unexpected(),
        }
    }

    fn label_identifier(&mut self) -> PResult<Name> {
        let (n, _) = {
            let pos = self.pos();
            match self.ident_name() {
                Some(n) => {
                    self.check_identifier(&n, false, pos)?;
                    self.next()?;
                    (n, pos)
                }
                None => return self.unexpected(),
            }
        };
        Ok(n)
    }

    // ----- program -------------------------------------------------------

    fn program(&mut self, module: bool) -> PResult<Program> {
        let mut body = Vec::new();
        if !module {
            self.directives(&mut body, None)?;
        }
        while self.tok.tok != Tok::Eof {
            if module {
                body.push(self.module_item()?);
            } else {
                body.push(self.statement_list_item()?);
            }
        }
        if !self.labels.is_empty() {
            return self.err("unbalanced labels");
        }
        Ok(Program { module, strict: self.ctx.strict, body, end: self.lx.source().len() as u32 })
    }

    /// Parses the directive prologue (`"use strict";`) into `out`. Returns
    /// whether a "use strict" directive was found.
    fn directives(&mut self, out: &mut Vec<Stmt>, mut on_strict: Option<&mut dyn FnMut(&Self) -> PResult<()>>) -> PResult<bool> {
        let mut found = false;
        let mut octal: Option<Pos> = None;
        loop {
            let Tok::Str(s) = &self.tok.tok else { break };
            let s = s.clone();
            let tok = self.tok.clone();
            // Only a lone string literal statement is a directive.
            let mut lx = self.lx.clone();
            let after = lx.next()?;
            let ends = match &after.tok {
                Tok::Punct(P::Semi) | Tok::Punct(P::RBrace) | Tok::Eof => true,
                _ => after.nl_before && !matches!(&after.tok, Tok::Punct(p) if is_continuation(*p)) && !matches!(&after.tok, Tok::Template { .. }),
            };
            if !ends {
                break;
            }
            if tok.escaped && octal.is_none() {
                octal = Some(pos_of(&tok));
            }
            let raw = self.lx.slice(tok.start + 1, tok.end - 1);
            if raw == "use strict" {
                found = true;
                if !self.ctx.strict {
                    self.ctx.strict = true;
                    if let Some(f) = on_strict.as_mut() {
                        f(self)?;
                    }
                }
                if let Some(p) = octal {
                    return self.err_at(p, "octal escape sequences are not allowed in strict mode");
                }
            }
            let pos = self.pos();
            self.next()?;
            self.semicolon()?;
            out.push(Stmt { kind: StmtKind::Expr(Expr::new(ExprKind::Str(s), pos)), pos });
        }
        Ok(found)
    }

    // ----- statements ----------------------------------------------------

    fn statement_list_item(&mut self) -> PResult<Stmt> {
        let pos = self.pos();
        if self.at_kw("function") {
            let f = self.function_declaration(false, pos, false)?;
            return Ok(Stmt { kind: StmtKind::Function(f), pos });
        }
        if self.at_kw("async") && !self.tok.escaped {
            let n = self.peek()?;
            if matches!(&n.tok, Tok::Ident(k) if &**k == "function") && !n.escaped && !n.nl_before {
                self.next()?;
                let f = self.function_declaration(true, pos, false)?;
                return Ok(Stmt { kind: StmtKind::Function(f), pos });
            }
        }
        if self.at_kw("class") {
            let c = self.class(true, false)?;
            return Ok(Stmt { kind: StmtKind::Class(c), pos });
        }
        if self.at_kw("const") {
            self.next()?;
            let d = self.var_declarations(VarKind::Const, false)?;
            self.semicolon()?;
            return Ok(Stmt { kind: StmtKind::Var(VarKind::Const, d), pos });
        }
        if self.is_let_declaration()? {
            self.next()?;
            let d = self.var_declarations(VarKind::Let, false)?;
            self.semicolon()?;
            return Ok(Stmt { kind: StmtKind::Var(VarKind::Let, d), pos });
        }
        if let Some(kind) = self.using_declaration_start()? {
            let d = self.var_declarations(kind, false)?;
            self.semicolon()?;
            return Ok(Stmt { kind: StmtKind::Var(kind, d), pos });
        }
        self.statement()
    }

    /// `using x = …` / `await using x = …` (explicit resource management).
    fn using_declaration_start(&mut self) -> PResult<Option<VarKind>> {
        if self.at_kw("using") {
            let n = self.peek()?;
            if let Tok::Ident(name) = &n.tok {
                if !n.nl_before && &**name != "in" && !(&**name == "of" && !n.escaped) {
                    self.next()?;
                    return Ok(Some(VarKind::Using));
                }
            }
        }
        if self.at_kw("await") && (self.ctx.await_ && !self.ctx.in_params) {
            let mut lx = self.lx.clone();
            let n = lx.next()?;
            if matches!(&n.tok, Tok::Ident(k) if &**k == "using") && !n.escaped && !n.nl_before {
                let n2 = lx.next()?;
                if matches!(&n2.tok, Tok::Ident(_)) && !n2.nl_before {
                    self.next()?;
                    self.next()?;
                    return Ok(Some(VarKind::AwaitUsing));
                }
            }
        }
        Ok(None)
    }

    /// `let` starts a declaration when followed by an identifier, `[` or `{`.
    fn is_let_declaration(&self) -> PResult<bool> {
        if !self.at_kw("let") {
            return Ok(false);
        }
        let n = self.peek()?;
        Ok(match &n.tok {
            Tok::Punct(P::LBracket) | Tok::Punct(P::LBrace) => true,
            // `let in …` / `let instanceof …` are expressions; any other
            // identifier (even on the next line) makes a declaration.
            Tok::Ident(name) => !(&**name == "in" || &**name == "instanceof"),
            _ => false,
        })
    }

    fn statement(&mut self) -> PResult<Stmt> {
        let pos = self.pos();
        let kind = match &self.tok.tok {
            Tok::Punct(P::LBrace) => StmtKind::Block(self.block()?),
            Tok::Punct(P::Semi) => {
                self.next()?;
                StmtKind::Empty
            }
            Tok::Ident(n) if !self.tok.escaped => match &**n {
                "var" => {
                    self.next()?;
                    let d = self.var_declarations(VarKind::Var, false)?;
                    self.semicolon()?;
                    StmtKind::Var(VarKind::Var, d)
                }
                "if" => return self.if_statement(),
                "for" => return self.for_statement(),
                "while" => {
                    self.next()?;
                    self.expect(P::LParen)?;
                    let test = self.expression(false)?;
                    self.expect(P::RParen)?;
                    let body = self.loop_body()?;
                    StmtKind::While(test, Box::new(body))
                }
                "do" => {
                    self.next()?;
                    let body = self.loop_body()?;
                    self.expect_kw("while")?;
                    self.expect(P::LParen)?;
                    let test = self.expression(false)?;
                    self.expect(P::RParen)?;
                    // `do … while (x)` ends without a semicolon if needed.
                    self.eat(P::Semi)?;
                    StmtKind::DoWhile(Box::new(body), test)
                }
                "continue" | "break" => {
                    let is_break = &**n == "break";
                    self.next()?;
                    let mut label = None;
                    if let Tok::Ident(_) = &self.tok.tok {
                        if !self.tok.nl_before {
                            let l = self.label_identifier()?;
                            match self.labels.iter().rev().find(|(n, _)| *n == l) {
                                None => return self.err_at(pos, &format!("undefined label '{l}'")),
                                Some((_, is_loop)) if !is_break && !*is_loop => {
                                    return self.err_at(pos, &format!("'{l}' is not a loop label"));
                                }
                                _ => {}
                            }
                            label = Some(l);
                        }
                    }
                    if label.is_none() {
                        if is_break && self.loop_depth == 0 && self.switch_depth == 0 {
                            return self.err_at(pos, "illegal break statement");
                        }
                        if !is_break && self.loop_depth == 0 {
                            return self.err_at(pos, "illegal continue statement");
                        }
                    }
                    self.semicolon()?;
                    if is_break { StmtKind::Break(label) } else { StmtKind::Continue(label) }
                }
                "return" => {
                    if !self.ctx.ret {
                        return self.err("illegal return statement");
                    }
                    self.next()?;
                    let arg = if self.at(P::Semi) || self.at(P::RBrace) || self.tok.tok == Tok::Eof || self.tok.nl_before {
                        None
                    } else {
                        Some(self.expression(false)?)
                    };
                    self.semicolon()?;
                    StmtKind::Return(arg)
                }
                "with" => {
                    if self.ctx.strict {
                        return self.err("'with' is not allowed in strict mode");
                    }
                    self.next()?;
                    self.expect(P::LParen)?;
                    let obj = self.expression(false)?;
                    self.expect(P::RParen)?;
                    let body = self.substatement()?;
                    StmtKind::With(obj, Box::new(body))
                }
                "switch" => {
                    self.next()?;
                    self.expect(P::LParen)?;
                    let d = self.expression(false)?;
                    self.expect(P::RParen)?;
                    self.expect(P::LBrace)?;
                    let mut cases = Vec::new();
                    let mut has_default = false;
                    self.switch_depth += 1;
                    while !self.eat(P::RBrace)? {
                        let test = if self.eat_kw("case")? {
                            Some(self.expression(false)?)
                        } else if self.at_kw("default") {
                            if has_default {
                                return self.err("more than one default clause in switch");
                            }
                            has_default = true;
                            self.next()?;
                            None
                        } else {
                            return self.unexpected();
                        };
                        self.expect(P::Colon)?;
                        let mut body = Vec::new();
                        while !(self.at_kw("case") || self.at_kw("default") || self.at(P::RBrace)) {
                            if self.tok.tok == Tok::Eof {
                                return self.unexpected();
                            }
                            body.push(self.statement_list_item()?);
                        }
                        cases.push(Case { test, body });
                    }
                    self.switch_depth -= 1;
                    StmtKind::Switch(d, cases)
                }
                "throw" => {
                    self.next()?;
                    if self.tok.nl_before {
                        return self.err("illegal newline after throw");
                    }
                    let e = self.expression(false)?;
                    self.semicolon()?;
                    StmtKind::Throw(e)
                }
                "try" => return self.try_statement(),
                "debugger" => {
                    self.next()?;
                    self.semicolon()?;
                    StmtKind::Debugger
                }
                "function" => {
                    // Annex B: a function declaration as the body of an `if`
                    // (handled there); elsewhere it is an error.
                    return self.err("function declarations are not allowed in this position");
                }
                "class" => return self.err("class declarations are not allowed in this position"),
                "const" => return self.err("lexical declaration cannot appear in a single-statement context"),
                "let" => {
                    let n = self.peek()?;
                    if matches!(n.tok, Tok::Punct(P::LBracket)) {
                        return self.err("lexical declaration cannot appear in a single-statement context");
                    }
                    return self.expression_or_labeled_statement();
                }
                "import" => {
                    let n = self.peek()?;
                    if matches!(n.tok, Tok::Punct(P::LParen) | Tok::Punct(P::Dot)) {
                        return self.expression_or_labeled_statement();
                    }
                    return self.err("import declarations may only appear at top level of a module");
                }
                "export" => return self.err("export declarations may only appear at top level of a module"),
                _ => return self.expression_or_labeled_statement(),
            },
            _ => return self.expression_or_labeled_statement(),
        };
        Ok(Stmt { kind, pos })
    }

    /// The body of `if`/`while`/`with`/labels: a Statement (not a declaration).
    fn substatement(&mut self) -> PResult<Stmt> {
        self.statement()
    }

    fn loop_body(&mut self) -> PResult<Stmt> {
        self.loop_depth += 1;
        let s = self.substatement();
        self.loop_depth -= 1;
        s
    }

    fn block(&mut self) -> PResult<Vec<Stmt>> {
        self.expect(P::LBrace)?;
        let mut body = Vec::new();
        while !self.eat(P::RBrace)? {
            if self.tok.tok == Tok::Eof {
                return self.unexpected();
            }
            body.push(self.statement_list_item()?);
        }
        Ok(body)
    }

    fn if_statement(&mut self) -> PResult<Stmt> {
        let pos = self.pos();
        self.next()?;
        self.expect(P::LParen)?;
        let test = self.expression(false)?;
        self.expect(P::RParen)?;
        let cons = self.if_body()?;
        let alt = if self.eat_kw("else")? { Some(Box::new(self.if_body()?)) } else { None };
        Ok(Stmt { kind: StmtKind::If(test, Box::new(cons), alt), pos })
    }

    /// Annex B.3.3: `if (x) function f() {}` in sloppy mode is a function
    /// declaration in an implicit block.
    fn if_body(&mut self) -> PResult<Stmt> {
        if self.at_kw("function") && !self.ctx.strict {
            let pos = self.pos();
            let f = self.function_declaration(false, pos, true)?;
            if f.is_generator || f.is_async {
                return self.err_at(pos, "generator/async declarations are not allowed here");
            }
            let s = Stmt { kind: StmtKind::Function(f), pos };
            return Ok(Stmt { kind: StmtKind::Block(vec![s]), pos });
        }
        self.substatement()
    }

    fn try_statement(&mut self) -> PResult<Stmt> {
        let pos = self.pos();
        self.next()?;
        let block = self.block()?;
        let mut handler = None;
        let mut finalizer = None;
        if self.eat_kw("catch")? {
            let param = if self.eat(P::LParen)? {
                let p = self.binding_target()?;
                self.expect(P::RParen)?;
                Some(p)
            } else {
                None
            };
            let body = self.block()?;
            handler = Some((param, body));
        }
        if self.eat_kw("finally")? {
            finalizer = Some(self.block()?);
        }
        if handler.is_none() && finalizer.is_none() {
            return self.err("missing catch or finally after try");
        }
        Ok(Stmt { kind: StmtKind::Try { block, handler: handler.map(Box::new), finalizer }, pos })
    }

    fn expression_or_labeled_statement(&mut self) -> PResult<Stmt> {
        let pos = self.pos();
        // A label: identifier followed by `:`.
        if let Tok::Ident(name) = &self.tok.tok {
            let name = name.clone();
            let n = self.peek()?;
            if n.tok == Tok::Punct(P::Colon) && !(self.at_kw("async") && false) {
                self.check_identifier(&name, false, pos)?;
                if self.labels.iter().any(|(l, _)| *l == name) {
                    return self.err(&format!("label '{name}' has already been declared"));
                }
                self.next()?;
                self.next()?;
                // Is the labeled item a loop (for `continue label`)?
                let is_loop = self.at_kw("for") || self.at_kw("while") || self.at_kw("do") || {
                    // Nested labels: `a: b: while …` – look through labels.
                    let mut lx = self.lx.clone();
                    let mut t = self.tok.clone();
                    loop {
                        if let Tok::Ident(_) = &t.tok {
                            let n = lx.next()?;
                            if n.tok == Tok::Punct(P::Colon) {
                                t = lx.next()?;
                                continue;
                            }
                        }
                        break matches!(&t.tok, Tok::Ident(k) if !t.escaped && matches!(&**k, "for" | "while" | "do"));
                    }
                };
                self.labels.push((name.clone(), is_loop));
                let body = if self.at_kw("function") {
                    if self.ctx.strict {
                        return self.err("in strict mode code, functions can only be declared at top level or inside a block");
                    }
                    let fpos = self.pos();
                    let f = self.function_declaration(false, fpos, true)?;
                    if f.is_generator || f.is_async {
                        return self.err_at(fpos, "generator/async declarations are not allowed here");
                    }
                    Stmt { kind: StmtKind::Function(f), pos: fpos }
                } else {
                    self.substatement()?
                };
                self.labels.pop();
                return Ok(Stmt { kind: StmtKind::Labeled(name, Box::new(body)), pos });
            }
        }
        if self.at_kw("async") {
            let n = self.peek()?;
            if matches!(&n.tok, Tok::Ident(k) if &**k == "function") && !n.nl_before {
                return self.err("async function declarations are not allowed in this position");
            }
        }
        let e = self.expression(false)?;
        self.semicolon()?;
        Ok(Stmt { kind: StmtKind::Expr(e), pos })
    }

    fn var_declarations(&mut self, kind: VarKind, no_in: bool) -> PResult<Vec<VarDecl>> {
        let mut out = Vec::new();
        loop {
            let target = if matches!(kind, VarKind::Using | VarKind::AwaitUsing) {
                let (n, p) = self.binding_identifier()?;
                Pattern::Ident(n, p)
            } else {
                self.binding_target()?
            };
            if kind != VarKind::Var {
                self.check_lexical_names(&target)?;
            }
            let init = if self.eat(P::Assign)? { Some(self.assignment(no_in)?) } else { None };
            out.push(VarDecl { target, init, end: self.prev_end });
            if !self.eat(P::Comma)? {
                break;
            }
        }
        Ok(out)
    }

    /// `let let = 1` and duplicate names in one lexical declaration.
    fn check_lexical_names(&self, target: &Pattern) -> PResult<()> {
        let mut names = Vec::new();
        bound_names(target, &mut names);
        for (n, p) in &names {
            if &**n == "let" {
                return self.err_at(*p, "'let' cannot be a lexically bound name");
            }
        }
        Ok(())
    }

    fn for_statement(&mut self) -> PResult<Stmt> {
        let pos = self.pos();
        self.next()?;
        let mut is_await = false;
        if self.at_kw("await") {
            if !self.ctx.await_ || self.ctx.in_params {
                return self.err("for await is only valid in async functions and modules");
            }
            self.next()?;
            is_await = true;
        }
        self.expect(P::LParen)?;
        // Head
        let mut init = None;
        if self.at(P::Semi) {
            if is_await {
                return self.unexpected();
            }
        } else {
            let decl_kind = if self.at_kw("var") {
                Some(VarKind::Var)
            } else if self.at_kw("const") {
                Some(VarKind::Const)
            } else if self.is_let_declaration()? {
                Some(VarKind::Let)
            } else {
                None
            };
            let using = if decl_kind.is_none() { self.for_using_start()? } else { None };
            if let Some(kind) = decl_kind.or(using) {
                if decl_kind.is_some() {
                    self.next()?;
                }
                let decls = self.var_declarations(kind, true)?;
                if decls.len() == 1 && (self.at_kw("of") || self.at_kw("in")) {
                    let is_of = self.at_kw("of");
                    let d = decls.into_iter().next().unwrap();
                    if let Some(i) = &d.init {
                        // Annex B: `for (var x = 1 in o)` (sloppy, simple binding).
                        let ok = !is_of && kind == VarKind::Var && !self.ctx.strict && matches!(d.target, Pattern::Ident(..));
                        if !ok {
                            return self.err_at(i.pos, "for-in/of loop variable declaration may not have an initializer");
                        }
                    }
                    if !is_of && matches!(kind, VarKind::Using | VarKind::AwaitUsing) {
                        return self.err("using declarations are not allowed in for-in");
                    }
                    if is_await && !is_of {
                        return self.unexpected();
                    }
                    self.next()?;
                    let right = if is_of { self.assignment(false)? } else { self.expression(false)? };
                    self.expect(P::RParen)?;
                    let body = self.loop_body()?;
                    let head = ForHead::Var(kind, d.target, d.init);
                    let kind = if is_of {
                        StmtKind::ForOf { head: Box::new(head), iter: right, body: Box::new(body), is_await }
                    } else {
                        StmtKind::ForIn(Box::new(head), right, Box::new(body))
                    };
                    return Ok(Stmt { kind, pos });
                }
                if is_await {
                    return self.unexpected();
                }
                for d in &decls {
                    if d.init.is_none() && (kind == VarKind::Const || matches!(kind, VarKind::Using | VarKind::AwaitUsing) || !matches!(d.target, Pattern::Ident(..))) {
                        return self.err("missing initializer in declaration");
                    }
                }
                init = Some(ForInit::Var(kind, decls));
            } else {
                // Expression or assignment target.
                let starts_with_let = self.at_kw("let");
                let starts_with_async = self.at_kw("async") && !self.tok.escaped;
                let saved_cover = self.pending_cover_init.take();
                self.cover_depth += 1;
                let e = self.expression(true);
                self.cover_depth -= 1;
                let e = e?;
                let cover = self.pending_cover_init.take();
                self.pending_cover_init = saved_cover;
                if self.at_kw("of") || self.at_kw("in") {
                    let is_of = self.at_kw("of");
                    if is_of && starts_with_let {
                        return self.err("for-of loop head may not start with 'let'");
                    }
                    if is_of && starts_with_async && !is_await && matches!(&e.kind, ExprKind::Ident(n) if &**n == "async") {
                        return self.err("for-of loop head may not start with 'async of'");
                    }
                    if is_await && !is_of {
                        return self.unexpected();
                    }
                    let target = self.to_assign_target(e, true)?;
                    self.next()?;
                    let right = if is_of { self.assignment(false)? } else { self.expression(false)? };
                    self.expect(P::RParen)?;
                    let body = self.loop_body()?;
                    let head = ForHead::Target(target);
                    let kind = if is_of {
                        StmtKind::ForOf { head: Box::new(head), iter: right, body: Box::new(body), is_await }
                    } else {
                        StmtKind::ForIn(Box::new(head), right, Box::new(body))
                    };
                    return Ok(Stmt { kind, pos });
                }
                if let Some(p) = cover {
                    return self.err_at(p, "invalid shorthand property initializer");
                }
                if is_await {
                    return self.unexpected();
                }
                init = Some(ForInit::Expr(e));
            }
        }
        self.expect(P::Semi)?;
        let test = if self.at(P::Semi) { None } else { Some(self.expression(false)?) };
        self.expect(P::Semi)?;
        let update = if self.at(P::RParen) { None } else { Some(self.expression(false)?) };
        self.expect(P::RParen)?;
        let body = self.loop_body()?;
        Ok(Stmt { kind: StmtKind::For { init: init.map(Box::new), test: test.map(Box::new), update: update.map(Box::new), body: Box::new(body) }, pos })
    }

    fn for_using_start(&mut self) -> PResult<Option<VarKind>> {
        if self.at_kw("using") {
            let mut lx = self.lx.clone();
            let n = lx.next()?;
            if let Tok::Ident(name) = &n.tok {
                if !n.nl_before && &**name != "in" {
                    if &**name == "of" && !n.escaped {
                        // `for (using of …)`: `using` is an identifier,
                        // unless `using of = …` / `using of of …`.
                        let n2 = lx.next()?;
                        if !matches!(n2.tok, Tok::Punct(P::Assign)) && !matches!(&n2.tok, Tok::Ident(k) if &**k == "of") {
                            return Ok(None);
                        }
                    }
                    self.next()?;
                    return Ok(Some(VarKind::Using));
                }
            }
            return Ok(None);
        }
        self.using_declaration_start()
    }

    // ----- functions -----------------------------------------------------

    fn function_declaration(&mut self, is_async: bool, start_pos: Pos, annex_b: bool) -> PResult<Rc<Function>> {
        let start = start_pos.offset;
        self.expect_kw("function")?;
        let is_generator = self.eat(P::Star)?;
        if annex_b && is_generator {
            return self.err_at(start_pos, "generator declarations are not allowed here");
        }
        // The name is bound in the enclosing scope: checked with the outer
        // context (except `yield`/`await` rules of the function itself).
        let (name, npos) = self.binding_identifier_for_function(false, is_async, is_generator)?;
        self.function_rest(Some((name, npos)), FnKind::Normal, is_async, is_generator, start, start_pos)
    }

    fn binding_identifier_for_function(&mut self, expression: bool, is_async: bool, is_generator: bool) -> PResult<(Name, Pos)> {
        let pos = self.pos();
        let Some(n) = self.ident_name() else { return self.unexpected() };
        let saved = self.ctx;
        if expression {
            // A function expression's name is checked in its own context.
            self.ctx.yield_ = is_generator;
            self.ctx.await_ = is_async;
            self.ctx.static_block = false;
        }
        let r = self.check_identifier(&n, true, pos);
        self.ctx = saved;
        r?;
        self.next()?;
        Ok((n, pos))
    }

    /// Parameters and body of a function, after its name.
    fn function_rest(
        &mut self,
        name: Option<(Name, Pos)>,
        kind: FnKind,
        is_async: bool,
        is_generator: bool,
        start: u32,
        pos: Pos,
    ) -> PResult<Rc<Function>> {
        let saved = self.ctx;
        let saved_labels = core::mem::take(&mut self.labels);
        let saved_loop = core::mem::replace(&mut self.loop_depth, 0);
        let saved_switch = core::mem::replace(&mut self.switch_depth, 0);
        let is_method = matches!(kind, FnKind::Method | FnKind::Getter | FnKind::Setter | FnKind::Constructor { .. } | FnKind::ClassInit);
        self.ctx = Ctx {
            strict: saved.strict,
            yield_: is_generator,
            await_: is_async,
            ret: true,
            super_call: matches!(kind, FnKind::Constructor { derived: true }),
            super_prop: is_method,
            new_target: true,
            no_arguments: false,
            static_block: false,
            in_params: true,
        };
        let r = self.function_params_and_body(name, kind, is_async, is_generator, start, pos);
        self.ctx = saved;
        self.labels = saved_labels;
        self.loop_depth = saved_loop;
        self.switch_depth = saved_switch;
        r
    }

    fn function_params_and_body(
        &mut self,
        name: Option<(Name, Pos)>,
        kind: FnKind,
        is_async: bool,
        is_generator: bool,
        start: u32,
        pos: Pos,
    ) -> PResult<Rc<Function>> {
        self.expect(P::LParen)?;
        let mut params = Vec::new();
        let mut rest = None;
        self.formal_params(&mut params, &mut rest)?;
        match kind {
            FnKind::Getter if !params.is_empty() || rest.is_some() => return self.err_at(pos, "getter must not have parameters"),
            FnKind::Setter if params.len() != 1 || rest.is_some() => return self.err_at(pos, "setter must have exactly one parameter"),
            _ => {}
        }
        self.ctx.in_params = false;
        let simple = rest.is_none() && params.iter().all(|p| p.default.is_none() && matches!(p.target, Pattern::Ident(..)));
        self.expect(P::LBrace)?;
        let was_strict = self.ctx.strict;
        let mut body = Vec::new();
        let became_strict = self.directives(&mut body, None)?;
        if became_strict && !simple {
            return self.err_at(pos, "\"use strict\" not allowed in function with non-simple parameters");
        }
        let strict = self.ctx.strict;
        if strict && !was_strict {
            // Retroactive checks on the name and parameters.
            if let Some((n, p)) = &name {
                if &**n == "eval" || &**n == "arguments" {
                    return self.err_at(*p, &format!("cannot bind '{n}' in strict mode"));
                }
                if STRICT_RESERVED.contains(&&**n) {
                    return self.err_at(*p, &format!("'{n}' is reserved in strict mode"));
                }
            }
        }
        while !self.eat(P::RBrace)? {
            if self.tok.tok == Tok::Eof {
                return self.unexpected();
            }
            body.push(self.statement_list_item()?);
        }
        let end = self.prev_end;
        self.check_params(&params, &rest, strict, simple, kind == FnKind::Arrow || matches!(kind, FnKind::Method | FnKind::Getter | FnKind::Setter | FnKind::Constructor { .. }))?;
        Ok(Rc::new(Function {
            name: name.map(|(n, _)| n),
            kind,
            is_async,
            is_generator,
            params,
            rest,
            body,
            expression_body: false,
            strict,
            simple_params: simple,
            start,
            end,
            pos,
        }))
    }

    /// Duplicate and strict-mode parameter names.
    fn check_params(&self, params: &[Param], rest: &Option<Pattern>, strict: bool, simple: bool, unique: bool) -> PResult<()> {
        let mut names = Vec::new();
        for p in params {
            bound_names(&p.target, &mut names);
        }
        if let Some(r) = rest {
            bound_names(r, &mut names);
        }
        for (i, (n, p)) in names.iter().enumerate() {
            if strict {
                if &**n == "eval" || &**n == "arguments" {
                    return self.err_at(*p, &format!("cannot bind '{n}' in strict mode"));
                }
                if STRICT_RESERVED.contains(&&**n) {
                    return self.err_at(*p, &format!("'{n}' is reserved in strict mode"));
                }
            }
            if (strict || !simple || unique) && names[..i].iter().any(|(m, _)| m == n) {
                return self.err_at(*p, &format!("duplicate parameter name '{n}'"));
            }
        }
        Ok(())
    }

    fn formal_params(&mut self, params: &mut Vec<Param>, rest: &mut Option<Pattern>) -> PResult<()> {
        while !self.eat(P::RParen)? {
            if self.eat(P::Ellipsis)? {
                let t = self.binding_target()?;
                if self.at(P::Assign) {
                    return self.err("rest parameter may not have a default initializer");
                }
                *rest = Some(t);
                if !self.at(P::RParen) {
                    return self.err("rest parameter must be last formal parameter");
                }
                continue;
            }
            let target = self.binding_target()?;
            let default = if self.eat(P::Assign)? { Some(self.assignment(false)?) } else { None };
            params.push(Param { target, default });
            if !self.at(P::RParen) {
                self.expect(P::Comma)?;
            }
        }
        Ok(())
    }

    fn formal_params_until_eof(&mut self, params: &mut Vec<Param>, rest: &mut Option<Pattern>) -> PResult<()> {
        while self.tok.tok != Tok::Eof {
            if self.eat(P::Ellipsis)? {
                *rest = Some(self.binding_target()?);
                if self.tok.tok != Tok::Eof {
                    return self.err("rest parameter must be last formal parameter");
                }
                break;
            }
            let target = self.binding_target()?;
            let default = if self.eat(P::Assign)? { Some(self.assignment(false)?) } else { None };
            params.push(Param { target, default });
            if self.tok.tok != Tok::Eof {
                self.expect(P::Comma)?;
            }
        }
        Ok(())
    }

    // ----- binding patterns ---------------------------------------------

    fn binding_target(&mut self) -> PResult<Pattern> {
        let pos = self.pos();
        match &self.tok.tok {
            Tok::Punct(P::LBracket) => {
                self.next()?;
                let mut elems = Vec::new();
                let mut rest = None;
                loop {
                    if self.eat(P::RBracket)? {
                        break;
                    }
                    if self.eat(P::Comma)? {
                        elems.push(None);
                        continue;
                    }
                    if self.eat(P::Ellipsis)? {
                        rest = Some(Box::new(self.binding_target()?));
                        if self.at(P::Assign) {
                            return self.err("rest element may not have a default initializer");
                        }
                        self.expect(P::RBracket)?;
                        break;
                    }
                    let target = self.binding_target()?;
                    let default = if self.eat(P::Assign)? { Some(self.assignment(false)?) } else { None };
                    elems.push(Some(PatElem { target, default }));
                    if !self.at(P::RBracket) {
                        self.expect(P::Comma)?;
                    }
                }
                Ok(Pattern::Array { elems, rest, pos })
            }
            Tok::Punct(P::LBrace) => {
                self.next()?;
                let mut props = Vec::new();
                let mut rest = None;
                while !self.eat(P::RBrace)? {
                    if self.eat(P::Ellipsis)? {
                        let (n, p) = self.binding_identifier()?;
                        rest = Some(Box::new(Pattern::Ident(n, p)));
                        self.expect(P::RBrace)?;
                        break;
                    }
                    let kpos = self.pos();
                    let short = match (&self.tok.tok, self.peek()?.tok) {
                        (Tok::Ident(n), Tok::Punct(P::Comma) | Tok::Punct(P::RBrace) | Tok::Punct(P::Assign)) => Some(n.clone()),
                        _ => None,
                    };
                    if let Some(n) = short {
                        self.check_identifier(&n, true, kpos)?;
                        self.next()?;
                        let default = if self.eat(P::Assign)? { Some(self.assignment(false)?) } else { None };
                        props.push(PatProp { key: PropKey::Name(utf16(&n)), target: Pattern::Ident(n, kpos), default });
                    } else {
                        let key = self.property_key(false)?;
                        self.expect(P::Colon)?;
                        let target = self.binding_target()?;
                        let default = if self.eat(P::Assign)? { Some(self.assignment(false)?) } else { None };
                        props.push(PatProp { key, target, default });
                    }
                    if !self.at(P::RBrace) {
                        self.expect(P::Comma)?;
                    }
                }
                Ok(Pattern::Object { props, rest, pos })
            }
            _ => {
                let (n, p) = self.binding_identifier()?;
                Ok(Pattern::Ident(n, p))
            }
        }
    }

    // ----- expressions ---------------------------------------------------

    pub fn expression(&mut self, no_in: bool) -> PResult<Expr> {
        let pos = self.pos();
        let first = self.assignment(no_in)?;
        if !self.at(P::Comma) {
            return Ok(first);
        }
        let mut items = vec![first];
        while self.eat(P::Comma)? {
            items.push(self.assignment(no_in)?);
        }
        Ok(Expr::new(ExprKind::Seq(items), pos))
    }

    fn is_assign_op(&self) -> Option<AssignOp> {
        let Tok::Punct(p) = self.tok.tok else { return None };
        Some(match p {
            P::Assign => AssignOp::Assign,
            P::PlusEq => AssignOp::Op(BinaryOp::Add),
            P::MinusEq => AssignOp::Op(BinaryOp::Sub),
            P::StarEq => AssignOp::Op(BinaryOp::Mul),
            P::SlashEq => AssignOp::Op(BinaryOp::Div),
            P::PercentEq => AssignOp::Op(BinaryOp::Mod),
            P::StarStarEq => AssignOp::Op(BinaryOp::Exp),
            P::ShlEq => AssignOp::Op(BinaryOp::Shl),
            P::ShrEq => AssignOp::Op(BinaryOp::Shr),
            P::UshrEq => AssignOp::Op(BinaryOp::Ushr),
            P::AmpEq => AssignOp::Op(BinaryOp::BitAnd),
            P::PipeEq => AssignOp::Op(BinaryOp::BitOr),
            P::CaretEq => AssignOp::Op(BinaryOp::BitXor),
            P::AmpAmpEq => AssignOp::Logical(LogicalOp::And),
            P::PipePipeEq => AssignOp::Logical(LogicalOp::Or),
            P::Question2Eq => AssignOp::Logical(LogicalOp::Nullish),
            _ => return None,
        })
    }

    pub fn assignment(&mut self, no_in: bool) -> PResult<Expr> {
        let pos = self.pos();
        // yield
        if self.at_kw("yield") && self.ctx.yield_ {
            return self.yield_expr(no_in);
        }
        // async arrow: `async x => …`, `async (…) => …` handled in primary/call.
        if self.at_kw("async") && !self.tok.escaped {
            let n = self.peek()?;
            if let Tok::Ident(name) = &n.tok {
                if !n.nl_before && &**name != "function" {
                    // `async x => body`
                    let mut lx = self.lx.clone();
                    lx.next()?;
                    let a = lx.next()?;
                    if a.tok == Tok::Punct(P::Arrow) && !a.nl_before {
                        let start = self.tok.start;
                        self.next()?;
                        let ppos = self.pos();
                        let saved = self.ctx;
                        self.ctx.await_ = true;
                        let r = self.binding_identifier();
                        self.ctx = saved;
                        let (pname, ppos2) = r?;
                        let _ = ppos;
                        let params = vec![Param { target: Pattern::Ident(pname, ppos2), default: None }];
                        return self.arrow_body(params, None, true, start, pos, no_in);
                    }
                }
            }
        }
        let start = self.tok.start;
        let saved_cover = self.pending_cover_init.take();
        let saved_ya = self.saw_yield_await.take();
        let starts_paren = self.at(P::LParen);
        let lhs = self.conditional(no_in)?;
        let ya = self.saw_yield_await.take();
        self.saw_yield_await = saved_ya.or(ya);

        // Arrow function: `x => …` or `(…) => …` (cover list in Paren/Cover).
        if self.at(P::Arrow) {
            if self.tok.nl_before {
                return self.err("line terminator not permitted before arrow");
            }
            let cover = self.pending_cover_init.take();
            self.pending_cover_init = saved_cover;
            let _ = cover;
            return match lhs.kind {
                ExprKind::Ident(n) if !starts_paren => {
                    self.check_identifier(&n, true, lhs.pos)?;
                    let params = vec![Param { target: Pattern::Ident(n, lhs.pos), default: None }];
                    self.arrow_body(params, None, false, start, pos, no_in)
                }
                ExprKind::Call { .. } | ExprKind::Paren(_) | ExprKind::Seq(_) | ExprKind::Array(_) => self.unexpected(),
                _ => self.unexpected(),
            };
        }

        if let Some(op) = self.is_assign_op() {
            let target = if op == AssignOp::Assign {
                let t = self.to_assign_target(lhs, true)?;
                self.pending_cover_init = saved_cover;
                t
            } else {
                if let Some(p) = self.pending_cover_init.take() {
                    return self.err_at(p, "invalid shorthand property initializer");
                }
                self.pending_cover_init = saved_cover;
                self.to_simple_target(lhs)?
            };
            self.next()?;
            let value = self.assignment(no_in)?;
            return Ok(Expr::new(ExprKind::Assign(op, Box::new(target), Box::new(value)), pos));
        }
        // Not an assignment: a pending `{a = 1}` is an error, unless an
        // enclosing pattern conversion will still consume it.
        if let Some(p) = self.pending_cover_init.take() {
            if saved_cover.is_none() && !self.cover_allowed_here() {
                return self.err_at(p, "invalid shorthand property initializer");
            }
            self.pending_cover_init = Some(saved_cover.unwrap_or(p));
        } else {
            self.pending_cover_init = saved_cover;
        }
        Ok(lhs)
    }

    /// A `{a = 1}` may still become a pattern when we are inside an array
    /// or object literal, or a parenthesized list (arrow parameters).
    fn cover_allowed_here(&self) -> bool {
        self.cover_depth > 0
    }

    fn yield_expr(&mut self, no_in: bool) -> PResult<Expr> {
        let pos = self.pos();
        if self.ctx.in_params {
            return self.err("yield expression not allowed in formal parameters");
        }
        self.next()?;
        self.saw_yield_await.get_or_insert(pos);
        let delegate;
        let arg = if self.tok.nl_before {
            delegate = false;
            None
        } else if self.eat(P::Star)? {
            delegate = true;
            Some(Box::new(self.assignment(no_in)?))
        } else {
            delegate = false;
            match &self.tok.tok {
                Tok::Punct(P::RParen | P::RBracket | P::RBrace | P::Comma | P::Semi | P::Colon) | Tok::Eof => None,
                Tok::Ident(n) if &**n == "in" && !self.tok.escaped => None,
                Tok::Punct(P::Question) => None,
                _ => Some(Box::new(self.assignment(no_in)?)),
            }
        };
        Ok(Expr::new(ExprKind::Yield(arg, delegate), pos))
    }

    fn arrow_body(&mut self, params: Vec<Param>, rest: Option<Pattern>, is_async: bool, start: u32, pos: Pos, no_in: bool) -> PResult<Expr> {
        self.expect(P::Arrow)?;
        let saved = self.ctx;
        let saved_labels = core::mem::take(&mut self.labels);
        let saved_loop = core::mem::replace(&mut self.loop_depth, 0);
        let saved_switch = core::mem::replace(&mut self.switch_depth, 0);
        let saved_cover = core::mem::replace(&mut self.cover_depth, 0);
        self.ctx.yield_ = false;
        self.ctx.await_ = is_async || (self.module && false);
        if is_async {
            self.ctx.await_ = true;
        }
        self.ctx.ret = true;
        self.ctx.in_params = false;
        // Arrows inherit super/new.target/arguments rules.
        let r = (|| -> PResult<Rc<Function>> {
            let simple = rest.is_none() && params.iter().all(|p| p.default.is_none() && matches!(p.target, Pattern::Ident(..)));
            if self.at(P::LBrace) {
                self.next()?;
                let was_strict = self.ctx.strict;
                let mut body = Vec::new();
                let became = self.directives(&mut body, None)?;
                if became && !simple {
                    return self.err_at(pos, "\"use strict\" not allowed in function with non-simple parameters");
                }
                while !self.eat(P::RBrace)? {
                    if self.tok.tok == Tok::Eof {
                        return self.unexpected();
                    }
                    body.push(self.statement_list_item()?);
                }
                let strict = self.ctx.strict;
                let _ = was_strict;
                self.check_params(&params, &rest, strict, simple, true)?;
                Ok(Rc::new(Function {
                    name: None,
                    kind: FnKind::Arrow,
                    is_async,
                    is_generator: false,
                    params,
                    rest,
                    body,
                    expression_body: false,
                    strict,
                    simple_params: simple,
                    start,
                    end: self.prev_end,
                    pos,
                }))
            } else {
                let bpos = self.pos();
                let e = self.assignment(no_in)?;
                let strict = self.ctx.strict;
                self.check_params(&params, &rest, strict, simple, true)?;
                Ok(Rc::new(Function {
                    name: None,
                    kind: FnKind::Arrow,
                    is_async,
                    is_generator: false,
                    params,
                    rest,
                    body: vec![Stmt { kind: StmtKind::Return(Some(e)), pos: bpos }],
                    expression_body: true,
                    strict,
                    simple_params: simple,
                    start,
                    end: self.prev_end,
                    pos,
                }))
            }
        })();
        self.ctx = saved;
        self.labels = saved_labels;
        self.loop_depth = saved_loop;
        self.switch_depth = saved_switch;
        self.cover_depth = saved_cover;
        let f = r?;
        Ok(Expr::new(ExprKind::Function(f), pos))
    }

    fn conditional(&mut self, no_in: bool) -> PResult<Expr> {
        let pos = self.pos();
        let test = self.binary(0, no_in)?;
        if !self.at(P::Question) {
            return Ok(test);
        }
        self.next()?;
        let cons = self.assignment(false)?;
        self.expect(P::Colon)?;
        let alt = self.assignment(no_in)?;
        Ok(Expr::new(ExprKind::Cond(Box::new(test), Box::new(cons), Box::new(alt)), pos))
    }

    fn binary_op(&self, no_in: bool) -> Option<(u8, Result<BinaryOp, LogicalOp>)> {
        match &self.tok.tok {
            Tok::Punct(p) => Some(match p {
                P::Question2 => (1, Err(LogicalOp::Nullish)),
                P::PipePipe => (2, Err(LogicalOp::Or)),
                P::AmpAmp => (3, Err(LogicalOp::And)),
                P::Pipe => (4, Ok(BinaryOp::BitOr)),
                P::Caret => (5, Ok(BinaryOp::BitXor)),
                P::Amp => (6, Ok(BinaryOp::BitAnd)),
                P::Eq2 => (7, Ok(BinaryOp::Eq)),
                P::Ne => (7, Ok(BinaryOp::Ne)),
                P::Eq3 => (7, Ok(BinaryOp::StrictEq)),
                P::Ne2 => (7, Ok(BinaryOp::StrictNe)),
                P::Lt => (8, Ok(BinaryOp::Lt)),
                P::Gt => (8, Ok(BinaryOp::Gt)),
                P::Le => (8, Ok(BinaryOp::Le)),
                P::Ge => (8, Ok(BinaryOp::Ge)),
                P::Shl => (9, Ok(BinaryOp::Shl)),
                P::Shr => (9, Ok(BinaryOp::Shr)),
                P::Ushr => (9, Ok(BinaryOp::Ushr)),
                P::Plus => (10, Ok(BinaryOp::Add)),
                P::Minus => (10, Ok(BinaryOp::Sub)),
                P::Star => (11, Ok(BinaryOp::Mul)),
                P::Slash => (11, Ok(BinaryOp::Div)),
                P::Percent => (11, Ok(BinaryOp::Mod)),
                P::StarStar => (12, Ok(BinaryOp::Exp)),
                _ => return None,
            }),
            Tok::Ident(n) if !self.tok.escaped => match &**n {
                "instanceof" => Some((8, Ok(BinaryOp::InstanceOf))),
                "in" if !no_in => Some((8, Ok(BinaryOp::In))),
                _ => None,
            },
            _ => None,
        }
    }

    /// Operator-precedence parsing of binary and logical expressions.
    fn binary(&mut self, min: u8, no_in: bool) -> PResult<Expr> {
        let pos = self.pos();
        // `#x in obj`
        let mut left = if let Tok::Private(name) = &self.tok.tok {
            let name = name.clone();
            let n = self.peek()?;
            if !(matches!(&n.tok, Tok::Ident(k) if &**k == "in") && !no_in && min <= 8) {
                return self.unexpected();
            }
            self.use_private(&name, pos)?;
            self.next()?;
            self.next()?;
            let rhs = self.binary(9, no_in)?;
            Expr::new(ExprKind::PrivateIn(name, Box::new(rhs)), pos)
        } else {
            self.unary()?
        };
        let mut seen_nullish = false;
        let mut seen_andor = false;
        loop {
            let Some((prec, op)) = self.binary_op(no_in) else { break };
            // Operators binding weaker than `min` end this operand; equal
            // ones are taken here (left-associative: the right operand is
            // parsed with min = prec + 1; `**` recurses with min = 12).
            if prec < min {
                break;
            }
            if prec == 12 {
                // `-x ** y` is an error.
                if let ExprKind::Unary(..) | ExprKind::Await(_) = left.kind {
                    return self.err("unparenthesized unary expression can't appear on the left-hand side of '**'");
                }
            }
            match op {
                Err(LogicalOp::Nullish) => {
                    if seen_andor {
                        return self.err("cannot mix ?? with || or && without parentheses");
                    }
                    seen_nullish = true;
                }
                Err(_) => {
                    if seen_nullish {
                        return self.err("cannot mix ?? with || or && without parentheses");
                    }
                    seen_andor = true;
                }
                _ => {}
            }
            self.next()?;
            let next_min = if prec == 12 { 12 } else { prec + 1 };
            let right = self.binary(next_min, no_in)?;
            if matches!(op, Err(LogicalOp::Nullish)) {
                if let ExprKind::Logical(LogicalOp::And | LogicalOp::Or, ..) = right.kind {
                    return self.err("cannot mix ?? with || or && without parentheses");
                }
            }
            if let Err(LogicalOp::And | LogicalOp::Or) = op {
                if let ExprKind::Logical(LogicalOp::Nullish, ..) = right.kind {
                    return self.err("cannot mix ?? with || or && without parentheses");
                }
            }
            left = match op {
                Ok(b) => Expr::new(ExprKind::Binary(b, Box::new(left), Box::new(right)), pos),
                Err(l) => Expr::new(ExprKind::Logical(l, Box::new(left), Box::new(right)), pos),
            };
            if let ExprKind::Logical(LogicalOp::Nullish, ref l, _) = left.kind {
                if let ExprKind::Logical(LogicalOp::And | LogicalOp::Or, ..) = l.kind {
                    return self.err("cannot mix ?? with || or && without parentheses");
                }
            }
        }
        Ok(left)
    }

    fn unary(&mut self) -> PResult<Expr> {
        let pos = self.pos();
        let op = match &self.tok.tok {
            Tok::Punct(P::Minus) => Some(UnaryOp::Minus),
            Tok::Punct(P::Plus) => Some(UnaryOp::Plus),
            Tok::Punct(P::Bang) => Some(UnaryOp::Not),
            Tok::Punct(P::Tilde) => Some(UnaryOp::BitNot),
            Tok::Ident(n) if !self.tok.escaped => match &**n {
                "typeof" => Some(UnaryOp::Typeof),
                "void" => Some(UnaryOp::Void),
                "delete" => Some(UnaryOp::Delete),
                _ => None,
            },
            _ => None,
        };
        if let Some(op) = op {
            self.next()?;
            let arg = self.unary()?;
            if op == UnaryOp::Delete {
                if self.ctx.strict {
                    if let ExprKind::Ident(_) = arg.unparen().kind {
                        return self.err_at(pos, "delete of an unqualified identifier in strict mode");
                    }
                }
                if let ExprKind::Member { prop: MemberProp::Private(_), .. } = &arg.unparen().kind {
                    return self.err_at(pos, "private fields can not be deleted");
                }
                if let ExprKind::OptionalChain(inner) = &arg.unparen().kind {
                    if let ExprKind::Member { prop: MemberProp::Private(_), .. } = &inner.kind {
                        return self.err_at(pos, "private fields can not be deleted");
                    }
                }
            }
            return Ok(Expr::new(ExprKind::Unary(op, Box::new(arg)), pos));
        }
        if self.at(P::PlusPlus) || self.at(P::MinusMinus) {
            let inc = self.at(P::PlusPlus);
            self.next()?;
            let arg = self.unary()?;
            let arg = self.check_update_target(arg)?;
            return Ok(Expr::new(ExprKind::Update(inc, true, Box::new(arg)), pos));
        }
        if self.at_kw("await") && self.ctx.await_ {
            if self.ctx.in_params {
                return self.err("await expression not allowed in formal parameters");
            }
            self.next()?;
            self.saw_yield_await.get_or_insert(pos);
            let arg = self.unary()?;
            return Ok(Expr::new(ExprKind::Await(Box::new(arg)), pos));
        }
        let e = self.lhs()?;
        if (self.at(P::PlusPlus) || self.at(P::MinusMinus)) && !self.tok.nl_before {
            let inc = self.at(P::PlusPlus);
            self.next()?;
            let e = self.check_update_target(e)?;
            return Ok(Expr::new(ExprKind::Update(inc, false, Box::new(e)), pos));
        }
        Ok(e)
    }

    fn check_update_target(&self, e: Expr) -> PResult<Expr> {
        match &e.unparen().kind {
            ExprKind::Ident(n) => {
                if self.ctx.strict && (&**n == "eval" || &**n == "arguments") {
                    return self.err_at(e.pos, "invalid update target");
                }
                Ok(e)
            }
            ExprKind::Member { optional: false, .. } | ExprKind::SuperMember(_) => Ok(e),
            ExprKind::Call { .. } if !self.ctx.strict => Ok(e),
            _ => self.err_at(e.pos, "invalid left-hand side expression in update operation"),
        }
    }

    /// LeftHandSideExpression: new / call / member / optional chains.
    fn lhs(&mut self) -> PResult<Expr> {
        let pos = self.pos();
        let mut e = if self.at_kw("new") {
            self.new_expr()?
        } else if self.at_kw("super") {
            self.super_expr()?
        } else if self.at_kw("import") {
            self.import_expr()?
        } else {
            self.primary()?
        };
        let mut in_chain = false;
        loop {
            match &self.tok.tok {
                Tok::Punct(P::Dot) => {
                    self.next()?;
                    let prop = self.member_name()?;
                    e = Expr::new(ExprKind::Member { obj: Box::new(e), prop, optional: false }, pos);
                }
                Tok::Punct(P::QuestionDot) => {
                    self.next()?;
                    in_chain = true;
                    if self.at(P::LParen) {
                        let args = self.arguments()?;
                        e = Expr::new(ExprKind::Call { callee: Box::new(e), args, optional: true }, pos);
                    } else if self.eat(P::LBracket)? {
                        let k = self.expression(false)?;
                        self.expect(P::RBracket)?;
                        e = Expr::new(ExprKind::Member { obj: Box::new(e), prop: MemberProp::Computed(Box::new(k)), optional: true }, pos);
                    } else if let Tok::Template { .. } = self.tok.tok {
                        return self.err("tagged template cannot be used in optional chain");
                    } else {
                        let prop = self.member_name()?;
                        e = Expr::new(ExprKind::Member { obj: Box::new(e), prop, optional: true }, pos);
                    }
                }
                Tok::Punct(P::LBracket) => {
                    self.next()?;
                    let k = self.expression(false)?;
                    self.expect(P::RBracket)?;
                    e = Expr::new(ExprKind::Member { obj: Box::new(e), prop: MemberProp::Computed(Box::new(k)), optional: false }, pos);
                }
                Tok::Punct(P::LParen) => {
                    // `async (…) => …`
                    let is_async_head = !in_chain
                        && matches!(&e.kind, ExprKind::Ident(n) if &**n == "async")
                        && !self.tok.nl_before
                        && self.lx.slice(e.pos.offset, e.pos.offset + 5) == "async";
                    if is_async_head {
                        let start = e.pos.offset;
                        let cover = self.paren_cover_list(true)?;
                        if self.at(P::Arrow) && !self.tok.nl_before {
                            if let Some(p) = cover.has_yield_await {
                                return self.err_at(p, "await/yield expression not allowed in arrow parameters");
                            }
                            let (params, rest) = self.cover_to_params(cover, true)?;
                            return self.arrow_body(params, rest, true, start, pos, false);
                        }
                        if let Some(p) = cover.cover_init {
                            return self.err_at(p, "invalid shorthand property initializer");
                        }
                        let mut args: Vec<Arg> = cover.items.into_iter().map(Arg::Expr).collect();
                        if let Some((r, _)) = cover.rest {
                            args.push(Arg::Spread(r));
                        }
                        e = Expr::new(ExprKind::Call { callee: Box::new(e), args, optional: false }, pos);
                        continue;
                    }
                    let args = self.arguments()?;
                    e = Expr::new(ExprKind::Call { callee: Box::new(e), args, optional: false }, pos);
                }
                Tok::Template { .. } => {
                    if in_chain {
                        return self.err("tagged template cannot be used in optional chain");
                    }
                    let (quasis, exprs) = self.template(true)?;
                    e = Expr::new(ExprKind::Tagged { tag: Box::new(e), quasis: Rc::new(quasis), exprs }, pos);
                }
                _ => break,
            }
        }
        if in_chain {
            e = Expr::new(ExprKind::OptionalChain(Box::new(e)), pos);
        }
        Ok(e)
    }

    fn member_name(&mut self) -> PResult<MemberProp> {
        match &self.tok.tok {
            Tok::Ident(n) => {
                let n = n.clone();
                self.next()?;
                Ok(MemberProp::Name(n))
            }
            Tok::Private(n) => {
                let n = n.clone();
                let pos = self.pos();
                self.use_private(&n, pos)?;
                self.next()?;
                Ok(MemberProp::Private(n))
            }
            _ => self.unexpected(),
        }
    }

    fn new_expr(&mut self) -> PResult<Expr> {
        let pos = self.pos();
        self.next()?;
        if self.eat(P::Dot)? {
            if self.at_kw("target") {
                if !self.ctx.new_target {
                    return self.err("new.target expression is not allowed here");
                }
                self.next()?;
                return Ok(Expr::new(ExprKind::NewTarget, pos));
            }
            return self.unexpected();
        }
        // The callee: a MemberExpression (no call, no optional chain).
        let mut callee = if self.at_kw("new") {
            self.new_expr()?
        } else if self.at_kw("super") {
            self.super_expr()?
        } else if self.at_kw("import") {
            let ipos = self.pos();
            let n = self.peek()?;
            if n.tok == Tok::Punct(P::LParen) {
                return self.err_at(ipos, "cannot use new with import()");
            }
            self.import_expr()?
        } else {
            self.primary()?
        };
        loop {
            match &self.tok.tok {
                Tok::Punct(P::Dot) => {
                    self.next()?;
                    let prop = self.member_name()?;
                    callee = Expr::new(ExprKind::Member { obj: Box::new(callee), prop, optional: false }, pos);
                }
                Tok::Punct(P::LBracket) => {
                    self.next()?;
                    let k = self.expression(false)?;
                    self.expect(P::RBracket)?;
                    callee = Expr::new(ExprKind::Member { obj: Box::new(callee), prop: MemberProp::Computed(Box::new(k)), optional: false }, pos);
                }
                Tok::Template { .. } => {
                    let (quasis, exprs) = self.template(true)?;
                    callee = Expr::new(ExprKind::Tagged { tag: Box::new(callee), quasis: Rc::new(quasis), exprs }, pos);
                }
                Tok::Punct(P::QuestionDot) => return self.err("invalid optional chain from new expression"),
                _ => break,
            }
        }
        let args = if self.at(P::LParen) { self.arguments()? } else { Vec::new() };
        Ok(Expr::new(ExprKind::New(Box::new(callee), args), pos))
    }

    fn super_expr(&mut self) -> PResult<Expr> {
        let pos = self.pos();
        self.next()?;
        match &self.tok.tok {
            Tok::Punct(P::LParen) => {
                if !self.ctx.super_call {
                    return self.err_at(pos, "'super' keyword unexpected here");
                }
                let args = self.arguments()?;
                Ok(Expr::new(ExprKind::SuperCall(args), pos))
            }
            Tok::Punct(P::Dot) => {
                if !self.ctx.super_prop {
                    return self.err_at(pos, "'super' keyword unexpected here");
                }
                self.next()?;
                match &self.tok.tok {
                    Tok::Ident(n) => {
                        let n = n.clone();
                        self.next()?;
                        Ok(Expr::new(ExprKind::SuperMember(MemberProp::Name(n)), pos))
                    }
                    _ => self.unexpected(),
                }
            }
            Tok::Punct(P::LBracket) => {
                if !self.ctx.super_prop {
                    return self.err_at(pos, "'super' keyword unexpected here");
                }
                self.next()?;
                let k = self.expression(false)?;
                self.expect(P::RBracket)?;
                Ok(Expr::new(ExprKind::SuperMember(MemberProp::Computed(Box::new(k))), pos))
            }
            _ => self.err_at(pos, "'super' keyword unexpected here"),
        }
    }

    fn import_expr(&mut self) -> PResult<Expr> {
        let pos = self.pos();
        self.next()?;
        if self.eat(P::Dot)? {
            if self.at_kw("meta") {
                if !self.module {
                    return self.err_at(pos, "import.meta may only appear in a module");
                }
                self.next()?;
                return Ok(Expr::new(ExprKind::ImportMeta, pos));
            }
            return self.unexpected();
        }
        self.expect(P::LParen)?;
        if self.at(P::RParen) {
            return self.unexpected();
        }
        let spec = self.assignment(false)?;
        let mut opts = None;
        if self.eat(P::Comma)? && !self.at(P::RParen) {
            opts = Some(Box::new(self.assignment(false)?));
            self.eat(P::Comma)?;
        }
        self.expect(P::RParen)?;
        Ok(Expr::new(ExprKind::ImportCall(Box::new(spec), opts), pos))
    }

    fn arguments(&mut self) -> PResult<Vec<Arg>> {
        self.expect(P::LParen)?;
        let mut args = Vec::new();
        while !self.eat(P::RParen)? {
            if self.eat(P::Ellipsis)? {
                args.push(Arg::Spread(self.assignment(false)?));
            } else {
                args.push(Arg::Expr(self.assignment(false)?));
            }
            if !self.at(P::RParen) {
                self.expect(P::Comma)?;
            }
        }
        Ok(args)
    }

    /// `( … )` that may be arrow parameters or a parenthesized expression
    /// (or, with `allow_call`, call arguments of `async`).
    fn paren_cover_list(&mut self, allow_call: bool) -> PResult<CoverList> {
        let _ = allow_call;
        self.expect(P::LParen)?;
        let mut out = CoverList { items: Vec::new(), rest: None, trailing_comma: false, cover_init: None, has_yield_await: None };
        self.cover_depth += 1;
        let saved_ya = self.saw_yield_await.take();
        let r = (|| -> PResult<()> {
            while !self.eat(P::RParen)? {
                if self.at(P::Ellipsis) {
                    let rpos = self.pos();
                    self.next()?;
                    let e = self.assignment(false)?;
                    out.rest = Some((e, rpos));
                    if !allow_call && !self.at(P::RParen) {
                        return self.err("rest parameter must be last formal parameter");
                    }
                    if allow_call && self.eat(P::Comma)? {
                        // `async(...a, b)` is a call; keep parsing as args.
                        let (r, _) = out.rest.take().unwrap();
                        out.items.push(Expr::new(ExprKind::Unary(UnaryOp::Void, Box::new(r)), rpos));
                        return self.err_at(rpos, "spread arguments in async call are not supported here");
                    }
                    continue;
                }
                let saved = self.pending_cover_init.take();
                let e = self.assignment(false)?;
                if let Some(p) = self.pending_cover_init.take() {
                    out.cover_init.get_or_insert(p);
                }
                self.pending_cover_init = saved;
                out.items.push(e);
                if !self.at(P::RParen) {
                    self.expect(P::Comma)?;
                    if self.at(P::RParen) {
                        out.trailing_comma = true;
                    }
                }
            }
            Ok(())
        })();
        self.cover_depth -= 1;
        out.has_yield_await = self.saw_yield_await.take();
        self.saw_yield_await = saved_ya.or(out.has_yield_await);
        r?;
        Ok(out)
    }

    fn cover_to_params(&mut self, cover: CoverList, is_async: bool) -> PResult<(Vec<Param>, Option<Pattern>)> {
        let saved = self.ctx;
        if is_async {
            self.ctx.await_ = true;
        }
        let r = (|| {
            let mut params = Vec::new();
            for e in cover.items {
                let (target, default) = match e.kind {
                    ExprKind::Assign(AssignOp::Assign, t, v) => {
                        let t = self.pattern_to_binding(*t)?;
                        (t, Some(*v))
                    }
                    _ => (self.expr_to_binding(e)?, None),
                };
                params.push(Param { target, default });
            }
            let rest = match cover.rest {
                Some((e, _)) => Some(self.expr_to_binding(e)?),
                None => None,
            };
            Ok((params, rest))
        })();
        self.ctx = saved;
        r
    }

    fn primary(&mut self) -> PResult<Expr> {
        let pos = self.pos();
        let t = self.tok.clone();
        match t.tok {
            Tok::Ident(ref n) => {
                if !t.escaped {
                    match &**n {
                        "function" => return self.function_expr(false, pos),
                        "class" => {
                            let c = self.class(false, false)?;
                            return Ok(Expr::new(ExprKind::Class(c), pos));
                        }
                        "this" => {
                            self.next()?;
                            return Ok(Expr::new(ExprKind::This, pos));
                        }
                        "null" => {
                            self.next()?;
                            return Ok(Expr::new(ExprKind::Null, pos));
                        }
                        "true" | "false" => {
                            self.next()?;
                            return Ok(Expr::new(ExprKind::Bool(&**n == "true"), pos));
                        }
                        "async" => {
                            let nt = self.peek()?;
                            if matches!(&nt.tok, Tok::Ident(k) if &**k == "function") && !nt.nl_before && !nt.escaped {
                                self.next()?;
                                return self.function_expr(true, pos);
                            }
                        }
                        "let" if self.ctx.strict => return self.err("'let' is reserved in strict mode"),
                        _ => {}
                    }
                }
                let n = n.clone();
                self.check_identifier(&n, false, pos)?;
                self.next()?;
                Ok(Expr::new(ExprKind::Ident(n), pos))
            }
            Tok::Num(v) => {
                if t.escaped && self.ctx.strict {
                    return self.err("octal literals are not allowed in strict mode");
                }
                self.next()?;
                Ok(Expr::new(ExprKind::Num(v), pos))
            }
            Tok::BigInt(ref s) => {
                let s = s.clone();
                self.next()?;
                Ok(Expr::new(ExprKind::BigInt(s), pos))
            }
            Tok::Str(ref s) => {
                if t.escaped && self.ctx.strict {
                    return self.err("octal escape sequences are not allowed in strict mode");
                }
                let s = s.clone();
                self.next()?;
                Ok(Expr::new(ExprKind::Str(s), pos))
            }
            Tok::Template { .. } => {
                let (quasis, exprs) = self.template(false)?;
                Ok(Expr::new(ExprKind::Template { quasis, exprs }, pos))
            }
            Tok::Punct(P::Slash) | Tok::Punct(P::SlashEq) => {
                let r = self.lx.rescan_regex(&t)?;
                self.tok = r;
                let Tok::Regex { body, flags } = self.tok.tok.clone() else { unreachable!() };
                validate_regex_flags(&flags).map_err(|m| SyntaxError { message: m.into(), line: pos.line, col: pos.col })?;
                let units: Vec<u16> = body.encode_utf16().collect();
                if let Err(m) = crate::regex::compile(&units, &flags) {
                    return Err(SyntaxError { message: format!("Invalid regular expression: /{body}/{flags}: {m}"), line: pos.line, col: pos.col });
                }
                self.next()?;
                Ok(Expr::new(ExprKind::Regex { body, flags }, pos))
            }
            Tok::Punct(P::LBracket) => self.array_literal(),
            Tok::Punct(P::LBrace) => self.object_literal(),
            Tok::Punct(P::LParen) => {
                let start = t.start;
                let cover = self.paren_cover_list(false)?;
                if self.at(P::Arrow) {
                    if let Some(p) = cover.has_yield_await {
                        return self.err_at(p, "yield/await expression not allowed in arrow parameters");
                    }
                    let (params, rest) = self.cover_to_params(cover, false)?;
                    return self.arrow_body_after_cover(params, rest, start, pos);
                }
                if cover.items.is_empty() || cover.rest.is_some() || cover.trailing_comma {
                    return self.unexpected();
                }
                if let Some(p) = cover.cover_init {
                    // May still be a pattern: `({a = 1}) => …` handled above;
                    // `({a = 1} = x)` is an assignment inside the parens.
                    return self.err_at(p, "invalid shorthand property initializer");
                }
                let inner = if cover.items.len() == 1 {
                    cover.items.into_iter().next().unwrap()
                } else {
                    let p = cover.items[0].pos;
                    Expr::new(ExprKind::Seq(cover.items), p)
                };
                Ok(Expr::new(ExprKind::Paren(Box::new(inner)), pos))
            }
            Tok::Private(_) => self.unexpected(),
            _ => self.unexpected(),
        }
    }

    fn arrow_body_after_cover(&mut self, params: Vec<Param>, rest: Option<Pattern>, start: u32, pos: Pos) -> PResult<Expr> {
        // The arrow body is parsed by `assignment`, which sees `=>` next;
        // parse it here directly. `no_in` does not reach inside parens.
        self.arrow_body(params, rest, false, start, pos, false)
    }

    fn function_expr(&mut self, is_async: bool, pos: Pos) -> PResult<Expr> {
        let start = pos.offset;
        self.expect_kw("function")?;
        let is_generator = self.eat(P::Star)?;
        let name = if let Tok::Ident(_) = &self.tok.tok {
            Some(self.binding_identifier_for_function(true, is_async, is_generator)?)
        } else {
            None
        };
        let f = self.function_rest(name, FnKind::Normal, is_async, is_generator, start, pos)?;
        Ok(Expr::new(ExprKind::Function(f), pos))
    }

    fn template(&mut self, tagged: bool) -> PResult<(Vec<TemplatePiece>, Vec<Expr>)> {
        let mut quasis = Vec::new();
        let mut exprs = Vec::new();
        loop {
            let t = self.next()?;
            let tpos = pos_of(&t);
            let Tok::Template { cooked, raw, tail } = t.tok else { return self.unexpected() };
            if cooked.is_none() && !tagged {
                return self.err_at(tpos, "invalid escape sequence in template");
            }
            quasis.push(TemplatePiece { cooked, raw });
            if tail {
                break;
            }
            exprs.push(self.expression(false)?);
            if !self.at(P::RBrace) {
                return self.unexpected();
            }
            let t = self.tok.clone();
            self.tok = self.lx.rescan_template(&t)?;
        }
        Ok((quasis, exprs))
    }

    fn array_literal(&mut self) -> PResult<Expr> {
        let pos = self.pos();
        self.next()?;
        let mut elems = Vec::new();
        self.cover_depth += 1;
        let r = (|| -> PResult<()> {
            loop {
                if self.eat(P::RBracket)? {
                    break;
                }
                if self.eat(P::Comma)? {
                    elems.push(ArrayElem::Hole);
                    continue;
                }
                if self.eat(P::Ellipsis)? {
                    elems.push(ArrayElem::Spread(self.assignment(false)?));
                } else {
                    elems.push(ArrayElem::Expr(self.assignment(false)?));
                }
                if !self.at(P::RBracket) {
                    self.expect(P::Comma)?;
                }
            }
            Ok(())
        })();
        self.cover_depth -= 1;
        r?;
        Ok(Expr::new(ExprKind::Array(elems), pos))
    }

    /// A property name: identifier name, string, number, bigint, `[expr]`
    /// or (in classes) a private name.
    fn property_key(&mut self, allow_private: bool) -> PResult<PropKey> {
        let pos = self.pos();
        let t = self.tok.clone();
        let k = match t.tok {
            Tok::Ident(n) => PropKey::Name(utf16(&n)),
            Tok::Str(s) => {
                if t.escaped && self.ctx.strict {
                    return self.err("octal escape sequences are not allowed in strict mode");
                }
                PropKey::Name(s)
            }
            Tok::Num(v) => {
                if t.escaped && self.ctx.strict {
                    return self.err("octal literals are not allowed in strict mode");
                }
                PropKey::Num(v)
            }
            Tok::BigInt(s) => PropKey::BigInt(s),
            Tok::Punct(P::LBracket) => {
                self.next()?;
                let e = self.assignment(false)?;
                self.expect(P::RBracket)?;
                return Ok(PropKey::Computed(Box::new(e)));
            }
            Tok::Private(n) if allow_private => {
                if &*n == "constructor" {
                    return self.err_at(pos, "classes may not have a private field named '#constructor'");
                }
                PropKey::Private(n)
            }
            _ => return self.unexpected(),
        };
        self.next()?;
        Ok(k)
    }

    fn object_literal(&mut self) -> PResult<Expr> {
        let pos = self.pos();
        self.next()?;
        let mut props = Vec::new();
        let mut has_proto = false;
        self.cover_depth += 1;
        let r = (|| -> PResult<()> {
            while !self.eat(P::RBrace)? {
                if self.eat(P::Ellipsis)? {
                    props.push(ObjProp::Spread(self.assignment(false)?));
                } else {
                    let p = self.object_property()?;
                    if let ObjProp::Proto(_) = &p {
                        if has_proto {
                            // Allowed only if this literal becomes a pattern;
                            // record like a cover initializer.
                            self.pending_cover_init.get_or_insert(pos);
                            self.dup_proto = Some(pos);
                        }
                        has_proto = true;
                    }
                    props.push(p);
                }
                if !self.at(P::RBrace) {
                    self.expect(P::Comma)?;
                }
            }
            Ok(())
        })();
        self.cover_depth -= 1;
        r?;
        Ok(Expr::new(ExprKind::Object(props), pos))
    }

    fn object_property(&mut self) -> PResult<ObjProp> {
        let pos = self.pos();
        let start = self.tok.start;
        // get/set/async/* prefixes
        let mut is_async = false;
        let mut is_gen = false;
        let mut kind = MethodKind::Method;
        if let Tok::Ident(n) = &self.tok.tok {
            if !self.tok.escaped {
                let n = n.clone();
                let next = self.peek()?;
                let is_key_next = !matches!(next.tok, Tok::Punct(P::Comma | P::Colon | P::LParen | P::RBrace | P::Assign));
                if is_key_next {
                    match &*n {
                        "get" | "set" => {
                            self.next()?;
                            kind = if &*n == "get" { MethodKind::Get } else { MethodKind::Set };
                        }
                        "async" if !next.nl_before => {
                            self.next()?;
                            is_async = true;
                            if self.eat(P::Star)? {
                                is_gen = true;
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
        if !is_async && kind == MethodKind::Method && self.eat(P::Star)? {
            is_gen = true;
        }
        let key_tok = self.tok.clone();
        let key = self.property_key(false)?;
        if is_async || is_gen || kind != MethodKind::Method || self.at(P::LParen) {
            let fkind = match kind {
                MethodKind::Get => FnKind::Getter,
                MethodKind::Set => FnKind::Setter,
                MethodKind::Method => FnKind::Method,
            };
            let func = self.function_rest(None, fkind, is_async, is_gen, start, pos)?;
            return Ok(ObjProp::Method { key, kind, func });
        }
        if self.eat(P::Colon)? {
            let value = self.assignment(false)?;
            if let PropKey::Name(n) = &key {
                if eq16(n, "__proto__") {
                    return Ok(ObjProp::Proto(value));
                }
            }
            return Ok(ObjProp::Value { key, value, shorthand: false });
        }
        // Shorthand `{ a }` or cover `{ a = 1 }`.
        let kpos = pos_of(&key_tok);
        let Tok::Ident(name) = key_tok.tok else { return self.unexpected() };
        if RESERVED.contains(&&*name) || (key_tok.escaped && false) {
            return self.err_at(kpos, &format!("unexpected reserved word '{name}'"));
        }
        self.check_identifier(&name, false, kpos)?;
        if self.at(P::Assign) {
            self.next()?;
            let init = self.assignment(false)?;
            self.pending_cover_init.get_or_insert(kpos);
            return Ok(ObjProp::CoverInit { name, init });
        }
        Ok(ObjProp::Value { key, value: Expr::new(ExprKind::Ident(name), kpos), shorthand: true })
    }

    // ----- classes -------------------------------------------------------

    fn class(&mut self, declaration: bool, default_export: bool) -> PResult<Rc<Class>> {
        let pos = self.pos();
        let start = self.tok.start;
        self.next()?;
        let saved = self.ctx;
        self.ctx.strict = true;
        let r = self.class_inner(declaration, default_export, pos, start);
        self.ctx = saved;
        r
    }

    fn class_inner(&mut self, declaration: bool, default_export: bool, pos: Pos, start: u32) -> PResult<Rc<Class>> {
        let name = if let Tok::Ident(n) = &self.tok.tok {
            if !(self.at_kw("extends")) {
                let n = n.clone();
                let npos = self.pos();
                self.check_identifier(&n, true, npos)?;
                self.next()?;
                Some(n)
            } else {
                None
            }
        } else {
            None
        };
        if declaration && name.is_none() && !default_export {
            return self.err("class name required");
        }
        let extends = if self.eat_kw("extends")? { Some(Box::new(self.lhs()?)) } else { None };
        self.expect(P::LBrace)?;
        self.classes.push(ClassScope { declared: Vec::new(), used: Vec::new() });
        let derived = extends.is_some();
        let mut members = Vec::new();
        let mut constructor = None;
        let r = (|| -> PResult<()> {
            while !self.eat(P::RBrace)? {
                if self.eat(P::Semi)? {
                    continue;
                }
                if let Some(m) = self.class_member(derived, &mut constructor)? {
                    members.push(m);
                }
            }
            Ok(())
        })();
        let scope = self.classes.pop().unwrap();
        r?;
        // Private names used but not declared here go to the outer class.
        for (n, p) in scope.used {
            if !scope.declared.iter().any(|(d, _)| *d == n) {
                match self.classes.last_mut() {
                    Some(outer) => outer.used.push((n, p)),
                    None => return self.err_at(p, &format!("private field '#{n}' must be declared in an enclosing class")),
                }
            }
        }
        Ok(Rc::new(Class { name, extends, constructor, members, start, end: self.prev_end, pos }))
    }

    fn declare_private(&mut self, name: &Name, kind: u8, pos: Pos) -> PResult<()> {
        // kind: 1 = field/method, 2 = getter, 3 = setter (a getter and a
        // setter pair may share a name); static flag in bit 2.
        let scope = self.classes.last_mut().unwrap();
        if let Some((_, k)) = scope.declared.iter().find(|(n, _)| n == name) {
            let (a, b) = (*k & 3, kind & 3);
            let same_static = (*k & 4) == (kind & 4);
            if !((a == 2 && b == 3 || a == 3 && b == 2) && same_static) {
                return self.err_at(pos, &format!("identifier '#{name}' has already been declared"));
            }
            // Mark the pair as complete.
            for (n, k) in scope.declared.iter_mut() {
                if n == name {
                    *k = 1 | (kind & 4);
                }
            }
            return Ok(());
        }
        scope.declared.push((name.clone(), kind));
        Ok(())
    }

    fn use_private(&mut self, name: &Name, pos: Pos) -> PResult<()> {
        match self.classes.last_mut() {
            Some(c) => {
                c.used.push((name.clone(), pos));
                Ok(())
            }
            None => {
                if self.private_ok_outside_class {
                    return Ok(());
                }
                self.err_at(pos, &format!("private field '#{name}' must be declared in an enclosing class"))
            }
        }
    }

    fn class_member(&mut self, derived: bool, constructor: &mut Option<Rc<Function>>) -> PResult<Option<ClassMember>> {
        let pos = self.pos();
        let start = self.tok.start;
        let mut is_static = false;
        // `static` as a modifier vs. a member named `static`.
        if self.at_kw("static") {
            let n = self.peek()?;
            if !matches!(n.tok, Tok::Punct(P::LParen | P::Assign | P::Semi | P::RBrace)) && !(n.nl_before && false) {
                self.next()?;
                is_static = true;
                if self.at(P::LBrace) {
                    // static block
                    let bpos = self.pos();
                    let f = self.static_block(bpos)?;
                    return Ok(Some(ClassMember::StaticBlock(f)));
                }
            }
        }
        let mut is_async = false;
        let mut is_gen = false;
        let mut kind = MethodKind::Method;
        let mut accessor = false;
        if let Tok::Ident(n) = &self.tok.tok {
            if !self.tok.escaped {
                let n = n.clone();
                let next = self.peek()?;
                let key_follows = !matches!(next.tok, Tok::Punct(P::LParen | P::Assign | P::Semi | P::RBrace)) && !(next.nl_before && matches!(&*n, "get" | "set" | "accessor" | "async") && !matches!(next.tok, Tok::Punct(P::Star) | Tok::Punct(P::LBracket) | Tok::Ident(_) | Tok::Private(_) | Tok::Str(_) | Tok::Num(_)));
                if key_follows {
                    match &*n {
                        "get" | "set" => {
                            self.next()?;
                            kind = if &*n == "get" { MethodKind::Get } else { MethodKind::Set };
                        }
                        "async" if !next.nl_before => {
                            self.next()?;
                            is_async = true;
                            if self.eat(P::Star)? {
                                is_gen = true;
                            }
                        }
                        "accessor" if !next.nl_before => {
                            self.next()?;
                            accessor = true;
                        }
                        _ => {}
                    }
                }
            }
        }
        if !is_async && kind == MethodKind::Method && !accessor && self.eat(P::Star)? {
            is_gen = true;
        }
        let kpos = self.pos();
        let key = self.property_key(true)?;
        let is_ctor_name = matches!(&key, PropKey::Name(n) if eq16(n, "constructor"));
        if self.at(P::LParen) && !accessor {
            let fkind = match kind {
                MethodKind::Get => FnKind::Getter,
                MethodKind::Set => FnKind::Setter,
                MethodKind::Method => {
                    if is_ctor_name && !is_static {
                        FnKind::Constructor { derived }
                    } else {
                        FnKind::Method
                    }
                }
            };
            if is_ctor_name && !is_static && (kind != MethodKind::Method || is_async || is_gen) {
                return self.err_at(kpos, "class constructor may not be an accessor, generator or async");
            }
            if is_static && matches!(&key, PropKey::Name(n) if eq16(n, "prototype")) {
                return self.err_at(kpos, "classes may not have a static property named 'prototype'");
            }
            if let PropKey::Private(n) = &key {
                let k = match kind {
                    MethodKind::Get => 2,
                    MethodKind::Set => 3,
                    MethodKind::Method => 1,
                } | if is_static { 4 } else { 0 };
                let n = n.clone();
                self.declare_private(&n, k, kpos)?;
            }
            let func = self.function_rest(None, fkind, is_async, is_gen, start, pos)?;
            if let FnKind::Constructor { .. } = fkind {
                if constructor.is_some() {
                    return self.err_at(kpos, "a class may only have one constructor");
                }
                *constructor = Some(func);
                return Ok(None);
            }
            return Ok(Some(ClassMember::Method { key, kind, is_static, func }));
        }
        if kind != MethodKind::Method || is_async || is_gen {
            return self.unexpected();
        }
        // Field
        if is_ctor_name || matches!(&key, PropKey::Name(n) if eq16(n, "constructor")) {
            return self.err_at(kpos, "classes may not have a field named 'constructor'");
        }
        if is_static && matches!(&key, PropKey::Name(n) if eq16(n, "prototype")) {
            return self.err_at(kpos, "classes may not have a static property named 'prototype'");
        }
        if let PropKey::Private(n) = &key {
            let n = n.clone();
            self.declare_private(&n, 1 | if is_static { 4 } else { 0 }, kpos)?;
        }
        let value = if self.eat(P::Assign)? { Some(self.field_initializer(kpos)?) } else { None };
        self.semicolon()?;
        Ok(Some(ClassMember::Field { key, is_static, value, accessor }))
    }

    fn field_initializer(&mut self, pos: Pos) -> PResult<Rc<Function>> {
        let saved = self.ctx;
        self.ctx = Ctx {
            strict: true,
            yield_: false,
            await_: false,
            ret: false,
            super_call: false,
            super_prop: true,
            new_target: true,
            no_arguments: true,
            static_block: false,
            in_params: false,
        };
        let start = self.tok.start;
        let r = self.assignment(false);
        self.ctx = saved;
        let e = r?;
        Ok(Rc::new(Function {
            name: None,
            kind: FnKind::Method,
            is_async: false,
            is_generator: false,
            params: Vec::new(),
            rest: None,
            body: vec![Stmt { kind: StmtKind::Return(Some(e)), pos }],
            expression_body: true,
            strict: true,
            simple_params: true,
            start,
            end: self.prev_end,
            pos,
        }))
    }

    fn static_block(&mut self, pos: Pos) -> PResult<Rc<Function>> {
        let saved = self.ctx;
        let saved_labels = core::mem::take(&mut self.labels);
        let saved_loop = core::mem::replace(&mut self.loop_depth, 0);
        let saved_switch = core::mem::replace(&mut self.switch_depth, 0);
        self.ctx = Ctx {
            strict: true,
            yield_: false,
            await_: false,
            ret: false,
            super_call: false,
            super_prop: true,
            new_target: true,
            no_arguments: true,
            static_block: true,
            in_params: false,
        };
        let start = self.tok.start;
        let r = self.block();
        self.ctx = saved;
        self.labels = saved_labels;
        self.loop_depth = saved_loop;
        self.switch_depth = saved_switch;
        let body = r?;
        Ok(Rc::new(Function {
            name: None,
            kind: FnKind::Method,
            is_async: false,
            is_generator: false,
            params: Vec::new(),
            rest: None,
            body,
            expression_body: false,
            strict: true,
            simple_params: true,
            start,
            end: self.prev_end,
            pos,
        }))
    }

    // ----- patterns from expressions ----------------------------------

    /// `lhs = …` / `for (lhs of …)`: an object/array literal becomes a
    /// pattern; otherwise a simple assignment target.
    fn to_assign_target(&mut self, e: Expr, allow_pattern: bool) -> PResult<Pattern> {
        match e.kind {
            ExprKind::Object(_) | ExprKind::Array(_) if allow_pattern => {
                self.dup_proto = None;
                self.pending_cover_init = None;
                self.expr_to_pattern(e)
            }
            _ => {
                if let Some(p) = self.pending_cover_init.take() {
                    return self.err_at(p, "invalid shorthand property initializer");
                }
                self.to_simple_target(e)
            }
        }
    }

    fn to_simple_target(&self, e: Expr) -> PResult<Pattern> {
        match &e.unparen().kind {
            ExprKind::Ident(n) => {
                if self.ctx.strict && (&**n == "eval" || &**n == "arguments") {
                    return self.err_at(e.pos, &format!("cannot assign to '{n}' in strict mode"));
                }
                let n = n.clone();
                Ok(Pattern::Ident(n, e.unparen().pos))
            }
            ExprKind::Member { optional: false, .. } | ExprKind::SuperMember(_) => Ok(Pattern::Expr(Box::new(e))),
            ExprKind::Call { .. } if !self.ctx.strict => Ok(Pattern::Expr(Box::new(e))),
            _ => self.err_at(e.pos, "invalid assignment target"),
        }
    }

    /// Converts an array/object literal (cover grammar) into an assignment
    /// pattern.
    fn expr_to_pattern(&self, e: Expr) -> PResult<Pattern> {
        let pos = e.pos;
        match e.kind {
            ExprKind::Array(elems) => {
                let mut out = Vec::new();
                let mut rest = None;
                let n = elems.len();
                for (i, el) in elems.into_iter().enumerate() {
                    match el {
                        ArrayElem::Hole => out.push(None),
                        ArrayElem::Expr(x) => out.push(Some(self.expr_to_pat_elem(x)?)),
                        ArrayElem::Spread(x) => {
                            if i + 1 != n {
                                return self.err_at(x.pos, "rest element must be last element");
                            }
                            if let ExprKind::Assign(..) = x.kind {
                                return self.err_at(x.pos, "rest element may not have a default initializer");
                            }
                            rest = Some(Box::new(self.expr_to_nested_target(x)?));
                        }
                    }
                }
                Ok(Pattern::Array { elems: out, rest, pos })
            }
            ExprKind::Object(props) => {
                let mut out = Vec::new();
                let mut rest = None;
                let n = props.len();
                for (i, p) in props.into_iter().enumerate() {
                    match p {
                        ObjProp::Value { key, value, shorthand } => {
                            if shorthand {
                                let ExprKind::Ident(name) = &value.kind else { unreachable!() };
                                if self.ctx.strict && (&**name == "eval" || &**name == "arguments") {
                                    return self.err_at(value.pos, &format!("cannot assign to '{name}' in strict mode"));
                                }
                                out.push(PatProp { key, target: Pattern::Ident(name.clone(), value.pos), default: None });
                            } else {
                                let el = self.expr_to_pat_elem(value)?;
                                out.push(PatProp { key, target: el.target, default: el.default });
                            }
                        }
                        ObjProp::Proto(value) => {
                            let el = self.expr_to_pat_elem(value)?;
                            out.push(PatProp { key: PropKey::Name(utf16("__proto__")), target: el.target, default: el.default });
                        }
                        ObjProp::CoverInit { name, init } => {
                            if self.ctx.strict && (&*name == "eval" || &*name == "arguments") {
                                return self.err_at(pos, &format!("cannot assign to '{name}' in strict mode"));
                            }
                            out.push(PatProp { key: PropKey::Name(utf16(&name)), target: Pattern::Ident(name, pos), default: Some(init) });
                        }
                        ObjProp::Spread(x) => {
                            if i + 1 != n {
                                return self.err_at(x.pos, "rest element must be last element");
                            }
                            match x.unparen().kind {
                                ExprKind::Ident(_) | ExprKind::Member { .. } | ExprKind::SuperMember(_) => {}
                                _ => return self.err_at(x.pos, "invalid rest element"),
                            }
                            rest = Some(Box::new(self.to_simple_target(x)?));
                        }
                        ObjProp::Method { .. } => return self.err_at(pos, "invalid destructuring assignment target"),
                    }
                }
                Ok(Pattern::Object { props: out, rest, pos })
            }
            _ => self.err_at(pos, "invalid destructuring assignment target"),
        }
    }

    fn expr_to_pat_elem(&self, e: Expr) -> PResult<PatElem> {
        match e.kind {
            ExprKind::Assign(AssignOp::Assign, target, value) => Ok(PatElem { target: *target, default: Some(*value) }),
            _ => Ok(PatElem { target: self.expr_to_nested_target(e)?, default: None }),
        }
    }

    fn expr_to_nested_target(&self, e: Expr) -> PResult<Pattern> {
        match e.kind {
            ExprKind::Object(_) | ExprKind::Array(_) => self.expr_to_pattern(e),
            _ => self.to_simple_target(e),
        }
    }

    /// Arrow parameters: an expression that must be a binding pattern.
    fn expr_to_binding(&self, e: Expr) -> PResult<Pattern> {
        let pos = e.pos;
        match e.kind {
            ExprKind::Ident(n) => {
                self.check_identifier(&n, true, pos)?;
                Ok(Pattern::Ident(n, pos))
            }
            ExprKind::Object(_) | ExprKind::Array(_) => {
                let p = self.expr_to_pattern(e)?;
                self.pattern_to_binding(p)
            }
            _ => self.err_at(pos, "invalid destructuring target in parameters"),
        }
    }

    /// Checks that an assignment pattern is also a valid binding pattern.
    fn pattern_to_binding(&self, p: Pattern) -> PResult<Pattern> {
        match p {
            Pattern::Ident(n, pos) => {
                self.check_identifier(&n, true, pos)?;
                Ok(Pattern::Ident(n, pos))
            }
            Pattern::Expr(e) => match e.kind {
                ExprKind::Object(_) | ExprKind::Array(_) => self.expr_to_binding(*e),
                _ => self.err_at(e.pos, "invalid destructuring target in parameters"),
            },
            Pattern::Array { elems, rest, pos } => {
                let mut out = Vec::new();
                for el in elems {
                    out.push(match el {
                        None => None,
                        Some(PatElem { target, default }) => Some(PatElem { target: self.pattern_to_binding(target)?, default }),
                    });
                }
                let rest = match rest {
                    Some(r) => Some(Box::new(self.pattern_to_binding(*r)?)),
                    None => None,
                };
                Ok(Pattern::Array { elems: out, rest, pos })
            }
            Pattern::Object { props, rest, pos } => {
                let mut out = Vec::new();
                for p in props {
                    out.push(PatProp { key: p.key, target: self.pattern_to_binding(p.target)?, default: p.default });
                }
                let rest = match rest {
                    Some(r) => Some(Box::new(self.pattern_to_binding(*r)?)),
                    None => None,
                };
                Ok(Pattern::Object { props: out, rest, pos })
            }
        }
    }

    // ----- modules -------------------------------------------------------

    fn module_item(&mut self) -> PResult<Stmt> {
        let pos = self.pos();
        if self.at_kw("import") {
            let n = self.peek()?;
            if !matches!(n.tok, Tok::Punct(P::LParen) | Tok::Punct(P::Dot)) {
                let imp = self.import_declaration()?;
                return Ok(Stmt { kind: StmtKind::Import(Box::new(imp)), pos });
            }
        }
        if self.at_kw("export") {
            let e = self.export_declaration()?;
            return Ok(Stmt { kind: StmtKind::Export(e), pos });
        }
        self.statement_list_item()
    }

    fn module_export_name(&mut self) -> PResult<Str16> {
        match &self.tok.tok {
            Tok::Ident(n) => {
                let s = utf16(n);
                self.next()?;
                Ok(s)
            }
            Tok::Str(s) => {
                let s = s.clone();
                if !is_well_formed(&s) {
                    return self.err("module export name must be a well-formed string");
                }
                self.next()?;
                Ok(s)
            }
            _ => self.unexpected(),
        }
    }

    fn from_clause(&mut self) -> PResult<Str16> {
        self.expect_kw("from")?;
        self.module_specifier()
    }

    fn module_specifier(&mut self) -> PResult<Str16> {
        match &self.tok.tok {
            Tok::Str(s) => {
                let s = s.clone();
                self.next()?;
                Ok(s)
            }
            _ => self.unexpected(),
        }
    }

    fn import_attributes(&mut self) -> PResult<Vec<(Str16, Str16)>> {
        let mut out = Vec::new();
        if (self.at_kw("with") || (self.at_kw("assert") && !self.tok.nl_before)) && !self.tok.nl_before {
            self.next()?;
            self.expect(P::LBrace)?;
            while !self.eat(P::RBrace)? {
                let k = match &self.tok.tok {
                    Tok::Ident(n) => utf16(n),
                    Tok::Str(s) => s.clone(),
                    _ => return self.unexpected(),
                };
                self.next()?;
                self.expect(P::Colon)?;
                let v = match &self.tok.tok {
                    Tok::Str(s) => s.clone(),
                    _ => return self.unexpected(),
                };
                self.next()?;
                if out.iter().any(|(x, _): &(Str16, Str16)| *x == k) {
                    return self.err("duplicate import attribute");
                }
                out.push((k, v));
                if !self.at(P::RBrace) {
                    self.expect(P::Comma)?;
                }
            }
        }
        Ok(out)
    }

    fn import_declaration(&mut self) -> PResult<Import> {
        let pos = self.pos();
        self.next()?;
        let mut imp = Import { specifier: utf16(""), default: None, namespace: None, named: Vec::new(), attributes: Vec::new(), pos };
        if let Tok::Str(_) = self.tok.tok {
            imp.specifier = self.module_specifier()?;
            imp.attributes = self.import_attributes()?;
            self.semicolon()?;
            return Ok(imp);
        }
        if let Tok::Ident(_) = self.tok.tok {
            if !self.at_kw("from") || {
                let n = self.peek()?;
                matches!(&n.tok, Tok::Ident(k) if &**k == "from") || n.tok == Tok::Punct(P::Comma)
            } {
                let (n, _) = self.binding_identifier()?;
                imp.default = Some(n);
                if !self.eat(P::Comma)? {
                    imp.specifier = self.from_clause()?;
                    imp.attributes = self.import_attributes()?;
                    self.semicolon()?;
                    return Ok(imp);
                }
            }
        }
        if self.eat(P::Star)? {
            self.expect_kw("as")?;
            let (n, _) = self.binding_identifier()?;
            imp.namespace = Some(n);
        } else if self.eat(P::LBrace)? {
            while !self.eat(P::RBrace)? {
                let ipos = self.pos();
                let is_str = matches!(self.tok.tok, Tok::Str(_));
                let ident_tok = self.tok.clone();
                let imported = self.module_export_name()?;
                let local = if self.eat_kw("as")? {
                    self.binding_identifier()?.0
                } else {
                    if is_str {
                        return self.err_at(ipos, "string import name requires 'as'");
                    }
                    let Tok::Ident(n) = ident_tok.tok else { unreachable!() };
                    self.check_identifier(&n, true, ipos)?;
                    n
                };
                imp.named.push((imported, local));
                if !self.at(P::RBrace) {
                    self.expect(P::Comma)?;
                }
            }
        } else {
            return self.unexpected();
        }
        imp.specifier = self.from_clause()?;
        imp.attributes = self.import_attributes()?;
        self.semicolon()?;
        Ok(imp)
    }

    fn export_declaration(&mut self) -> PResult<Export> {
        self.next()?;
        if self.eat(P::Star)? {
            let exported = if self.eat_kw("as")? { Some(self.module_export_name()?) } else { None };
            let from = self.from_clause()?;
            let attributes = self.import_attributes()?;
            self.semicolon()?;
            return Ok(Export::All { exported, from, attributes });
        }
        if self.eat_kw("default")? {
            let pos = self.pos();
            if self.at_kw("function") {
                let start = pos.offset;
                self.next()?;
                let is_gen = self.eat(P::Star)?;
                let name = if let Tok::Ident(_) = self.tok.tok { Some(self.binding_identifier_for_function(false, false, is_gen)?) } else { None };
                let f = self.function_rest(name, FnKind::Normal, false, is_gen, start, pos)?;
                return Ok(Export::DefaultFunction(f));
            }
            if self.at_kw("async") {
                let n = self.peek()?;
                if matches!(&n.tok, Tok::Ident(k) if &**k == "function") && !n.nl_before {
                    let start = pos.offset;
                    self.next()?;
                    self.next()?;
                    let is_gen = self.eat(P::Star)?;
                    let name = if let Tok::Ident(_) = self.tok.tok { Some(self.binding_identifier_for_function(false, true, is_gen)?) } else { None };
                    let f = self.function_rest(name, FnKind::Normal, true, is_gen, start, pos)?;
                    return Ok(Export::DefaultFunction(f));
                }
            }
            if self.at_kw("class") {
                let c = self.class(true, true)?;
                return Ok(Export::DefaultClass(c));
            }
            let e = self.assignment(false)?;
            self.semicolon()?;
            return Ok(Export::DefaultExpr(e));
        }
        if self.at(P::LBrace) {
            self.next()?;
            let mut specs = Vec::new();
            let mut idents_ok = true;
            while !self.eat(P::RBrace)? {
                let is_str = matches!(self.tok.tok, Tok::Str(_));
                let tok = self.tok.clone();
                let local = self.module_export_name()?;
                if is_str {
                    idents_ok = false;
                } else if let Tok::Ident(n) = &tok.tok {
                    if RESERVED.contains(&&**n) || (n.as_ref() == "await") {
                        idents_ok = false;
                    }
                }
                let exported = if self.eat_kw("as")? { self.module_export_name()? } else { local.clone() };
                specs.push((local, exported));
                if !self.at(P::RBrace) {
                    self.expect(P::Comma)?;
                }
            }
            let from = if self.at_kw("from") {
                let f = self.from_clause()?;
                let attrs = self.import_attributes()?;
                Some((f, attrs))
            } else {
                if !idents_ok {
                    return self.err("export of a reserved word or string requires 'from'");
                }
                None
            };
            self.semicolon()?;
            return Ok(Export::Named { specs, from });
        }
        let pos = self.pos();
        if self.at_kw("var") || self.at_kw("let") || self.at_kw("const") || self.at_kw("function") || self.at_kw("class") || self.at_kw("async") || self.at_kw("using") || self.at_kw("await") {
            let s = self.statement_list_item()?;
            match &s.kind {
                StmtKind::Var(..) | StmtKind::Function(_) | StmtKind::Class(_) => {}
                _ => return self.err_at(pos, "unexpected export"),
            }
            if let StmtKind::Var(VarKind::Using | VarKind::AwaitUsing, _) = s.kind {
                return self.err_at(pos, "using declarations cannot be exported");
            }
            return Ok(Export::Decl(Box::new(s)));
        }
        self.unexpected()
    }
}

fn is_continuation(p: P) -> bool {
    !matches!(p, P::LBrace | P::Bang | P::Tilde | P::PlusPlus | P::MinusMinus)
}

/// Names bound by a pattern, with positions.
pub fn bound_names(p: &Pattern, out: &mut Vec<(Name, Pos)>) {
    match p {
        Pattern::Ident(n, pos) => out.push((n.clone(), *pos)),
        Pattern::Expr(_) => {}
        Pattern::Object { props, rest, .. } => {
            for pp in props {
                bound_names(&pp.target, out);
            }
            if let Some(r) = rest {
                bound_names(r, out);
            }
        }
        Pattern::Array { elems, rest, .. } => {
            for e in elems.iter().flatten() {
                bound_names(&e.target, out);
            }
            if let Some(r) = rest {
                bound_names(r, out);
            }
        }
    }
}

fn validate_regex_flags(flags: &str) -> Result<(), &'static str> {
    let mut seen = 0u32;
    for c in flags.chars() {
        let bit = match c {
            'd' => 1,
            'g' => 2,
            'i' => 4,
            'm' => 8,
            's' => 16,
            'u' => 32,
            'v' => 64,
            'y' => 128,
            _ => return Err("invalid regular expression flags"),
        };
        if seen & bit != 0 {
            return Err("duplicate regular expression flag");
        }
        seen |= bit;
    }
    if seen & 32 != 0 && seen & 64 != 0 {
        return Err("regular expression flags u and v are exclusive");
    }
    Ok(())
}

fn is_well_formed(s: &[u16]) -> bool {
    let mut i = 0;
    while i < s.len() {
        let c = s[i];
        if (0xD800..0xDC00).contains(&c) {
            if i + 1 < s.len() && (0xDC00..0xE000).contains(&s[i + 1]) {
                i += 2;
                continue;
            }
            return false;
        }
        if (0xDC00..0xE000).contains(&c) {
            return false;
        }
        i += 1;
    }
    true
}
