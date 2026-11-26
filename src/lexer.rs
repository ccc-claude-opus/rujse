//! ECMAScript lexical scanner.

use alloc::{rc::Rc, string::String, vec, vec::Vec};

use crate::unicode::{is_id_continue, is_id_start, is_js_whitespace, is_line_terminator};

#[derive(Clone, Debug, PartialEq)]
pub enum Tok {
    Eof,
    /// IdentifierName, including reserved words and keywords (the parser
    /// decides). `name` has escapes already decoded.
    Ident(Rc<str>),
    /// `#name` (private name), name without `#`, escapes decoded.
    Private(Rc<str>),
    Punct(P),
    /// Numeric literal value.
    Num(f64),
    /// BigInt literal: decimal digit string of the value (e.g. `0x10n` -> "16"), no `n`, no separators.
    BigInt(Rc<str>),
    /// String literal, cooked value as UTF-16 code units (lone surrogates allowed).
    Str(Rc<[u16]>),
    /// A template piece. `cooked` is None when it has an invalid escape
    /// (allowed in tagged templates). `raw` is the raw text with CRLF and CR normalized to LF.
    /// `tail` = this piece ends with a backtick (otherwise it ends with `${`).
    Template { cooked: Option<Rc<[u16]>>, raw: Rc<[u16]>, tail: bool },
    /// Regular expression literal: body between the slashes and the flags, unvalidated.
    Regex { body: Rc<str>, flags: Rc<str> },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum P {
    LBrace, RBrace, LParen, RParen, LBracket, RBracket,
    Dot, Ellipsis, Semi, Comma, Lt, Gt, Le, Ge, Eq2, Ne, Eq3, Ne2,
    Plus, Minus, Star, Percent, StarStar, PlusPlus, MinusMinus,
    Shl, Shr, Ushr, Amp, Pipe, Caret, Bang, Tilde, AmpAmp, PipePipe, Question2,
    Question, QuestionDot, Colon, Assign, PlusEq, MinusEq, StarEq, PercentEq, StarStarEq,
    ShlEq, ShrEq, UshrEq, AmpEq, PipeEq, CaretEq, AmpAmpEq, PipePipeEq, Question2Eq,
    Arrow, Slash, SlashEq, At,
}

#[derive(Clone, Debug)]
pub struct Token {
    pub tok: Tok,
    /// Byte offsets into the source.
    pub start: u32,
    pub end: u32,
    /// 1-based line and column (column in UTF-16 code units, 1-based) of `start`.
    pub line: u32,
    pub col: u32,
    /// A LineTerminator (or a multi-line comment containing one) precedes this token.
    pub nl_before: bool,
    /// Ident/Private: the source spelling contained a `\u` escape.
    /// Str/Template: the literal contained a legacy octal escape (`\1`..`\7`, `\0` followed by a digit, `\08`, `\09`) or a NonOctalDecimalEscape (`\8`, `\9`) — the parser rejects it in strict mode.
    pub escaped: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SyntaxError {
    pub message: String,
    pub line: u32,
    pub col: u32,
}

#[derive(Clone)]
pub struct Lexer<'a> {
    src: &'a str,
    pos: usize,
    line: u32,
    col: u32,
    // Bit 0 records a pending newline; bit 1 says that the current physical
    // line contains only trivia. Keeping both in one flag keeps the lexer small.
    flags: u8,
}

const NL_PENDING: u8 = 1;
const LINE_ONLY_TRIVIA: u8 = 2;
const BASE_1E9: u64 = 1_000_000_000;

impl<'a> Lexer<'a> {
    pub fn new(src: &'a str) -> Self {
        Self {
            src,
            pos: 0,
            line: 1,
            col: 1,
            flags: LINE_ONLY_TRIVIA,
        }
    }

    /// Next token. A `/` or `/=` is always returned as Punct(Slash/SlashEq);
    /// a `}` always as Punct(RBrace) (the parser rescans, see below).
    pub fn next(&mut self) -> Result<Token, SyntaxError> {
        self.skip_trivia()?;

        let nl_before = self.flags & NL_PENDING != 0;
        self.flags &= !NL_PENDING;
        let start = self.pos;
        let line = self.line;
        let col = self.col;

        if self.pos >= self.src.len() {
            return Ok(Token {
                tok: Tok::Eof,
                start: start as u32,
                end: start as u32,
                line,
                col,
                nl_before,
                escaped: false,
            });
        }

        let byte = self.src.as_bytes()[self.pos];
        let (tok, escaped) = match byte {
            b'`' => {
                let (cooked, raw, tail, escaped) = self.scan_initial_template(line, col)?;
                (Tok::Template { cooked, raw, tail }, escaped)
            }
            b'\'' | b'"' => {
                let (value, escaped) = self.scan_string(line, col)?;
                (Tok::Str(value), escaped)
            }
            b'0'..=b'9' => self.scan_number(line, col)?,
            b'.' => {
                if self.peek_byte(1).is_some_and(|b| b.is_ascii_digit()) {
                    self.scan_number(line, col)?
                } else {
                    (Tok::Punct(self.scan_punctuator()), false)
                }
            }
            b'#' => {
                let follows_identifier = self.peek_byte(1) == Some(b'\\')
                    || self.src[self.pos + 1..].chars().next().is_some_and(is_identifier_start_raw);
                if !follows_identifier {
                    return Err(error_at("invalid private name", line, col));
                }
                self.consume_char();
                let (name, escaped) = self.scan_identifier(line, col)?;
                (Tok::Private(Rc::from(name)), escaped)
            }
            b'$' | b'_' | b'\\' => {
                let (name, escaped) = self.scan_identifier(line, col)?;
                (Tok::Ident(Rc::from(name)), escaped)
            }
            b'{' | b'}' | b'(' | b')' | b'[' | b']' | b';' | b',' | b'<' | b'>' | b'=' | b'!' | b'+' | b'-' | b'*'
            | b'%' | b'&' | b'|' | b'^' | b'~' | b'?' | b':' | b'/' | b'@' => (Tok::Punct(self.scan_punctuator()), false),
            _ => {
                if let Some(c) = self.peek_char() {
                    if is_identifier_start_raw(c) {
                        let (name, escaped) = self.scan_identifier(line, col)?;
                        (Tok::Ident(Rc::from(name)), escaped)
                    } else {
                        return Err(error_at("unexpected character", line, col));
                    }
                } else {
                    return Err(error_at("unexpected character", line, col));
                }
            }
        };

        self.flags &= !LINE_ONLY_TRIVIA;
        Ok(Token {
            tok,
            start: start as u32,
            end: self.pos as u32,
            line,
            col,
            nl_before,
            escaped,
        })
    }

