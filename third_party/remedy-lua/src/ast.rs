//! The syntax tree the parser builds and the compiler, the type checker and the formatter
//! read.

#[allow(unused_imports)]
use crate::nostd::prelude::*;
use crate::diag::Pos;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    IDiv,
    Mod,
    Pow,
    Concat,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    And,
    Or,
    BAnd,
    BOr,
    BXor,
    Shl,
    Shr,
}

impl BinOp {
    pub fn symbol(self) -> &'static str {
        match self {
            BinOp::Add => "+",
            BinOp::Sub => "-",
            BinOp::Mul => "*",
            BinOp::Div => "/",
            BinOp::IDiv => "//",
            BinOp::Mod => "%",
            BinOp::Pow => "^",
            BinOp::Concat => "..",
            BinOp::Eq => "==",
            BinOp::Ne => "~=",
            BinOp::Lt => "<",
            BinOp::Le => "<=",
            BinOp::Gt => ">",
            BinOp::Ge => ">=",
            BinOp::And => "and",
            BinOp::Or => "or",
            BinOp::BAnd => "&",
            BinOp::BOr => "|",
            BinOp::BXor => "~",
            BinOp::Shl => "<<",
            BinOp::Shr => ">>",
        }
    }

    /// Left and right binding priority (Lua's table).
    pub fn priority(self) -> (u8, u8) {
        match self {
            BinOp::Or => (1, 1),
            BinOp::And => (2, 2),
            BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => (3, 3),
            BinOp::BOr => (4, 4),
            BinOp::BXor => (5, 5),
            BinOp::BAnd => (6, 6),
            BinOp::Shl | BinOp::Shr => (7, 7),
            BinOp::Concat => (9, 8),
            BinOp::Add | BinOp::Sub => (10, 10),
            BinOp::Mul | BinOp::Div | BinOp::IDiv | BinOp::Mod => (11, 11),
            BinOp::Pow => (14, 13),
        }
    }
}

