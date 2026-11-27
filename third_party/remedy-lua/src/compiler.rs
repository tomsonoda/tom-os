//! The compiler: syntax tree to register-machine bytecode.
//!
//! One pass over the tree per function. Locals live in registers from the bottom of the
//! frame; temporaries are allocated above them stack-wise. Conditions compile to compare
//! and test instructions followed by jumps, patched once their targets are known. `goto`,
//! `break` and `continue` share one mechanism: pending jumps resolved against labels, with
//! the same scope rules as Lua 5.4.
//!
//! It does not recurse. The walk over the tree is a stack of tasks: a task either finishes
//! with its result in `ret` or pushes its continuation and then the task it waits for.
//! The stack (and the stack of functions being compiled) is charged to the state's memory
//! while it is used, so nesting is bounded by memory alone ("not enough memory").
//!
//! It is checked against the recursive compiler it replaced (`front_oracle`, feature
//! `front-oracle`): the same bytecode for every chunk.

#[allow(unused_imports)]
use crate::nostd::prelude::*;
use crate::nostd::collections::HashMap;
use crate::nostd::sync::Arc;

use crate::ast::*;
use crate::bytecode::{ArithOp, Instr as MInstr, LocVar, Op, Proto, Rk as MRk, SETLIST_MULTI, UpvalDesc};
use crate::diag::{Diagnostic, Pos};
use crate::heap::Heap;
use crate::value::Value;

type CResult<T> = Result<T, Diagnostic>;

/// A register, counted from the frame's base: bounded by memory alone. The machine's
/// instructions have 16-bit registers; `emit` maps these onto them, reaching the ones past
/// a window through `Window` and `MoveAbs` (only in functions that have that many).
type Reg = u32;
/// The compiler's instructions: the machine's, over its own registers.
type Instr = Op<Reg, Rk>;
/// "All of them" for a count (a machine count field writes it 0xffff, `cnt`).
const MULTI: Reg = Reg::MAX;
/// The largest count a machine count field holds; larger ones go through `SetTop` (values
/// in) and `PadNil` (results out) with the field MULTI.
const MAX_COUNT: Reg = 0xfffe;
/// Registers one window holds: those below it need no `Window` (every function that fits,
/// that is nearly all, compiles exactly as without windows).
const WIN: usize = 0x7fff;
/// Free registers `emit` may take above the others to reach far ones.
const SCRATCH: usize = 4;

/// A register or a constant (the high bit).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Rk(u32);

impl Rk {
    const CONST: u32 = 1 << 31;
    fn reg(r: Reg) -> Rk {
        Rk(r)
    }
    fn konst(k: u16) -> Rk {
        Rk(u32::from(k) | Self::CONST)
    }
    fn is_const(self) -> bool {
        self.0 & Self::CONST != 0
    }
    fn index(self) -> usize {
        (self.0 & !Self::CONST) as usize
    }
}

/// A `Jmp`'s `close` field for closing from register `c` (from the frame's base): only
/// when `close_fits(c)` (else a `Close` does it).
fn close_field(c: Reg) -> u16 {
    cnt(c + 1).min(0xfffe)
}

/// Whether a `Jmp` can close from register `c` itself.
fn close_fits(c: Reg) -> bool {
    c < 0xfffd
}

/// A register or count as an index.
/// Where a constructor's `k = v` or `[k] = v` store is attributed: C Lua emits it once
/// the value is read, on the line its last token ends on (`ls->lastline`), so an error
/// ("table index is nil") in `{[k] = function() ... end}` is on the line of `end` (LR-4).
pub(crate) fn field_store_pos(v: &Expr) -> Pos {
    v.end
}

fn us<T: Into<u64>>(x: T) -> usize {
    x.into() as usize
}

/// A count field: MULTI as 0xffff, else the count (at most `MAX_COUNT`: the `emit_*`
/// helpers route larger ones through `SetTop`/`PadNil`).
fn cnt(n: Reg) -> u16 {
    if n == MULTI { 0xffff } else { n.min(MAX_COUNT) as u16 }
}

/// How an instruction uses a register span.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Use {
    Read,
    Write,
    Both,
}

/// The registers an instruction names: (first, count, use). Counts past one register are
/// spans that must lie in one window; single registers may be far (moved in or out).
fn operands(i: &Instr, top: Reg, out: &mut Vec<(Reg, Reg, Use)>) {
    use Use::{Both, Read, Write};
    let mut one = |r: Reg, u: Use| out.push((r, 1, u));
    let rk = |k: Rk| (!k.is_const()).then_some(k.index() as Reg);
    match *i {
        Op::Move { a, b } | Op::Unm { a, b } | Op::BNot { a, b } | Op::Not { a, b } | Op::Len { a, b } | Op::ToStr { a, b } => {
            one(b, Read);
            one(a, Write);
        }
        Op::LoadK { a, .. }
        | Op::LoadInt { a, .. }
        | Op::LoadBool { a, .. }
        | Op::LoadFalseSkip { a }
        | Op::GetUpval { a, .. }
        | Op::GetSelf { a, .. }
        | Op::GetTabUp { a, .. }
        | Op::NewTable { a, .. }
        | Op::Closure { a, .. } => one(a, Write),
        Op::SetUpval { a, .. } | Op::Test { a, .. } => one(a, Read),
        // A level, not a value: the register itself must be in the window (a span keeps it
        // from being moved through a scratch register).
        Op::Tbc { a } | Op::Close { a } => out.push((a, 2, Read)),
        Op::SetTabUp { v, .. } => {
            if let Some(r) = rk(v) {
                one(r, Read);
            }
        }
        Op::GetTable { a, b, c } | Op::Add { a, b, c } | Op::Sub { a, b, c } | Op::Mul { a, b, c } | Op::Div { a, b, c } | Op::Mod { a, b, c } | Op::IDiv { a, b, c } | Op::Pow { a, b, c } | Op::BAnd { a, b, c } | Op::BOr { a, b, c } | Op::BXor { a, b, c } | Op::Shl { a, b, c } | Op::Shr { a, b, c } => {
            one(b, Read);
            one(c, Read);
            one(a, Write);
        }
        Op::GetTableK { a, b, .. }
        | Op::AddK { a, b, .. }
        | Op::SubK { a, b, .. }
        | Op::MulK { a, b, .. }
        | Op::DivK { a, b, .. }
        | Op::ModK { a, b, .. }
        | Op::IDivK { a, b, .. }
        | Op::AddI { a, b, .. }
        | Op::PowK { a, b, .. }
        | Op::BAndK { a, b, .. }
        | Op::BOrK { a, b, .. }
        | Op::BXorK { a, b, .. }
        | Op::ShlK { a, b, .. }
        | Op::ShrK { a, b, .. } => {
            one(b, Read);
            one(a, Write);
        }
        Op::SetTable { a, b, c } => {
            one(a, Read);
            one(b, Read);
            if let Some(r) = rk(c) {
                one(r, Read);
            }
        }
        Op::SetTableK { a, c, .. } => {
            one(a, Read);
            if let Some(r) = rk(c) {
                one(r, Read);
            }
        }
        Op::SelfK { a, b, .. } => {
            one(b, Read);
            out.push((a, 2, Write));
        }
        Op::Arith { a, b, c, .. } => {
            for k in [b, c] {
                if let Some(r) = rk(k) {
                    one(r, Read);
                }
            }
            one(a, Write);
        }
        Op::EqK { a, .. } | Op::EqI { a, .. } | Op::LtI { a, .. } | Op::LeI { a, .. } | Op::GtI { a, .. } | Op::GeI { a, .. } => one(a, Read),
        Op::Eq { b, c, .. } | Op::Lt { b, c, .. } | Op::Le { b, c, .. } => {
            for k in [b, c] {
                if let Some(r) = rk(k) {
                    one(r, Read);
                }
            }
        }
        Op::TestSet { a, b, .. } => {
            one(b, Read);
            one(a, Write);
        }
        Op::LoadNil { a, n } => out.push((a, Reg::from(n) + 1, Write)),
        Op::Concat { a, n } => out.push((a, Reg::from(n).max(1), Both)),
        Op::Call { a, b, c } => {
            let args = if b == 0xffff { top.saturating_sub(a).max(1) } else { Reg::from(b) + 1 };
            let res = if c == 0xffff { 1 } else { Reg::from(c).max(1) };
            out.push((a, args.max(res), Both));
        }
        Op::TailCall { a, b } => out.push((a, if b == 0xffff { top.saturating_sub(a).max(1) } else { Reg::from(b) + 1 }, Read)),
        Op::Return { a, b } => out.push((a, if b == 0xffff { top.saturating_sub(a).max(1) } else { Reg::from(b).max(1) }, Read)),
        Op::VarArg { a, n } => out.push((a, if n == 0xffff { 1 } else { Reg::from(n).max(1) }, Write)),
        Op::SetList { a, n, .. } => out.push((a, Reg::from(n) + 1, Read)),
        Op::ForPrep { a, .. } | Op::ForLoop { a, .. } => out.push((a, 4, Both)),
        Op::TForPrep { a, .. } | Op::TForLoop { a, .. } => out.push((a, 5, Both)),
        Op::TForCall { a, c } => out.push((a, 4 + Reg::from(c), Both)),
        Op::Return1 { a } => one(a, Read),
        Op::Jmp { .. } | Op::Window { .. } | Op::MoveAbs { .. } | Op::SetTop { .. } | Op::PadNil { .. } | Op::Return0 => {}
    }
}

/// The registers an instruction needs in its window when its whole span does not fit
/// one: calls, tail calls, returns, `...` with all its values and the generic `for`'s
/// iterator call read and write their spans on the stack itself (the machine makes sure
/// of it), so only their first registers (`a` and the next; the iterator call, the six it
/// copies) need it. None for the others.
fn anchored(i: &Instr) -> Option<(Reg, Reg)> {
    match *i {
        Op::Call { a, .. } | Op::TailCall { a, .. } | Op::Return { a, .. } => Some((a, 2)),
        Op::VarArg { a, n: 0xffff } => Some((a, 2)),
        Op::TForCall { a, .. } => Some((a, 7)),
        _ => None,
    }
}

/// Array items stored per `SetList`.
pub(crate) const LIST_FLUSH: usize = 50;
pub(crate) const BREAK: &str = "\u{1}break";
pub(crate) const CONTINUE: &str = "\u{1}continue";

#[derive(Clone)]
enum VarKind {
    Reg(Reg),
    /// A `<const>` local with a constant value: no register.
    Const(Value),
}

#[derive(Clone)]
struct ActiveVar {
    name: Arc<str>,
    kind: VarKind,
    attrib: Attrib,
    locvar: usize,
}

pub(crate) struct BlockScope {
    pub(crate) nactvar: usize,
    pub(crate) first_label: usize,
    pub(crate) first_goto: usize,
    pub(crate) is_loop: bool,
    /// A local of this block is captured by a closure or is to-be-closed.
    pub(crate) has_upval: bool,
}

pub(crate) struct LabelDesc {
    pub(crate) name: String,
    pub(crate) pc: usize,
    pub(crate) nactvar: usize,
    pub(crate) line: u32,
}

struct GotoDesc {
    name: String,
    pc: usize,
    line: u32,
    nactvar: usize,
    /// Register level to close when the jump is taken (leaves scopes with upvalues).
    close: Option<Reg>,
}

impl crate::diag::KeyHeap for ConstKey {}

#[derive(Hash, PartialEq, Eq)]
enum ConstKey {
    Int(i64),
    Num(u64),
    Bool(bool),
    Nil,
}

struct FuncState {
    code: Vec<MInstr>,
    lines: Vec<u32>,
    /// Above the last pc a jump goes to (C Lua's `lasttarget`): only past it may the last
    /// instruction be changed in place.
    last_target: usize,
    consts: Vec<Value>,
    /// Sharded (see `shardmap`): a function of a million constants.
    const_map: crate::shardmap::ShardMap<ConstKey, u32>,
    /// String constants (apart from the others: looked up by their bytes).
    str_map: crate::shardmap::ShardMap<Vec<u8>, u32>,
    protos: Vec<u32>,
    upvals: Vec<UpvalDesc>,
    locvars: Vec<LocVar>,
    actives: Vec<ActiveVar>,
    blocks: Vec<BlockScope>,
    labels: Vec<LabelDesc>,
    gotos: Vec<GotoDesc>,
    freereg: usize,
    max_stack: usize,
    /// Registers taken in all when past one window (scratch ones included).
    frame: usize,
    num_params: Reg,
    is_vararg: bool,
    line_defined: u32,
    /// Captured locals: (index in `actives`) → captured.
    captured: Vec<bool>,
    /// A to-be-closed variable is active (no tail calls then).
    tbc_active: usize,
    /// Bytes of its buffers charged to the state's memory (`charge_fs`).
    inner: usize,
}

/// Target of a value: how many values an expression list should produce.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Want {
    Fixed(usize),
    Multi,
}

/// What a finished task hands to the task waiting for it.
enum Ret {
    None,
    Reg(Reg),
    Rk(Rk),
    Key(KeyRef),
    Place(Place),
    /// A count field (arguments, values).
    Count(Reg),
    Jumps(Vec<usize>),
    Proto(u32),
}

/// A table constructor's progress.
#[derive(Clone, Copy)]
struct TableSt<'a> {
    fields: &'a [Field],
    i: usize,
    t: Reg,
    dst: Reg,
    pos: Pos,
    pending: usize,
    stored: usize,
    /// The free register to restore at the end (the expression's).
    save: Reg,
}

/// An expression list's progress.
#[derive(Clone, Copy)]
struct ExplistSt<'a> {
    exprs: &'a [Expr],
    i: usize,
    want: Want,
    pos: Pos,
    base: usize,
}