    /// The parser found `/` or `/=` (token `tok`) where an expression starts:
    /// rescan from `tok.start` as a RegularExpressionLiteral and return it
    /// (Tok::Regex); the lexer continues after it.
    pub fn rescan_regex(&mut self, tok: &Token) -> Result<Token, SyntaxError> {
        let start = tok.start as usize;
        if self.src.as_bytes().get(start) != Some(&b'/') {
            return Err(error_at("expected regular expression", tok.line, tok.col));
        }

        self.pos = start;
        self.line = tok.line;
        self.col = tok.col;
        self.flags = 0;
        self.consume_char();

        let body_start = self.pos;
        let mut in_class = false;
        let body_end = loop {
            let Some(c) = self.peek_char() else {
                return Err(error_at("unterminated regular expression", tok.line, tok.col));
            };
            if is_line_terminator(c) {
                return Err(error_at("unterminated regular expression", tok.line, tok.col));
            }

            if c == '\\' {
                self.consume_char();
                let Some(escaped) = self.peek_char() else {
                    return Err(error_at("unterminated regular expression", tok.line, tok.col));
                };
                if is_line_terminator(escaped) {
                    return Err(error_at("unterminated regular expression", tok.line, tok.col));
                }
                self.consume_char();
                continue;
            }

            if c == '[' && !in_class {
                in_class = true;
                self.consume_char();
                continue;
            }
            if c == ']' && in_class {
                in_class = false;
                self.consume_char();
                continue;
            }
            if c == '/' && !in_class {
                let end = self.pos;
                self.consume_char();
                break end;
            }
            self.consume_char();
        };

        let flags_start = self.pos;
        loop {
            let Some(c) = self.peek_char() else { break };
            if c == '\\' {
                return Err(error_at("invalid regular expression flag", tok.line, tok.col));
            }
            if is_identifier_continue_raw(c) {
                self.consume_char();
            } else {
                break;
            }
        }
        let flags = &self.src[flags_start..self.pos];

        Ok(Token {
            tok: Tok::Regex {
                body: Rc::from(&self.src[body_start..body_end]),
                flags: Rc::from(flags),
            },
            start: tok.start,
            end: self.pos as u32,
            line: tok.line,
            col: tok.col,
            nl_before: tok.nl_before,
            escaped: false,
        })
    }

    /// The parser found `}` (token `tok`) that closes a `${ … }` substitution:
    /// rescan from `tok.start` as TemplateMiddle/TemplateTail (Tok::Template).
    pub fn rescan_template(&mut self, tok: &Token) -> Result<Token, SyntaxError> {
        let start = tok.start as usize;
        if self.src.as_bytes().get(start) != Some(&b'}') {
            return Err(error_at("expected template continuation", tok.line, tok.col));
        }

        self.pos = start;
        self.line = tok.line;
        self.col = tok.col;
        self.flags = 0;
        self.consume_char();

        let (cooked, raw, tail, escaped) = self.scan_template_segment(tok.line, tok.col)?;
        Ok(Token {
            tok: Tok::Template { cooked, raw, tail },
            start: tok.start,
            end: self.pos as u32,
            line: tok.line,
            col: tok.col,
            nl_before: tok.nl_before,
            escaped,
        })
    }

    /// Source text of a byte range (for function `toString`).
    pub fn slice(&self, start: u32, end: u32) -> &'a str {
        &self.src[start as usize..end as usize]
    }

