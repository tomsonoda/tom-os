//! The parser: tokens to a syntax tree, Lua 5.4's grammar plus RemedyLua's additions when
//! the dialect allows them.
//!
//! It does not recurse. Every construct that nests (expressions, blocks, function bodies,
//! table constructors, call arguments, interpolations, type annotations) keeps its partial
//! state in a frame on an explicit stack; a frame either finishes with a value in `ret` or
//! pushes its continuation and then the construct it waits for. The stack is charged to
//! the memory the chunk may use, so nesting is bounded by memory alone ("not enough
//! memory"), never by a level count or the native stack.
//!
//! It is checked against the recursive parser it replaced (`front_oracle`, feature
//! `front-oracle`): same tree, or the same error at the same place.

#[allow(unused_imports)]
use crate::nostd::prelude::*;
use crate::Dialect;
use crate::ast::*;
use crate::diag::{Diagnostic, Pos};
use crate::lexer::{InterpPart, Lexer, TOKEN_COST, Text, Tok, Token, token_bytes};

type PResult<T> = Result<T, Diagnostic>;

/// Parses a whole chunk.
pub(crate) fn parse(src: &[u8], chunk: &str, dialect: Dialect) -> PResult<Block> {
    parse_within(src, chunk, dialect, usize::MAX)
}

/// Parses within `memory` bytes for the tokens, the tree and the parser's stack (counted
/// before they are made: a chunk too big or too deep for the memory left fails with "not
/// enough memory").
pub(crate) fn parse_within(src: &[u8], chunk: &str, dialect: Dialect, memory: usize) -> PResult<Block> {
    parse_counted(Text::Whole(src), chunk, dialect, memory, false).0.map(|(b, _)| b)
}

crate::thread_local! {
    /// The heap bytes the tree of this thread's last successful parse holds: its vectors
    /// at capacity, boxes, names and strings, as counted while it was built (`tree_bytes`).
    static TREE_BYTES: crate::nostd::cell::Cell<usize> = const { crate::nostd::cell::Cell::new(0) };
}

/// The heap bytes the tree of this thread's last successful parse holds (what it is
/// charged while it waits to be freed, S6-FRONT-4: counted as it was built, as walking a
/// big tree to measure it took tens of milliseconds).
pub(crate) fn tree_bytes() -> usize {
    TREE_BYTES.with(crate::nostd::cell::Cell::get)
}

/// `parse_within`, and the bytes the syntax tree holds (counted as it was built: kept
/// against the memory limit while the tree is compiled, S6-FRONT-3).
/// With `defer`, the tokens are freed a piece at a time, watching the interrupt (left to
/// `diag::free_deferred` once told to stop): freeing millions of them took tens of
/// milliseconds between the parse's last look at the host's interrupt and the compiler's
/// first (S6-FRONT-4).
/// Also the text read (`src` may be read as it is parsed).
pub(crate) fn parse_counted<'a>(src: Text<'a>, chunk: &'a str, dialect: Dialect, memory: usize, defer: bool) -> (PResult<(Block, usize)>, Text<'a>) {
    let (r, _, tree, toks, lex) = parse_tokens(Vec::new(), Lexer::of(src, chunk, dialect), true, dialect, memory, false);
    // (Inside a call too: millions of tokens dropped at once were a 45 ms stretch there.)
    let _ = defer;
    crate::diag::free_watched(toks);
    (r.map(|b| (b, tree)), lex.into_text())
}

/// The memory a parse for `check` or `format` may use: they have no state, so no memory
/// limit of their own. Nested interpolations take memory quadratic in the source
/// (S5SEC-1): unbounded, 100 KB of them took 1.9 GB (S5C-FRONT-3). 1 KiB per source byte
/// is far more than any other construct takes, however nested, plus a default state's
/// whole memory.
pub(crate) fn host_memory(len: usize) -> usize {
    len.saturating_mul(1024).saturating_add(64 << 20)
}

/// The line given to what `prefix` closes at the cut: past any real line, so nothing
/// found there comes before the syntax error.
pub(crate) const CUT_LINE: u32 = u32::MAX / 2;

/// What C Lua had compiled when it met the syntax error in `src`: the statements before
/// the error, with every construct still open there closed (C Lua compiles as it parses,
/// so a compile error in them is reported first). None when there is no syntax error or
/// nothing can be made of the text before it. Also where the cut is (the blocks closed
/// there end at it) and whether the bad token ends a block (`end`, `else`, ...).
pub(crate) fn prefix(src: &[u8], chunk: &str, dialect: Dialect, memory: usize) -> Option<(Block, Pos, bool)> {
    let toks = Lexer::new(src, chunk, dialect).with_memory(memory).tokens().ok()?;
    // The tokens are kept for the second parse while the first parses a copy: that copy's
    // buffer is not memory the first parse may use (S6-FRONT-3).
    let copy = toks.len().saturating_mul(crate::nostd::mem::size_of::<Token>());
    let (r, at, _, _, _) = parse_tokens(toks.clone(), Lexer::new(src, chunk, dialect), false, dialect, memory.saturating_sub(copy), false);
    if r.is_ok() {
        return None;
    }
    // The cut stands where the bad token was: C Lua had read it (and numbers what it then
    // checked by its line), unless it was a lexical error, which stops reading at once.
    let pos = match toks.get(at) {
        Some(t) if !matches!(t.tok, Tok::Error(_)) => Pos { line: t.end.line, col: t.pos.col },
        _ => Pos { line: CUT_LINE, col: 1 },
    };
    let follow = toks.get(at).is_some_and(|t| matches!(t.tok, Tok::Else | Tok::Elseif | Tok::End | Tok::Eof));
    let mut cut: Vec<Token> = toks.into_iter().take(at).collect();
    let end = cut.last().map_or(0, |t| t.span.1);
    cut.push(Token { tok: Tok::Eof, pos, span: (end, end), end: pos });
    parse_tokens(cut, Lexer::new(src, chunk, dialect), false, dialect, memory, true).0.ok().map(|b| (b, pos, follow))
}

/// Parses `toks`, then (with `more`) the rest of what `lex` reads; also the index of the
/// token where it stopped, the bytes the tree holds, the tokens and the lexer. With
/// `recover`, the end of the tokens closes whatever is open (see `prefix`).
#[allow(clippy::type_complexity)]
fn parse_tokens<'a>(
    toks: Vec<Token>,
    lex: Lexer<'a>,
    more: bool,
    dialect: Dialect,
    memory: usize,
    recover: bool,
) -> (PResult<Block>, usize, usize, Vec<Token>, Lexer<'a>) {
    let tok_held = tokens_held(&toks);
    let chunk = lex.chunk();
    let mut p = Parser {
        toks,
        base: 0,
        pins: Vec::new(),
        lex,
        more,
        tok_held,
        fail: None,
        sub_src: None,
        i: 0,
        chunk,
        dialect,
        vararg: vec![true],
        frames: vec![Frame::StartBlock],
        stats: Vec::new(),
        room: memory,
        tree: 0,
        strings: 0,
        sub_held: 0,
        ret: Val::None,
        recover,
    };
    p.fill(0);
    if let Err(d) = p.run() {
        if d.message == crate::diag::COMPILE_STOPPED {
            // Freed later, so the stopped compile returns at once (S5C-FRONT-12).
            p.defer_state();
        }
        return (Err(d), p.i, p.tree, crate::nostd::mem::take(&mut p.toks), p.lex);
    }
    let b = p.take_block();
    if !p.check(&Tok::Eof) {
        return (Err(p.error_near("<eof> expected")), p.i, p.tree, crate::nostd::mem::take(&mut p.toks), p.lex);
    }
    // What the tree holds besides the vectors and boxes counted as it was built: its share
    // of each token (`TOKEN_COST` and `token_bytes`, less the token itself, freed now).
    let tree = p.tree.saturating_add(p.tok_held.saturating_sub(p.toks.len().saturating_mul(crate::nostd::mem::size_of::<Token>())));
    TREE_BYTES.with(|t| t.set(p.tree.saturating_add(p.strings)));
    (Ok(b), p.i, tree, crate::nostd::mem::take(&mut p.toks), p.lex)
}

struct Parser<'a> {
    /// The tokens read so far: the chunk's are read one at a time as the parser moves on
    /// (as C Lua's `luaX_next`), from `lex` while `more`.
    toks: Vec<Token>,
    /// The index of `toks[0]`.
    base: usize,
    /// Where return type lists being read started (`ret_types`): kept, to be read again.
    pins: Vec<usize>,
    /// The chunk's text and the lexer reading it (to quote tokens in messages).
    lex: Lexer<'a>,
    more: bool,
    /// What the chunk's tokens take (`TOKEN_COST` and `token_bytes`), counted in `held`.
    tok_held: usize,
    /// Why reading a token failed (the memory limit or the host's stop): reported in
    /// place of whatever the step that wanted the token made of its absence.
    fail: Option<Diagnostic>,
    /// The text of the interpolated expression being parsed, when `toks` are its tokens.
    sub_src: Option<Vec<u8>>,
    i: usize,
    chunk: &'a str,
    dialect: Dialect,
    /// Per function being parsed: whether it is variadic.
    vararg: Vec<bool>,
    frames: Vec<Frame>,
    /// The statements of each block being parsed (a finished statement goes straight in).
    stats: Vec<Vec<Stat>>,
    /// The memory for the parse: the tokens, the tree, the parser's stacks and
    /// interpolated expressions.
    room: usize,
    /// Bytes the tree's vectors and boxes hold (`push_counted`).
    tree: usize,
    /// Bytes of the names and strings the tree holds (copied from the tokens).
    strings: usize,
    /// Bytes held by the interpolated expressions being parsed: each level's text, its
    /// copy and its tokens, kept until the level is done. Nested n deep they add up to the
    /// square of the source (S5SEC-1), so they count against `room` too.
    sub_held: usize,
    /// What the last finished construct made.
    ret: Val,
    /// Parsing the text before a syntax error (`prefix`): the end closes what is open.
    recover: bool,
}

