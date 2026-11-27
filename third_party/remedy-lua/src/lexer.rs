//! The lexer: source text to tokens, with the line and column of each.

#[allow(unused_imports)]
use crate::nostd::prelude::*;
use crate::Dialect;
use crate::diag::{Diagnostic, Pos};

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Tok {
    Name(String),
    Int(i64),
    Num(f64),
    Str(Vec<u8>),
    /// A backtick string: literal parts and `{expression}` parts (source text and where it
    /// starts).
    Interp(Vec<InterpPart>),
    /// Any other single character.
    Other(u8),
    /// A lexical error at this point of the source.
    Error(Box<Diagnostic>),
    // Keywords.
    And,
    Break,
    Do,
    Else,
    Elseif,
    End,
    False,
    For,
    Function,
    Goto,
    If,
    In,
    Local,
    Nil,
    Not,
    Or,
    Repeat,
    Return,
    Then,
    True,
    Until,
    While,
    // Symbols.
    Plus,
    Minus,
    Star,
    Slash,
    DSlash,
    Percent,
    Caret,
    Hash,
    Amp,
    Tilde,
    Pipe,
    Shl,
    Shr,
    Eq,
    Ne,
    Le,
    Ge,
    Lt,
    Gt,
    Assign,
    LParen,
    RParen,
    LBrace,
    RBrace,
    LBracket,
    RBracket,
    DColon,
    Semi,
    Colon,
    Comma,
    Dot,
    Concat,
    Dots,
    // RemedyLua.
    /// `+=` and the other compound assignments: the operator.
    OpAssign(crate::ast::BinOp),
    Arrow,
    Question,
    Eof,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum InterpPart {
    Lit(Vec<u8>),
    /// The expression's text as written: its bytes, not made UTF-8 (S6-FRONT-5).
    Expr(Vec<u8>, Pos),
}

#[derive(Clone, Debug)]
pub(crate) struct Token {
    pub tok: Tok,
    pub pos: Pos,
    /// Byte range in the source (for messages that quote the token as written).
    pub span: (usize, usize),
    /// Where it ends: the line and column just past its last character (C Lua numbers a
    /// token by the line it ends on).
    pub end: Pos,
}

/// A token quoted in a syntax message as C Lua does: as written, except a short string,
/// which C Lua quotes with its escapes already applied (cut at a `\0`, like `%s`).
pub(crate) fn near_text(src: &[u8], t: &Token) -> String {
    let raw = src.get(t.span.0..t.span.1).unwrap_or_default();
    if let (Tok::Str(s), Some(&d)) = (&t.tok, raw.first())
        && (d == b'"' || d == b'\'')
    {
        let mut b = vec![d];
        b.extend_from_slice(s);
        b.push(d);
        let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
        return format!("'{}'", String::from_utf8_lossy(b.get(..end).unwrap_or_default()));
    }
    // A long string: C Lua's text leaves out the line break right after the opening bracket.
    if let (Tok::Str(_), Some(b'[')) = (&t.tok, raw.first()) {
        let open = raw.iter().skip(1).position(|&c| c != b'=').map_or(raw.len(), |n| n + 2);
        let (head, rest) = raw.split_at(open.min(raw.len()));
        let skip = match rest {
            [b'\r', b'\n', ..] | [b'\n', b'\r', ..] => 2,
            [b'\n' | b'\r', ..] => 1,
            _ => 0,
        };
        let mut b = head.to_vec();
        // C Lua saves each line break in it as one `\n` (`\r\n` and a lone `\r` too:
        // S5C-FRONT-10).
        let mut rest = rest.get(skip..).unwrap_or_default().iter().copied().peekable();
        while let Some(c) = rest.next() {
            if c == b'\n' || c == b'\r' {
                rest.next_if(|&d| (d == b'\n' || d == b'\r') && d != c);
                b.push(b'\n');
            } else {
                b.push(c);
            }
        }
        return format!("'{}'", String::from_utf8_lossy(&b));
    }
    format!("'{}'", String::from_utf8_lossy(raw))
}

impl Tok {
    /// How the token reads in an error message.
    pub fn describe(&self) -> String {
        match self {
            Tok::Name(n) => n.clone(),
            Tok::Int(i) => i.to_string(),
            Tok::Num(f) => crate::number::fmt_float(*f),
            Tok::Str(s) => String::from_utf8_lossy(s).into_owned(),
            Tok::Interp(_) => "`...`".into(),
            Tok::Error(_) => "<error>".into(),
            Tok::Other(c) => {
                if c.is_ascii_graphic() { char::from(*c).to_string() } else { format!("<\\{c}>") }
            }
            Tok::Eof => "<eof>".into(),
            Tok::OpAssign(op) => format!("{}=", op.symbol()),
            other => keyword_or_symbol(other).to_owned(),
        }
    }
}

fn keyword_or_symbol(t: &Tok) -> &'static str {
    match t {
        Tok::And => "and",
        Tok::Break => "break",
        Tok::Do => "do",
        Tok::Else => "else",
        Tok::Elseif => "elseif",
        Tok::End => "end",
        Tok::False => "false",
        Tok::For => "for",
        Tok::Function => "function",
        Tok::Goto => "goto",
        Tok::If => "if",
        Tok::In => "in",
        Tok::Local => "local",
        Tok::Nil => "nil",
        Tok::Not => "not",
        Tok::Or => "or",
        Tok::Repeat => "repeat",
        Tok::Return => "return",
        Tok::Then => "then",
        Tok::True => "true",
        Tok::Until => "until",
        Tok::While => "while",
        Tok::Plus => "+",
        Tok::Minus => "-",
        Tok::Star => "*",
        Tok::Slash => "/",
        Tok::DSlash => "//",
        Tok::Percent => "%",
        Tok::Caret => "^",
        Tok::Hash => "#",
        Tok::Amp => "&",
        Tok::Tilde => "~",
        Tok::Pipe => "|",
        Tok::Shl => "<<",
        Tok::Shr => ">>",
        Tok::Eq => "==",
        Tok::Ne => "~=",
        Tok::Le => "<=",
        Tok::Ge => ">=",
        Tok::Lt => "<",
        Tok::Gt => ">",
        Tok::Assign => "=",
        Tok::LParen => "(",
        Tok::RParen => ")",
        Tok::LBrace => "{",
        Tok::RBrace => "}",
        Tok::LBracket => "[",
        Tok::RBracket => "]",
        Tok::DColon => "::",
        Tok::Semi => ";",
        Tok::Colon => ":",
        Tok::Comma => ",",
        Tok::Dot => ".",
        Tok::Concat => "..",
        Tok::Dots => "...",
        Tok::Arrow => "->",
        Tok::Question => "?",
        _ => "?",
    }
}