pub(crate) const UNARY_PRIORITY: u8 = 12;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum UnOp {
    Neg,
    Not,
    Len,
    BNot,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Expr {
    pub kind: ExprKind,
    /// Where Lua attributes it: its operator, its arguments for a call, else its start.
    pub pos: Pos,
    /// Where its last token ends (just past it).
    pub end: Pos,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum ExprKind {
    Nil,
    True,
    False,
    Dots,
    Int(i64),
    Num(f64),
    Str(Vec<u8>),
    Interp(Vec<InterpExpr>),
    Func(Box<FuncBody>),
    Table(Vec<Field>),
    Bin(BinOp, Box<Expr>, Box<Expr>),
    /// `a .. b .. c`: concatenation is right-associative and done in one instruction.
    Concat(Vec<Expr>),
    Un(UnOp, Box<Expr>),
    Name(String),
    Index(Box<Expr>, Box<Expr>),
    Call(Box<Expr>, Vec<Expr>),
    /// `obj:name(args)`; the `Pos` is the name's (C Lua numbers `SELF` there).
    Method(Box<Expr>, String, Pos, Vec<Expr>),
    Paren(Box<Expr>),
}

impl Expr {
    /// Where C Lua numbers a call: the line its arguments start on, or for a string
    /// argument (`f"..."`, `f[[...]]`), where that string ends.
    pub fn call_at(&self) -> Pos {
        if let ExprKind::Call(_, args) | ExprKind::Method(_, _, _, args) = &self.kind
            && let [a] = args.as_slice()
            && a.pos == self.pos
            && matches!(a.kind, ExprKind::Str(_))
        {
            return a.end;
        }
        self.pos
    }
}

impl ExprKind {
    /// Whether it can give several values (a call or `...`).
    pub fn is_multi(&self) -> bool {
        matches!(self, ExprKind::Call(..) | ExprKind::Method(..) | ExprKind::Dots)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum InterpExpr {
    Lit(Vec<u8>),
    Expr(Expr),
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Field {
    Pos(Expr),
    Named(String, Expr, Pos),
    Keyed(Expr, Expr),
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Param {
    pub name: String,
    pub ty: Option<Type>,
    pub pos: Pos,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct FuncBody {
    pub params: Vec<Param>,
    pub vararg: bool,
    /// The varargs' type (`...: T`), when annotated.
    pub vararg_ty: Option<Type>,
    pub ret: Option<Vec<Type>>,
    pub body: Block,
    pub pos: Pos,
    pub end_line: u32,
    /// The line of the token after its `end`, where C Lua closes it (and reports a `goto`
    /// left without its label).
    pub close_line: u32,
    /// Has an implicit `self` first parameter (`function t:m()`).
    pub is_method: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Block {
    pub stats: Vec<Stat>,
    pub end: Pos,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Attrib {
    None,
    Const,
    Close,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct LocalName {
    pub name: String,
    pub attrib: Attrib,
    pub ty: Option<Type>,
    pub pos: Pos,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Stat {
    pub kind: StatKind,
    pub pos: Pos,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum StatKind {
    /// A call as a statement.
    Call(Expr),
    Local(Vec<LocalName>, Vec<Expr>),
    /// Targets, values, and the line of the token after each target (`,` or `=`), where
    /// C Lua reports an assignment to a const variable (S6-FRONT-7); empty when each is
    /// on its target's last line (`follow_line`).
    Assign(Vec<Expr>, Vec<Expr>, Vec<u32>),
    /// `t op= e` (boxed, as `NumFor`: S6-FRONT-3).
    OpAssign(Box<(Expr, BinOp, Expr)>),
    Do(Block),
    While(Expr, Block),
    Repeat(Block, Expr),
    If(Vec<(Expr, Block)>, Option<Block>),
    /// Boxed: inline, it made every statement 384 bytes (S6-FRONT-3).
    NumFor(Box<ForNum>),
    /// `line`: where the token after `in` ends, the line C Lua gives the iterator's call;
    /// `do_line`: the line of its `do` (the loop's preparation).
    GenFor { names: Vec<LocalName>, exprs: Vec<Expr>, body: Block, line: u32, do_line: u32 },
    Function { path: Vec<String>, method: Option<String>, body: Box<FuncBody> },
    LocalFunction(LocalName, Box<FuncBody>),
    Return(Vec<Expr>),
    Break,
    Continue,
    /// The label's name and the line of the name, where C Lua's `gotostat` takes the
    /// goto's line (S6-FRONT-7).
    Goto(String, u32),
    Label(String),
    /// `type Name = T` (checked, compiles to nothing).
    TypeDef(String, Type),
}

/// A numeric `for`. `do_line`: the line of its `do`, where C Lua numbers the loop's
/// preparation.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ForNum {
    pub var: LocalName,
    pub start: Expr,
    pub limit: Expr,
    pub step: Option<Expr>,
    pub body: Block,
    pub do_line: u32,
}

/// A type annotation.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Type {
    /// A name: `number`, `string`, an alias…
    Named(String, Pos),
    Nil,
    Optional(Box<Type>),
    Union(Vec<Type>),
    /// `(a, b) -> (c, d)`; `...` last for variadic.
    Func { params: Vec<Type>, ret: Vec<Type>, vararg: bool },
    /// `{T}`.
    Array(Box<Type>),
    /// `{[K]: V}`.
    Map(Box<Type>, Box<Type>),
    /// `{x: T, y: U}`.
    Shape(Vec<(String, Type)>),
    /// A string literal type (`"left" | "right"`).
    Literal(Vec<u8>),
}

// ---- dropping without recursion ----
//
// A tree a million levels deep must drop without a million native frames: each node's
// children are moved onto an explicit list before the node itself drops (empty). Leaves
// drop where they are found.

/// Children still to be taken apart. A long list is taken apart an element at a time
/// (`Dropper::step` stops between any two).
enum Node {
    Expr(Expr),
    Stats(Vec<Stat>),
    Exprs(Vec<Expr>),
    Fields(Vec<Field>),
    Conds(Vec<(Expr, Block)>),
    Names(Vec<LocalName>),
    Type(Type),
}

/// Lists longer than this are taken apart an element at a time.
const LONG: usize = 16;

/// Takes the nodes on `stack` apart at once (a tree dropped where it is).
fn drain(mut stack: Vec<Node>) {
    while let Some(n) = stack.pop() {
        match n {
            Node::Expr(mut e) => take_expr(&mut e.kind, &mut stack),
            Node::Stats(mut v) => v.iter_mut().for_each(|s| take_stat(&mut s.kind, &mut stack)),
            Node::Exprs(v) => v.into_iter().for_each(|e| push_expr(e, &mut stack)),
            Node::Fields(v) => v.into_iter().for_each(|f| push_field(f, &mut stack)),
            Node::Conds(v) => v.into_iter().for_each(|(c, mut b)| {
                push_expr(c, &mut stack);
                take_block(&mut b, &mut stack);
            }),
            Node::Names(mut v) => v.iter_mut().for_each(|n| take_ty(n.ty.take(), &mut stack)),
            Node::Type(mut t) => take_type(&mut t, &mut stack),
        }
    }
}

/// A tree being freed in pieces (S6-FRONT-4: freeing a multi-megabyte source's tree took
/// 80 to 140 ms in one go, with no look at the host's interrupt).
/// Its nodes, and the heap bytes they hold: as measured by whoever built it (the parse
/// counts its tree as it builds it), or else by walking them (`nodes_bytes`) when first
/// asked, less what freeing has given back so far.
pub(crate) struct Dropper(Vec<Node>, crate::nostd::cell::Cell<Option<usize>>);

impl Dropper {
    fn of(stack: Vec<Node>) -> Dropper {
        Dropper(stack, crate::nostd::cell::Cell::new(None))
    }

    pub(crate) fn of_block(mut b: Block) -> Dropper {
        let mut stack = Vec::new();
        take_block(&mut b, &mut stack);
        Dropper::of(stack)
    }

    pub(crate) fn of_blocks(blocks: Vec<Vec<Stat>>) -> Dropper {
        Dropper::of(blocks.into_iter().filter(|v| !v.is_empty()).map(Node::Stats).collect())
    }

    pub(crate) fn of_expr(e: Expr) -> Dropper {
        let mut stack = Vec::new();
        push_expr(e, &mut stack);
        Dropper::of(stack)
    }

    /// The same, holding `bytes` on the heap, as its builder counted them (no walk).
    pub(crate) fn with_bytes(self, bytes: usize) -> Dropper {
        self.1.set(Some(bytes));
        self
    }

    fn held(&self) -> usize {
        match self.1.get() {
            Some(n) => n,
            None => {
                let n = nodes_bytes(&self.0);
                self.1.set(Some(n));
                n
            }
        }
    }

    /// The heap bytes its nodes still hold, and its own list.
    pub(crate) fn bytes(&self) -> usize {
        self.held().saturating_add(self.0.capacity() * crate::nostd::mem::size_of::<Node>())
    }

    fn added(&mut self, from: usize) {
        // Measured with the rest when it is first asked; once measured, what is added now.
        if let Some(n) = self.1.get() {
            self.1.set(Some(n.saturating_add(nodes_bytes(self.0.get(from..).unwrap_or_default()))));
        }
    }

    pub(crate) fn add_expr(&mut self, e: Expr) {
        let n = self.0.len();
        push_expr(e, &mut self.0);
        self.added(n);
    }

    pub(crate) fn add_block(&mut self, mut b: Block) {
        let n = self.0.len();
        take_block(&mut b, &mut self.0);
        self.added(n);
    }

    pub(crate) fn add_exprs(&mut self, v: Vec<Expr>) {
        if !v.is_empty() {
            self.0.push(Node::Exprs(v));
            self.added(self.0.len() - 1);
        }
    }

    pub(crate) fn add_fields(&mut self, v: Vec<Field>) {
        if !v.is_empty() {
            self.0.push(Node::Fields(v));
            self.added(self.0.len() - 1);
        }
    }

    pub(crate) fn add_conds(&mut self, v: Vec<(Expr, Block)>) {
        if !v.is_empty() {
            self.0.push(Node::Conds(v));
            self.added(self.0.len() - 1);
        }
    }

    pub(crate) fn add_names(&mut self, v: Vec<LocalName>) {
        if !v.is_empty() {
            self.0.push(Node::Names(v));
            self.added(self.0.len() - 1);
        }
    }

    /// Takes apart up to `budget` nodes (each a few frees): whether it is all freed.
    pub(crate) fn step(&mut self, budget: usize) -> bool {
        // What a step frees: the bytes of the nodes it takes apart, less those of the
        // nodes it leaves (each is measured once more when it is taken apart in turn, so
        // this is exact only for shallow nodes; the rest goes when it is all freed).
        let held = self.held();
        let mut freed = 0;
        let done = self.take_apart(budget, &mut freed);
        self.1.set(Some(if done { 0 } else { held.saturating_sub(freed) }));
        done
    }

    fn take_apart(&mut self, mut budget: usize, freed_: &mut usize) -> bool {
        let stack = &mut self.0;
        while budget > 0 {
            let Some(n) = stack.pop() else { return true };
            match n {
                Node::Expr(mut e) => {
                    budget -= 1;
                    *freed_ += expr_own_bytes(&e);
                    take_expr(&mut e.kind, stack);
                }
                Node::Stats(v) => {
                    // A list element's own bytes go when it is taken apart, and the list's
                    // buffer as it is given back.
                    let before = v.capacity() * crate::nostd::mem::size_of::<Stat>();
                    let k = (budget).min(4096).min(v.len());
                    let freed: usize = v.iter().rev().take(k).map(stat_own_bytes).sum();
                    // (each statement's own buffers; what it leaves as nodes is measured
                    // when they are taken apart)
                    list(v, &mut budget, stack, Node::Stats, |mut s, st| take_stat(&mut s.kind, st));
                    let after = match stack.last() {
                        Some(Node::Stats(v)) => v.capacity() * crate::nostd::mem::size_of::<Stat>(),
                        _ => 0,
                    };
                    *freed_ += freed + before.saturating_sub(after);
                }
                Node::Exprs(v) => list(v, &mut budget, stack, Node::Exprs, push_expr),
                Node::Fields(v) => list(v, &mut budget, stack, Node::Fields, push_field),
                Node::Conds(v) => list(v, &mut budget, stack, Node::Conds, |(c, mut b), st| {
                    push_expr(c, st);
                    take_block(&mut b, st);
                }),
                Node::Names(v) => list(v, &mut budget, stack, Node::Names, |mut n, st| take_ty(n.ty.take(), st)),
                Node::Type(mut t) => {
                    budget -= 1;
                    take_type(&mut t, stack);
                }
            }
        }
        stack.is_empty()
    }
}

/// Gives back the room a list being taken apart from its end no longer uses, a megabyte
/// at a time: a big list's buffer freed at once (hundreds of megabytes, given back to the
/// system) took tens of milliseconds (S6-FRONT-4).
pub(crate) fn shed<T>(v: &mut Vec<T>) {
    if (v.capacity() - v.len()).saturating_mul(crate::nostd::mem::size_of::<T>()) >= 1 << 20 {
        v.shrink_to(v.len());
    }
}

/// Takes apart up to `budget` elements of a list from its end (at most a stretch, then it
/// gives back what it no longer uses), and puts what is left of it back on the stack.
fn list<T>(mut v: Vec<T>, budget: &mut usize, stack: &mut Vec<Node>, node: fn(Vec<T>) -> Node, mut each: impl FnMut(T, &mut Vec<Node>)) {
    let k = (*budget).min(4096).min(v.len()).max(1);
    *budget = budget.saturating_sub(k);
    for _ in 0..k {
        match v.pop() {
            Some(x) => each(x, stack),
            None => break,
        }
    }
    shed(&mut v);
    if !v.is_empty() {
        stack.push(node(v));
    }
}

fn push_field(f: Field, stack: &mut Vec<Node>) {
    match f {
        Field::Pos(v) | Field::Named(_, v, _) => push_expr(v, stack),
        Field::Keyed(k, v) => {
            push_expr(k, stack);
            push_expr(v, stack);
        }
    }
}

fn expr_is_leaf(k: &ExprKind) -> bool {
    matches!(
        k,
        ExprKind::Nil
            | ExprKind::True
            | ExprKind::False
            | ExprKind::Dots
            | ExprKind::Int(_)
            | ExprKind::Num(_)
            | ExprKind::Str(_)
            | ExprKind::Name(_)
    )
}

fn type_is_leaf(t: &Type) -> bool {
    matches!(t, Type::Named(..) | Type::Nil | Type::Literal(_))
}

fn stat_is_leaf(k: &StatKind) -> bool {
    matches!(k, StatKind::Break | StatKind::Continue | StatKind::Goto(..) | StatKind::Label(_))
}

fn push_expr(e: Expr, stack: &mut Vec<Node>) {
    if !expr_is_leaf(&e.kind) {
        stack.push(Node::Expr(e));
    }
}

fn push_exprs(v: Vec<Expr>, stack: &mut Vec<Node>) {
    if v.len() > LONG {
        stack.push(Node::Exprs(v));
        return;
    }
    for e in v {
        push_expr(e, stack);
    }
}

#[allow(clippy::boxed_local, reason = "the tree's own box, moved out of")]
fn push_box(e: Box<Expr>, stack: &mut Vec<Node>) {
    if !expr_is_leaf(&e.kind) {
        stack.push(Node::Expr(*e));
    }
}

fn push_type(t: Type, stack: &mut Vec<Node>) {
    if !type_is_leaf(&t) {
        stack.push(Node::Type(t));
    }
}

fn take_block(b: &mut Block, stack: &mut Vec<Node>) {
    if !b.stats.is_empty() {
        stack.push(Node::Stats(crate::nostd::mem::take(&mut b.stats)));
    }
}

fn take_ty(t: Option<Type>, stack: &mut Vec<Node>) {
    if let Some(t) = t {
        push_type(t, stack);
    }
}

fn take_func(f: &mut FuncBody, stack: &mut Vec<Node>) {
    for p in &mut f.params {
        take_ty(p.ty.take(), stack);
    }
    take_ty(f.vararg_ty.take(), stack);
    for t in f.ret.take().into_iter().flatten() {
        push_type(t, stack);
    }
    take_block(&mut f.body, stack);
}

fn take_expr(k: &mut ExprKind, stack: &mut Vec<Node>) {
    match crate::nostd::mem::replace(k, ExprKind::Nil) {
        ExprKind::Interp(parts) => {
            for p in parts {
                if let InterpExpr::Expr(x) = p {
                    push_expr(x, stack);
                }
            }
        }
        ExprKind::Func(mut f) => take_func(&mut f, stack),
        ExprKind::Table(fields) if fields.len() > LONG => stack.push(Node::Fields(fields)),
        ExprKind::Table(fields) => fields.into_iter().for_each(|f| push_field(f, stack)),
        ExprKind::Bin(_, l, r) | ExprKind::Index(l, r) => {
            push_box(l, stack);
            push_box(r, stack);
        }
        ExprKind::Concat(v) => push_exprs(v, stack),
        ExprKind::Un(_, x) | ExprKind::Paren(x) => push_box(x, stack),
        ExprKind::Call(f, args) | ExprKind::Method(f, _, _, args) => {
            push_box(f, stack);
            push_exprs(args, stack);
        }
        _ => {}
    }
}

/// Taken in place (a statement is large: it is not moved).
fn take_stat(k: &mut StatKind, stack: &mut Vec<Node>) {
    fn ex(e: &mut Expr, stack: &mut Vec<Node>) {
        if !expr_is_leaf(&e.kind) {
            stack.push(Node::Expr(crate::nostd::mem::replace(e, Expr { kind: ExprKind::Nil, pos: e.pos, end: e.end })));
        }
    }
    fn exs(v: &mut Vec<Expr>, stack: &mut Vec<Node>) {
        push_exprs(crate::nostd::mem::take(v), stack);
    }
    fn names(v: &mut Vec<LocalName>, stack: &mut Vec<Node>) {
        if v.len() > 16 {
            // A list of its own, freed a name at a time (`Dropper` counts each): one
            // statement of a hundred names was a hundred frees in one piece.
            stack.push(Node::Names(crate::nostd::mem::take(v)));
            return;
        }
        for n in v {
            take_ty(n.ty.take(), stack);
        }
    }
    match k {
        StatKind::Call(e) => ex(e, stack),
        StatKind::Local(ns, es) => {
            names(ns, stack);
            exs(es, stack);
        }
        StatKind::Assign(ts, es, _) => {
            exs(ts, stack);
            exs(es, stack);
        }
        StatKind::OpAssign(b) => {
            let (t, _, e) = &mut **b;
            ex(t, stack);
            ex(e, stack);
        }
        StatKind::Do(b) => take_block(b, stack),
        StatKind::While(c, b) | StatKind::Repeat(b, c) => {
            ex(c, stack);
            take_block(b, stack);
        }
        StatKind::If(conds, els) => {
            for (c, b) in conds {
                ex(c, stack);
                take_block(b, stack);
            }
            if let Some(b) = els {
                take_block(b, stack);
            }
        }
        StatKind::NumFor(f) => {
            let ForNum { var, start, limit, step, body, .. } = &mut **f;
            take_ty(var.ty.take(), stack);
            ex(start, stack);
            ex(limit, stack);
            if let Some(s) = step {
                ex(s, stack);
            }
            take_block(body, stack);
        }
        StatKind::GenFor { names: ns, exprs, body, .. } => {
            names(ns, stack);
            exs(exprs, stack);
            take_block(body, stack);
        }
        StatKind::Function { body, .. } => take_func(body, stack),
        StatKind::LocalFunction(n, body) => {
            take_ty(n.ty.take(), stack);
            take_func(body, stack);
        }
        StatKind::Return(es) => exs(es, stack),
        StatKind::TypeDef(_, t) => {
            if !type_is_leaf(t) {
                stack.push(Node::Type(crate::nostd::mem::replace(t, Type::Nil)));
            }
        }
        StatKind::Break | StatKind::Continue | StatKind::Goto(..) | StatKind::Label(_) => {}
    }
}

fn take_type(t: &mut Type, stack: &mut Vec<Node>) {
    // `Type` is itself an enum with a `Drop`: its parts are taken in place, and what is left
    // (empty boxes and lists) drops shallowly.
    let ty = |b: &mut Box<Type>| crate::nostd::mem::replace(&mut **b, Type::Nil);
    match t {
        Type::Optional(b) | Type::Array(b) => push_type(ty(b), stack),
        Type::Map(k, v) => {
            push_type(ty(k), stack);
            push_type(ty(v), stack);
        }
        Type::Union(v) => crate::nostd::mem::take(v).into_iter().for_each(|t| push_type(t, stack)),
        Type::Func { params, ret, .. } => {
            crate::nostd::mem::take(params).into_iter().for_each(|t| push_type(t, stack));
            crate::nostd::mem::take(ret).into_iter().for_each(|t| push_type(t, stack));
        }
        Type::Shape(fields) => crate::nostd::mem::take(fields).into_iter().for_each(|(_, t)| push_type(t, stack)),
        Type::Named(..) | Type::Nil | Type::Literal(_) => {}
    }
}

impl Drop for Expr {
    fn drop(&mut self) {
        if !expr_is_leaf(&self.kind) {
            let mut stack = Vec::new();
            take_expr(&mut self.kind, &mut stack);
            drain(stack);
        }
    }
}

impl Drop for Stat {
    fn drop(&mut self) {
        if !stat_is_leaf(&self.kind) {
            let mut stack = Vec::new();
            take_stat(&mut self.kind, &mut stack);
            drain(stack);
        }
    }
}

impl Drop for Block {
    fn drop(&mut self) {
        if !self.stats.is_empty() {
            let mut stack = Vec::new();
            for s in &mut self.stats {
                take_stat(&mut s.kind, &mut stack);
            }
            drain(stack);
        }
    }
}

impl Drop for Type {
    fn drop(&mut self) {
        if !type_is_leaf(self) {
            let mut stack = Vec::new();
            take_type(self, &mut stack);
            drain(stack);
        }
    }
}

/// Lua's `constfolding` (`lcode.c`) of `a op b` on constants, exactly: only numbers (no
/// string coercion); bitwise operations only when both have exact integer values (floats
/// included); no division, floor division or modulo by zero; and no float result that is
/// NaN or zero (either sign), which would not round-trip as a constant. None: not folded.
/// Shared by both parsers (literals) and both compilers (`<const>` locals, S5COR-13).
pub(crate) fn fold_bin(op: BinOp, a: crate::Value, b: crate::Value) -> Option<crate::Value> {
    use crate::Value::{Int, Num};
    use crate::number;
    let int_of = |v: crate::Value| match v {
        Int(i) => Some(i),
        Num(f) => number::float_to_int(f),
        _ => None,
    };
    let num_of = |v: crate::Value| match v {
        Int(i) => Some(i as f64),
        Num(f) => Some(f),
        _ => None,
    };
    let (x, y) = (num_of(a)?, num_of(b)?);
    let r = match op {
        BinOp::BAnd | BinOp::BOr | BinOp::BXor | BinOp::Shl | BinOp::Shr => {
            let (i, j) = (int_of(a)?, int_of(b)?);
            return Some(Int(match op {
                BinOp::BAnd => i & j,
                BinOp::BOr => i | j,
                BinOp::BXor => i ^ j,
                BinOp::Shl => number::shift_left(i, j),
                _ => number::shift_left(i, j.wrapping_neg()),
            }));
        }
        BinOp::Div | BinOp::IDiv | BinOp::Mod if y == 0.0 => return None,
        BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::IDiv | BinOp::Mod if matches!((a, b), (Int(_), Int(_))) => {
            let (Int(i), Int(j)) = (a, b) else { return None };
            return Some(Int(match op {
                BinOp::Add => i.wrapping_add(j),
                BinOp::Sub => i.wrapping_sub(j),
                BinOp::Mul => i.wrapping_mul(j),
                BinOp::IDiv => number::int_floor_div(i, j),
                _ => number::int_mod(i, j),
            }));
        }
        BinOp::Add => x + y,
        BinOp::Sub => x - y,
        BinOp::Mul => x * y,
        BinOp::Div => x / y,
        BinOp::Pow => x.powf(y),
        BinOp::IDiv => (x / y).floor(),
        BinOp::Mod => number::float_mod(x, y),
        _ => return None,
    };
    (!r.is_nan() && r != 0.0).then_some(Num(r))
}

/// `op v` folded as Lua's code generator does (`constfolding` for `-` and `~`, `codenot`
/// for `not`), or None.
pub(crate) fn fold_un(op: UnOp, v: crate::Value) -> Option<crate::Value> {
    use crate::Value::{Bool, Int, Nil, Num, Str};
    match (op, v) {
        (UnOp::Neg, Int(i)) => Some(Int(i.wrapping_neg())),
        (UnOp::Neg, Num(f)) => (f != 0.0 && !f.is_nan()).then_some(Num(-f)),
        (UnOp::BNot, Int(i)) => Some(Int(!i)),
        (UnOp::BNot, Num(f)) => crate::number::float_to_int(f).map(|i| Int(!i)),
        (UnOp::Not, Nil | Bool(false)) => Some(Bool(true)),
        (UnOp::Not, Bool(true) | Int(_) | Num(_) | Str(_)) => Some(Bool(false)),
        _ => None,
    }
}

/// A literal's value for folding: numbers, booleans and `nil` (strings only for `not`).
pub(crate) fn literal_value(e: &ExprKind) -> Option<crate::Value> {
    match e {
        ExprKind::Int(i) => Some(crate::Value::Int(*i)),
        ExprKind::Num(f) => Some(crate::Value::Num(*f)),
        ExprKind::True => Some(crate::Value::Bool(true)),
        ExprKind::False => Some(crate::Value::Bool(false)),
        ExprKind::Nil => Some(crate::Value::Nil),
        _ => None,
    }
}

/// An expression of a folded value (a number or a boolean).
pub(crate) fn literal_kind(v: crate::Value) -> Option<ExprKind> {
    match v {
        crate::Value::Int(i) => Some(ExprKind::Int(i)),
        crate::Value::Num(f) => Some(ExprKind::Num(f)),
        crate::Value::Bool(true) => Some(ExprKind::True),
        crate::Value::Bool(false) => Some(ExprKind::False),
        _ => None,
    }
}

/// The line C Lua would number an interpolated string's concatenation with, written out
/// as its parts joined by `..` (S6-FRONT-LR6: the last `..`, just after the part before
/// the last: where an expression part ends, or where a literal part ends, at the `{` of
/// the expression after it), so an error in a `..=` merged into it is reported where its
/// desugaring's is. `pos` when there is no such part.
pub(crate) fn interp_join_line(parts: &[InterpExpr], pos: Pos) -> u32 {
    let n = parts.len();
    match (n.checked_sub(2).and_then(|i| parts.get(i)), parts.last()) {
        (Some(InterpExpr::Expr(e)), _) => e.end.line,
        (Some(InterpExpr::Lit(_)), Some(InterpExpr::Expr(e))) => e.pos.line,
        _ => pos.line,
    }
}

/// The line of the token after assignment target `i` (`StatKind::Assign`'s third part).
pub(crate) fn follow_line(targets: &[Expr], follow: &[u32], i: usize) -> u32 {
    follow.get(i).copied().unwrap_or_else(|| targets.get(i).map_or(0, |t| t.end.line))
}

/// Notes the line of the token after the target just pushed (the last of `targets`), kept
/// only once one differs from its target's last line (no allocation otherwise).
pub(crate) fn note_follow(targets: &[Expr], follow: &mut Vec<u32>, line: u32) {
    let i = targets.len().saturating_sub(1);
    if follow.is_empty() && targets.last().is_some_and(|t| t.end.line == line) {
        return;
    }
    if follow.is_empty() {
        follow.extend(targets.iter().take(i).map(|t| t.end.line));
    }
    follow.push(line);
}

// ---- what a tree holds ----

/// A part of a tree, by reference, for `nodes_bytes`'s walk.
enum Part<'a> {
    Expr(&'a Expr),
    Stat(&'a Stat),
    Block(&'a Block),
    Type(&'a Type),
    Func(&'a FuncBody),
    Name(&'a LocalName),
}

fn cap<T>(v: &Vec<T>) -> usize {
    v.capacity() * crate::nostd::mem::size_of::<T>()
}

/// The heap bytes `nodes` hold, exactly (every buffer at its capacity, every box), walked
/// without recursion (S6-FRONT-4: what a compile leaves to free is charged this, not an
/// estimate, so a host allocation right after a huge load does not meet a false limit).
fn nodes_bytes(nodes: &[Node]) -> usize {
    let mut todo: Vec<Part<'_>> = Vec::new();
    let mut n = 0usize;
    for node in nodes {
        match node {
            Node::Expr(e) => todo.push(Part::Expr(e)),
            Node::Stats(v) => {
                n += cap(v);
                todo.extend(v.iter().map(Part::Stat));
            }
            Node::Exprs(v) => {
                n += cap(v);
                todo.extend(v.iter().map(Part::Expr));
            }
            Node::Fields(v) => {
                n += cap(v);
                for f in v {
                    field(f, &mut n, &mut todo);
                }
            }
            Node::Conds(v) => {
                n += cap(v);
                for (c, b) in v {
                    todo.push(Part::Expr(c));
                    todo.push(Part::Block(b));
                }
            }
            Node::Names(v) => {
                n += cap(v);
                todo.extend(v.iter().map(Part::Name));
            }
            Node::Type(t) => todo.push(Part::Type(t)),
        }
    }
    while let Some(p) = todo.pop() {
        n += part_bytes(p, &mut todo);
    }
    n
}

fn field<'a>(f: &'a Field, n: &mut usize, todo: &mut Vec<Part<'a>>) {
    match f {
        Field::Pos(v) => todo.push(Part::Expr(v)),
        Field::Named(s, v, _) => {
            *n += s.capacity();
            todo.push(Part::Expr(v));
        }
        Field::Keyed(k, v) => {
            todo.push(Part::Expr(k));
            todo.push(Part::Expr(v));
        }
    }
}

/// A part's own bytes (its buffers and boxes); its parts go on `todo`.
fn part_bytes<'a>(p: Part<'a>, todo: &mut Vec<Part<'a>>) -> usize {
    let exprs = |v: &'a Vec<Expr>, todo: &mut Vec<Part<'a>>| {
        todo.extend(v.iter().map(Part::Expr));
        cap(v)
    };
    let names = |v: &'a Vec<LocalName>, todo: &mut Vec<Part<'a>>| {
        todo.extend(v.iter().map(Part::Name));
        cap(v)
    };
    let boxed = crate::nostd::mem::size_of::<Expr>();
    match p {
        Part::Expr(e) => match &e.kind {
            ExprKind::Str(v) => v.capacity(),
            ExprKind::Name(s) => s.capacity(),
            ExprKind::Interp(v) => {
                let mut n = cap(v);
                for i in v {
                    match i {
                        InterpExpr::Lit(b) => n += b.capacity(),
                        InterpExpr::Expr(x) => todo.push(Part::Expr(x)),
                    }
                }
                n
            }
            ExprKind::Func(f) => {
                todo.push(Part::Func(f));
                crate::nostd::mem::size_of::<FuncBody>()
            }
            ExprKind::Table(v) => {
                let mut n = cap(v);
                for f in v {
                    field(f, &mut n, todo);
                }
                n
            }
            ExprKind::Bin(_, l, r) | ExprKind::Index(l, r) => {
                todo.push(Part::Expr(l));
                todo.push(Part::Expr(r));
                2 * boxed
            }
            ExprKind::Concat(v) => exprs(v, todo),
            ExprKind::Un(_, x) | ExprKind::Paren(x) => {
                todo.push(Part::Expr(x));
                boxed
            }
            ExprKind::Call(f, args) => {
                todo.push(Part::Expr(f));
                boxed + exprs(args, todo)
            }
            ExprKind::Method(f, s, _, args) => {
                todo.push(Part::Expr(f));
                boxed + s.capacity() + exprs(args, todo)
            }
            ExprKind::Nil | ExprKind::True | ExprKind::False | ExprKind::Dots | ExprKind::Int(_) | ExprKind::Num(_) => 0,
        },
        Part::Block(b) => {
            todo.extend(b.stats.iter().map(Part::Stat));
            cap(&b.stats)
        }
        Part::Name(l) => {
            if let Some(t) = &l.ty {
                todo.push(Part::Type(t));
            }
            l.name.capacity()
        }
        Part::Func(f) => {
            let mut n = cap(&f.params);
            for p in &f.params {
                n += p.name.capacity();
                if let Some(t) = &p.ty {
                    todo.push(Part::Type(t));
                }
            }
            if let Some(t) = &f.vararg_ty {
                todo.push(Part::Type(t));
            }
            if let Some(r) = &f.ret {
                n += cap(r);
                todo.extend(r.iter().map(Part::Type));
            }
            todo.push(Part::Block(&f.body));
            n
        }
        Part::Type(t) => match t {
            Type::Named(s, _) => s.capacity(),
            Type::Literal(b) => b.capacity(),
            Type::Nil => 0,
            Type::Optional(b) | Type::Array(b) => {
                todo.push(Part::Type(b));
                crate::nostd::mem::size_of::<Type>()
            }
            Type::Map(k, v) => {
                todo.push(Part::Type(k));
                todo.push(Part::Type(v));
                2 * crate::nostd::mem::size_of::<Type>()
            }
            Type::Union(v) => {
                todo.extend(v.iter().map(Part::Type));
                cap(v)
            }
            Type::Func { params, ret, .. } => {
                todo.extend(params.iter().map(Part::Type));
                todo.extend(ret.iter().map(Part::Type));
                cap(params) + cap(ret)
            }
            Type::Shape(v) => {
                let mut n = cap(v);
                for (s, t) in v {
                    n += s.capacity();
                    todo.push(Part::Type(t));
                }
                n
            }
        },
        Part::Stat(s) => stat_bytes(s, todo, exprs, names),
    }
}

fn stat_bytes<'a>(
    s: &'a Stat,
    todo: &mut Vec<Part<'a>>,
    exprs: impl Fn(&'a Vec<Expr>, &mut Vec<Part<'a>>) -> usize,
    names: impl Fn(&'a Vec<LocalName>, &mut Vec<Part<'a>>) -> usize,
) -> usize {
    match &s.kind {
        StatKind::Call(e) => {
            todo.push(Part::Expr(e));
            0
        }
        StatKind::Local(ns, es) => names(ns, todo) + exprs(es, todo),
        StatKind::Assign(ts, es, follow) => exprs(ts, todo) + exprs(es, todo) + cap(follow),
        StatKind::OpAssign(b) => {
            todo.push(Part::Expr(&b.0));
            todo.push(Part::Expr(&b.2));
            crate::nostd::mem::size_of::<(Expr, BinOp, Expr)>()
        }
        StatKind::Do(b) => {
            todo.push(Part::Block(b));
            0
        }
        StatKind::While(c, b) | StatKind::Repeat(b, c) => {
            todo.push(Part::Expr(c));
            todo.push(Part::Block(b));
            0
        }
        StatKind::If(conds, els) => {
            for (c, b) in conds {
                todo.push(Part::Expr(c));
                todo.push(Part::Block(b));
            }
            if let Some(b) = els {
                todo.push(Part::Block(b));
            }
            cap(conds)
        }
        StatKind::NumFor(f) => {
            todo.push(Part::Name(&f.var));
            todo.push(Part::Expr(&f.start));
            todo.push(Part::Expr(&f.limit));
            if let Some(e) = &f.step {
                todo.push(Part::Expr(e));
            }
            todo.push(Part::Block(&f.body));
            crate::nostd::mem::size_of::<ForNum>()
        }
        StatKind::GenFor { names: ns, exprs: es, body, .. } => {
            todo.push(Part::Block(body));
            names(ns, todo) + exprs(es, todo)
        }
        StatKind::Function { path, method, body } => {
            todo.push(Part::Func(body));
            cap(path) + path.iter().map(String::capacity).sum::<usize>() + method.as_ref().map_or(0, String::capacity) + crate::nostd::mem::size_of::<FuncBody>()
        }
        StatKind::LocalFunction(n, body) => {
            todo.push(Part::Name(n));
            todo.push(Part::Func(body));
            crate::nostd::mem::size_of::<FuncBody>()
        }
        StatKind::Return(es) => exprs(es, todo),
        StatKind::Goto(s, _) | StatKind::Label(s) => s.capacity(),
        StatKind::TypeDef(s, t) => {
            todo.push(Part::Type(t));
            s.capacity()
        }
        StatKind::Break | StatKind::Continue => 0,
    }
}

/// A statement's own bytes: its buffers and boxes, not its parts'.
fn stat_own_bytes(s: &Stat) -> usize {
    let mut todo = Vec::new();
    part_bytes(Part::Stat(s), &mut todo)
}

/// An expression's own bytes: its buffers and boxes, not its parts'.
fn expr_own_bytes(e: &Expr) -> usize {
    let mut todo = Vec::new();
    part_bytes(Part::Expr(e), &mut todo)
}