/// What a finished construct hands to the frame waiting for it.
enum Val {
    None,
    Expr(Expr),
    Exprs(Vec<Expr>),
    Block(Block),
    Func(Box<FuncBody>),
    Type(Type),
    Types(Vec<Type>),
}

/// A function's parameter list as far as it is read.
struct FnHead {
    is_method: bool,
    pos: Pos,
    params: Vec<Param>,
    vararg: bool,
    vararg_ty: Option<Type>,
}

struct NumFor {
    open: Pos,
    var: LocalName,
    start: Option<Expr>,
    limit: Option<Expr>,
    step: Option<Expr>,
    do_line: u32,
}

/// The tokens being parsed when an interpolated expression's took their place.
struct Saved {
    toks: Vec<Token>,
    sub_src: Option<Vec<u8>>,
    i: usize,
    base: usize,
}

enum FieldKind {
    Named(String, Pos),
    Key,
    KeyVal(Expr),
    Positional,
}

/// A step of the parse: `Start*` begin a construct; the rest continue one with the value
/// of what they waited for in `ret`.
enum Frame {
    StartBlock,
    StartExpr(u8),
    StartTy,
    // ---- expressions ----
    /// A unary operator's operand is in; then the operator loop at `limit`.
    Unary {
        op: UnOp,
        pos: Pos,
        limit: u8,
    },
    /// The left operand is in: the operator loop at the limit.
    Loop(u8),
    BinRight {
        limit: u8,
        op: BinOp,
        opos: Pos,
        left: Expr,
    },
    /// `a .. b .. c` (right-associative, one node): the parts so far.
    ConcatPart {
        limit: u8,
        pos: Pos,
        parts: Vec<Expr>,
    },
    ParenClose(Pos),
    SuffixIndex {
        e: Expr,
        pos: Pos,
    },
    SuffixCall {
        e: Expr,
        pos: Pos,
    },
    SuffixMethod {
        e: Box<Expr>,
        name: String,
        npos: Pos,
        pos: Pos,
    },
    WrapFunc(Pos),
    ArgsClose(Pos),
    WrapArg,
    ExprList(Vec<Expr>),
    TableField {
        pos: Pos,
        fields: Vec<Field>,
        kind: FieldKind,
    },
    Interp {
        pos: Pos,
        parts: crate::nostd::vec::IntoIter<InterpPart>,
        out: Vec<InterpExpr>,
        saved: Box<Saved>,
    },
    FuncParamTy {
        head: FnHead,
        name: String,
        pos: Pos,
    },
    FuncVarargTy(FnHead),
    FuncRet(FnHead),
    FuncEnd {
        head: FnHead,
        ret: Option<Vec<Type>>,
    },
    // ---- blocks and statements ----
    BlockStat,
    BlockRet,
    RetStat(Pos),
    IfCond {
        pos: Pos,
        conds: Vec<(Expr, Block)>,
    },
    IfBody {
        pos: Pos,
        conds: Vec<(Expr, Block)>,
        cond: Expr,
    },
    IfElse {
        pos: Pos,
        conds: Vec<(Expr, Block)>,
    },
    WhileCond(Pos),
    WhileBody {
        pos: Pos,
        cond: Expr,
    },
    DoBody(Pos),
    RepeatBody(Pos),
    RepeatCond {
        pos: Pos,
        body: Block,
    },
    ForFirstTy {
        open: Pos,
        npos: Pos,
        name: String,
    },
    NumStart(Box<NumFor>),
    NumLimit(Box<NumFor>),
    NumStep(Box<NumFor>),
    NumBody(Box<NumFor>),
    GenNameTy {
        open: Pos,
        names: Vec<LocalName>,
        name: String,
        pos: Pos,
    },
    GenExprs {
        open: Pos,
        names: Vec<LocalName>,
        line: u32,
    },
    GenBody {
        open: Pos,
        names: Vec<LocalName>,
        exprs: Vec<Expr>,
        line: u32,
        do_line: u32,
    },
    FuncStat {
        pos: Pos,
        path: Vec<String>,
        method: Option<String>,
    },
    LocalFunc {
        pos: Pos,
        name: String,
        npos: Pos,
    },
    LocalTy {
        spos: Pos,
        names: Vec<LocalName>,
        closes: usize,
        name: String,
        attrib: Attrib,
        pos: Pos,
    },
    LocalExprs {
        pos: Pos,
        names: Vec<LocalName>,
    },
    TypeDef {
        pos: Pos,
        name: String,
    },
    ExprStat(Pos),
    AssignTarget {
        pos: Pos,
        targets: Vec<Expr>,
        follow: Vec<u32>,
    },
    AssignExprs {
        pos: Pos,
        targets: Vec<Expr>,
        follow: Vec<u32>,
    },
    OpAssign {
        pos: Pos,
        target: Expr,
        op: BinOp,
    },
    // ---- types ----
    TyAlts(Vec<Type>),
    TyShape {
        fields: Vec<(String, Type)>,
        name: String,
    },
    TyMapKey,
    TyMapVal(Box<Type>),
    TyArray,
    TyFuncParam(Vec<Type>),
    TyFuncRet {
        params: Vec<Type>,
        vararg: bool,
    },
    RetTypes {
        save: usize,
        types: Vec<Type>,
    },
    WrapTypes,
}

/// What a level of interpolation holds while it is parsed: its text (twice: the parser's
/// copy and the token's, cloned) and its tokens.
fn interp_held(text: &[u8], toks: &[Token]) -> usize {
    text.len().saturating_mul(2).saturating_add(tokens_held(toks))
}

/// The memory `toks` and their share of the tree take (`TOKEN_COST` and `token_bytes`).
fn tokens_held(toks: &[Token]) -> usize {
    // A step per token (S5C-FRONT-12: a pass over a big source's tokens is time too; a
    // stop found here is kept, and the parser's first step reports it).
    toks.iter().fold(0usize, |n, t| {
        crate::diag::compile_tick();
        n.saturating_add(TOKEN_COST).saturating_add(token_bytes(&t.tok))
    })
}

/// An expression moved into a box of its own (or a vector of one): counted in the tree.
const EXPR_BOX: usize = crate::nostd::mem::size_of::<Expr>();

fn assignable(e: &Expr) -> bool {
    matches!(e.kind, ExprKind::Name(_) | ExprKind::Index(..))
}

impl<'a> Parser<'a> {
    fn run(&mut self) -> PResult<()> {
        while let Some(f) = self.frames.pop() {
            self.stack_room()?;
            let r = self.step(f);
            if let Some(d) = self.fail.take() {
                return Err(d);
            }
            r?;
            // A step pushes a few frames and builds a node or two at most: checked here,
            // the stacks and the tree stay within their memory (as allocated: S6-FRONT-3).
            if self.held() > self.room {
                return Err(Diagnostic::error(self.chunk, self.pos(), "not enough memory"));
            }
            if crate::diag::compile_tick() {
                return Err(Diagnostic::error(self.chunk, self.pos(), crate::diag::COMPILE_STOPPED));
            }
        }
        Ok(())
    }