    /// The whole source.
    pub fn source(&self) -> &'a str {
        self.src
    }

    // ----- trivia ---------------------------------------------------------

    fn skip_trivia(&mut self) -> Result<(), SyntaxError> {
        loop {
            let Some(byte) = self.peek_byte(0) else { break };
            match byte {
                b' ' | b'\t' | 0x0b | 0x0c => {
                    self.consume_char();
                }
                b'\n' | b'\r' => {
                    self.consume_line_terminator();
                    self.flags |= NL_PENDING | LINE_ONLY_TRIVIA;
                }
                b'/' if self.peek_byte(1) == Some(b'/') => {
                    self.skip_single_line_comment(2);
                }
                b'/' if self.peek_byte(1) == Some(b'*') => {
                    self.skip_block_comment()?;
                }
                b'<' if self.src[self.pos..].starts_with("<!--") => {
                    self.skip_single_line_comment(4);
                }
                b'-' if self.flags & LINE_ONLY_TRIVIA != 0
                    && self.src[self.pos..].starts_with("-->") =>
                {
                    self.skip_single_line_comment(3);
                }
                b'#' if self.pos == 0 && self.peek_byte(1) == Some(b'!') => {
                    self.skip_single_line_comment(2);
                }
                _ if byte >= 0x80 => {
                    let c = self.peek_char().expect("a byte starts a UTF-8 character");
                    if is_js_whitespace(c) {
                        self.consume_char();
                    } else if is_line_terminator(c) {
                        self.consume_line_terminator();
                        self.flags |= NL_PENDING | LINE_ONLY_TRIVIA;
                    } else {
                        break;
                    }
                }
                _ => break,
            }
        }
        Ok(())
    }

    fn skip_single_line_comment(&mut self, prefix_len: usize) {
        for _ in 0..prefix_len {
            self.consume_char();
        }
        while let Some(c) = self.peek_char() {
            if is_line_terminator(c) {
                self.consume_line_terminator();
                self.flags |= NL_PENDING | LINE_ONLY_TRIVIA;
                break;
            }
            self.consume_char();
        }
    }

    fn skip_block_comment(&mut self) -> Result<(), SyntaxError> {
        let line = self.line;
        let col = self.col;
        self.consume_char();
        self.consume_char();
        loop {
            let Some(c) = self.peek_char() else {
                return Err(error_at("unterminated comment", line, col));
            };
            if c == '*' && self.peek_byte(1) == Some(b'/') {
                self.consume_char();
                self.consume_char();
                break;
            }
            if is_line_terminator(c) {
                self.consume_line_terminator();
                self.flags |= NL_PENDING | LINE_ONLY_TRIVIA;
            } else {
                self.consume_char();
            }
        }
        Ok(())
    }

    // ----- identifiers ----------------------------------------------------

    fn scan_identifier(&mut self, line: u32, col: u32) -> Result<(String, bool), SyntaxError> {
        let mut name = String::new();
        let mut escaped = false;
        let mut first = true;

        loop {
            let Some(c) = self.peek_char() else { break };
            if c == '\\' {
                let decoded = self.scan_identifier_escape(line, col)?;
                let valid = if first {
                    is_identifier_start_raw(decoded)
                } else {
                    is_identifier_continue_raw(decoded)
                };
                if !valid {
                    return Err(error_at("invalid identifier escape", line, col));
                }
                name.push(decoded);
                escaped = true;
                first = false;
                continue;
            }

            let valid = if first {
                is_identifier_start_raw(c)
            } else {
                is_identifier_continue_raw(c)
            };
            if !valid {
                break;
            }
            name.push(c);
            self.consume_char();
            first = false;
        }

        if name.is_empty() {
            return Err(error_at("invalid identifier", line, col));
        }
        Ok((name, escaped))
    }

    fn scan_identifier_escape(&mut self, line: u32, col: u32) -> Result<char, SyntaxError> {
        self.consume_char(); // '\\'
        if self.peek_byte(0) != Some(b'u') {
            return Err(error_at("invalid identifier escape", line, col));
        }
        self.consume_char();

        let value = if self.peek_byte(0) == Some(b'{') {
            self.consume_char();
            let mut value = 0u32;
            let mut count = 0usize;
            loop {
                let Some(byte) = self.peek_byte(0) else {
                    return Err(error_at("invalid unicode escape", line, col));
                };
                if byte == b'}' {
                    break;
                }
                let Some(digit) = hex_value(byte) else {
                    return Err(error_at("invalid unicode escape", line, col));
                };
                value = value
                    .checked_mul(16)
                    .and_then(|v| v.checked_add(digit as u32))
                    .ok_or_else(|| error_at("unicode escape out of range", line, col))?;
                count += 1;
                self.consume_char();
            }
            if count == 0 {
                return Err(error_at("invalid unicode escape", line, col));
            }
            self.consume_char();
            if value > 0x10ffff {
                return Err(error_at("unicode escape out of range", line, col));
            }
            value
        } else {
            let mut value = 0u32;
            for _ in 0..4 {
                let Some(byte) = self.peek_byte(0) else {
                    return Err(error_at("invalid unicode escape", line, col));
                };
                let digit = hex_value(byte)
                    .ok_or_else(|| error_at("invalid unicode escape", line, col))?;
                value = (value << 4) | digit as u32;
                self.consume_char();
            }
            value
        };

        char::from_u32(value).ok_or_else(|| error_at("invalid identifier escape", line, col))
    }
}
// Numeric scanning lives in a separate impl block to keep the public API
// section easy to navigate.
impl<'a> Lexer<'a> {
    fn scan_number(&mut self, line: u32, col: u32) -> Result<(Tok, bool), SyntaxError> {
        let byte = self.src.as_bytes()[self.pos];
        if byte == b'.' {
            return self.scan_leading_dot_number(line, col);
        }
        if byte == b'0' {
            let radix = match self.peek_byte(1) {
                Some(b'x') | Some(b'X') => Some(16),
                Some(b'o') | Some(b'O') => Some(8),
                Some(b'b') | Some(b'B') => Some(2),
                _ => None,
            };
            if let Some(radix) = radix {
                self.consume_char();
                self.consume_char();
                return self.scan_radix_number(radix, line, col);
            }
        }
        self.scan_decimal_number(line, col)
    }