fn keyword(s: &str) -> Option<Tok> {
    Some(match s {
        "and" => Tok::And,
        "break" => Tok::Break,
        "do" => Tok::Do,
        "else" => Tok::Else,
        "elseif" => Tok::Elseif,
        "end" => Tok::End,
        "false" => Tok::False,
        "for" => Tok::For,
        "function" => Tok::Function,
        "goto" => Tok::Goto,
        "if" => Tok::If,
        "in" => Tok::In,
        "local" => Tok::Local,
        "nil" => Tok::Nil,
        "not" => Tok::Not,
        "or" => Tok::Or,
        "repeat" => Tok::Repeat,
        "return" => Tok::Return,
        "then" => Tok::Then,
        "true" => Tok::True,
        "until" => Tok::Until,
        "while" => Tok::While,
        _ => return None,
    })
}

/// Memory one token takes, and its share of the syntax tree that the parser does not
/// count as it builds the tree (`parser::push_counted`, its boxes): the token itself, and
/// a word for what is made in place. (It was 128 bytes for both, which was 2 to 3 times
/// too little for blocks and statements and too much for the rest: S6-FRONT-3.)
pub(crate) const TOKEN_COST: usize = crate::nostd::mem::size_of::<Token>() + 8;

/// What the allocator adds to a small allocation of its own (a name's or a string's).
const ALLOC_OVERHEAD: usize = 24;

/// Memory a token takes besides `TOKEN_COST`: the bytes of its name or string, which the
/// token holds and the syntax tree copies (S5C-FRONT-9: a 15 MB string literal loaded
/// under a 32 MiB limit held 60 MB, half of it never counted).
pub(crate) fn token_bytes(t: &Tok) -> usize {
    let n = match t {
        Tok::Name(s) => s.len(),
        Tok::Str(s) => s.len(),
        Tok::Interp(parts) => parts
            .iter()
            .map(|p| match p {
                InterpPart::Lit(b) => b.len(),
                InterpPart::Expr(e, _) => e.len(),
            })
            .sum(),
        _ => 0,
    };
    // Each copy is an allocation of its own (S6-FRONT-3: a one-letter name took 64 bytes
    // in its two copies, counted as 2).
    if n == 0 { 0 } else { n.saturating_add(ALLOC_OVERHEAD).saturating_mul(2) }
}

/// More source, appended to the buffer; false at the end of it (`load`'s reader).
pub(crate) type Pull<'a> = &'a mut dyn FnMut(&mut Vec<u8>) -> bool;

/// The text being lexed: whole, or read a piece at a time as the lexer needs it, as C
/// Lua's `zgetc` calls `load`'s reader (so the reader is called as often as in C Lua, and
/// a syntax error stops the reading).
pub(crate) enum Text<'a> {
    Whole(&'a [u8]),
    Read(Vec<u8>, Option<Pull<'a>>),
}

impl Text<'_> {
    /// What has been read.
    pub fn bytes(&self) -> &[u8] {
        match self {
            Text::Whole(s) => s,
            Text::Read(b, _) => b,
        }
    }

    /// Byte `j`, reading on to it first.
    fn byte(&mut self, j: usize) -> Option<u8> {
        if let Text::Read(b, pull) = self {
            while j >= b.len() {
                if !pull.as_mut().is_some_and(|p| p(b)) {
                    *pull = None;
                    break;
                }
            }
        }
        self.bytes().get(j).copied()
    }

    /// The buffer read (empty for whole text).
    pub fn take(self) -> Vec<u8> {
        match self {
            Text::Whole(_) => Vec::new(),
            Text::Read(b, _) => b,
        }
    }
}