    /// Pushes `x` onto `v` (a vector of the tree being built), counting in `tree` the
    /// bytes its buffer grows by: a vector's first element gets a buffer of one, then
    /// they double, and growing must leave room for the old buffer and the new at once
    /// (S6-FRONT-3: the tree was counted at 128 bytes a token, where a block's statements
    /// alone took 1.5 KB, four 384-byte statements, the least a vector of them holds, so
    /// nesting took 2 to 3 times the memory limit).
    fn push_counted<T: Send + 'static>(&mut self, v: &mut Vec<T>, x: T) -> PResult<()> {
        if v.len() == v.capacity() {
            let before = v.capacity();
            let after = if before == 0 { 1 } else { before.saturating_mul(2).max(4) };
            let grow = after.saturating_sub(before).saturating_mul(crate::nostd::mem::size_of::<T>());
            if self.held().saturating_add(after.saturating_mul(crate::nostd::mem::size_of::<T>())) > self.room {
                return Err(Diagnostic::error(self.chunk, self.pos(), "not enough memory"));
            }
            if crate::diag::grow_watched(v, after) {
                return Err(Diagnostic::error(self.chunk, self.pos(), crate::diag::COMPILE_STOPPED));
            }
            self.tree = self.tree.saturating_add(grow);
        }
        v.push(x);
        Ok(())
    }

    /// Room in the parser's own stacks for a step: grown here, where growing (the old
    /// buffer and the new at once) is checked against the memory left.
    fn stack_room(&mut self) -> PResult<()> {
        let tight = |cap: usize, len: usize| cap - len < 8;
        if tight(self.frames.capacity(), self.frames.len()) || tight(self.stats.capacity(), self.stats.len()) {
            let more = self.frames.capacity().max(8) * crate::nostd::mem::size_of::<Frame>()
                + self.stats.capacity().max(8) * crate::nostd::mem::size_of::<Vec<Stat>>();
            if self.held().saturating_add(more) > self.room {
                return Err(Diagnostic::error(self.chunk, self.pos(), "not enough memory"));
            }
            // (Grown in place: moving a stack a stretch at a time, as `push_counted` does
            // the tree's lists, cost deep nesting 40% more time than it saved.)
            if tight(self.frames.capacity(), self.frames.len()) {
                self.frames.reserve(self.frames.capacity().max(8));
            }
            if tight(self.stats.capacity(), self.stats.len()) {
                self.stats.reserve(self.stats.capacity().max(8));
            }
        }
        Ok(())
    }

    /// Hands what a stopped parse built to `diag::defer_pieces`, in pieces that free
    /// quickly: a frame or value can hold a list of millions of nodes (S6-FRONT-4: a
    /// table constructor's fields so far took 80 ms to free as one piece).
    fn defer_state(&mut self) {
        // One piecewise item for all of it: nesting can be a million deep, and an item
        // each would be an allocation each.
        let mut d = crate::ast::Dropper::of_blocks(crate::nostd::mem::take(&mut self.stats));
        match crate::nostd::mem::replace(&mut self.ret, Val::None) {
            Val::Expr(e) => d.add_expr(e),
            Val::Exprs(v) => d.add_exprs(v),
            Val::Block(b) => d.add_block(b),
            other => crate::diag::defer_drop(other),
        }
        // The lists the frames hold are taken out in place (moving a million frames
        // elsewhere took longer than freeing them); the frames go as one piecewise list.
        use crate::nostd::mem::take;
        for f in &mut self.frames {
            match f {
                Frame::ConcatPart { parts: v, .. } | Frame::ExprList(v) | Frame::AssignTarget { targets: v, .. } | Frame::AssignExprs { targets: v, .. } => d.add_exprs(take(v)),
                Frame::GenBody { names, exprs: v, .. } => {
                    d.add_names(take(names));
                    d.add_exprs(take(v));
                }
                Frame::TableField { fields, .. } => d.add_fields(take(fields)),
                Frame::IfCond { conds: c, .. } | Frame::IfBody { conds: c, .. } | Frame::IfElse { conds: c, .. } => d.add_conds(take(c)),
                Frame::GenNameTy { names, .. } | Frame::GenExprs { names, .. } | Frame::LocalTy { names, .. } | Frame::LocalExprs { names, .. } => {
                    d.add_names(take(names));
                }
                _ => {}
            }
        }
        crate::diag::defer_pieces(crate::nostd::mem::take(&mut self.toks));
        crate::diag::defer_pieces(d.with_bytes(self.tree.saturating_add(self.strings)));
        crate::diag::defer_pieces(take(&mut self.frames));
    }

    /// Bytes the parse holds besides its tokens.
    fn held(&self) -> usize {
        self.frames
            .capacity()
            .saturating_mul(crate::nostd::mem::size_of::<Frame>())
            .saturating_add(self.stats.capacity().saturating_mul(crate::nostd::mem::size_of::<Vec<Stat>>()))
            .saturating_add(self.tree)
            .saturating_add(self.sub_held)
            .saturating_add(self.tok_held)
    }

    fn push(&mut self, f: Frame) {
        self.frames.push(f);
    }

    /// Pushes a continuation and then the construct it waits for.
    fn call(&mut self, cont: Frame, start: Frame) -> PResult<()> {
        self.frames.push(cont);
        self.frames.push(start);
        Ok(())
    }

    fn done(&mut self, v: Val) -> PResult<()> {
        self.ret = v;
        Ok(())
    }

    fn stat(&mut self, kind: StatKind, pos: Pos) -> PResult<()> {
        if let Some(mut v) = self.stats.pop() {
            let r = self.push_counted(&mut v, Stat { kind, pos });
            self.stats.push(v);
            r?;
        }
        Ok(())
    }

    // ---- values handed back (a frame always gets the kind it waits for) ----

    fn take(&mut self) -> Val {
        crate::nostd::mem::replace(&mut self.ret, Val::None)
    }

    fn take_expr(&mut self) -> Expr {
        match self.take() {
            Val::Expr(e) => e,
            _ => Expr { kind: ExprKind::Nil, pos: Pos::default(), end: Pos::default() },
        }
    }

    fn take_exprs(&mut self) -> Vec<Expr> {
        match self.take() {
            Val::Exprs(v) => v,
            _ => Vec::new(),
        }
    }

    fn take_block(&mut self) -> Block {
        match self.take() {
            Val::Block(b) => b,
            _ => Block::default(),
        }
    }

    fn take_func(&mut self, head: FnHead, ret: Option<Vec<Type>>, body: Block, end_line: u32, close_line: u32) -> FuncBody {
        FuncBody { params: head.params, vararg: head.vararg, vararg_ty: head.vararg_ty, ret, body, pos: head.pos, end_line, close_line, is_method: head.is_method }
    }

    fn take_type(&mut self) -> Type {
        match self.take() {
            Val::Type(t) => t,
            _ => Type::Nil,
        }
    }

    fn take_types(&mut self) -> Vec<Type> {
        match self.take() {
            Val::Types(v) => v,
            _ => Vec::new(),
        }
    }

    // ---- tokens ----

    fn rl(&self) -> bool {
        self.dialect == Dialect::RemedyLua
    }

    fn tok(&self) -> &Tok {
        self.at(self.i).map(|t| &t.tok).unwrap_or(&Tok::Eof)
    }

    /// Token `j` (`toks` holds those from `base` on).
    fn at(&self, j: usize) -> Option<&Token> {
        self.toks.get(j.checked_sub(self.base)?)
    }

    /// The token `n` ahead, read first if need be (C Lua's `luaX_lookahead`).
    fn tok_at(&mut self, n: usize) -> &Tok {
        self.fill(n);
        self.at(self.i + n).map(|t| &t.tok).unwrap_or(&Tok::Eof)
    }

    /// Where the current token ends (just past it).
    fn tok_end(&self) -> Pos {
        self.at(self.i).map_or_else(|| self.pos(), |t| t.end)
    }

    /// Where the last token read ends (just past it): the end of what was just parsed.
    fn last_end(&self) -> Pos {
        match self.i.checked_sub(1).and_then(|j| self.at(j)) {
            Some(t) => t.end,
            None => self.pos(),
        }
    }

    fn pos(&self) -> Pos {
        self.at(self.i).or(self.toks.last()).map(|t| t.pos).unwrap_or_default()
    }

    /// Reads the chunk's tokens up to the one `k` ahead of the current one. A failure is
    /// kept for `run` (`fail`).
    fn fill(&mut self, k: usize) {
        while self.more && self.sub_src.is_none() && self.base + self.toks.len() <= self.i + k {
            // (A stop the lexer saw is kept, for the parser's next look.)
            let reads = self.lex.reads();
            if reads {
                crate::diag::parse_reading((Some(self.held().saturating_add(self.strings)), None));
            }
            let (t, _) = self.lex.token();
            if reads && let (_, Some(room)) = crate::diag::parse_reading((None, None)) {
                self.room = room;
            }
            self.more = t.tok != Tok::Eof;
            self.tok_held = self.tok_held.saturating_add(TOKEN_COST).saturating_add(token_bytes(&t.tok));
            // Growing the vector holds the old buffer and the new, twice its size, at once
            // (S6-FRONT-3: a big source's tokens took 1.4 times the limit right then).
            // The tokens passed are dropped, but for the last (`last_end`) and from where a
            // return type list may be read again (`pins`): the chunk's tokens take a few
            // slots, not memory growing with the chunk.
            let keep = self.pins.first().copied().unwrap_or(usize::MAX).min(self.i.saturating_sub(1));
            if self.toks.len() == self.toks.capacity() && keep.saturating_sub(self.base) >= self.toks.len() / 2 {
                let n = keep - self.base;
                self.toks.drain(..n);
                self.base = keep;
                self.tok_held = self.tok_held.saturating_sub(n.saturating_mul(crate::nostd::mem::size_of::<Token>()));
            }
            if self.toks.len() == self.toks.capacity() {
                let cap = self.toks.capacity().saturating_mul(2).max(4);
                let grown = cap.saturating_mul(crate::nostd::mem::size_of::<Token>());
                let fail = if self.held().saturating_add(grown) > self.room {
                    Some("not enough memory")
                } else if crate::diag::grow_watched(&mut self.toks, cap) {
                    // Between looks at the interrupt, a stretch at a time (doubling millions
                    // of tokens in one `realloc` took 20 to 60 ms).
                    Some(crate::diag::COMPILE_STOPPED)
                } else {
                    None
                };
                if let Some(m) = fail {
                    self.more = false;
                    self.fail = Some(Diagnostic::error(self.chunk, t.pos, m));
                    return;
                }
            }
            self.toks.push(t);
        }
    }

    fn advance(&mut self) {
        if self.i < self.base + self.toks.len() {
            self.i += 1;
        }
        // C Lua reads the next token as it moves onto it (`luaX_next`).
        self.fill(0);
        // A step per token too (S5C-FRONT-12): one parser step can take a whole name or
        // expression list, so a step alone is no measure of time between looks at the
        // host's interrupt. A stop found here is kept; the run loop reports it.
        crate::diag::compile_tick();
    }

    fn check(&self, t: &Tok) -> bool {
        self.tok() == t
    }

    fn accept(&mut self, t: &Tok) -> bool {
        if self.check(t) {
            self.advance();
            true
        } else {
            false
        }
    }

    /// The text the tokens come from.
    fn text(&self) -> &[u8] {
        match &self.sub_src {
            Some(s) => s,
            None => self.lex.text(),
        }
    }

    /// The current token as Lua's messages quote it: as written, or `<eof>`.
    fn near(&self) -> String {
        let Some(t) = self.at(self.i) else { return "<eof>".into() };
        let src = self.text();
        match &t.tok {
            Tok::Eof => "<eof>".into(),
            Tok::Name(_) | Tok::Int(_) | Tok::Num(_) | Tok::Str(_) if t.span.1 > t.span.0 => {
                crate::lexer::near_text(src, t)
            }
            other => format!("'{}'", other.describe()),
        }
    }

    /// An error found just after a token was taken (C Lua's `luaK_semerror`): C Lua had
    /// read the next token by then, so a lexical error there comes first, and its message
    /// gives the line where that token ends (S5C-FRONT-7).
    fn semerror(&self, pos: Pos, msg: String) -> Diagnostic {
        if let Some(Token { tok: Tok::Error(d), .. }) = self.at(self.i) {
            return (**d).clone();
        }
        Diagnostic::error(self.chunk, pos, msg).lua_line(self.tok_end().line)
    }

    fn error_near(&self, msg: &str) -> Diagnostic {
        if let Some(Token { tok: Tok::Error(d), .. }) = self.at(self.i) {
            return (**d).clone();
        }
        {
            // About the token; Lua's message gives the line where it ends.
            let end = self.tok_end();
            // A NUL byte is token 0 in C Lua, which its message leaves out.
            let text = if matches!(self.tok(), Tok::Other(0)) { msg.to_owned() } else { format!("{msg} near {}", self.near()) };
            Diagnostic::error(self.chunk, self.pos(), text).to(end).lua_line(end.line)
        }
    }

    /// Expressions left unfinished at the cut, kept as values (`local _ = ...`) so what they
    /// hold (functions) is still compiled.
    fn cut_values(&mut self, pos: Pos, exprs: Vec<Expr>) -> PResult<()> {
        let name = LocalName { name: String::from("(cut)"), attrib: Attrib::None, ty: None, pos };
        self.tree += crate::nostd::mem::size_of::<LocalName>();
        self.stat(StatKind::Local(vec![name], exprs), pos)
    }

    /// At the cut `prefix` made: whatever was expected counts as there.
    fn at_cut(&self) -> bool {
        self.recover && self.check(&Tok::Eof)
    }

    fn expect(&mut self, t: &Tok, what: &str) -> PResult<()> {
        if self.accept(t) || self.at_cut() { Ok(()) } else { Err(self.error_near(&format!("'{what}' expected"))) }
    }

    /// Expects the closing token of a construct opened at `open`.
    fn expect_match(&mut self, t: &Tok, what: &str, opener: &str, open: Pos) -> PResult<()> {
        if self.accept(t) || self.at_cut() {
            return Ok(());
        }
        if open.line == self.tok_end().line {
            Err(self.error_near(&format!("'{what}' expected")))
        } else {
            Err(self.error_near(&format!("'{what}' expected (to close '{opener}' at line {})", open.line)))
        }
    }

    fn name(&mut self) -> PResult<String> {
        match self.tok() {
            Tok::Name(n) => {
                let n = n.clone();
                self.strings += n.capacity();
                self.advance();
                Ok(n)
            }
            _ if self.at_cut() => Ok(String::from("(cut)")),
            _ => Err(self.error_near("<name> expected")),
        }
    }

    fn block_follow(&self, with_until: bool) -> bool {
        match self.tok() {
            Tok::Else | Tok::Elseif | Tok::End | Tok::Eof => true,
            Tok::Until => with_until,
            _ => false,
        }
    }

    fn binop(&self) -> Option<BinOp> {
        Some(match self.tok() {
            Tok::Plus => BinOp::Add,
            Tok::Minus => BinOp::Sub,
            Tok::Star => BinOp::Mul,
            Tok::Slash => BinOp::Div,
            Tok::DSlash => BinOp::IDiv,
            Tok::Percent => BinOp::Mod,
            Tok::Caret => BinOp::Pow,
            Tok::Concat => BinOp::Concat,
            Tok::Eq => BinOp::Eq,
            Tok::Ne => BinOp::Ne,
            Tok::Lt => BinOp::Lt,
            Tok::Le => BinOp::Le,
            Tok::Gt => BinOp::Gt,
            Tok::Ge => BinOp::Ge,
            Tok::And => BinOp::And,
            Tok::Or => BinOp::Or,
            Tok::Amp => BinOp::BAnd,
            Tok::Pipe => BinOp::BOr,
            Tok::Tilde => BinOp::BXor,
            Tok::Shl => BinOp::Shl,
            Tok::Shr => BinOp::Shr,
            _ => return None,
        })
    }

    // ---- the machine ----

    fn step(&mut self, f: Frame) -> PResult<()> {
        match f {
            Frame::StartBlock => {
                self.stats.push(Vec::new());
                self.block_next()
            }
            Frame::StartExpr(limit) => self.start_expr(limit),
            Frame::StartTy => {
                self.push(Frame::TyAlts(Vec::new()));
                self.ty_base()
            }

            // ---- expressions ----
            Frame::Unary { op, pos, limit } => {
                let e = self.take_expr();
                self.tree += EXPR_BOX;
                self.op_loop(limit, fold_unary(op, e, pos))
            }
            Frame::Loop(limit) => {
                let left = self.take_expr();
                self.op_loop(limit, left)
            }
            Frame::BinRight { limit, op, opos, left } => {
                let right = self.take_expr();
                self.tree += 2 * EXPR_BOX;
                self.op_loop(limit, fold_binary(op, left, right, opos))
            }
            Frame::ConcatPart { limit, pos, mut parts } => {
                let x = self.take_expr();
                self.push_counted(&mut parts, x)?;
                let opos = self.pos();
                if self.accept(&Tok::Concat) {
                    // The rest of `a .. b .. c`: what binds tighter than `..` is one part.
                    // (The instruction takes the last `..`'s line, as in C Lua.)
                    return self.call(Frame::ConcatPart { limit, pos: opos, parts }, Frame::StartExpr(9));
                }
                self.op_loop(limit, Expr { kind: ExprKind::Concat(parts), pos, end: self.last_end() })
            }
            Frame::ParenClose(pos) => {
                let e = self.take_expr();
                self.expect_match(&Tok::RParen, ")", "(", pos)?;
                self.tree += EXPR_BOX;
                self.suffixes(Expr { kind: ExprKind::Paren(Box::new(e)), pos, end: self.last_end() })
            }
            Frame::SuffixIndex { e, pos } => {
                let k = self.take_expr();
                self.expect(&Tok::RBracket, "]")?;
                self.tree += 2 * EXPR_BOX;
                self.suffixes(Expr { kind: ExprKind::Index(Box::new(e), Box::new(k)), pos, end: self.last_end() })
            }
            Frame::SuffixCall { e, pos } => {
                let args = self.take_exprs();
                self.tree += EXPR_BOX;
                self.suffixes(Expr { kind: ExprKind::Call(Box::new(e), args), pos, end: self.last_end() })
            }
            Frame::SuffixMethod { e, name, npos, pos } => {
                let args = self.take_exprs();
                self.suffixes(Expr { kind: ExprKind::Method(e, name, npos, args), pos, end: self.last_end() })
            }
            Frame::WrapFunc(pos) => match self.take() {
                Val::Func(f) => self.done(Val::Expr(Expr { kind: ExprKind::Func(f), pos, end: self.last_end() })),
                _ => self.done(Val::Expr(Expr { kind: ExprKind::Nil, pos, end: self.last_end() })),
            },
            Frame::ArgsClose(pos) => {
                let v = self.take_exprs();
                self.expect_match(&Tok::RParen, ")", "(", pos)?;
                self.done(Val::Exprs(v))
            }
            Frame::WrapArg => {
                let e = self.take_expr();
                self.tree += EXPR_BOX;
                self.done(Val::Exprs(vec![e]))
            }
            Frame::ExprList(mut v) => {
                let x = self.take_expr();
                self.push_counted(&mut v, x)?;
                if self.accept(&Tok::Comma) {
                    return self.call(Frame::ExprList(v), Frame::StartExpr(0));
                }
                self.done(Val::Exprs(v))
            }
            Frame::TableField { pos, mut fields, kind } => {
                match kind {
                    FieldKind::Named(n, fpos) => {
                        let v = self.take_expr();
                        self.push_counted(&mut fields, Field::Named(n, v, fpos))?;
                    }
                    FieldKind::Key => {
                        let k = self.take_expr();
                        self.expect(&Tok::RBracket, "]")?;
                        self.expect(&Tok::Assign, "=")?;
                        return self.call(Frame::TableField { pos, fields, kind: FieldKind::KeyVal(k) }, Frame::StartExpr(0));
                    }
                    FieldKind::KeyVal(k) => {
                        let v = self.take_expr();
                        self.push_counted(&mut fields, Field::Keyed(k, v))?;
                    }
                    FieldKind::Positional => {
                        let v = self.take_expr();
                        self.push_counted(&mut fields, Field::Pos(v))?;
                    }
                }
                if !self.accept(&Tok::Comma) && !self.accept(&Tok::Semi) {
                    return self.table_close(pos, fields);
                }
                self.table_next(pos, fields)
            }
            Frame::Interp { pos, parts, mut out, saved } => {
                let e = self.take_expr();
                if !self.check(&Tok::Eof) {
                    return Err(self.error_near("'}' expected in interpolated string"));
                }
                let Saved { toks, sub_src, i, base } = *saved;
                let held = interp_held(self.sub_src.as_deref().unwrap_or_default(), &self.toks);
                self.sub_held = self.sub_held.saturating_sub(held);
                self.toks = toks;
                self.sub_src = sub_src;
                self.i = i;
                self.base = base;
                self.push_counted(&mut out, InterpExpr::Expr(e))?;
                self.interp_next(pos, parts, out)
            }
            Frame::FuncParamTy { mut head, name, pos } => {
                let ty = self.take_type();
                self.push_counted(&mut head.params, Param { name, ty: Some(ty), pos })?;
                if self.accept(&Tok::Comma) { self.func_params(head) } else { self.func_params_done(head) }
            }
            Frame::FuncVarargTy(mut head) => {
                // `...: T`: the varargs' type.
                head.vararg_ty = Some(self.take_type());
                self.func_params_done(head)
            }
            Frame::FuncRet(head) => {
                let ret = self.take_types();
                self.func_block(head, Some(ret))
            }
            Frame::FuncEnd { head, ret } => {
                let body = self.take_block();
                self.vararg.pop();
                let end_line = self.pos().line;
                // Closed only by a real `end` (at the cut of `prefix` it never was).
                let closed = self.check(&Tok::End);
                self.expect_match(&Tok::End, "end", "function", head.pos)?;
                let close_line = if closed { self.tok_end().line } else { CUT_LINE };
                let f = self.take_func(head, ret, body, end_line, close_line);
                self.tree += crate::nostd::mem::size_of::<FuncBody>();
                self.done(Val::Func(Box::new(f)))
            }

            // ---- blocks and statements ----
            Frame::BlockStat => self.block_next(),
            Frame::BlockRet => self.block_end(),
            Frame::RetStat(pos) => {
                let exprs = self.take_exprs();
                self.ret_end(pos, exprs)
            }
            Frame::IfCond { pos, conds } => {
                let cond = self.take_expr();
                self.expect(&Tok::Then, "then")?;
                self.call(Frame::IfBody { pos, conds, cond }, Frame::StartBlock)
            }
            Frame::IfBody { pos, mut conds, cond } => {
                let b = self.take_block();
                self.push_counted(&mut conds, (cond, b))?;
                if self.accept(&Tok::Elseif) {
                    return self.call(Frame::IfCond { pos, conds }, Frame::StartExpr(0));
                }
                if self.accept(&Tok::Else) {
                    return self.call(Frame::IfElse { pos, conds }, Frame::StartBlock);
                }
                self.expect_match(&Tok::End, "end", "if", pos)?;
                self.stat(StatKind::If(conds, None), pos)
            }
            Frame::IfElse { pos, conds } => {
                let b = self.take_block();
                self.expect_match(&Tok::End, "end", "if", pos)?;
                self.stat(StatKind::If(conds, Some(b)), pos)
            }
            Frame::WhileCond(pos) => {
                let cond = self.take_expr();
                self.expect(&Tok::Do, "do")?;
                self.call(Frame::WhileBody { pos, cond }, Frame::StartBlock)
            }
            Frame::WhileBody { pos, cond } => {
                let body = self.take_block();
                self.expect_match(&Tok::End, "end", "while", pos)?;
                self.stat(StatKind::While(cond, body), pos)
            }
            Frame::DoBody(pos) => {
                let b = self.take_block();
                self.expect_match(&Tok::End, "end", "do", pos)?;
                self.stat(StatKind::Do(b), pos)
            }
            Frame::RepeatBody(pos) => {
                let body = self.take_block();
                self.expect_match(&Tok::Until, "until", "repeat", pos)?;
                self.call(Frame::RepeatCond { pos, body }, Frame::StartExpr(0))
            }
            Frame::RepeatCond { pos, body } => {
                let cond = self.take_expr();
                self.stat(StatKind::Repeat(body, cond), pos)
            }
            Frame::ForFirstTy { open, npos, name } => {
                let ty = self.take_type();
                self.for_after_first(open, LocalName { name, attrib: Attrib::None, ty: Some(ty), pos: npos })
            }
            Frame::NumStart(mut nf) => {
                nf.start = Some(self.take_expr());
                self.expect(&Tok::Comma, ",")?;
                self.call(Frame::NumLimit(nf), Frame::StartExpr(0))
            }
            Frame::NumLimit(mut nf) => {
                nf.limit = Some(self.take_expr());
                if self.accept(&Tok::Comma) {
                    return self.call(Frame::NumStep(nf), Frame::StartExpr(0));
                }
                self.num_do(nf)
            }
            Frame::NumStep(mut nf) => {
                nf.step = Some(self.take_expr());
                self.num_do(nf)
            }
            Frame::NumBody(nf) => {
                let body = self.take_block();
                let NumFor { open, var, start, limit, step, do_line } = *nf;
                self.expect_match(&Tok::End, "end", "for", open)?;
                let nil = || Expr { kind: ExprKind::Nil, pos: Pos::default(), end: Pos::default() };
                let (start, limit) = (start.unwrap_or_else(nil), limit.unwrap_or_else(nil));
                self.tree += crate::nostd::mem::size_of::<ForNum>();
                self.stat(StatKind::NumFor(Box::new(ForNum { var, start, limit, step, body, do_line })), open)
            }
            Frame::GenNameTy { open, mut names, name, pos } => {
                let ty = self.take_type();
                self.push_counted(&mut names, LocalName { name, attrib: Attrib::None, ty: Some(ty), pos })?;
                self.gen_names(open, names)
            }
            Frame::GenExprs { open, names, line } => {
                let exprs = self.take_exprs();
                self.expect(&Tok::Do, "do")?;
                let do_line = self.last_end().line;
                self.call(Frame::GenBody { open, names, exprs, line, do_line }, Frame::StartBlock)
            }
            Frame::GenBody { open, names, exprs, line, do_line } => {
                let body = self.take_block();
                self.expect_match(&Tok::End, "end", "for", open)?;
                self.stat(StatKind::GenFor { names, exprs, body, line, do_line }, open)
            }
            Frame::FuncStat { pos, path, method } => match self.take() {
                Val::Func(body) => self.stat(StatKind::Function { path, method, body }, pos),
                _ => Ok(()),
            },
            Frame::LocalFunc { pos, name, npos } => match self.take() {
                Val::Func(body) => {
                    let name = LocalName { name, attrib: Attrib::None, ty: None, pos: npos };
                    self.stat(StatKind::LocalFunction(name, body), pos)
                }
                _ => Ok(()),
            },
            Frame::LocalTy { spos, mut names, mut closes, name, attrib, pos } => {
                let ty = self.take_type();
                if self.local_name_done(&mut names, &mut closes, name, attrib, Some(ty), pos)? {
                    self.local_names(spos, names, closes)
                } else {
                    self.local_exprs(spos, names)
                }
            }
            Frame::LocalExprs { pos, names } => {
                let exprs = self.take_exprs();
                self.stat(StatKind::Local(names, exprs), pos)
            }
            Frame::TypeDef { pos, name } => {
                let ty = self.take_type();
                self.stat(StatKind::TypeDef(name, ty), pos)
            }
            Frame::ExprStat(pos) => {
                let e = self.take_expr();
                self.expr_stat(pos, e)
            }
            Frame::AssignTarget { pos, mut targets, follow } => {
                let t = self.take_expr();
                if !assignable(&t) {
                    if self.at_cut() {
                        self.push_counted(&mut targets, t)?;
                        return self.cut_values(pos, targets);
                    }
                    return Err(self.error_near("syntax error"));
                }
                self.push_counted(&mut targets, t)?;
                self.assign_targets(pos, targets, follow)
            }
            Frame::AssignExprs { pos, targets, follow } => {
                let exprs = self.take_exprs();
                self.stat(StatKind::Assign(targets, exprs, follow), pos)
            }
            Frame::OpAssign { pos, target, op } => {
                let v = self.take_expr();
                self.tree += crate::nostd::mem::size_of::<(Expr, BinOp, Expr)>();
                self.stat(StatKind::OpAssign(Box::new((target, op, v))), pos)
            }

            // ---- types ----
            Frame::TyAlts(mut alts) => {
                let mut t = self.take_type();
                while self.accept(&Tok::Question) {
                    t = Type::Optional(Box::new(t));
                }
                self.push_counted(&mut alts, t)?;
                if self.accept(&Tok::Pipe) {
                    self.push(Frame::TyAlts(alts));
                    return self.ty_base();
                }
                let t = if alts.len() == 1 { alts.pop().unwrap_or(Type::Nil) } else { Type::Union(alts) };
                self.done(Val::Type(t))
            }
            Frame::TyShape { mut fields, name } => {
                let t = self.take_type();
                self.push_counted(&mut fields, (name, t))?;
                if !self.accept(&Tok::Comma) && !self.accept(&Tok::Semi) {
                    self.expect(&Tok::RBrace, "}")?;
                    return self.done(Val::Type(Type::Shape(fields)));
                }
                self.ty_shape(fields)
            }
            Frame::TyMapKey => {
                let k = self.take_type();
                self.expect(&Tok::RBracket, "]")?;
                self.expect(&Tok::Colon, ":")?;
                self.call(Frame::TyMapVal(Box::new(k)), Frame::StartTy)
            }
            Frame::TyMapVal(k) => {
                let v = self.take_type();
                self.accept(&Tok::Comma);
                self.expect(&Tok::RBrace, "}")?;
                self.done(Val::Type(Type::Map(k, Box::new(v))))
            }
            Frame::TyArray => {
                let elem = self.take_type();
                self.expect(&Tok::RBrace, "}")?;
                self.done(Val::Type(Type::Array(Box::new(elem))))
            }
            Frame::TyFuncParam(mut params) => {
                let x = self.take_type();
                self.push_counted(&mut params, x)?;
                if self.accept(&Tok::Comma) { self.ty_func_params(params) } else { self.ty_func_close(params, false) }
            }
            Frame::TyFuncRet { params, vararg } => {
                let ret = self.take_types();
                self.done(Val::Type(Type::Func { params, ret, vararg }))
            }
            Frame::RetTypes { save, mut types } => {
                let x = self.take_type();
                self.push_counted(&mut types, x)?;
                if self.accept(&Tok::Comma) {
                    return self.call(Frame::RetTypes { save, types }, Frame::StartTy);
                }
                self.expect(&Tok::RParen, ")")?;
                self.ret_types_close(save, types)
            }
            Frame::WrapTypes => {
                let t = self.take_type();
                self.done(Val::Types(vec![t]))
            }
        }
    }

    // ---- blocks and statements ----

    /// The next statement of the innermost open block, or its end.
    fn block_next(&mut self) -> PResult<()> {
        loop {
            if self.block_follow(true) {
                return self.block_end();
            }
            if self.check(&Tok::Return) {
                self.push(Frame::BlockRet);
                return self.ret_stat();
            }
            if self.accept(&Tok::Semi) {
                continue;
            }
            self.push(Frame::BlockStat);
            return self.statement();
        }
    }

    fn block_end(&mut self) -> PResult<()> {
        let end = self.pos();
        let stats = self.stats.pop().unwrap_or_default();
        self.done(Val::Block(Block { stats, end }))
    }

    fn ret_stat(&mut self) -> PResult<()> {
        let pos = self.pos();
        self.advance();
        if self.block_follow(true) || self.check(&Tok::Semi) {
            return self.ret_end(pos, Vec::new());
        }
        self.push(Frame::RetStat(pos));
        self.exprlist()
    }

    fn ret_end(&mut self, pos: Pos, exprs: Vec<Expr>) -> PResult<()> {
        self.accept(&Tok::Semi);
        // The block ends here: what must follow is the enclosing construct's to check
        // (`'end' expected ...`), as in C Lua.
        self.stat(StatKind::Return(exprs), pos)
    }

    /// Whether the name at the cursor starts a contextual keyword statement (`continue`,
    /// `type X =`) rather than an expression.
    fn contextual(&mut self) -> Option<&'static str> {
        if !self.rl() {
            return None;
        }
        let Tok::Name(n) = self.tok() else { return None };
        match n.as_str() {
            "continue" => {
                let next = self.tok_at(1);
                let continues_expr = matches!(
                    next,
                    Tok::Assign
                        | Tok::LParen
                        | Tok::Dot
                        | Tok::LBracket
                        | Tok::Colon
                        | Tok::Comma
                        | Tok::Str(_)
                        | Tok::LBrace
                        | Tok::OpAssign(_)
                );
                (!continues_expr).then_some("continue")
            }
            "type" => {
                (matches!(self.tok_at(1), Tok::Name(_)) && matches!(self.tok_at(2), Tok::Assign)).then_some("type")
            }
            _ => None,
        }
    }

    /// Starts a statement (not `;`, not `return`: the block takes those).
    fn statement(&mut self) -> PResult<()> {
        let pos = self.pos();
        match self.tok() {
            Tok::If => {
                self.advance();
                self.call(Frame::IfCond { pos, conds: Vec::new() }, Frame::StartExpr(0))
            }
            Tok::While => {
                self.advance();
                self.call(Frame::WhileCond(pos), Frame::StartExpr(0))
            }
            Tok::Do => {
                self.advance();
                self.call(Frame::DoBody(pos), Frame::StartBlock)
            }
            Tok::For => {
                self.advance();
                let npos = self.pos();
                let name = self.name()?;
                if self.rl() && self.accept(&Tok::Colon) {
                    return self.call(Frame::ForFirstTy { open: pos, npos, name }, Frame::StartTy);
                }
                self.for_after_first(pos, LocalName { name, attrib: Attrib::None, ty: None, pos: npos })
            }
            Tok::Repeat => {
                self.advance();
                self.call(Frame::RepeatBody(pos), Frame::StartBlock)
            }
            Tok::Function => {
                self.advance();
                let mut path = vec![self.name()?];
                self.tree += crate::nostd::mem::size_of::<String>();
                let mut method = None;
                while self.accept(&Tok::Dot) {
                    let n = self.name()?;
                    self.push_counted(&mut path, n)?;
                }
                if self.accept(&Tok::Colon) {
                    method = Some(self.name()?);
                }
                let is_method = method.is_some();
                self.push(Frame::FuncStat { pos, path, method });
                self.func_body(is_method, pos)
            }
            Tok::Local => {
                self.advance();
                if self.accept(&Tok::Function) {
                    let npos = self.pos();
                    let name = self.name()?;
                    self.push(Frame::LocalFunc { pos, name, npos });
                    // Defined where its parameters start, as C Lua's `localfunc` (S5C-FRONT-5).
                    let at = self.pos();
                    return self.func_body(false, at);
                }
                self.local_names(pos, Vec::new(), 0)
            }
            Tok::DColon => {
                self.advance();
                let name = self.name()?;
                let closed = self.check(&Tok::DColon);
                self.expect(&Tok::DColon, "::")?;
                if !closed {
                    // At the cut of `prefix`: C Lua stops at the missing `::`. It reads the
                    // labels (and `;`) after a label before it checks and makes that one, so
                    // the run of labels just before was never checked or made either
                    // (S5C-FRONT-6).
                    if let Some(v) = self.stats.last_mut() {
                        while v.last().is_some_and(|s| matches!(s.kind, StatKind::Label(_))) {
                            v.pop();
                        }
                    }
                    return Ok(());
                }
                self.stat(StatKind::Label(name), pos)
            }
            Tok::Break => {
                self.advance();
                self.stat(StatKind::Break, pos)
            }
            Tok::Goto => {
                self.advance();
                let line = self.tok_end().line;
                let name = self.name()?;
                self.stat(StatKind::Goto(name, line), pos)
            }
            _ => match self.contextual() {
                Some("continue") => {
                    self.advance();
                    self.stat(StatKind::Continue, pos)
                }
                Some(_) => {
                    self.advance();
                    let name = self.name()?;
                    self.expect(&Tok::Assign, "=")?;
                    self.call(Frame::TypeDef { pos, name }, Frame::StartTy)
                }
                None => {
                    self.push(Frame::ExprStat(pos));
                    self.suffixed()
                }
            },
        }
    }

    fn for_after_first(&mut self, open: Pos, first: LocalName) -> PResult<()> {
        if self.accept(&Tok::Assign) {
            let nf = NumFor { open, var: first, start: None, limit: None, step: None, do_line: 0 };
            return self.call(Frame::NumStart(Box::new(nf)), Frame::StartExpr(0));
        }
        self.tree += crate::nostd::mem::size_of::<LocalName>();
        self.gen_names(open, vec![first])
    }

    fn num_do(&mut self, mut nf: Box<NumFor>) -> PResult<()> {
        self.expect(&Tok::Do, "do")?;
        nf.do_line = self.last_end().line;
        self.call(Frame::NumBody(nf), Frame::StartBlock)
    }

    /// The names after the first of a generic `for`, then `in` and its expressions.
    fn gen_names(&mut self, open: Pos, mut names: Vec<LocalName>) -> PResult<()> {
        while self.accept(&Tok::Comma) {
            let pos = self.pos();
            let name = self.name()?;
            if self.rl() && self.accept(&Tok::Colon) {
                return self.call(Frame::GenNameTy { open, names, name, pos }, Frame::StartTy);
            }
            self.push_counted(&mut names, LocalName { name, attrib: Attrib::None, ty: None, pos })?;
        }
        if !self.check(&Tok::In) {
            if self.at_cut() {
                return Ok(());
            }
            return Err(self.error_near(if names.len() > 1 { "'in' expected" } else { "'=' or 'in' expected" }));
        }
        self.advance();
        let line = self.tok_end().line;
        self.push(Frame::GenExprs { open, names, line });
        self.exprlist()
    }

    fn attrib(&mut self) -> PResult<Attrib> {
        if !self.check(&Tok::Lt) {
            return Ok(Attrib::None);
        }
        self.advance();
        let a = self.name()?;
        self.expect(&Tok::Gt, ">")?;
        match a.as_str() {
            "const" => Ok(Attrib::Const),
            "close" => Ok(Attrib::Close),
            _ => Err(self.semerror(self.pos(), format!("unknown attribute '{a}'"))),
        }
    }

    /// The names of a `local` statement, from the next one on.
    fn local_names(&mut self, spos: Pos, mut names: Vec<LocalName>, mut closes: usize) -> PResult<()> {
        loop {
            let pos = self.pos();
            let name = self.name()?;
            let attrib = self.attrib()?;
            if self.rl() && self.accept(&Tok::Colon) {
                return self.call(Frame::LocalTy { spos, names, closes, name, attrib, pos }, Frame::StartTy);
            }
            if !self.local_name_done(&mut names, &mut closes, name, attrib, None, pos)? {
                return self.local_exprs(spos, names);
            }
        }
    }

    /// Finishes a name of a `local` statement: whether another follows.
    fn local_name_done(
        &mut self,
        names: &mut Vec<LocalName>,
        closes: &mut usize,
        name: String,
        mut attrib: Attrib,
        ty: Option<Type>,
        pos: Pos,
    ) -> PResult<bool> {
        if attrib == Attrib::None {
            attrib = self.attrib()?;
        }
        if attrib == Attrib::Close {
            *closes += 1;
            if *closes > 1 {
                return Err(self.semerror(pos, "multiple to-be-closed variables in local list".into()));
            }
        }
        self.push_counted(names, LocalName { name, attrib, ty, pos })?;
        Ok(self.accept(&Tok::Comma))
    }

    fn local_exprs(&mut self, pos: Pos, names: Vec<LocalName>) -> PResult<()> {
        if self.accept(&Tok::Assign) {
            self.push(Frame::LocalExprs { pos, names });
            return self.exprlist();
        }
        self.stat(StatKind::Local(names, Vec::new()), pos)
    }

    fn expr_stat(&mut self, pos: Pos, e: Expr) -> PResult<()> {
        if self.check(&Tok::Assign) || self.check(&Tok::Comma) {
            if !assignable(&e) {
                return Err(self.error_near("syntax error"));
            }
            self.tree += EXPR_BOX;
            return self.assign_targets(pos, vec![e], Vec::new());
        }
        if let Tok::OpAssign(op) = *self.tok() {
            if !assignable(&e) {
                return Err(Diagnostic::error(self.chunk, e.pos, "syntax error (cannot assign to this expression)"));
            }
            self.advance();
            return self.call(Frame::OpAssign { pos, target: e, op }, Frame::StartExpr(0));
        }
        if !matches!(e.kind, ExprKind::Call(..) | ExprKind::Method(..)) {
            if self.at_cut() {
                return self.cut_values(pos, vec![e]);
            }
            return Err(self.error_near("syntax error"));
        }
        self.stat(StatKind::Call(e), pos)
    }

    fn assign_targets(&mut self, pos: Pos, targets: Vec<Expr>, mut follow: Vec<u32>) -> PResult<()> {
        // C Lua checks a target is not const with the token after it read (`restassign`).
        crate::ast::note_follow(&targets, &mut follow, self.tok_end().line);
        if self.accept(&Tok::Comma) {
            self.push(Frame::AssignTarget { pos, targets, follow });
            return self.suffixed();
        }
        self.expect(&Tok::Assign, "=")?;
        self.push(Frame::AssignExprs { pos, targets, follow });
        self.exprlist()
    }

    // ---- expressions ----

    fn exprlist(&mut self) -> PResult<()> {
        self.call(Frame::ExprList(Vec::new()), Frame::StartExpr(0))
    }

    /// An expression whose operators bind tighter than `limit`.
    fn start_expr(&mut self, limit: u8) -> PResult<()> {
        let pos = self.pos();
        let unop = match self.tok() {
            Tok::Not => Some(UnOp::Not),
            Tok::Minus => Some(UnOp::Neg),
            Tok::Hash => Some(UnOp::Len),
            Tok::Tilde => Some(UnOp::BNot),
            _ => None,
        };
        if let Some(op) = unop {
            self.advance();
            return self.call(Frame::Unary { op, pos, limit }, Frame::StartExpr(UNARY_PRIORITY));
        }
        self.push(Frame::Loop(limit));
        self.simple()
    }

    /// The binary operators after `left` that bind tighter than `limit`.
    fn op_loop(&mut self, limit: u8, left: Expr) -> PResult<()> {
        if let Some(op) = self.binop() {
            let (l, r) = op.priority();
            if l > limit {
                let opos = self.pos();
                self.advance();
                if op == BinOp::Concat {
                    self.tree += EXPR_BOX;
                    return self.call(Frame::ConcatPart { limit, pos: opos, parts: vec![left] }, Frame::StartExpr(9));
                }
                return self.call(Frame::BinRight { limit, op, opos, left }, Frame::StartExpr(r));
            }
        }
        self.done(Val::Expr(left))
    }

    fn simple(&mut self) -> PResult<()> {
        let pos = self.pos();
        // A string's bytes are moved from its token, not copied (16 MiB took 5 ms in one
        // go): an expression's token is read once (only types are read again: `ret_types`).
        let at = self.i.wrapping_sub(self.base);
        if let Some(Token { tok: Tok::Str(s), .. }) = self.toks.get_mut(at) {
            let s = crate::nostd::mem::take(s);
            self.strings += s.capacity();
            self.advance();
            return self.done(Val::Expr(Expr { kind: ExprKind::Str(s), pos, end: self.last_end() }));
        }
        let kind = match self.tok() {
            Tok::Nil => ExprKind::Nil,
            Tok::True => ExprKind::True,
            Tok::False => ExprKind::False,
            Tok::Int(i) => ExprKind::Int(*i),
            Tok::Num(f) => ExprKind::Num(*f),
            Tok::Interp(parts) => {
                let parts = parts.clone();
                self.strings += parts.iter().map(|p| if let crate::lexer::InterpPart::Lit(b) = p { b.capacity() } else { 0 }).sum::<usize>();
                self.advance();
                return self.interp_next(pos, parts.into_iter(), Vec::new());
            }
            Tok::Dots => {
                if !self.vararg.last().copied().unwrap_or(false) {
                    return Err(self.error_near("cannot use '...' outside a vararg function"));
                }
                ExprKind::Dots
            }
            Tok::LBrace => return self.table(),
            Tok::Function => {
                self.advance();
                self.push(Frame::WrapFunc(pos));
                // Defined where its parameters start, as in C Lua's `simpleexp` (S5C-FRONT-5).
                let at = self.pos();
                return self.func_body(false, at);
            }
            _ => return self.suffixed(),
        };
        self.advance();
        self.done(Val::Expr(Expr { kind, pos, end: self.last_end() }))
    }

    /// The parts of an interpolated string from the next on: each expression part is
    /// parsed from its own tokens, which take the place of the chunk's meanwhile.
    fn interp_next(&mut self, pos: Pos, mut parts: crate::nostd::vec::IntoIter<InterpPart>, mut out: Vec<InterpExpr>) -> PResult<()> {
        for p in parts.by_ref() {
            match p {
                InterpPart::Lit(b) => self.push_counted(&mut out, InterpExpr::Lit(b))?,
                InterpPart::Expr(text, tpos) => {
                    let left = self.room.saturating_sub(self.held());
                    let toks = Lexer::at(&text, self.chunk, self.dialect, tpos).with_memory(left).tokens()?;
                    self.sub_held = self.sub_held.saturating_add(interp_held(&text, &toks));
                    if self.held() > self.room {
                        return Err(Diagnostic::error(self.chunk, self.pos(), "not enough memory"));
                    }
                    let saved = Saved {
                        toks: crate::nostd::mem::replace(&mut self.toks, toks),
                        sub_src: self.sub_src.replace(text),
                        i: crate::nostd::mem::replace(&mut self.i, 0),
                        base: crate::nostd::mem::replace(&mut self.base, 0),
                    };
                    return self.call(Frame::Interp { pos, parts, out, saved: Box::new(saved) }, Frame::StartExpr(0));
                }
            }
        }
        self.done(Val::Expr(Expr { kind: ExprKind::Interp(out), pos, end: self.last_end() }))
    }

    fn suffixed(&mut self) -> PResult<()> {
        let pos = self.pos();
        match self.tok() {
            Tok::Name(n) => {
                let n = n.clone();
                self.strings += n.capacity();
                self.advance();
                self.suffixes(Expr { kind: ExprKind::Name(n), pos, end: self.last_end() })
            }
            Tok::LParen => {
                self.advance();
                self.call(Frame::ParenClose(pos), Frame::StartExpr(0))
            }
            _ if self.at_cut() => self.done(Val::Expr(Expr { kind: ExprKind::Nil, pos, end: pos })),
            _ => Err(self.error_near("unexpected symbol")),
        }
    }

    /// `.name`, `[k]`, `:m(args)` and `(args)` after `e`.
    fn suffixes(&mut self, mut e: Expr) -> PResult<()> {
        loop {
            let pos = self.pos();
            match self.tok() {
                Tok::Dot => {
                    self.advance();
                    let n = self.name()?;
                    let k = Expr { kind: ExprKind::Str(n.into_bytes()), pos, end: self.last_end() };
                    self.tree += 2 * EXPR_BOX;
                    e = Expr { kind: ExprKind::Index(Box::new(e), Box::new(k)), pos, end: self.last_end() };
                    // A chain of `.name` is built in one step: checked as it grows, and each
                    // link a step (S6-FRONT-4: 3M links ran 450 ms without a look).
                    if self.held() > self.room {
                        return Err(Diagnostic::error(self.chunk, self.pos(), "not enough memory"));
                    }
                    if crate::diag::compile_tick() {
                        // The chain so far is freed later, like the rest (S5C-FRONT-12).
                        // (Its bytes are in the parse's count, charged with the rest.)
                        crate::diag::defer_pieces(crate::ast::Dropper::of_expr(e).with_bytes(0));
                        return Err(Diagnostic::error(self.chunk, self.pos(), crate::diag::COMPILE_STOPPED));
                    }
                }
                Tok::LBracket => {
                    self.advance();
                    return self.call(Frame::SuffixIndex { e, pos }, Frame::StartExpr(0));
                }
                Tok::Colon => {
                    self.advance();
                    let npos = self.pos();
                    let name = self.name()?;
                    let pos = self.pos();
                    self.tree += EXPR_BOX;
                    self.push(Frame::SuffixMethod { e: Box::new(e), name, npos, pos });
                    return self.args();
                }
                Tok::LParen | Tok::Str(_) | Tok::LBrace => {
                    let pos = self.pos();
                    self.push(Frame::SuffixCall { e, pos });
                    return self.args();
                }
                _ => return self.done(Val::Expr(e)),
            }
        }
    }

    fn args(&mut self) -> PResult<()> {
        let pos = self.pos();
        match self.tok() {
            Tok::Str(s) => {
                let s = s.clone();
                self.strings += s.capacity();
                self.advance();
                self.tree += EXPR_BOX;
                self.done(Val::Exprs(vec![Expr { kind: ExprKind::Str(s), pos, end: self.last_end() }]))
            }
            Tok::LBrace => {
                self.push(Frame::WrapArg);
                self.table()
            }
            Tok::LParen => {
                self.advance();
                if self.accept(&Tok::RParen) {
                    return self.done(Val::Exprs(Vec::new()));
                }
                self.push(Frame::ArgsClose(pos));
                self.exprlist()
            }
            _ if self.at_cut() => self.done(Val::Exprs(Vec::new())),
            _ => Err(self.error_near("function arguments expected")),
        }
    }

    fn table(&mut self) -> PResult<()> {
        let pos = self.pos();
        self.expect(&Tok::LBrace, "{")?;
        self.table_next(pos, Vec::new())
    }

    /// The next field of a table constructor, or its end.
    fn table_next(&mut self, pos: Pos, fields: Vec<Field>) -> PResult<()> {
        if self.check(&Tok::RBrace) {
            return self.table_close(pos, fields);
        }
        let fpos = self.pos();
        // Looked past a name only (C Lua's `luaX_lookahead` in `field`).
        let named = matches!(self.tok(), Tok::Name(_)) && matches!(self.tok_at(1), Tok::Assign);
        let kind = if let (true, Tok::Name(n)) = (named, self.tok()) {
            let n = n.clone();
                self.strings += n.capacity();
            self.advance();
            self.advance();
            FieldKind::Named(n, fpos)
        } else if self.accept(&Tok::LBracket) {
            FieldKind::Key
        } else {
            FieldKind::Positional
        };
        self.call(Frame::TableField { pos, fields, kind }, Frame::StartExpr(0))
    }

    fn table_close(&mut self, pos: Pos, fields: Vec<Field>) -> PResult<()> {
        self.expect_match(&Tok::RBrace, "}", "{", pos)?;
        self.done(Val::Expr(Expr { kind: ExprKind::Table(fields), pos, end: self.last_end() }))
    }

    fn func_body(&mut self, is_method: bool, pos: Pos) -> PResult<()> {
        self.expect(&Tok::LParen, "(")?;
        let head = FnHead { is_method, pos, params: Vec::new(), vararg: false, vararg_ty: None };
        if self.check(&Tok::RParen) {
            return self.func_params_done(head);
        }
        self.func_params(head)
    }

    /// Parameters from the next on.
    fn func_params(&mut self, mut head: FnHead) -> PResult<()> {
        loop {
            if self.accept(&Tok::Dots) {
                head.vararg = true;
                if self.rl() && self.accept(&Tok::Colon) {
                    return self.call(Frame::FuncVarargTy(head), Frame::StartTy);
                }
                return self.func_params_done(head);
            }
            let pos = self.pos();
            let name = match self.tok() {
                Tok::Name(n) => {
                    let n = n.clone();
                self.strings += n.capacity();
                    self.advance();
                    n
                }
                _ if self.at_cut() => return self.func_params_done(head),
                _ => return Err(self.error_near("<name> or '...' expected")),
            };
            if self.rl() && self.accept(&Tok::Colon) {
                return self.call(Frame::FuncParamTy { head, name, pos }, Frame::StartTy);
            }
            self.push_counted(&mut head.params, Param { name, ty: None, pos })?;
            if !self.accept(&Tok::Comma) {
                return self.func_params_done(head);
            }
        }
    }

    fn func_params_done(&mut self, head: FnHead) -> PResult<()> {
        self.expect(&Tok::RParen, ")")?; // a plain check in C Lua (no "to close")
        if self.rl() && self.accept(&Tok::Colon) {
            self.push(Frame::FuncRet(head));
            return self.ret_types();
        }
        self.func_block(head, None)
    }

    fn func_block(&mut self, head: FnHead, ret: Option<Vec<Type>>) -> PResult<()> {
        self.vararg.push(head.vararg);
        self.call(Frame::FuncEnd { head, ret }, Frame::StartBlock)
    }

    // ---- types ----

    /// Return types: `T` or `(T, U)` or `()`.
    fn ret_types(&mut self) -> PResult<()> {
        if self.check(&Tok::LParen) {
            let save = self.i;
            self.pins.push(save);
            self.advance();
            if self.accept(&Tok::RParen) {
                return self.ret_types_close(save, Vec::new());
            }
            return self.call(Frame::RetTypes { save, types: Vec::new() }, Frame::StartTy);
        }
        self.call(Frame::WrapTypes, Frame::StartTy)
    }

    fn ret_types_close(&mut self, save: usize, types: Vec<Type>) -> PResult<()> {
        self.pins.pop();
        if self.check(&Tok::Arrow) {
            // It was a function type after all.
            self.i = save;
            return self.call(Frame::WrapTypes, Frame::StartTy);
        }
        self.done(Val::Types(types))
    }

    fn ty_base(&mut self) -> PResult<()> {
        let pos = self.pos();
        match self.tok() {
            Tok::Nil => {
                self.advance();
                self.done(Val::Type(Type::Nil))
            }
            Tok::Function => {
                self.advance();
                self.done(Val::Type(Type::Named("function".into(), pos)))
            }
            Tok::Name(n) => {
                let mut n = n.clone();
                self.strings += n.capacity();
                self.advance();
                while self.accept(&Tok::Dot) {
                    n.push('.');
                    n.push_str(&self.name()?);
                }
                self.done(Val::Type(Type::Named(n, pos)))
            }
            Tok::Str(s) => {
                let s = s.clone();
                self.strings += s.capacity();
                self.advance();
                self.done(Val::Type(Type::Literal(s)))
            }
            Tok::LBrace => {
                self.advance();
                if self.accept(&Tok::RBrace) {
                    return self.done(Val::Type(Type::Shape(Vec::new())));
                }
                if self.accept(&Tok::LBracket) {
                    return self.call(Frame::TyMapKey, Frame::StartTy);
                }
                if matches!(self.tok(), Tok::Name(_)) && matches!(self.tok_at(1), Tok::Colon) {
                    return self.ty_shape(Vec::new());
                }
                self.call(Frame::TyArray, Frame::StartTy)
            }
            Tok::LParen => {
                self.advance();
                if self.check(&Tok::RParen) {
                    return self.ty_func_close(Vec::new(), false);
                }
                self.ty_func_params(Vec::new())
            }
            _ => Err(self.error_near("type expected")),
        }
    }

    /// The fields of a shape type from the next on.
    fn ty_shape(&mut self, fields: Vec<(String, Type)>) -> PResult<()> {
        if self.check(&Tok::RBrace) {
            self.advance();
            return self.done(Val::Type(Type::Shape(fields)));
        }
        let name = self.name()?;
        self.expect(&Tok::Colon, ":")?;
        self.call(Frame::TyShape { fields, name }, Frame::StartTy)
    }

    /// The parameter types of a function type from the next on.
    fn ty_func_params(&mut self, params: Vec<Type>) -> PResult<()> {
        if self.accept(&Tok::Dots) {
            return self.ty_func_close(params, true);
        }
        // Parameter names are allowed and ignored: `(x: number) -> ()`.
        if matches!(self.tok(), Tok::Name(_)) && matches!(self.tok_at(1), Tok::Colon) {
            self.advance();
            self.advance();
        }
        self.call(Frame::TyFuncParam(params), Frame::StartTy)
    }

    fn ty_func_close(&mut self, mut params: Vec<Type>, vararg: bool) -> PResult<()> {
        self.expect(&Tok::RParen, ")")?;
        if self.accept(&Tok::Arrow) {
            self.push(Frame::TyFuncRet { params, vararg });
            return self.ret_types();
        }
        if params.len() == 1 && !vararg {
            return self.done(Val::Type(params.pop().unwrap_or(Type::Nil)));
        }
        Err(self.error_near("'->' expected"))
    }
}