    fn scan_radix_number(
        &mut self,
        radix: u32,
        line: u32,
        col: u32,
    ) -> Result<(Tok, bool), SyntaxError> {
        let mut digits = Vec::new();
        let mut previous_is_digit = false;

        loop {
            let Some(byte) = self.peek_byte(0) else { break };
            if let Some(digit) = radix_digit(byte, radix) {
                digits.push(digit);
                previous_is_digit = true;
                self.consume_char();
                continue;
            }
            if byte == b'_' {
                let next_is_digit = self
                    .peek_byte(1)
                    .is_some_and(|b| radix_digit(b, radix).is_some());
                if !previous_is_digit || !next_is_digit {
                    return Err(error_at("invalid numeric separator", line, col));
                }
                self.consume_char();
                previous_is_digit = false;
                continue;
            }
            break;
        }

        if digits.is_empty() {
            return Err(error_at("invalid numeric literal", line, col));
        }

        if self.peek_byte(0) == Some(b'n') {
            self.consume_char();
            self.reject_number_suffix(line, col)?;
            return Ok((Tok::BigInt(Rc::from(radix_to_decimal(&digits, radix))), false));
        }

        self.reject_number_suffix(line, col)?;
        Ok((Tok::Num(radix_to_f64(&digits, radix)), false))
    }

    fn scan_decimal_number(
        &mut self,
        line: u32,
        col: u32,
    ) -> Result<(Tok, bool), SyntaxError> {
        let first = self.src.as_bytes()[self.pos];

        // A leading zero followed by a digit is one of the Annex B decimal
        // forms. It never accepts separators or a BigInt suffix.
        if first == b'0' && self.peek_byte(1).is_some_and(|b| b.is_ascii_digit()) {
            self.consume_char(); // 0
            let second = self.src.as_bytes()[self.pos];
            self.consume_char();

            if second == b'8' || second == b'9' {
                // NonOctalDecimalIntegerLiteral is exactly `08` or `09`.
                // A fraction or exponent may follow it.
                if self.peek_byte(0).is_some_and(|b| b.is_ascii_digit()) {
                    return Err(error_at("invalid numeric literal", line, col));
                }
                let mut text = String::new();
                push_decimal_u32(&mut text, (second - b'0') as u32);
                self.scan_decimal_fraction(&mut text, line, col)?;
                self.scan_decimal_exponent(&mut text, line, col)?;
                let value = text
                    .parse::<f64>()
                    .map_err(|_| error_at("invalid number", line, col))?;
                self.reject_number_suffix(line, col)?;
                return Ok((Tok::Num(value), true));
            }

            let mut octal = vec![0u8, second - b'0'];
            while let Some(byte) = self.peek_byte(0) {
                if !byte.is_ascii_digit() {
                    break;
                }
                if byte == b'8' || byte == b'9' {
                    return Err(error_at("invalid numeric literal", line, col));
                }
                octal.push(byte - b'0');
                self.consume_char();
            }
            if self.peek_byte(0) == Some(b'_') {
                return Err(error_at("invalid numeric separator", line, col));
            }
            if self.peek_byte(0) == Some(b'n') {
                return Err(error_at("bigint literal is not allowed here", line, col));
            }

            let mut text = radix_to_decimal(&octal, 8);
            self.scan_decimal_fraction(&mut text, line, col)?;
            self.scan_decimal_exponent(&mut text, line, col)?;
            let value = text
                .parse::<f64>()
                .map_err(|_| error_at("invalid number", line, col))?;
            self.reject_number_suffix(line, col)?;
            return Ok((Tok::Num(value), true));
        }

        self.consume_char(); // first digit
        if first == b'0' && self.peek_byte(0) == Some(b'_') {
            return Err(error_at("invalid numeric separator", line, col));
        }

        let integer = self.consume_decimal_digits(Some(first), line, col)?;
        if self.peek_byte(0) == Some(b'n') {
            self.consume_char();
            self.reject_number_suffix(line, col)?;
            return Ok((Tok::BigInt(Rc::from(radix_to_decimal(&integer, 10))), false));
        }

        let mut text = String::new();
        append_digits(&mut text, &integer);
        self.scan_decimal_fraction(&mut text, line, col)?;
        self.scan_decimal_exponent(&mut text, line, col)?;
        let value = text
            .parse::<f64>()
            .map_err(|_| error_at("invalid number", line, col))?;
        self.reject_number_suffix(line, col)?;
        Ok((Tok::Num(value), false))
    }

    fn scan_leading_dot_number(
        &mut self,
        line: u32,
        col: u32,
    ) -> Result<(Tok, bool), SyntaxError> {
        self.consume_char(); // '.'
        let fraction = self.consume_decimal_digits(None, line, col)?;
        if fraction.is_empty() {
            return Err(error_at("invalid numeric literal", line, col));
        }
        let mut text = String::from("0.");
        append_digits(&mut text, &fraction);
        self.scan_decimal_exponent(&mut text, line, col)?;
        let value = text
            .parse::<f64>()
            .map_err(|_| error_at("invalid number", line, col))?;
        self.reject_number_suffix(line, col)?;
        Ok((Tok::Num(value), false))
    }

    fn consume_decimal_digits(
        &mut self,
        first: Option<u8>,
        line: u32,
        col: u32,
    ) -> Result<Vec<u8>, SyntaxError> {
        let mut digits = Vec::new();
        let mut previous_is_digit = false;

        if let Some(digit) = first {
            digits.push(digit - b'0');
            previous_is_digit = true;
        }

        loop {
            let Some(byte) = self.peek_byte(0) else { break };
            if byte.is_ascii_digit() {
                digits.push(byte - b'0');
                previous_is_digit = true;
                self.consume_char();
                continue;
            }
            if byte == b'_' {
                let next_is_digit = self.peek_byte(1).is_some_and(|b| b.is_ascii_digit());
                if !previous_is_digit || !next_is_digit {
                    return Err(error_at("invalid numeric separator", line, col));
                }
                self.consume_char();
                previous_is_digit = false;
                continue;
            }
            break;
        }
        Ok(digits)
    }