pub(crate) struct Lexer<'a> {
    src: Text<'a>,
    i: usize,
    line: u32,
    line_start: usize,
    dialect: Dialect,
    chunk: &'a str,
    /// Where the string being read starts (for "near" in its errors).
    str_start: usize,
    /// Where the escape being read starts (C Lua quotes it as written).
    esc_start: usize,
    /// The memory a chunk's tokens and tree may take: bounded by memory alone.
    memory: usize,
    /// A lexical error was returned: only `Eof` follows.
    failed: bool,
}

/// Compiler scan work (`diag::compile_work`) per byte of an interpolated expression's
/// text: copied here, cloned with its token, lexed and parsed again (calibrated against
/// plain code with `sweep5_sec`'s nested-interpolation test).
const INTERP_WORK_PER_BYTE: usize = 4;

/// Whether `b` read as text (invalid UTF-8 replaced, as `String::from_utf8_lossy` does)
/// trims to nothing. Stops at the first non-blank character (`utf8_chunks` and
/// `from_utf8_lossy` would read the whole text first).
fn is_blank(b: &[u8]) -> bool {
    let mut i = 0;
    while let Some(&c) = b.get(i) {
        if c.is_ascii() {
            if !char::from(c).is_whitespace() {
                return false;
            }
            i += 1;
            continue;
        }
        let w = b.get(i..b.len().min(i + 4)).unwrap_or_default();
        match w.utf8_chunks().next().and_then(|ch| ch.valid().chars().next()) {
            Some(ch) if ch.is_whitespace() => i += ch.len_utf8(),
            _ => return false,
        }
    }
    true
}

impl<'a> Lexer<'a> {
    pub fn new(src: &'a [u8], chunk: &'a str, dialect: Dialect) -> Self {
        Self::of(Text::Whole(src), chunk, dialect)
    }

    pub fn of(src: Text<'a>, chunk: &'a str, dialect: Dialect) -> Self {
        Lexer { src, i: 0, line: 1, line_start: 0, dialect, chunk, str_start: 0, esc_start: 0, memory: usize::MAX, failed: false }
    }

    /// Starts at a position (for an interpolated expression's text).
    pub fn at(src: &'a [u8], chunk: &'a str, dialect: Dialect, pos: Pos) -> Self {
        Lexer { src: Text::Whole(src), i: 0, line: pos.line, line_start: 0usize.wrapping_sub(pos.col as usize - 1), dialect, chunk, str_start: 0, esc_start: 0, memory: usize::MAX, failed: false }
    }

    fn pos(&self) -> Pos {
        Pos { line: self.line, col: (self.i.wrapping_sub(self.line_start) + 1) as u32 }
    }

    fn peek(&mut self) -> u8 {
        self.peek_at(0)
    }

    fn peek_at(&mut self, n: usize) -> u8 {
        self.src.byte(self.i + n).unwrap_or(0)
    }

    fn at_end(&mut self) -> bool {
        self.src.byte(self.i).is_none()
    }