/// A step of the walk: the first group starts a construct, the rest continue one with the
/// result of what they waited for in `ret`.
enum Task<'a> {
    /// An error found before a construct that C Lua reports after it (compiled first).
    Fail(Box<Diagnostic>),
    Block { b: &'a Block, repeat: bool },
    ExprToReg { e: &'a Expr, dst: Reg },
    ExprNext(&'a Expr),
    ExprAnyReg(&'a Expr),
    ExprRk(&'a Expr),
    KeyRef(&'a Expr),
    CondJump { e: &'a Expr, when: bool },
    Multi { e: &'a Expr, n: Reg },
    Function(&'a FuncBody),

    // ---- continuations ----
    BlockNext { b: &'a Block, i: usize, repeat: bool },
    LeaveBlock(Pos),
    FunctionEnd(&'a FuncBody),
    ExprNextCall(Pos),
    CallFinish { e: &'a Expr, want: Want, base: Reg },
    SetupArgs { args: &'a [Expr], pos: Pos },
    SetupMethod { e: &'a Expr, base: Reg },
    SetupMethodArgs,
    MultiFinish { e: &'a Expr, n: Reg, base: Reg },
    ExplistNext(ExplistSt<'a>),
    ExplistAfterMulti { st: ExplistSt<'a>, needed: usize },
    ExplistRetMulti,
    IndexObj { key: &'a Expr, dst: Reg, pos: Pos, save: Reg },
    IndexKey { t: Reg, dst: Reg, pos: Pos, save: Reg },
    CallToReg { dst: Reg, save: Reg },
    FuncToReg { dst: Reg, pos: Pos, save: Reg },
    ConcatNext { parts: &'a [Expr], i: usize, base: Reg, dst: Reg, pos: Pos, save: Reg },
    InterpExpr { parts: &'a [InterpExpr], i: usize, base: Reg, dst: Reg, pos: Pos, save: Reg },
    UnTo { op: UnOp, dst: Reg, pos: Pos, save: Reg },
    TablePos(TableSt<'a>),
    TableMulti(TableSt<'a>),
    TableKey { st: TableSt<'a>, save: Reg },
    TableKeyVal { st: TableSt<'a>, save: Reg, kr: KeyRef },
    LogicL { op: BinOp, r: &'a Expr, target: Reg, dst: Reg, pos: Pos, save: Reg },
    LogicR { j: usize, target: Reg, dst: Reg, save: Reg },
    CmpToReg { dst: Reg, save: Reg },
    ConcatL { r: &'a Expr, base: Reg, dst: Reg, pos: Pos, save: Reg },
    ConcatR { base: Reg, dst: Reg, pos: Pos, save: Reg },
    ArithL { op: BinOp, r: &'a Expr, dst: Reg, pos: Pos, save: Reg },
    ArithR { op: BinOp, b: Rk, dst: Reg, pos: Pos, save: Reg },
    SpineFirst { spine: Vec<(BinOp, &'a Expr, Pos)>, acc: Reg, dst: Reg, save: Reg },
    SpineNext { spine: Vec<(BinOp, &'a Expr, Pos)>, acc: Reg, dst: Reg, save: Reg, b: Rk, op: BinOp, pos: Pos, save2: Reg },
    /// `l and r` / `l or r` as a jump where the jumps of `l` are patched after `r`.
    CondSkip { r: &'a Expr, when: bool },
    CondSkipEnd(Vec<usize>),
    /// ... and where the jumps of both are the result.
    CondBoth { r: &'a Expr, when: bool },
    CondBothEnd(Vec<usize>),
    CmpL { op: BinOp, r: &'a Expr, pos: Pos, when: bool, save: Reg },
    CmpR { op: BinOp, a: Rk, pos: Pos, when: bool, save: Reg },
    TestJump { pos: Pos, when: bool, save: Reg },
    PlaceObj(&'a Expr),
    PlaceKey(Reg),
    // statements
    LocalAfter { s: &'a Stat, base: Reg, konst: Option<Value> },
    AssignPlace { s: &'a Stat, places: Vec<Place> },
    AssignOne { p: Place, pos: Pos },
    AssignMulti { places: Vec<Place>, base: Reg, pos: Pos },
    OpAssignPlace(&'a Stat),
    OpConcatEnd { p: Place, a: Reg, dst: Reg, base: Reg, pos: Pos },
    OpArithEnd { p: Place, cur: Reg, op: BinOp, pos: Pos },
    WhileCond { body: &'a Block, start: usize },
    WhileBody { body: &'a Block, start: usize, exits: Vec<usize> },
    RepeatBody { body: &'a Block, cond: &'a Expr, start: usize },
    RepeatCond { body: &'a Block, start: usize },
    IfCond { s: &'a Stat, i: usize, to_end: Vec<usize> },
    IfBlock { s: &'a Stat, i: usize, to_end: Vec<usize>, skip: Vec<usize> },
    IfElseEnd { end: Pos, to_end: Vec<usize> },
    NumForStart { s: &'a Stat, base: Reg },
    NumForLimit { s: &'a Stat, base: Reg },
    NumForStep { s: &'a Stat, base: Reg },
    NumForEnd { s: &'a Stat, base: Reg, prep: usize, body_start: usize },
    GenForExprs { s: &'a Stat, base: Reg },
    GenForEnd { s: &'a Stat, base: Reg, prep: usize, body_start: usize },
    Closure { r: Reg, pos: Pos, place: Option<Place> },
    RetTail { base: Reg, pos: Pos },
    RetOne(Pos),
    RetMulti { base: Reg, pos: Pos },
}

pub(crate) struct Compiler<'a, 'h> {
    heap: &'h mut Heap,
    chunk: Arc<str>,
    fs: Vec<FuncState>,
    line: u32,
    tasks: Vec<Task<'a>>,
    ret: Ret,
    /// Bytes of the state's memory the task stack and function stack are charged.
    charged: usize,
    /// Bytes the open functions' buffers are charged (each its `inner`).
    inner: usize,
    /// Steps until the open function's buffers are looked at again (`charge_fs`).
    fs_check: u32,
    /// `fold_logic` of each `and`/`or` node seen (by address), so chains fold in linear time.
    fold_memo: HashMap<usize, &'a Expr>,
    /// An instruction too wide to emit (`emit` cannot fail): reported after the step.
    emit_error: Option<Diagnostic>,
    /// Chains of unary operators and fields reuse their target register (off only to
    /// compare with the old compiler, which did not).
    reuse: bool,
    /// Compiling what `parser::prefix` made: the cut, and whether its token ends a block.
    prefix: Option<(Pos, bool)>,
    /// How many locals and upvalues of every open function have each name: a global is
    /// known at once, not by a walk out through every enclosing function (which made a
    /// global used at each of n nested levels cost n squared steps).
    in_scope: crate::shardmap::ShardMap<Arc<str>, u32>,
}

/// Steps between looks at the open function's buffers (`Compiler::charge_fs`).
const FS_CHECK_EVERY: u32 = 64;

/// Compiles a chunk: a variadic function with `_ENV` as its only upvalue.
pub(crate) fn compile(block: &Block, chunk: &str, heap: &mut Heap) -> CResult<u32> {
    compile_as(block, chunk, heap, true, None)
}

/// `compile`, with or without register reuse along chains of unary operators and fields
/// (without: the old compiler's bytecode exactly, to compare with it).
pub(crate) fn compile_with(block: &Block, chunk: &str, heap: &mut Heap, reuse: bool) -> CResult<u32> {
    compile_as(block, chunk, heap, reuse, None)
}

/// `compile` for what `parser::prefix` made: the main chunk was never closed, so nothing
/// is reported where it ends.
/// The blocks closed at the cut end at `cut`; `follow` tells whether the token there ends
/// a block (for a label just before it).
pub(crate) fn compile_prefix(block: &Block, chunk: &str, heap: &mut Heap, cut: Pos, follow: bool) -> CResult<u32> {
    compile_as(block, chunk, heap, true, Some((cut, follow)))
}

fn compile_as(block: &Block, chunk: &str, heap: &mut Heap, reuse: bool, prefix: Option<(Pos, bool)>) -> CResult<u32> {
    heap.blocked += 1;
    let mut c = Compiler {
        heap,
        chunk: Arc::from(chunk),
        fs: Vec::new(),
        line: 1,
        tasks: Vec::new(),
        ret: Ret::None,
        charged: 0,
        inner: 0,
        fs_check: 0,
        fold_memo: HashMap::<_, _, crate::nostd::collections::hash_map::RandomState>::default(),
        emit_error: None,
        reuse,
        prefix,
        in_scope: crate::shardmap::ShardMap::new(),
    };
    let r = c.main(block);
    if matches!(&r, Err(d) if d.message == crate::diag::COMPILE_STOPPED) {
        // Freed later, so the stopped compile returns at once (S5C-FRONT-12): a multi-
        // megabyte source's locals, debug names and scope map took 100 ms to free.
        crate::diag::defer_pieces(crate::nostd::mem::take(&mut c.in_scope).into_pieces());
        for mut fs in crate::nostd::mem::take(&mut c.fs) {
            crate::diag::defer_pieces(crate::nostd::mem::take(&mut fs.actives));
            crate::diag::defer_pieces(crate::nostd::mem::take(&mut fs.locvars));
            crate::diag::defer_pieces(crate::nostd::mem::take(&mut fs.upvals));
            crate::diag::defer_pieces(crate::nostd::mem::take(&mut fs.str_map).into_pieces());
            crate::diag::defer_drop(fs);
        }
    }
    // The scope map: empty now, but its table may be tens of megabytes to give back.
    release_map(crate::nostd::mem::take(&mut c.in_scope));
    // The stacks were temporary: their charge (and the collector debt it made) goes.
    let charged = c.charged + c.inner;
    c.heap.mem.release(charged);
    c.heap.mem.debt = c.heap.mem.debt.saturating_sub(charged as isize);
    c.heap.blocked -= 1;
    r
}

/// A list the compile is done with: a big one through `diag::release` (later, or a piece
/// at a time), a small one now.
fn release_big<T: Send + 'static>(v: Vec<T>) {
    if v.capacity().saturating_mul(crate::nostd::mem::size_of::<T>()) >= 1 << 16 {
        crate::diag::release(v);
    }
}

/// `release_big` for a map: only a big one is worth the pieces (thousands of nested
/// functions' empty maps, each deferred, held 9 MB).
fn release_map<K: crate::nostd::hash::Hash + Eq + crate::diag::KeyHeap + Send + 'static, V: Send + 'static>(m: crate::shardmap::ShardMap<K, V>) {
    if m.capacity() >= 1024 {
        crate::diag::release(m.into_pieces());
    }
}

/// Makes room for one more element in a compiler list that may grow huge (`grow_watched`).
fn grow_list<T: Send + 'static>(v: &mut Vec<T>) {
    if v.len() == v.capacity() {
        crate::diag::grow_watched(v, v.capacity().saturating_mul(2).max(4));
    }
}

impl FuncState {
    /// Appends `i` at `line`: its index.
    #[inline]
    fn push(&mut self, i: MInstr, line: u32) -> usize {
        self.code.push(i);
        self.lines.push(line);
        self.code.len() - 1
    }

    fn new(line_defined: u32) -> Self {
        FuncState {
            code: Vec::new(),
            lines: Vec::new(),
            consts: Vec::new(),
            const_map: crate::shardmap::ShardMap::new(),
            str_map: crate::shardmap::ShardMap::new(),
            protos: Vec::new(),
            upvals: Vec::new(),
            locvars: Vec::new(),
            actives: Vec::new(),
            blocks: Vec::new(),
            labels: Vec::new(),
            gotos: Vec::new(),
            freereg: 0,
            max_stack: 2,
            frame: 0,
            num_params: 0,
            is_vararg: false,
            line_defined,
            captured: Vec::new(),
            tbc_active: 0,
            last_target: 0,
            inner: 0,
        }
    }

    /// Bytes its buffers hold, as allocated (S6-FRONT-3: only the struct itself was
    /// charged while it was compiled, and nested functions' code, locals and scopes took
    /// about 1 KB a level beside it). Map entries count a word of control and padding.
    fn buffer_bytes(&self) -> usize {
        use crate::nostd::mem::size_of;
        fn v<T>(x: &Vec<T>) -> usize {
            x.capacity() * size_of::<T>()
        }
        v(&self.code)
            + v(&self.lines)
            + v(&self.consts)
            + v(&self.protos)
            + v(&self.upvals)
            + v(&self.locvars)
            + v(&self.actives)
            + v(&self.blocks)
            + v(&self.labels)
            + v(&self.gotos)
            + v(&self.captured)
            + self.const_map.capacity() * (size_of::<(ConstKey, u32)>() + 8)
            + self.str_map.capacity() * (size_of::<(Vec<u8>, u32)>() + 8)
    }

    /// Registers the active locals occupy.
    fn nvarstack(&self) -> usize {
        self.reglevel(self.actives.len())
    }

    /// Registers the first `n` active locals occupy.
    fn reglevel(&self, n: usize) -> usize {
        for (k, v) in self.actives.iter().take(n).rev().enumerate() {
            if let VarKind::Reg(r) = v.kind {
                crate::diag::compile_work(k);
                return us(r) + 1;
            }
        }
        crate::diag::compile_work(n);
        0
    }
}

impl<'a, 'h> Compiler<'a, 'h> {
    fn fs(&mut self) -> &mut FuncState {
        // There is always a function being compiled while compiling.
        let n = self.fs.len();
        #[allow(clippy::indexing_slicing, reason = "fs is never empty while compiling")]
        &mut self.fs[n - 1]
    }

    fn fsr(&self) -> &FuncState {
        let n = self.fs.len();
        #[allow(clippy::indexing_slicing, reason = "fs is never empty while compiling")]
        &self.fs[n - 1]
    }

    fn err(&self, pos: Pos, msg: impl Into<String>) -> Diagnostic {
        Diagnostic::error(&self.chunk, pos, msg)
    }

    fn at(&mut self, pos: Pos) {
        if pos.line > 0 {
            self.line = pos.line;
        }
    }

    /// R[dst] = R[src], unless they are one register.
    fn move_to(&mut self, dst: Reg, src: Reg) {
        if src != dst {
            self.emit(Instr::Move { a: dst, b: src });
        }
    }

    /// Emits `i`: its index. Its registers are the machine's as they are when the function
    /// fits one window; past it, the instruction's spans get a `Window` and far single
    /// registers are moved in and out through free ones (`MoveAbs`).
    fn emit(&mut self, i: Instr) -> usize {
        let i = specialize(i, &self.fsr().consts);
        let line = self.line;
        let fs = self.fs();
        if fs.max_stack <= WIN {
            return fs.push(lower(i, 0, &[]), line);
        }
        self.emit_windowed(i, line)
    }

    #[cold]
    fn emit_windowed(&mut self, i: Instr, line: u32) -> usize {
        let mut ops = Vec::new();
        let top = self.fsr().freereg as Reg;
        operands(&i, top, &mut ops);
        let extent = ops.iter().map(|&(r, n, _)| r as usize + n as usize).max().unwrap_or(0);
        let fs = self.fs();
        if extent <= WIN {
            return fs.push(lower(i, 0, &[]), line);
        }
        // A move between registers too far apart for one window: one instruction.
        if let Op::Move { a, b } = i {
            return self.move_abs(a, b, line);
        }
        // Nils past one window: a run of windows.
        if let Op::LoadNil { a, n } = i
            && usize::from(n) >= WIN / 2
        {
            const CHUNK: Reg = (WIN / 2) as Reg;
            let (mut r, end) = (a, a + Reg::from(n) + 1);
            let mut first = None;
            while r < end {
                let k = (end - r).min(CHUNK);
                let at = self.emit_windowed(Op::LoadNil { a: r, n: (k - 1) as u16 }, line);
                first.get_or_insert(at);
                r += k;
            }
            return first.unwrap_or(0);
        }
        // `...` adjusted to more values than one window holds: all of them, then the rest nil.
        if let Op::VarArg { a, n } = i
            && n != 0xffff
        {
            let at = self.emit_windowed(Op::VarArg { a, n: 0xffff }, line);
            self.pad_nil(a + Reg::from(n), line);
            return at;
        }
        if let Some((a, k)) = anchored(&i) {
            ops.clear();
            ops.push((a, k, Use::Both));
        }
        let hi = ops.iter().map(|&(r, n, _)| r as usize + n as usize).max().unwrap_or(0);
        let fs = self.fs();
        // The window: its spans, and above them (and above the whole of the instruction's
        // spans) the scratch registers far ones go through.
        let scratch = fs.freereg.max(hi).max(extent);
        let lo_span = ops.iter().filter(|o| o.1 > 1).map(|o| o.0 as usize).min();
        let mut off = (scratch + SCRATCH).saturating_sub(WIN);
        if let Some(lo) = lo_span {
            off = off.min(lo);
        }
        let far: Vec<(Reg, Use)> = ops.iter().filter(|o| o.1 == 1 && (o.0 as usize) < off).map(|o| (o.0, o.2)).collect();
        let fits = ops.iter().all(|&(r, n, _)| (r as usize) < off || (r as usize + n as usize) <= off + WIN)
            && (far.is_empty() || scratch + far.len() <= off + WIN)
            && Reg::try_from(scratch + far.len()).is_ok();
        if !fits {
            if self.emit_error.is_none() {
                self.emit_error = Some(self.err(Pos { line, col: 1 }, "too many registers in one instruction (one call, return or list of values)"));
            }
            return self.fsr().code.len();
        }
        let fs = self.fs();
        fs.frame = fs.frame.max(scratch + far.len());
        let mut map: Vec<(Reg, Reg)> = Vec::new();
        for (j, &(r, u)) in far.iter().enumerate() {
            let s = (scratch + j) as Reg;
            if map.iter().any(|m| m.0 == r) {
                continue;
            }
            map.push((r, s));
            if u != Use::Write {
                self.move_abs(s, r, line);
            }
        }
        let fs = self.fs();
        if off > 0 {
            fs.push(MInstr::Window { off: off as u32 }, line);
        }
        let at = fs.push(lower(i, off as Reg, &map), line);
        for &(r, u) in &far {
            if u != Use::Read
                && let Some(&(_, s)) = map.iter().find(|m| m.0 == r)
            {
                self.move_abs(r, s, line);
            }
        }
        at
    }

    /// R[a] = R[b], registers anywhere in the frame: `MoveAbs`, its source reached through
    /// a window when it is past 2^24. Its index.
    fn move_abs(&mut self, a: Reg, b: Reg, line: u32) -> usize {
        let fs = self.fs();
        let off = b & !0xff_ffff;
        if off > 0 {
            fs.push(MInstr::Window { off }, line);
        }
        let b = b - off;
        fs.push(MInstr::MoveAbs { a, b: (b & 0xffff) as u16, b_hi: (b >> 16) as u8 }, line)
    }

    /// The registers from the top up to `n` (from the frame's base) become nil.
    fn pad_nil(&mut self, n: Reg, line: u32) {
        let fs = self.fs();
        fs.push(MInstr::PadNil { n }, line);
        fs.frame = fs.frame.max(n as usize);
    }

    /// The top is register `n` (from the frame's base), for the next instruction's MULTI.
    fn set_top(&mut self, n: Reg) {
        let line = self.line;
        let fs = self.fs();
        fs.push(MInstr::SetTop { n }, line);
        fs.frame = fs.frame.max(n as usize);
    }

    /// A count for a count field, `SetTop` first when it does not fit one (the field is
    /// then MULTI): `a + 1 + n` is where the values end, `skip` 1 for a call's function.
    fn count_in(&mut self, a: Reg, n: Reg, skip: Reg) -> u16 {
        if n != MULTI && n > MAX_COUNT {
            self.set_top(a + skip + n);
            return cnt(MULTI);
        }
        cnt(n)
    }

    /// `n` registers from `a` become nil (`LoadNil`s of at most a window each).
    fn load_nils(&mut self, a: Reg, n: Reg) {
        let mut r = a;
        let end = a + n;
        while r < end {
            let k = (end - r).min(WIN as Reg);
            self.emit(Instr::LoadNil { a: r, n: (k - 1) as u16 });
            r += k;
        }
    }

    /// `Call` with any number of arguments and results.
    fn emit_call(&mut self, a: Reg, nargs: Reg, c: Reg) -> usize {
        let b = self.count_in(a, nargs, 1);
        if c != MULTI && c > MAX_COUNT {
            let at = self.emit(Instr::Call { a, b, c: cnt(MULTI) });
            let line = self.line;
            self.pad_nil(a + c, line);
            return at;
        }
        self.emit(Instr::Call { a, b, c: cnt(c) })
    }

    /// A concatenation of `n` values from `a` whose last value was itself just concatenated
    /// into its register: that instruction takes them all (C Lua's `codeconcat`), so
    /// `a .. (b .. c)` is one instruction. False when it cannot.
    fn merge_concat(&mut self, a: Reg, n: usize) -> bool {
        let fs = self.fs();
        let last = us(a) + n - 1;
        if fs.max_stack > WIN || fs.code.len() <= fs.last_target {
            return false;
        }
        let Some(MInstr::Concat { a: la, n: ln }) = fs.code.last_mut() else { return false };
        let total = n - 1 + usize::from(*ln);
        let (Ok(a16), Ok(n16)) = (u16::try_from(us(a)), u16::try_from(total)) else { return false };
        if usize::from(*la) != last || total > WIN - SCRATCH {
            return false;
        }
        (*la, *ln) = (a16, n16);
        true
    }

    /// `R[a] = R[a] .. … .. R[a+n-1]`, in pieces of at most a window from the top down
    /// when longer (the same steps: concatenation works from the top two down).
    fn emit_concat(&mut self, a: Reg, n: usize) {
        let piece = WIN - SCRATCH;
        let mut n = n;
        while n > piece {
            self.emit(Instr::Concat { a: a + (n - piece) as Reg, n: piece as u16 });
            n = n - piece + 1;
        }
        self.emit(Instr::Concat { a, n: n as u16 });
    }

    /// Sets the jump offset of the (loop) instruction at `pc`.
    fn set_off(&mut self, pc: usize, off: i32) {
        if let Some(slot) = self.fs().code.get_mut(pc) {
            *slot = match *slot {
                MInstr::ForPrep { a, .. } => MInstr::ForPrep { a, off },
                MInstr::ForLoop { a, .. } => MInstr::ForLoop { a, off },
                MInstr::TForPrep { a, .. } => MInstr::TForPrep { a, off },
                MInstr::TForLoop { a, .. } => MInstr::TForLoop { a, off },
                other => other,
            };
        }
    }

    fn pc(&self) -> usize {
        self.fsr().code.len()
    }

    fn set_instr(&mut self, pc: usize, i: MInstr) {
        if let Some(slot) = self.fs().code.get_mut(pc) {
            *slot = i;
        }
    }

    fn instr(&self, pc: usize) -> Option<MInstr> {
        self.fsr().code.get(pc).copied()
    }

    // ---- the machine ----

    fn push(&mut self, t: Task<'a>) {
        self.tasks.push(t);
    }

    /// Pushes a continuation and then the task it waits for. A task on a leaf (a constant,
    /// a name) runs at once: it finishes without pushing tasks; so does a function's start,
    /// which only pushes its body: this never nests.
    fn then(&mut self, cont: Task<'a>, task: Task<'a>) -> CResult<()> {
        self.tasks.push(cont);
        self.start(task)
    }

    /// Starts a task: at once on a leaf, else on the stack.
    fn start(&mut self, task: Task<'a>) -> CResult<()> {
        match task {
            Task::ExprToReg { e, dst } if is_leaf(e) => self.expr_to_reg(e, dst),
            Task::ExprNext(e) if is_leaf(e) => self.expr_next(e),
            Task::ExprAnyReg(e) if is_leaf(e) => self.expr_any_reg(e),
            Task::ExprRk(e) if is_leaf(e) => self.expr_rk(e),
            Task::KeyRef(e) if is_leaf(e) => self.key_ref(e),
            Task::CondJump { e, when } if is_leaf(e) => self.cond_jump(e, when),
            // A function only pushes its body: at once too. (Not calls: their arguments
            // start through `seq`, which would nest call within call.)
            Task::Function(f) => self.function(f),
            task => {
                self.tasks.push(task);
                Ok(())
            }
        }
    }

    fn done(&mut self, r: Ret) -> CResult<()> {
        self.ret = r;
        Ok(())
    }

    fn take(&mut self) -> Ret {
        crate::nostd::mem::replace(&mut self.ret, Ret::None)
    }

    fn take_reg(&mut self) -> Reg {
        match self.take() {
            Ret::Reg(r) => r,
            _ => 0,
        }
    }

    fn take_rk(&mut self) -> Rk {
        match self.take() {
            Ret::Rk(k) => k,
            Ret::Reg(r) => Rk::reg(r),
            _ => Rk::reg(0),
        }
    }

    fn take_key(&mut self) -> KeyRef {
        match self.take() {
            Ret::Key(k) => k,
            Ret::Reg(r) => KeyRef::Reg(r),
            _ => KeyRef::Reg(0),
        }
    }

    fn take_place(&mut self) -> Place {
        match self.take() {
            Ret::Place(p) => p,
            _ => Place::Local(0),
        }
    }

    fn take_count(&mut self) -> Reg {
        match self.take() {
            Ret::Count(n) => n,
            _ => 0,
        }
    }

    fn take_jumps(&mut self) -> Vec<usize> {
        match self.take() {
            Ret::Jumps(j) => j,
            _ => Vec::new(),
        }
    }

    /// Grows the task and function stacks before a step can, when they are nearly full:
    /// doubled, charged first for the old buffer and the new at once (S6-FRONT-3: charged
    /// after growing, 32,768 nested functions held 1.3 times the limit while the function
    /// stack moved).
    fn stack_room(&mut self) -> CResult<()> {
        let tight = |cap: usize, len: usize| cap - len < 8;
        if !tight(self.tasks.capacity(), self.tasks.len()) && !tight(self.fs.capacity(), self.fs.len()) {
            return Ok(());
        }
        self.stack_room_grow()
    }

    #[cold]
    fn stack_room_grow(&mut self) -> CResult<()> {
        let tight = |cap: usize, len: usize| cap - len < 8;
        let grow = |cap: usize, len: usize| if tight(cap, len) { cap.max(8) } else { 0 };
        let (gt, gf) = (grow(self.tasks.capacity(), self.tasks.len()), grow(self.fs.capacity(), self.fs.len()));
        // While moving: the old buffers, and the new ones of the grown size.
        let moving = self.tasks.capacity() * crate::nostd::mem::size_of::<Task<'a>>() * usize::from(gt > 0)
            + self.fs.capacity() * crate::nostd::mem::size_of::<FuncState>() * usize::from(gf > 0);
        let need = self.stacks_bytes() + gt * crate::nostd::mem::size_of::<Task<'a>>() + gf * crate::nostd::mem::size_of::<FuncState>() + moving;
        if need > self.charged {
            self.charge_more(need)?;
        }
        self.tasks.reserve(gt);
        self.fs.reserve(gf);
        Ok(())
    }

    fn stacks_bytes(&self) -> usize {
        self.tasks.capacity() * crate::nostd::mem::size_of::<Task<'a>>()
            + self.fs.capacity() * crate::nostd::mem::size_of::<FuncState>()
            + self.fold_memo.capacity() * 16
    }

    /// Charges the stacks' growth to the state's memory.
    fn charge_stacks(&mut self) -> CResult<()> {
        let need = self.stacks_bytes();
        if need > self.charged {
            self.charge_more(need)?;
        }
        Ok(())
    }

    /// Charges the growth of the open function's buffers since they were last looked at.
    fn charge_fs(&mut self) -> CResult<()> {
        let Some(fs) = self.fs.last_mut() else { return Ok(()) };
        let now = fs.buffer_bytes();
        if now > fs.inner {
            let more = now - fs.inner;
            if self.heap.mem.charge(more).is_err() {
                return Err(self.err(Pos { line: self.line, col: 1 }, "not enough memory"));
            }
            fs.inner = now;
            self.inner += more;
        }
        Ok(())
    }

    #[cold]
    fn charge_more(&mut self, need: usize) -> CResult<()> {
        if self.heap.mem.charge(need - self.charged).is_err() {
            return Err(self.err(Pos { line: self.line, col: 1 }, "not enough memory"));
        }
        self.charged = need;
        Ok(())
    }

    fn run(&mut self) -> CResult<()> {
        let mut cap = self.tasks.capacity();
        while let Some(t) = self.tasks.pop() {
            self.stack_room()?;
            if crate::diag::compile_tick() {
                return Err(self.err(Pos { line: self.line, col: 1 }, crate::diag::COMPILE_STOPPED));
            }
            self.step(t)?;
            if let Some(e) = self.emit_error.take() {
                return Err(e);
            }
            // A step pushes a few tasks at most: checked here, the stack stays within memory.
            if self.tasks.capacity() != cap {
                cap = self.tasks.capacity();
                self.charge_stacks()?;
            }
            // A step emits a few instructions and adds a local or a constant at most: the
            // open function's buffers are looked at every few steps.
            self.fs_check = self.fs_check.wrapping_sub(1);
            if self.fs_check == 0 || self.fs_check > FS_CHECK_EVERY {
                self.fs_check = FS_CHECK_EVERY;
                self.charge_fs()?;
            }
        }
        Ok(())
    }

    fn step(&mut self, t: Task<'a>) -> CResult<()> {
        match t {
            Task::Block { b, repeat } => self.block_stat(b, 0, repeat),
            Task::ExprToReg { e, dst } => self.expr_to_reg(e, dst),
            Task::ExprNext(e) => self.expr_next(e),
            Task::ExprAnyReg(e) => self.expr_any_reg(e),
            Task::ExprRk(e) => self.expr_rk(e),
            Task::KeyRef(e) => self.key_ref(e),
            Task::CondJump { e, when } => self.cond_jump(e, when),
            Task::Multi { e, n } => self.multi(e, n),
            Task::Function(f) => self.function(f),

            Task::BlockNext { b, i, repeat } => {
                // Temporaries never outlive a statement.
                let lvl = self.fsr().nvarstack();
                self.set_freereg(lvl);
                self.block_stat(b, i + 1, repeat)
            }
            Task::LeaveBlock(end) => self.leave_block(end),
            Task::FunctionEnd(f) => self.function_end(f),
            Task::ExprNextCall(pos) => {
                let base = self.take_reg();
                self.reserve(1, pos)?;
                self.done(Ret::Reg(base))
            }
            Task::CallFinish { e, want, base } => {
                let nargs = self.take_count();
                let c = match want {
                    Want::Fixed(n) => n as Reg,
                    Want::Multi => MULTI,
                };
                self.at(e.call_at());
                self.emit_call(base, nargs, c);
                self.set_freereg(us(base));
                self.done(Ret::Reg(base))
            }
            Task::SetupArgs { args, pos } => self.explist(args, Want::Multi, pos),
            Task::SetupMethod { e, base } => {
                let o = self.take_reg();
                let ExprKind::Method(_, name, npos, args) = &e.kind else { return Err(self.err(e.pos, "internal: not a call")) };
                self.set_freereg(us(base));
                self.reserve(2, e.pos)?;
                let k = self.str_const(name.as_bytes(), e.pos)?;
                self.at(*npos);
                match Self::k16(k) {
                    Some(k) => {
                        self.emit(Instr::SelfK { a: base, b: o, k });
                    }
                    None => {
                        self.emit(Instr::Move { a: base + 1, b: o });
                        let kr = self.alloc_reg(e.pos)?;
                        self.emit(Instr::LoadK { a: kr, k });
                        self.emit(Instr::GetTable { a: base, b: base + 1, c: kr });
                        self.set_freereg(us(base) + 2);
                    }
                }
                self.push(Task::SetupMethodArgs);
                self.explist(args, Want::Multi, e.pos)
            }
            Task::SetupMethodArgs => {
                let n = self.take_count();
                self.done(Ret::Count(if n == MULTI { MULTI } else { n + 1 }))
            }
            Task::MultiFinish { e, n, base } => {
                let nargs = self.take_count();
                self.at(e.call_at());
                // The results' registers count before the call is emitted: past one window,
                // it is placed by them.
                if n != MULTI {
                    self.need_stack(us(base) + us(n));
                }
                self.emit_call(base, nargs, n);
                self.set_freereg(us(base));
                self.done(Ret::None)
            }
            Task::ExplistNext(st) => self.explist_next(ExplistSt { i: st.i + 1, ..st }),
            Task::ExplistAfterMulti { st, needed } => {
                self.set_freereg(us(self.freereg()) + needed);
                self.need_stack(us(self.freereg()));
                self.explist_next(ExplistSt { i: st.i + 1, ..st })
            }
            Task::ExplistRetMulti => self.done(Ret::Count(MULTI)),
            Task::IndexObj { key, dst, pos, save } => {
                let t = self.take_reg();
                self.then(Task::IndexKey { t, dst, pos, save }, Task::KeyRef(key))
            }
            Task::IndexKey { t, dst, pos, save } => {
                let k = self.take_key();
                self.at(pos);
                self.load_place(&Place::Index(t, k), dst)?;
                self.restore(save, dst)
            }
            Task::CallToReg { dst, save } => {
                let base = self.take_reg();
                self.move_to(dst, base);
                self.restore(save, dst)
            }
            Task::FuncToReg { dst, pos, save } => {
                let p = self.take_proto();
                self.at(pos);
                self.emit(Instr::Closure { a: dst, p });
                self.restore(save, dst)
            }
            Task::ConcatNext { parts, i, base, dst, pos, save } => {
                if let Some(p) = parts.get(i) {
                    return self.then(Task::ConcatNext { parts, i: i + 1, base, dst, pos, save }, Task::ExprNext(p));
                }
                self.at(pos);
                if !self.merge_concat(base, parts.len()) {
                    self.emit_concat(base, parts.len());
                }
                self.move_to(dst, base);
                self.restore(save, dst)
            }
            Task::InterpExpr { parts, i, base, dst, pos, save } => {
                let r = self.take_reg();
                if let Some(InterpExpr::Expr(e)) = parts.get(i) {
                    self.at(e.pos);
                }
                self.emit(Instr::ToStr { a: r, b: r });
                self.interp_next(parts, i + 1, base, dst, pos, save)
            }
            Task::UnTo { op, dst, pos, save } => {
                let r = self.take_reg();
                self.at(pos);
                self.emit(match op {
                    UnOp::Neg => Instr::Unm { a: dst, b: r },
                    UnOp::Not => Instr::Not { a: dst, b: r },
                    UnOp::Len => Instr::Len { a: dst, b: r },
                    UnOp::BNot => Instr::BNot { a: dst, b: r },
                });
                self.restore(save, dst)
            }
            Task::TablePos(mut st) => {
                st.pending += 1;
                if st.pending == LIST_FLUSH {
                    self.at(st.pos);
                    self.emit(Instr::SetList { a: st.t, n: st.pending as u8, off: st.stored as u32 });
                    st.stored += st.pending;
                    st.pending = 0;
                    self.set_freereg(us(st.t) + 1);
                }
                self.table_next(TableSt { i: st.i + 1, ..st })
            }
            Task::TableMulti(mut st) => {
                self.at(st.pos);
                self.emit(Instr::SetList { a: st.t, n: SETLIST_MULTI, off: st.stored as u32 });
                st.pending = 0;
                self.set_freereg(us(st.t) + 1);
                self.table_next(TableSt { i: st.i + 1, ..st })
            }
            Task::TableKey { st, save } => {
                let kr = self.take_key();
                let Some(Field::Keyed(_, v)) = st.fields.get(st.i) else { return Ok(()) };
                self.then(Task::TableKeyVal { st, save, kr }, Task::ExprRk(v))
            }
            Task::TableKeyVal { st, save, kr } => {
                let val = self.take_rk();
                if let Some(Field::Named(_, v, pos) | Field::Keyed(Expr { pos, .. }, v)) = st.fields.get(st.i) {
                    self.at(*pos);
                    self.at(field_store_pos(v));
                }
                self.store(&Place::Index(st.t, kr), val)?;
                self.set_freereg(us(save));
                self.table_next(TableSt { i: st.i + 1, ..st })
            }
            Task::LogicL { op, r, target, dst, pos, save } => {
                self.at(pos);
                // and: jump to the end when false; or: when true.
                self.emit(Instr::Test { a: target, k: op == BinOp::Or });
                let j = self.jump();
                self.then(Task::LogicR { j, target, dst, save }, Task::ExprToReg { e: r, dst: target })
            }
            Task::LogicR { j, target, dst, save } => {
                self.patch_here(&[j]);
                self.move_to(dst, target);
                self.restore(save, dst)
            }
            Task::CmpToReg { dst, save } => {
                let jumps = self.take_jumps();
                if us(dst) + 1 > WIN {
                    // Past a window: no skip over the windowed load (it is two instructions).
                    self.emit(Instr::LoadBool { a: dst, v: false });
                    let j = self.jump();
                    self.patch_here(&jumps);
                    self.emit(Instr::LoadBool { a: dst, v: true });
                    self.patch_here(&[j]);
                    return self.restore(save, dst);
                }
                self.emit(Instr::LoadFalseSkip { a: dst });
                self.patch_here(&jumps);
                self.emit(Instr::LoadBool { a: dst, v: true });
                self.restore(save, dst)
            }
            Task::ConcatL { r, base, dst, pos, save } => self.then(Task::ConcatR { base, dst, pos, save }, Task::ExprNext(r)),
            Task::ConcatR { base, dst, pos, save } => {
                self.at(pos);
                if !self.merge_concat(base, 2) {
                    self.emit(Instr::Concat { a: base, n: 2 });
                }
                self.move_to(dst, base);
                self.restore(save, dst)
            }
            Task::ArithL { op, r, dst, pos, save } => {
                let b = self.take_rk();
                self.seq(Task::ArithR { op, b, dst, pos, save }, |c| c.start(Task::ExprRk(r)))
            }
            Task::ArithR { op, b, dst, pos, save } => {
                let c = self.take_rk();
                self.at(pos);
                let aop = arith_op(op).ok_or_else(|| self.err(pos, "internal: operator"))?;
                self.emit(Instr::Arith { op: aop, a: dst, b, c });
                self.restore(save, dst)
            }
            Task::SpineFirst { spine, acc, dst, save } => {
                let b = self.take_rk();
                self.spine_next(spine, acc, dst, save, b)
            }
            Task::SpineNext { spine, acc, dst, save, b, op, pos, save2 } => {
                let c = self.take_rk();
                self.at(pos);
                let aop = arith_op(op).ok_or_else(|| self.err(pos, "internal: operator"))?;
                self.emit(Instr::Arith { op: aop, a: acc, b, c });
                self.set_freereg(us(save2).max(us(acc) + 1));
                self.spine_next(spine, acc, dst, save, Rk::reg(acc))
            }
            Task::CondSkip { r, when } => {
                let skip = self.take_jumps();
                self.then(Task::CondSkipEnd(skip), Task::CondJump { e: r, when })
            }
            Task::CondSkipEnd(skip) => {
                let j = self.take_jumps();
                self.patch_here(&skip);
                self.done(Ret::Jumps(j))
            }
            Task::CondBoth { r, when } => {
                let j = self.take_jumps();
                self.then(Task::CondBothEnd(j), Task::CondJump { e: r, when })
            }
            Task::CondBothEnd(mut j) => {
                j.extend(self.take_jumps());
                self.done(Ret::Jumps(j))
            }
            Task::CmpL { op, r, pos, when, save } => {
                let a = self.take_rk();
                self.seq(Task::CmpR { op, a, pos, when, save }, |c| c.start(Task::ExprRk(r)))
            }
            Task::CmpR { op, a, pos, when, save } => {
                let b = self.take_rk();
                self.at(pos);
                let i = match op {
                    BinOp::Eq => Instr::Eq { b: a, c: b, k: when },
                    BinOp::Ne => Instr::Eq { b: a, c: b, k: !when },
                    BinOp::Lt => Instr::Lt { b: a, c: b, k: when },
                    BinOp::Le => Instr::Le { b: a, c: b, k: when },
                    BinOp::Gt => Instr::Lt { b, c: a, k: when },
                    _ => Instr::Le { b, c: a, k: when },
                };
                self.emit(i);
                let j = self.jump();
                self.set_freereg(us(save));
                self.done(Ret::Jumps(vec![j]))
            }
            Task::TestJump { pos, when, save } => {
                let r = self.take_reg();
                self.at(pos);
                self.emit(Instr::Test { a: r, k: when });
                let j = self.jump();
                self.set_freereg(us(save));
                self.done(Ret::Jumps(vec![j]))
            }
            Task::PlaceObj(key) => {
                let r = self.take_reg();
                self.then(Task::PlaceKey(r), Task::KeyRef(key))
            }
            Task::PlaceKey(r) => {
                let k = self.take_key();
                self.done(Ret::Place(Place::Index(r, k)))
            }

            // ---- statements ----
            Task::LocalAfter { s, base, konst } => self.local_after(s, base, konst),
            Task::AssignPlace { s, mut places } => {
                places.push(self.take_place());
                self.check_conflict(&mut places, s.pos)?;
                self.assign_places(s, places)
            }
            Task::AssignOne { p, pos } => {
                let v = self.take_rk();
                self.at(pos);
                self.store(&p, v)
            }
            Task::AssignMulti { places, base, pos } => {
                self.at(pos);
                for (i, p) in places.iter().enumerate().rev() {
                    self.store(p, Rk::reg(base + i as Reg))?;
                }
                Ok(())
            }
            Task::OpAssignPlace(s) => self.op_assign_place(s),
            Task::OpConcatEnd { p, a, dst, base, pos } => {
                self.at(pos);
                // `x ..= e` is `x = x .. e`: a concatenation `e` ends with is merged into
                // this one, keeping its line, as C Lua's `codeconcat` merges it (LR-6: a
                // runtime error was numbered with the statement's first line).
                if !self.merge_concat(a, 2) {
                    self.emit(Instr::Concat { a, n: 2 });
                }
                self.emit(Instr::Move { a: dst, b: a });
                self.set_freereg(us(base));
                if !matches!(p, Place::Local(_)) {
                    self.store(&p, Rk::reg(dst))?;
                }
                Ok(())
            }
            Task::OpArithEnd { p, cur, op, pos } => {
                let v = self.take_rk();
                self.at(pos);
                let aop = arith_op(op).ok_or_else(|| self.err(pos, "bad compound operator"))?;
                self.emit(Instr::Arith { op: aop, a: cur, b: Rk::reg(cur), c: v });
                if !matches!(p, Place::Local(_)) {
                    self.store(&p, Rk::reg(cur))?;
                }
                Ok(())
            }
            Task::WhileCond { body, start } => {
                let exits = self.take_jumps();
                self.enter_block(false);
                self.then(Task::WhileBody { body, start, exits }, Task::Block { b: body, repeat: false })
            }
            Task::WhileBody { body, start, exits } => {
                self.leave_block(body.end)?;
                let first_goto = self.fsr().blocks.last().map_or(0, |b| b.first_goto);
                self.create_label(CONTINUE, body.end.line, None, first_goto, None)?;
                self.at(body.end);
                self.jump_back(start);
                self.patch_here(&exits);
                self.leave_block(body.end)
            }
            Task::RepeatBody { body, cond, start } => {
                let first_goto = self.fsr().blocks.iter().rev().nth(1).map_or(0, |b| b.first_goto);
                self.create_label(CONTINUE, body.end.line, None, first_goto, None)?;
                self.at(cond.pos);
                self.seq(Task::RepeatCond { body, start }, |c| c.cond_jump(cond, false))
            }
            Task::RepeatCond { body, start } => {
                let back = self.take_jumps();
                let has_upval = self.fsr().blocks.last().is_some_and(|b| b.has_upval);
                let nact = self.fsr().blocks.last().map_or(0, |b| b.nactvar);
                let level = self.fsr().reglevel(nact) as Reg;
                for j in back {
                    self.patch_jump(j, start, has_upval.then_some(level));
                }
                self.leave_block(body.end)?;
                self.leave_block(body.end)
            }
            Task::IfCond { s, i, to_end } => {
                let skip = self.take_jumps();
                let StatKind::If(conds, _) = &s.kind else { return Ok(()) };
                let Some((_, block)) = conds.get(i) else { return Ok(()) };
                self.enter_block(false);
                self.then(Task::IfBlock { s, i, to_end, skip }, Task::Block { b: block, repeat: false })
            }
            Task::IfBlock { s, i, mut to_end, skip } => {
                let StatKind::If(conds, els) = &s.kind else { return Ok(()) };
                let Some((_, block)) = conds.get(i) else { return Ok(()) };
                self.leave_block(block.end)?;
                let last = i + 1 == conds.len() && els.is_none();
                if !last && !block_ends_in_jump(block) {
                    to_end.push(self.jump());
                }
                self.patch_here(&skip);
                self.if_next(s, i + 1, to_end)
            }
            Task::IfElseEnd { end, to_end } => {
                self.leave_block(end)?;
                self.patch_here(&to_end);
                Ok(())
            }
            Task::NumForStart { s, base } => {
                let StatKind::NumFor(f) = &s.kind else { return Ok(()) };
                self.then(Task::NumForLimit { s, base }, Task::ExprNext(&f.limit))
            }
            Task::NumForLimit { s, base } => {
                let StatKind::NumFor(f) = &s.kind else { return Ok(()) };
                match &f.step {
                    Some(e) => self.then(Task::NumForStep { s, base }, Task::ExprNext(e)),
                    None => {
                        let r = self.alloc_reg(s.pos)?;
                        self.emit(Instr::LoadInt { a: r, i: 1 });
                        self.num_for_body(s, base)
                    }
                }
            }
            Task::NumForStep { s, base } => self.num_for_body(s, base),
            Task::NumForEnd { s, base, prep, body_start } => {
                let StatKind::NumFor(f) = &s.kind else { return Ok(()) };
                let body = &f.body;
                self.leave_block(body.end)?;
                let first_goto = self.fsr().blocks.last().map_or(0, |b| b.first_goto);
                self.create_label(CONTINUE, body.end.line, None, first_goto, None)?;
                self.at(s.pos);
                // (The jump counts from the instruction itself, after any window.)
                let lp = self.emit(Instr::ForLoop { a: base, off: 0 });
                self.set_off(lp, (body_start as i64 - lp as i64 - 1) as i32);
                let after = self.pc();
                self.set_off(prep, (after as i64 - prep as i64 - 1) as i32);
                self.leave_block(body.end)
            }
            Task::GenForExprs { s, base } => self.gen_for_body(s, base),
            Task::GenForEnd { s, base, prep, body_start } => {
                let StatKind::GenFor { names, body, line, .. } = &s.kind else { return Ok(()) };
                self.leave_block(body.end)?;
                let first_goto = self.fsr().blocks.last().map_or(0, |b| b.first_goto);
                self.create_label(CONTINUE, body.end.line, None, first_goto, None)?;
                self.at(Pos { line: *line, col: 1 });
                // (Jumped to where its code starts: a window comes first.)
                let call = self.pc();
                let wanted = names.len() as Reg;
                if wanted > MAX_COUNT {
                    self.emit(Instr::TForCall { a: base, c: cnt(MULTI) });
                    let line = self.line;
                    self.pad_nil(base + 4 + wanted, line);
                } else {
                    self.emit(Instr::TForCall { a: base, c: cnt(wanted) });
                }
                self.set_off(prep, (call as i64 - prep as i64 - 1) as i32);
                self.need_stack(us(base) + 4 + names.len());
                let lp = self.emit(Instr::TForLoop { a: base, off: 0 });
                self.set_off(lp, (body_start as i64 - lp as i64 - 1) as i32);
                self.leave_block(body.end)
            }
            Task::Fail(d) => Err(*d),
            Task::Closure { r, pos, place } => {
                let p = self.take_proto();
                self.at(pos);
                self.emit(Instr::Closure { a: r, p });
                if let Some(place) = place {
                    self.store(&place, Rk::reg(r))?;
                }
                Ok(())
            }
            Task::RetTail { base, pos } => {
                let nargs = self.take_count();
                self.at(pos);
                let b = self.count_in(base, nargs, 1);
                self.emit(Instr::TailCall { a: base, b });
                // Reached when the callee is not a Lua function (it ran as a plain call).
                self.emit(Instr::Return { a: base, b: cnt(MULTI) });
                Ok(())
            }
            Task::RetOne(pos) => {
                let r = self.take_reg();
                self.at(pos);
                self.emit(Instr::Return { a: r, b: 1 });
                Ok(())
            }
            Task::RetMulti { base, pos } => {
                let n = self.take_count();
                self.at(pos);
                let b = self.count_in(base, n, 0);
                self.emit(Instr::Return { a: base, b });
                Ok(())
            }
        }
    }

    fn take_proto(&mut self) -> u32 {
        match self.take() {
            Ret::Proto(p) => p,
            _ => 0,
        }
    }

    /// The end of an expression compiled into register `dst`: its temporaries go, and
    /// `dst` is handed back (for `expr_next`).
    fn restore(&mut self, save: Reg, dst: Reg) -> CResult<()> {
        self.set_freereg(us(save));
        self.done(Ret::Reg(dst))
    }

    // ---- registers ----

    fn alloc_reg(&mut self, pos: Pos) -> CResult<Reg> {
        self.reserve(1, pos)?;
        Ok((self.fsr().freereg - 1) as Reg)
    }

    fn reserve(&mut self, n: usize, _pos: Pos) -> CResult<()> {
        // No bound but memory (the stack is charged as it grows when the code runs).
        self.fs().freereg += n;
        self.need_stack(self.fsr().freereg);
        Ok(())
    }

    /// The frame holds at least `n` registers.
    fn need_stack(&mut self, n: usize) {
        let fs = self.fs();
        fs.max_stack = fs.max_stack.max(n);
    }

    fn freereg(&self) -> Reg {
        self.fsr().freereg as Reg
    }

    fn set_freereg(&mut self, r: usize) {
        self.fs().freereg = r;
    }

    // ---- constants ----

    fn konst(&mut self, key: ConstKey, v: Value, pos: Pos) -> CResult<u32> {
        if let Some(&i) = self.fsr().const_map.get(&key) {
            return Ok(i);
        }
        let fs = self.fs();
        let i = fs.consts.len() as u32;
        if i >= (1 << 24) {
            return Err(self.err(pos, "function has too many constants"));
        }
        let fs = self.fs();
        grow_list(&mut fs.consts);
        fs.consts.push(v);
        fs.const_map.insert(key, i);
        Ok(i)
    }

    fn str_const(&mut self, s: &[u8], pos: Pos) -> CResult<u32> {
        let long = s.len() > crate::stdlib::COPY_PIECE;
        // Looked up by the bytes themselves: no key to build when it is there already.
        if !long && let Some(&i) = self.fsr().str_map.get(s) {
            return Ok(i);
        }
        let r = if long { self.long_const(s, pos)? } else { self.heap.intern(s).map_err(|()| self.err(pos, "not enough memory"))? };
        let fs = self.fs();
        let i = fs.consts.len() as u32;
        if i >= (1 << 24) {
            return Err(self.err(pos, "function has too many constants"));
        }
        let fs = self.fs();
        grow_list(&mut fs.consts);
        fs.consts.push(Value::Str(r));
        if !long {
            fs.str_map.insert(s.to_vec(), i);
        }
        Ok(i)
    }

    /// A constant longer than `COPY_PIECE`, as `Lua::settle_key` settles a long key: made,
    /// hashed and compared with the one interned string it can equal a piece at a time,
    /// each a look at the host's interrupt and deadline, then interned without comparing
    /// again (16 MiB was hashed, compared and copied for `str_map` in one go, 15 ms with no
    /// look). Not in `str_map`: an equal one in the same function is another constant.
    fn long_const(&mut self, s: &[u8], pos: Pos) -> CResult<crate::value::StrRef> {
        let r = self.heap.new_str(s).map_err(|()| self.err(pos, "not enough memory"))?;
        let stopped = |c: &Self| c.err(pos, crate::diag::COMPILE_STOPPED);
        let mut run = None;
        let hash = loop {
            if let Some(h) = self.heap.hash_piece(self.heap.str_bytes(r), &mut run) {
                break h;
            }
            if crate::diag::compile_ticks(crate::diag::COMPILE_WATCH_EVERY) {
                return Err(stopped(self));
            }
        };
        if let Some(x) = self.heap.strings.get(r.idx) {
            x.hash.set(hash);
        }
        let Some(c) = self.heap.key_best(r) else { return Ok(self.heap.canon_absent(r, None)) };
        let mut at = 0;
        while at < s.len() {
            let end = at.saturating_add(crate::stdlib::COPY_PIECE).min(s.len());
            if let Some(d) = self.heap.diff_piece(r.idx, c, at, end) {
                return Ok(self.heap.canon_absent(r, Some((c, d))));
            }
            at = end;
            if crate::diag::compile_ticks(crate::diag::COMPILE_WATCH_EVERY) {
                return Err(stopped(self));
            }
        }
        Ok(self.heap.keep(c))
    }

    fn num_const(&mut self, v: Value, pos: Pos) -> CResult<u32> {
        match v {
            Value::Int(i) => self.konst(ConstKey::Int(i), v, pos),
            Value::Num(f) => self.konst(ConstKey::Num(f.to_bits()), v, pos),
            Value::Bool(b) => self.konst(ConstKey::Bool(b), v, pos),
            Value::Nil => self.konst(ConstKey::Nil, v, pos),
            _ => self.konst(ConstKey::Nil, Value::Nil, pos),
        }
    }

    /// A constant index usable in a 16-bit field, or None.
    fn k16(k: u32) -> Option<u16> {
        u16::try_from(k).ok().filter(|&k| k < MRk::CONST)
    }

    // ---- functions ----

    fn main(&mut self, block: &'a Block) -> CResult<u32> {
        let mut fs = FuncState::new(0);
        fs.is_vararg = true;
        fs.upvals.push(UpvalDesc { in_stack: true, idx: 0, name: Arc::from("_ENV") });
        self.scope_add(&Arc::from("_ENV"));
        self.fs.push(fs);
        self.charge_stacks()?;
        self.enter_block(false);
        self.push(Task::Block { b: block, repeat: false });
        self.run()?;
        self.line = block.end.line.max(self.line);
        self.leave_block(block.end)?;
        // Closing the chunk's block can be where the stop is found (S5C-FRONT-12).
        if crate::diag::compile_stopped() {
            return Err(self.err(Pos { line: self.line, col: 1 }, crate::diag::COMPILE_STOPPED));
        }
        let line = block.end.line;
        self.line = line;
        self.emit(Instr::Return { a: 0, b: 0 });
        if let Some(e) = self.emit_error.take() {
            return Err(e);
        }
        let fs = self.pop_fs().ok_or_else(|| self.err(block.end, "internal: no function"))?;
        if let Some(g) = fs.gotos.first() {
            let at = if self.prefix.is_some() { crate::parser::CUT_LINE } else { block.end.line };
            return Err(self.undefined_goto(g, at));
        }
        self.finish(fs, block.end, 0)
    }

    /// A goto left pending when its function ends (at line `at`, where C Lua reports it).
    fn undefined_goto(&self, g: &GotoDesc, at: u32) -> Diagnostic {
        let pos = Pos { line: at, col: 1 };
        if g.name == BREAK {
            self.err(pos, format!("break outside loop at line {}", g.line))
        } else if g.name == CONTINUE {
            self.err(pos, format!("continue outside loop at line {}", g.line))
        } else {
            self.err(pos, format!("no visible label '{}' for <goto> at line {}", g.name, g.line))
        }
    }

    /// Makes the prototype, in the heap (charged to the memory account).
    fn finish(&mut self, mut fs: FuncState, pos: Pos, last_line: u32) -> CResult<u32> {
        let ncode = fs.code.len();
        // The prototype is charged what its vectors hold (`mem_size`): no spare room in
        // them (S6-FRONT-3: up to half of a long function's code was spare, uncharged).
        // A big function's constant maps (a key each) are freed later from the host
        // (S6-FRONT-4: 600,000 string constants took 20 ms to free as the compile ended).
        release_map(crate::nostd::mem::take(&mut fs.str_map));
        release_map(crate::nostd::mem::take(&mut fs.const_map));
        // So are its lists of active locals when big (a chunk of 800,000 locals: 20 ms at
        // once); small ones go now (held, thousands of nested functions' would add up).
        release_big(crate::nostd::mem::take(&mut fs.actives));
        release_big(crate::nostd::mem::take(&mut fs.captured));
        fs.code.shrink_to_fit();
        fs.lines.shrink_to_fit();
        fs.consts.shrink_to_fit();
        fs.protos.shrink_to_fit();
        fs.upvals.shrink_to_fit();
        fs.locvars.shrink_to_fit();
        let mut p = Proto {
            code: fs.code,
            lines: fs.lines,
            consts: fs.consts,
            protos: fs.protos,
            upvals: fs.upvals,
            locvars: fs.locvars,
            num_params: fs.num_params,
            is_vararg: fs.is_vararg,
            max_stack: fs.max_stack.min(WIN) as u16,
            frame: if fs.max_stack > WIN { fs.max_stack.max(fs.frame) as u32 } else { 0 },
            source: Arc::clone(&self.chunk),
            line_defined: fs.line_defined,
            last_line_defined: last_line,
            chunk_name: Value::Nil,
            hints: vec![crate::nostd::cell::Cell::new(0); ncode],
            upval_writes: 0,
        };
        crate::bytecode::settle_upvals(&mut p, &mut self.heap.protos);
        self.heap.mem.charge(p.mem_size()).map_err(|()| self.err(pos, "not enough memory"))?;
        Ok(self.heap.new_proto_charged(p))
    }

    fn function(&mut self, f: &'a FuncBody) -> CResult<()> {
        // What the enclosing function holds is charged before it waits for this one.
        self.charge_fs()?;
        let mut fs = FuncState::new(f.pos.line);
        fs.is_vararg = f.vararg;
        self.fs.push(fs);
        self.charge_stacks()?;
        self.enter_block(false);
        let mut params: Vec<(String, Pos)> = Vec::new();
        if f.is_method {
            params.push(("self".into(), f.pos));
        }
        params.extend(f.params.iter().map(|p| (p.name.clone(), p.pos)));
        if Reg::try_from(params.len()).is_err() {
            return Err(self.err(f.pos, "too many parameters"));
        }
        for (name, pos) in &params {
            let r = self.alloc_reg(*pos)?;
            self.add_local(name, VarKind::Reg(r), Attrib::None);
        }
        self.fs().num_params = params.len() as Reg;
        self.then(Task::FunctionEnd(f), Task::Block { b: &f.body, repeat: false })
    }

    fn function_end(&mut self, f: &'a FuncBody) -> CResult<()> {
        // The function's closing happens at its `end` (as C Lua's return there).
        self.line = f.end_line;
        self.leave_block(f.body.end)?;
        self.emit(Instr::Return { a: 0, b: 0 });
        let fs = self.pop_fs().ok_or_else(|| self.err(f.pos, "internal: no function"))?;
        if let Some(g) = fs.gotos.first() {
            return Err(self.undefined_goto(g, f.close_line));
        }
        let p = self.finish(fs, f.pos, f.end_line)?;
        let parent = self.fs();
        parent.protos.push(p);
        let idx = (parent.protos.len() - 1) as u32;
        self.done(Ret::Proto(idx))
    }

    // ---- scopes ----

    fn scope_add(&mut self, name: &Arc<str>) {
        *self.in_scope.or_insert(Arc::clone(name), 0) += 1;
    }

    fn scope_remove(in_scope: &mut crate::shardmap::ShardMap<Arc<str>, u32>, name: &str) {
        if let Some(n) = in_scope.get_mut(name) {
            *n -= 1;
            if *n == 0 {
                in_scope.remove(name);
            }
        }
    }

    /// Closes the innermost function: its names leave scope.
    fn pop_fs(&mut self) -> Option<FuncState> {
        let fs = self.fs.pop()?;
        // Its buffers go to the prototype, which is charged for them (`finish`).
        self.heap.mem.release(fs.inner);
        self.heap.mem.debt = self.heap.mem.debt.saturating_sub(fs.inner as isize);
        self.inner -= fs.inner;
        for v in &fs.actives {
            Self::scope_remove(&mut self.in_scope, &v.name);
        }
        for u in &fs.upvals {
            Self::scope_remove(&mut self.in_scope, &u.name);
        }
        Some(fs)
    }

    fn add_local(&mut self, name: &str, kind: VarKind, attrib: Attrib) {
        // A step per local (S5C-FRONT-12): one statement can declare any number, so a task
        // alone is no measure of time between looks at the host's interrupt (a stop found
        // here is kept; the run loop reports it).
        crate::diag::compile_tick();
        let pc = self.pc() as u32;
        let name: Arc<str> = Arc::from(name);
        self.scope_add(&name);
        let fs = self.fs();
        // A function of a million locals: its lists grow between looks at the interrupt
        // (each doubling took 10 to 40 ms in one `realloc`; a stop found here is kept).
        grow_list(&mut fs.locvars);
        grow_list(&mut fs.actives);
        grow_list(&mut fs.captured);
        let locvar = fs.locvars.len();
        if let VarKind::Reg(r) = kind {
            fs.locvars.push(LocVar { name: Arc::clone(&name), reg: cnt(r), start: pc, end: u32::MAX });
        }
        fs.actives.push(ActiveVar { name, kind, attrib, locvar });
        fs.captured.push(false);
    }

    fn remove_locals_to(&mut self, n: usize) {
        let pc = self.pc() as u32;
        let Some(fs) = self.fs.last_mut() else { return };
        while fs.actives.len() > n {
            // A stopped compile ends in an error, and its scope map is freed later: the rest
            // of a run of 300,000 locals is not worth walking (S5C-FRONT-12).
            if crate::diag::compile_tick() || crate::diag::compile_stopped() {
                fs.actives.truncate(n);
                fs.captured.truncate(n);
                break;
            }
            if let Some(v) = fs.actives.pop() {
                Self::scope_remove(&mut self.in_scope, &v.name);
                fs.captured.pop();
                if matches!(v.kind, VarKind::Reg(_))
                    && let Some(lv) = fs.locvars.get_mut(v.locvar)
                {
                    lv.end = pc;
                }
                if v.attrib == Attrib::Close {
                    fs.tbc_active = fs.tbc_active.saturating_sub(1);
                }
            }
        }
    }

    fn enter_block(&mut self, is_loop: bool) {
        let fs = self.fs();
        fs.blocks.push(BlockScope {
            nactvar: fs.actives.len(),
            first_label: fs.labels.len(),
            first_goto: fs.gotos.len(),
            is_loop,
            has_upval: false,
        });
    }

    fn leave_block(&mut self, end: Pos) -> CResult<()> {
        let Some(bl) = self.fs().blocks.pop() else { return Ok(()) };
        let level = self.fsr().reglevel(bl.nactvar);
        let has_locals = self.fsr().actives.len() > bl.nactvar;
        // A loop's break label sits right after it, and resolves only its own breaks.
        if bl.is_loop {
            self.create_label(BREAK, end.line, Some(bl.nactvar), bl.first_goto, None)?;
        }
        if bl.has_upval && has_locals {
            self.emit(Instr::Close { a: level as Reg });
        }
        self.remove_locals_to(bl.nactvar);
        self.set_freereg(level);
        let fs = self.fs();
        fs.labels.truncate(bl.first_label);
        // Pending gotos leave the block: they no longer see its locals, and must close its
        // upvalues when taken.
        crate::diag::compile_work(fs.gotos.len().saturating_sub(bl.first_goto));
        for g in fs.gotos.iter_mut().skip(bl.first_goto) {
            if g.nactvar > bl.nactvar {
                if bl.has_upval {
                    g.close = Some(g.close.map_or(level as Reg, |c| c.min(level as Reg)));
                }
                g.nactvar = bl.nactvar;
            }
        }
        Ok(())
    }

    /// Defines a label at the current position and resolves pending gotos to it (those
    /// from `first_goto` on). `at_end`: the label ends its block, so it is outside the
    /// scope of the block's locals (number of locals then).
    /// A label statement. C Lua checks it (and resolves the gotos waiting for it) after
    /// reading the labels and `;` that follow it, so its errors give the line of the next
    /// token: `report`.
    fn label_stat(&mut self, name: &str, pos: Pos, at_end: bool, report: u32) -> CResult<()> {
        let dup = self.fsr().labels.iter().position(|l| l.name == name);
        crate::diag::compile_work(dup.map_or(self.fsr().labels.len(), |i| i + 1));
        if let Some(l) = dup.and_then(|i| self.fsr().labels.get(i)) {
            let line = l.line;
            return Err(self.err(pos, format!("label '{name}' already defined on line {line}")).lua_line(report));
        }
        let bl_nact = self.fsr().blocks.last().map_or(0, |b| b.nactvar);
        let first_goto = self.fsr().blocks.last().map_or(0, |b| b.first_goto);
        self.create_label(name, pos.line, at_end.then_some(bl_nact), first_goto, Some(report))
    }

    /// The labels from `i` on in `b` (a run of them; `i` is the first): checked last to
    /// first, as C Lua's `labelstat` reads the following ones before its own check. The
    /// index after the run.
    fn label_run(&mut self, b: &Block, i: usize, repeat: bool) -> CResult<usize> {
        let mut j = i;
        while b.stats.get(j).is_some_and(|s| matches!(s.kind, StatKind::Label(_))) {
            j += 1;
        }
        let report = b.stats.get(j).map_or(b.end.line, |s| s.pos.line);
        // A block closed at the cut really goes on with the bad token, unless it ends one:
        // then the label is last even in a repeat body (C Lua's `block_follow` without
        // `until`: `repeat ... ::l:: end` checks for `until` only after the label).
        let at_end = j == b.stats.len()
            && match self.prefix {
                Some((cut, follow)) if cut == b.end => follow,
                _ => !repeat,
            };
        for k in (i..j).rev() {
            if let Some(Stat { kind: StatKind::Label(name), pos }) = b.stats.get(k) {
                self.label_stat(name, *pos, at_end, report)?;
            }
        }
        Ok(j)
    }

    fn create_label(&mut self, name: &str, line: u32, at_end: Option<usize>, first_goto: usize, report: Option<u32>) -> CResult<()> {
        let pc = self.pc();
        let nactvar = at_end.unwrap_or(self.fsr().actives.len());
        // Gotos that close from past what a jump's 16-bit field holds: one `Close` at the
        // label does it for them (a level no local the label sees is at, so it closes
        // nothing for code that reaches the label otherwise), and their jumps close nothing.
        let far = self
            .fsr()
            .gotos
            .iter()
            .skip(first_goto)
            .filter(|g| g.name == name)
            .filter_map(|g| g.close.filter(|&c| !close_fits(c)))
            .min();
        if let Some(c) = far {
            self.at(Pos { line, col: 1 });
            self.emit(Instr::Close { a: c });
            for g in self.fs().gotos.iter_mut().skip(first_goto) {
                if g.name == name && g.close.is_some_and(|c| !close_fits(c)) {
                    g.close = None;
                }
            }
        }
        let mut i = first_goto;
        crate::diag::compile_work(self.fsr().gotos.len().saturating_sub(first_goto));
        while i < self.fsr().gotos.len() {
            let matches = self.fsr().gotos.get(i).is_some_and(|g| g.name == name);
            if !matches {
                i += 1;
                continue;
            }
            crate::diag::compile_work(self.fsr().gotos.len() - i);
            let g = self.fs().gotos.remove(i);
            if g.nactvar < nactvar {
                let var = self.fsr().actives.get(g.nactvar).map(|v| v.name.to_string()).unwrap_or_default();
                let what = if name == CONTINUE { "continue".to_owned() } else { format!("goto {name}") };
                return Err(self.err(
                    Pos { line: g.line, col: 1 },
                    format!("<{what}> at line {} jumps into the scope of local '{var}'", g.line),
                )
                .lua_line(report.unwrap_or(g.line)));
            }
            self.patch_jump(g.pc, pc, g.close);
        }
        self.fs().labels.push(LabelDesc { name: name.to_owned(), pc, nactvar, line });
        Ok(())
    }

    fn patch_jump(&mut self, jmp: usize, target: usize, close: Option<Reg>) {
        let fs = self.fs();
        fs.last_target = fs.last_target.max(target);
        let off = target as i64 - jmp as i64 - 1;
        if let Some(MInstr::Jmp { close: old, .. }) = self.instr(jmp) {
            let close = match (close, old) {
                (Some(c), 0) => close_field(c),
                (Some(c), o) => close_field(c).min(o),
                (None, o) => o,
            };
            self.set_instr(jmp, MInstr::Jmp { off: off as i32, close });
        }
    }

    fn jump(&mut self) -> usize {
        self.emit(Instr::Jmp { off: 0, close: 0 })
    }

    fn patch_here(&mut self, jumps: &[usize]) {
        let here = self.pc();
        for &j in jumps {
            self.patch_jump(j, here, None);
        }
    }

    fn jump_back(&mut self, target: usize) {
        let pc = self.pc() as i64;
        self.emit(Instr::Jmp { off: (target as i64 - pc - 1) as i32, close: 0 });
    }

    fn goto(&mut self, name: &str, pos: Pos) -> CResult<()> {
        let line = pos.line;
        let nactvar = self.fsr().actives.len();
        // A visible label: a backward jump, closing what it leaves.
        let at = self.fsr().labels.iter().rposition(|l| l.name == name);
        crate::diag::compile_work(at.map_or(self.fsr().labels.len(), |i| self.fsr().labels.len() - i));
        let found = at.and_then(|i| self.fsr().labels.get(i)).map(|l| (l.pc, l.nactvar));
        if let Some((target, lnact)) = found {
            let level = self.fsr().reglevel(lnact);
            let mut close = if self.fsr().nvarstack() > level { close_field(level as Reg) } else { 0 };
            if close > 0 && !close_fits(level as Reg) {
                // (Past a jump's 16-bit field: a `Close` of its own.)
                self.emit(Instr::Close { a: level as Reg });
                close = 0;
            }
            let pc = self.pc() as i64;
            self.emit(Instr::Jmp { off: (target as i64 - pc - 1) as i32, close });
            return Ok(());
        }
        let pc = self.jump();
        self.fs().gotos.push(GotoDesc { name: name.to_owned(), pc, line, nactvar, close: None });
        Ok(())
    }

    // ---- variables ----

    /// Finds a variable by name: a local of this function, an upvalue, or None (global).
    fn resolve(&mut self, name: &str) -> Option<VarRef> {
        let depth = self.fs.len();
        self.resolve_in(depth - 1, name)
    }

    /// Looks outward from function `level` for the nearest one that has `name`, then
    /// captures it as an upvalue in each function from there back in.
    fn resolve_in(&mut self, level: usize, name: &str) -> Option<VarRef> {
        if !self.in_scope.contains_key(name) {
            return None;
        }
        let mut l = level;
        let mut found = loop {
            let fs = self.fs.get(l)?;
            // Only the names compared count (recent locals are found at once).
            let local = fs.actives.iter().rposition(|v| &*v.name == name);
            crate::diag::compile_work(local.map_or(fs.actives.len(), |i| fs.actives.len() - i));
            if let Some(i) = local {
                let v = fs.actives.get(i)?;
                break match v.kind {
                    VarKind::Reg(r) => VarRef::Local(r, i, v.attrib),
                    VarKind::Const(c) => VarRef::Const(c),
                };
            }
            let up = fs.upvals.iter().position(|u| &*u.name == name);
            crate::diag::compile_work(up.map_or(fs.upvals.len(), |u| u + 1));
            if let Some(u) = up {
                break VarRef::Upval(u as Reg);
            }
            if l == 0 {
                return None;
            }
            l -= 1;
        };
        for lv in l + 1..=level {
            let (in_stack, idx) = match found {
                VarRef::Const(c) => return Some(VarRef::Const(c)),
                VarRef::Local(r, i, _) => {
                    let ofs = self.fs.get_mut(lv - 1)?;
                    if let Some(c) = ofs.captured.get_mut(i) {
                        *c = true;
                    }
                    // Mark the block that declared it as having upvalues.
                    let nb = ofs.blocks.len();
                    let at = ofs.blocks.iter().rposition(|b| b.nactvar <= i);
                    crate::diag::compile_work(at.map_or(nb, |j| nb - j));
                    let decl_block = at.and_then(|j| ofs.blocks.get_mut(j));
                    if let Some(b) = decl_block {
                        b.has_upval = true;
                    }
                    (true, r)
                }
                VarRef::Upval(u) => (false, u),
            };
            if self.fs.get(lv)?.upvals.len() >= us(u16::MAX) {
                return None;
            }
            let name: Arc<str> = Arc::from(name);
            *self.in_scope.or_insert(Arc::clone(&name), 0) += 1;
            let fs = self.fs.get_mut(lv)?;
            fs.upvals.push(UpvalDesc { in_stack, idx, name });
            found = VarRef::Upval((fs.upvals.len() - 1) as Reg);
        }
        Some(found)
    }

    /// Where `_ENV` is.
    fn env(&mut self, pos: Pos) -> CResult<VarRef> {
        match self.resolve("_ENV") {
            Some(v) => Ok(v),
            None => Err(self.err(pos, "too many upvalues")),
        }
    }

    // ---- statements ----

    /// Statement `i` of `b` on (the rest follow as each one finishes).
    fn block_stat(&mut self, b: &'a Block, mut i: usize, repeat: bool) -> CResult<()> {
        let n = b.stats.len();
        loop {
            let Some(s) = b.stats.get(i) else { return Ok(()) };
            // S5C-FRONT-12: statements that finish at once run here, not through `run`, so
            // a block of them (800,000 locals) went on for a quarter second after the
            // host's interrupt: the stop is looked at here too.
            if crate::diag::compile_tick() {
                return Err(self.err(Pos { line: self.line, col: 1 }, crate::diag::COMPILE_STOPPED));
            }
            if matches!(s.kind, StatKind::Label(_)) {
                i = self.label_run(b, i, repeat)?;
                continue;
            }
            // A label is "at the end" when only labels follow it (and the block is not a
            // repeat body, whose condition still sees its locals).
            let at_end = !repeat && b.stats.iter().skip(i + 1).all(|s| matches!(s.kind, StatKind::Label(_)));
            let mark = self.tasks.len();
            self.push(Task::BlockNext { b, i, repeat });
            self.statement(s, at_end && i < n)?;
            if self.tasks.len() != mark + 1 {
                return Ok(());
            }
            // The statement finished at once: on to the next without the stack.
            self.tasks.pop();
            let lvl = self.fsr().nvarstack();
            self.set_freereg(lvl);
            i += 1;
        }
    }

    /// Pushes `cont`, then starts what it waits for with `f`; when that finished at once
    /// (nothing pushed above `cont`), runs `cont` too. `f` only starts things (they push
    /// what is not a leaf), and the continuations run this way run `seq` only for ones that
    /// do not (`CmpL` → `CmpR`, `ArithL` → `ArithR`): this nests a few levels at most.
    fn seq(&mut self, cont: Task<'a>, f: impl FnOnce(&mut Self) -> CResult<()>) -> CResult<()> {
        let mark = self.tasks.len();
        self.push(cont);
        f(self)?;
        if self.tasks.len() == mark + 1
            && let Some(t) = self.tasks.pop()
        {
            return self.step(t);
        }
        Ok(())
    }

    fn statement(&mut self, s: &'a Stat, label_at_end: bool) -> CResult<()> {
        self.at(s.pos);
        match &s.kind {
            StatKind::Call(e) => self.call(e, Want::Fixed(0)),
            StatKind::Local(names, exprs) => self.local_stat(s, names, exprs),
            StatKind::Assign(targets, exprs, follow) => self.assign(s, targets, exprs, follow),
            StatKind::OpAssign(b) => {
                self.push(Task::OpAssignPlace(s));
                self.place(&b.0, b.0.pos.line)
            }
            StatKind::Do(b) => {
                self.enter_block(false);
                self.then(Task::LeaveBlock(b.end), Task::Block { b, repeat: false })
            }
            StatKind::While(cond, body) => {
                self.enter_block(true);
                let start = self.pc();
                self.seq(Task::WhileCond { body, start }, |c| c.cond_jump(cond, false))
            }
            StatKind::Repeat(body, cond) => {
                self.enter_block(true);
                let start = self.pc();
                self.enter_block(false);
                self.then(Task::RepeatBody { body, cond, start }, Task::Block { b: body, repeat: true })
            }
            StatKind::If(..) => self.if_next(s, 0, Vec::new()),
            StatKind::NumFor(f) => {
                self.enter_block(true);
                let base = self.freereg();
                self.push(Task::NumForStart { s, base });
                self.expr_next(&f.start)
            }
            StatKind::GenFor { exprs, .. } => {
                self.enter_block(true);
                let base = self.freereg();
                self.push(Task::GenForExprs { s, base });
                self.explist(exprs, Want::Fixed(4), s.pos)
            }
            StatKind::Function { path, method, body } => self.func_stat(path, method.as_deref(), body, s.pos),
            StatKind::LocalFunction(name, body) => {
                let r = self.alloc_reg(s.pos)?;
                self.add_local(&name.name, VarKind::Reg(r), Attrib::None);
                self.then(Task::Closure { r, pos: s.pos, place: None }, Task::Function(body))
            }
            StatKind::Return(exprs) => self.ret(exprs, s.pos),
            StatKind::Break => self.goto_loop(BREAK, s.pos),
            StatKind::Continue => self.goto_loop(CONTINUE, s.pos),
            StatKind::Goto(name, line) => self.goto(name, Pos { line: *line, col: s.pos.col }),
            StatKind::Label(name) => self.label_stat(name, s.pos, label_at_end, s.pos.line),
            StatKind::TypeDef(..) => Ok(()),
        }
    }

    fn goto_loop(&mut self, name: &str, pos: Pos) -> CResult<()> {
        // Outside any loop it stays pending, reported when the function ends (as C Lua).
        let pc = self.jump();
        let nactvar = self.fsr().actives.len();
        self.fs().gotos.push(GotoDesc { name: name.to_owned(), pc, line: pos.line, nactvar, close: None });
        Ok(())
    }

    fn local_stat(&mut self, s: &'a Stat, names: &'a [LocalName], exprs: &'a [Expr]) -> CResult<()> {
        // `local x <const> = constant`: a compile-time constant, no register. As in C Lua's
        // `localstat`, the last name of a statement with as many values as names (S5COR-16:
        // only a lone name was).
        if names.len() == exprs.len()
            && let (Some(n), Some(e)) = (names.last(), exprs.last())
            && n.attrib == Attrib::Const
            && let Some(v) = self.const_value(e)?
        {
            if names.len() == 1 {
                self.add_local(&n.name, VarKind::Const(v), Attrib::Const);
                return Ok(());
            }
            let base = self.freereg();
            let rest = exprs.get(..exprs.len() - 1).unwrap_or_default();
            return self.seq(Task::LocalAfter { s, base, konst: Some(v) }, |c| c.explist(rest, Want::Fixed(names.len() - 1), s.pos));
        }
        let base = self.freereg();
        self.seq(Task::LocalAfter { s, base, konst: None }, |c| c.explist(exprs, Want::Fixed(names.len()), s.pos))
    }

    fn local_after(&mut self, s: &'a Stat, base: Reg, konst: Option<Value>) -> CResult<()> {
        let StatKind::Local(names, exprs) = &s.kind else { return Ok(()) };
        // (With `konst`, the last name is that constant, after the others.)
        let regs = names.len() - usize::from(konst.is_some());
        self.set_freereg(us(base) + regs);
        for (i, n) in names.iter().take(regs).enumerate() {
            let r = base + i as Reg;
            self.add_local(&n.name, VarKind::Reg(r), n.attrib);
            if n.attrib == Attrib::Close {
                // C Lua's line: the statement's last token (the end of its values).
                self.at(exprs.last().map_or(n.pos, |e| e.end));
                self.emit(Instr::Tbc { a: r });
                let fs = self.fs();
                fs.tbc_active += 1;
                if let Some(b) = fs.blocks.last_mut() {
                    b.has_upval = true;
                }
            }
        }
        if let (Some(v), Some(n)) = (konst, names.last()) {
            self.add_local(&n.name, VarKind::Const(v), Attrib::Const);
        }
        Ok(())
    }

    /// The value of a constant expression (for `<const>` locals).
    /// The value of a compile-time constant expression, as Lua's `luaK_exp2const` sees it
    /// after its code generator folds (`ast::fold_bin`, `ast::fold_un`): literals, `<const>`
    /// locals, and operators on them (S5COR-13: only literals and names counted, so `local b
    /// <const> = a * 2` with `a` constant got a register, and `t[b]` was no constant key).
    /// A loop over an explicit stack, not recursion.
    fn const_value(&mut self, e: &Expr) -> CResult<Option<Value>> {
        enum Step<'e> {
            Eval(&'e Expr),
            Un(UnOp),
            Bin(BinOp),
        }
        let mut work = vec![Step::Eval(e)];
        let mut vals: Vec<Value> = Vec::new();
        while let Some(step) = work.pop() {
            crate::diag::compile_work(1);
            match step {
                Step::Eval(x) => match &x.kind {
                    ExprKind::Nil => vals.push(Value::Nil),
                    ExprKind::True => vals.push(Value::Bool(true)),
                    ExprKind::False => vals.push(Value::Bool(false)),
                    ExprKind::Int(i) => vals.push(Value::Int(*i)),
                    ExprKind::Num(f) => vals.push(Value::Num(*f)),
                    ExprKind::Str(s) => {
                        let k = self.str_const(s, x.pos)?;
                        vals.push(self.fsr().consts.get(k as usize).copied().unwrap_or_default());
                    }
                    ExprKind::Name(n) => match self.resolve(n) {
                        Some(VarRef::Const(v)) => vals.push(v),
                        _ => return Ok(None),
                    },
                    ExprKind::Paren(i) => work.push(Step::Eval(i)),
                    ExprKind::Un(op @ (UnOp::Neg | UnOp::BNot | UnOp::Not), i) => {
                        work.push(Step::Un(*op));
                        work.push(Step::Eval(i));
                    }
                    ExprKind::Bin(op, l, r) if arith_op(*op).is_some() => {
                        work.push(Step::Bin(*op));
                        work.push(Step::Eval(r));
                        work.push(Step::Eval(l));
                    }
                    _ => return Ok(None),
                },
                Step::Un(op) => {
                    let Some(v) = vals.pop().and_then(|v| crate::ast::fold_un(op, v)) else { return Ok(None) };
                    vals.push(v);
                }
                Step::Bin(op) => {
                    let (Some(b), Some(a)) = (vals.pop(), vals.pop()) else { return Ok(None) };
                    let Some(v) = crate::ast::fold_bin(op, a, b) else { return Ok(None) };
                    vals.push(v);
                }
            }
        }
        Ok(vals.pop())
    }

    /// Refuses an assignment to a const variable, reported at `line` (C Lua's: the token
    /// after the target, S6-FRONT-7).
    fn check_assignable(&self, pos: Pos, line: u32, v: &Option<VarRef>, name: &str) -> CResult<()> {
        let refuse = || Err(self.err(pos, format!("attempt to assign to const variable '{name}'")).lua_line(line));
        match v {
            Some(VarRef::Const(_)) | Some(VarRef::Local(_, _, Attrib::Const | Attrib::Close)) => refuse(),
            Some(VarRef::Upval(_)) => {
                // An upvalue made from a const local is const too: look it up outward.
                if self.upval_is_const(name) {
                    return refuse();
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    fn upval_is_const(&self, name: &str) -> bool {
        for fs in self.fs.iter().rev().skip(1) {
            let at = fs.actives.iter().rposition(|v| &*v.name == name);
            crate::diag::compile_work(at.map_or(fs.actives.len(), |i| fs.actives.len() - i));
            if let Some(v) = at.and_then(|i| fs.actives.get(i)) {
                return matches!(v.attrib, Attrib::Const | Attrib::Close);
            }
        }
        false
    }

    fn assign(&mut self, s: &'a Stat, targets: &'a [Expr], exprs: &'a [Expr], follow: &[u32]) -> CResult<()> {
        // `x = e` to a local: evaluate straight into its register.
        if let ([t], [e]) = (targets, exprs)
            && let ExprKind::Name(n) = &t.kind
        {
            let v = self.resolve(n);
            self.check_assignable(t.pos, crate::ast::follow_line(targets, follow, 0), &v, n)?;
            if let Some(VarRef::Local(r, _, _)) = v {
                return self.expr_to_reg(e, r);
            }
        }
        self.assign_places(s, Vec::new())
    }

    /// The assignment's targets from the next on, then its values.
    fn assign_places(&mut self, s: &'a Stat, places: Vec<Place>) -> CResult<()> {
        let StatKind::Assign(targets, exprs, follow) = &s.kind else { return Ok(()) };
        if let Some(t) = targets.get(places.len()) {
            let line = crate::ast::follow_line(targets, follow, places.len());
            self.push(Task::AssignPlace { s, places });
            return self.place(t, line);
        }
        let pos = s.pos;
        // C Lua numbers the stores where the values end (the last token read).
        let store_at = exprs.last().map_or(pos, |e| e.end);
        if let ([p], [e]) = (places.as_slice(), exprs.as_slice()) {
            return self.seq(Task::AssignOne { p: *p, pos: store_at }, |c| c.expr_rk(e));
        }
        let base = self.freereg();
        let n = places.len();
        self.seq(Task::AssignMulti { places, base, pos: store_at }, |c| c.explist(exprs, Want::Fixed(n), pos))
    }

    /// Prepares an assignment target: evaluates its table and key. A const one is
    /// reported at `line`.
    fn place(&mut self, t: &'a Expr, line: u32) -> CResult<()> {
        match &t.kind {
            ExprKind::Name(n) => {
                let p = self.place_name(n, t.pos, line)?;
                self.done(Ret::Place(p))
            }
            ExprKind::Index(obj, key) => {
                if let Some(p) = self.upval_field(obj, key)? {
                    return self.done(Ret::Place(p));
                }
                self.then(Task::PlaceObj(key), Task::ExprAnyReg(obj))
            }
            _ => Err(self.err(t.pos, "syntax error (cannot assign to this expression)")),
        }
    }


    /// C Lua's `check_conflict`, when a target has just been read (the last of `places`):
    /// a local or upvalue that an earlier target's table (or key register) is gets copied
    /// now, before the targets after it and the values run, and those earlier targets use
    /// the copy (S5COR-9: the copy was made after every target had been read, so a later
    /// target's key that reassigned the local changed where the earlier store went).
    fn check_conflict(&mut self, places: &mut [Place], pos: Pos) -> CResult<()> {
        let Some((last, earlier)) = places.split_last_mut() else { return Ok(()) };
        crate::diag::compile_work(earlier.len());
        let extra = self.freereg();
        let mut conflict = false;
        match *last {
            Place::Local(r) => {
                for p in earlier.iter_mut() {
                    if let Place::Index(t, k) = p {
                        if *t == r {
                            *t = extra;
                            conflict = true;
                        }
                        if let KeyRef::Reg(kr) = k
                            && *kr == r
                        {
                            *kr = extra;
                            conflict = true;
                        }
                    }
                }
            }
            Place::Upval(u) => {
                for p in earlier.iter_mut() {
                    if let Place::Global(t, k) = *p
                        && t == u
                    {
                        *p = Place::Index(extra, KeyRef::K(k));
                        conflict = true;
                    }
                }
            }
            _ => {}
        }
        if conflict {
            let tmp = self.alloc_reg(pos)?;
            match *last {
                Place::Local(r) => self.emit(Instr::Move { a: tmp, b: r }),
                Place::Upval(u) => self.emit(Instr::GetUpval { a: tmp, u: cnt(u) }),
                _ => 0,
            };
        }
        Ok(())
    }

    /// `name.key` as an assignment target where `name` is an upvalue and `key` a short
    /// string constant: C Lua stores it with `SETTABUP`, which reads the upvalue when it
    /// stores, after the values (S5COR-9: it was read into a register first, so a value
    /// that reassigned the upvalue saw its store go to the old table). C Lua's own rule:
    /// short strings (40 bytes) among the first 256 constants.
    fn upval_field(&mut self, obj: &Expr, key: &Expr) -> CResult<Option<Place>> {
        let (ExprKind::Name(n), ExprKind::Str(s)) = (&obj.kind, &key.kind) else { return Ok(None) };
        if s.len() > 40 {
            return Ok(None);
        }
        let Some(VarRef::Upval(u)) = self.resolve(n) else { return Ok(None) };
        let k = self.str_const(s, key.pos)?;
        Ok((k <= 255).then_some(Place::Global(u, k)))
    }

    fn place_name(&mut self, n: &str, pos: Pos, line: u32) -> CResult<Place> {
        let v = self.resolve(n);
        self.check_assignable(pos, line, &v, n)?;
        Ok(match v {
            Some(VarRef::Local(r, _, _)) => Place::Local(r),
            Some(VarRef::Upval(u)) => Place::Upval(u),
            Some(VarRef::Const(_)) => Place::Local(0),
            None => {
                let k = self.str_const(n.as_bytes(), pos)?;
                match self.env(pos)? {
                    VarRef::Upval(u) => Place::Global(u, k),
                    VarRef::Local(r, _, _) => Place::Index(r, KeyRef::K(k)),
                    VarRef::Const(_) => return Err(self.err(pos, "_ENV is a constant")),
                }
            }
        })
    }

    fn key_ref(&mut self, key: &'a Expr) -> CResult<()> {
        let k = match &key.kind {
            ExprKind::Str(s) => Some(self.str_const(s, key.pos)?),
            ExprKind::Int(i) => Some(self.num_const(Value::Int(*i), key.pos)?),
            ExprKind::Num(f) if !f.is_nan() => Some(self.num_const(Value::Num(*f), key.pos)?),
            ExprKind::True => Some(self.num_const(Value::Bool(true), key.pos)?),
            ExprKind::False => Some(self.num_const(Value::Bool(false), key.pos)?),
            // A `<const>` local, or operators on constants, is the constant it folds to, as
            // in C Lua (`t[k]` with `k <const> = 5` is `t[5]`; S5COR-13).
            ExprKind::Name(_) | ExprKind::Paren(_) | ExprKind::Un(..) | ExprKind::Bin(..) => match self.const_value(key)? {
                Some(Value::Str(r)) => {
                    let bytes = self.heap.str_bytes(r).to_vec();
                    Some(self.str_const(&bytes, key.pos)?)
                }
                Some(v @ (Value::Int(_) | Value::Bool(_))) => Some(self.num_const(v, key.pos)?),
                Some(Value::Num(f)) if !f.is_nan() => Some(self.num_const(Value::Num(f), key.pos)?),
                _ => None,
            },
            _ => None,
        };
        if let Some(k) = k
            && Self::k16(k).is_some()
        {
            return self.done(Ret::Key(KeyRef::K(k)));
        }
        // A register (handed back as `Ret::Reg`).
        self.expr_any_reg(key)
    }

    /// `key_ref` of a string key (the names of `function a.b.c()`).
    fn str_key(&mut self, s: &[u8], pos: Pos) -> CResult<KeyRef> {
        let k = self.str_const(s, pos)?;
        if Self::k16(k).is_some() {
            return Ok(KeyRef::K(k));
        }
        let r = self.alloc_reg(pos)?;
        self.at(pos);
        let save = self.freereg();
        let k = self.str_const(s, pos)?;
        self.load_const(k, r);
        self.set_freereg(us(save));
        Ok(KeyRef::Reg(r))
    }

    fn store(&mut self, p: &Place, v: Rk) -> CResult<()> {
        match *p {
            Place::Local(r) => {
                if v.is_const() {
                    self.emit(Instr::LoadK { a: r, k: v.index() as u32 });
                } else if v.index() != us(r) {
                    self.emit(Instr::Move { a: r, b: v.index() as Reg });
                }
            }
            Place::Upval(u) => {
                let r = self.rk_to_reg(v)?;
                self.emit(Instr::SetUpval { a: r, u: cnt(u) });
            }
            Place::Global(u, k) => match Self::k16(k) {
                Some(k) => {
                    self.emit(Instr::SetTabUp { u: cnt(u), k, v });
                }
                None => {
                    let t = self.alloc_reg(Pos::default())?;
                    self.emit(Instr::GetUpval { a: t, u: cnt(u) });
                    let kr = self.alloc_reg(Pos::default())?;
                    self.emit(Instr::LoadK { a: kr, k });
                    self.emit(Instr::SetTable { a: t, b: kr, c: v });
                }
            },
            Place::Index(t, KeyRef::K(k)) => match Self::k16(k) {
                Some(k) => {
                    self.emit(Instr::SetTableK { a: t, k, c: v });
                }
                None => {
                    let kr = self.alloc_reg(Pos::default())?;
                    self.emit(Instr::LoadK { a: kr, k });
                    self.emit(Instr::SetTable { a: t, b: kr, c: v });
                }
            },
            Place::Index(t, KeyRef::Reg(k)) => {
                self.emit(Instr::SetTable { a: t, b: k, c: v });
            }
        }
        Ok(())
    }

    fn rk_to_reg(&mut self, v: Rk) -> CResult<Reg> {
        if v.is_const() {
            let r = self.alloc_reg(Pos::default())?;
            self.emit(Instr::LoadK { a: r, k: v.index() as u32 });
            Ok(r)
        } else {
            Ok(v.index() as Reg)
        }
    }

    fn op_assign_place(&mut self, s: &'a Stat) -> CResult<()> {
        let p = self.take_place();
        let StatKind::OpAssign(b) = &s.kind else { return Ok(()) };
        let (_, op, e) = &**b;
        let (op, pos) = (*op, s.pos);
        // The current value.
        let cur = match &p {
            Place::Local(r) => *r,
            _ => {
                let tmp = self.alloc_reg(pos)?;
                self.load_place(&p, tmp)?;
                tmp
            }
        };
        let dst = cur;
        self.at(pos);
        if op == BinOp::Concat {
            let base = self.freereg();
            let a = self.alloc_reg(pos)?;
            self.emit(Instr::Move { a, b: cur });
            let b = self.alloc_reg(pos)?;
            self.push(Task::OpConcatEnd { p, a, dst, base, pos });
            return self.expr_to_reg(e, b);
        }
        self.push(Task::OpArithEnd { p, cur, op, pos });
        self.expr_rk(e)
    }

    fn load_place(&mut self, p: &Place, dst: Reg) -> CResult<()> {
        match *p {
            Place::Local(r) => {
                self.emit(Instr::Move { a: dst, b: r });
            }
            Place::Upval(u) => {
                self.emit(Instr::GetUpval { a: dst, u: cnt(u) });
            }
            Place::Global(u, k) => match Self::k16(k) {
                Some(k) => {
                    self.emit(Instr::GetTabUp { a: dst, u: cnt(u), k });
                }
                None => {
                    self.emit(Instr::GetUpval { a: dst, u: cnt(u) });
                    let kr = self.alloc_reg(Pos::default())?;
                    self.emit(Instr::LoadK { a: kr, k });
                    self.emit(Instr::GetTable { a: dst, b: dst, c: kr });
                }
            },
            Place::Index(t, KeyRef::K(k)) => match Self::k16(k) {
                Some(k) => {
                    self.emit(Instr::GetTableK { a: dst, b: t, k });
                }
                None => {
                    let kr = self.alloc_reg(Pos::default())?;
                    self.emit(Instr::LoadK { a: kr, k });
                    self.emit(Instr::GetTable { a: dst, b: t, c: kr });
                }
            },
            Place::Index(t, KeyRef::Reg(k)) => {
                self.emit(Instr::GetTable { a: dst, b: t, c: k });
            }
        }
        Ok(())
    }

    /// Condition `i` of an `if` on, then its `else`.
    fn if_next(&mut self, s: &'a Stat, i: usize, to_end: Vec<usize>) -> CResult<()> {
        let StatKind::If(conds, els) = &s.kind else { return Ok(()) };
        if let Some((cond, _)) = conds.get(i) {
            self.at(cond.pos);
            return self.seq(Task::IfCond { s, i, to_end }, |c| c.cond_jump(cond, false));
        }
        if let Some(b) = els {
            self.enter_block(false);
            return self.then(Task::IfElseEnd { end: b.end, to_end }, Task::Block { b, repeat: false });
        }
        self.patch_here(&to_end);
        Ok(())
    }

    fn num_for_body(&mut self, s: &'a Stat, base: Reg) -> CResult<()> {
        let StatKind::NumFor(f) = &s.kind else { return Ok(()) };
        let ForNum { var, body, do_line, .. } = &**f;
        // Hidden control registers, then the loop variable.
        self.add_local("(for state)", VarKind::Reg(base), Attrib::None);
        self.add_local("(for state)", VarKind::Reg(base + 1), Attrib::None);
        self.add_local("(for state)", VarKind::Reg(base + 2), Attrib::None);
        self.at(Pos { line: *do_line, col: 1 });
        let prep = self.emit(Instr::ForPrep { a: base, off: 0 });
        let body_start = self.pc();
        self.enter_block(false);
        let r = self.alloc_reg(var.pos)?;
        self.add_local(&var.name, VarKind::Reg(r), Attrib::None);
        self.then(Task::NumForEnd { s, base, prep, body_start }, Task::Block { b: body, repeat: false })
    }

    fn gen_for_body(&mut self, s: &'a Stat, base: Reg) -> CResult<()> {
        let StatKind::GenFor { names, body, do_line, .. } = &s.kind else { return Ok(()) };
        self.set_freereg(us(base) + 4);
        // S5COR-4: the closing value (the fourth) is to-be-closed, so the loop is "inside a
        // to-be-closed variable" (C Lua's `marktobeclosed`) until its end: no tail call in
        // its body, else `return f()` closed it before `f` ran (and `__close` saw no error).
        for i in 0..4 {
            let attrib = if i == 3 { Attrib::Close } else { Attrib::None };
            self.add_local("(for state)", VarKind::Reg(base + i), attrib);
        }
        self.fs().tbc_active += 1;
        // The closing value is to-be-closed: leaving the loop closes it.
        if let Some(b) = self.fs().blocks.last_mut() {
            b.has_upval = true;
        }
        self.at(Pos { line: *do_line, col: 1 });
        let prep = self.emit(Instr::TForPrep { a: base, off: 0 });
        let body_start = self.pc();
        self.enter_block(false);
        for n in names {
            let r = self.alloc_reg(n.pos)?;
            self.add_local(&n.name, VarKind::Reg(r), Attrib::None);
        }
        self.then(Task::GenForEnd { s, base, prep, body_start }, Task::Block { b: body, repeat: false })
    }

    fn func_stat(&mut self, path: &[String], method: Option<&str>, body: &'a FuncBody, pos: Pos) -> CResult<()> {
        let Some(first) = path.first() else { return Ok(()) };
        if path.len() == 1 && method.is_none() {
            // A const `x` in `function x() ... end`: C Lua's `funcstat` checks it after the
            // body, with the token after `end` read (S6-FRONT-7).
            let v = self.resolve(first);
            if let Err(d) = self.check_assignable(pos, body.close_line, &v, first) {
                return self.then(Task::Fail(Box::new(d)), Task::Function(body));
            }
            let place = self.place_name(first, pos, pos.line)?;
            if let Place::Local(r) = place {
                return self.then(Task::Closure { r, pos, place: None }, Task::Function(body));
            }
            let r = self.alloc_reg(pos)?;
            return self.then(Task::Closure { r, pos, place: Some(place) }, Task::Function(body));
        }
        // a.b.c[:m]: evaluate a.b, then store the closure under the last name.
        let (last, prefix) = match method {
            Some(m) => (m.to_owned(), path),
            None => (path.last().cloned().unwrap_or_default(), path.get(..path.len() - 1).unwrap_or_default()),
        };
        let t = self.path_any_reg(prefix, pos)?;
        let k = self.str_const(last.as_bytes(), pos)?;
        let r = self.alloc_reg(pos)?;
        self.then(Task::Closure { r, pos, place: Some(Place::Index(t, KeyRef::K(k))) }, Task::Function(body))
    }

    /// `expr_any_reg` of `a.b.c` (the path of `function a.b.c.d()`), as a loop: down the
    /// path each level takes its register, then up it each loads its field.
    fn path_any_reg(&mut self, prefix: &[String], pos: Pos) -> CResult<Reg> {
        let Some((first, rest)) = prefix.split_first() else { return Err(self.err(pos, "internal: empty path")) };
        let mut levels = Vec::with_capacity(rest.len());
        for key in rest.iter().rev() {
            let r = self.alloc_reg(pos)?;
            self.at(pos);
            levels.push((r, self.freereg(), key));
        }
        let mut t = match self.resolve(first) {
            Some(VarRef::Local(r, _, _)) => r,
            _ => {
                let r = self.alloc_reg(pos)?;
                self.at(pos);
                let save = self.freereg();
                self.name_to_reg(first, r, pos)?;
                self.set_freereg(us(save));
                r
            }
        };
        while let Some((r, save, key)) = levels.pop() {
            let k = self.str_key(key.as_bytes(), pos)?;
            self.at(pos);
            self.load_place(&Place::Index(t, k), r)?;
            self.set_freereg(us(save));
            t = r;
        }
        Ok(t)
    }

    fn ret(&mut self, exprs: &'a [Expr], pos: Pos) -> CResult<()> {
        // `return f(x)`: a tail call, unless a to-be-closed variable must close first.
        if let [e] = exprs
            && matches!(e.kind, ExprKind::Call(..) | ExprKind::Method(..))
            && self.fsr().tbc_active == 0
        {
            let base = self.freereg();
            self.push(Task::RetTail { base, pos: e.call_at() }); // the call's line, as in C Lua
            return self.call_setup(e, base);
        }
        let base = self.freereg();
        if let [e] = exprs
            && !e.kind.is_multi()
        {
            // One value: return it from where it is.
            return self.seq(Task::RetOne(e.end), |c| c.expr_any_reg(e));
        }
        // C Lua numbers the return where its values end (the last token read): a
        // `__close` that fails there names that line (S5COR-15).
        let at = exprs.last().map_or(pos, |e| e.end);
        self.seq(Task::RetMulti { base, pos: at }, |c| c.explist(exprs, Want::Multi, pos))
    }

    // ---- expressions ----

    /// Evaluates `exprs` into consecutive registers from the first free one. Hands back the
    /// count field for the consumer (MULTI when the last expression gave a variable number).
    fn explist(&mut self, exprs: &'a [Expr], want: Want, pos: Pos) -> CResult<()> {
        let base = us(self.freereg());
        self.explist_next(ExplistSt { exprs, i: 0, want, pos, base })
    }

    fn explist_next(&mut self, mut st: ExplistSt<'a>) -> CResult<()> {
        let n = st.exprs.len();
        // Leaves go in at once (they wait for nothing).
        while let Some(e) = st.exprs.get(st.i)
            && is_leaf(e)
            && !(st.i + 1 == n && e.kind.is_multi())
        {
            self.expr_next(e)?;
            st.i += 1;
        }
        if let Some(e) = st.exprs.get(st.i) {
            let last = st.i + 1 == n;
            if last && e.kind.is_multi() {
                match st.want {
                    Want::Multi => return self.then(Task::ExplistRetMulti, Task::Multi { e, n: MULTI }),
                    Want::Fixed(w) => {
                        let needed = w.saturating_sub(n - 1);
                        if needed != 0 {
                            return self.then(Task::ExplistAfterMulti { st, needed }, Task::Multi { e, n: needed as Reg });
                        }
                    }
                }
            }
            return self.then(Task::ExplistNext(st), Task::ExprNext(e));
        }
        let got = us(self.freereg()) - st.base;
        match st.want {
            Want::Fixed(w) => {
                if got < w {
                    let first = self.freereg();
                    self.reserve(w - got, st.pos)?;
                    self.load_nils(first, (w - got) as Reg);
                }
                self.done(Ret::Count(w.min(us(MULTI) - 1) as Reg))
            }
            Want::Multi => self.done(Ret::Count(got as Reg)),
        }
    }

    /// A call or `...` producing `n` results (MULTI: all, setting the top) from the first
    /// free register (which it does not reserve).
    fn multi(&mut self, e: &'a Expr, n: Reg) -> CResult<()> {
        if let ExprKind::Dots = e.kind {
            let a = self.freereg();
            self.at(e.pos);
            // The values' registers count before it is emitted: past one window, it is
            // placed by them (it wrote past the stack otherwise, and they were lost).
            if n != MULTI {
                self.need_stack(us(a) + us(n));
            }
            if n != MULTI && n > MAX_COUNT {
                self.emit(Instr::VarArg { a, n: cnt(MULTI) });
                let line = self.line;
                self.pad_nil(a + n, line);
            } else {
                self.emit(Instr::VarArg { a, n: cnt(n) });
            }
            return self.done(Ret::None);
        }
        let base = self.freereg();
        self.seq(Task::MultiFinish { e, n, base }, |c| c.call_setup(e, base))
    }

    /// A call, as a statement or for `want` results placed from the first free register
    /// (hands back that register).
    fn call(&mut self, e: &'a Expr, want: Want) -> CResult<()> {
        let base = self.freereg();
        self.seq(Task::CallFinish { e, want, base }, |c| c.call_setup(e, base))
    }

    /// Puts the function and its arguments in registers from `base`; hands back the
    /// argument count field.
    fn call_setup(&mut self, e: &'a Expr, base: Reg) -> CResult<()> {
        match &e.kind {
            ExprKind::Call(f, args) => self.seq(Task::SetupArgs { args, pos: e.pos }, |c| c.start(Task::ExprNext(f))),
            ExprKind::Method(obj, ..) => self.then(Task::SetupMethod { e, base }, Task::ExprAnyReg(obj)),
            _ => Err(self.err(e.pos, "internal: not a call")),
        }
    }

    /// The value of `e` in the next free register (reserved; handed back).
    fn expr_next(&mut self, e: &'a Expr) -> CResult<()> {
        if matches!(e.kind, ExprKind::Call(..) | ExprKind::Method(..)) {
            self.push(Task::ExprNextCall(e.pos));
            return self.call(e, Want::Fixed(1));
        }
        if let ExprKind::Dots = e.kind {
            let a = self.freereg();
            self.at(e.pos);
            self.emit(Instr::VarArg { a, n: 1 });
            self.reserve(1, e.pos)?;
            return self.done(Ret::Reg(a));
        }
        let r = self.alloc_reg(e.pos)?;
        // (`expr_to_reg` hands `r` back.)
        self.expr_to_reg(e, r)
    }

    /// The register of `e` when it is a local of this function (C Lua's `VNONRELOC`): `and`
    /// and `or` test it in place with `TestSet`.
    fn local_reg(&mut self, e: &Expr) -> Option<Reg> {
        let e = match &e.kind {
            ExprKind::Paren(inner) if matches!(inner.kind, ExprKind::Name(_)) => &**inner,
            _ => e,
        };
        match &e.kind {
            ExprKind::Name(n) => match self.resolve(n) {
                Some(VarRef::Local(r, _, _)) => Some(r),
                _ => None,
            },
            _ => None,
        }
    }

    /// A register holding `e`: a local's own register, or a new temporary (handed back).
    fn expr_any_reg(&mut self, e: &'a Expr) -> CResult<()> {
        let e = match &e.kind {
            ExprKind::Paren(inner) if matches!(inner.kind, ExprKind::Name(_)) => &**inner,
            _ => e,
        };
        if let ExprKind::Name(n) = &e.kind
            && let Some(VarRef::Local(r, _, _)) = self.resolve(n)
        {
            return self.done(Ret::Reg(r));
        }
        self.expr_next(e)
    }

    /// A constant slot or a register holding `e` (handed back as `Rk` or `Reg`).
    fn expr_rk(&mut self, e: &'a Expr) -> CResult<()> {
        let k = match &e.kind {
            ExprKind::Str(s) => Some(self.str_const(s, e.pos)?),
            ExprKind::Int(i) => Some(self.num_const(Value::Int(*i), e.pos)?),
            ExprKind::Num(f) if !f.is_nan() => Some(self.num_const(Value::Num(*f), e.pos)?),
            ExprKind::Nil => Some(self.num_const(Value::Nil, e.pos)?),
            ExprKind::True => Some(self.num_const(Value::Bool(true), e.pos)?),
            ExprKind::False => Some(self.num_const(Value::Bool(false), e.pos)?),
            ExprKind::Name(n) => match self.resolve(n) {
                Some(VarRef::Const(v)) => Some(self.value_const(v, e.pos)?),
                _ => None,
            },
            _ => None,
        };
        if let Some(k) = k
            && let Some(k) = Self::k16(k)
        {
            return self.done(Ret::Rk(Rk::konst(k)));
        }
        self.expr_any_reg(e)
    }

    fn value_const(&mut self, v: Value, pos: Pos) -> CResult<u32> {
        match v {
            Value::Str(s) => {
                let bytes = self.heap.str_bytes(s).to_vec();
                self.str_const(&bytes, pos)
            }
            other => self.num_const(other, pos),
        }
    }

    fn load_const(&mut self, k: u32, dst: Reg) {
        self.emit(Instr::LoadK { a: dst, k });
    }

    /// A name's value into `dst`.
    fn name_to_reg(&mut self, n: &str, dst: Reg, pos: Pos) -> CResult<()> {
        match self.resolve(n) {
            Some(VarRef::Local(r, _, _)) => {
                self.move_to(dst, r);
            }
            Some(VarRef::Const(v)) => {
                let k = self.value_const(v, pos)?;
                self.load_const(k, dst);
            }
            Some(VarRef::Upval(u)) => {
                self.emit(Instr::GetUpval { a: dst, u: cnt(u) });
            }
            None => {
                let k = self.str_const(n.as_bytes(), pos)?;
                match self.env(pos)? {
                    VarRef::Upval(u) => self.load_place(&Place::Global(u, k), dst)?,
                    VarRef::Local(r, _, _) => self.load_place(&Place::Index(r, KeyRef::K(k)), dst)?,
                    VarRef::Const(_) => return Err(self.err(pos, "_ENV is a constant")),
                }
            }
        }
        Ok(())
    }

    /// Evaluates `e` into register `dst` (one value; `dst` is handed back).
    fn expr_to_reg(&mut self, e: &'a Expr, dst: Reg) -> CResult<()> {
        self.at(e.pos);
        let save = self.freereg();
        match &e.kind {
            ExprKind::Nil => {
                self.emit(Instr::LoadNil { a: dst, n: 0 });
            }
            ExprKind::True => {
                self.emit(Instr::LoadBool { a: dst, v: true });
            }
            ExprKind::False => {
                self.emit(Instr::LoadBool { a: dst, v: false });
            }
            ExprKind::Int(i) => match i32::try_from(*i) {
                Ok(i) => {
                    self.emit(Instr::LoadInt { a: dst, i });
                }
                Err(_) => {
                    let k = self.num_const(Value::Int(*i), e.pos)?;
                    self.load_const(k, dst);
                }
            },
            ExprKind::Num(f) => {
                let k = self.num_const(Value::Num(*f), e.pos)?;
                self.load_const(k, dst);
            }
            ExprKind::Str(s) => {
                let k = self.str_const(s, e.pos)?;
                self.load_const(k, dst);
            }
            ExprKind::Dots => {
                self.emit(Instr::VarArg { a: dst, n: 1 });
            }
            // The inner expression restores the same free register itself: no task left.
            ExprKind::Paren(inner) => {
                self.push(Task::ExprToReg { e: inner, dst });
                return Ok(());
            }
            ExprKind::Name(n) => self.name_to_reg(n, dst, e.pos)?,
            ExprKind::Index(obj, key) => {
                // (C Lua reads a field where its key ends.)
                if self.chains_into(obj, dst) {
                    return self.then(Task::IndexObj { key, dst, pos: e.end, save }, Task::ExprToReg { e: obj, dst });
                }
                return self.then(Task::IndexObj { key, dst, pos: e.end, save }, Task::ExprAnyReg(obj));
            }
            ExprKind::Call(..) | ExprKind::Method(..) => {
                self.push(Task::CallToReg { dst, save });
                return self.call(e, Want::Fixed(1));
            }
            ExprKind::Func(f) => return self.then(Task::FuncToReg { dst, pos: e.pos, save }, Task::Function(f)),
            ExprKind::Table(fields) => return self.table(fields, dst, e.pos, save),
            ExprKind::Concat(parts) => {
                // In `dst` itself only when it is a temporary (a local may be read by the
                // parts).
                if us(dst) + 1 == us(self.freereg()) && us(dst) >= self.fsr().nvarstack() {
                    self.set_freereg(us(dst));
                }
                let base = self.freereg();
                self.push(Task::ConcatNext { parts, i: 0, base, dst, pos: e.pos, save });
                return Ok(());
            }
            ExprKind::Interp(parts) => {
                // Built in `dst` itself when it is a temporary, as a concatenation is: then
                // a `..` around it merges with it (S6-FRONT-LR6).
                if us(dst) + 1 == us(self.freereg()) && us(dst) >= self.fsr().nvarstack() {
                    self.set_freereg(us(dst));
                }
                let base = self.freereg();
                return self.interp_next(parts, 0, base, dst, e.pos, save);
            }
            ExprKind::Un(op, inner) => {
                if self.chains_into(inner, dst) {
                    return self.then(Task::UnTo { op: *op, dst, pos: e.pos, save }, Task::ExprToReg { e: inner, dst });
                }
                return self.then(Task::UnTo { op: *op, dst, pos: e.pos, save }, Task::ExprAnyReg(inner));
            }
            ExprKind::Bin(op, l, r) => return self.binary(*op, l, r, dst, e.pos, save),
        }
        self.restore(save, dst)
    }

    /// Whether operand `e` of a unary operator or a field access whose value goes to
    /// temporary `dst` goes to `dst` too: when it is itself one (`not not x`, `-(-x)`,
    /// `t.a.b`), so a chain takes one register, as in C Lua, not one a level.
    fn chains_into(&self, e: &Expr, dst: Reg) -> bool {
        self.reuse && matches!(e.kind, ExprKind::Un(..) | ExprKind::Index(..)) && us(dst) >= self.fsr().nvarstack()
    }

    /// The parts of an interpolated string from `i` on.
    fn interp_next(&mut self, parts: &'a [InterpExpr], mut i: usize, base: Reg, dst: Reg, pos: Pos, save: Reg) -> CResult<()> {
        while let Some(p) = parts.get(i) {
            match p {
                InterpExpr::Lit(b) => {
                    let r = self.alloc_reg(pos)?;
                    let k = self.str_const(b, pos)?;
                    self.load_const(k, r);
                }
                InterpExpr::Expr(e) => {
                    return self.then(Task::InterpExpr { parts, i, base, dst, pos, save }, Task::ExprNext(e));
                }
            }
            i += 1;
        }
        self.at(pos);
        if parts.len() == 1 {
            self.move_to(dst, base);
        } else {
            self.at(Pos { line: crate::ast::interp_join_line(parts, pos), col: 1 });
            self.emit_concat(base, parts.len());
            self.move_to(dst, base);
        }
        self.restore(save, dst)
    }

    fn table(&mut self, fields: &'a [Field], dst: Reg, pos: Pos, save: Reg) -> CResult<()> {
        // Built in `dst` itself when it is the newest register (items go right above it).
        // In `dst` itself only when it is the newest temporary: a local may be read by the
        // fields (`x = {x}`).
        let in_place = us(dst) + 1 == us(self.freereg()) && us(dst) >= self.fsr().nvarstack();
        let t = if in_place { dst } else { self.alloc_reg(pos)? };
        // Counted a stretch at a time, each a step (S6-FRONT-4: 3M fields took 10 ms or
        // more without a look at the interrupt).
        let mut narr = 0;
        for part in fields.chunks(4096) {
            narr += part.iter().filter(|f| matches!(f, Field::Pos(_))).count();
            if crate::diag::compile_ticks(u32::try_from(part.len()).unwrap_or(u32::MAX)) {
                return Err(self.err(pos, crate::diag::COMPILE_STOPPED));
            }
        }
        let nhash = fields.len() - narr;
        // A final call or `...` is not counted in Lua's array size (`lastlistfield`): the
        // table is made with room for it, sized as C Lua sizes it (`NEWTABLE_MULTRET`).
        let multret = matches!(fields.last(), Some(Field::Pos(e)) if e.kind.is_multi());
        let hash = nhash.min(crate::bytecode::NEWTABLE_HASH_MAX as usize) as u16 | if multret { crate::bytecode::NEWTABLE_MULTRET } else { 0 };
        self.emit(Instr::NewTable { a: t, arr: narr.min(u16::MAX as usize) as u16, hash });
        self.table_next(TableSt { fields, i: 0, t, dst, pos, pending: 0, stored: 0, save })
    }

    /// Field `st.i` of a table constructor on.
    fn table_next(&mut self, st: TableSt<'a>) -> CResult<()> {
        let n = st.fields.len();
        match st.fields.get(st.i) {
            Some(Field::Pos(e)) => {
                let last = st.i + 1 == n;
                if last && e.kind.is_multi() {
                    return self.then(Task::TableMulti(st), Task::Multi { e, n: MULTI });
                }
                self.then(Task::TablePos(st), Task::ExprNext(e))
            }
            Some(Field::Named(name, v, fpos)) => {
                let save = self.freereg();
                let k = self.str_const(name.as_bytes(), *fpos)?;
                self.then(Task::TableKeyVal { st, save, kr: KeyRef::K(k) }, Task::ExprRk(v))
            }
            Some(Field::Keyed(k, _)) => {
                let save = self.freereg();
                self.then(Task::TableKey { st, save }, Task::KeyRef(k))
            }
            None => {
                if st.pending > 0 {
                    self.at(st.pos);
                    self.emit(Instr::SetList { a: st.t, n: st.pending as u8, off: st.stored as u32 });
                }
                if st.t != st.dst {
                    self.emit(Instr::Move { a: st.dst, b: st.t });
                }
                self.restore(st.save, st.dst)
            }
        }
    }

    fn binary(&mut self, op: BinOp, l: &'a Expr, r: &'a Expr, dst: Reg, pos: Pos, save: Reg) -> CResult<()> {
        match op {
            BinOp::And | BinOp::Or => {
                let fl = self.fold_logic(l)?;
                if decided_by_constant(op, fl) {
                    // `k and x` with k a true constant, `nil or x`: just x (as Lua's code
                    // generator folds them).
                    // (`r` restores the same free register itself.)
                    self.push(Task::ExprToReg { e: r, dst });
                    return Ok(());
                }
                // Into a temporary when `dst` is a local the right side may read.
                let target = if us(dst) < self.fsr().nvarstack() { self.alloc_reg(pos)? } else { dst };
                if let Some(lr) = self.local_reg(fl) {
                    // A local on the left: tested and copied in one step, as C Lua.
                    self.at(pos);
                    self.emit(Instr::TestSet { a: target, b: lr, k: op == BinOp::Or });
                    let j = self.jump();
                    return self.then(Task::LogicR { j, target, dst, save }, Task::ExprToReg { e: r, dst: target });
                }
                self.then(Task::LogicL { op, r, target, dst, pos, save }, Task::ExprToReg { e: fl, dst: target })
            }
            BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
                self.push(Task::CmpToReg { dst, save });
                self.at(pos);
                self.compare(op, l, r, r.end, true) // C Lua numbers a comparison where its right side ends
            }
            BinOp::Concat => {
                let base = self.freereg();
                self.then(Task::ConcatL { r, base, dst, pos, save }, Task::ExprNext(l))
            }
            _ => {
                // A left-deep chain (`a + b + c ...`) compiles in a loop.
                let mut spine: Vec<(BinOp, &'a Expr, Pos)> = vec![(op, r, pos)];
                let mut left = l;
                while let ExprKind::Bin(op2, l2, r2) = &left.kind {
                    if arith_op(*op2).is_none() {
                        break;
                    }
                    spine.push((*op2, r2, left.pos));
                    left = l2;
                }
                if spine.len() == 1 {
                    return self.seq(Task::ArithL { op, r, dst, pos, save }, |c| c.start(Task::ExprRk(l)));
                }
                // The running value lives in a temporary (a local `dst` may be read later).
                let temp = us(dst) >= self.fsr().nvarstack() && us(dst) + 1 == us(self.freereg());
                let acc = if temp { dst } else { self.alloc_reg(pos)? };
                self.then(Task::SpineFirst { spine, acc, dst, save }, Task::ExprRk(left))
            }
        }
    }

    fn spine_next(&mut self, mut spine: Vec<(BinOp, &'a Expr, Pos)>, acc: Reg, dst: Reg, save: Reg, b: Rk) -> CResult<()> {
        if let Some((op, r, pos)) = spine.pop() {
            let save2 = self.freereg();
            return self.then(Task::SpineNext { spine, acc, dst, save, b, op, pos, save2 }, Task::ExprRk(r));
        }
        self.move_to(dst, acc);
        self.restore(save, dst)
    }

    /// `l op r` (a comparison) as a jump taken when its truth is `when`.
    fn compare(&mut self, op: BinOp, l: &'a Expr, r: &'a Expr, pos: Pos, when: bool) -> CResult<()> {
        let save = self.freereg();
        self.seq(Task::CmpL { op, r, pos, when, save }, |c| c.start(Task::ExprRk(l)))
    }

    /// Code that jumps (the jumps to patch are handed back) when `e`'s truth is `when`, and
    /// falls through otherwise.
    fn cond_jump(&mut self, e: &'a Expr, when: bool) -> CResult<()> {
        self.at(e.pos);
        match &e.kind {
            ExprKind::Nil | ExprKind::False => {
                let j = if when { Vec::new() } else { vec![self.jump()] };
                return self.done(Ret::Jumps(j));
            }
            ExprKind::True | ExprKind::Int(_) | ExprKind::Num(_) | ExprKind::Str(_) => {
                let j = if when { vec![self.jump()] } else { Vec::new() };
                return self.done(Ret::Jumps(j));
            }
            ExprKind::Paren(inner) if !inner.kind.is_multi() => {
                self.push(Task::CondJump { e: inner, when });
                return Ok(());
            }
            ExprKind::Un(UnOp::Not, inner) => {
                self.push(Task::CondJump { e: inner, when: !when });
                return Ok(());
            }
            ExprKind::Bin(BinOp::And, l, r) => {
                if when {
                    return self.then(Task::CondSkip { r, when: true }, Task::CondJump { e: l, when: false });
                }
                return self.then(Task::CondBoth { r, when: false }, Task::CondJump { e: l, when: false });
            }
            ExprKind::Bin(BinOp::Or, l, r) => {
                if when {
                    return self.then(Task::CondBoth { r, when: true }, Task::CondJump { e: l, when: true });
                }
                return self.then(Task::CondSkip { r, when: false }, Task::CondJump { e: l, when: true });
            }
            ExprKind::Bin(op @ (BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge), l, r) => {
                return self.compare(*op, l, r, r.end, when);
            }
            _ => {}
        }
        let save = self.freereg();
        self.seq(Task::TestJump { pos: e.pos, when, save }, |c| c.start(Task::ExprAnyReg(e)))
    }

    /// `a and b` / `a or b` after folding constant left sides away. Each node's answer is
    /// kept, so a chain of n operators takes n steps however often its parts are asked.
    fn fold_logic(&mut self, e: &'a Expr) -> CResult<&'a Expr> {
        fn logic(e: &Expr) -> bool {
            matches!(e.kind, ExprKind::Bin(BinOp::And | BinOp::Or, ..))
        }
        fn key(e: &Expr) -> usize {
            crate::nostd::ptr::from_ref(e).addr()
        }
        if !logic(e) {
            return Ok(e);
        }
        let mut todo = vec![e];
        while let Some(&n) = todo.last() {
            if self.fold_memo.contains_key(&key(n)) {
                todo.pop();
                continue;
            }
            let ExprKind::Bin(op, l, r) = &n.kind else {
                todo.pop();
                continue;
            };
            let fl = if logic(l) {
                match self.fold_memo.get(&key(l)) {
                    Some(&v) => v,
                    None => {
                        todo.push(l);
                        continue;
                    }
                }
            } else {
                &**l
            };
            let v = if decided_by_constant(*op, fl) {
                if logic(r) {
                    match self.fold_memo.get(&key(r)) {
                        Some(&v) => v,
                        None => {
                            todo.push(r);
                            continue;
                        }
                    }
                } else {
                    &**r
                }
            } else {
                n
            };
            self.fold_memo.insert(key(n), v);
            todo.pop();
        }
        // The answers are charged with the stacks.
        self.charge_stacks()?;
        Ok(self.fold_memo.get(&key(e)).copied().unwrap_or(e))
    }
}

#[derive(Clone, Copy)]
enum VarRef {
    Local(Reg, usize, Attrib),
    Upval(Reg),
    Const(Value),
}

#[derive(Clone, Copy)]
enum KeyRef {
    K(u32),
    Reg(Reg),
}

#[derive(Clone, Copy)]
enum Place {
    Local(Reg),
    Upval(Reg),
    Global(Reg, u32),
    Index(Reg, KeyRef),
}

/// An expression compiled without tasks of its own (it never waits for another).
fn is_leaf(e: &Expr) -> bool {
    matches!(
        e.kind,
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

/// `i` in the machine's registers, in a window from `off`; registers in `far` are the
/// scratch ones given.
fn lower(i: Instr, off: Reg, far: &[(Reg, Reg)]) -> MInstr {
    let r = |r: Reg| {
        let r = far.iter().find(|m| m.0 == r).map_or(r, |m| m.1);
        r.wrapping_sub(off) as u16
    };
    i.map(r, |k: Rk| if k.is_const() { MRk::konst(k.index() as u16) } else { MRk::reg(r(k.index() as Reg)) })
}

/// What `specialize` needs of a register-or-constant operand: this compiler's, or the
/// oracle's narrower one.
pub(crate) trait RkOperand: Copy {
    type Reg: Copy;
    fn is_const(self) -> bool;
    fn index(self) -> usize;
    fn as_reg(self) -> Self::Reg;
}

impl RkOperand for Rk {
    type Reg = Reg;
    #[inline]
    fn is_const(self) -> bool {
        Rk::is_const(self)
    }
    #[inline]
    fn index(self) -> usize {
        Rk::index(self)
    }
    #[inline]
    fn as_reg(self) -> Reg {
        Rk::index(self) as Reg
    }
}

impl RkOperand for MRk {
    type Reg = crate::bytecode::Reg;
    #[inline]
    fn is_const(self) -> bool {
        MRk::is_const(self)
    }
    #[inline]
    fn index(self) -> usize {
        MRk::index(self)
    }
    #[inline]
    fn as_reg(self) -> crate::bytecode::Reg {
        MRk::index(self) as crate::bytecode::Reg
    }
}

/// `Arith` on a register and a register or constant as its one-opcode form, an integer
/// constant as an immediate (`x + 1`, `x - 1` as `AddI`), and comparisons with a constant
/// as their constant or immediate forms, as C Lua 5.4 encodes them.
pub(crate) fn specialize<K: RkOperand>(i: Op<K::Reg, K>, consts: &[Value]) -> Op<K::Reg, K> {
    // A small integer constant: an immediate.
    let imm = |k: K| -> Option<i16> {
        if !k.is_const() {
            return None;
        }
        match consts.get(k.index()) {
            Some(Value::Int(n)) => i16::try_from(*n).ok().filter(|n| *n != i16::MIN),
            _ => None,
        }
    };
    match i {
        Op::Eq { b, c, k } => {
            // The constant on either side (no metamethod sees a constant).
            let (r, kc) = if b.is_const() { (c, b) } else { (b, c) };
            if r.is_const() || !kc.is_const() {
                return i;
            }
            let a = r.as_reg();
            return match imm(kc) {
                Some(n) => Op::EqI { a, i: n, eq: k },
                None => Op::EqK { a, k: kc.index() as u16, eq: k },
            };
        }
        Op::Lt { b, c, k } | Op::Le { b, c, k } => {
            let le = matches!(i, Op::Le { .. });
            if let Some(n) = imm(c)
                && !b.is_const()
            {
                let a = b.as_reg();
                return if le { Op::LeI { a, i: n, k } } else { Op::LtI { a, i: n, k } };
            }
            if let Some(n) = imm(b)
                && !c.is_const()
            {
                let a = c.as_reg();
                return if le { Op::GeI { a, i: n, k } } else { Op::GtI { a, i: n, k } };
            }
            return i;
        }
        _ => {}
    }
    let Op::Arith { op, a, b, c } = i else { return i };
    if b.is_const() {
        return i;
    }
    let b = b.as_reg();
    if c.is_const() {
        let k = c.index() as u16;
        match (op, imm(c)) {
            (ArithOp::Add, Some(n)) => return Op::AddI { a, b, i: n, sub: false },
            (ArithOp::Sub, Some(n)) => return Op::AddI { a, b, i: -n, sub: true },
            _ => {}
        }
        match op {
            ArithOp::Add => Op::AddK { a, b, k },
            ArithOp::Sub => Op::SubK { a, b, k },
            ArithOp::Mul => Op::MulK { a, b, k },
            ArithOp::Div => Op::DivK { a, b, k },
            ArithOp::Mod => Op::ModK { a, b, k },
            ArithOp::IDiv => Op::IDivK { a, b, k },
            ArithOp::Pow => Op::PowK { a, b, k },
            ArithOp::BAnd => Op::BAndK { a, b, k },
            ArithOp::BOr => Op::BOrK { a, b, k },
            ArithOp::BXor => Op::BXorK { a, b, k },
            ArithOp::Shl => Op::ShlK { a, b, k },
            ArithOp::Shr => Op::ShrK { a, b, k },
        }
    } else {
        let c = c.as_reg();
        match op {
            ArithOp::Add => Op::Add { a, b, c },
            ArithOp::Sub => Op::Sub { a, b, c },
            ArithOp::Mul => Op::Mul { a, b, c },
            ArithOp::Div => Op::Div { a, b, c },
            ArithOp::Mod => Op::Mod { a, b, c },
            ArithOp::IDiv => Op::IDiv { a, b, c },
            ArithOp::Pow => Op::Pow { a, b, c },
            ArithOp::BAnd => Op::BAnd { a, b, c },
            ArithOp::BOr => Op::BOr { a, b, c },
            ArithOp::BXor => Op::BXor { a, b, c },
            ArithOp::Shl => Op::Shl { a, b, c },
            ArithOp::Shr => Op::Shr { a, b, c },
        }
    }
}

pub(crate) fn arith_op(op: BinOp) -> Option<ArithOp> {
    Some(match op {
        BinOp::Add => ArithOp::Add,
        BinOp::Sub => ArithOp::Sub,
        BinOp::Mul => ArithOp::Mul,
        BinOp::Div => ArithOp::Div,
        BinOp::IDiv => ArithOp::IDiv,
        BinOp::Mod => ArithOp::Mod,
        BinOp::Pow => ArithOp::Pow,
        BinOp::BAnd => ArithOp::BAnd,
        BinOp::BOr => ArithOp::BOr,
        BinOp::BXor => ArithOp::BXor,
        BinOp::Shl => ArithOp::Shl,
        BinOp::Shr => ArithOp::Shr,
        _ => return None,
    })
}

pub(crate) fn block_ends_in_jump(b: &Block) -> bool {
    matches!(b.stats.last().map(|s| &s.kind), Some(StatKind::Return(_) | StatKind::Break | StatKind::Goto(..)))
}

/// Whether `l op r` is just `r` because `l` is a constant: a true constant before `and`,
/// `nil` or `false` before `or`.
pub(crate) fn decided_by_constant(op: BinOp, l: &Expr) -> bool {
    match op {
        BinOp::And => matches!(l.kind, ExprKind::True | ExprKind::Int(_) | ExprKind::Num(_) | ExprKind::Str(_)),
        BinOp::Or => matches!(l.kind, ExprKind::Nil | ExprKind::False),
        _ => false,
    }
}