    fn scan_decimal_fraction(
        &mut self,
        text: &mut String,
        line: u32,
        col: u32,
    ) -> Result<(), SyntaxError> {
        if self.peek_byte(0) != Some(b'.') {
            return Ok(());
        }
        self.consume_char();
        let digits = self.consume_decimal_digits(None, line, col)?;
        text.push('.');
        append_digits(text, &digits);
        Ok(())
    }

    fn scan_decimal_exponent(
        &mut self,
        text: &mut String,
        line: u32,
        col: u32,
    ) -> Result<(), SyntaxError> {
        let Some(marker) = self.peek_byte(0) else { return Ok(()) };
        if marker != b'e' && marker != b'E' {
            return Ok(());
        }
        self.consume_char();
        text.push('e');
        if matches!(self.peek_byte(0), Some(b'+') | Some(b'-')) {
            let sign = self.src.as_bytes()[self.pos];
            self.consume_char();
            text.push(sign as char);
        }
        let digits = self.consume_decimal_digits(None, line, col)?;
        if digits.is_empty() {
            return Err(error_at("invalid exponent", line, col));
        }
        append_digits(text, &digits);
        Ok(())
    }

    fn reject_number_suffix(&self, line: u32, col: u32) -> Result<(), SyntaxError> {
        let Some(byte) = self.peek_byte(0) else { return Ok(()) };
        if byte.is_ascii_digit() {
            return Err(error_at("identifier directly after number", line, col));
        }
        if byte == b'\\' || byte == b'$' || byte == b'_' || byte.is_ascii_alphabetic() {
            return Err(error_at("identifier directly after number", line, col));
        }
        if byte >= 0x80 {
            if let Some(c) = self.peek_char() {
                if is_identifier_start_raw(c) {
                    return Err(error_at("identifier directly after number", line, col));
                }
            }
        }
        Ok(())
    }
}
// String and template scanning.
impl<'a> Lexer<'a> {
    fn scan_string(&mut self, line: u32, col: u32) -> Result<(Rc<[u16]>, bool), SyntaxError> {
        let quote = self.src.as_bytes()[self.pos];
        self.consume_char();
        let mut value = Vec::new();
        let mut escaped = false;

        loop {
            let Some(byte) = self.peek_byte(0) else {
                return Err(error_at("unterminated string", line, col));
            };
            if byte == quote {
                self.consume_char();
                break;
            }
            if byte == b'\n' || byte == b'\r' {
                return Err(error_at("unescaped line terminator in string", line, col));
            }
            if byte == b'\\' {
                let outcome = self.scan_escape(false, &mut value, line, col)?;
                if !outcome.valid {
                    return Err(error_at("invalid escape sequence", line, col));
                }
                escaped |= outcome.legacy;
                continue;
            }

            let c = self.peek_char().expect("a byte starts a UTF-8 character");
            push_utf16(&mut value, c as u32);
            self.consume_char();
        }

        Ok((Rc::from(value), escaped))
    }

    fn scan_initial_template(
        &mut self,
        line: u32,
        col: u32,
    ) -> Result<(Option<Rc<[u16]>>, Rc<[u16]>, bool, bool), SyntaxError> {
        self.consume_char(); // opening backtick
        self.scan_template_segment(line, col)
    }

    fn scan_template_segment(
        &mut self,
        line: u32,
        col: u32,
    ) -> Result<(Option<Rc<[u16]>>, Rc<[u16]>, bool, bool), SyntaxError> {
        let mut raw = Vec::new();
        let mut cooked = Vec::new();
        let mut cooked_valid = true;
        let mut escaped = false;

        loop {
            let Some(byte) = self.peek_byte(0) else {
                return Err(error_at("unterminated template", line, col));
            };

            if byte == b'`' {
                self.consume_char();
                let cooked = if cooked_valid { Some(Rc::from(cooked)) } else { None };
                return Ok((cooked, Rc::from(raw), true, escaped));
            }
            if byte == b'$' && self.peek_byte(1) == Some(b'{') {
                self.consume_char();
                self.consume_char();
                let cooked = if cooked_valid { Some(Rc::from(cooked)) } else { None };
                return Ok((cooked, Rc::from(raw), false, escaped));
            }
            if byte == b'\\' {
                let escape_start = self.pos;
                let mut ignored = Vec::new();
                let outcome = self.scan_escape(true, &mut ignored, line, col)?;
                append_normalized_raw(&mut raw, &self.src[escape_start..self.pos]);
                escaped |= outcome.legacy;
                if !outcome.valid {
                    cooked_valid = false;
                } else if cooked_valid {
                    cooked.extend_from_slice(&ignored);
                }
                continue;
            }

            if byte == b'\r' || byte == b'\n' {
                self.consume_line_terminator();
                raw.push('\n' as u16);
                if cooked_valid {
                    cooked.push('\n' as u16);
                }
                continue;
            }

            let c = self.peek_char().expect("a byte starts a UTF-8 character");
            let mut buf = [0u16; 2];
            let units = c.encode_utf16(&mut buf);
            raw.extend_from_slice(units);
            if cooked_valid {
                cooked.extend_from_slice(units);
            }
            self.consume_char();
        }
    }