/// Folds `-5`, `-5.0`, `~5` and `not` of a constant into constants, as Lua's code
/// generator does (`ast::fold_un`).
fn fold_unary(op: UnOp, e: Expr, pos: Pos) -> Expr {
    let end = e.end;
    let v = match &e.kind {
        // (`not "s"` is false: the one string fold.)
        ExprKind::Str(_) if op == UnOp::Not => Some(crate::Value::Bool(false)),
        k => crate::ast::literal_value(k).and_then(|v| crate::ast::fold_un(op, v)),
    };
    match v.and_then(crate::ast::literal_kind) {
        Some(kind) => Expr { kind, pos, end },
        None => Expr { kind: ExprKind::Un(op, Box::new(e)), pos, end },
    }
}

/// Folds arithmetic on numeric constants as Lua's `constfolding` does (`ast::fold_bin`).
fn fold_binary(op: BinOp, l: Expr, r: Expr, pos: Pos) -> Expr {
    let end = r.end;
    let num = |e: &ExprKind| matches!(e, ExprKind::Int(_) | ExprKind::Num(_)).then(|| crate::ast::literal_value(e)).flatten();
    if let (Some(a), Some(b)) = (num(&l.kind), num(&r.kind))
        && let Some(kind) = crate::ast::fold_bin(op, a, b).and_then(crate::ast::literal_kind)
    {
        return Expr { kind, pos, end };
    }
    Expr { kind: ExprKind::Bin(op, Box::new(l), Box::new(r)), pos, end }
}