    pub fn chunk(&self) -> &'a str {
        self.chunk
    }

    /// The text read so far.
    pub fn text(&self) -> &[u8] {
        self.src.bytes()
    }

    /// Whether more text may be read (`load`'s reader).
    pub fn reads(&self) -> bool {
        matches!(self.src, Text::Read(_, Some(_)))
    }

    pub fn into_text(self) -> Text<'a> {
        self.src
    }

    /// An error about the escape being read (from its backslash through the current character).
    fn esc_err(&self, msg: Vec<u8>) -> Diagnostic {
        let col = |i: usize| (i.wrapping_sub(self.line_start) + 1) as u32;
        let start = Pos { line: self.line, col: col(self.esc_start) };
        let end = Pos { line: self.line, col: col((self.i + 1).min(self.src.bytes().len())) };
        Diagnostic::error_bytes(self.chunk, start, msg).to(end).lua_line(self.line)
    }

    /// `err` with a message of bytes (quoting source as written).
    fn err_bytes(&self, pos: Pos, msg: Vec<u8>) -> Diagnostic {
        Diagnostic::error_bytes(self.chunk, pos, msg).to(self.pos()).lua_line(self.line)
    }

    /// An error about the text from `pos` to here, which C Lua reports at this line.
    fn err(&self, pos: Pos, msg: impl Into<String>) -> Diagnostic {
        Diagnostic::error(self.chunk, pos, msg).to(self.pos()).lua_line(self.line)
    }

    fn newline(&mut self) {
        // \n, \r, \r\n or \n\r each count as one.
        let c = self.peek();
        self.i += 1;
        let d = self.peek();
        if (d == b'\n' || d == b'\r') && d != c {
            self.i += 1;
        }
        self.line += 1;
        self.line_start = self.i;
    }

    /// Limits the tokens to what `bytes` of memory can hold with their syntax tree.
    pub fn with_memory(mut self, bytes: usize) -> Self {
        self.memory = bytes;
        self
    }

    /// The next token, and whether the host said to stop (kept: the parser's next look
    /// at the interrupt sees it too). A lexical error becomes a token (`Tok::Error`, then
    /// `Eof`): the parser reports it only when it gets there, so an earlier syntax error
    /// is reported first (as C Lua, which reads tokens as it parses).
    pub fn token(&mut self) -> (Token, bool) {
        if self.failed {
            return (Token { tok: Tok::Eof, pos: self.pos(), span: (self.i, self.i), end: self.pos() }, false);
        }
        let start = self.i;
        let mut t = match self.next() {
            Ok(t) => t,
            Err(d) => {
                self.failed = true;
                let pos = d.pos;
                return (Token { tok: Tok::Error(Box::new(d)), pos, span: (self.i, self.i), end: pos }, false);
            }
        };
        t.span.1 = self.i;
        t.end = self.pos();
        if t.tok != Tok::Eof {
            // C Lua has read the character after a token too (its `current`).
            self.peek();
        }
        // A step per token, and per 64 bytes it spans: one interpolation's token can span
        // the whole text, read again at each level of nesting (S5SEC-1).
        let read = self.i.saturating_sub(start) / 64;
        (t, crate::diag::compile_ticks(u32::try_from(read).unwrap_or(u32::MAX).saturating_add(1)))
    }

    pub fn tokens(mut self) -> Result<Vec<Token>, Diagnostic> {
        let mut out = Vec::new();
        let mut held = 0usize;
        loop {
            let (t, stop) = self.token();
            if stop {
                // The tokens so far are freed later (S6-FRONT-4: millions of them took up
                // to 60 ms to free before the stopped load returned).
                crate::diag::defer_pieces(out);
                return Err(self.err(self.pos(), crate::diag::COMPILE_STOPPED));
            }
            if matches!(t.tok, Tok::Error(_)) {
                let (pos, span) = (t.pos, t.span);
                out.push(t);
                out.push(Token { tok: Tok::Eof, pos, span, end: pos });
                return Ok(out);
            }
            let eof = t.tok == Tok::Eof;
            held = held.saturating_add(TOKEN_COST).saturating_add(token_bytes(&t.tok));
            // Growing the vector holds the old buffer and the new, twice its size, at once
            // (S6-FRONT-3: a big source's tokens took 1.4 times the limit right then).
            if out.len() == out.capacity() && !eof {
                let grown = out.capacity().saturating_mul(2).saturating_mul(crate::nostd::mem::size_of::<Token>());
                if held.saturating_add(grown) > self.memory {
                    return Err(self.err(self.pos(), "not enough memory"));
                }
                // Between looks at the interrupt, a stretch at a time (doubling millions of
                // tokens in one `realloc` took 20 to 60 ms).
                let cap = out.capacity().saturating_mul(2).max(4);
                if crate::diag::grow_watched(&mut out, cap) {
                    return Err(self.err(self.pos(), crate::diag::COMPILE_STOPPED));
                }
            }
            out.push(t);
            if eof {
                return Ok(out);
            }
            if held > self.memory {
                return Err(self.err(self.pos(), "not enough memory"));
            }
        }
    }

    fn next(&mut self) -> Result<Token, Diagnostic> {
        // Blanks, comments and names look as long tokens do (SEC7-N3).
        let mut look = self.i + (1 << 16);
        loop {
            self.long_look(&mut look)?;
            let c = self.peek();
            match c {
                b'\n' | b'\r' => self.newline(),
                b' ' | b'\t' | 0x0b | 0x0c => self.i += 1,
                b'-' if self.peek_at(1) == b'-' => {
                    // A long comment's errors are about it from its `--`.
                    let p = self.pos();
                    self.i += 2;
                    if self.peek() == b'[' {
                        let save = (self.i, self.line, self.line_start);
                        if let Some(level) = self.long_bracket_level() {
                            self.long_string(level, p, true)?;
                            look = self.i + (1 << 16);
                            continue;
                        }
                        (self.i, self.line, self.line_start) = save;
                    }
                    while !self.at_end() && self.peek() != b'\n' && self.peek() != b'\r' {
                        self.long_look(&mut look)?;
                        self.i += 1;
                    }
                }
                _ => break,
            }
        }
        let pos = self.pos();
        if self.at_end() {
            return Ok(Token { tok: Tok::Eof, pos, span: (self.i, self.i), end: pos });
        }
        let c = self.peek();
        let rl = self.dialect == Dialect::RemedyLua;
        // Read no further than C Lua's `llex` does (one character past each token, so a
        // reader is called as often): a third character only after `..` or `//`.
        let two = [c, self.peek_at(1)];
        let three = if matches!(&two, b".." | b"//") { [c, two[1], self.peek_at(2)] } else { [c, two[1], 0] };
        let tok_start = self.i;
        let sym = |t: Tok, n: usize, s: &mut Self| {
            s.i += n;
            Ok(Token { tok: t, pos, span: (tok_start, 0), end: pos })
        };
        use crate::ast::BinOp;
        if rl {
            match &three {
                b"//=" => return sym(Tok::OpAssign(BinOp::IDiv), 3, self),
                b"..=" => return sym(Tok::OpAssign(BinOp::Concat), 3, self),
                _ => {}
            }
            match &two {
                b"+=" => return sym(Tok::OpAssign(BinOp::Add), 2, self),
                b"-=" => return sym(Tok::OpAssign(BinOp::Sub), 2, self),
                b"*=" => return sym(Tok::OpAssign(BinOp::Mul), 2, self),
                b"/=" => return sym(Tok::OpAssign(BinOp::Div), 2, self),
                b"%=" => return sym(Tok::OpAssign(BinOp::Mod), 2, self),
                b"->" => return sym(Tok::Arrow, 2, self),
                _ => {}
            }
            if c == b'?' {
                return sym(Tok::Question, 1, self);
            }
            if c == b'`' {
                return self.interp(pos);
            }
        }
        if &three == b"..." {
            return sym(Tok::Dots, 3, self);
        }
        match &two {
            b"//" => return sym(Tok::DSlash, 2, self),
            b"<<" => return sym(Tok::Shl, 2, self),
            b">>" => return sym(Tok::Shr, 2, self),
            b"==" => return sym(Tok::Eq, 2, self),
            b"~=" => return sym(Tok::Ne, 2, self),
            b"<=" => return sym(Tok::Le, 2, self),
            b">=" => return sym(Tok::Ge, 2, self),
            b"::" => return sym(Tok::DColon, 2, self),
            b".." if !self.peek_at(2).is_ascii_digit() => return sym(Tok::Concat, 2, self),
            _ => {}
        }
        let single = match c {
            b'+' => Some(Tok::Plus),
            b'-' => Some(Tok::Minus),
            b'*' => Some(Tok::Star),
            b'/' => Some(Tok::Slash),
            b'%' => Some(Tok::Percent),
            b'^' => Some(Tok::Caret),
            b'#' => Some(Tok::Hash),
            b'&' => Some(Tok::Amp),
            b'~' => Some(Tok::Tilde),
            b'|' => Some(Tok::Pipe),
            b'<' => Some(Tok::Lt),
            b'>' => Some(Tok::Gt),
            b'=' => Some(Tok::Assign),
            b'(' => Some(Tok::LParen),
            b')' => Some(Tok::RParen),
            b'{' => Some(Tok::LBrace),
            b'}' => Some(Tok::RBrace),
            b']' => Some(Tok::RBracket),
            b';' => Some(Tok::Semi),
            b':' => Some(Tok::Colon),
            b',' => Some(Tok::Comma),
            _ => None,
        };
        if let Some(t) = single {
            return sym(t, 1, self);
        }
        if c == b'[' {
            if let Some(level) = self.long_bracket_level() {
                let s = self.long_string(level, pos, false)?;
                return Ok(Token { tok: Tok::Str(s), pos, span: (tok_start, 0), end: pos });
            }
            if self.peek_at(1) == b'=' {
                // `[=` not followed by `=`s and `[`.
                let mut j = self.i + 1;
                while self.src.byte(j) == Some(b'=') {
                    j += 1;
                }
                let text = String::from_utf8_lossy(self.src.bytes().get(self.i..j).unwrap_or_default()).into_owned();
                return Err(self.err(pos, format!("invalid long string delimiter near '{text}'")));
            }
            return sym(Tok::LBracket, 1, self);
        }
        if c == b'"' || c == b'\'' {
            let s = self.short_string(c, pos)?;
            return Ok(Token { tok: Tok::Str(s), pos, span: (tok_start, 0), end: pos });
        }
        if c == b'.' {
            if self.peek_at(1).is_ascii_digit() {
                return self.number(pos);
            }
            if self.peek_at(1) == b'.' {
                return sym(Tok::Concat, 2, self);
            }
            return sym(Tok::Dot, 1, self);
        }
        if c.is_ascii_digit() {
            return self.number(pos);
        }
        if c.is_ascii_alphabetic() || c == b'_' {
            let start = self.i;
            while self.peek().is_ascii_alphanumeric() || self.peek() == b'_' {
                self.long_look(&mut look)?;
                self.i += 1;
            }
            let word = String::from_utf8_lossy(self.src.bytes().get(start..self.i).unwrap_or_default()).into_owned();
            let tok = keyword(&word).unwrap_or(Tok::Name(word));
            return Ok(Token { tok, pos, span: (tok_start, 0), end: pos });
        }
        // Any other character is a token of its own (the parser rejects it where it is).
        sym(Tok::Other(c), 1, self)
    }

    /// Whether a long bracket opens at `at` (without consuming it).
    fn long_bracket_level_at(&mut self, at: usize) -> Option<usize> {
        let mut j = at + 1;
        let mut level = 0;
        while self.src.byte(j) == Some(b'=') {
            level += 1;
            j += 1;
        }
        (self.src.byte(at) == Some(b'[') && self.src.byte(j) == Some(b'[')).then_some(level)
    }

    /// At `[`: the level of a long bracket `[==[`, consuming it, or None (nothing consumed).
    fn long_bracket_level(&mut self) -> Option<usize> {
        let mut j = self.i + 1;
        let mut level = 0;
        while self.src.byte(j) == Some(b'=') {
            level += 1;
            j += 1;
        }
        if self.src.byte(j) == Some(b'[') {
            self.i = j + 1;
            Some(level)
        } else {
            None
        }
    }

    fn long_string(&mut self, level: usize, start: Pos, comment: bool) -> Result<Vec<u8>, Diagnostic> {
        let mut out = Vec::new();
        // A newline right after the opening bracket is skipped.
        if self.peek() == b'\n' || self.peek() == b'\r' {
            self.newline();
        }
        let mut look = self.i + (1 << 16);
        loop {
            self.long_look(&mut look)?;
            if self.at_end() {
                let what = if comment { "unfinished long comment" } else { "unfinished long string" };
                // About the whole string or comment; Lua reports it where the text ends.
                return Err(self.err(start, format!("{what} (starting at line {}) near <eof>", start.line)));
            }
            let c = self.peek();
            if c == b']' {
                let mut j = self.i + 1;
                let mut l = 0;
                while self.src.byte(j) == Some(b'=') {
                    l += 1;
                    j += 1;
                }
                if l == level && self.src.byte(j) == Some(b']') {
                    self.i = j + 1;
                    return Ok(out);
                }
                out.push(c);
                self.i += 1;
            } else if c == b'\n' || c == b'\r' {
                out.push(b'\n');
                self.newline();
            } else {
                if !comment {
                    out.push(c);
                }
                self.i += 1;
            }
        }
    }

    /// `what near '...'`, with the string read so far as llex quotes it in errors (plus the
    /// current character). As C Lua's buffer holds it: the quote, the text read so far
    /// with its escapes applied (`out`), then the escape being read as written; cut at a
    /// NUL byte, like `%s`. Bytes as they are (S6-FRONT-7: they were made UTF-8, where C
    /// Lua's message has them as written).
    fn near_buf(&self, what: &str, out: &[u8], with_current: bool) -> Vec<u8> {
        let end = if with_current { self.i + 1 } else { self.i };
        let mut b = self.src.bytes().get(self.str_start..self.str_start + 1).unwrap_or_default().to_vec();
        b.extend_from_slice(out);
        b.extend_from_slice(self.src.bytes().get(self.esc_start.min(end)..end.min(self.src.bytes().len())).unwrap_or_default());
        let cut = b.iter().position(|&c| c == 0).unwrap_or(b.len());
        b.truncate(cut);
        let mut m = format!("{what} near '").into_bytes();
        m.extend_from_slice(&b);
        m.push(b'\'');
        m
    }

    /// A look at the host's interrupt and deadline every 64 KiB of a long token (a 16 MiB
    /// string was read in one go, 20 ms with no look).
    fn long_look(&mut self, next: &mut usize) -> Result<(), Diagnostic> {
        if self.i >= *next {
            *next = self.i + (1 << 16);
            if crate::diag::compile_ticks(1024) {
                return Err(self.err(self.pos(), crate::diag::COMPILE_STOPPED));
            }
        }
        Ok(())
    }

    fn short_string(&mut self, quote: u8, start: Pos) -> Result<Vec<u8>, Diagnostic> {
        self.str_start = self.i;
        self.i += 1;
        let mut out = Vec::new();
        let mut look = self.i + (1 << 16);
        loop {
            self.long_look(&mut look)?;
            if self.at_end() {
                return Err(self.err(start, "unfinished string near <eof>"));
            }
            let c = self.peek();
            if c == quote {
                self.i += 1;
                return Ok(out);
            }
            match c {
                b'\n' | b'\r' => {
                    self.esc_start = self.i;
                    return Err(self.err_bytes(start, self.near_buf("unfinished string", &out, false)));
                }
                b'\\' => {
                    self.esc_start = self.i;
                    self.i += 1;
                    self.escape(&mut out)?;
                }
                _ => {
                    out.push(c);
                    self.i += 1;
                }
            }
        }
    }

    /// After a backslash: one escape sequence.
    fn escape(&mut self, out: &mut Vec<u8>) -> Result<(), Diagnostic> {
        let pos = self.pos();
        let c = self.peek();
        let simple = match c {
            b'a' => Some(7),
            b'b' => Some(8),
            b'f' => Some(12),
            b'n' => Some(b'\n'),
            b'r' => Some(b'\r'),
            b't' => Some(b'\t'),
            b'v' => Some(11),
            b'\\' => Some(b'\\'),
            b'"' => Some(b'"'),
            b'\'' => Some(b'\''),
            b'`' | b'{' | b'}' if self.dialect == Dialect::RemedyLua => Some(c),
            _ => None,
        };
        if let Some(b) = simple {
            out.push(b);
            self.i += 1;
            return Ok(());
        }
        match c {
            b'\n' | b'\r' => {
                out.push(b'\n');
                self.newline();
            }
            b'z' => {
                self.i += 1;
                loop {
                    match self.peek() {
                        b'\n' | b'\r' => self.newline(),
                        b' ' | b'\t' | 0x0b | 0x0c => self.i += 1,
                        _ => break,
                    }
                }
            }
            b'x' => {
                self.i += 1;
                let mut v = 0u32;
                for _ in 0..2 {
                    let Some(d) = char::from(self.peek()).to_digit(16) else {
                        return Err(self.esc_err(self.near_buf("hexadecimal digit expected", out, true)));
                    };
                    v = v * 16 + d;
                    self.i += 1;
                }
                out.push(v as u8);
            }
            b'u' => {
                self.i += 1;
                if self.peek() != b'{' {
                    return Err(self.esc_err(self.near_buf("missing '{'", out, true)));
                }
                self.i += 1;
                let mut v: u64 = 0;
                let mut n = 0;
                while let Some(d) = char::from(self.peek()).to_digit(16) {
                    v = v * 16 + u64::from(d);
                    if v > 0x7FFF_FFFF {
                        return Err(self.esc_err(self.near_buf("UTF-8 value too large", out, true)));
                    }
                    self.i += 1;
                    n += 1;
                }
                if n == 0 {
                    return Err(self.esc_err(self.near_buf("hexadecimal digit expected", out, true)));
                }
                if self.peek() != b'}' {
                    return Err(self.esc_err(self.near_buf("missing '}'", out, true)));
                }
                self.i += 1;
                utf8_encode(v as u32, out);
            }
            c if c.is_ascii_digit() => {
                let mut v = 0u32;
                for _ in 0..3 {
                    if !self.peek().is_ascii_digit() {
                        break;
                    }
                    v = v * 10 + u32::from(self.peek() - b'0');
                    self.i += 1;
                }
                if v > 255 {
                    return Err(self.esc_err(self.near_buf("decimal escape too large", out, true)));
                }
                out.push(v as u8);
            }
            _ => {
                if self.at_end() {
                    return Err(self.err(pos, "unfinished string near <eof>"));
                }
                return Err(self.esc_err(self.near_buf("invalid escape sequence", out, true)));
            }
        }
        Ok(())
    }

    fn number(&mut self, pos: Pos) -> Result<Token, Diagnostic> {
        let start = self.i;
        // C Lua tests for `0x` at the character after a leading '.' too (`.0x1p4`).
        let lead = usize::from(self.peek() == b'.');
        let hex = self.peek_at(lead) == b'0' && matches!(self.peek_at(lead + 1), b'x' | b'X');
        let (e1, e2) = if hex { (b'p', b'P') } else { (b'e', b'E') };
        if hex {
            self.i += lead + 2;
        }
        let mut look = self.i + (1 << 16);
        loop {
            self.long_look(&mut look)?;
            let c = self.peek();
            if c == e1 || c == e2 {
                self.i += 1;
                if matches!(self.peek(), b'+' | b'-') {
                    self.i += 1;
                }
            } else if c.is_ascii_hexdigit() || c == b'.' {
                self.i += 1;
            } else {
                break;
            }
        }
        // A numeral touching a letter takes that one letter (and is malformed), like Lua.
        if self.peek().is_ascii_alphabetic() || self.peek() == b'_' {
            self.i += 1;
        }
        let text = self.src.bytes().get(start..self.i).unwrap_or_default();
        match crate::number::str_to_number(text) {
            Some(crate::Value::Int(i)) => Ok(Token { tok: Tok::Int(i), pos, span: (start, 0), end: pos }),
            Some(crate::Value::Num(f)) => Ok(Token { tok: Tok::Num(f), pos, span: (start, 0), end: pos }),
            _ => Err(self.err(pos, format!("malformed number near '{}'", String::from_utf8_lossy(text)))),
        }
    }

    /// A backtick string: `` `text {expr} text` ``. `\{` and `\}` are literal braces.
    fn interp(&mut self, pos: Pos) -> Result<Token, Diagnostic> {
        let tok_start = self.i;
        self.i += 1;
        let mut parts = Vec::new();
        let mut lit = Vec::new();
        loop {
            if self.at_end() {
                return Err(self.err(pos, "unfinished interpolated string near <eof>"));
            }
            let c = self.peek();
            match c {
                b'`' => {
                    self.i += 1;
                    break;
                }
                b'\\' => {
                    // Where an escape error starts and what it quotes, as `short_string`
                    // sets them (S6-FRONT-6: left from the last string read, the column was
                    // 4294967247 and the quote held all the source since that string).
                    self.str_start = tok_start;
                    self.esc_start = self.i;
                    self.i += 1;
                    self.escape(&mut lit)?;
                }
                b'\n' | b'\r' => {
                    lit.push(b'\n');
                    self.newline();
                }
                b'{' => {
                    if !lit.is_empty() {
                        parts.push(InterpPart::Lit(crate::nostd::mem::take(&mut lit)));
                    }
                    self.i += 1;
                    let epos = self.pos();
                    let start = self.i;
                    let mut depth = 0usize;
                    loop {
                        if self.at_end() {
                            return Err(self.err(epos, "unfinished '{' in interpolated string"));
                        }
                        match self.peek() {
                            b'{' => {
                                depth += 1;
                                self.i += 1;
                            }
                            b'}' if depth == 0 => break,
                            b'}' => {
                                depth -= 1;
                                self.i += 1;
                            }
                            q @ (b'"' | b'\'') => {
                                let p = self.pos();
                                self.short_string(q, p)?;
                            }
                            b'[' if self.long_bracket_level_at(self.i).is_some() => {
                                let p = self.pos();
                                if let Some(level) = self.long_bracket_level() {
                                    self.long_string(level, p, false)?;
                                }
                            }
                            b'-' if self.peek_at(1) == b'-' => {
                                self.i += 2;
                                let p = self.pos();
                                if self.peek() == b'[' && self.long_bracket_level_at(self.i).is_some() {
                                    if let Some(level) = self.long_bracket_level() {
                                        self.long_string(level, p, true)?;
                                    }
                                } else {
                                    while !self.at_end() && self.peek() != b'\n' && self.peek() != b'\r' {
                                        self.i += 1;
                                    }
                                }
                            }
                            b'`' => {
                                let p = self.pos();
                                self.skip_interp(p)?;
                            }
                            b'\n' | b'\r' => self.newline(),
                            _ => self.i += 1,
                        }
                    }
                    // The bytes as written (S6-FRONT-5: `from_utf8_lossy` turned `"\xFF"`
                    // written raw into three bytes of U+FFFD).
                    let text = self.src.bytes().get(start..self.i).unwrap_or_default().to_vec();
                    self.i += 1;
                    if is_blank(&text) {
                        return Err(self.err(epos, "empty expression in interpolated string"));
                    }
                    // The text is copied and lexed again by the parser, once per enclosing
                    // interpolation: nested n deep, that is quadratic in the source, while
                    // `load` charges its length once. Paid for as scan work (S5SEC-1).
                    crate::diag::compile_work(text.len().saturating_mul(INTERP_WORK_PER_BYTE));
                    parts.push(InterpPart::Expr(text, epos));
                }
                _ => {
                    lit.push(c);
                    self.i += 1;
                }
            }
        }
        if !lit.is_empty() || parts.is_empty() {
            parts.push(InterpPart::Lit(lit));
        }
        // The span starts at the backtick (S5C-FRONT-1): from 0, every end position taken
        // of the token (`token_end`) walked the source before it, so n interpolated
        // strings parsed in time quadratic in the source, and their ends were wrong.
        Ok(Token { tok: Tok::Interp(parts), pos, span: (tok_start, 0), end: pos })
    }

    /// Skips an interpolated string inside an interpolated expression, with the same
    /// errors as `interp` reading it. Strings nest in expressions nest in strings: the
    /// levels are a list here, not native frames.
    fn skip_interp(&mut self, pos: Pos) -> Result<(), Diagnostic> {
        enum Level {
            /// Where the string starts, and the index of its backtick.
            Str(Pos, usize),
            Code { epos: Pos, start: usize, depth: usize },
        }
        let mut scratch = Vec::new();
        let tick = self.i;
        self.i += 1;
        let mut levels = vec![Level::Str(pos, tick)];
        while let Some(level) = levels.last_mut() {
            match level {
                Level::Str(pos, tick) => {
                    if self.at_end() {
                        return Err(self.err(*pos, "unfinished interpolated string near <eof>"));
                    }
                    match self.peek() {
                        b'`' => {
                            self.i += 1;
                            levels.pop();
                        }
                        b'\\' => {
                            // As in `interp` (S6-FRONT-6); what this string's text held
                            // before the escape is not kept here, so the quote is the
                            // backtick and the escape.
                            self.str_start = *tick;
                            self.esc_start = self.i;
                            self.i += 1;
                            scratch.clear();
                            self.escape(&mut scratch)?;
                        }
                        b'\n' | b'\r' => self.newline(),
                        b'{' => {
                            self.i += 1;
                            let epos = self.pos();
                            levels.push(Level::Code { epos, start: self.i, depth: 0 });
                        }
                        _ => self.i += 1,
                    }
                }
                Level::Code { epos, start, depth } => {
                    if self.at_end() {
                        return Err(self.err(*epos, "unfinished '{' in interpolated string"));
                    }
                    match self.peek() {
                        b'{' => {
                            *depth += 1;
                            self.i += 1;
                        }
                        b'}' if *depth == 0 => {
                            // Only whether it is blank: copying the text here (as `interp`
                            // does) made each skip quadratic in the nesting (S5SEC-1).
                            let blank = is_blank(self.src.bytes().get(*start..self.i).unwrap_or_default());
                            let epos = *epos;
                            self.i += 1;
                            if blank {
                                return Err(self.err(epos, "empty expression in interpolated string"));
                            }
                            levels.pop();
                        }
                        b'}' => {
                            *depth -= 1;
                            self.i += 1;
                        }
                        q @ (b'"' | b'\'') => {
                            let p = self.pos();
                            self.short_string(q, p)?;
                        }
                        b'[' if self.long_bracket_level_at(self.i).is_some() => {
                            let p = self.pos();
                            if let Some(level) = self.long_bracket_level() {
                                self.long_string(level, p, false)?;
                            }
                        }
                        b'-' if self.peek_at(1) == b'-' => {
                            self.i += 2;
                            let p = self.pos();
                            if self.peek() == b'[' && self.long_bracket_level_at(self.i).is_some() {
                                if let Some(level) = self.long_bracket_level() {
                                    self.long_string(level, p, true)?;
                                }
                            } else {
                                while !self.at_end() && self.peek() != b'\n' && self.peek() != b'\r' {
                                    self.i += 1;
                                }
                            }
                        }
                        b'`' => {
                            let p = self.pos();
                            let tick = self.i;
                            self.i += 1;
                            levels.push(Level::Str(p, tick));
                        }
                        b'\n' | b'\r' => self.newline(),
                        _ => self.i += 1,
                    }
                }
            }
        }
        Ok(())
    }
}

/// Lua's UTF-8 encoding, which takes values up to 2^31 (six-byte forms).
pub(crate) fn utf8_encode(x: u32, out: &mut Vec<u8>) {
    if x < 0x80 {
        out.push(x as u8);
        return;
    }
    let mut buf = [0u8; 8];
    let mut n = 1;
    let mut x = x;
    let mut mfb: u32 = 0x3f;
    loop {
        if let Some(b) = buf.get_mut(8 - n) {
            *b = (0x80 | (x & 0x3f)) as u8;
        }
        n += 1;
        x >>= 6;
        mfb >>= 1;
        if x <= mfb {
            break;
        }
    }
    if let Some(b) = buf.get_mut(8 - n) {
        *b = ((!mfb << 1) | x) as u8;
    }
    out.extend_from_slice(buf.get(8 - n..).unwrap_or_default());
}