    fn scan_escape(
        &mut self,
        template: bool,
        value: &mut Vec<u16>,
        line: u32,
        col: u32,
    ) -> Result<EscapeOutcome, SyntaxError> {
        self.consume_char(); // '\\'
        let Some(byte) = self.peek_byte(0) else {
            return Err(error_at("unterminated escape sequence", line, col));
        };

        let after_slash = self.peek_char().expect("a byte starts a UTF-8 character");
        if is_line_terminator(after_slash) {
            self.consume_line_terminator();
            return Ok(EscapeOutcome { valid: true, legacy: false });
        }

        let simple = match byte {
            b'b' => Some(0x08),
            b't' => Some(0x09),
            b'n' => Some(0x0a),
            b'v' => Some(0x0b),
            b'f' => Some(0x0c),
            b'r' => Some(0x0d),
            b'"' => Some(0x22),
            b'\'' => Some(0x27),
            b'\\' => Some(0x5c),
            _ => None,
        };
        if let Some(unit) = simple {
            self.consume_char();
            value.push(unit);
            return Ok(EscapeOutcome { valid: true, legacy: false });
        }

        if byte == b'x' {
            self.consume_char();
            let mut hex = 0u16;
            let mut count = 0usize;
            for _ in 0..2 {
                let Some(digit) = self.peek_byte(0).and_then(hex_value) else { break };
                hex = (hex << 4) | digit as u16;
                count += 1;
                self.consume_char();
            }
            if count != 2 {
                return if template {
                    Ok(EscapeOutcome { valid: false, legacy: false })
                } else {
                    Err(error_at("invalid hexadecimal escape", line, col))
                };
            }
            value.push(hex);
            return Ok(EscapeOutcome { valid: true, legacy: false });
        }

        if byte == b'u' {
            self.consume_char();
            if self.peek_byte(0) == Some(b'{') {
                self.consume_char();
                let mut code = 0u32;
                let mut count = 0usize;
                let mut overflow = false;
                loop {
                    let Some(digit) = self.peek_byte(0).and_then(hex_value) else { break };
                    count += 1;
                    if !overflow {
                        match code.checked_mul(16).and_then(|v| v.checked_add(digit as u32)) {
                            Some(v) => code = v,
                            None => overflow = true,
                        }
                    }
                    self.consume_char();
                }
                if self.peek_byte(0) == Some(b'}') {
                    self.consume_char();
                }
                let valid = count != 0 && !overflow && code <= 0x10ffff;
                if !valid {
                    return if template {
                        Ok(EscapeOutcome { valid: false, legacy: false })
                    } else {
                        Err(error_at("invalid unicode escape", line, col))
                    };
                }
                push_code_point(value, code);
                return Ok(EscapeOutcome { valid: true, legacy: false });
            }

            let mut code = 0u32;
            let mut count = 0usize;
            for _ in 0..4 {
                let Some(digit) = self.peek_byte(0).and_then(hex_value) else { break };
                code = (code << 4) | digit as u32;
                count += 1;
                self.consume_char();
            }
            if count != 4 {
                return if template {
                    Ok(EscapeOutcome { valid: false, legacy: false })
                } else {
                    Err(error_at("invalid unicode escape", line, col))
                };
            }
            // A fixed-width escape denotes one UTF-16 code unit. Surrogates
            // are therefore allowed as lone values.
            value.push(code as u16);
            return Ok(EscapeOutcome { valid: true, legacy: false });
        }

        // Legacy octal and NonOctalDecimalEscape sequences.
        if byte == b'0' && !self.peek_byte(1).is_some_and(|b| b.is_ascii_digit()) {
            self.consume_char();
            value.push(0);
            return Ok(EscapeOutcome { valid: true, legacy: false });
        }
        if (b'0'..=b'7').contains(&byte) {
            let mut octal = byte - b'0';
            self.consume_char();

            if byte == b'0' && self.peek_byte(0).is_some_and(|b| b.is_ascii_digit()) {
                let next = self.peek_byte(0).expect("a digit follows");
                if next == b'8' || next == b'9' {
                    // `\08` and `\09` are the NUL escape followed by an
                    // ordinary digit. They are nevertheless legacy escapes.
                    value.push(0);
                    return Ok(EscapeOutcome { valid: !template, legacy: true });
                }

                // ZeroToThree OctalDigit OctalDigit: at most three digits,
                // including the leading zero.
                if let Some(next) = self.peek_byte(0) {
                    if (b'0'..=b'7').contains(&next) {
                        octal = octal * 8 + (next - b'0');
                        self.consume_char();
                        if let Some(next) = self.peek_byte(0) {
                            if (b'0'..=b'7').contains(&next) {
                                octal = octal * 8 + (next - b'0');
                                self.consume_char();
                            }
                        }
                    }
                }
            } else if byte >= b'1' && byte <= b'3' {
                // NonZeroOctalDigit may be followed by two octal digits.
                for _ in 0..2 {
                    let Some(next) = self.peek_byte(0) else { break };
                    if !(b'0'..=b'7').contains(&next) {
                        break;
                    }
                    octal = octal * 8 + (next - b'0');
                    self.consume_char();
                }
            } else if byte >= b'4' && byte <= b'7' {
                // FourToSeven OctalDigit: only one following octal digit.
                if let Some(next) = self.peek_byte(0) {
                    if (b'0'..=b'7').contains(&next) {
                        octal = octal * 8 + (next - b'0');
                        self.consume_char();
                    }
                }
            }

            value.push(octal as u16);
            return Ok(EscapeOutcome { valid: !template, legacy: true });
        }

        if byte == b'8' || byte == b'9' {
            self.consume_char();
            value.push(byte as u16);
            return Ok(EscapeOutcome { valid: !template, legacy: true });
        }

        // IdentityEscape: any other character denotes itself.
        let c = self.peek_char().expect("a byte starts a UTF-8 character");
        push_utf16(value, c as u32);
        self.consume_char();
        Ok(EscapeOutcome { valid: true, legacy: false })
    }
}

struct EscapeOutcome {
    valid: bool,
    legacy: bool,
}
// Punctuators and low-level source helpers.
impl<'a> Lexer<'a> {
    fn scan_punctuator(&mut self) -> P {
        let b0 = self.src.as_bytes()[self.pos];
        let b1 = self.peek_byte(1);
        let b2 = self.peek_byte(2);
        let b3 = self.peek_byte(3);

        let (punct, len) = match b0 {
            b'{' => (P::LBrace, 1),
            b'}' => (P::RBrace, 1),
            b'(' => (P::LParen, 1),
            b')' => (P::RParen, 1),
            b'[' => (P::LBracket, 1),
            b']' => (P::RBracket, 1),
            b';' => (P::Semi, 1),
            b',' => (P::Comma, 1),
            b':' => (P::Colon, 1),
            b'~' => (P::Tilde, 1),
            b'@' => (P::At, 1),
            b'.' => {
                if b1 == Some(b'.') && b2 == Some(b'.') {
                    (P::Ellipsis, 3)
                } else {
                    (P::Dot, 1)
                }
            }
            b'<' => match (b1, b2) {
                (Some(b'<'), Some(b'=')) => (P::ShlEq, 3),
                (Some(b'<'), _) => (P::Shl, 2),
                (Some(b'='), _) => (P::Le, 2),
                _ => (P::Lt, 1),
            },
            b'>' => match (b1, b2, b3) {
                (Some(b'>'), Some(b'>'), Some(b'=')) => (P::UshrEq, 4),
                (Some(b'>'), Some(b'>'), _) => (P::Ushr, 3),
                (Some(b'>'), Some(b'='), _) => (P::ShrEq, 3),
                (Some(b'>'), _, _) => (P::Shr, 2),
                (Some(b'='), _, _) => (P::Ge, 2),
                _ => (P::Gt, 1),
            },
            b'=' => match (b1, b2) {
                (Some(b'='), Some(b'=')) => (P::Eq3, 3),
                (Some(b'='), _) => (P::Eq2, 2),
                (Some(b'>'), _) => (P::Arrow, 2),
                _ => (P::Assign, 1),
            },
            b'!' => match (b1, b2) {
                (Some(b'='), Some(b'=')) => (P::Ne2, 3),
                (Some(b'='), _) => (P::Ne, 2),
                _ => (P::Bang, 1),
            },
            b'+' => match b1 {
                Some(b'+') => (P::PlusPlus, 2),
                Some(b'=') => (P::PlusEq, 2),
                _ => (P::Plus, 1),
            },
            b'-' => match b1 {
                Some(b'-') => (P::MinusMinus, 2),
                Some(b'=') => (P::MinusEq, 2),
                _ => (P::Minus, 1),
            },
            b'*' => match b1 {
                Some(b'*') => {
                    if b2 == Some(b'=') {
                        (P::StarStarEq, 3)
                    } else {
                        (P::StarStar, 2)
                    }
                }
                Some(b'=') => (P::StarEq, 2),
                _ => (P::Star, 1),
            },
            b'%' => match b1 {
                Some(b'=') => (P::PercentEq, 2),
                _ => (P::Percent, 1),
            },
            b'&' => match b1 {
                Some(b'&') => {
                    if b2 == Some(b'=') {
                        (P::AmpAmpEq, 3)
                    } else {
                        (P::AmpAmp, 2)
                    }
                }
                Some(b'=') => (P::AmpEq, 2),
                _ => (P::Amp, 1),
            },
            b'|' => match b1 {
                Some(b'|') => {
                    if b2 == Some(b'=') {
                        (P::PipePipeEq, 3)
                    } else {
                        (P::PipePipe, 2)
                    }
                }
                Some(b'=') => (P::PipeEq, 2),
                _ => (P::Pipe, 1),
            },
            b'^' => match b1 {
                Some(b'=') => (P::CaretEq, 2),
                _ => (P::Caret, 1),
            },
            b'?' => match (b1, b2) {
                (Some(b'?'), Some(b'=')) => (P::Question2Eq, 3),
                (Some(b'?'), _) => (P::Question2, 2),
                (Some(b'.'), _) => {
                    // `?.` is an optional-chain punctuator only when the dot
                    // does not begin a decimal numeric literal.
                    if b2.is_some_and(|b| b.is_ascii_digit()) {
                        (P::Question, 1)
                    } else {
                        (P::QuestionDot, 2)
                    }
                }
                _ => (P::Question, 1),
            },
            b'/' => match b1 {
                Some(b'=') => (P::SlashEq, 2),
                _ => (P::Slash, 1),
            },
            _ => unreachable!("scan_punctuator called for a non-punctuator"),
        };

        for _ in 0..len {
            self.consume_char();
        }
        punct
    }

    fn peek_byte(&self, offset: usize) -> Option<u8> {
        self.src.as_bytes().get(self.pos + offset).copied()
    }

    fn peek_char(&self) -> Option<char> {
        self.src[self.pos..].chars().next()
    }

    fn consume_char(&mut self) {
        let c = self.peek_char().expect("no character to consume");
        self.pos += c.len_utf8();
        self.col += c.len_utf16() as u32;
    }

    fn consume_line_terminator(&mut self) {
        match self.peek_char() {
            Some('\r') => {
                self.pos += 1;
                if self.peek_byte(0) == Some(b'\n') {
                    self.pos += 1;
                }
            }
            Some(c @ ('\n' | '\u{2028}' | '\u{2029}')) => {
                self.pos += c.len_utf8();
            }
            _ => unreachable!("no line terminator to consume"),
        }
        self.line += 1;
        self.col = 1;
    }
}

fn is_identifier_start_raw(c: char) -> bool {
    c == '$' || c == '_' || is_id_start(c)
}

fn is_identifier_continue_raw(c: char) -> bool {
    c == '$' || c == '\u{200c}' || c == '\u{200d}' || is_id_continue(c)
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn radix_digit(byte: u8, radix: u32) -> Option<u8> {
    let value = match byte {
        b'0'..=b'9' => byte - b'0',
        b'a'..=b'z' => byte - b'a' + 10,
        b'A'..=b'Z' => byte - b'A' + 10,
        _ => return None,
    };
    if (value as u32) < radix { Some(value) } else { None }
}

fn error_at(message: &str, line: u32, col: u32) -> SyntaxError {
    SyntaxError {
        message: String::from(message),
        line,
        col,
    }
}
// Exact numeric conversion helpers.
fn radix_to_decimal(digits: &[u8], radix: u32) -> String {
    if digits.is_empty() {
        return String::from("0");
    }

    // Little-endian base-1e9 limbs. This is deliberately independent of
    // floating point and therefore exact for arbitrarily large BigInts.
    let mut limbs: Vec<u32> = Vec::new();
    for &digit in digits {
        let mut carry = digit as u64;
        for limb in &mut limbs {
            let value = *limb as u64 * radix as u64 + carry;
            *limb = (value % BASE_1E9) as u32;
            carry = value / BASE_1E9;
        }
        if carry != 0 {
            limbs.push(carry as u32);
        }
    }

    if limbs.is_empty() {
        return String::from("0");
    }

    let mut out = String::new();
    push_decimal_u32(&mut out, *limbs.last().expect("a limb exists"));
    for limb in limbs.iter().rev().skip(1) {
        push_decimal_padded(&mut out, *limb, 9);
    }
    out
}

fn radix_to_f64(digits: &[u8], radix: u32) -> f64 {
    // Build the exact binary representation. Hexadecimal, octal and binary
    // literals all have an integral number of bits per digit.
    let width = match radix {
        2 => 1,
        8 => 3,
        16 => 4,
        _ => unreachable!("radix literals use a power-of-two radix"),
    };
    let mut bits: Vec<u8> = Vec::new();
    let mut started = false;
    for &digit in digits {
        for shift in (0..width).rev() {
            let bit = (digit as u32 >> shift) & 1;
            if bit != 0 {
                started = true;
            }
            if started {
                bits.push(bit as u8);
            }
        }
    }

    if bits.is_empty() {
        return 0.0;
    }
    if bits.len() <= 53 {
        let mut value = 0u64;
        for bit in bits {
            value = (value << 1) | bit as u64;
        }
        return value as f64;
    }

    let mut significand = 0u64;
    for &bit in &bits[..53] {
        significand = (significand << 1) | bit as u64;
    }

    // Round to nearest, ties to even. The discarded part is represented by
    // the first discarded bit and whether any lower bit is set.
    let first_discarded = bits[53] != 0;
    let lower_discarded = bits[54..].iter().any(|&bit| bit != 0);
    let greater_than_half = first_discarded && lower_discarded;
    let exactly_half = first_discarded && !lower_discarded;
    if greater_than_half || (exactly_half && significand & 1 != 0) {
        significand += 1;
    }

    let mut exponent = bits.len() - 1;
    if significand == (1u64 << 53) {
        significand >>= 1;
        exponent += 1;
    }
    if exponent > 1023 {
        return f64::INFINITY;
    }

    let fraction = significand - (1u64 << 52);
    let encoded = ((exponent as u64 + 1023) << 52) | fraction;
    f64::from_bits(encoded)
}

fn append_digits(out: &mut String, digits: &[u8]) {
    for &digit in digits {
        out.push((b'0' + digit) as char);
    }
}

fn push_decimal_u32(out: &mut String, mut value: u32) {
    let mut buffer = [0u8; 10];
    let mut index = buffer.len();
    loop {
        index -= 1;
        buffer[index] = b'0' + (value % 10) as u8;
        value /= 10;
        if value == 0 {
            break;
        }
    }
    for &byte in &buffer[index..] {
        out.push(byte as char);
    }
}

fn push_decimal_padded(out: &mut String, value: u32, width: usize) {
    let mut buffer = [0u8; 10];
    let mut index = buffer.len();
    let mut value = value;
    loop {
        index -= 1;
        buffer[index] = b'0' + (value % 10) as u8;
        value /= 10;
        if value == 0 {
            break;
        }
    }
    let digits = buffer.len() - index;
    for _ in digits..width {
        out.push('0');
    }
    for &byte in &buffer[index..] {
        out.push(byte as char);
    }
}

fn push_utf16(out: &mut Vec<u16>, code: u32) {
    if code <= 0xffff {
        out.push(code as u16);
    } else {
        push_code_point(out, code);
    }
}

fn push_code_point(out: &mut Vec<u16>, code: u32) {
    if code <= 0xffff {
        out.push(code as u16);
        return;
    }
    let adjusted = code - 0x1_0000;
    out.push(0xd800 + ((adjusted >> 10) as u16));
    out.push(0xdc00 + ((adjusted & 0x3ff) as u16));
}

fn append_normalized_raw(out: &mut Vec<u16>, text: &str) {
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\r' {
            out.push('\n' as u16);
            if chars.peek() == Some(&'\n') {
                chars.next();
            }
        } else {
            let mut buffer = [0u16; 2];
            out.extend_from_slice(c.encode_utf16(&mut buffer));
        }
    }
}
