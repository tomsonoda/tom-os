//! The virtual machine: the execution loop, calls, metamethods, errors, coroutines and the
//! collector's driver.
//!
//! Lua-to-Lua calls, `pcall`, metamethods written in Lua and coroutine switches all happen
//! inside one loop without growing the Rust stack, so scripts can recurse deeply, yield
//! from inside `pcall` and metamethods, and the fuel counter sees every step. Rust
//! re-enters the loop only for library functions that call back into Lua (`table.sort`'s
//! comparator, `string.gsub`'s replacement function, `__tostring`, `__close`), and that
//! nesting has its own limit.

#[allow(unused_imports)]
use crate::nostd::prelude::*;
use crate::nostd::sync::{Arc, Weak};

use crate::bytecode::{ArithOp, Instr, MULTI, Proto, Rk};
use crate::heap::{CoStatus, Func, Heap, Pending, STEP_SIZE, ThreadState, UPVAL_SIZE, Upval};
use crate::number;
use crate::table::SetError;
use crate::value::{FuncRef, StrRef, TableRef, ThreadRef, Value};
use crate::{LimitKind, Limits};

/// A library function written in Rust.
pub(crate) type NativeFn = fn(&mut Lua, Args) -> Result<Flow, LuaError>;

/// Where a native call's arguments are: `n` values from stack slot `base`.
#[derive(Clone, Copy, Debug)]
pub struct Args {
    pub(crate) func: usize,
    pub(crate) base: usize,
    pub(crate) n: usize,
}

/// What a native function asks the machine to do when it returns.
pub(crate) enum Flow {
    /// Return the values in `rets`.
    Return,
    /// `pcall`/`xpcall`: call the first argument with the rest, protected.
    PCall { handler: Option<Value>, nargs: usize },
    /// `coroutine.yield` with the values in `rets`.
    Yield,
    /// `coroutine.resume` (or a `wrap` function) of `co` with the values in `rets`.
    Resume { co: ThreadRef, wrap: bool },
    /// Call `f(arg)` in the native's place, its results cut to three (`pairs` calling
    /// `__pairs`: C Lua's `lua_callk`, so it may yield).
    Call3 { f: Value, arg: Value },
}

/// What went wrong while running (see `LuaError`).
#[derive(Debug)]
pub(crate) enum ErrorKind {
    /// A raised value (message strings already carry their position).
    Value(Value),
    /// A message that gets the position of the running Lua function.
    Msg(String),
    /// A message raised by the machine while a Rust function runs: no position (as C
    /// Lua's errors raised inside C functions).
    Plain(String),
    /// `attempt to {op} a {ty} value`, with what the operand was (see `annotate`).
    Type { op: &'static str, ty: String, operand: u8 },
    /// `attempt to call a {ty} value`.
    Call { ty: String },
    /// `bad argument #{n} to '{name}' ({msg})`.
    Arg(usize, String),
    /// `number{info} has no integer representation`.
    IntRepr { operand: u8 },
    /// A limit: not catchable.
    Limit(LimitKind),
    /// Not an error: the call paused at a collector step at the end of the host's poll,
    /// its frames kept (`Lua::pending_call`, SEC7-G1). Nothing catches it.
    Pending,
}

/// What `Lua::settle_key` found for a key.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Settled {
    /// Nothing to do (not a long string, or its interned copy known).
    Plain,
    /// Its interned copy (now known).
    Found,
    /// No interned string equals it: the one it could have equaled and the first byte
    /// where they differ (None: there was none).
    Absent(Option<(u32, usize)>),
}

/// An error while running: one word, so results carrying it come back in registers (at
/// 48 bytes, each call site's result took a stack slot of its own: `Lua::run`'s frame was
/// 6 KiB, which bounded nested host calls to 59 levels in `Limits::stack_bytes`).
#[derive(Debug)]
pub(crate) struct LuaError(Box<ErrorKind>);

const _: () = assert!(crate::nostd::mem::size_of::<Result<Value, LuaError>>() == 16);

/// A call from the host paused at a collector step: where its results go, the thread and
/// depth it returns at, whether a native's scratch keeps them, and its native top.
#[derive(Clone, Copy, Debug)]
pub(crate) struct PendingCall {
    pos: usize,
    bthread: ThreadRef,
    bdepth: usize,
    keep: bool,
    ntop: usize,
}

impl From<ErrorKind> for LuaError {
    #[cold]
    fn from(k: ErrorKind) -> Self {
        LuaError(Box::new(k))
    }
}

#[allow(non_snake_case, reason = "built as the `ErrorKind` variants they stand for")]
impl LuaError {
    pub(crate) fn msg(s: impl Into<String>) -> Self {
        ErrorKind::Msg(s.into()).into()
    }
    pub(crate) fn Value(v: Value) -> Self {
        ErrorKind::Value(v).into()
    }
    pub(crate) fn Msg(s: String) -> Self {
        ErrorKind::Msg(s).into()
    }
    pub(crate) fn Plain(s: String) -> Self {
        ErrorKind::Plain(s).into()
    }
    pub(crate) fn Arg(n: usize, s: String) -> Self {
        ErrorKind::Arg(n, s).into()
    }
    pub(crate) fn Limit(k: LimitKind) -> Self {
        ErrorKind::Limit(k).into()
    }
    pub(crate) fn kind(&self) -> &ErrorKind {
        &self.0
    }
    pub(crate) fn into_kind(self) -> ErrorKind {
        *self.0
    }
    /// As raised from Rust: a message without a position (`Msg` becomes `Plain`).
    pub(crate) fn plain(mut self) -> Self {
        if let ErrorKind::Msg(m) = &mut *self.0 {
            *self.0 = ErrorKind::Plain(crate::nostd::mem::take(m));
        }
        self
    }
    /// The limit it is, if it is one (not catchable).
    /// The call paused (`ErrorKind::Pending`).
    pub(crate) fn is_pending(&self) -> bool {
        matches!(*self.0, ErrorKind::Pending)
    }

    pub(crate) fn limit(&self) -> Option<LimitKind> {
        match *self.0 {
            ErrorKind::Limit(k) => Some(k),
            _ => None,
        }
    }
}

/// Metamethod events.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub(crate) enum Tm {
    Index = 0,
    NewIndex,
    Gc,
    Mode,
    Len,
    Eq,
    Add,
    Sub,
    Mul,
    Mod,
    Pow,
    Div,
    IDiv,
    BAnd,
    BOr,
    BXor,
    Shl,
    Shr,
    Unm,
    BNot,
    Lt,
    Le,
    Concat,
    Call,
    Close,
    ToString,
    Name,
    Metatable,
    Pairs,
}

pub(crate) const TM_NAMES: [&str; 29] = [
    "__index",
    "__newindex",
    "__gc",
    "__mode",
    "__len",
    "__eq",
    "__add",
    "__sub",
    "__mul",
    "__mod",
    "__pow",
    "__div",
    "__idiv",
    "__band",
    "__bor",
    "__bxor",
    "__shl",
    "__shr",
    "__unm",
    "__bnot",
    "__lt",
    "__le",
    "__concat",
    "__call",
    "__close",
    "__tostring",
    "__name",
    "__metatable",
    "__pairs",
];

fn arith_tm(op: ArithOp) -> Tm {
    match op {
        ArithOp::Add => Tm::Add,
        ArithOp::Sub => Tm::Sub,
        ArithOp::Mul => Tm::Mul,
        ArithOp::Mod => Tm::Mod,
        ArithOp::Pow => Tm::Pow,
        ArithOp::Div => Tm::Div,
        ArithOp::IDiv => Tm::IDiv,
        ArithOp::BAnd => Tm::BAnd,
        ArithOp::BOr => Tm::BOr,
        ArithOp::BXor => Tm::BXor,
        ArithOp::Shl => Tm::Shl,
        ArithOp::Shr => Tm::Shr,
    }
}

fn is_bitwise(op: ArithOp) -> bool {
    matches!(op, ArithOp::BAnd | ArithOp::BOr | ArithOp::BXor | ArithOp::Shl | ArithOp::Shr)
}

/// An error raised at C level (by a protected call, or a call from Rust, itself): a call
/// error then carries no position.
fn c_level(e: LuaError) -> LuaError {
    match e.into_kind() {
        ErrorKind::Call { ty } => LuaError::Plain(format!("attempt to call a {ty} value")),
        other => other.into(),
    }
}

/// The results a generic `for`'s iterator call wants: `c`, or all of them (MULTI, for more
/// than a 16-bit count: `PadNil` adjusts them).
#[inline]
fn tfor_wanted(c: u16) -> i32 {
    if c == MULTI { -1 } else { i32::from(c) }
}

/// What happens with a metamethod's result when the Lua function computing it returns.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Cont {
    /// Store it in a stack slot.
    Store(usize),
    /// Ignore it.
    Discard,
    /// A comparison: skip the caller's next instruction unless truthy(result) == k.
    Cond { k: bool },
    /// Concatenation in progress over `n` values from slot `a`.
    Concat { a: usize, n: usize },
    /// Closing to-be-closed variables down to `level` on a normal exit; then `then`.
    Closing { level: usize, then: AfterClose },
    /// `__pairs` called by `pairs` (`Flow::Call3`): its results in the frame's place, cut
    /// to three, then adjusted to `wanted`.
    Three { wanted: i32 },
    /// A `__close` run while an error unwinds to the protected call of frame `frame`
    /// returned: the next one, then the protected call's `false, error`.
    ErrClosing { frame: usize },
    /// The innermost of `levels + 1` nested protected calls (`pcall(pcall, f)`: each one's
    /// callee is the next; S5COR-6b) finished, its status and results from slot
    /// `dest + levels` up to `top` (its `wanted` is always -1): each enclosing one puts its
    /// `true` in the slot below (their slots are `dest..dest + levels`, one above the
    /// other), and the outermost's results go to `dest`, adjusted to `wanted`. One frame
    /// (or pending call) carries them all, so a yield inside works, as in C Lua, and the
    /// depth costs no native stack.
    ///
    /// Two things may wrap it (S5COR-6c): `three`, the innermost callee is a Lua `__pairs`
    /// that `pairs` called (`pcall(pcall, pairs, t)`), whose results are cut to three
    /// first; and `then`, the outermost call is a metamethod (`pcall` as `__add`, say, or
    /// `pairs` as `__index`, with `levels` 0 and no protected call of its own when the
    /// frame is a `FrameKind::Cont`), whose continuation then gets the outermost's first
    /// result: `then` with `back` and `n` is that continuation, packed (`pack_then`), and
    /// `levels`, `then` and `three` share a word (`NestBits`), so a frame keeps its size.
    Nest { dest: usize, bits: NestBits, wanted: i32, back: u32, n: u32 },
}

/// A metamethod's continuation after a `Cont::Nest`, packed with its `back` and `n`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Then {
    None = 0,
    Store = 1,
    Discard = 2,
    CondFalse = 3,
    CondTrue = 4,
    Concat = 5,
}

/// A `Cont::Nest`'s `levels` (below 2^27), `then` and `three`, in one word: bit 0 is
/// `three`, bits 1-3 `then`, the rest `levels`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct NestBits(u32);

impl NestBits {
    /// None past 2^27 - 1 levels (2^27 protected calls one above the other need 2 GiB of
    /// stack); the call then runs nested, once per 2^27 levels.
    fn new(levels: u32, then: Then, three: bool) -> Option<NestBits> {
        (levels < 1 << 27).then_some(NestBits(levels << 4 | (then as u32) << 1 | u32::from(three)))
    }
    #[inline]
    pub(crate) fn levels(self) -> u32 {
        self.0 >> 4
    }
    #[inline]
    pub(crate) fn three(self) -> bool {
        self.0 & 1 != 0
    }
    #[inline]
    pub(crate) fn then(self) -> Then {
        match (self.0 >> 1) & 7 {
            1 => Then::Store,
            2 => Then::Discard,
            3 => Then::CondFalse,
            4 => Then::CondTrue,
            5 => Then::Concat,
            _ => Then::None,
        }
    }
    fn with_levels(self, levels: u32) -> Option<NestBits> {
        NestBits::new(levels, self.then(), self.three())
    }
    fn with_three(self) -> NestBits {
        NestBits(self.0 | 1)
    }
}

/// `c`, the continuation of a metamethod called at `dest`, as a `Cont::Nest`'s `then`,
/// `back` (its slot, `dest - back`: a register of the frame below) and `n`. None for a
/// continuation that is not a metamethod's, or one too far below to pack (which a frame's
/// registers never are).
fn pack_then(c: Cont, dest: usize) -> Option<(Then, u32, u32)> {
    let back = |slot: usize| dest.checked_sub(slot).and_then(|d| u32::try_from(d).ok());
    match c {
        Cont::Store(slot) => Some((Then::Store, back(slot)?, 0)),
        Cont::Discard => Some((Then::Discard, 0, 0)),
        Cont::Cond { k } => Some((if k { Then::CondTrue } else { Then::CondFalse }, 0, 0)),
        Cont::Concat { a, n } => Some((Then::Concat, back(a)?, u32::try_from(n).ok()?)),
        _ => None,
    }
}

/// The continuation `pack_then` packed.
fn unpack_then(then: Then, dest: usize, back: u32, n: u32) -> Option<Cont> {
    let slot = dest.saturating_sub(back as usize);
    match then {
        Then::None => None,
        Then::Store => Some(Cont::Store(slot)),
        Then::Discard => Some(Cont::Discard),
        Then::CondFalse => Some(Cont::Cond { k: false }),
        Then::CondTrue => Some(Cont::Cond { k: true }),
        Then::Concat => Some(Cont::Concat { a: slot, n: n as usize }),
    }
}

impl Cont {
    /// Whether the machine called a metamethod for an instruction with this continuation
    /// (for `debug.getinfo`'s "metamethod" names).
    pub(crate) fn is_metamethod(self) -> bool {
        matches!(self, Cont::Store(_) | Cont::Discard | Cont::Cond { .. } | Cont::Concat { .. } | Cont::Closing { .. })
    }
}

/// What a normal-exit close goes on with.
#[derive(Clone, Copy, Debug)]
pub(crate) enum AfterClose {
    /// Nothing (a block's end or a jump out of it).
    Nothing,
    /// The frame returns `n` results from `src`.
    Return { src: usize, n: usize },
}

// `Cont::Nest` fits in the continuation as it was: a frame stays 136 bytes (64-bit).
const _: () = assert!(crate::nostd::mem::size_of::<Cont>() <= 32 && crate::nostd::mem::size_of::<FrameKind>() <= 48);
// The tail-call flag fits in the frame's padding, and the record is 128 bytes: a frame
// is found by a shift (at 136 it took a shift and an add, on every frame switch).
const _: () = assert!(crate::nostd::mem::size_of::<Frame>() <= 128);

/// The continuation that completes the protected call of kind `kind` whose callee, at
/// `func`, is a protected call too, and every protected call enclosing it (S5COR-6b), and
/// then the metamethod's continuation the outermost one's status goes to (S5COR-6c). None
/// when its slots are not one above the other (which the call protocol never does) or its
/// continuation is not one a `Cont::Nest` carries.
fn nest_of(kind: FrameKind, func: usize) -> Option<Cont> {
    let FrameKind::Protected { dest, wanted, cont } = kind else { return None };
    let nest = match cont {
        Some(Cont::Nest { dest, bits, wanted, back, n }) if !bits.three() => {
            Cont::Nest { dest, bits: bits.with_levels(bits.levels().checked_add(1)?)?, wanted, back, n }
        }
        None => Cont::Nest { dest, bits: NestBits::new(1, Then::None, false)?, wanted, back: 0, n: 0 },
        Some(c) => {
            let (then, back, n) = pack_then(c, dest)?;
            Cont::Nest { dest, bits: NestBits::new(1, then, false)?, wanted, back, n }
        }
    };
    let Cont::Nest { dest, bits, .. } = nest else { return None };
    (dest.checked_add(bits.levels() as usize) == Some(func)).then_some(nest)
}

/// The protected calls a call of kind `kind` sits on: one for a protected call, one more
/// for each enclosing one it completes (`Cont::Nest`).
#[inline]
pub(crate) fn pcall_levels(kind: FrameKind) -> usize {
    match kind {
        FrameKind::Protected { cont: Some(Cont::Nest { bits, .. }), .. } => 1 + bits.levels() as usize,
        FrameKind::Protected { .. } => 1,
        _ => 0,
    }
}

/// A Lua `__pairs` that `pairs` called: its frame sits on `pairs`, a Rust activation (for
/// error levels) and a C level (`lua_callk` in C Lua).
#[inline]
fn under_pairs(kind: FrameKind) -> bool {
    match kind {
        FrameKind::Cont(Cont::Three { .. }) | FrameKind::Protected { cont: Some(Cont::Three { .. }), .. } => true,
        FrameKind::Cont(Cont::Nest { bits, .. }) | FrameKind::Protected { cont: Some(Cont::Nest { bits, .. }), .. } => bits.three(),
        _ => false,
    }
}

/// The C levels (`nCcalls` in C Lua) a frame or native call of kind `kind` that Rust
/// started adds: each protected call (`pcall` calls from C), the metamethod call (C Lua's
/// `luaT_callTMres`), and `pairs`'s call of `__pairs` (S5COR-6c: `pcall(pairs, t)` and
/// `pcall` as a metamethod each counted one level short).
#[inline]
pub(crate) fn c_calls(kind: FrameKind) -> usize {
    let meta = match kind {
        FrameKind::Cont(Cont::Three { .. }) | FrameKind::Normal => false,
        FrameKind::Cont(Cont::Nest { bits, .. }) => bits.then() != Then::None,
        FrameKind::Cont(_) => true,
        FrameKind::Protected { cont: Some(Cont::Nest { bits, .. }), .. } => bits.then() != Then::None,
        FrameKind::Protected { cont, dest, .. } => cont.is_some_and(|c| pack_then(c, dest).is_some()),
    };
    pcall_levels(kind) + usize::from(meta) + usize::from(under_pairs(kind))
}

/// The frames an error's position skips, for [`Lua::where_`] walking down from the top:
/// those strictly between a protected call `lo` an error unwinds to and the `__close` it
/// runs above them, at `hi`. An interval counts once the walk is below its `hi`.
struct Dead {
    /// `err_closing`'s interval, until the walk is below its `hi`.
    ec: Option<(usize, usize)>,
    /// The least `lo` of the intervals counting: the frames above it are gone.
    min_lo: usize,
    /// The `lo`s counting, at or below the walk (the greatest on top).
    los: crate::nostd::collections::BinaryHeap<usize>,
}

impl Dead {
    /// An interval whose `hi` the walk passed.
    fn add(&mut self, lo: usize) {
        self.min_lo = self.min_lo.min(lo);
        self.los.push(lo);
    }

    /// The walk is at frame `t`: `err_closing`'s interval counts if its `hi` is above.
    fn activate(&mut self, t: usize) {
        if let Some((lo, hi)) = self.ec
            && hi > t
        {
            self.ec = None;
            self.add(lo);
        }
    }

    fn gone(&self, t: usize) -> bool {
        self.min_lo < t
    }

    /// Frame `t` is a protected call being unwound to (`t` never grows between calls).
    fn is_lo(&mut self, t: usize) -> bool {
        while self.los.peek().is_some_and(|&l| l > t) {
            self.los.pop();
        }
        self.los.peek() == Some(&t)
    }
}

/// Passes `k` Rust activations (no position) in [`Lua::where_`]'s walk: the position
/// (empty) when the level asked for is one of them.
fn pass_rust(back: &mut Option<usize>, level: usize, k: usize) -> Option<String> {
    let b = *back.get_or_insert(level);
    match b.checked_sub(k) {
        Some(left) => {
            *back = Some(left);
            None
        }
        None => Some(String::new()),
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) enum FrameKind {
    #[default]
    Normal,
    /// Called by `pcall`: results go to `dest` after `true`; a `pcall` used as a
    /// metamethod passes its status on through `cont`.
    Protected { dest: usize, wanted: i32, cont: Option<Cont> },
    Cont(Cont),
}

/// A Lua function's activation.
pub(crate) struct Frame {
    pub closure: FuncRef,
    /// The prototype (in the heap). (The end of its registers is not kept: it is always
    /// `base + frame_size`, see `Lua::frame_top`.)
    pub proto: u32,
    /// Stack slot of the function: results go here.
    pub func: usize,
    /// Register 0.
    pub base: usize,
    pub pc: usize,
    pub nresults: i32,
    pub vstart: usize,
    pub vcount: usize,
    pub kind: FrameKind,
    /// Called from a Lua function (not from Rust).
    pub from_lua: bool,
    /// Entered by a tail call (`debug.getinfo`'s `istailcall`; it has no call-site name).
    pub tail: bool,
    /// `xpcall`'s message handler.
    pub handler: Option<Value>,
}

/// One activation of a thread (`Lua::activations`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Act {
    /// A Lua frame (its index).
    Lua(usize),
    /// A Rust function: the stack slot that holds it, when known.
    C(Option<usize>),
    /// Where Rust started the thread's frames (a call from the host, a coroutine's body):
    /// a level for error positions, no activation in a traceback.
    Base,
}

/// A run of activations, walked newest first (`Lua::walk_activations`).
#[derive(Clone, Copy, Debug)]
enum Run {
    /// A Lua frame (its index).
    Lua(usize),
    /// `count` Rust functions, in slots `slot..slot + count` (oldest first), when known.
    C { slot: Option<usize>, count: usize },
    /// Where Rust started the thread's frames ([`Act::Base`]).
    Base,
}

pub(crate) enum Called {
    /// A Lua frame was pushed: run it.
    Lua,
    /// A native function finished; results are in place.
    Done,
    /// The running thread changed.
    Switched,
}

/// A protected call's callee to call next (`Lua::nested_pending`): its slot, its arguments,
/// the kind that completes every enclosing protected call, and the innermost one's
/// message handler.
#[derive(Clone, Copy)]
pub(crate) struct NestedCall {
    pub func: usize,
    pub nargs: usize,
    pub kind: FrameKind,
    pub handler: Option<Value>,
}

/// A frame's register window: registers are 8-bit numbers.
pub(crate) use crate::bytecode::{WIDE_WINDOW, WINDOW};
/// Room above a frame's registers for metamethod calls and native arguments.
pub(crate) const EXTRA_STACK: usize = 24;

/// Spare coroutine stacks kept (`Lua::spare_stacks`), and the largest kept, in slots.
const SPARE_STACKS: usize = 8;
const SPARE_STACK_SLOTS: usize = 2048;
/// Bytes a stack slot holds.
const SLOT_BYTES: usize = crate::table::SLOT_SIZE;
/// Fuel a coroutine's first resume costs for the memory it maps (LR-8): its record and its
/// register window, a unit per 128 bytes (37). Fixed, so the same whether a spare stack is
/// reused or not, with the fast loop or not, under any hash seed.
pub(crate) const COROUTINE_COST: u64 =
    ((crate::heap::THREAD_BASE + (256 + EXTRA_STACK + 1) * SLOT_BYTES) / 128) as u64;
/// Most `__index`/`__newindex` links followed, and `__call` links.
const MAX_TAG_LOOP: usize = 2000;
/// Longest `__name` used in messages and `tostring`.
const MAX_NAME: usize = 256;
/// The fuel handed out at a time: how many steps may pass between looks at the host's
/// interrupt and deadline (a fraction of a millisecond of work).
/// Fuel for each link of an `__index` or `__newindex` chain (and the probing of its lookup
/// besides, S5GC-1). A link reads the metatable, finds its `__index` and looks the key up
/// in another table: about two instructions' work, and the loads of different tables, not
/// in the caches for a long chain. At 1, keys aimed at the worst layout (a known hash
/// seed; S5B-OPEN-2's residual) in 1,999 tables of 2^9 slots on one chain ran at 8.3x
/// plain code per unit of fuel with the fast loop and 6.5x without, against 4.2x and 3.4x
/// for the same tables looked up directly. The count is the program's, never the seed's.
pub(crate) const LINK_COST: u64 = 2;

/// Links of one access charged `LINK_COST`; each past them costs `DEEP_LINK_COST`. A class
/// hierarchy is a few links deep; the amplifier of S5B-OPEN-2's residual is a long chain
/// through tables not in the caches, a cache miss or more per link: at 2 units a link the
/// worst layout still ran at 5.3x plain code per unit of fuel here (fast loop; 4.0x
/// without) and up to 9x on slower memory.
pub(crate) const SHALLOW_LINKS: usize = 4;
pub(crate) const DEEP_LINK_COST: u64 = 8;

/// The fuel for link `n` (from 0) of one `__index` or `__newindex` chain walk.
#[inline]
pub(crate) const fn link_cost(n: usize) -> u64 {
    if n < SHALLOW_LINKS { LINK_COST } else { DEEP_LINK_COST }
}

pub(crate) const FUEL_SLICE: i64 = 1 << 14;

/// Fuel for making an error message (its position and text; calibrated with
/// `examples/fuelcal.rs`), on top of what naming its variables reads.
const ERROR_FUEL: u64 = 24;

/// Collector work units per fuel unit.
pub(crate) const WORK_PER_FUEL: usize = 4;

/// Steps in Rust functions in a row that leave the atomic phase for a step at an
/// instruction (`Lua::atomic_deferred`).
const ATOMIC_DEFERRALS: u32 = 16;
/// Collector work a full collection does between charges (and looks at the interrupt).
const FULL_GC_CHUNK: usize = 1 << 14;

/// Fuel for writing a float as text (`%.14g` and friends cost about as much as eight
/// instructions; calibrated: `examples/fuelcal.rs`).
pub(crate) const FLOAT_TEXT_FUEL: u64 = 8;

/// What runs scripts: a RemedyLua state with its globals, heap and limits. One thread
/// uses it at a time; it may move between threads (`Send`).
pub struct Lua {
    pub(crate) heap: Heap,
    pub(crate) cur: Box<ThreadState>,
    pub(crate) cur_ref: ThreadRef,
    pub(crate) main_ref: ThreadRef,
    /// End of the last variable-count result list.
    pub(crate) top: usize,
    /// While a native runs: the first free stack slot above its arguments.
    pub(crate) ntop: usize,
    pub(crate) rets: Vec<Value>,
    pub(crate) globals: TableRef,
    pub(crate) registry: TableRef,
    pub(crate) string_meta: Option<TableRef>,
    pub(crate) tm_names: Vec<StrRef>,
    /// Bumped whenever something the fast loop's context copies changes (the limits, the
    /// string metatable, the library functions it runs in place): `run` makes its
    /// context again then, and only then.
    pub(crate) ctx_epoch: u64,
    /// The pattern matcher's state, kept between `find`, `match` and `gmatch` calls (made
    /// afresh it was 700 bytes to set up per call; see `MatchState::reuse`).
    pub(crate) match_state: Option<Box<crate::stdlib::pattern::MatchState>>,
    /// Stacks of coroutines that were closed or finished, for new coroutines to reuse
    /// (`retire_stack`, LR-8). Bounded (`SPARE_STACKS`, `SPARE_STACK_SLOTS`, a sixteenth of
    /// the memory limit), counted in `Mem::used` as slack while kept (`Mem::slack_in`),
    /// emptied by full collections.
    pub(crate) spare_stacks: Vec<Vec<Value>>,
    pub(crate) fuel: i64,
    /// Fuel left when a limit stopped the call from the host (`fuel` itself is then spent,
    /// so nothing runs on): what `fuel_left` reports for it.
    pub(crate) stopped_left: Option<u64>,
    pub(crate) limits: Limits,
    /// Execution loops entered from Rust and not yet left.
    pub(crate) c_depth: usize,
    pub(crate) native_depth: u32,
    /// Calls through `call_native` (a test probe: `Lua::native_calls`).
    pub(crate) native_calls: u64,
    pub(crate) native_from_lua: bool,
    /// The metamethod event the running native was called for (`call_meta`), which names
    /// it in argument errors as C Lua names it (`bad argument #1 to 'index'`).
    pub(crate) native_event: Option<&'static str>,
    /// The event `call_meta` is about to call a metamethod for, taken by `call_native`.
    pub(crate) pending_event: Option<&'static str>,
    /// The native running now (for error messages).
    pub(crate) native_name: &'static str,
    /// Where the native stack was when the host called in (see `Limits::stack_bytes`).
    pub(crate) stack_base: usize,
    /// Coroutines resumed and not yet back (each resumed from inside another).
    pub(crate) resume_depth: usize,
    /// Natives running directly under `pcall` (`pcall(string.gsub, ...)`), which have no
    /// frame of their own: C levels for `call_handler` (S5COR-5).
    pub(crate) protected_natives: usize,
    /// Collector work done but not yet a whole unit of fuel (`gc_step`): carried to the
    /// next step, so the fuel charged is the work done over `WORK_PER_FUEL`, however a
    /// cycle's work splits into steps (S5C-1).
    pub(crate) gc_work_carry: usize,
    /// Marking work done but not yet charged (a step stops after the object that passes
    /// its budget): each unfinished marking step charges exactly its budget and keeps the
    /// rest here, and marking counts as finished only once what is left fits in a step, so
    /// how marking splits into steps follows the total work alone, never the order objects
    /// are reached in (which follows the hash seed: S6GC-2 made that visible).
    pub(crate) gc_mark_over: usize,
    /// Atomic-phase work done past a call's fuel, owed by the call that resumes it.
    pub(crate) atomic_owed: usize,
    /// Test probe (SEC8-F7): collector work units owed past a call's fuel (`owe_gc`) since
    /// the state was made, and those of them a cycle's start charged (a carry left when the
    /// last cycle ended).
    pub(crate) gc_owed_probe: (u64, u64),
    /// A marking step paused at the end of the host's poll (SEC7-G1): what it started
    /// with (the objects made since the last step's part, its budget whole, its budget),
    /// so `gc_step` goes on with it, inside its loop (`Heap::propagate_resume`), and ends
    /// it as it would have ended.
    pub(crate) gc_pending: Option<(usize, usize, usize)>,
    /// The call from the host paused with it, its frames kept.
    pub(crate) pending_call: Option<PendingCall>,
    /// The paused instruction runs again on resume, without the step it took.
    pub(crate) due_skip: bool,
    /// A step paused inside its atomic phase: the work the step had done before it, and
    /// the allowance it gave it, so it ends as it would have (SEC7-G1).
    pub(crate) gc_atomic_pre: Option<(usize, usize)>,
    /// The atomic phase not started in a step inside a Rust function, times in a row: it
    /// starts at a step at an instruction, where the call can pause (SEC7-G1), or after
    /// `ATOMIC_DEFERRALS` such steps (a script whose allocations all come from Rust
    /// functions still ends its cycles).
    pub(crate) atomic_deferred: u32,
    /// A call from the host paused before it started (resuming a suspended atomic phase
    /// at its entry, `resume_atomic`): the function and its arguments.
    pub(crate) pending_start: Option<(Value, Vec<Value>)>,
    /// `collectgarbage('step')` called from Lua code: its step ran at the call (the cycle
    /// before it), for the function to answer from (`collect_step`, SEC7-G1).
    pub(crate) gc_call_before: Option<u32>,
    /// The paused instruction was set to run again, its fetch's unit given back: when the
    /// step it paused in ends the call instead (a limit), the unit is taken back, as the
    /// instruction does not run again (SEC7-G1).
    pub(crate) rewound: bool,
    /// While a native `__close` runs as an error unwinds to the protected call of this
    /// frame, called with this many frames: the frames between are gone for error levels,
    /// as in C Lua (S5C-COR-3); a callable `__close`'s own frame above them is not (S6-6).
    pub(crate) err_closing: Option<(usize, usize)>,
    /// A protected call whose callee is a protected call in turn (`pcall(pcall, f)`)
    /// leaves that callee's own callee here and returns `Called::Done`: `precall` goes on
    /// with it, in a loop, so the depth costs no native stack (S5COR-6b).
    pub(crate) nested_pending: Option<NestedCall>,
    /// Natives in progress: their thread, how many Lua frames it had when they were
    /// called (for error positions by level), each one's stack slot (for
    /// `debug.traceback` and `debug.getinfo`), and the protected calls it sits on, which
    /// are activations too, in the slots just below (a native called by `pcall`; S6-2: a
    /// run of 100,000 nested ones took an entry each, at each level).
    pub(crate) native_stack: Vec<(ThreadRef, usize, usize, usize)>,
    /// The native whose error was raised last (its `native_stack` record) and that error.
    /// (With the protected calls it sat on, as `call_native` counts them.)
    pub(crate) err_native: Option<((ThreadRef, usize, usize), usize, Value)>,
    /// While a message handler runs: the native that raised the error it handles, and
    /// the protected calls it sat on.
    pub(crate) handler_native: Option<((ThreadRef, usize, usize), usize)>,
    /// S6-7: while a message handler runs (`call_handler`), its thread, and C Lua's C
    /// levels where the error escaping it was raised (`on_error` sees the raise with the
    /// frames still there). C Lua calls the handler again at that point (`luaG_errormsg`),
    /// so an error raised inside a metamethod or a library callback costs it more levels.
    pub(crate) handler_raise: HandlerWatch,
    pub(crate) roots: Vec<(Weak<()>, Value)>,
    /// The fast loop is on (a test switch turns it off to check it against `run` alone).
    pub(crate) fast: bool,
    /// Instructions the full path ran while the fast loop was on (see `Lua::full_steps`).
    pub(crate) full_steps: u64,
    /// Length of `roots` after it was last pruned of dropped handles.
    pub(crate) roots_pruned_at: usize,
    /// Objects natives made and still hold, kept alive until they return.
    pub(crate) scratch: Vec<Value>,
    /// Values the host made or got outside a call (`create_*`, `from_data`, `get`, a call's
    /// results), kept alive until the next call from the host returns.
    pub(crate) held: Vec<Value>,
    /// Where the running native's own part of `scratch` starts. Only that part is compacted
    /// (`push_scratch`), so positions below it (a caller's, `sort`'s root slots) stay put.
    pub(crate) scratch_floor: usize,
    /// The length of `scratch` at which its running part is next compacted.
    pub(crate) scratch_compact_at: usize,
    /// The length of `held` at which it is next compacted.
    pub(crate) held_compact_at: usize,
    /// The results of the last `Lua::call` made inside each running host function (the
    /// innermost one's from `host_rets_base`), kept until that function's next call
    /// (S5API-3: `rets` is the natives' return channel, cleared by any native call).
    pub(crate) host_rets: Vec<Value>,
    pub(crate) host_rets_base: usize,
    pub(crate) libs: crate::Libs,
    pub(crate) rng: [u64; 4],
    /// `print` output not taken yet (when no sink is set).
    pub(crate) output: Vec<u8>,
    /// Bytes `print` and `warn` wrote in the current call from the host.
    pub(crate) output_used: usize,
    /// Bytes of `output`'s buffer charged to the memory account (until `take_output`).
    pub(crate) output_charged: usize,
    /// Whether `warn` writes: off until a script sends "@on", as in `luaL_newstate`.
    pub(crate) warn_on: bool,
    /// The collector mode a script last asked for (only the incremental collector exists).
    pub(crate) gc_generational: bool,
    /// The last error object that left the state, with its text (see `from_host_error`).
    pub(crate) last_error: Option<(String, Value)>,
    /// A limit that stopped a call: later steps of the same call from the host fail with
    /// it too, even when a host function swallowed the error.
    pub(crate) limit_hit: Option<LimitKind>,
    /// Fuel not yet given to `fuel`: the budget is handed out a slice at a time, and between
    /// slices the interrupt and the deadline are checked (see `refuel`).
    pub(crate) reserve: i64,
    /// The host's interrupt: set from any thread, it stops the running call.
    pub(crate) interrupt: crate::nostd::sync::Arc<crate::nostd::sync::atomic::AtomicBool>,
    /// A clock and how long one call from the host may run by it.
    pub(crate) time_limit: Option<(crate::Clock, crate::nostd::time::Duration)>,
    /// When the running call must stop, by `time_limit`'s clock.
    pub(crate) deadline: Option<crate::nostd::time::Duration>,
    /// The host's own data, by type.
    pub(crate) app_data: crate::nostd::collections::HashMap<crate::nostd::any::TypeId, Box<dyn crate::nostd::any::Any + Send>>,
    pub(crate) print_sink: Option<Box<dyn FnMut(&str) + Send>>,
    /// Host calls in progress (fuel resets only at the outermost).
    pub(crate) entry: u32,
    /// A panic (a host function's, or a bug) unwound through a call: the machine may be
    /// mid-instruction, so every later call is refused.
    pub(crate) poisoned: bool,
    /// What compiles left to free a piece at a time, each with the bytes it is charged.
    pub(crate) deferred: crate::diag::Deferred,
    /// Traceback of the last error that left the machine, if one did: its frames, top
    /// first (at most 22), as an index in `trace_sources`, the line, and the line the
    /// function starts on. S6GC-H1: noted at each error, made into text only for the host
    /// ([`Lua::take_traceback`]), as most errors are caught.
    pub(crate) last_trace: Option<Vec<(usize, u32, u32)>>,
    /// The chunks `last_trace` names.
    pub(crate) trace_sources: Vec<Arc<str>>,
    /// The clocks of `os.time`/`os.date` (time since the Unix epoch) and `os.clock` (time
    /// the program has run); `None` reads as 0 (see `Lua::set_os_clocks`).
    pub(crate) os_clocks: (Option<crate::Clock>, Option<crate::Clock>),
    /// Seconds east of UTC for `os.date` local time.
    pub(crate) utc_offset: i64,
    /// Interned strings the libraries use often.
    pub(crate) names: crate::stdlib::Names,
    /// Modules the host registered for `require` (`Lua::add_module`), by name.
    pub(crate) modules: crate::nostd::collections::HashMap<Vec<u8>, crate::ModuleSource>,
    /// What `os.getenv` reads (`Libs::ENV`, `Lua::set_env`): the host's map, never the
    /// process environment. Not counted against the memory limit (the host's copy).
    pub(crate) env: crate::nostd::collections::HashMap<Vec<u8>, crate::nostd::sync::Arc<[u8]>>,
    /// The longest name `env` has held: a longer one is looked up without being hashed.
    pub(crate) env_name_max: usize,
    /// `io`'s standard streams (stdin, stdout, stderr) as the host set them (`Libs::IO`);
    /// None while an operation holds one, or before the host set one (the default).
    pub(crate) io_std: [Option<crate::stdlib::io::FileState>; 3],
    /// What `io.open` asks for files (`Lua::set_file_opener`).
    pub(crate) io_opener: Option<crate::IoOpener>,
    /// The file handles' metatable (`FILE*`, in the registry too).
    pub(crate) io_meta: Option<TableRef>,
    /// The host's module loader for `require` (`Lua::set_module_loader`).
    pub(crate) module_loader: Option<crate::ModuleLoader>,
}

pub(crate) type Vm = Lua;

/// What `on_error` notes for `call_handler` (S6-7).
#[derive(Default, Clone)]
pub(crate) struct HandlerWatch {
    /// The thread the handler runs on (errors raised on others do not count).
    thread: Option<ThreadRef>,
    /// C Lua's C levels where the error escaping the handler was raised.
    raised: Option<usize>,
    /// For a round that may pass C Lua's 200th level: the handler's first frame and the
    /// levels above the handler's call where the 200th falls.
    cross: Option<(usize, usize)>,
    /// The position of the Lua function whose metamethod call fell on the 200th level
    /// (C Lua's "C stack overflow" there is raised at it).
    pos: Option<String>,
}

/// Frames of the resume chain walked (`c_levels`) per unit of fuel. It was 16, as
/// `charge_error` charges frames, while a global write left the fast loop; with global
/// writes in it, plain code that writes globals (the cost tests' control) runs 2.8x as
/// fast per unit, and 80 coroutines 1,900 frames deep under `xpcall` errors ran at 16x
/// it (S5B-WALK-4's bound is 12x). At 5: PERF-RECAL. At 1:
/// plain code kept getting faster in the fast loop (at 3 the test read up to 9.6x alone and
/// 13.4x under the suite's load), most of it from a resume's walk (`resume_refused`), which
/// still charged a unit per 16 frames; both walks are charged from this one rate.
const RESUMER_FRAMES_PER_FUEL: usize = 1;

/// C levels C Lua has in use where `xpcall`'s handler first runs from the top of a chunk,
/// beyond ours (`call_handler`).
const HANDLER_BASE_LEVELS: usize = 2;
/// C Lua's limit of C levels (`LUAI_MAXCCALLS`).
const C_HANDLER_LEVELS: usize = 200;
/// C levels a resume adds in C Lua (`lua_resume` and the resuming library function).
const RESUME_LEVELS: usize = 1;

/// The least length `push_scratch` and `push_held` let a list grow to before compacting it.
const COMPACT_MIN: usize = 1024;

/// Drops repeated objects and plain values from `v[from..]`, keeping the first copy of each
/// object, so every value that stays only moves down (marks into `v` above `from` still keep
/// what was below them): how many are left from `from`.
pub(crate) fn compact_values(v: &mut Vec<Value>, from: usize) -> usize {
    fn key(v: Value) -> Option<(u8, u32, u32)> {
        match v {
            Value::Str(r) => Some((0, r.idx, r.generation)),
            Value::Table(r) => Some((1, r.idx, r.generation)),
            Value::Func(r) => Some((2, r.idx, r.generation)),
            Value::UserData(r) => Some((4, r.idx, r.generation)),
            Value::Thread(r) => Some((5, r.idx, r.generation)),
            _ => None,
        }
    }
    let tail = v.split_off(from.min(v.len()));
    let mut seen = crate::nostd::collections::HashSet::with_capacity_and_hasher(tail.len(), MixHash);
    v.extend(tail.into_iter().filter(|x| key(*x).is_some_and(|k| seen.insert(k))));
    v.len() - from.min(v.len())
}


/// A cheap hasher for `compact_values`' keys (an object's kind, slot and generation, which
/// a script does not choose): a splitmix64 finish over the words written. SipHash made the
/// compaction cost about a hundred instructions per value kept.
#[derive(Clone, Copy, Default)]
struct MixHash;

impl crate::nostd::hash::BuildHasher for MixHash {
    type Hasher = MixHasher;
    fn build_hasher(&self) -> MixHasher {
        MixHasher(0)
    }
}

struct MixHasher(u64);

impl crate::nostd::hash::Hasher for MixHasher {
    fn finish(&self) -> u64 {
        let mut z = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = self.0.rotate_left(8) ^ u64::from(b);
        }
    }
    fn write_u8(&mut self, n: u8) {
        self.0 = self.0.rotate_left(8) ^ u64::from(n);
    }
    fn write_u32(&mut self, n: u32) {
        self.0 = self.0.rotate_left(32) ^ u64::from(n);
    }
}

pub(crate) fn native_dead(_: &mut Lua, _: Args) -> Result<Flow, LuaError> {
    Err(LuaError::msg("attempt to call a function that was collected"))
}

impl Lua {
    pub(crate) fn new_vm(mut limits: Limits, libs: crate::Libs) -> Lua {
        limits.stack_bytes = limits.stack_bytes.min(crate::test_stack_cap());
        // One seed for everything random in the state (fixed by the host, or fresh).
        let base = limits.hash_seed.unwrap_or_else(crate::heap::random_seed);
        let mut heap = Heap::new(limits.memory, crate::heap::mix_seed(base, 1));
        heap.table_seed = crate::heap::mix_seed(base, 2);
        // The machine's own objects are made with room to spare.
        heap.mem.headroom = 1 << 20;
        heap.blocked += 1;
        let main_ref = heap.new_thread(ThreadState::default()).unwrap_or(ThreadRef { idx: 0, generation: 0 });
        let globals = heap.new_table(0, 64).unwrap_or(TableRef { idx: 0, generation: 0 });
        let registry = heap.new_table(0, 8).unwrap_or(TableRef { idx: 0, generation: 0 });
        let tm_names: Vec<StrRef> =
            TM_NAMES.iter().map(|n| heap.intern(n.as_bytes()).unwrap_or(StrRef { idx: 0, generation: 0 })).collect();
        heap.mode_key = tm_names.get(Tm::Mode as usize).copied();
        let names = crate::stdlib::Names::new(&mut heap);
        // Not `mix_seed(base, 3)`: that is invertible, and `math.random(0)` returns the
        // generator's state word, so one call gave back `base` and with it the string and
        // table hash seeds (S5GC-4).
        let seed = crate::heap::oneway_seed(base, 3);
        let mut lua = Lua {
            heap,
            cur: Box::new(ThreadState { status: CoStatus::Running, ..ThreadState::default() }),
            cur_ref: main_ref,
            main_ref,
            top: 0,
            ntop: 0,
            rets: Vec::new(),
            globals,
            registry,
            string_meta: None,
            tm_names,
            ctx_epoch: 0,
            match_state: None,
            spare_stacks: Vec::new(),
            fuel: i64::MAX,
            stopped_left: None,
            limits,
            c_depth: 0,
            native_depth: 0,
            native_calls: 0,
            native_from_lua: false,
            native_event: None,
            pending_event: None,
            native_name: "?",
            native_stack: Vec::new(),
            err_native: None,
            handler_native: None,
            handler_raise: HandlerWatch::default(),
            stack_base: crate::stack_pos(),
            resume_depth: 0,
            protected_natives: 0,
            gc_work_carry: 0,
            gc_mark_over: 0,
            atomic_owed: 0,
            gc_owed_probe: (0, 0),
            gc_pending: None,
            pending_call: None,
            due_skip: false,
            gc_atomic_pre: None,
            atomic_deferred: 0,
            pending_start: None,
            gc_call_before: None,
            rewound: false,
            err_closing: None,
            nested_pending: None,
            roots: Vec::new(),
            scratch: Vec::new(),
            held: Vec::new(),
            scratch_floor: 0,
            scratch_compact_at: COMPACT_MIN,
            held_compact_at: COMPACT_MIN,
            host_rets: Vec::new(),
            host_rets_base: 0,
            roots_pruned_at: 0,
            fast: crate::nostd::env::var_os("REMEDYLUA_TEST_NO_FAST_LOOP").is_none(),
            full_steps: 0,
            libs,
            rng: [0; 4],
            output: Vec::new(),
            output_used: 0,
            output_charged: 0,
            warn_on: false,
            limit_hit: None,
            reserve: 0,
            interrupt: crate::nostd::sync::Arc::default(),
            time_limit: None,
            deadline: None,
            app_data: crate::nostd::collections::HashMap::<_, _, crate::nostd::collections::hash_map::RandomState>::default(),
            last_error: None,
            gc_generational: false,
            print_sink: None,
            entry: 0,
            poisoned: false,
            deferred: Vec::new(),
            last_trace: None,
            trace_sources: Vec::new(),
            os_clocks: crate::default_os_clocks(),
            utc_offset: 0,
            names,
            modules: crate::nostd::collections::HashMap::<_, _, crate::nostd::collections::hash_map::RandomState>::default(),
            env: crate::nostd::collections::HashMap::<_, _, crate::nostd::collections::hash_map::RandomState>::default(),
            env_name_max: 0,
            io_std: [None, None, None],
            io_opener: None,
            io_meta: None,
            module_loader: None,
        };
        // S5API-7: seeded as `math.randomseed` seeds it (C Lua's `setseed`, 16 values
        // discarded). The state set by hand before made the second value the same for
        // every seed.
        lua.seed_random(seed, seed.rotate_left(32));
        crate::stdlib::open(&mut lua, libs);
        lua.heap.blocked -= 1;
        lua.heap.mem.headroom = 0;
        lua
    }

    // ---- stack ----

    #[inline]
    pub(crate) fn st(&self, i: usize) -> Value {
        self.cur.stack.get(i).copied().unwrap_or(Value::Nil)
    }

    #[inline]
    pub(crate) fn set_st(&mut self, i: usize, v: Value) {
        if let Some(s) = self.cur.stack.get_mut(i) {
            *s = v;
        }
    }

    /// Makes the running thread's stack at least `n` slots.
    #[inline]
    pub(crate) fn ensure_stack(&mut self, n: usize) -> Result<(), LuaError> {
        if n <= self.cur.stack.len() {
            return Ok(());
        }
        self.grow_stack(n)
    }

    /// `ensure_stack` when the stack is too short (out of line: the check is inlined).
    #[inline(never)]
    fn grow_stack(&mut self, n: usize) -> Result<(), LuaError> {
        self.stack_room(n)?;
        self.cur.stack.resize(n, Value::Nil);
        Ok(())
    }

    /// Makes the running thread's stack able to hold `n` slots (charged), without
    /// lengthening it.
    fn stack_room(&mut self, n: usize) -> Result<(), LuaError> {
        if n > self.limits.stack_slots {
            return Err(LuaError::msg("stack overflow"));
        }
        let old_cap = self.cur.stack.capacity();
        if n > old_cap {
            let new_cap = n.max(old_cap * 2).max(64).min(self.limits.stack_slots.max(n));
            self.charge_stack((new_cap - old_cap) * 16)?;
            self.cur.stack.reserve_exact(new_cap - self.cur.stack.len());
        }
        Ok(())
    }

    /// Room for more call frames, charged (S5GC-7: frame records were not, and at about
    /// 100 bytes against some 36 bytes of stack a level, coroutines parked deep in
    /// recursion held 285 MB under a 64 MB limit). The fast loop leaves growing to here.
    #[cold]
    fn grow_frames(&mut self) -> Result<(), LuaError> {
        let cap = self.cur.frames.capacity();
        let want = (cap * 2).max(2);
        let charged = (want - cap) * crate::heap::FRAME_BYTES;
        self.charge_stack(charged)?;
        self.cur.frames.reserve_exact(want - self.cur.frames.len());
        let got = self.cur.frames.capacity().saturating_sub(cap) * crate::heap::FRAME_BYTES;
        self.heap.mem.settle(charged, got);
        Ok(())
    }

    /// Makes the stack hold a full register window (256 slots) from `base`: registers are
    /// then indexed by their 8-bit number without checks. (The slack past the top is not
    /// counted against `stack_slots`.)
    #[inline]
    pub(crate) fn ensure_window(&mut self, base: usize, width: usize) -> Result<(), LuaError> {
        let n = base + width + EXTRA_STACK;
        if n <= self.cur.stack.len() {
            return Ok(());
        }
        if base > self.limits.stack_slots {
            return Err(LuaError::msg("stack overflow"));
        }
        let old_cap = self.cur.stack.capacity();
        if n > old_cap {
            let new_cap = n.max(old_cap * 2).max(64);
            self.charge_stack((new_cap - old_cap) * 16)?;
            self.cur.stack.reserve_exact(new_cap - self.cur.stack.len());
        }
        self.cur.stack.resize(n, Value::Nil);
        Ok(())
    }

    /// The end of the running frame's registers (0 without one).
    fn frame_top(&self) -> usize {
        self.cur.frames.last().map_or(0, |f| f.base + self.proto(f.proto).map_or(0, Proto::frame_size))
    }

    /// Where a call made now (a metamethod, a native's callback) puts its function.
    fn call_top(&self) -> usize {
        let frame_top = self.frame_top();
        if self.native_depth > 0 || self.ntop > frame_top { self.ntop.max(frame_top) } else { frame_top }
    }

    // ---- memory ----

    /// The error for memory the budget cannot give (after a full collection). Where C Lua
    /// would already have stopped with its own catchable error, that error: past its 200 C
    /// levels (metamethods, protected calls and resumes nested: "C stack overflow"), past its
    /// 1,000,000 stack slots, or, growing this thread's own stack (`own`), with that stack
    /// filling the budget (call depth is bounded by memory here: "stack overflow"); C Lua
    /// has no budget, so anything else failing then is the uncatchable memory
    /// limit. Nothing was allocated, so the cap holds; the unwinding frees the frames and
    /// the collector gives their room back. The error's message gets a little room past the
    /// cap (`on_error` takes it back): the budget is full, and C Lua's allocator would not be.
    #[cold]
    fn memory_error(&mut self, own: bool) -> LuaError {
        let e = if self.c_levels().0 > C_HANDLER_LEVELS {
            LuaError::msg("C stack overflow")
        } else if self.cur.stack.len() > 1_000_000 || own && self.cur.mem_size().saturating_mul(4) >= self.heap.mem.cap {
            LuaError::msg("stack overflow")
        } else {
            return LuaError::Limit(LimitKind::Memory);
        };
        self.heap.mem.headroom = self.heap.mem.headroom.max(1 << 10);
        e
    }

    /// Accounts for `n` bytes, collecting first when that passes the cap.
    pub(crate) fn charge_mem(&mut self, n: usize) -> Result<(), LuaError> {
        self.charge_room(n, false)
    }

    /// `charge_mem` for this thread's own stack or frames (`memory_error`).
    fn charge_stack(&mut self, n: usize) -> Result<(), LuaError> {
        self.charge_room(n, true)
    }

    fn charge_room(&mut self, n: usize, own: bool) -> Result<(), LuaError> {
        if self.heap.mem.charge(n).is_ok() {
            return Ok(());
        }
        self.emergency_gc()?;
        if self.heap.mem.charge(n).is_ok() || (self.release_held()? && self.heap.mem.charge(n).is_ok()) {
            return Ok(());
        }
        Err(self.memory_error(own))
    }

    fn emergency_gc(&mut self) -> Result<(), LuaError> {
        if self.heap.blocked > 0 {
            return Err(LuaError::Limit(LimitKind::Memory));
        }
        self.full_gc()
    }

    /// The last resort when memory is full outside any call: the values held for the host
    /// (`held`) go too, and are collected. Whether there was anything to release.
    fn release_held(&mut self) -> Result<bool, LuaError> {
        if self.c_depth > 0 || self.held.is_empty() || self.heap.blocked > 0 {
            return Ok(false);
        }
        self.held.clear();
        self.full_gc()?;
        Ok(true)
    }

    /// Runs `f` (an allocation), collecting and retrying once when memory is full.
    fn alloc<T>(&mut self, f: impl Fn(&mut Heap) -> Result<T, ()>) -> Result<T, LuaError> {
        if let Ok(v) = f(&mut self.heap) {
            return Ok(v);
        }
        self.emergency_gc()?;
        if let Ok(v) = f(&mut self.heap) {
            return Ok(v);
        }
        if self.release_held()? && let Ok(v) = f(&mut self.heap) {
            return Ok(v);
        }
        Err(self.memory_error(false))
    }

    /// Records `name` (a string) as the chunk name of compiled chunk `f` and every
    /// function nested in it (`debug.getinfo`'s `source`). A walk over the prototypes, which
    /// the compile already paid for, in a loop (no native recursion).
    pub(crate) fn set_chunk_name(&mut self, f: Value, name: Value) {
        let Value::Func(fr) = f else { return };
        let Some(Func::Lua { proto, .. }) = self.heap.funcs.get(fr.idx) else { return };
        let mut todo = vec![*proto];
        while let Some(i) = todo.pop() {
            if let Some(p) = self.heap.protos.get_mut(i) {
                p.chunk_name = name;
                todo.extend_from_slice(&p.protos);
            }
        }
        // A prototype marked already would not mark it again.
        if name.is_collectable() && self.heap.phase == crate::heap::Phase::Propagate {
            self.heap.mark(name);
        }
    }

    /// Forgets the roots whose handles were dropped, giving back their charge (LR-17).
    pub(crate) fn prune_roots(&mut self) {
        let before = self.roots.len();
        self.roots.retain(|(w, _)| w.strong_count() > 0);
        self.heap.mem.release((before - self.roots.len()) * crate::heap::ROOT_BYTES);
    }

    /// Keeps a value a native made alive until the native returns.
    #[inline]
    pub(crate) fn hold(&mut self, v: Value) {
        if self.native_depth > 0 && v.is_collectable() {
            self.push_scratch(v);
        }
    }

    /// Keeps `v` in the running native's scratch. S5API-1: a native that calls back many
    /// times (`gsub` with a function, `sort`'s comparator, `load`'s reader, a host function
    /// calling `get` in a loop) kept every result and capture, mostly the same few objects,
    /// one uncharged slot each: native memory many times the memory limit, bounded only by
    /// fuel. Each time the running part doubles, repeats and plain values are dropped from
    /// it, so it stays within twice the number of distinct objects it keeps (themselves
    /// charged).
    #[inline]
    pub(crate) fn push_scratch(&mut self, v: Value) {
        self.scratch.push(v);
        if self.scratch.len() >= self.scratch_compact_at {
            let floor = self.scratch_floor.min(self.scratch.len());
            let kept = compact_values(&mut self.scratch, floor);
            self.scratch_compact_at = floor + kept + kept.max(COMPACT_MIN);
        }
    }

    /// Keeps `v` for the host until its next call (S5API-2: compacted as `push_scratch`
    /// does, so a host that reads the same values over and over between calls does not grow
    /// it without bound).
    pub(crate) fn push_held(&mut self, v: Value) {
        self.held.push(v);
        if self.held.len() >= self.held_compact_at {
            let kept = compact_values(&mut self.held, 0);
            self.held_compact_at = kept + kept.max(COMPACT_MIN);
        }
    }

    /// A string (interned when short).
    pub(crate) fn new_str(&mut self, b: &[u8]) -> Result<Value, LuaError> {
        if b.len() > self.limits.string_len {
            return Err(LuaError::Limit(LimitKind::Memory));
        }
        let r = self.alloc(|h| h.new_str(b))?;
        let v = Value::Str(r);
        self.hold(v);
        if self.native_depth > 0 {
            // A library function that makes many strings pays for their interner walks as
            // it goes (S5GC-3), not once it returns.
            self.charge_probes()?;
        }
        Ok(v)
    }

    pub(crate) fn new_table(&mut self, arr: usize, hash: usize) -> Result<TableRef, LuaError> {
        self.new_table_sized::<false>(arr, arr, hash)
    }

    /// A table with room for `room` array slots, sized as Lua sizes it (`lua_createtable`
    /// or a constructor): `asize` array slots, `hash` fields (so `#` agrees).
    pub(crate) fn new_table_sized<const REGISTER: bool>(&mut self, room: usize, asize: usize, hash: usize) -> Result<TableRef, LuaError> {
        let t = self.alloc(|h| Heap::new_table_in::<REGISTER>(&mut h.mem, &mut h.tables, &mut h.born, (h.phase, h.sweep, h.table_seed), room, hash))?;
        if let Some(tab) = self.heap.tables.get_mut(t.idx) {
            tab.init_sizes(asize, hash);
        }
        self.hold(Value::Table(t));
        Ok(t)
    }

    pub(crate) fn new_native(&mut self, f: NativeFn, name: &'static str, upvals: Vec<Value>) -> Result<Value, LuaError> {
        let up: Box<[Value]> = upvals.into_boxed_slice();
        let size = crate::heap::FUNC_BASE + crate::heap::boxed(up.len() * 16);
        self.charge_mem(size)?;
        let r = self.heap.new_func_charged(Func::Native { f, upvals: up, name, in_place: 0 });
        let v = Value::Func(r);
        self.hold(v);
        Ok(v)
    }

    /// Bytes in a string value.
    #[inline]
    pub(crate) fn bytes(&self, s: StrRef) -> &[u8] {
        self.heap.str_bytes(s)
    }

    // ---- fuel ----

    /// Charges the probe steps of the table work done since the last instruction (see
    /// `table::PROBES`), for natives that look up many keys in one call.
    pub(crate) fn charge_probes(&mut self) -> Result<(), LuaError> {
        self.charge(crate::table::take_probes())
    }

    /// Charges `n` steps of fuel. Library functions charge as they work, so the interrupt is
    /// looked at here too: a long `gsub` or `sort` stops as soon as the host asks.
    #[inline]
    pub(crate) fn charge(&mut self, n: u64) -> Result<(), LuaError> {
        self.fuel = self.fuel.saturating_sub(i64::try_from(n).unwrap_or(i64::MAX));
        if self.fuel < 0 || self.interrupt.load(crate::nostd::sync::atomic::Ordering::Relaxed) {
            return self.refuel();
        }
        Ok(())
    }

    /// Stops the call for the host's interrupt or its deadline, without charging fuel: for
    /// natives that do much work per unit charged, between chunks of it (S6-5: the deadline
    /// was read only when a slice of fuel ran out, so one big charge read it once).
    #[cold]
    pub(crate) fn check_time(&mut self) -> Result<(), LuaError> {
        if self.interrupt.load(crate::nostd::sync::atomic::Ordering::Relaxed) {
            return Err(LuaError::Limit(LimitKind::Interrupted));
        }
        if let (Some(at), Some((clock, _))) = (self.deadline, &self.time_limit)
            && clock() >= at
        {
            return Err(LuaError::Limit(LimitKind::Time));
        }
        Ok(())
    }

    /// Settles a long string key before a lookup walks the interner with it: its hash and the
    /// comparison with an interned string of that hash are done a piece at a time
    /// (`stdlib::COPY_PIECE`) between looks at the host's interrupt and deadline. (S6T-KEY: the
    /// first lookup of a 16 MiB key hashed it in one go, 6.4 ms past a 1 ms time limit;
    /// a lookup of an equal copy also compared all its bytes.) No fuel: the hash and the
    /// comparison were free before and are the same work, done earlier.
    ///
    /// The comparison is with the one interned string the key can equal (`Heap::key_best`,
    /// S6T-EQ), and what it found is used by the lookup or interning that follows, which
    /// so compares nothing again: a lookup compares at most one key's worth of bytes.
    #[inline]
    pub(crate) fn settle_key(&mut self, k: Value) -> Result<Settled, LuaError> {
        match k {
            Value::Str(r) if self.heap.key_needs_settling(r) => self.settle_long_key(r),
            _ => Ok(Settled::Plain),
        }
    }

    #[cold]
    #[inline(never)]
    fn settle_long_key(&mut self, r: crate::value::StrRef) -> Result<Settled, LuaError> {
        let mut run = None;
        while self.heap.strings.get(r.idx).is_some_and(|s| s.hash.get() == 0) {
            match self.heap.hash_piece(self.heap.str_bytes(r), &mut run) {
                Some(h) => self.heap.strings.get(r.idx).map_or((), |s| s.hash.set(h)),
                None => self.check_time()?,
            }
        }
        let Some(c) = self.heap.key_best(r) else { return Ok(Settled::Absent(None)) };
        let len = self.heap.str_bytes(r).len();
        let mut at = 0;
        while at < len {
            self.check_time()?;
            let end = at.saturating_add(crate::stdlib::COPY_PIECE).min(len);
            let found = self.heap.diff_piece(r.idx, c, at, end);
            // SEC-M11: the bytes compared are paid for. A miss records nothing, so a key that
            // differs from an interned one of its hash and length only at its end (made with
            // a seed the script knows) compared its whole length at every lookup, free: 4 MiB
            // for 4 units. A miss happens only between strings of one whole hash, so fuel
            // stays the same under any seed for a program not aimed at one.
            self.charge_bytes(found.unwrap_or(end).saturating_sub(at))?;
            if let Some(d) = found {
                return Ok(Settled::Absent(Some((c, d))));
            }
            at = end;
        }
        self.heap.set_twin(r, c);
        Ok(Settled::Found)
    }

    /// `heap.get_in` on table `t`, with a long key settled (`settle_key`) when the plain
    /// lookup misses: the same lookups otherwise, so the same probes and fuel.
    #[inline]
    pub(crate) fn get_in_settled(&mut self, t: u32, key: Value) -> Result<Value, LuaError> {
        let Some(tab) = self.heap.tables.get(t) else { return Ok(Value::Nil) };
        let v = tab.get(key);
        if !v.is_nil() {
            return Ok(v);
        }
        if let Settled::Absent(_) = self.settle_key(key)? {
            // No interned copy: in no table.
            return Ok(v);
        }
        let Some(tab) = self.heap.tables.get(t) else { return Ok(Value::Nil) };
        Ok(self.heap.retry_key(key).map_or(v, |k| tab.get(k)))
    }

    /// The fuel in hand ran out (or the host may have interrupted): stops the call for the
    /// host's interrupt, its deadline or the end of the budget, or hands out the next slice
    /// of fuel. The budget stays exact: a call stops at the step where fuel and reserve
    /// together run out, whatever the slice.
    #[cold]
    pub(crate) fn refuel(&mut self) -> Result<(), LuaError> {
        if let Some(k) = self.limit_hit {
            return Err(LuaError::Limit(k));
        }
        if self.interrupt.load(crate::nostd::sync::atomic::Ordering::Relaxed) {
            return Err(LuaError::Limit(LimitKind::Interrupted));
        }
        if let (Some(at), Some((clock, _))) = (self.deadline, &self.time_limit)
            && clock() >= at
        {
            return Err(LuaError::Limit(LimitKind::Time));
        }
        if !self.heap.mem.freeing.is_empty() {
            self.heap.mem.unpark();
        }
        if self.fuel >= 0 {
            return Ok(());
        }
        let give = self.reserve.min(FUEL_SLICE.saturating_add(self.fuel.saturating_neg()));
        self.reserve -= give;
        self.fuel = self.fuel.saturating_add(give);
        if self.fuel < 0 { Err(LuaError::Limit(LimitKind::Fuel)) } else { Ok(()) }
    }

    // ---- collector ----

    /// Marks the roots: the work, the running thread's slots walked (S5B-WALK-1).
    fn mark_roots(&mut self) -> usize {
        let h = &mut self.heap;
        // The registry and its loaded-modules table first, so the globals are traversed
        // before them (the gray list is a stack): the libraries in the loaded table are
        // reached through the globals, as they were before that table existed, and the
        // modules only the loaded table holds right after the globals.
        h.mark(Value::Table(self.registry));
        if let Some(l) = self.names.loaded {
            h.mark(Value::Table(l));
        }
        h.mark(Value::Table(self.globals));
        if let Some(m) = self.string_meta {
            h.mark(Value::Table(m));
        }
        for &n in &self.tm_names {
            h.mark(Value::Str(n));
        }
        self.names.mark(h);
        h.mark(Value::Thread(self.main_ref));
        h.mark(Value::Thread(self.cur_ref));
        let walked = h.mark_thread_state(&self.cur);
        // Dropped roots forgotten, their charge given back (LR-17, as `prune_roots`).
        let before = self.roots.len();
        self.roots.retain(|(w, _)| w.strong_count() > 0);
        h.mem.release((before - self.roots.len()) * crate::heap::ROOT_BYTES);
        for (_, v) in &self.roots {
            h.mark(*v);
        }
        for v in &self.scratch {
            h.mark(*v);
        }
        for v in &self.rets {
            h.mark(*v);
        }
        for v in &self.held {
            h.mark(*v);
        }
        for v in &self.host_rets {
            h.mark(*v);
        }
        if let Some((_, v)) = &self.last_error {
            h.mark(*v);
        }
        walked
    }

    /// One step of collection, paid for by the allocation since the last.
    #[inline]
    pub(crate) fn gc_step(&mut self) -> Result<(), LuaError> {
        if self.heap.mem.trace.is_none() { self.gc_step_now() } else { self.gc_step_traced() }
    }

    /// `gc_step` with a trace record of it: its slots, fuel and time (SEC7-G1).
    #[cold]
    #[inline(never)]
    fn gc_step_traced(&mut self) -> Result<(), LuaError> {
        use crate::gctrace::TraceOp;
        let op = match self.heap.phase {
            crate::heap::Phase::Pause => TraceOp::Start,
            crate::heap::Phase::Propagate if self.heap.atomic_suspended() => TraceOp::Atomic,
            crate::heap::Phase::Propagate => TraceOp::Mark,
            crate::heap::Phase::Sweep => TraceOp::Sweep,
        };
        let (scanned, fuel) = (self.heap.scanned, self.fuel.saturating_add(self.reserve));
        let resumed = self.gc_pending.is_some();
        let start = self.heap.mem.trace.as_mut().map(|t| {
            // (A step resumed after a pause is the same step.)
            t.step += u32::from(!resumed);
            (t.now(), t.step)
        });
        let r = self.gc_step_now();
        let used = fuel.saturating_sub(self.fuel.max(-1).saturating_add(self.reserve.max(0)));
        if let (Some(t), Some((start, step))) = (self.heap.mem.trace.as_mut(), start) {
            t.note(op, step, self.heap.scanned.wrapping_sub(scanned) as u64, 0, u64::try_from(used).unwrap_or(0), start);
        }
        r
    }

    fn gc_step_now(&mut self) -> Result<(), LuaError> {
        // A step paused at the end of the host's poll goes on as it stood (SEC7-G1).
        let resumed = self.gc_pending.take();
        let (born_step, full, budget) = if let Some(p) = resumed {
            p
        } else {
            let born_step = self.heap.born.take_step();
            if self.heap.blocked > 0 {
                return Ok(());
            }
            if self.heap.stopped {
                return self.charge_born();
            }
            if self.heap.mem.stress {
                // The test mode's collections are not the script's work: not charged (the
                // interrupt still stops one).
                let (fuel, reserve) = (self.fuel, self.reserve);
                self.fuel = i64::MAX;
                let r = self.full_gc();
                (self.fuel, self.reserve) = (fuel, reserve);
                return r;
            }
            // A step's budget follows the debt, so one big allocation (an array part
            // doubling to 2^21 slots, 32 MiB) made a step of millions of units, done in one
            // go past the call's fuel (40 ms). A step does at most what the call's fuel
            // still pays for; the rest stays owed (as debt, below) and is done by the steps
            // that follow (S6GC-2e).
            let full = self.heap.step_budget();
            // (Less what the objects made since the last step owe, charged with this step.)
            (born_step, full, full.min(self.gc_allow(self.heap.born.pending()).max(1)))
        };
        // The budget not given to a marking or sweeping step that did not finish its phase.
        let mut unspent = 0;
        // A suspended atomic phase's work done past the last call's fuel is this step's
        // (charged with it, as far as the call's fuel goes, the rest owed again: in the
        // step's own trace record, SEC8-F7).
        let mut work = if resumed.is_none() && self.heap.atomic_suspended() { crate::nostd::mem::take(&mut self.atomic_owed) } else { 0 };
        match self.heap.phase {
            crate::heap::Phase::Pause => {
                // Work done and not yet charged when the last cycle ended (a full collection
                // stopped for the fuel in its sweep: `charge_or_owe`) is charged, not dropped
                // (SEC8-F7): as far as this step's budget goes, the rest owed to the marking
                // steps after, as marking's own.
                let over = crate::nostd::mem::take(&mut self.gc_mark_over);
                let pay = over.min(budget);
                work += pay;
                self.gc_mark_over = over - pay;
                self.gc_owed_probe.1 += pay as u64;
                work += self.clear_dead_stack();
                self.heap.start_cycle();
                work += self.mark_roots();
                work += 64;
            }
            crate::heap::Phase::Propagate => {
                // A suspended atomic phase goes on where it stopped.
                let stop = crate::heap::Stop {
                    interrupt: &self.interrupt,
                    deadline: self.deadline.filter(|_| self.entry > 0).zip(self.time_limit.as_ref().map(|(clock, _)| clock)),
                };
                let mut stopped = false;
                let done = if self.heap.atomic_suspended() {
                    true
                } else {
                    // The objects made since the last step take their part of it, as they
                    // did as gray objects (`Born::step`; they are traversed in any case).
                    let mark = budget.saturating_sub(born_step);
                    let (empty, w, cut, _) = if resumed.is_some() {
                        self.heap.propagate_resume(&stop)
                    } else {
                        self.heap.step_slots = crate::heap::STEP_SLOTS;
                        self.heap.propagate(mark.saturating_sub(self.gc_mark_over), &stop)
                    };
                    if crate::nostd::mem::take(&mut self.heap.paused) {
                        // Paused at the end of the host's poll: nothing charged or settled
                        // yet; the resume goes on with it before any script runs, so it ends
                        // where it would have, charged as it would have been.
                        self.gc_pending = Some((born_step, full, budget));
                        return Err(ErrorKind::Pending.into());
                    }
                    stopped = cut;
                    let owed = self.gc_mark_over + w;
                    let done = empty && owed <= mark;
                    if done {
                        self.gc_mark_over = 0;
                        work += owed;
                    } else {
                        self.gc_mark_over = owed.saturating_sub(mark);
                        work += mark.min(owed);
                        unspent = full - budget;
                    }
                    done
                };
                if stopped {
                    // A marking step stopped by the interrupt or the deadline: its work is
                    // charged and the call stops (marking goes on at the next step).
                    self.heap.mem.debt = -STEP_SIZE;
                    let work = work + crate::nostd::mem::take(&mut self.gc_work_carry);
                    self.charge_gc(work / WORK_PER_FUEL)?;
                    return Err(LuaError::Limit(self.stop_kind()));
                }
                // SEC7-G1: the atomic phase starts at a step at an instruction, where the call
                // can pause in it, not in a Rust function's (`collectgarbage('step')` among
                // them): the next step at an instruction starts it (marking has nothing left
                // to do meanwhile but what the writes since give it). In every call alike,
                // so where it runs follows the program alone.
                let defer = done
                    && self.native_depth > 0
                    && self.gc_atomic_pre.is_none()
                    && !self.heap.atomic_suspended()
                    && self.atomic_deferred < ATOMIC_DEFERRALS;
                if defer {
                    self.atomic_deferred += 1;
                } else if done {
                    self.atomic_deferred = 0;
                    let (pre, allow) = match self.gc_atomic_pre.take() {
                        // (Paused in it: it goes on with what it had.)
                        Some(p) => p,
                        None => {
                            work += self.clear_dead_stack();
                            work += self.mark_roots();
                            (work, self.gc_allow(work))
                        }
                    };
                    work = pre;
                    let stop = crate::heap::Stop {
                        interrupt: &self.interrupt,
                        deadline: self.deadline.filter(|_| self.entry > 0).zip(self.time_limit.as_ref().map(|(clock, _)| clock)),
                    };
                    match self.heap.atomic(&stop, allow) {
                        crate::heap::Atomic::Paused => {
                            // Paused at the end of the host's poll: nothing charged yet.
                            self.gc_atomic_pre = Some((pre, allow));
                            self.gc_pending = Some((born_step, full, budget));
                            return Err(ErrorKind::Pending.into());
                        }
                        crate::heap::Atomic::Done(w) => work += w,
                        crate::heap::Atomic::Interrupted(w) => {
                            self.charge_gc((work + w) / WORK_PER_FUEL)?;
                            return Err(LuaError::Limit(LimitKind::Interrupted));
                        }
                        crate::heap::Atomic::Suspended(w) => return self.atomic_suspended(work + w, LimitKind::Fuel),
                        crate::heap::Atomic::TimedOut(w) => return self.atomic_suspended(work + w, LimitKind::Time),
                    }
                }
            }
            crate::heap::Phase::Sweep => {
                let stop = crate::heap::Stop {
                    interrupt: &self.interrupt,
                    deadline: self.deadline.filter(|_| self.entry > 0).zip(self.time_limit.as_ref().map(|(clock, _)| clock)),
                };
                let (done, w, cut) = self.heap.sweep_step(budget, &stop);
                work += w;
                if cut {
                    // Stopped by the interrupt or the deadline: its work is charged and the
                    // call stops (the sweep goes on at the next step).
                    self.heap.mem.debt = -STEP_SIZE;
                    let work = work + crate::nostd::mem::take(&mut self.gc_work_carry);
                    self.charge_gc(work / WORK_PER_FUEL)?;
                    return Err(LuaError::Limit(self.stop_kind()));
                }
                if done {
                    self.end_cycle();
                } else {
                    unspent = full - budget;
                }
            }
        }
        if self.heap.phase != crate::heap::Phase::Pause {
            // (The inverse of `step_budget`: the bytes the budget not given covers.)
            let owed = (unspent as u64).saturating_mul(800) / (self.heap.stepmul.max(1) as u64);
            let cap = self.heap.mem.cap;
            let room = cap.saturating_sub(self.memory_used());
            let runway = cap / 4;
            // Even sweeping just the table slab would take more than one host call.
            // Begin paying sooner while there is room to run the script between steps.
            let tables = &self.heap.tables;
            self.heap.born.trace_new_tables = room < runway
                && ((tables.slots() - tables.free.len()) / WORK_PER_FUEL) as u64 > self.limits.fuel;
            let interval = if self.heap.born.trace_new_tables {
                let proportional = ((STEP_SIZE as u128 * room as u128) / runway as u128) as isize;
                (proportional * proportional / STEP_SIZE).max(STEP_SIZE / 64)
            } else {
                STEP_SIZE
            };
            self.heap.mem.debt = (-interval).saturating_add(isize::try_from(owed).unwrap_or(isize::MAX));
        }
        // S5C-1: a step stops after the object that passes its budget, so how a cycle's
        // work splits into steps follows the order objects are reached, which follows
        // where keys sit in hash parts, which follows the seed. Each step's remainder was
        // dropped, so fuel differed by a unit or two between seeds; it is carried now.
        // (With what the objects made since the last step owe: COV-5, `heap::Born`.)
        let work = work + crate::nostd::mem::take(&mut self.gc_work_carry) + self.heap.born.take_work();
        self.gc_work_carry = work % WORK_PER_FUEL;
        self.charge_gc(work / WORK_PER_FUEL)
    }

    /// Charges the collector's `units` of fuel as far as the call's fuel goes; what it cannot
    /// pay is owed (`owe_gc`), never dropped (SEC8-F7), and the call stops on its fuel limit.
    #[inline]
    fn charge_gc(&mut self, units: usize) -> Result<(), LuaError> {
        let left = self.call_fuel();
        if units > left {
            return self.owe_gc(units - left, left);
        }
        self.charge(units as u64)
    }

    /// `charge_gc` past the call's fuel: `owed` units of fuel to the collector's next steps or
    /// collection (`gc_mark_over`, in work units), the call charged the `left` it had.
    #[cold]
    fn owe_gc(&mut self, owed: usize, left: usize) -> Result<(), LuaError> {
        self.gc_mark_over += owed * WORK_PER_FUEL;
        self.gc_owed_probe.0 += (owed * WORK_PER_FUEL) as u64;
        self.charge(left as u64)?;
        self.fuel = -1;
        self.reserve = 0;
        Err(LuaError::Limit(LimitKind::Fuel))
    }

    /// A full collection's `work` (units; `charged` units of fuel of it paid already)
    /// charged as far as the call's fuel goes: past it, the call is charged what fuel it had
    /// and stops on its fuel limit, and the rest is owed (`gc_mark_over`, charged by the
    /// collector's next steps or collection). Dropped, as it was, the work done past the
    /// call's fuel (up to a piece of `FULL_GC_CHUNK` and an object) went uncharged, by an
    /// amount that followed where the pieces fell in the order objects are reached, so the
    /// seed: 5,766,987 units against 5,773,106 for seeds 1 and 7 over 3,300 calls of the G1
    /// reproducer at its memory limit (SEC8-F7).
    fn charge_or_owe(&mut self, work: usize, charged: &mut usize) -> Result<(), LuaError> {
        let pay = work / WORK_PER_FUEL - *charged;
        let left = self.call_fuel();
        if pay <= left {
            self.charge(pay as u64)?;
            *charged += pay;
            return Ok(());
        }
        // (The remainder under a unit is owed too: the whole of the work is charged.)
        self.gc_mark_over += work % WORK_PER_FUEL;
        *charged += left;
        self.owe_gc(pay - left, left)
    }

    /// The collector work the running call may still do: its fuel and reserve, less `done`
    /// (work not charged yet), in work units. Unlimited outside a call (a host `load`'s
    /// allocations collect unmetered).
    fn gc_allow(&self, done: usize) -> usize {
        self.call_fuel().saturating_mul(WORK_PER_FUEL).saturating_sub(done + self.gc_work_carry)
    }

    /// The running call's fuel and reserve: unlimited outside a call.
    fn call_fuel(&self) -> usize {
        if self.entry == 0 {
            return usize::MAX;
        }
        usize::try_from(self.fuel.max(0).saturating_add(self.reserve.max(0))).unwrap_or(usize::MAX)
    }

    /// An atomic phase stopped at the call's fuel (S6GC-2e) after `work` units not charged
    /// yet: the call is charged what fuel it had and stops on its fuel limit; the rest is
    /// owed by the call that resumes it (`resume_atomic`), so how the work splits between
    /// calls follows its total, never the order objects were reached in.
    /// (Stopped at the deadline instead, `kind` is `Time`: the same, the call stopping on
    /// its time limit with the fuel it has left, which `enter` reports: SEC8-F1, zeroed
    /// here, it read as the whole budget spent.)
    #[cold]
    fn atomic_suspended(&mut self, work: usize, kind: LimitKind) -> Result<(), LuaError> {
        let work = work + crate::nostd::mem::take(&mut self.gc_work_carry);
        let left = self.call_fuel();
        self.atomic_owed = work.saturating_sub(left.saturating_mul(WORK_PER_FUEL));
        let _ = self.charge((work - self.atomic_owed) as u64 / WORK_PER_FUEL as u64);
        if kind == LimitKind::Fuel {
            self.fuel = -1;
            self.reserve = 0;
        }
        Err(LuaError::Limit(kind))
    }

    /// Why a collector step stopped part-way (`heap::Stop`): the interrupt, else the deadline.
    #[cold]
    fn stop_kind(&self) -> LimitKind {
        if self.interrupt.load(crate::nostd::sync::atomic::Ordering::Relaxed) { LimitKind::Interrupted } else { LimitKind::Time }
    }

    /// Finishes a suspended atomic phase before a call from the host runs any script code
    /// (a script must not run inside one): charged to this call, which stops on its fuel
    /// limit at once if the phase does not fit in it (it is suspended again).
    /// Whether the call at `func` with `nargs` arguments is `collectgarbage('step')` or
    /// `collectgarbage('step', 0)`: a basic step.
    fn basic_step_call(&self, func: usize, nargs: usize) -> bool {
        let is_step = matches!(self.st(func + 1), Value::Str(r) if self.heap.strings.get(r.idx).is_some_and(|s| &*s.bytes == b"step"));
        is_step && (nargs == 1 || (nargs == 2 && matches!(self.st(func + 2), Value::Int(0) | Value::Nil)))
    }

    /// The step of `collectgarbage('step')` called from Lua code, before the function runs
    /// (it answers from `gc_call_before`): what the function's own does, at a step that may
    /// pause (`gc_step_pausable`).
    fn collect_step(&mut self) -> Result<(), LuaError> {
        self.gc_call_before = Some(self.heap.cycle);
        let stopped = crate::nostd::mem::replace(&mut self.heap.stopped, false);
        self.heap.mem.debt = self.heap.mem.debt.max(crate::heap::STEP_SIZE);
        let r = self.gc_step_pausable();
        self.heap.stopped = stopped;
        r
    }

    /// `resume_atomic` at the entry of a call from the host, which may pause at the end of
    /// the host's poll (`Heap::poll_allow`): the call then is pending before it started
    /// (`pending_start`, SEC7-G1).
    pub(crate) fn resume_atomic_pausable(&mut self) -> Result<(), LuaError> {
        self.heap.pause_ok = self.heap.poll_allow.is_some() && self.entry == 1;
        let r = self.resume_atomic();
        self.heap.pause_ok = false;
        r
    }

    pub(crate) fn resume_atomic(&mut self) -> Result<(), LuaError> {
        if !self.heap.atomic_suspended() {
            return Ok(());
        }
        // (What it owes is charged by the step: `gc_step_now`.)
        let stopped = crate::nostd::mem::replace(&mut self.heap.stopped, false);
        let r = self.gc_step();
        self.heap.stopped = stopped;
        r
    }

    /// Clears the running thread's stack above what is live, so stale slots do not keep
    /// garbage alive. Only called between instructions, where the live part is the top
    /// frame's registers (and a pending variable-length result list), or from a library
    /// function, where it is the function's arguments and what it pushed. Returns the
    /// collector work: the whole stack, not the slots cleared (S5B-WALK-1). Where the live
    /// part ends follows when the atomic step comes, which follows the order objects are
    /// reached in, so counting the cleared slots made fuel follow the hash seed (COV-5).
    fn clear_dead_stack(&mut self) -> usize {
        // In a library function the live stack ends at its arguments and what it pushed (C
        // Lua's `L->top` then): its Lua caller's registers above the call are dead, so a
        // value only they hold is garbage, as in C Lua (weak tables see it go).
        let live = if self.native_depth > 0 {
            let mut live = self.ntop;
            if let Some(&(i, _)) = self.cur.open_upvals.last() {
                live = live.max(i + 1);
            }
            if let Some(&i) = self.cur.tbc.last() {
                live = live.max(i + 1);
            }
            live
        } else {
            let frame_top = self.frame_top();
            frame_top.max(self.top).max(self.ntop)
        };
        if self.cur.frames.is_empty() {
            return 0;
        }
        self.cur.stack.get_mut(live..).unwrap_or_default().fill(Value::Nil);
        self.cur.stack.len()
    }

    /// Clears the whole stack when no call is running (back at the host).
    pub(crate) fn clear_idle_stack(&mut self) {
        if self.cur.frames.is_empty() && self.c_depth == 0 {
            // O(1), not a walk over the high-water length: after one deep recursion with
            // the collector stopped (so the stack never shrinks), every later call from
            // the host paid for clearing millions of slots. The capacity stays charged.
            self.cur.stack.clear();
            self.top = 0;
        }
    }

    /// Gives back stack the running thread no longer uses (after deep recursion), as C
    /// Lua's collector does.
    fn shrink_stack(&mut self) {
        let frame_top = self.cur.frames.last().map_or(0, |f| f.base + self.proto(f.proto).map_or(WINDOW, |p| p.window().max(p.frame_size())));
        let mut live = frame_top.max(self.top).max(self.ntop);
        if let Some(&(i, _)) = self.cur.open_upvals.last() {
            live = live.max(i + 1);
        }
        if let Some(&i) = self.cur.tbc.last() {
            live = live.max(i + 1);
        }
        crate::heap::shrink_frames(&mut self.cur.frames, &mut self.heap.mem);
        let want = (live * 2).max(64) + EXTRA_STACK;
        let cap = self.cur.stack.capacity();
        if cap <= want * 2 {
            return;
        }
        let keep = (live + EXTRA_STACK).min(self.cur.stack.len());
        self.cur.stack.truncate(keep);
        self.cur.stack.shrink_to(want);
        let now = self.cur.stack.capacity();
        self.heap.mem.release(cap.saturating_sub(now) * 16);
        self.top = self.top.min(keep);
    }

    fn end_cycle(&mut self) {
        self.shrink_stack();
        self.heap.shrink_parked();
        self.heap.trim_scratch();
        let est = self.heap.mem.live();
        self.heap.estimate = est;
        // The next cycle starts when the heap grows by the pause (doubles by default), or
        // nears the cap.
        let cap = self.heap.mem.cap;
        let pause = self.heap.pause;
        let mut grow = est.saturating_mul(pause) / 100;
        if pause >= 200 {
            grow = grow.max(est + 256 * 1024);
        }
        let threshold = grow.min(cap - cap / 8).max(est + STEP_SIZE as usize);
        self.heap.mem.debt = est as isize - threshold as isize;
    }

    /// A complete collection (finishing the one in progress first).
    pub(crate) fn full_gc(&mut self) -> Result<(), LuaError> {
        if self.heap.mem.trace.is_none() { self.full_gc_now() } else { self.full_gc_traced() }
    }

    /// `full_gc` with a trace record of its pass: its slots, fuel and time (SEC8-G3).
    #[cold]
    #[inline(never)]
    fn full_gc_traced(&mut self) -> Result<(), LuaError> {
        let (scanned, fuel) = (self.heap.scanned, self.fuel.saturating_add(self.reserve));
        let began = match self.heap.phase {
            crate::heap::Phase::Pause => crate::gctrace::FULL_BEGAN_PAUSE,
            crate::heap::Phase::Propagate => crate::gctrace::FULL_BEGAN_MARK,
            crate::heap::Phase::Sweep => crate::gctrace::FULL_BEGAN_SWEEP,
        };
        let start = self.heap.mem.trace.as_mut().map(|t| {
            t.step += 1;
            (t.now(), t.step)
        });
        let r = self.full_gc_now();
        let used = fuel.saturating_sub(self.fuel.max(-1).saturating_add(self.reserve.max(0)));
        if let (Some(t), Some((start, step))) = (self.heap.mem.trace.as_mut(), start) {
            t.note(crate::gctrace::TraceOp::Full, step, self.heap.scanned.wrapping_sub(scanned) as u64, began, u64::try_from(used).unwrap_or(0), start);
        }
        r
    }

    fn full_gc_now(&mut self) -> Result<(), LuaError> {
        if self.heap.blocked > 0 {
            return Ok(());
        }
        // Marking work steps did past their budgets and did not charge yet (`gc_mark_over`)
        // is charged here: dropped, the fuel followed where the steps before had stopped
        // in the order objects are reached, so the seed.
        let mut work = self.clear_dead_stack() + crate::nostd::mem::take(&mut self.gc_mark_over) + crate::nostd::mem::take(&mut self.atomic_owed) + self.heap.born.take_work();
        let mut charged = 0;
        let mut cycles = 0;
        // Finish the current cycle, then run a whole new one, charged as it goes so a big
        // heap's collection stops at the fuel limit or the host's interrupt (the next
        // collection carries on where this one stopped).
        loop {
            match self.heap.phase {
                crate::heap::Phase::Pause => {
                    if cycles == 1 {
                        break;
                    }
                    cycles += 1;
                    self.heap.start_cycle();
                    work += self.mark_roots();
                }
                crate::heap::Phase::Propagate => {
                    let stop = crate::heap::Stop {
                        interrupt: &self.interrupt,
                        deadline: self.deadline.filter(|_| self.entry > 0).zip(self.time_limit.as_ref().map(|(clock, _)| clock)),
                    };
                    let done = if self.heap.atomic_suspended() {
                        true
                    } else {
                        self.heap.step_slots = usize::MAX;
                        // (A piece no bigger than what the call can still pay: SEC8-F7.)
                        let piece = FULL_GC_CHUNK.min(self.gc_allow(work - charged * WORK_PER_FUEL).max(1));
                        let (done, w, cut, _) = self.heap.propagate(piece, &stop);
                        work += w;
                        if cut {
                            self.charge_or_owe(work, &mut charged)?;
                            return Err(LuaError::Limit(self.stop_kind()));
                        }
                        done
                    };
                    if done {
                        work += self.mark_roots();
                        let allow = self.gc_allow(work - charged * WORK_PER_FUEL);
                        let stop = crate::heap::Stop {
                            interrupt: &self.interrupt,
                            deadline: self.deadline.filter(|_| self.entry > 0).zip(self.time_limit.as_ref().map(|(clock, _)| clock)),
                        };
                        match self.heap.atomic(&stop, allow) {
                            crate::heap::Atomic::Done(w) => work += w,
                            // (Never here: a full collection does not pause, `pause_ok` is
                            // off in it. Its next pass goes on.)
                            crate::heap::Atomic::Paused => {}
                            crate::heap::Atomic::Interrupted(w) => {
                                self.charge_or_owe(work + w, &mut charged)?;
                                return Err(LuaError::Limit(LimitKind::Interrupted));
                            }
                            crate::heap::Atomic::Suspended(w) => {
                                return self.atomic_suspended(work + w - charged * WORK_PER_FUEL, LimitKind::Fuel);
                            }
                            crate::heap::Atomic::TimedOut(w) => {
                                return self.atomic_suspended(work + w - charged * WORK_PER_FUEL, LimitKind::Time);
                            }
                        }
                    }
                }
                crate::heap::Phase::Sweep => {
                    let stop = crate::heap::Stop {
                        interrupt: &self.interrupt,
                        deadline: self.deadline.filter(|_| self.entry > 0).zip(self.time_limit.as_ref().map(|(clock, _)| clock)),
                    };
                    let piece = FULL_GC_CHUNK.min(self.gc_allow(work - charged * WORK_PER_FUEL).max(1));
                    let (done, w, cut) = self.heap.sweep_step(piece, &stop);
                    work += w;
                    if cut {
                        self.charge_or_owe(work, &mut charged)?;
                        return Err(LuaError::Limit(self.stop_kind()));
                    }
                    if done {
                        self.end_cycle();
                    }
                }
            }
            self.charge_or_owe(work, &mut charged)?;
        }
        // A full collection gives the table buffer pool back to the system too, every
        // free slot at a slab's end (SEC-M5), and the spare coroutine stacks.
        self.heap.mem.drain_pool();
        let stop = crate::heap::Stop {
            interrupt: &self.interrupt,
            deadline: self.deadline.filter(|_| self.entry > 0).zip(self.time_limit.as_ref().map(|(clock, _)| clock)),
        };
        if self.heap.trim_slabs(&stop) {
            return Err(LuaError::Limit(self.stop_kind()));
        }
        self.drop_spare_stacks();
        Ok(())
    }

    /// A collector step when one is due, else the born work charged (`Heap::gc_due`).
    fn gc_due_step(&mut self) -> Result<(), LuaError> {
        if self.heap.mem.debt > 0 { self.gc_step_pausable() } else { self.charge_born() }
    }

    /// A step that may pause at the end of the host's poll (`Heap::poll_allow`): only at
    /// an instruction of the call from the host itself, with no Rust function and no
    /// coroutine resumed from Lua under it, so the call can be resumed as it stands (a
    /// step inside a library function, `collectgarbage` among them, runs to its end).
    fn gc_step_pausable(&mut self) -> Result<(), LuaError> {
        self.heap.pause_ok = self.heap.poll_allow.is_some() && self.entry == 1 && self.c_depth == 1 && self.native_depth == 0 && self.resume_depth == 0;
        let r = self.gc_step();
        // A step ended past the end of the host's poll pauses the call there too: its work
        // all done and charged, nothing of it pending (steps of one object each never
        // reached a look inside them, and the script ran its whole fuel in one poll).
        let over = r.is_ok() && self.gc_pending.is_none() && self.heap.poll_over();
        self.heap.pause_ok = false;
        if over {
            return Err(ErrorKind::Pending.into());
        }
        r
    }

    /// What the objects made since the last step owe (COV-5, `heap::Born`).
    fn charge_born(&mut self) -> Result<(), LuaError> {
        let work = self.heap.born.take_work() + crate::nostd::mem::take(&mut self.gc_work_carry);
        self.gc_work_carry = work % WORK_PER_FUEL;
        self.charge_gc(work / WORK_PER_FUEL)
    }

    // ---- metatables ----

    pub(crate) fn metatable(&self, v: Value) -> Option<TableRef> {
        match v {
            Value::Table(t) => self.heap.tables.get(t.idx).and_then(|t| t.meta),
            Value::UserData(u) => self.heap.uds.get(u.idx).and_then(|u| u.meta),
            Value::Str(_) => self.string_meta,
            _ => None,
        }
    }

    #[inline]
    pub(crate) fn tm_in(&self, mt: TableRef, e: Tm) -> Value {
        let Some(t) = self.heap.tables.get(mt.idx) else { return Value::Nil };
        let bit = 1u32 << (e as u8);
        if t.absent.get() & bit != 0 {
            return Value::Nil;
        }
        let name = self.tm_names.get(e as usize).copied();
        let v = name.map_or(Value::Nil, |n| t.get_str(Value::Str(n)));
        if v.is_nil() {
            t.absent.set(t.absent.get() | bit);
        }
        v
    }

    #[inline]
    pub(crate) fn tm(&self, v: Value, e: Tm) -> Value {
        match self.metatable(v) {
            Some(mt) => self.tm_in(mt, e),
            None => Value::Nil,
        }
    }

    /// A binary event's metamethod: `x`'s, else `y`'s.
    #[inline]
    pub(crate) fn tm2(&self, x: Value, y: Value, e: Tm) -> Value {
        let mm = self.tm(x, e);
        if mm.is_nil() { self.tm(y, e) } else { mm }
    }

    /// The type name for the machine's messages, as C's `luaT_objtypename`: a table's or a
    /// userdata's `__name` (a string), else the plain type name. Other types' metatables
    /// (the strings' one) are not looked at.
    pub(crate) fn obj_type_name(&self, v: Value) -> String {
        if matches!(v, Value::Table(_) | Value::UserData(_)) { self.named_type(v) } else { v.type_name().to_owned() }
    }

    /// The type name for a library's "expected, got" messages, as C's `luaL_typeerror` (and
    /// `luaL_tolstring`) read it with `luaL_getmetafield`: the `__name` (a string) of any
    /// value's metatable, the strings' one included (`getmetatable("").__name = "S"` makes
    /// `math.floor("x")` say "got S"). Libraries that name a value with `luaL_typename`
    /// (`table.concat`, `gsub`'s replacement) use the plain `Value::type_name`.
    pub(crate) fn arg_type_name(&self, v: Value) -> String {
        self.named_type(v)
    }

    fn named_type(&self, v: Value) -> String {
        if let Some(mt) = self.metatable(v)
            && let Value::Str(s) = self.tm_in(mt, Tm::Name)
        {
            // Bounded: a huge `__name` must not make every message huge.
            let b = self.bytes(s);
            return String::from_utf8_lossy(b.get(..b.len().min(MAX_NAME)).unwrap_or_default()).into_owned();
        }
        v.type_name().to_owned()
    }

    // ---- raw table access ----

    #[inline]
    pub(crate) fn raw_get(&self, t: TableRef, k: Value) -> Value {
        self.heap.tables.get(t.idx).map_or(Value::Nil, |t| self.heap.get_in(t, k))
    }

    pub(crate) fn raw_set(&mut self, t: TableRef, k: Value, v: Value) -> Result<(), LuaError> {
        let settled = self.settle_key(k)?;
        // Tables hold interned keys only.
        let k = match (k, settled) {
            (Value::Str(r), Settled::Absent(best)) => Value::Str(self.heap.canon_absent(r, best)),
            (Value::Str(r), _) => Value::Str(self.heap.canon(r)),
            _ => k,
        };
        // The table the collector traverses in chunks: the entry overwritten is marked
        // first, the entry written after (S6GC-2e).
        let part = t.idx == self.heap.part;
        let held = if part { self.heap.part_writing(t.idx, k) } else { k };
        self.heap.mem.fuel = self.call_fuel();
        self.heap.mem.fuel_watchable = true;
        let r = match self.heap.tables.get_mut(t.idx) {
            Some(tab) => tab.set(k, v, &mut self.heap.mem),
            None => Ok(()),
        };
        self.heap.mem.fuel_watchable = false;
        match r {
            Ok(()) => {}
            Err(SetError::NilKey) => return Err(LuaError::msg("table index is nil")),
            Err(SetError::NanKey) => return Err(LuaError::msg("table index is NaN")),
            // A resize stopped between pieces by the interrupt or deadline: the table is as
            // it was, and the call stops as at any other look.
            Err(SetError::Stopped(k)) => {
                if part {
                    self.heap.part_written(t.idx, held, Value::Nil);
                }
                // (What it did is charged: LR-29d, `Table::resumed`.)
                let heap = &mut self.heap;
                let units = heap.tables.get_mut(t.idx).map_or(0, |tab| tab.take_units(&heap.mem));
                let _ = self.charge(units);
                return Err(LuaError::Limit(k));
            }
            Err(SetError::Fuel) => {
                if part {
                    self.heap.part_written(t.idx, held, Value::Nil);
                }
                (self.fuel, self.reserve) = (-1, 0);
                return Err(LuaError::Limit(LimitKind::Fuel));
            }
            Err(SetError::Memory) => {
                self.emergency_gc()?;
                let r = match self.heap.tables.get_mut(t.idx) {
                    Some(tab) => tab.set(k, v, &mut self.heap.mem),
                    None => Ok(()),
                };
                if r.is_err() {
                    if part {
                        self.heap.part_written(t.idx, held, Value::Nil);
                    }
                    return Err(self.memory_error(false));
                }
            }
        }
        if part {
            self.heap.part_written(t.idx, held, v);
        }
        if v.is_collectable() || k.is_collectable() {
            self.heap.barrier_table(t);
        }
        // Resizing is work: charged by the slots it copied.
        let heap = &mut self.heap;
        let resized = heap.tables.get_mut(t.idx).map_or(0, |tab| tab.take_resized(&heap.mem));
        if resized >= 16 {
            self.charge(resized as u64 / 16)?;
        }
        Ok(())
    }

    pub(crate) fn raw_len(&self, t: TableRef) -> i64 {
        self.heap.tables.get(t.idx).map_or(0, |t| t.border())
    }

    // ---- upvalues ----

    #[inline]
    fn upval_ref(&self, closure: FuncRef, u: u16) -> Option<u32> {
        match self.heap.funcs.get(closure.idx) {
            Some(Func::Lua { upvals, .. }) => upvals.get(usize::from(u)).map(|r| r.idx),
            _ => None,
        }
    }

    #[inline]
    pub(crate) fn upval_get(&self, closure: FuncRef, u: u16) -> Value {
        let Some(r) = self.upval_ref(closure, u) else { return Value::Nil };
        self.upval_read(r)
    }

    pub(crate) fn upval_read(&self, r: u32) -> Value {
        match self.heap.upvals.get(r) {
            Some(Upval::Closed(v)) => *v,
            Some(Upval::Open { thread, idx }) => {
                if *thread == self.cur_ref {
                    self.st(*idx)
                } else {
                    self.heap.threads.get(thread.idx).and_then(|t| t.stack.get(*idx).copied()).unwrap_or_default()
                }
            }
            None => Value::Nil,
        }
    }

    fn upval_set(&mut self, closure: FuncRef, u: u16, v: Value) {
        let Some(r) = self.upval_ref(closure, u) else { return };
        match self.heap.upvals.get(r).copied() {
            Some(Upval::Closed(_)) => {
                if let Some(slot) = self.heap.upvals.get_mut(r) {
                    *slot = Upval::Closed(v);
                }
                if v.is_collectable() {
                    self.heap.barrier_upval(r);
                }
            }
            Some(Upval::Open { thread, idx }) => {
                if thread == self.cur_ref {
                    self.set_st(idx, v);
                } else if let Some(t) = self.heap.threads.get_mut(thread.idx)
                    && let Some(s) = t.stack.get_mut(idx)
                {
                    *s = v;
                }
            }
            None => {}
        }
    }

    /// The innermost to-be-closed value at or above `level`, taken off the list.
    fn pop_tbc(&mut self, level: usize) -> Option<Value> {
        let slot = self.cur.tbc.last().copied().filter(|&slot| slot >= level)?;
        self.cur.tbc.pop();
        Some(self.st(slot))
    }

    /// Closes the running thread's upvalues at or above `level`.
    pub(crate) fn close_upvals(&mut self, level: usize) {
        while let Some(&(idx, r)) = self.cur.open_upvals.last() {
            if idx < level {
                break;
            }
            self.cur.open_upvals.pop();
            let v = self.st(idx);
            if let Some(slot) = self.heap.upvals.get_mut(r) {
                *slot = Upval::Closed(v);
            }
            if v.is_collectable() {
                self.heap.barrier_upval(r);
            }
        }
    }

    /// Closes upvalues and to-be-closed variables at or above `level` (normal exit).
    pub(crate) fn close_level(&mut self, level: usize) -> Result<(), LuaError> {
        self.close_upvals(level);
        while let Some(v) = self.pop_tbc(level) {
            self.call_close(v, Value::Nil)?;
        }
        Ok(())
    }

    /// Closes at or above `level` while an error unwinds: each `__close` gets the error,
    /// and an error in one replaces it. Returns the error to go on with.
    pub(crate) fn close_with_error(&mut self, level: usize, mut err: Value) -> Result<Value, LuaError> {
        self.close_upvals(level);
        while let Some(v) = self.pop_tbc(level) {
            match self.call_close(v, err) {
                Ok(()) => {}
                Err(e) if e.limit().is_some() => return Err(e),
                Err(e) => err = self.normalize(e)?,
            }
        }
        Ok(err)
    }

    /// Closes the to-be-closed variables of the protected call of frame `i` while an error
    /// unwinds to it (the error in its `dest + 1` slot), then makes the call return
    /// `false, error`. A Lua `__close` runs as a frame of its own that continues here when
    /// it returns (`Cont::ErrClosing`); an error in one replaces the error (`on_error`
    /// comes back here). Only limits stop it.
    fn close_error_async(&mut self, i: usize) -> Result<(), LuaError> {
        let Some(fr) = self.cur.frames.get(i) else { return Ok(()) };
        let FrameKind::Protected { dest, wanted, cont } = fr.kind else { return Ok(()) };
        let level = fr.func;
        let mut err = self.st(dest + 1);
        self.close_upvals(level);
        while let Some(v) = self.pop_tbc(level) {
            let mm = self.tm(v, Tm::Close);
            if matches!(mm, Value::Func(f) if matches!(self.heap.funcs.get(f.idx), Some(Func::Lua { .. }))) {
                let pos = self.call_top();
                let called = self.place_call(pos, mm, &[v, err]).and_then(|()| {
                    self.precall_event(Some("close"), pos, 2, 0, FrameKind::Cont(Cont::ErrClosing { frame: i }))
                });
                match called {
                    Ok(_) => return Ok(()),
                    Err(e) if e.limit().is_some() => return Err(e),
                    Err(e) => {
                        err = self.normalize(e)?;
                        err = self.close_error_handled(i, err)?;
                        self.set_st(dest + 1, err);
                        continue;
                    }
                }
            }
            let outer = self.err_closing.replace((i, self.cur.frames.len()));
            let r = if self.tm(v, Tm::Close).is_nil() {
                // C Lua calls the missing method from C: no name for it, no position.
                Err(LuaError::Plain("attempt to call a nil value".into()))
            } else {
                self.call_close(v, err)
            };
            let r = match r {
                Ok(()) => Ok(None),
                Err(e) if e.limit().is_some() => Err(e),
                Err(e) => self.normalize(e).map(Some),
            };
            self.err_closing = outer;
            if let Some(e) = r? {
                err = self.close_error_handled(i, e)?;
                self.set_st(dest + 1, err);
            }
        }
        self.cur.frames.truncate(i);
        self.set_st(dest, Value::Bool(false));
        self.set_st(dest + 1, err);
        self.move_results(dest, dest, 2, wanted)?;
        if let Some(c) = cont {
            self.apply_cont(c, Value::Bool(false))?;
        }
        Ok(())
    }

    /// An error a `__close` raised while an error unwinds to the protected call of frame
    /// `i`: through that call's message handler, if it has one (S5C-COR-3).
    fn close_error_handled(&mut self, i: usize, err: Value) -> Result<Value, LuaError> {
        match self.cur.frames.get(i).and_then(|f| f.handler) {
            Some(h) => self.call_handler(h, err),
            None => Ok(err),
        }
    }

    fn call_close(&mut self, v: Value, err: Value) -> Result<(), LuaError> {
        let mm = self.tm(v, Tm::Close);
        if mm.is_nil() {
            // Checked when declared; gone now: calling it fails, as in C Lua.
            return Err(LuaError::msg("attempt to call a nil value (metamethod 'close')"));
        }
        self.call_nested(mm, &[v, err]).map(|_| ())
    }

    // ---- calls ----

    /// Starts a call of the value at `func` with `nargs` arguments above it.
    #[inline]
    pub(crate) fn precall(
        &mut self,
        func: usize,
        nargs: usize,
        nres: i32,
        kind: FrameKind,
        from_lua: bool,
    ) -> Result<Called, LuaError> {
        match self.precall_one(func, nargs, nres, kind, from_lua) {
            Ok(Called::Done) if self.nested_pending.is_some() => self.precall_nested(),
            r => r,
        }
    }

    /// The callees of nested protected calls (`nested_pending`), one after the other
    /// (S5COR-6b). Each is called as a protected call's callee with the kind that completes
    /// it and every enclosing one (`Cont::Nest`), and gets the innermost one's handler; a
    /// value that cannot be called makes the innermost return `false, error`.
    #[cold]
    fn precall_nested(&mut self) -> Result<Called, LuaError> {
        loop {
            let Some(NestedCall { func, nargs, kind, handler }) = self.nested_pending.take() else {
                return Err(LuaError::msg("internal: no nested call"));
            };
            let FrameKind::Protected { dest, cont: Some(nest), .. } = kind else {
                return Err(LuaError::msg("internal: nested call without its protected calls"));
            };
            match self.precall_one(func, nargs, -1, kind, false) {
                Ok(Called::Done) if self.nested_pending.is_some() => {}
                Ok(Called::Lua) => {
                    self.set_handler(dest, handler);
                    return Ok(Called::Lua);
                }
                // (A call that finished at once completed every level: `finish_call`.)
                Ok(other) => return Ok(other),
                Err(e) if e.limit().is_some() => return Err(e),
                Err(e) => {
                    self.pcall_failed(e, handler, dest, -1)?;
                    // Finished at once: a metamethod's caller (`call_meta`) reads the result.
                    self.complete_nest(nest, false)?;
                    return Ok(Called::Done);
                }
            }
        }
    }

    /// The handler of the protected call whose frame was just pushed for `dest` (the
    /// callee's own frame: not that of a protected call it made in turn, which
    /// `pcall(xpcall, f, h)` puts on top).
    fn set_handler(&mut self, dest: usize, handler: Option<Value>) {
        if let Some(f) = self.cur.frames.last_mut()
            && matches!(f.kind, FrameKind::Protected { dest: d, .. } if d == dest)
        {
            f.handler = handler;
        }
    }

    /// A protected call whose callee could not be called: `false` and the error (through
    /// `handler`) as its results at `dest`.
    fn pcall_failed(&mut self, e: LuaError, handler: Option<Value>, dest: usize, wanted: i32) -> Result<(), LuaError> {
        let mut ev = self.normalize(c_level(e))?;
        if let Some(h) = handler {
            ev = self.call_handler(h, ev)?;
        }
        self.set_st(dest, Value::Bool(false));
        self.set_st(dest + 1, ev);
        self.move_results(dest, dest, 2, wanted)
    }

    /// One step of `precall`: may leave a nested protected call's callee to call next.
    #[inline]
    fn precall_one(
        &mut self,
        func: usize,
        nargs: usize,
        nres: i32,
        kind: FrameKind,
        from_lua: bool,
    ) -> Result<Called, LuaError> {
        let mut nargs = nargs;
        let mut fv = self.st(func);
        for _ in 0..MAX_TAG_LOOP {
            let Value::Func(f) = fv else {
                let mm = self.tm(fv, Tm::Call);
                if mm.is_nil() {
                    return Err(ErrorKind::Call { ty: self.obj_type_name(fv) }.into());
                }
                // Each link moves the arguments: charged by their number.
                self.charge(1 + nargs as u64 / 8)?;
                self.ensure_stack(func + nargs + 2 + EXTRA_STACK)?;
                self.cur.stack.copy_within(func..func + 1 + nargs, func + 1);
                self.set_st(func, mm);
                nargs += 1;
                fv = mm;
                continue;
            };
            enum K {
                Lua(u32),
                Native(NativeFn, &'static str),
                Host(crate::heap::HostFn, &'static str),
            }
            let k = match self.heap.funcs.get(f.idx) {
                Some(Func::Lua { proto, .. }) => K::Lua(*proto),
                Some(Func::Native { f, name, .. }) => K::Native(*f, name),
                Some(Func::Host { f, .. }) => K::Host(Arc::clone(f), "?"),
                None => return Err(LuaError::msg("attempt to call a dead function")),
            };
            return match k {
                K::Lua(proto) => self.push_frame(f, proto, func, nargs, nres, kind, from_lua),
                K::Native(nf, name) => self.call_native(func, nargs, nres, kind, from_lua, name, |l, a| nf(l, a)),
                K::Host(hf, name) => self.call_native(func, nargs, nres, kind, from_lua, name, |l, a| hf(l, a)),
            };
        }
        Err(LuaError::msg("'__call' chain too long; possible loop"))
    }

    #[allow(clippy::too_many_arguments, reason = "the call protocol's own state")]
    fn push_frame(
        &mut self,
        closure: FuncRef,
        proto: u32,
        func: usize,
        nargs: usize,
        nres: i32,
        kind: FrameKind,
        from_lua: bool,
    ) -> Result<Called, LuaError> {
        if self.cur.frames.len() >= self.limits.call_depth {
            return Err(LuaError::msg("stack overflow"));
        }
        if !matches!(kind, FrameKind::Normal) && self.cur.frames.len() < self.cur.unrolled {
            // Everything from here up was popped since the resume: this frame is new.
            self.cur.unrolled = self.cur.frames.len();
        }
        // Before the window is made: charging may collect, which may shrink the stack to
        // the frames it has.
        if self.cur.frames.len() == self.cur.frames.capacity() {
            self.grow_frames()?;
        }
        let Some(p) = self.proto(proto) else { return Err(LuaError::msg("internal: no prototype")) };
        let np = p.num_params as usize;
        let is_vararg = p.is_vararg;
        let width = p.window();
        let (base, vstart, vcount) = if is_vararg && nargs > np {
            // Arguments are moved: work by their number.
            if nargs > 16 {
                self.charge(nargs as u64 / 8)?;
            }
            let nb = func + 1 + nargs;
            // (Parameters past one window are written here too: `np`.)
            self.ensure_window(nb, width.max(np))?;
            for i in 0..np {
                let v = self.st(func + 1 + i);
                self.set_st(nb + i, v);
            }
            (nb, func + 1 + np, nargs - np)
        } else {
            self.ensure_window(func + 1, width.max(np))?;
            // Missing parameters (SEC-M7: a function may have thousands).
            self.charge(list_fuel(np.saturating_sub(nargs)))?;
            for i in nargs..np {
                self.set_st(func + 1 + i, Value::Nil);
            }
            (func + 1, func + 1 + np, 0)
        };
        self.cur.frames.push(Frame {
            closure,
            proto,
            func,
            base,
            pc: 0,
            nresults: nres,
            vstart,
            vcount,
            kind,
            from_lua,
            tail: false,
            handler: None,
        });
        Ok(Called::Lua)
    }

    #[allow(clippy::too_many_arguments, reason = "the call protocol's own state")]
    #[inline]
    fn call_native(
        &mut self,
        func: usize,
        nargs: usize,
        nres: i32,
        kind: FrameKind,
        from_lua: bool,
        name: &'static str,
        f: impl Fn(&mut Lua, Args) -> Result<Flow, LuaError>,
    ) -> Result<Called, LuaError> {
        self.ensure_stack(func + 1 + nargs + EXTRA_STACK)?;
        self.native_calls = self.native_calls.wrapping_add(1);
        self.rets.clear();
        let saved = (self.ntop, self.native_from_lua, self.native_name);
        let saved_event = crate::nostd::mem::replace(&mut self.native_event, self.pending_event.take());
        self.ntop = func + 1 + nargs;
        self.native_from_lua = from_lua;
        self.native_name = name;
        let mark = self.scratch.len();
        let floor = crate::nostd::mem::replace(&mut self.scratch_floor, mark);
        self.native_depth += 1;
        // A function `pcall` called directly sits on `pcall`, which has returned already:
        // it counts as an activation of its own (for error levels), and so does each
        // enclosing protected call it completes (`pcall(pcall, f)`).
        let under_pcall = if from_lua { 0 } else { pcall_levels(kind) };
        // (The protected calls' own slots are just below, outermost first.)
        // Its C levels: those protected calls, and the metamethod call it is (S5COR-6c).
        let c_levels = if from_lua { 0 } else { c_calls(kind) };
        self.protected_natives += c_levels;
        self.native_stack.push((self.cur_ref, self.cur.frames.len(), func, under_pcall));
        let owed_before = crate::stdlib::owed();
        let held_before = crate::stdlib::held_back();
        let r = f(self, Args { func, base: func + 1, n: nargs });
        // Buffers the function counted against memory and dropped on the way out (SEC-M1).
        let back = crate::stdlib::held_back().saturating_sub(held_before);
        if back > 0 {
            crate::stdlib::set_held_back(held_before);
            self.heap.mem.release(back);
        }
        // The work the function recorded (`stdlib::owe`, `OutBuf`), paid on every exit:
        // an error after building output is not free. Nested calls took their own part.
        let done = crate::stdlib::owed().saturating_sub(owed_before);
        crate::stdlib::set_owed(owed_before);
        let r = match (done, r) {
            (0, r) => r,
            (n, r) => match self.charge(n) {
                Err(e) => Err(e),
                Ok(()) => r,
            },
        };
        self.native_depth -= 1;
        self.scratch.truncate(mark);
        self.scratch_floor = floor;
        // A limit hit under this native (a host function that swallowed it) still stops
        // the call.
        let flow = match (r, self.limit_hit) {
            (Ok(fl), None) => fl,
            (Ok(_), Some(k)) => {
                let e = self.native_failed(LuaError::Limit(k), saved, (under_pcall, c_levels));
                self.native_event = saved_event;
                return Err(e);
            }
            (Err(e), _) => {
                let e = self.native_failed(e, saved, (under_pcall, c_levels));
                self.native_event = saved_event;
                return Err(e);
            }
        };
        self.native_stack.pop();
        self.protected_natives -= c_levels;
        (self.ntop, self.native_from_lua, self.native_name) = saved;
        self.native_event = saved_event;
        match (flow, kind) {
            // The common case: results straight into place.
            (Flow::Return, FrameKind::Normal) => {
                let n = self.rets.len();
                let want = if nres < 0 { n } else { nres as usize };
                // The results, or the nils up to what is wanted when more (SEC-M7).
                self.charge(list_fuel(n.max(want)))?;
                let need = func + n.max(want) + 2;
                // One result into a stack long enough already (`place_rets` does no more
                // then): stored here, without the call.
                match self.rets.first() {
                    Some(&v) if n == 1 && need <= self.cur.stack.len() => {
                        if let Some(d) = self.cur.stack.get_mut(func) {
                            *d = v;
                        }
                    }
                    _ => self.place_rets(func, need)?,
                }
                if let Some(rest) = self.cur.stack.get_mut(func + n..func + want) {
                    rest.fill(Value::Nil);
                }
                if nres < 0 {
                    self.top = func + n;
                }
                self.rets.clear();
                Ok(Called::Done)
            }
            (flow, kind) => self.native_flow(flow, func, nres, kind, from_lua),
        }
    }

    /// A call from Lua code to a library function marked to run in place
    /// (`Func::Native::in_place`, `string::in_place`), made without `call_native`'s set-up:
    /// the same results, fuel, memory and stack. None when it cannot: `call_native` makes
    /// the call then, from the start (`in_place` leaves nothing done).
    #[inline(never)]
    fn call_in_place(&mut self, op: u8, func: usize, nargs: usize, nres: i32) -> Option<Result<Called, LuaError>> {
        let want = usize::try_from(nres).unwrap_or(0);
        // The stack `call_native` makes sure of, room for two results and the nils wanted
        // (more than 16 are paid: SEC-M7), and no limit hit pending (it stops the call).
        // (Off with the fast loop: the tests' reference path is `call_native`.)
        if !self.fast || self.limit_hit.is_some() || nargs > 4 || want > 16 || self.cur.stack.len() < (func + 1 + nargs + EXTRA_STACK).max(func + want.max(2) + 2) {
            return None;
        }
        let mut args = [Value::Nil; 4];
        for (i, a) in args.iter_mut().enumerate().take(nargs) {
            *a = self.st(func + 1 + i);
        }
        // (Its fuel is the fuel in hand before the call's step.)
        let (r, n, cost) = crate::stdlib::string::in_place(&mut self.heap, self.names.types(), &mut self.match_state, usize::from(op - 1), args.get(..nargs)?, self.fuel + 1, self.limits.string_len)?;
        self.pending_event = None;
        self.fuel -= cost;
        for i in 0..n.max(want) {
            self.set_st(func + i, r.get(i).copied().unwrap_or(Value::Nil));
        }
        if nres < 0 {
            self.top = func + n;
        }
        // The interner's walk past the fuel in hand: the next slice, or the stop, where
        // `new_str` would take it. (The collector's step after it is the call's.)
        if self.fuel < 0
            && let Err(e) = self.refuel()
        {
            return Some(Err(e));
        }
        Some(Ok(Called::Done))
    }

    /// `coroutine.yield(...)` called from Lua code: what `call_native` does for it, without
    /// the set-up it undoes before a yield is handled (`native_flow`): `yield_` only gathers
    /// its arguments and cannot fail, owes no fuel and calls nothing. (With a limit hit
    /// pending, `call_native` raises it: the caller takes that path then.)
    ///
    /// What `call_native` leaves behind for a native called from Lua code that returns
    /// `Flow::Yield`, item by item: `ntop`, `native_from_lua`, `native_name`,
    /// `native_event`, the scratch floor and `native_depth` are put back; the
    /// `native_stack` entry is pushed and popped; `c_calls` and `pcall_levels` are 0 from
    /// Lua code, so `protected_natives` does not move; `yield_` owes no fuel. The one
    /// lasting effect is `pending_event` taken (cleared): done here too.
    fn yield_from_lua(&mut self, func: usize, nargs: usize, nres: i32) -> Result<Called, LuaError> {
        self.ensure_stack(func + 1 + nargs + EXTRA_STACK)?;
        self.pending_event = None;
        self.rets.clear();
        for i in 0..nargs {
            let v = self.st(func + 1 + i);
            self.rets.push(v);
        }
        self.native_flow(Flow::Yield, func, nres, FrameKind::Normal, true)
    }

    /// Copies a native's results (`rets`) to the stack from `func`, which is made `need`
    /// slots at least. S6-5: many results (`string.byte`, `table.unpack`) are written once,
    /// not over a stack first filled with nils, and the host's interrupt and deadline are
    /// looked at as they go: 3M of them ran 54 to 74 ms past a 20 ms time limit.
    fn place_rets(&mut self, func: usize, need: usize) -> Result<(), LuaError> {
        let n = self.rets.len();
        if n <= crate::stdlib::NATIVE_CHUNK {
            self.ensure_stack(need)?;
            if let Some(dst) = self.cur.stack.get_mut(func..func + n) {
                // (A loop: results are few, and `copy_from_slice` is a call to memcpy.)
                for (d, v) in dst.iter_mut().zip(&self.rets) {
                    *d = *v;
                }
            }
            return Ok(());
        }
        // SEC-M15: the list is counted while the stack grows beside it, and not kept after
        // (kept, its capacity stayed outside the account for good: 14 MiB after a
        // `string.byte` of 900,000).
        // (Not debt: the collector's pace is as before.)
        let mut hold = crate::stdlib::MemHold::default();
        let rets = crate::nostd::mem::take(&mut self.rets);
        let bytes = rets.capacity().saturating_mul(crate::nostd::mem::size_of::<Value>());
        let r = hold.cover(self, bytes).and_then(|()| self.stack_room(need));
        if let Err(e) = r {
            hold.give_back(self);
            return Err(e);
        }
        let mut stopped = Ok(());
        let mut at = func;
        for part in rets.chunks(crate::stdlib::NATIVE_CHUNK) {
            let end = at + part.len();
            if let Some(dst) = self.cur.stack.get_mut(at..end) {
                dst.copy_from_slice(part);
            } else {
                self.cur.stack.truncate(at);
                self.cur.stack.resize(at, Value::Nil);
                self.cur.stack.extend_from_slice(part);
            }
            at = end;
            if let Err(e) = self.check_time() {
                stopped = Err(e);
                break;
            }
        }
        drop(rets);
        hold.give_back(self);
        // On a stop the stack is left as far as it was written: past the first chunk, so
        // past every frame's register window (filling the rest took 5 to 10 ms).
        stopped?;
        if self.cur.stack.len() < need {
            self.cur.stack.resize(need, Value::Nil);
        }
        Ok(())
    }

    /// A native's error, given its position and name while the native is still the
    /// running function; the call state is put back.
    #[cold]
    fn native_failed(&mut self, e: LuaError, saved: (usize, bool, &'static str), (under_pcall, c_levels): (usize, usize)) -> LuaError {
        // A long result list an error cut short is not kept: past the call it was outside
        // the account for good (SEC7-1: 39 MiB after a stopped `table.unpack`).
        clear_list(&mut self.rets);
        let e = match e.kind() {
            ErrorKind::Limit(_) | ErrorKind::Value(_) => e,
            _ => match self.normalize(e) {
                Ok(v) => LuaError::Value(v),
                Err(e) => e,
            },
        };
        // The error's message handler (`xpcall`) runs where it was raised, as in C Lua:
        // this native is still a level for `debug.traceback` there (`handler_native`).
        if let ErrorKind::Value(v) = *e.kind() {
            self.err_native = self.native_stack.last().map(|&(t, d, slot, _)| ((t, d, slot), under_pcall, v));
        }
        self.native_stack.pop();
        self.protected_natives -= c_levels;
        (self.ntop, self.native_from_lua, self.native_name) = saved;
        e
    }

    /// What a native asked for besides returning to a plain call: results through a
    /// protected or continued frame, a protected call, a yield or a resume.
    fn native_flow(&mut self, flow: Flow, func: usize, nres: i32, kind: FrameKind, from_lua: bool) -> Result<Called, LuaError> {
        match flow {
            Flow::Return => {
                let n = self.rets.len();
                self.place_rets(func, func + n.max(nres.max(0) as usize) + 2)?;
                self.rets.clear();
                match kind {
                    FrameKind::Cont(_) => self.move_results(func, func, n, nres)?,
                    k => self.finish_call(k, func, func, n, nres, false)?,
                }
                Ok(Called::Done)
            }
            Flow::PCall { handler, nargs: fnargs } if let Some(nest) = nest_of(kind, func) => {
                // S5COR-6 / S5COR-6b: `pcall(pcall, f)` (or `xpcall`, either way round, any
                // depth). This protected call's callee gets one frame (or pending call) that
                // completes it and every enclosing one (`Cont::Nest`): `f` runs as in a
                // plain protected call, so it may yield, and the depth costs no native
                // stack. (The enclosing calls' slots are `dest`, `dest + 1`, ..., `func`.)
                let pkind = FrameKind::Protected { dest: func, wanted: -1, cont: Some(nest) };
                // `precall` calls it (not here: the callee may be a protected call too).
                self.nested_pending = Some(NestedCall { func: func + 1, nargs: fnargs, kind: pkind, handler });
                Ok(Called::Done)
            }
            Flow::PCall { handler, nargs: fnargs } if matches!(kind, FrameKind::Protected { .. }) => {
                // A protected call whose continuation a `Cont::Nest` cannot carry (none the
                // machine makes: `nest_of` packs every metamethod's): run as a nested call
                // of its own, which ends with its status and results as a plain return
                // that the outer call then wraps. Not yieldable inside.
                let me = self.st(func);
                let mut args = Vec::with_capacity(fnargs + 2);
                args.push(self.st(func + 1));
                if let Some(h) = handler {
                    args.push(h);
                }
                args.extend((0..fnargs).map(|i| self.st(func + 2 + i)));
                let out = self.call_nested(me, &args)?;
                self.rets.clear();
                self.rets.extend_from_slice(&out);
                self.native_flow(Flow::Return, func, nres, kind, from_lua)
            }
            Flow::PCall { handler, nargs: fnargs } => {
                let cont = match kind {
                    FrameKind::Cont(c) => Some(c),
                    _ => None,
                };
                let pkind = FrameKind::Protected { dest: func, wanted: nres, cont };
                match self.precall(func + 1, fnargs, -1, pkind, false) {
                    Ok(Called::Lua) => {
                        self.set_handler(func, handler);
                        Ok(Called::Lua)
                    }
                    Ok(other) => Ok(other),
                    Err(e) if e.limit().is_some() => Err(e),
                    Err(e) => {
                        self.pcall_failed(e, handler, func, nres)?;
                        Ok(Called::Done)
                    }
                }
            }
            Flow::Yield => {
                self.do_yield(func, nres, kind)?;
                Ok(Called::Switched)
            }
            Flow::Resume { co, wrap } => self.do_resume(func, nres, co, wrap, from_lua, kind),
            Flow::Call3 { f, arg } => {
                // A Lua function into a plain call: a frame in the native's place, which
                // sits on it (from Rust) for error levels.
                let is_lua = matches!(f, Value::Func(r) if matches!(self.heap.funcs.get(r.idx), Some(Func::Lua { .. })));
                if is_lua && matches!(kind, FrameKind::Normal) {
                    self.ensure_stack(func + 2 + EXTRA_STACK)?;
                    self.set_st(func, f);
                    self.set_st(func + 1, arg);
                    return self.precall(func, 1, -1, FrameKind::Cont(Cont::Three { wanted: nres }), false);
                }
                // Under `pcall` (`pcall(pairs, t)`): the same frame, protected, its results cut
                // to three when they arrive (so `__pairs` may yield there too).
                // S5COR-6c: under nested protected calls (`pcall(pcall, pairs, t)`), or a
                // protected call or `pairs` itself used as a metamethod, the same: the
                // frame's `Cont::Nest` cuts them to three, then completes the rest.
                let three = match kind {
                    _ if !is_lua => None,
                    FrameKind::Protected { dest, wanted, cont: None } => {
                        Some(FrameKind::Protected { dest, wanted, cont: Some(Cont::Three { wanted }) })
                    }
                    FrameKind::Protected { dest, wanted, cont: Some(Cont::Nest { dest: d, bits, wanted: w, back, n }) } => {
                        // (Never `three` already: that kind is a Lua `__pairs` frame's.)
                        let nest = Cont::Nest { dest: d, bits: bits.with_three(), wanted: w, back, n };
                        Some(FrameKind::Protected { dest, wanted, cont: Some(nest) })
                    }
                    FrameKind::Protected { dest, wanted, cont: Some(c) } => pack_then(c, dest).and_then(|(then, back, n)| {
                        let nest = Cont::Nest { dest, bits: NestBits::new(0, then, true)?, wanted, back, n };
                        Some(FrameKind::Protected { dest, wanted: -1, cont: Some(nest) })
                    }),
                    FrameKind::Cont(c) => pack_then(c, func).and_then(|(then, back, n)| {
                        let nest = Cont::Nest { dest: func, bits: NestBits::new(0, then, true)?, wanted: nres, back, n };
                        Some(FrameKind::Cont(nest))
                    }),
                    FrameKind::Normal => None,
                };
                if let Some(kind) = three {
                    self.ensure_stack(func + 2 + EXTRA_STACK)?;
                    self.set_st(func, f);
                    self.set_st(func + 1, arg);
                    return self.precall(func, 1, -1, kind, false);
                }
                // shortcut: a callable that is not a Lua function runs to completion, not
                // yieldable.
                let mut out = self.call_nested(f, &[arg])?;
                out.resize(3, Value::Nil);
                self.rets.clear();
                self.rets.extend_from_slice(&out);
                self.native_flow(Flow::Return, func, nres, kind, from_lua)
            }
        }
    }

    /// C Lua's count of C levels in use here (`nCcalls`), for `call_handler`: nested library
    /// callbacks, each protected call (`pcall` is a C function there) and metamethod call on
    /// this thread and on the threads that resumed it, and each resume. Walks at most until the count passes
    /// C Lua's limit (only on the error path). Also returns the frames walked.
    fn c_levels(&self) -> (usize, usize) {
        // Frames C Lua calls from C: protected calls, and metamethods, `__close` and
        // `__pairs` (continued frames here).
        // (Frames a resume after a yield unrolled count no more, as in C Lua.)
        let protected = |t: &ThreadState| {
            t.frames.iter().skip(t.unrolled).map(|f| c_calls(f.kind)).sum::<usize>()
        };
        let mut walked = self.cur.frames.len();
        let mut level = self.c_depth + self.protected_natives + HANDLER_BASE_LEVELS + protected(&self.cur);
        let mut next = self.cur.resumer;
        while let Some(r) = next {
            if level > 2 * C_HANDLER_LEVELS {
                break;
            }
            let Some(t) = self.heap.threads.get(r.idx) else { break };
            // A `pcall` of the resuming function itself (`pcall(coroutine.wrap(f))`) has no
            // frame of its own: it is the pending resume's kind.
            let pcall_of_resume = t.resume_pending.as_ref().map_or(0, |p| if p.from_lua { 0 } else { c_calls(p.kind) });
            level += RESUME_LEVELS + protected(t) + pcall_of_resume;
            walked += t.frames.len();
            next = t.resumer;
        }
        (level, walked)
    }

    /// An upper bound, in constant time, of the C levels this thread's frames and its
    /// resumers hold (`c_levels` without the VM-wide terms), for a call at `func`: a frame
    /// holds at most three (a protected call, a metamethod, `pairs`) besides the protected
    /// calls a nested one completes (`Cont::Nest`), which are as many as the stack slots
    /// between its `dest` and `func`; those ranges are disjoint and below `func`. And frames'
    /// call slots rise one above the other, so there are at most `func + 1` of them.
    #[inline]
    fn c_bound_here(&self, func: usize) -> usize {
        4 * (func + 1) + self.cur.c_bound
    }

    /// S6-7: the position C Lua gives the "C stack overflow" raised by the call at level
    /// `at` above a handler whose first frame is `from`: the Lua caller of the metamethod
    /// frame there. A library function calling back (`sort`'s comparator, `__tostring` from
    /// `tostring`) is a level of its own, at the frame its call made (`native_stack`); its
    /// call falling on the 200th is raised in it, with no position.
    fn cross_where(&self, from: usize, at: usize) -> Option<String> {
        let mut held = 0;
        for i in from..self.cur.frames.len() {
            let f = self.cur.frames.get(i)?;
            let natives = self.native_stack.iter().filter(|&&(t, d, ..)| t == self.cur_ref && d == i).count();
            if natives > 0 && held + natives >= at {
                return None;
            }
            held += natives + c_calls(f.kind);
            if held >= at {
                if held != at || !matches!(f.kind, FrameKind::Cont(_)) {
                    return None;
                }
                let caller = self.cur.frames.get(i.checked_sub(1)?)?;
                let w = self.frame_where(caller);
                return (!w.is_empty()).then_some(w);
            }
        }
        None
    }

    /// Calls `xpcall`'s handler on an error value (non-yieldable). S5COR-5: as in C Lua, the
    /// handler stays in place while it runs, so an error it raises is handled by calling it
    /// again with that error (C Lua's `luaG_errormsg` runs inside the raise); its first
    /// result becomes the error. Ours gave "error in error handling" at the first one. A
    /// handler that keeps failing gets "C stack overflow" from C Lua's 200 C levels on and
    /// stops at 220 with "error in error handling" (LUA_ERRERR): the same counts here,
    /// from the native levels already in use. A loop, not recursion: the native stack does
    /// not grow with the chain.
    fn call_handler(&mut self, h: Value, err: Value) -> Result<Value, LuaError> {
        // The native that raised this error, if one did (the error value tells a stale
        // record from the error being handled).
        let raised = match self.err_native.take() {
            Some((n, under, v)) if self.raw_equal(&v, &err) => Some((n, under)),
            _ => None,
        };
        let saved = crate::nostd::mem::replace(&mut self.handler_native, raised);
        let watch = crate::nostd::mem::replace(&mut self.handler_raise, HandlerWatch { thread: Some(self.cur_ref), ..HandlerWatch::default() });
        let r = self.call_handler_at(h, err);
        self.handler_native = saved;
        self.handler_raise = watch;
        r
    }

    fn call_handler_at(&mut self, h: Value, err: Value) -> Result<Value, LuaError> {
        const C_LEVELS: usize = C_HANDLER_LEVELS;
        const ERRERR_AT: usize = C_LEVELS / 10 * 11;
        let mut err = err;
        let (level, walked) = self.c_levels();
        // (A plain error in the handler is raised one level above this: the handler's.)
        let base = level;
        // Past C Lua's limit (protected calls nested deeper than its 200 C levels, which C
        // Lua refuses and RemedyLua runs: the superset rule), the handler runs as it would at
        // C Lua's last level that still runs one: once with the error, then "C stack
        // overflow", then "error in error handling". It was never called there.
        let mut level = level.min(C_LEVELS - 1);
        // S5B-WALK-4: the walk goes through the frames of every coroutine that resumed this
        // one, which `charge_error` (this thread's frames) does not see: 150 nested
        // coroutines 1,900 frames deep made each `xpcall` error in the innermost walk 285,000
        // frames, 440x plain code per unit of fuel. A unit per frame (`RESUMER_FRAMES_PER_FUEL`).
        self.charge((walked / RESUMER_FRAMES_PER_FUEL) as u64)?;
        loop {
            if level + 1 >= ERRERR_AT {
                return self.new_str(b"error in error handling");
            }
            if level == C_LEVELS {
                // (Positioned when the call at the 200th was a metamethod's from Lua code.)
                let msg = match self.handler_raise.pos.take() {
                    Some(w) => format!("{w}C stack overflow"),
                    None => "C stack overflow".into(),
                };
                err = self.new_str(msg.as_bytes())?;
            }
            self.handler_raise.raised = None;
            self.handler_raise.pos = None;
            // (The handler's first frame goes where the next frame would.)
            self.handler_raise.cross = (level < C_LEVELS).then(|| (self.cur.frames.len(), C_LEVELS - level));
            let r = match self.call_nested(h, &[err]) {
                Ok(v) => return Ok(v.first().copied().unwrap_or_default()),
                Err(e) if e.limit().is_some() => return Err(e),
                Err(e) => self.normalize(e),
            };
            err = r?;
            // C Lua calls the handler again where the error was raised: one level above
            // the handler's for a plain one, more inside a metamethod, a library callback
            // or a `__close` (S6-7: 216 calls against C Lua's 109 for an `__index` error).
            let next = level + self.handler_raise.raised.map_or(1, |r| r.saturating_sub(base).max(1));
            // A call at C Lua's 200th level, the handler's or one inside it, raises "C stack
            // overflow" there instead of running, and the handler is called next at the
            // 201st, which is `C_LEVELS` here (as for a plain error): a step past it lands on it.
            level = if level < C_LEVELS && next > C_LEVELS { C_LEVELS } else { next };
        }
    }

    /// Copies `n` results from `src` to `dst`, adjusted to `wanted` (-1: all, setting the
    /// top).
    pub(crate) fn move_results(&mut self, dst: usize, src: usize, n: usize, wanted: i32) -> Result<(), LuaError> {
        let want = if wanted < 0 { n } else { wanted as usize };
        // The values moved, or the nils written up to what the caller wants when more
        // (SEC-M7: a call adjusted to 30,000 results wrote them all for one unit).
        let written = n.max(want);
        if written > 16 {
            self.charge(written as u64 / 8)?;
        }
        self.ensure_stack(dst + want.max(n) + 1)?;
        if dst != src && n > 0 {
            self.cur.stack.copy_within(src..src + n, dst);
        }
        for i in n..want {
            self.set_st(dst + i, Value::Nil);
        }
        if wanted < 0 {
            self.top = dst + n;
        }
        Ok(())
    }

    /// Places a finished call's results by its kind.
    /// `apply_cont`: a `pcall` used as a metamethod passes its status on through its
    /// continuation (false when the caller reads the result itself, as `call_meta` does
    /// for calls that finish at once).
    fn finish_call(
        &mut self,
        kind: FrameKind,
        func: usize,
        src: usize,
        n: usize,
        nres: i32,
        apply_cont: bool,
    ) -> Result<(), LuaError> {
        match kind {
            FrameKind::Normal => self.move_results(func, src, n, nres),
            FrameKind::Protected { dest, wanted, cont } => {
                // `pcall(pairs, t)` with a Lua `__pairs`: its results cut to three.
                let n = if matches!(cont, Some(Cont::Three { .. })) || matches!(cont, Some(Cont::Nest { bits, .. }) if bits.three()) {
                    self.move_results(dest + 1, src, n.min(3), 3)?;
                    3
                } else {
                    self.move_results(dest + 1, src, n, -1)?;
                    n
                };
                self.set_st(dest, Value::Bool(true));
                self.move_results(dest, dest, n + 1, wanted)?;
                match cont {
                    // Enclosing protected calls are completed however this one finished;
                    // the metamethod's continuation only when asked.
                    Some(nest @ Cont::Nest { .. }) => self.complete_nest(nest, apply_cont),
                    Some(c) if apply_cont => self.apply_cont(c, Value::Bool(true)),
                    _ => Ok(()),
                }
            }
            FrameKind::Cont(nest @ Cont::Nest { dest, .. }) => {
                // `pairs` used as a metamethod (`levels` 0, `three`).
                self.move_results(dest, src, n.min(3), 3)?;
                self.move_results(dest, dest, 3, -1)?;
                self.complete_nest(nest, apply_cont)
            }
            FrameKind::Cont(Cont::Three { wanted }) => {
                self.move_results(func, src, n.min(3), 3)?;
                self.move_results(func, func, 3, wanted)
            }
            FrameKind::Cont(c) => {
                let v = if n > 0 { self.st(src) } else { Value::Nil };
                self.apply_cont(c, v)
            }
        }
    }

    /// Completes the protected calls a `Cont::Nest` carries, the innermost one's status and
    /// results from `dest + levels` up to `top`, then (`apply_then`) gives the metamethod's
    /// continuation the outermost one's first result. A caller that reads that result
    /// itself (`call_meta`, for a call that finished at once) passes false.
    fn complete_nest(&mut self, nest: Cont, apply_then: bool) -> Result<(), LuaError> {
        let Cont::Nest { dest, bits, wanted, back, n: then_n } = nest else { return Ok(()) };
        let levels = bits.levels() as usize;
        let n = self.top.saturating_sub(dest + levels);
        for j in 0..levels {
            self.set_st(dest + j, Value::Bool(true));
        }
        self.move_results(dest, dest, n + levels, wanted)?;
        match unpack_then(bits.then(), dest, back, then_n) {
            Some(c) if apply_then => {
                let v = self.st(dest);
                self.apply_cont(c, v)
            }
            _ => Ok(()),
        }
    }

    fn apply_cont(&mut self, c: Cont, v: Value) -> Result<(), LuaError> {
        match c {
            Cont::Store(slot) => self.set_st(slot, v),
            Cont::Discard | Cont::Three { .. } => {}
            Cont::ErrClosing { frame } => self.close_error_async(frame)?,
            Cont::Nest { .. } => self.complete_nest(c, true)?,
            Cont::Cond { k } => {
                if v.truthy() != k
                    && let Some(f) = self.cur.frames.last_mut()
                {
                    f.pc += 1;
                }
            }
            Cont::Concat { a, n } => {
                self.set_st(a + n - 1, v);
                // A type error after a `__concat` returned names its operand, as when the
                // instruction itself raises it.
                match self.concat(a, n).map_err(LuaError::into_kind) {
                    Ok(_) => {}
                    Err(ErrorKind::Type { op, ty, operand }) => {
                        let Some(&Frame { proto, pc, base, .. }) = self.cur.frames.last() else {
                            return Err(ErrorKind::Type { op, ty, operand }.into());
                        };
                        let r = u16::try_from((a + usize::from(operand)).saturating_sub(base)).unwrap_or(u16::MAX);
                        let at = pc.saturating_sub(1);
                        return Err(self.annotate(ErrorKind::Type { op, ty, operand: 0 }.into(), proto, at, &[crate::debug::Operand::Reg(r)]));
                    }
                    Err(e) => return Err(e.into()),
                }
            }
            Cont::Closing { level, then } => {
                let floor = match then {
                    AfterClose::Return { src, n } => src + n,
                    AfterClose::Nothing => 0,
                };
                self.close_async(level, then, floor)?;
            }
        }
        Ok(())
    }

    /// Closes upvalues and to-be-closed variables down to `level` on a normal exit. A
    /// `__close` written in Lua runs as a frame (so it may yield), continuing here when it
    /// returns. True when everything finished at once (and nothing changed the frames).
    fn close_async(&mut self, level: usize, then: AfterClose, floor: usize) -> Result<bool, LuaError> {
        self.close_upvals(level);
        while let Some(v) = self.pop_tbc(level) {
            let mm = self.tm(v, Tm::Close);
            // Not callable (gone, or never a function): the message names the event, with
            // a position, as C Lua's on a normal exit.
            if !matches!(mm, Value::Func(_)) && self.tm(mm, Tm::Call).is_nil() {
                return Err(LuaError::msg(format!("attempt to call a {} value (metamethod 'close')", self.obj_type_name(mm))));
            }
            let is_lua = matches!(mm, Value::Func(f) if matches!(self.heap.funcs.get(f.idx), Some(Func::Lua { .. })));
            if is_lua {
                let pos = self.call_top().max(floor);
                self.place_call(pos, mm, &[v, Value::Nil])?;
                self.precall_event(Some("close"), pos, 2, 0, FrameKind::Cont(Cont::Closing { level, then }))?;
                return Ok(false);
            }
            let saved = self.ntop;
            self.ntop = self.ntop.max(floor);
            let r = self.call_nested(mm, &[v, Value::Nil]);
            self.ntop = saved;
            r?;
        }
        match then {
            AfterClose::Nothing => Ok(true),
            AfterClose::Return { src, n } => {
                self.finish_return(src, n)?;
                Ok(false)
            }
        }
    }

    /// Returns from the top frame with `n` values from `src`.
    fn do_return(&mut self, src: usize, n: usize) -> Result<(), LuaError> {
        let Some(base) = self.cur.frames.last().map(|f| f.base) else { return Ok(()) };
        if self.cur.open_upvals.last().is_some_and(|(i, _)| *i >= base) {
            self.close_upvals(base);
        }
        if self.cur.tbc.last().is_some_and(|i| *i >= base) {
            // Results stay in place: `__close` calls go above them (they may be an open
            // list past the frame's registers); the return finishes after the last one.
            self.close_async(base, AfterClose::Return { src, n }, src + n)?;
            return Ok(());
        }
        self.finish_return(src, n)
    }

    /// The rest of a return, once everything is closed.
    fn finish_return(&mut self, src: usize, n: usize) -> Result<(), LuaError> {
        let Some(frame) = self.cur.frames.pop() else { return Ok(()) };
        self.finish_call(frame.kind, frame.func, src, n, frame.nresults, true)?;
        // Not a callback's return to a native body (`nny`: SEC7-V1, `wrap(table.sort)`).
        if self.cur.frames.is_empty() && self.cur_ref != self.main_ref && self.cur.resumer.is_some() && self.cur.nny == 0 {
            self.finish_coroutine()?;
        }
        Ok(())
    }

    /// Calls `f` with `args` from Rust and runs it to completion: the results.
    pub(crate) fn call_nested(&mut self, f: Value, args: &[Value]) -> Result<Vec<Value>, LuaError> {
        let (pos, keep) = self.call_nested_at(f, args)?;
        let out = self.cur.stack.get(pos..self.top).map(<[Value]>::to_vec).unwrap_or_default();
        // The results stay reachable from the native's scratch.
        if keep {
            for v in out.iter().copied().filter(|v| v.is_collectable()) {
                self.push_scratch(v);
            }
        }
        Ok(out)
    }

    /// Goes on with the call from the host that paused at a collector step: the step first,
    /// to its end (or the poll's), then the call's frames, as if it had not paused.
    pub(crate) fn resume_nested(&mut self) -> Result<Vec<Value>, LuaError> {
        let Some(call) = self.pending_call.take() else {
            return Err(LuaError::msg("no call is pending"));
        };
        self.c_depth += 1;
        self.cur.nny += 1;
        let saved = (self.native_depth, self.ntop);
        self.native_depth = 0;
        self.ntop = call.ntop;
        // (Paused between steps: no step to go on with.)
        let step = if self.gc_pending.is_some() { self.gc_step_pausable() } else { Ok(()) };
        // Ended by its step (a limit, not a pause): the instruction set to run again does
        // not, so its unit is charged again and what it was set to skip goes.
        match &step {
            Err(e) if !e.is_pending() && crate::nostd::mem::take(&mut self.rewound) => {
                self.fuel -= 1;
                self.due_skip = false;
                self.gc_call_before = None;
            }
            Ok(()) => self.rewound = false,
            Err(_) => {}
        }
        let r = step.and_then(|()| {
            // The paused instruction runs again without the step it took; with no step due
            // now it never looks.
            if !self.heap.gc_due() {
                self.due_skip = false;
            }
            self.execute(call.bthread, call.bdepth)
        });
        let ntop = self.ntop;
        self.native_depth = saved.0;
        self.ntop = saved.1;
        self.cur.nny = self.cur.nny.saturating_sub(1);
        self.c_depth -= 1;
        let (pos, keep) = self.end_nested(r, PendingCall { ntop, ..call })?;
        let out = self.cur.stack.get(pos..self.top).map(<[Value]>::to_vec).unwrap_or_default();
        if keep {
            for v in out.iter().copied().filter(|v| v.is_collectable()) {
                self.push_scratch(v);
            }
        }
        Ok(out)
    }

    /// Drops the call from the host that paused at a collector step: the step finished
    /// first (uncharged: the call is gone), so the heap is as after any step, then the
    /// frames dropped as a limit drops them.
    pub(crate) fn cancel_nested(&mut self) {
        if self.pending_start.take().is_some() {
            // Paused before it started: its step finished (uncharged), nothing else to drop.
            let (fuel, reserve) = (self.fuel, self.reserve);
            self.fuel = i64::MAX;
            let _ = self.gc_step();
            (self.fuel, self.reserve) = (fuel, reserve);
            self.gc_pending = None;
            return;
        }
        let Some(call) = self.pending_call.take() else { return };
        if self.gc_pending.is_some() {
            let (fuel, reserve) = (self.fuel, self.reserve);
            self.fuel = i64::MAX;
            // (`pause_ok` is off: it runs to its end.)
            let _ = self.gc_step();
            (self.fuel, self.reserve) = (fuel, reserve);
            self.gc_pending = None;
        }
        self.due_skip = false;
        self.gc_call_before = None;
        self.rewound = false;
        self.close_upvals(call.pos);
        self.cur.tbc.retain(|&s| s < call.pos);
        self.cur.frames.truncate(call.bdepth);
    }

    /// `call_nested` for the first result only (nil when there is none), without a vector.
    pub(crate) fn call_nested1(&mut self, f: Value, args: &[Value]) -> Result<Value, LuaError> {
        let (pos, keep) = self.call_nested_at(f, args)?;
        let v = if self.top > pos { self.st(pos) } else { Value::Nil };
        if keep && v.is_collectable() {
            self.push_scratch(v);
        }
        Ok(v)
    }

    /// Calls `f` from Rust: its results are left at `pos..top`; `keep`: a native is
    /// running, whose scratch must keep them reachable.
    fn call_nested_at(&mut self, f: Value, args: &[Value]) -> Result<(usize, bool), LuaError> {
        if self.c_depth >= self.limits.native_depth
            || self.stack_base.abs_diff(crate::stack_pos()) > self.limits.stack_bytes
        {
            // C Lua's message for its C levels (`luaE_checkcstack`), positioned as there: at
            // the running Lua function, none when a library function is running (LIBS-5:
            // a `require` cycle said "t:2: stack overflow (too many nested calls from
            // library functions)" where C Lua says "C stack overflow").
            let native_on_top =
                self.native_stack.last().is_some_and(|&(t, d, ..)| t == self.cur_ref && d == self.cur.frames.len());
            return Err(if native_on_top {
                LuaError::Plain("C stack overflow".into())
            } else {
                LuaError::msg("C stack overflow")
            });
        }
        let pos = self.call_top();
        self.ensure_stack(pos + args.len() + 1 + EXTRA_STACK)?;
        if let Some((slot, rest)) = self.cur.stack.get_mut(pos..pos + 1 + args.len()).and_then(|s| s.split_first_mut()) {
            *slot = f;
            for (d, a) in rest.iter_mut().zip(args) {
                *d = *a;
            }
        }
        let bthread = self.cur_ref;
        let bdepth = self.cur.frames.len();
        self.c_depth += 1;
        self.cur.nny += 1;
        let saved = (self.native_depth, self.ntop, self.scratch.len());
        self.native_depth = 0;
        self.ntop = pos + 1 + args.len();
        // A Lua function's frame straight away: all `precall` comes to for one.
        let called = match f {
            Value::Func(fr) if let Some(&Func::Lua { proto, .. }) = self.heap.funcs.get(fr.idx) => {
                self.push_frame(fr, proto, pos, args.len(), -1, FrameKind::Normal, false)
            }
            _ => self.precall(pos, args.len(), -1, FrameKind::Normal, false),
        };
        let r = match called {
            Ok(Called::Done) => Ok(()),
            Ok(Called::Lua) => {
                // The fast loop first, as `run`'s first step would take it (the frame was just
                // pushed with its window): a callback it finishes needs no `run` at all.
                if self.fast && self.cur.frames.last().and_then(|f| self.proto(f.proto)).is_some_and(|p| !p.wide()) {
                    let ctx = crate::fast::Ctx { cur: bthread, stop_depth: bdepth, ..self.fast_ctx() };
                    crate::fast::run::<WINDOW>(&mut self.heap, &mut self.cur, &mut self.top, &ctx, &mut self.fuel);
                }
                if self.cur.frames.len() > bdepth { self.execute(bthread, bdepth) } else { Ok(()) }
            }
            Ok(Called::Switched) => self.execute(bthread, bdepth),
            Err(e) => Err(c_level(e)),
        };
        let ntop = self.ntop;
        self.native_depth = saved.0;
        self.ntop = saved.1;
        self.cur.nny = self.cur.nny.saturating_sub(1);
        self.c_depth -= 1;
        self.end_nested(r, PendingCall { pos, bthread, bdepth, keep: saved.0 > 0, ntop })
    }

    /// How a call from Rust ended (`call_nested_at`, `resume_nested`); one that paused at a
    /// collector step keeps its frames (`pending_call`).
    fn end_nested(&mut self, r: Result<(), LuaError>, call: PendingCall) -> Result<(usize, bool), LuaError> {
        let PendingCall { pos, bdepth, keep, .. } = call;
        match r {
            Ok(()) => Ok((pos, keep)),
            Err(e) => {
                if e.is_pending() {
                    self.pending_call = Some(call);
                    return Err(e);
                }
                if e.limit().is_some() {
                    self.close_upvals(pos);
                    self.cur.tbc.retain(|&s| s < pos);
                    self.cur.frames.truncate(bdepth);
                    return Err(e);
                }
                let ev = self.normalize(e)?;
                // Close while the erroring frames are still there (their calls go above).
                let ev = self.close_with_error(pos, ev);
                self.cur.frames.truncate(bdepth);
                Err(LuaError::Value(ev?))
            }
        }
    }

    // ---- errors ----

    /// The source position of the running Lua function: `chunk:line:` or empty.
    pub(crate) fn where_(&self, level: usize) -> String {
        // How many more activations to pass; set by the topmost one (outside any Rust
        // function, level 1 is the running Lua function itself). S6-1: only the activations
        // above the level asked for are walked (S6-2: a run of Rust ones in one step).
        let mut back: Option<usize> = None;
        let mut found = String::new();
        self.walk_activations(&self.cur, self.cur_ref, self.err_closing, None, |run| match run {
            Run::Lua(i) => {
                let b = *back.get_or_insert(level.saturating_sub(1));
                if b == 0 {
                    if let Some(f) = self.cur.frames.get(i) {
                        found = self.frame_where(f);
                    }
                    return true;
                }
                back = Some(b - 1);
                false
            }
            Run::C { count, .. } => pass_rust(&mut back, level, count).is_some(),
            Run::Base => pass_rust(&mut back, level, 1).is_some(),
        });
        found
    }

    /// The activations of thread `ts` (ref `tref`), oldest first, as C Lua's levels are
    /// (`debug.traceback` and `debug.getinfo` walk them): its Lua frames, and the Rust
    /// functions it is in where they were called. `err_closing` is `Lua::err_closing` for
    /// the running thread (None for another). `extra` is a native that is gone from
    /// `native_stack` but is still a level (the one that raised the error a message handler
    /// handles): its depth, slot, and the protected calls it sat on.
    pub(crate) fn activations(
        &self,
        ts: &ThreadState,
        tref: ThreadRef,
        err_closing: Option<(usize, usize)>,
        extra: Option<(usize, usize, usize)>,
    ) -> Vec<Act> {
        let mut runs = Vec::new();
        self.walk_activations(ts, tref, err_closing, extra, |run| {
            runs.push(run);
            false
        });
        let mut recs = Vec::with_capacity(runs.len());
        for run in runs.into_iter().rev() {
            match run {
                Run::Lua(i) => recs.push(Act::Lua(i)),
                Run::Base => recs.push(Act::Base),
                Run::C { slot: None, count } => recs.extend(crate::nostd::iter::repeat_n(Act::C(None), count)),
                Run::C { slot: Some(s), count } => recs.extend((0..count).map(|j| Act::C(Some(s + j)))),
            }
        }
        recs
    }

    /// How many activations [`Lua::activations`] lists (walked by runs, without listing them).
    pub(crate) fn activation_count(
        &self,
        ts: &ThreadState,
        tref: ThreadRef,
        err_closing: Option<(usize, usize)>,
        extra: Option<(usize, usize, usize)>,
    ) -> usize {
        let mut count = 0;
        self.walk_activations(ts, tref, err_closing, extra, |run| {
            count += match run {
                Run::C { count, .. } => count,
                Run::Lua(_) | Run::Base => 1,
            };
            false
        });
        count
    }

    /// Walks the activations of thread `ts` (as [`Lua::activations`] lists them) from the
    /// newest, in runs, until `visit` says to stop. Each frame, and each run of Rust
    /// activations, is one step: an error's position (S6-1) costs only the activations
    /// above the level it asks for.
    fn walk_activations(
        &self,
        ts: &ThreadState,
        tref: ThreadRef,
        err_closing: Option<(usize, usize)>,
        extra: Option<(usize, usize, usize)>,
        mut visit: impl FnMut(Run) -> bool,
    ) {
        let n = ts.frames.len();
        // The natives in progress on this thread, oldest first: their depth, and their run
        // of activations (the protected calls each sat on, then itself) by slot.
        let mut natives: Vec<(usize, usize, usize)> = self
            .native_stack
            .iter()
            .filter(|(t, ..)| *t == tref)
            .map(|&(_, d, slot, under)| (d, slot.wrapping_sub(under), under + 1))
            .collect();
        if let Some((d, slot, under)) = extra
            && d <= n
        {
            // Before the natives called later at its depth (the handler, if a native), after
            // the protected calls it sat on.
            let at = natives.iter().position(|&(nd, ..)| nd >= d).unwrap_or(natives.len());
            natives.insert(at, (d, slot.wrapping_sub(under), under + 1));
        }
        // S5C-COR-1: a `__close` run while an error unwinds to the protected call of frame
        // `lo` runs above the frames being unwound (`Cont::ErrClosing` at `hi`, or a native
        // or callable `__close` called with `hi` frames: `err_closing`); C Lua runs it once
        // they are gone, on `pcall` itself. Those frames, and what they called, are not
        // levels.
        let ec_hi = err_closing.map(|(_, hi)| hi);
        let mut dead = Dead { ec: err_closing, min_lo: usize::MAX, los: crate::nostd::collections::BinaryHeap::new() };
        // The protected calls frame `f` sits on, by slot, outermost first; and `pairs`, newer,
        // under a Lua `__pairs` (S5COR-6c: `pcall(pairs, t)` was a level short). Visited
        // newest first: true to stop.
        let pcalls = |f: &Frame, with_pairs: bool, visit: &mut dyn FnMut(Run) -> bool| match f.kind {
            FrameKind::Protected { dest, .. } => {
                (with_pairs && under_pairs(f.kind) && visit(Run::C { slot: None, count: 1 }))
                    || visit(Run::C { slot: Some(dest), count: pcall_levels(f.kind).max(1) })
            }
            FrameKind::Cont(_) if under_pairs(f.kind) => visit(Run::C { slot: None, count: 1 }),
            _ => visit(Run::Base),
        };
        let mut k = natives.len();
        let mut i = n;
        loop {
            // The natives called at depth `i` sit just below frame `i`.
            let mut lo_nat = k;
            while lo_nat > 0 && natives.get(lo_nat - 1).is_some_and(|&(d, ..)| d >= i) {
                lo_nat -= 1;
            }
            let any = natives.get(lo_nat..k).is_some_and(|s| s.iter().any(|&(d, ..)| d == i));
            if let Some(f) = ts.frames.get(i) {
                dead.activate(i);
                if !dead.gone(i) {
                    if dead.is_lo(i) {
                        // The protected call's callee is gone; `pcall` (a Rust activation)
                        // stays.
                        if pcalls(f, false, &mut visit) {
                            return;
                        }
                    } else {
                        if visit(Run::Lua(i)) {
                            return;
                        }
                        // A frame Rust started (`pcall`, a resume) sits on that Rust function;
                        // a metamethod or `__close` the machine called (a continuation, or
                        // `err_closing`'s callable `__close`, S6-6) does not.
                        if !any
                            && ec_hi != Some(i)
                            && !f.from_lua
                            && (!matches!(f.kind, FrameKind::Cont(_)) || under_pairs(f.kind))
                            && pcalls(f, true, &mut visit)
                        {
                            return;
                        }
                    }
                }
                if let FrameKind::Cont(Cont::ErrClosing { frame }) = f.kind
                    && frame < i
                {
                    dead.add(frame);
                }
            }
            // The Rust functions called at this depth, newest first: gone with the frame
            // that called them (the native running on top, and a native `__close`, never
            // are).
            if any {
                let keep = i == n || i == 0 || ec_hi == Some(i) || {
                    dead.activate(i - 1);
                    !dead.gone(i - 1) && !dead.is_lo(i - 1)
                };
                for &(d, slot, count) in natives.get(lo_nat..k).unwrap_or_default().iter().rev() {
                    if keep && d == i && visit(Run::C { slot: Some(slot), count }) {
                        return;
                    }
                }
            }
            k = lo_nat;
            if i == 0 {
                return;
            }
            i -= 1;
        }
    }

    /// The prototype `p`.
    #[inline]
    pub(crate) fn proto(&self, p: u32) -> Option<&Proto> {
        self.heap.protos.get(p)
    }

    /// `chunk:line: ` of a frame where it stands.
    fn frame_where(&self, f: &Frame) -> String {
        match self.proto(f.proto) {
            Some(p) => format!("{}:{}: ", p.source, p.line(f.pc.saturating_sub(1))),
            None => String::new(),
        }
    }

    /// Charges making an error message: the text and its position (a walk over the call
    /// stack), and the bytecode read to name variables (`debug::take_work`).
    pub(crate) fn charge_error(&mut self) -> Result<(), LuaError> {
        let depth = self.cur.frames.len() + self.native_stack.len();
        self.charge(ERROR_FUEL + ((depth + crate::debug::take_work()) / 16) as u64)
    }

    /// Turns an error into the value scripts see.
    pub(crate) fn normalize(&mut self, e: LuaError) -> Result<Value, LuaError> {
        let msg = match e.into_kind() {
            ErrorKind::Value(v) => return Ok(v),
            ErrorKind::Limit(k) => return Err(LuaError::Limit(k)),
            ErrorKind::Pending => return Err(ErrorKind::Pending.into()),
            ErrorKind::Msg(m) => format!("{}{m}", self.where_(1)),
            ErrorKind::Plain(m) => m,
            ErrorKind::Type { op, ty, .. } => format!("{}attempt to {op} a {ty} value", self.where_(1)),
            ErrorKind::Call { ty } => format!("{}attempt to call a {ty} value", self.where_(1)),
            ErrorKind::IntRepr { .. } => format!("{}number has no integer representation", self.where_(1)),
            ErrorKind::Arg(n, m) => return self.arg_error_value(n, &m),
        };
        self.charge_error()?;
        let cut = truncate_utf8(&msg, self.limits.error_len);
        let v = self.new_str(cut.as_bytes());
        // (A `memory_error`'s room is for this message only; with no room even for it, the
        // memory limit.)
        self.heap.mem.headroom = 0;
        v.map_err(|e| if e.limit().is_some() { e } else { LuaError::Limit(LimitKind::Memory) })
    }

    /// `bad argument #n to 'f' (msg)`, with the function named as its call site names it.
    pub(crate) fn arg_error_value(&mut self, n: usize, msg: &str) -> Result<Value, LuaError> {
        self.arg_error_bytes(n, msg.as_bytes())
    }

    /// [`Lua::arg_error_value`] for a message that quotes a script's bytes as they are.
    pub(crate) fn arg_error_bytes(&mut self, n: usize, msg: &[u8]) -> Result<Value, LuaError> {
        let (name, method) = self.called_name();
        let head = if method && n == 1 {
            format!("calling '{name}' on bad self (")
        } else {
            format!("bad argument #{} to '{name}' (", if method { n - 1 } else { n })
        };
        let mut full = self.where_(1).into_bytes();
        full.extend_from_slice(head.as_bytes());
        full.extend_from_slice(msg);
        full.push(b')');
        self.charge_error()?;
        self.new_str(cut_message(&full, self.limits.error_len))
    }

    /// An error with a message that quotes a script's bytes as they are, after the running
    /// function's position (as `LuaError::Msg` gets it). S5COR-3: messages are Rust strings
    /// elsewhere, so a byte that is not UTF-8 (an `é` cut in half in `string.pack`'s format)
    /// came out converted, where C Lua shows the byte itself.
    pub(crate) fn msg_bytes(&mut self, parts: &[&[u8]]) -> LuaError {
        let mut full = self.where_(1).into_bytes();
        for p in parts {
            full.extend_from_slice(p);
        }
        if let Err(e) = self.charge_error() {
            return e;
        }
        match self.new_str(cut_message(&full, self.limits.error_len)) {
            Ok(v) => LuaError::Value(v),
            Err(e) => e,
        }
    }

    /// The running native's name as the calling code names it, and whether it was called
    /// as a method.
    fn called_name(&self) -> (String, bool) {
        if self.native_from_lua
            && let Some(f) = self.cur.frames.last()
        {
            let pc = f.pc.saturating_sub(1);
            let Some(p) = self.proto(f.proto) else { return (self.native_name.to_owned(), false) };
            match p.code.get(pc) {
                Some(Instr::Call { a, .. } | Instr::TailCall { a, .. }) => {
                    if let Some((kind, name)) = crate::debug::obj_name(self, p, pc, *a) {
                        return (name, kind == "method");
                    }
                }
                Some(Instr::TForCall { .. }) => return ("for iterator".into(), false),
                _ => {}
            }
        }
        // Called as a metamethod: C Lua names the event (`funcnamefromcode`).
        if let Some(event) = self.native_event {
            return (event.into(), false);
        }
        // A library's internal helper (an iterator `ipairs` or `gmatch` returns) has no
        // global name: C Lua's `pushglobalfuncname` finds none and says `?`.
        if self.native_name.ends_with("_aux") {
            return ("?".into(), false);
        }
        (self.native_name.to_owned(), false)
    }

    /// Adds what an operand was to a type error raised by an instruction.
    fn annotate(&self, e: LuaError, pidx: u32, pc: usize, operands: &[crate::debug::Operand]) -> LuaError {
        let empty = Proto::default();
        let proto = self.proto(pidx).unwrap_or(&empty);
        match e.into_kind() {
            ErrorKind::Type { op, ty, operand } => {
                let info = operands
                    .get(usize::from(operand))
                    .map(|o| crate::debug::var_info(self, proto, pc, *o))
                    .unwrap_or_default();
                LuaError::Msg(format!("attempt to {op} a {ty} value{info}"))
            }
            ErrorKind::Call { ty } => {
                let info = operands.first().map(|o| crate::debug::var_info(self, proto, pc, *o)).unwrap_or_default();
                LuaError::Msg(format!("attempt to call a {ty} value{info}"))
            }
            ErrorKind::IntRepr { operand } => {
                let info = operands
                    .get(usize::from(operand))
                    .map(|o| crate::debug::var_info(self, proto, pc, *o))
                    .unwrap_or_default();
                LuaError::Msg(format!("number{info} has no integer representation"))
            }
            other => other.into(),
        }
    }

    /// Notes the running thread's frames for a traceback ([`Lua::take_traceback`]).
    fn note_traceback(&mut self) {
        let mut trace = self.last_trace.take().unwrap_or_default();
        trace.clear();
        self.trace_sources.clear();
        for f in self.cur.frames.iter().rev().take(22) {
            let Some(p) = self.heap.protos.get(f.proto) else { continue };
            if !self.trace_sources.last().is_some_and(|s| Arc::ptr_eq(s, &p.source)) {
                self.trace_sources.push(Arc::clone(&p.source));
            }
            trace.push((self.trace_sources.len() - 1, p.line(f.pc.saturating_sub(1)), p.line_defined));
        }
        self.last_trace = Some(trace);
    }

    /// The traceback of the last error that left the machine (then forgotten): empty if
    /// none did.
    pub(crate) fn take_traceback(&mut self) -> String {
        let Some(trace) = self.last_trace.take() else { return String::new() };
        let mut s = String::from("stack traceback:");
        for &(src, line, defined) in &trace {
            let Some(source) = self.trace_sources.get(src) else { continue };
            let what = if defined == 0 { "in main chunk".to_owned() } else { format!("in function <{source}:{defined}>") };
            s.push_str(&format!("\n\t{source}:{line}: {what}"));
        }
        self.trace_sources.clear();
        s
    }

    /// Handles an error in the loop with boundary (`bthread`, `bdepth`): unwinds to a
    /// `pcall` (Ok: go on running), kills coroutines it escapes from, or returns it.
    fn on_error(&mut self, e: LuaError, bthread: ThreadRef, bdepth: usize) -> Result<(), LuaError> {
        if e.is_pending() {
            return Err(e);
        }
        if e.limit().is_some() {
            self.abort_to(bthread);
            return Err(e);
        }
        let mut err = match self.normalize(e) {
            Ok(v) => v,
            Err(e) if e.limit().is_some() => {
                self.abort_to(bthread);
                return Err(e);
            }
            Err(other) => return Err(other),
        };
        let mut first = true;
        loop {
            let min = if self.cur_ref == bthread { bdepth } else { 0 };
            let found = (min..self.cur.frames.len())
                .rev()
                .find(|&i| matches!(self.cur.frames.get(i).map(|f| f.kind), Some(FrameKind::Protected { .. })));
            // S6-7: under a message handler, where this error was raised, unless something
            // catches it (the first, deepest raise of an error that escapes).
            if crate::nostd::mem::take(&mut first) && self.handler_raise.thread == Some(self.cur_ref) {
                if found.is_some() {
                    self.handler_raise.raised = None;
                    self.handler_raise.pos = None;
                } else if self.handler_raise.raised.is_none() {
                    // (The walk charged as `call_handler`'s is: once a round, uncharged, a
                    // handler failing under a deep resume chain ran at 157x plain code per
                    // unit of fuel, S6R-6.)
                    let (level, walked) = self.c_levels();
                    if let Err(e) = self.charge((walked / RESUMER_FRAMES_PER_FUEL) as u64) {
                        self.abort_to(bthread);
                        return Err(e);
                    }
                    self.handler_raise.raised = Some(level);
                    self.handler_raise.pos = self.handler_raise.cross.and_then(|(from, at)| self.cross_where(from, at));
                }
            }
            if let Some(i) = found {
                let Some(fr) = self.cur.frames.get(i) else { break };
                let dest = match fr.kind {
                    FrameKind::Protected { dest, .. } => dest,
                    _ => fr.func,
                };
                let handler = fr.handler;
                if let Some(h) = handler {
                    err = self.call_handler(h, err).inspect_err(|_| self.abort_to(bthread))?;
                }
                // The handler stays: an error in a `__close` run below goes through it too,
                // as in C Lua (`luaD_closeprotected` closes with the handler in place),
                // S5C-COR-3. It said the original error only.
                // `__close` handlers run while the erroring frames are still there, so their
                // calls go above every pending to-be-closed slot; a Lua one runs as a frame
                // of its own (it may yield, as in C Lua), and the error waits in the slot
                // where it is returned (`dest + 1`, below the frames being closed).
                self.set_st(dest + 1, err);
                // The protected call's outcome may go to a metamethod's continuation (`pcall`
                // as `__concat`), which can raise an error of its own in the frame below: that
                // one unwinds again, from there (S5COR-8: it went past every `pcall`).
                match self.close_error_async(i) {
                    Ok(()) => return Ok(()),
                    Err(e) => {
                        err = self.rethrown(e, bthread)?;
                        continue;
                    }
                }
            }
            if self.cur_ref != bthread && self.cur.resumer.is_some() {
                // A coroutine that dies does not unwind: its to-be-closed variables wait for
                // `coroutine.close`, except under `wrap`, which closes it at once.
                let by_wrap = self
                    .cur
                    .resumer
                    .and_then(|r| self.heap.threads.get(r.idx))
                    .and_then(|t| t.resume_pending)
                    .is_some_and(|p| p.wrap);
                if by_wrap {
                    err = match self.close_with_error(0, err) {
                        Ok(v) => v,
                        Err(e) => {
                            self.abort_to(bthread);
                            return Err(e);
                        }
                    };
                }
                if by_wrap {
                    self.cur.frames.clear();
                } else {
                    // LIBS-4: as in C Lua, a coroutine that died by an error keeps its frames
                    // (and the function that raised it) for `debug.traceback` and
                    // `debug.getinfo`, until `coroutine.close` or the collector frees it.
                    self.cur.died_in = match self.err_native {
                        Some(((t, d, slot), _, v)) if t == self.cur_ref && self.raw_equal(&v, &err) => {
                            Some((d, slot, self.st(slot)))
                        }
                        _ => None,
                    };
                }
                self.cur.status = CoStatus::Dead;
                self.cur.error = Some(err);
                let wrap = self.switch_back()?;
                if wrap.wrap {
                    // `wrap` propagates the error into its caller.
                    if let Value::Str(s) = err
                        && wrap.from_lua
                    {
                        let w = self.cur.frames.last().map_or_else(String::new, |f| self.frame_where(f));
                        if !w.is_empty() {
                            // A copy of the message at each `wrap` it crosses: paid
                            // (S6GC-1: a 4 MiB message ran at 300x plain code per unit).
                            self.charge_bytes(self.bytes(s).len())?;
                            let mut b = w.into_bytes();
                            b.extend_from_slice(self.bytes(s));
                            err = self.new_str(&b)?;
                        }
                    }
                    if let FrameKind::Protected { dest, wanted, cont } = wrap.kind {
                        // The wrap function was called by `pcall`: caught here.
                        self.set_st(dest, Value::Bool(false));
                        self.set_st(dest + 1, err);
                        let r = self.move_results(dest, dest, 2, wanted).and_then(|()| match cont {
                            Some(c) => self.apply_cont(c, Value::Bool(false)),
                            None => Ok(()),
                        });
                        match r {
                            Ok(()) => return Ok(()),
                            Err(e) => {
                                err = self.rethrown(e, bthread)?;
                                continue;
                            }
                        }
                    }
                    continue;
                }
                self.set_st(wrap.func, Value::Bool(false));
                self.set_st(wrap.func + 1, err);
                match self.finish_call(wrap.kind, wrap.func, wrap.func, 2, wrap.nresults, true) {
                    Ok(()) => return Ok(()),
                    Err(e) => {
                        err = self.rethrown(e, bthread)?;
                        continue;
                    }
                }
            }
            break;
        }
        self.note_traceback();
        Err(LuaError::Value(err))
    }

    /// An error raised while `on_error` completed a protected call (by the continuation its
    /// outcome went to): its value, to unwind again; a limit stops everything up to
    /// `bthread`.
    #[cold]
    fn rethrown(&mut self, e: LuaError, bthread: ThreadRef) -> Result<Value, LuaError> {
        let r = if e.limit().is_some() { Err(e) } else { self.normalize(e) };
        if r.as_ref().is_err_and(|e| e.limit().is_some()) {
            self.abort_to(bthread);
            return r;
        }
        r
    }

    /// After a limit: kills the coroutines between the running one and `bthread`.
    fn abort_to(&mut self, bthread: ThreadRef) {
        while self.cur_ref != bthread {
            self.close_upvals(0);
            self.cur.tbc.clear();
            self.cur.frames.clear();
            self.cur.status = CoStatus::Dead;
            let Some(r) = self.cur.resumer.take() else { break };
            self.resume_depth = self.resume_depth.saturating_sub(1);
            self.swap_to(r);
            self.cur.resume_pending = None;
            self.cur.status = CoStatus::Running;
        }
    }

    // ---- coroutines ----

    /// Makes `to` the running thread, storing the current one in its slot.
    /// The fast loop's context (see `ctx_epoch`), for the running thread with no stop
    /// depth (`run` sets both per frame).
    fn fast_ctx(&self) -> crate::fast::Ctx {
        crate::fast::Ctx {
            cur: self.cur_ref,
            call_depth: self.limits.call_depth,
            stop_depth: 0,
            index_name: self.tm_names.get(Tm::Index as usize).map_or(Value::Nil, |s| Value::Str(*s)),
            eq_name: self.tm_names.get(Tm::Eq as usize).map_or(Value::Nil, |s| Value::Str(*s)),
            string_meta: self.string_meta,
            string_len: self.limits.string_len,
            next_fn: self.names.next_fn,
            ipairs_aux: self.names.ipairs_aux,
            utf8_iters: self.names.utf8_iters,
            math_fast: self.names.math_fast,
            math_mask: self.names.math_mask,
            table_fast: self.names.table_fast,
            table_mask: self.names.table_mask,
        }
    }

    fn swap_to(&mut self, to: ThreadRef) {
        // The running thread's slot holds an empty placeholder: `to`'s state comes in,
        // the running one goes to its own slot, the placeholder to `to`'s. Boxed, these
        // are pointer swaps (moving the states themselves was most of a switch's cost).
        let (from, into) = (self.cur_ref.idx as usize, to.idx as usize);
        let threads = &mut self.heap.threads;
        if from != into && from < threads.slots() && let Some(slot) = threads.at_mut(into) {
            crate::nostd::mem::swap(&mut self.cur, slot);
            threads.swap_slots(from, into);
        } else {
            let incoming = self.heap.threads.get_mut(to.idx).map(crate::nostd::mem::take).unwrap_or_default();
            let outgoing = crate::nostd::mem::replace(&mut self.cur, incoming);
            if let Some(slot) = self.heap.threads.get_mut(self.cur_ref.idx) {
                *slot = outgoing;
            }
        }
        self.cur_ref = to;
    }

    /// Near C Lua's limit (the bound reached it), the exact count: what C Lua does with
    /// this resume of `co`, and the outcome when it does not run it (`do_resume`). At 200
    /// levels it refuses and the coroutine stays as it was. At 199 the resume goes ahead,
    /// taking the 200th, and a coroutine not yet started then fails in the call of its
    /// body (`ccall`'s check) before any of it runs: it dies by an unpositioned "C stack
    /// overflow", which the resume returns (or `wrap` raises) as from a coroutine that
    /// failed. The same outcome here, without switching to it.
    #[cold]
    #[inline(never)]
    #[allow(clippy::too_many_arguments)]
    fn resume_refused(
        &mut self,
        func: usize,
        nres: i32,
        co: ThreadRef,
        wrap: bool,
        from_lua: bool,
        kind: FrameKind,
        pcall_of_resume: usize,
    ) -> Result<Option<Called>, LuaError> {
        let (level, walked) = self.c_levels();
        // (The same walk as `call_handler`'s, charged the same.)
        self.charge((walked / RESUMER_FRAMES_PER_FUEL) as u64)?;
        // The resumer's count in C Lua (`from`'s `nCcalls`): `c_levels` is the count a
        // message handler's call reaches, one past the native running it and one past the
        // `lua_resume` that `coroutine.resume` or `wrap` is to C Lua.
        let from = (level + pcall_of_resume).saturating_sub(2);
        let fresh = self.heap.threads.get(co.idx).is_some_and(|t| t.fresh);
        let dies = from + 1 == C_HANDLER_LEVELS && fresh;
        if from < C_HANDLER_LEVELS && !dies && self.resume_depth < self.limits.native_depth {
            return Ok(None);
        }
        let msg = "C stack overflow";
        if dies && self.resume_depth < self.limits.native_depth {
            let e = self.new_str(msg.as_bytes())?;
            if let Some(t) = self.heap.threads.get_mut(co.idx) {
                t.fresh = false;
                t.status = CoStatus::Dead;
                // (`wrap` closes a coroutine that failed, as `auxwrap` does: no error kept.)
                t.error = if wrap { None } else { Some(e) };
            }
        }
        if wrap {
            return Err(if from_lua { LuaError::msg(msg) } else { LuaError::Plain(msg.into()) });
        }
        let m = self.new_str(msg.as_bytes())?;
        self.set_st(func, Value::Bool(false));
        self.set_st(func + 1, m);
        self.finish_call(kind, func, func, 2, nres, false)?;
        Ok(Some(Called::Done))
    }

    fn do_resume(
        &mut self,
        func: usize,
        nres: i32,
        co: ThreadRef,
        wrap: bool,
        from_lua: bool,
        kind: FrameKind,
    ) -> Result<Called, LuaError> {
        let args = crate::nostd::mem::take(&mut self.rets);
        let status = if co == self.cur_ref {
            CoStatus::Running
        } else {
            self.heap.threads.get(co.idx).map_or(CoStatus::Dead, |t| t.status)
        };
        if status != CoStatus::Suspended || !self.heap.threads.live(co.idx, co.generation) {
            let msg = match status {
                CoStatus::Dead => "cannot resume dead coroutine",
                _ => "cannot resume non-suspended coroutine",
            };
            if wrap {
                return Err(if from_lua { LuaError::msg(msg) } else { LuaError::Plain(msg.into()) });
            }
            let m = self.new_str(msg.as_bytes())?;
            self.set_st(func, Value::Bool(false));
            self.set_st(func + 1, m);
            self.finish_call(kind, func, func, 2, nres, false)?;
            return Ok(Called::Done);
        }
        // LR-3: C Lua's `lua_resume` starts the thread's C levels from the resumer's and
        // refuses at its limit, before the coroutine runs: "C stack overflow" with no
        // position, which `coroutine.resume` returns and `wrap` raises (positioned at its
        // caller when that is a Lua function, as `auxwrap`'s `luaL_where(L, 1)`). A `pcall`
        // of the resuming function is a C level of its own there. Ours stopped at 200 nested
        // resumes whatever else was in use, and put the position on.
        // The exact count walks the frames of this thread and its resumers (`c_levels`), so
        // a resume first takes an upper bound in constant time and walks only when that
        // reaches the limit (charged as `call_handler` charges the walk).
        let pcall_of_resume = if from_lua { 0 } else { c_calls(kind) };
        let here = pcall_of_resume + self.c_bound_here(func);
        if self.c_depth + self.protected_natives + (HANDLER_BASE_LEVELS - 1) + here >= C_HANDLER_LEVELS
            || self.resume_depth + 1 > self.limits.native_depth
        {
            if let Some(done) = self.resume_refused(func, nres, co, wrap, from_lua, kind, pcall_of_resume)? {
                return Ok(done);
            }
        }
        // Its first resume maps its register window (LR-8); charged once the resume is
        // known to run, so a refused resume leaves the coroutine as it was.
        if self.heap.threads.get(co.idx).is_some_and(|t| t.fresh) {
            self.charge(COROUTINE_COST)?;
        }
        // The resumed thread's bound: the resume's level, this thread's and its resumers'.
        let inherited = RESUME_LEVELS + here;
        self.resume_depth += 1;
        self.cur.resume_pending = Some(Pending { func, nresults: nres, wrap, from_lua, kind });
        self.cur.status = CoStatus::Normal;
        let (prev, depth) = (self.cur_ref, self.cur.frames.len());
        self.swap_to(co);
        self.cur.status = CoStatus::Running;
        self.cur.resumer = Some(prev);
        self.cur.c_bound = inherited;
        if self.cur.fresh {
            self.cur.fresh = false;
            self.ensure_stack(args.len() + 1 + EXTRA_STACK)?;
            for (i, a) in args.iter().enumerate() {
                self.set_st(1 + i, *a);
            }
            match self.precall(0, args.len(), -1, FrameKind::Normal, false) {
                Ok(Called::Done) => self.finish_coroutine()?,
                Ok(Called::Lua | Called::Switched) => {}
                // A native body's error kills the coroutine here, as the loop would, not in
                // the resumer's `pcall` with the coroutine still running (SEC7-V1).
                Err(e) => self.on_error(e, prev, depth)?,
            }
        } else if let Some(p) = self.cur.yield_pending.take() {
            self.cur.unrolled = self.cur.frames.len();
            self.ensure_stack(p.func + args.len() + 1)?;
            for (i, a) in args.iter().enumerate() {
                self.set_st(p.func + i, *a);
            }
            self.finish_call(p.kind, p.func, p.func, args.len(), p.nresults, true)?;
        }
        self.give_back_rets(args);
        Ok(Called::Switched)
    }

    /// A buffer taken from `rets` goes back (emptied) when `rets` has none, so a switch
    /// does not allocate a new one each time.
    fn give_back_rets(&mut self, mut v: Vec<Value>) {
        // Not a long one: kept, it was outside the account for good (SEC-M15).
        if self.rets.capacity() == 0 && v.capacity() <= crate::stdlib::NATIVE_CHUNK {
            v.clear();
            self.rets = v;
        }
    }

    /// The running coroutine's body returned: its results go to the resumer.
    fn finish_coroutine(&mut self) -> Result<(), LuaError> {
        let vals: Vec<Value> = self.cur.stack.get(0..self.top).map(<[Value]>::to_vec).unwrap_or_default();
        self.close_upvals(0);
        self.cur.status = CoStatus::Dead;
        let me = self.cur_ref;
        let r = self.deliver(vals);
        // Its stack is not needed any more: the next coroutine's (LR-8).
        self.retire_stack(me);
        r
    }

    /// Switches to the resumer with `vals` as the resume's results.
    fn deliver(&mut self, vals: Vec<Value>) -> Result<(), LuaError> {
        let p = self.switch_back()?;
        let mut off = 0;
        if !p.wrap {
            self.ensure_stack(p.func + 1)?;
            self.set_st(p.func, Value::Bool(true));
            off = 1;
        }
        // The values are counted while the stack grows beside them (SEC-M15).
        let mut hold = crate::stdlib::MemHold::default();
        let bytes = vals.capacity().saturating_mul(crate::nostd::mem::size_of::<Value>());
        hold.cover(self, bytes)?;
        let r = self.ensure_stack(p.func + off + vals.len() + 1);
        hold.give_back(self);
        r?;
        for (i, v) in vals.iter().enumerate() {
            self.set_st(p.func + off + i, *v);
        }
        let n = vals.len();
        self.give_back_rets(vals);
        self.finish_call(p.kind, p.func, p.func, n + off, p.nresults, true)
    }

    /// Runs on `co`'s stack for a moment (`coroutine.close`), without letting it yield.
    /// The closing thread is `normal` meanwhile, as a resumer is (S6: it said `running`, and
    /// closing it from a `__close` there said "cannot close a running coroutine").
    /// Returns its status, for [`Lua::swap_out_after_close`].
    ///
    /// Its handlers are called above what is live on its own stack (`parked_live`), not
    /// above the closer's: `ntop` is the closer's while `coroutine.close` runs, and a
    /// handler called there (LR-8) grew every closed coroutine's stack to the closer's
    /// height, a copy of 240 KiB per `coroutine.close` from a coroutine 15,000 slots deep
    /// (15x plain code per unit of fuel).
    pub(crate) fn swap_in_for_close(&mut self, co: ThreadRef) -> (CoStatus, usize) {
        let was = crate::nostd::mem::replace(&mut self.cur.status, CoStatus::Normal);
        self.swap_to(co);
        self.cur.nny += 1;
        let live = crate::heap::parked_live(&self.cur, &self.heap.protos).unwrap_or(self.cur.stack.len());
        (was, crate::nostd::mem::replace(&mut self.ntop, live))
    }

    /// Back from [`Lua::swap_in_for_close`], to `me`, with the status and `ntop` it had.
    pub(crate) fn swap_out_after_close(&mut self, me: ThreadRef, (was, ntop): (CoStatus, usize)) {
        self.cur.nny = self.cur.nny.saturating_sub(1);
        self.ntop = ntop;
        self.swap_to(me);
        self.cur.status = was;
    }

    /// Back to the resumer; returns where its resume call wants results.
    fn switch_back(&mut self) -> Result<Pending, LuaError> {
        self.resume_depth = self.resume_depth.saturating_sub(1);
        let Some(r) = self.cur.resumer.take() else {
            return Err(LuaError::msg("internal: coroutine without a resumer"));
        };
        self.swap_to(r);
        self.cur.status = CoStatus::Running;
        self.cur.resume_pending.take().ok_or_else(|| LuaError::msg("internal: resumer not waiting"))
    }

    fn do_yield(&mut self, func: usize, nres: i32, kind: FrameKind) -> Result<(), LuaError> {
        // Raised from the yield itself, which has no position.
        if self.cur_ref == self.main_ref {
            return Err(LuaError::Value(self.new_str(b"attempt to yield from outside a coroutine")?));
        }
        // A coroutine with no resumer runs its `__close` handlers for `coroutine.close`:
        // C Lua's message for any coroutine that cannot yield (S5GC-6).
        if self.cur.nny > 0 || self.cur.resumer.is_none() {
            return Err(LuaError::Value(self.new_str(b"attempt to yield across a C-call boundary")?));
        }
        let vals = crate::nostd::mem::take(&mut self.rets);
        self.cur.yield_pending = Some(Pending { func, nresults: nres, wrap: false, from_lua: false, kind });
        self.cur.status = CoStatus::Suspended;
        self.deliver(vals)
    }

    /// A new coroutine running `f`, on a spare stack when there is one (`retire_stack`).
    ///
    /// LR-8: a coroutine's first resume grows its stack to a register window (about 4.5
    /// KiB), a `__close` handler run by `coroutine.close` to twice that, and a closed or
    /// finished coroutine kept it until the collector freed the thread. With the collector
    /// stopped (or a long pause) every new coroutine touched about 10 KiB of fresh pages
    /// nothing reused: making, resuming and closing one ran at 10x to 20x plain code per
    /// unit of fuel on a process's first runs (a coroutine closed from one 15,000 slots
    /// deep, more: `swap_in_for_close`). A closed or finished coroutine's stack is given to
    /// the next one now: about 860 bytes a coroutine stay fresh (C Lua: 960), and with the
    /// collector stopped making and closing one costs what it does with it running. One
    /// that is never closed nor finished (a `wrap` left suspended) still maps a window,
    /// 9x plain code per unit of fuel with the collector stopped, whose cycles are what
    /// paid for it otherwise: the window is charged at its first resume (`COROUTINE_COST`).
    pub(crate) fn new_coroutine(&mut self, f: Value) -> Result<ThreadRef, LuaError> {
        // A thread made and later swept (`wrap` paid nothing for it).
        self.charge(8)?;
        let mut stack = match self.spare_stacks.pop() {
            Some(s) => {
                self.heap.mem.slack_out(s.capacity() * SLOT_BYTES);
                s
            }
            // Its first resume makes the room it needs.
            None => Vec::with_capacity(1),
        };
        stack.clear();
        stack.push(f);
        let st = ThreadState { stack, fresh: true, ..ThreadState::default() };
        // `f` is the caller's argument, so a collection here keeps it.
        self.charge_mem(st.mem_size())?;
        let t = self.heap.place_thread(st);
        self.hold(Value::Thread(t));
        Ok(t)
    }

    /// Takes the stack of `co`, a coroutine that was closed or finished, for a new
    /// coroutine to reuse (`new_coroutine`); its frame records go back to the system. Only
    /// a dead thread holding nothing on its stack (no frames kept for a traceback, no
    /// open upvalues, no pending to-be-closed variables) gives it up.
    pub(crate) fn retire_stack(&mut self, co: ThreadRef) {
        if co == self.cur_ref || co == self.main_ref {
            return;
        }
        if !self.heap.threads.live(co.idx, co.generation) {
            return;
        }
        let Some(t) = self.heap.threads.get_mut(co.idx) else { return };
        if t.status != CoStatus::Dead || t.fresh || !t.frames.is_empty() || !t.tbc.is_empty() || !t.open_upvals.is_empty() {
            return;
        }
        let mut stack = crate::nostd::mem::take(&mut t.stack);
        let frames = crate::nostd::mem::take(&mut t.frames);
        let mem = &mut self.heap.mem;
        mem.release(frames.capacity() * crate::heap::FRAME_BYTES);
        let cap = stack.capacity();
        mem.release(cap * SLOT_BYTES);
        // (A kept stack goes back into `used` as slack: resident, counted, not live.)
        let held: usize = self.spare_stacks.iter().map(|s| s.capacity() * SLOT_BYTES).sum();
        if (1..=SPARE_STACK_SLOTS).contains(&cap)
            && self.spare_stacks.len() < SPARE_STACKS
            && held + cap * SLOT_BYTES <= mem.cap / 16
        {
            stack.clear();
            mem.slack_in(cap * SLOT_BYTES);
            self.spare_stacks.push(stack);
        }
    }

    /// Gives the spare coroutine stacks back to the system.
    fn drop_spare_stacks(&mut self) {
        for s in self.spare_stacks.drain(..) {
            self.heap.mem.slack_out(s.capacity() * SLOT_BYTES);
        }
    }

    // ---- the loop ----

    /// Runs until the thread `bthread` is back to `bdepth` frames.
    pub(crate) fn execute(&mut self, bthread: ThreadRef, bdepth: usize) -> Result<(), LuaError> {
        loop {
            match self.run(bthread, bdepth) {
                Ok(()) => return Ok(()),
                Err(e) => self.on_error(e, bthread, bdepth)?,
            }
        }
    }

    /// The execution loop. Lua-to-Lua calls and returns switch frames here, without
    /// leaving; it returns when the thread `bthread` is back to `bdepth` frames. Errors
    /// leave the failing frame's `pc` (and the fuel) saved.
    ///
    /// Registers are a 256-slot window of the stack (`ensure_window` keeps it in bounds),
    /// indexed by their 8-bit numbers without checks; fuel lives in a local, written back
    /// before anything that looks at it.
    #[allow(clippy::too_many_lines, reason = "the interpreter's dispatch is one match")]
    #[allow(clippy::indexing_slicing, reason = "a u8 register into the 256-slot window cannot be out of bounds")]
    fn run(&mut self, bthread: ThreadRef, bdepth: usize) -> Result<(), LuaError> {
        use crate::debug::Operand as O;
        let mut ctx = self.fast_ctx();
        let mut epoch = self.ctx_epoch;
        // The width of the fast loop that just switched frames and stopped (in the frame
        // `'reload` then picks up). Stopped in a frame of its own width, it stopped at the
        // instruction it cannot run, so that one runs in full without entering it again
        // for nothing (natives right after a call or return paid two entries).
        let mut fast_tried: Option<bool> = None;
        'reload: loop {
            if self.cur_ref == bthread && self.cur.frames.len() <= bdepth {
                return Ok(());
            }
            let Some(frame) = self.cur.frames.last() else { return Ok(()) };
            let pidx = frame.proto;
            let closure = frame.closure;
            // Register 0; `base` is the same but for an instruction after `Window`.
            let frame_base = frame.base;
            let mut base = frame_base;
            let mut pc = frame.pc;
            let mut fuel = self.fuel;
            let wide = self.proto(pidx).is_some_and(Proto::wide);
            let width = if wide { WIDE_WINDOW } else { WINDOW };
            if self.cur.stack.len() < base + width {
                self.ensure_window(base, width)?;
            }

            macro_rules! save {
                () => {{
                    if let Some(f) = self.cur.frames.last_mut() {
                        f.pc = pc;
                    }
                    self.fuel = fuel;
                }};
            }
            // A call that may charge fuel or switch frames: state saved before, fuel back
            // after.
            macro_rules! call {
                ($e:expr) => {{
                    save!();
                    let r = $e;
                    fuel = self.fuel;
                    r
                }};
            }
            // A collector step at an instruction (`gc_due_step`). Paused at the end of the
            // host's poll, an instruction that steps before its work (`before`: nothing done
            // yet) runs again on resume, without the step (`due_skip`) and with its fetch's
            // unit given back; one that steps after its work goes on with the next.
            macro_rules! gc_step_at {
                ($before:expr) => {
                    if let Err(e) = call!(self.gc_due_step()) {
                        save!();
                        if $before && e.is_pending() {
                            if let Some(f) = self.cur.frames.last_mut() {
                                f.pc = pc - 1 - usize::from(base != frame_base);
                            }
                            self.fuel += 1;
                            self.due_skip = true;
                            self.rewound = true;
                        }
                        return Err(e);
                    }
                };
            }
            macro_rules! t {
                ($e:expr) => {
                    match $e {
                        Ok(v) => v,
                        Err(e) => {
                            save!();
                            return Err(e);
                        }
                    }
                };
            }
            // `t!(call!(..))` for a call that may pop the running frame (a return, a tail call):
            // its state was saved before, and on an error the frame on top may be its caller,
            // whose own position must stay (S5COR-11: saving again wrote the returning
            // function's position into the caller, so an error a metamethod's continuation
            // raised there named the wrong line).
            macro_rules! tpop {
                // (Both uses reload the frame, and the fuel with it, right after.)
                ($e:expr) => {{
                    save!();
                    if let Err(e) = $e {
                        return Err(e);
                    }
                }};
            }
            macro_rules! ta {
                ($e:expr, $ops:expr) => {
                    match $e {
                        Ok(v) => v,
                        Err(e) => {
                            save!();
                            let ops = $ops;
                            return Err(self.annotate(e, pidx, pc - 1, &ops));
                        }
                    }
                };
            }
            // After a call that pushed a frame (or switched threads): state was saved by
            // `call!` before it.
            macro_rules! reload {
                () => {
                    continue 'reload
                };
            }
            macro_rules! reg {
                ($i:expr) => {
                    self.cur.stack.get(base + ($i) as usize).copied().unwrap_or(Value::Nil)
                };
            }
            macro_rules! setr {
                ($i:expr, $v:expr) => {{
                    let v = $v;
                    if let Some(s) = self.cur.stack.get_mut(base + ($i) as usize) {
                        *s = v;
                    }
                }};
            }
            macro_rules! kv {
                ($k:expr) => {
                    self.heap.protos.at(pidx as usize).and_then(|p: &Proto| p.consts.get(($k) as usize).copied()).unwrap_or(Value::Nil)
                };
            }
            macro_rules! rk {
                ($x:expr) => {{
                    let x: Rk = $x;
                    if x.is_const() { kv!(x.index()) } else { reg!(x.index()) }
                }};
            }
            // Arithmetic with a fast path for numbers and the general path (strings,
            // metamethods, errors) otherwise.
            // Arithmetic: numbers by the shared rules (`fast::arith`), else the general
            // path (strings, metamethods, errors).
            macro_rules! arith {
                ($op:expr, $a:expr, $x:expr, $y:expr, $ops:expr) => {{
                    let (x, y): (Value, Value) = ($x, $y);
                    let fast = if $op == ArithOp::Sub && matches!(y, Value::Int(_)) && $ops.len() == 2 && matches!($ops[1], O::Rk(r) if r.is_const()) {
                        crate::fast::sub_k(x, y)
                    } else {
                        crate::fast::arith($op, x, y)
                    };
                    match fast {
                        Some(v) => setr!($a, v),
                        None => match ta!(call!(self.arith($op, x, y, Cont::Store(base + usize::from($a)))), $ops) {
                            Some(v) => setr!($a, v),
                            None => reload!(),
                        },
                    }
                }};
            }
            // `arith!` on two registers, or on a register and a constant.
            macro_rules! arith_rr {
                ($op:ident, $a:expr, $b:expr, $c:expr) => {
                    arith!(ArithOp::$op, $a, reg!(usize::from($b)), reg!(usize::from($c)), [O::Reg($b), O::Reg($c)])
                };
            }
            macro_rules! arith_rk {
                ($op:ident, $a:expr, $b:expr, $k:expr) => {
                    arith!(ArithOp::$op, $a, reg!(usize::from($b)), kv!($k), [O::Reg($b), O::Rk(Rk::konst($k))])
                };
            }

            // The fast loop's context: made again only when what it copies changed
            // (`ctx_epoch`); the running thread and the stop depth are this frame's.
            if self.ctx_epoch != epoch {
                ctx = self.fast_ctx();
                epoch = self.ctx_epoch;
            }
            ctx.cur = self.cur_ref;
            ctx.stop_depth = if self.cur_ref == bthread { bdepth } else { 0 };
            // Whether the next instruction is tried in the fast loop (see `fast_tried`).
            let mut fast_now = self.fast && fast_tried.take() != Some(wide);
            loop {
                // The common cases run in the fast loop; it stops at the first instruction
                // it cannot finish, which runs below in full.
                save!();
                let moved = fast_now
                    && if wide {
                        crate::fast::run::<WIDE_WINDOW>(&mut self.heap, &mut self.cur, &mut self.top, &ctx, &mut self.fuel)
                    } else {
                        crate::fast::run::<WINDOW>(&mut self.heap, &mut self.cur, &mut self.top, &ctx, &mut self.fuel)
                    };
                if moved {
                    fast_tried = Some(wide);
                    continue 'reload;
                }
                self.full_steps += u64::from(self.fast);
                fast_now = self.fast;
                fuel = self.fuel;
                pc = self.cur.frames.last().map_or(pc, |f| f.pc);
                // One step, plus the probe steps of the instruction before (see
                // `table::PROBES`; the fast loop charges them at the same point).
                fuel = match crate::table::take_probes() {
                    0 => fuel.saturating_sub(1),
                    n => fuel.saturating_sub(1 + i64::try_from(n).unwrap_or(i64::MAX)),
                };
                if fuel < 0 {
                    // The next slice of fuel, or the stop (fuel, interrupt, deadline).
                    self.fuel = fuel;
                    if let Err(e) = self.refuel() {
                        save!();
                        return Err(e);
                    }
                    fuel = self.fuel;
                }
                let Some(ins) = self.heap.protos.at(pidx as usize).and_then(|p| p.code.get(pc)).copied() else {
                    save!();
                    return Err(LuaError::msg("internal: ran past the end of the code"));
                };
                pc += 1;
                base = frame_base;
                let ins = match ins {
                    // The next instruction, its registers from `frame_base + off`.
                    Instr::Window { off } => {
                        base = frame_base + off as usize;
                        if self.cur.stack.len() < base + width {
                            t!(call!(self.ensure_window(base, width)));
                        }
                        let Some(next) = self.heap.protos.at(pidx as usize).and_then(|p| p.code.get(pc)).copied() else {
                            save!();
                            return Err(LuaError::msg("internal: ran past the end of the code"));
                        };
                        pc += 1;
                        next
                    }
                    other => other,
                };
                match ins {
                    Instr::Window { .. } => {}
                    Instr::MoveAbs { a, b, b_hi } => {
                        // (The source counts from the window: `base` is `frame_base` unless a
                        // `Window` came first.)
                        let (dst, src) = (frame_base + a as usize, base + (usize::from(b) | usize::from(b_hi) << 16));
                        if self.cur.stack.len() <= dst.max(src) {
                            t!(call!(self.ensure_window(dst.max(src), 1)));
                        }
                        let v = self.cur.stack.get(src).copied().unwrap_or(Value::Nil);
                        if let Some(s) = self.cur.stack.get_mut(dst) {
                            *s = v;
                        }
                    }
                    Instr::SetTop { n } => {
                        let t = frame_base + n as usize;
                        t!(call!(self.ensure_stack(t + EXTRA_STACK)));
                        self.top = t;
                    }
                    Instr::PadNil { n } => {
                        let to = frame_base + n as usize;
                        let from = self.top.min(to);
                        // Results adjusted to a count past 16 bits: work by the number filled.
                        t!(call!(self.charge((to - from) as u64 / 8)));
                        t!(call!(self.ensure_stack(to + EXTRA_STACK)));
                        if let Some(s) = self.cur.stack.get_mut(from..to) {
                            s.fill(Value::Nil);
                        }
                    }
                    Instr::Move { a, b } => setr!(usize::from(a), reg!(usize::from(b))),
                    Instr::LoadK { a, k } => {
                        let v = kv!(k);
                        setr!(a, v);
                    }
                    Instr::LoadInt { a, i } => setr!(usize::from(a), Value::Int(i64::from(i))),
                    Instr::LoadBool { a, v } => setr!(usize::from(a), Value::Bool(v)),
                    Instr::LoadFalseSkip { a } => {
                        setr!(usize::from(a), Value::Bool(false));
                        pc += 1;
                    }
                    Instr::LoadNil { a, n } => {
                        // SEC-M7: up to 65,536 registers in one instruction (frames past 255
                        // registers): paid past 16, as every list of values is.
                        t!(call!(self.charge(list_fuel(usize::from(n) + 1))));
                        for i in 0..=usize::from(n) {
                            setr!(usize::from(a) + i, Value::Nil);
                        }
                    }
                    Instr::GetUpval { a, u } => {
                        let v = self.upval_get(closure, u);
                        setr!(a, v);
                    }
                    Instr::GetSelf { a, .. } => setr!(a, Value::Func(closure)),
                    Instr::SetUpval { a, u } => {
                        let v = reg!(usize::from(a));
                        self.upval_set(closure, u, v);
                    }
                    Instr::GetTabUp { a, u, k } => {
                        let t = self.upval_get(closure, u);
                        let key = kv!(k);
                        if let Value::Table(tr) = t {
                            if let Some(v) = self.get_field_cached(tr, key, pidx, pc - 1) {
                                setr!(a, v);
                                continue;
                            }
                        }
                        match ta!(call!(self.index(t, key, Cont::Store(base + usize::from(a)))), [O::Upval(u)]) {
                            Some(v) => setr!(a, v),
                            None => reload!(),
                        }
                    }
                    Instr::SetTabUp { u, k, v } => {
                        let t = self.upval_get(closure, u);
                        let key = kv!(k);
                        let val = rk!(v);
                        // (Through `call!`, as `SetTableK`: a rehash charges fuel, and the
                        // count here must not be written back over it. PERF-SEC-1.)
                        if let Value::Table(tr) = t
                            && t!(call!(self.set_field_fast(tr, key, val)))
                        {
                            continue;
                        }
                        if !ta!(call!(self.set_index(t, key, val)), [O::Upval(u)]) {
                            reload!();
                        }
                    }
                    Instr::GetTable { a, b, c } => {
                        let t = reg!(usize::from(b));
                        let key = reg!(usize::from(c));
                        if let Value::Table(tr) = t
                            && let Some(tab) = self.heap.tables.get(tr.idx)
                            && !matches!(key, Value::Str(r) if self.heap.key_needs_settling(r))
                        {
                            let v = self.heap.get_in(tab, key);
                            if !v.is_nil() || tab.meta.is_none() {
                                setr!(a, v);
                                continue;
                            }
                        }
                        match ta!(call!(self.index(t, key, Cont::Store(base + usize::from(a)))), [O::Reg(b)]) {
                            Some(v) => setr!(a, v),
                            None => reload!(),
                        }
                    }
                    Instr::GetTableK { a, b, k } => {
                        let t = reg!(usize::from(b));
                        let key = kv!(k);
                        if let Value::Table(tr) = t
                            && let Some(v) = self.get_field_cached(tr, key, pidx, pc - 1)
                        {
                            setr!(a, v);
                            continue;
                        }
                        match ta!(call!(self.index(t, key, Cont::Store(base + usize::from(a)))), [O::Reg(b)]) {
                            Some(v) => setr!(a, v),
                            None => reload!(),
                        }
                    }
                    Instr::SelfK { a, b, k } => {
                        let t = reg!(usize::from(b));
                        setr!(usize::from(a) + 1, t);
                        let key = kv!(k);
                        // (The fast loop finds methods; here, the general lookup, which
                        // charges each `__index` link as the fast loop does.)
                        match ta!(call!(self.index(t, key, Cont::Store(base + usize::from(a)))), [O::Reg(b)]) {
                            Some(v) => setr!(a, v),
                            None => reload!(),
                        }
                    }
                    Instr::SetTable { a, b, c } => {
                        let t = reg!(usize::from(a));
                        let key = reg!(usize::from(b));
                        let val = rk!(c);
                        if let Value::Table(tr) = t
                            && self.heap.tables.get(tr.idx).is_some_and(|t| t.meta.is_none())
                        {
                            t!(call!(self.raw_set(tr, key, val)));
                            continue;
                        }
                        if !ta!(call!(self.set_index(t, key, val)), [O::Reg(a)]) {
                            reload!();
                        }
                    }
                    Instr::SetTableK { a, k, c } => {
                        let t = reg!(usize::from(a));
                        let key = kv!(k);
                        let val = rk!(c);
                        if let Value::Table(tr) = t
                            && t!(call!(self.set_field_fast(tr, key, val)))
                        {
                            continue;
                        }
                        if !ta!(call!(self.set_index(t, key, val)), [O::Reg(a)]) {
                            reload!();
                        }
                    }
                    Instr::NewTable { a, arr, hash } => {
                        if self.heap.gc_due() && !crate::nostd::mem::take(&mut self.due_skip) {
                            gc_step_at!(true);
                        }
                        let (room, asize, hash) = crate::bytecode::newtable_sizes(arr, hash);
                        let tr = t!(call!(self.new_table_sized::<true>(room, asize, hash)));
                        setr!(a, Value::Table(tr));
                    }
                    Instr::Add { a, b, c } => arith_rr!(Add, a, b, c),
                    Instr::AddK { a, b, k } => arith_rk!(Add, a, b, k),
                    Instr::Sub { a, b, c } => arith_rr!(Sub, a, b, c),
                    Instr::SubK { a, b, k } => arith_rk!(Sub, a, b, k),
                    Instr::Mul { a, b, c } => arith_rr!(Mul, a, b, c),
                    Instr::MulK { a, b, k } => arith_rk!(Mul, a, b, k),
                    Instr::Div { a, b, c } => arith_rr!(Div, a, b, c),
                    Instr::DivK { a, b, k } => arith_rk!(Div, a, b, k),
                    Instr::Mod { a, b, c } => arith_rr!(Mod, a, b, c),
                    Instr::ModK { a, b, k } => arith_rk!(Mod, a, b, k),
                    Instr::IDiv { a, b, c } => arith_rr!(IDiv, a, b, c),
                    Instr::IDivK { a, b, k } => arith_rk!(IDiv, a, b, k),
                    Instr::Arith { op, a, b, c } => {
                        let x = rk!(b);
                        let y = rk!(c);
                        let fast = if op == ArithOp::Sub && c.is_const() { crate::fast::sub_k(x, y) } else { crate::fast::arith(op, x, y) };
                        match fast {
                            Some(v) => setr!(a, v),
                            None => {
                                match ta!(call!(self.arith(op, x, y, Cont::Store(base + usize::from(a)))), [O::Rk(b), O::Rk(c)]) {
                                    Some(v) => setr!(a, v),
                                    None => reload!(),
                                }
                            }
                        }
                    }
                    Instr::Unm { a, b } => {
                        let x = reg!(usize::from(b));
                        match x {
                            Value::Int(i) => setr!(usize::from(a), Value::Int(i.wrapping_neg())),
                            Value::Num(f) => setr!(usize::from(a), Value::Num(-f)),
                            _ => match ta!(call!(self.unary(Tm::Unm, x, Cont::Store(base + usize::from(a)))), [O::Reg(b)]) {
                                Some(v) => setr!(a, v),
                                None => reload!(),
                            },
                        }
                    }
                    Instr::BNot { a, b } => {
                        let x = reg!(usize::from(b));
                        match x {
                            Value::Int(i) => setr!(usize::from(a), Value::Int(!i)),
                            _ => match ta!(call!(self.unary(Tm::BNot, x, Cont::Store(base + usize::from(a)))), [O::Reg(b)]) {
                                Some(v) => setr!(a, v),
                                None => reload!(),
                            },
                        }
                    }
                    Instr::Not { a, b } => setr!(usize::from(a), Value::Bool(!reg!(usize::from(b)).truthy())),
                    Instr::Len { a, b } => {
                        let x = reg!(usize::from(b));
                        match x {
                            Value::Str(s) => {
                                let n = self.heap.str_bytes(s).len() as i64;
                                setr!(a, Value::Int(n));
                            }
                            Value::Table(tr) if self.heap.tables.get(tr.idx).is_some_and(|t| t.meta.is_none()) => {
                                let n = self.raw_len(tr);
                                setr!(a, Value::Int(n));
                            }
                            _ => match ta!(call!(self.len(x, Cont::Store(base + usize::from(a)))), [O::Reg(b)]) {
                                Some(v) => setr!(a, v),
                                None => reload!(),
                            },
                        }
                    }
                    Instr::Concat { a, n } => {
                        if self.heap.gc_due() && !crate::nostd::mem::take(&mut self.due_skip) {
                            gc_step_at!(true);
                        }
                        let first = base + usize::from(a);
                        let done = match call!(self.concat(first, usize::from(n))) {
                            Ok(d) => d,
                            Err(e) => {
                                // Which operand failed: concat reports it relative to `a`.
                                let e = match e.into_kind() {
                                    ErrorKind::Type { op, ty, operand } => {
                                        let r = a.saturating_add(u16::from(operand));
                                        self.annotate(ErrorKind::Type { op, ty, operand: 0 }.into(), pidx, pc - 1, &[O::Reg(r)])
                                    }
                                    other => other.into(),
                                };
                                save!();
                                return Err(e);
                            }
                        };
                        if !done {
                            reload!();
                        }
                    }
                    Instr::ToStr { a, b } => {
                        let x = reg!(usize::from(b));
                        if matches!(x, Value::Str(_)) {
                            setr!(usize::from(a), x);
                        } else {
                            let s = t!(call!(self.tostring(x)));
                            setr!(a, s);
                        }
                    }
                    Instr::Jmp { off, close } => {
                        pc = (pc as i64 + i64::from(off)) as usize;
                        if close > 0 && !t!(call!(self.close_async(base + usize::from(close) - 1, AfterClose::Nothing, 0))) {
                            reload!();
                        }
                    }
                    Instr::AddI { a, b, i, sub } => {
                        // As `x + i` (or `x - -i`, whose metamethod and message see `-`).
                        let x = reg!(usize::from(b));
                        match crate::fast::arith(ArithOp::Add, x, Value::Int(i64::from(i))) {
                            Some(v) => setr!(a, v),
                            None => {
                                let (op, y) = if sub { (ArithOp::Sub, -i64::from(i)) } else { (ArithOp::Add, i64::from(i)) };
                                match ta!(call!(self.arith(op, x, Value::Int(y), Cont::Store(base + usize::from(a)))), [O::Reg(b)]) {
                                    Some(v) => setr!(a, v),
                                    None => reload!(),
                                }
                            }
                        }
                    }
                    Instr::Pow { a, b, c } => arith_rr!(Pow, a, b, c),
                    Instr::PowK { a, b, k } => arith_rk!(Pow, a, b, k),
                    Instr::BAnd { a, b, c } => arith_rr!(BAnd, a, b, c),
                    Instr::BAndK { a, b, k } => arith_rk!(BAnd, a, b, k),
                    Instr::BOr { a, b, c } => arith_rr!(BOr, a, b, c),
                    Instr::BOrK { a, b, k } => arith_rk!(BOr, a, b, k),
                    Instr::BXor { a, b, c } => arith_rr!(BXor, a, b, c),
                    Instr::BXorK { a, b, k } => arith_rk!(BXor, a, b, k),
                    Instr::Shl { a, b, c } => arith_rr!(Shl, a, b, c),
                    Instr::ShlK { a, b, k } => arith_rk!(Shl, a, b, k),
                    Instr::Shr { a, b, c } => arith_rr!(Shr, a, b, c),
                    Instr::ShrK { a, b, k } => arith_rk!(Shr, a, b, k),
                    Instr::EqK { a, k, eq } => {
                        let (x, y) = (reg!(usize::from(a)), kv!(k));
                        let res = x.raw_eq(y)
                            || match (x, y) {
                                (Value::Str(sa), Value::Str(sb)) => t!(call!(self.str_eq(sa, sb))),
                                _ => false,
                            };
                        if res != eq {
                            pc += 1;
                        }
                    }
                    Instr::EqI { a, i, eq } => {
                        if reg!(usize::from(a)).raw_eq(Value::Int(i64::from(i))) != eq {
                            pc += 1;
                        }
                    }
                    Instr::LtI { a, i, k } | Instr::LeI { a, i, k } | Instr::GtI { a, i, k } | Instr::GeI { a, i, k } => {
                        // Written with the immediate on the right (`<`, `<=`) or on the left.
                        let (r, n) = (reg!(usize::from(a)), Value::Int(i64::from(i)));
                        let (tm, x, y, le) = match ins {
                            Instr::LtI { .. } => (Tm::Lt, r, n, false),
                            Instr::LeI { .. } => (Tm::Le, r, n, true),
                            Instr::GtI { .. } => (Tm::Lt, n, r, false),
                            _ => (Tm::Le, n, r, true),
                        };
                        let res = match crate::fast::less(x, y, le) {
                            Some(r) => r,
                            None => match t!(call!(self.order_slow(tm, x, y, Cont::Cond { k }))) {
                                Some(r) => r,
                                None => reload!(),
                            },
                        };
                        if res != k {
                            pc += 1;
                        }
                    }
                    Instr::Eq { b, c, k } => {
                        let x = rk!(b);
                        let y = rk!(c);
                        let res = if x.raw_eq(y) {
                            true
                        } else {
                            match (x, y) {
                                (Value::Str(sa), Value::Str(sb)) => t!(call!(self.str_eq(sa, sb))),
                                (Value::Table(_), Value::Table(_)) | (Value::UserData(_), Value::UserData(_)) => {
                                    match t!(call!(self.eq_slow(x, y, Cont::Cond { k }))) {
                                        Some(r) => r,
                                        None => reload!(),
                                    }
                                }
                                _ => false,
                            }
                        };
                        if res != k {
                            pc += 1;
                        }
                    }
                    Instr::Lt { b, c, k } => {
                        let x = rk!(b);
                        let y = rk!(c);
                        let res = match (x, y) {
                            _ if let Some(r) = crate::fast::less(x, y, false) => r,
                            (Value::Str(s1), Value::Str(s2)) => {
                                let n = self.heap.str_bytes(s1).len().min(self.heap.str_bytes(s2).len());
                                t!(call!(self.charge_bytes(n)));
                                self.heap.str_bytes(s1) < self.heap.str_bytes(s2)
                            }
                            _ => match t!(call!(self.order_slow(Tm::Lt, x, y, Cont::Cond { k }))) {
                                Some(r) => r,
                                None => reload!(),
                            },
                        };
                        if res != k {
                            pc += 1;
                        }
                    }
                    Instr::Le { b, c, k } => {
                        let x = rk!(b);
                        let y = rk!(c);
                        let res = match (x, y) {
                            _ if let Some(r) = crate::fast::less(x, y, true) => r,
                            (Value::Str(s1), Value::Str(s2)) => {
                                let n = self.heap.str_bytes(s1).len().min(self.heap.str_bytes(s2).len());
                                t!(call!(self.charge_bytes(n)));
                                self.heap.str_bytes(s1) <= self.heap.str_bytes(s2)
                            }
                            _ => match t!(call!(self.order_slow(Tm::Le, x, y, Cont::Cond { k }))) {
                                Some(r) => r,
                                None => reload!(),
                            },
                        };
                        if res != k {
                            pc += 1;
                        }
                    }
                    Instr::Test { a, k } => {
                        if reg!(usize::from(a)).truthy() != k {
                            pc += 1;
                        }
                    }
                    Instr::TestSet { a, b, k } => {
                        let v = reg!(usize::from(b));
                        if v.truthy() == k {
                            setr!(usize::from(a), v);
                        } else {
                            pc += 1;
                        }
                    }
                    Instr::Call { a, b, c } => {
                        let func = base + usize::from(a);
                        let nargs = if b == MULTI { self.top.saturating_sub(func + 1) } else { usize::from(b) };
                        let nres = if c == MULTI { -1 } else { i32::from(c) };
                        // A Lua function: a new frame, run here. A library function: called
                        // straight away.
                        let called = match reg!(usize::from(a)) {
                            Value::Func(f) => match self.heap.funcs.get(f.idx) {
                                Some(Func::Lua { proto, .. }) => {
                                    let proto = *proto;
                                    ta!(call!(self.push_frame(f, proto, func, nargs, nres, FrameKind::Normal, true)), [O::Reg(a)]);
                                    continue 'reload;
                                }
                                Some(Func::Native { .. }) if Some(f) == self.names.yield_fn && self.limit_hit.is_none() => {
                                    call!(self.yield_from_lua(func, nargs, nres))
                                }
                                // `collectgarbage('step')`: its step first, here, where the
                                // call can pause at the end of the host's poll (in the
                                // function it could not); paused, the call runs again on
                                // resume, its step done, as an instruction that steps before
                                // its work does (`gc_step_at!`).
                                Some(Func::Native { f: nf, name, .. })
                                    if Some(f) == self.names.collect_fn && self.limit_hit.is_none() && self.gc_call_before.is_none() && self.basic_step_call(func, nargs) =>
                                {
                                    let (nf, name) = (*nf, *name);
                                    if let Err(e) = call!(self.collect_step()) {
                                        save!();
                                        if e.is_pending() {
                                            if let Some(fr) = self.cur.frames.last_mut() {
                                                fr.pc = pc - 1 - usize::from(base != frame_base);
                                            }
                                            self.fuel += 1;
                                            self.rewound = true;
                                        } else {
                                            self.gc_call_before = None;
                                        }
                                        return Err(e);
                                    }
                                    call!(self.call_native(func, nargs, nres, FrameKind::Normal, true, name, nf))
                                }
                                Some(Func::Native { f: nf, name, in_place, .. }) => {
                                    let (nf, name, op) = (*nf, *name, *in_place);
                                    call!(match if op > 0 { self.call_in_place(op, func, nargs, nres) } else { None } {
                                        Some(r) => r,
                                        None => self.call_native(func, nargs, nres, FrameKind::Normal, true, name, nf),
                                    })
                                }
                                _ => call!(self.precall(func, nargs, nres, FrameKind::Normal, true)),
                            },
                            _ => call!(self.precall(func, nargs, nres, FrameKind::Normal, true)),
                        };
                        match ta!(called, [O::Reg(a)]) {
                            Called::Done => {
                                if self.heap.gc_due() {
                                    gc_step_at!(false);
                                }
                            }
                            Called::Lua | Called::Switched => continue 'reload,
                        }
                    }
                    Instr::TailCall { a, b } => {
                        let func = base + usize::from(a);
                        let mut nargs = if b == MULTI { self.top.saturating_sub(func + 1) } else { usize::from(b) };
                        // A callable object: its `__call` handler goes first, and the call stays
                        // a tail call when the handler is a Lua function.
                        let mut tries = 0;
                        while !matches!(self.st(func), Value::Func(_)) && tries < MAX_TAG_LOOP {
                            let fv = self.st(func);
                            let mm = self.tm(fv, Tm::Call);
                            if mm.is_nil() {
                                break;
                            }
                            t!(call!(self.charge(1 + nargs as u64 / 8)));
                            t!(call!(self.ensure_stack(func + nargs + 2 + EXTRA_STACK)));
                            self.cur.stack.copy_within(func..func + 1 + nargs, func + 1);
                            self.set_st(func, mm);
                            nargs += 1;
                            tries += 1;
                        }
                        let is_lua = matches!(self.st(func), Value::Func(f)
                            if matches!(self.heap.funcs.get(f.idx), Some(Func::Lua { .. })));
                        if is_lua {
                            tpop!(self.tail_call(func, nargs));
                            continue 'reload;
                        }
                        match ta!(call!(self.precall(func, nargs, -1, FrameKind::Normal, true)), [O::Reg(a)]) {
                            Called::Done => {}
                            Called::Lua | Called::Switched => continue 'reload,
                        }
                    }
                    Instr::Return { .. } | Instr::Return0 | Instr::Return1 { .. } => {
                        let (a, b) = match ins {
                            Instr::Return { a, b } => (a, b),
                            Instr::Return1 { a } => (a, 1),
                            _ => (0, 0),
                        };
                        let src = base + usize::from(a);
                        let n = if b == MULTI { self.top.saturating_sub(src) } else { usize::from(b) };
                        // The common case: nothing to close, few results, back to a Lua caller or
                        // to Rust above another frame (a callback: `finish_return` does no more
                        // then; under the last frame a coroutine finishes).
                        if n <= 16
                            && let Some(f) = self.cur.frames.last()
                            && (f.from_lua || self.cur.frames.len() > 1)
                            && matches!(f.kind, FrameKind::Normal)
                            && !self.cur.open_upvals.last().is_some_and(|(i, _)| *i >= base)
                            && !self.cur.tbc.last().is_some_and(|i| *i >= base)
                        {
                            let (func, wanted) = (f.func, f.nresults);
                            let want = if wanted < 0 { n } else { wanted as usize };
                            // (More than 16 wanted is paid: `move_results`, SEC-M7.)
                            if want <= 16 && func + want.max(n) < self.cur.stack.len() {
                                self.cur.stack.copy_within(src..src + n, func);
                                if let Some(rest) = self.cur.stack.get_mut(func + n..func + want) {
                                    rest.fill(Value::Nil);
                                }
                                if wanted < 0 {
                                    self.top = func + n;
                                }
                                self.cur.frames.pop();
                                self.fuel = fuel;
                                continue 'reload;
                            }
                        }
                        tpop!(self.do_return(src, n));
                        continue 'reload;
                    }
                    Instr::ForPrep { a, off } => {
                        if t!(call!(self.for_prep(base + usize::from(a)))) {
                            pc = (pc as i64 + i64::from(off)) as usize;
                        }
                    }
                    Instr::ForLoop { a, off } => {
                        let ra = usize::from(a);
                        match (reg!(ra), reg!(ra + 1), reg!(ra + 2)) {
                            (Value::Int(idx), Value::Int(count), Value::Int(step)) => {
                                if count as u64 > 0 {
                                    let i = idx.wrapping_add(step);
                                    setr!(ra + 1, Value::Int((count as u64 - 1) as i64));
                                    setr!(ra, Value::Int(i));
                                    setr!(ra + 3, Value::Int(i));
                                    pc = (pc as i64 + i64::from(off)) as usize;
                                }
                            }
                            (Value::Num(idx), Value::Num(limit), Value::Num(step)) => {
                                let i = idx + step;
                                if if step > 0.0 { i <= limit } else { limit <= i } {
                                    setr!(ra, Value::Num(i));
                                    setr!(ra + 3, Value::Num(i));
                                    pc = (pc as i64 + i64::from(off)) as usize;
                                }
                            }
                            _ => {}
                        }
                    }
                    Instr::TForPrep { a, off } => {
                        let slot = base + usize::from(a) + 3;
                        let v = self.st(slot);
                        if v.truthy() {
                            if self.tm(v, Tm::Close).is_nil() {
                                save!();
                                return Err(LuaError::msg("variable '(for state)' got a non-closable value"));
                            }
                            self.cur.tbc.push(slot);
                        }
                        pc = (pc as i64 + i64::from(off)) as usize;
                    }
                    Instr::TForCall { a, c } => {
                        let ra = usize::from(a);
                        setr!(ra + 4, reg!(ra));
                        setr!(ra + 5, reg!(ra + 1));
                        setr!(ra + 6, reg!(ra + 2));
                        let func = base + ra + 4;
                        if let Value::Func(f) = reg!(ra + 4)
                            && let Some(Func::Lua { proto, .. }) = self.heap.funcs.get(f.idx)
                        {
                            let proto = *proto;
                            ta!(call!(self.push_frame(f, proto, func, 2, tfor_wanted(c), FrameKind::Normal, true)), [O::Reg(a)]);
                            continue 'reload;
                        }
                        // C Lua names the callee of this call "for iterator" whatever it is.
                        let r = call!(self.precall(func, 2, tfor_wanted(c), FrameKind::Normal, true)).map_err(|e| match e.into_kind() {
                            ErrorKind::Call { ty } => LuaError::Msg(format!("attempt to call a {ty} value (for iterator 'for iterator')")),
                            e => e.into(),
                        });
                        match ta!(r, [O::Reg(a)]) {
                            Called::Done => {}
                            Called::Lua | Called::Switched => continue 'reload,
                        }
                    }
                    Instr::TForLoop { a, off } => {
                        let ra = usize::from(a);
                        let v = reg!(ra + 4);
                        if !v.is_nil() {
                            setr!(ra + 2, v);
                            pc = (pc as i64 + i64::from(off)) as usize;
                        }
                    }
                    Instr::SetList { a, n, off } => {
                        let ra = base + usize::from(a);
                        let count = if n == crate::bytecode::SETLIST_MULTI { self.top.saturating_sub(ra + 1) } else { usize::from(n) };
                        if let Value::Table(tr) = self.st(ra) {
                            save!();
                            // The table the collector traverses in chunks (S6GC-2e): the
                            // entries overwritten are marked first, those written after.
                            let part = tr.idx == self.heap.part;
                            if part {
                                for j in 0..count {
                                    self.heap.part_writing(tr.idx, Value::Int((off as usize + j + 1) as i64));
                                }
                            }
                            // The values straight from the stack (a copy would be an allocation
                            // per constructor).
                            let set = |heap: &mut Heap, stack: &[Value]| {
                                let vals = stack.get(ra + 1..ra + 1 + count).unwrap_or_default();
                                match heap.tables.get_mut(tr.idx) {
                                    Some(tab) => tab.set_list(off as usize, vals, &mut heap.mem),
                                    None => Ok(()),
                                }
                            };
                            // (The fuel the list's growth may take, refused past it: LR-29d.)
                            self.heap.mem.fuel = self.call_fuel();
                            let mut res = set(&mut self.heap, &self.cur.stack);
                            if matches!(res, Err(SetError::Memory)) {
                                t!(call!(self.emergency_gc()));
                                res = set(&mut self.heap, &self.cur.stack);
                            }
                            if part {
                                for j in 0..count {
                                    let v = self.cur.stack.get(ra + 1 + j).copied().unwrap_or_default();
                                    self.heap.part_written(tr.idx, Value::Int((off as usize + j + 1) as i64), if res.is_ok() { v } else { Value::Nil });
                                }
                            }
                            if let Err(e) = res {
                                save!();
                                // (A resize stopped by the interrupt or deadline, or refused
                                // for the fuel left: that limit.)
                                let k = match e {
                                    SetError::Stopped(k) => {
                                        let heap = &mut self.heap;
                                        let units = heap.tables.get_mut(tr.idx).map_or(0, |tab| tab.take_units(&heap.mem));
                                        let _ = self.charge(units);
                                        k
                                    }
                                    SetError::Fuel => {
                                        (self.fuel, self.reserve) = (-1, 0);
                                        LimitKind::Fuel
                                    }
                                    _ => return Err(self.memory_error(false)),
                                };
                                return Err(LuaError::Limit(k));
                            }
                            self.heap.barrier_table(tr);
                            let heap = &mut self.heap;
                            let resized = heap.tables.get_mut(tr.idx).map_or(0, |tab| tab.take_resized(&heap.mem));
                            if resized >= 16 {
                                t!(call!(self.charge(resized as u64 / 16)));
                            }
                        }
                    }
                    Instr::Closure { a, p } => {
                        if self.heap.gc_due() && !crate::nostd::mem::take(&mut self.due_skip) {
                            gc_step_at!(true);
                        }
                        // Upvalues from registers count from the frame's base.
                        let f = t!(call!(self.make_closure(pidx, closure, frame_base, p as usize)));
                        setr!(a, f);
                    }
                    Instr::VarArg { a, n } => {
                        let (vstart, vcount) = self.cur.frames.last().map_or((0, 0), |f| (f.vstart, f.vcount));
                        let ra = base + usize::from(a);
                        if n == MULTI {
                            t!(call!(self.charge(vcount as u64 / 8)));
                            t!(call!(self.ensure_stack(ra + vcount + EXTRA_STACK)));
                            self.cur.stack.copy_within(vstart..vstart + vcount, ra);
                            self.top = ra + vcount;
                        } else {
                            // SEC-M7: a fixed count up to 65,535, paid past 16 as the open one.
                            t!(call!(self.charge(list_fuel(usize::from(n)))));
                            for i in 0..usize::from(n) {
                                let v = if i < vcount { self.st(vstart + i) } else { Value::Nil };
                                self.set_st(ra + i, v);
                            }
                        }
                    }
                    Instr::Close { a } => {
                        if !t!(call!(self.close_async(base + usize::from(a), AfterClose::Nothing, 0))) {
                            reload!();
                        }
                    }
                    Instr::Tbc { a } => {
                        let v = reg!(usize::from(a));
                        if v.truthy() {
                            if self.tm(v, Tm::Close).is_nil() {
                                save!();
                                let name = self
                                    .proto(pidx)
                                    .and_then(|p| crate::debug::local_name(p, pc - 1, a))
                                    .unwrap_or_else(|| "?".into());
                                return Err(LuaError::msg(format!("variable '{name}' got a non-closable value")));
                            }
                            self.cur.tbc.push(base + usize::from(a));
                        }
                    }
                }
            }
        }
    }

    /// `t[key]` for a constant string key through the instruction's slot hint, when the
    /// table has it (or has no metatable to ask): None means "take the general path".
    #[inline]
    fn get_field_cached(&mut self, t: TableRef, key: Value, pidx: u32, pc: usize) -> Option<Value> {
        let tab = self.heap.tables.get(t.idx)?;
        let hint = self.heap.protos.at(pidx as usize).and_then(|p| p.hints.get(pc)).map_or(0, crate::nostd::cell::Cell::get);
        let (v, slot) = tab.get_hinted(key, hint);
        if let Some(slot) = slot
            && slot != hint
            && let Some(h) = self.heap.protos.at(pidx as usize).and_then(|p| p.hints.get(pc))
        {
            h.set(slot);
        }
        if !v.is_nil() || tab.meta.is_none() { Some(v) } else { None }
    }



    /// `t[key] = val` in place when the key exists and nothing intercepts (no metatable, or
    /// the field is there already): true when done.
    #[inline]
    fn set_field_fast(&mut self, t: TableRef, key: Value, val: Value) -> Result<bool, LuaError> {
        let Some(tab) = self.heap.tables.get(t.idx) else { return Ok(true) };
        if tab.meta.is_some() && self.get_in_settled(t.idx, key)?.is_nil() {
            return Ok(false);
        }
        self.raw_set(t, key, val)?;
        Ok(true)
    }

    fn tail_call(&mut self, func: usize, nargs: usize) -> Result<(), LuaError> {
        let Some(frame) = self.cur.frames.last() else { return Ok(()) };
        let (fbase, ffunc, nres, kind, from_lua, handler) =
            (frame.base, frame.func, frame.nresults, frame.kind, frame.from_lua, frame.handler);
        self.close_upvals(fbase);
        if self.cur.tbc.last().is_some_and(|&s| s >= fbase) {
            self.close_level(fbase)?;
        }
        if nargs > 16 {
            self.charge(nargs as u64 / 8)?;
        }
        self.cur.stack.copy_within(func..func + 1 + nargs, ffunc);
        self.cur.frames.pop();
        // The new frame takes the old one's place, unrolled or not (`ThreadState::unrolled`).
        let unrolled = self.cur.unrolled;
        let depth = self.cur.frames.len();
        let r = self.precall(ffunc, nargs, nres, kind, from_lua);
        if matches!(r, Ok(Called::Lua))
            && self.cur.frames.len() == depth + 1
            && let Some(f) = self.cur.frames.get_mut(depth)
        {
            f.tail = true;
        }
        if self.cur.frames.len() <= unrolled {
            self.cur.unrolled = unrolled;
        }
        if let Some(f) = self.cur.frames.last_mut()
            && handler.is_some()
            && let (FrameKind::Protected { dest, .. }, FrameKind::Protected { dest: was, .. }) = (f.kind, kind)
            && dest == was
        {
            f.handler = handler;
        }
        r.map(|_| ())
    }

    fn make_closure(&mut self, parent: u32, closure: FuncRef, base: usize, p: usize) -> Result<Value, LuaError> {
        loop {
            match new_closure(&mut self.heap, &mut self.cur.open_upvals, self.cur_ref, parent, closure, base, p) {
                Ok((v, extra)) => {
                    // Its upvalues (S5B-WALK-5; `charge` does not collect).
                    if extra > 0 {
                        self.charge(extra)?;
                    }
                    return Ok(v);
                }
                Err(None) => return Err(LuaError::msg("internal: no such prototype")),
                // Not enough memory: collect (or stop at the limit), then make it.
                Err(Some(size)) => {
                    self.charge_mem(size)?;
                    self.heap.mem.release(size);
                }
            }
        }
    }

    /// Numeric `for` preparation: false to run the loop, true to skip it.
    fn for_prep(&mut self, ra: usize) -> Result<bool, LuaError> {
        let (init, limit, step) = (self.st(ra), self.st(ra + 1), self.st(ra + 2));
        if let (Value::Int(i), Value::Int(s)) = (init, step) {
            if s == 0 {
                return Err(LuaError::msg("'for' step is zero"));
            }
            let lim = match limit {
                Value::Int(l) => l,
                // Exactly, through the integer first (a numeric string included).
                other => match self.tonumber_charged(other)? {
                    Some(Value::Int(l)) => l,
                    Some(Value::Num(f)) => {
                        match if s < 0 { number::float_ceil_to_int(f) } else { number::float_floor_to_int(f) } {
                            Some(l) => l,
                            None => {
                                if (f > 0.0) == (s < 0) {
                                    return Ok(true);
                                }
                                if f > 0.0 { i64::MAX } else { i64::MIN }
                            }
                        }
                    }
                    _ => {
                        return Err(LuaError::msg(format!(
                            "bad 'for' limit (number expected, got {})",
                            self.obj_type_name(other)
                        )));
                    }
                },
            };
            if if s > 0 { i > lim } else { i < lim } {
                return Ok(true);
            }
            let count = if s > 0 {
                (lim as u64).wrapping_sub(i as u64) / (s as u64)
            } else {
                let s1 = (s.wrapping_add(1)).wrapping_neg() as u64 + 1;
                (i as u64).wrapping_sub(lim as u64) / s1
            };
            self.set_st(ra + 1, Value::Int(count as i64));
            self.set_st(ra + 3, Value::Int(i));
            return Ok(false);
        }
        let f = |v: Value, what: &str, s: &mut Self| -> Result<f64, LuaError> {
            match s.tonumber_charged(v)?.and_then(Value::as_num) {
                Some(f) => Ok(f),
                None => Err(LuaError::msg(format!("bad 'for' {what} (number expected, got {})", s.obj_type_name(v)))),
            }
        };
        let l = f(limit, "limit", self)?;
        let st = f(step, "step", self)?;
        let i = f(init, "initial value", self)?;
        if st == 0.0 {
            return Err(LuaError::msg("'for' step is zero"));
        }
        if if st > 0.0 { l < i } else { i < l } {
            return Ok(true);
        }
        self.set_st(ra, Value::Num(i));
        self.set_st(ra + 1, Value::Num(l));
        self.set_st(ra + 2, Value::Num(st));
        self.set_st(ra + 3, Value::Num(i));
        Ok(false)
    }

    // ---- metamethod paths ----

    /// Puts `f` and `args` at slot `pos`, to be called there.
    #[inline]
    fn place_call(&mut self, pos: usize, f: Value, args: &[Value]) -> Result<(), LuaError> {
        self.ensure_stack(pos + args.len() + 1 + EXTRA_STACK)?;
        self.set_st(pos, f);
        for (i, a) in args.iter().enumerate() {
            self.set_st(pos + 1 + i, *a);
        }
        Ok(())
    }

    /// `precall` of a metamethod for event `ev` (named in the callee's messages).
    #[inline]
    fn precall_event(
        &mut self,
        ev: Option<&'static str>,
        pos: usize,
        nargs: usize,
        nres: i32,
        kind: FrameKind,
    ) -> Result<Called, LuaError> {
        self.pending_event = ev;
        let r = self.precall(pos, nargs, nres, kind, false);
        self.pending_event = None;
        r
    }

    /// Calls metamethod `f` with `args` for an instruction: Some(result) when done now
    /// (a Rust function), None when a Lua frame was pushed (its result goes by `cont`).
    /// `ev` names the metamethod when `f` cannot be called (C's `funcnamefromcode`).
    fn call_meta(&mut self, ev: Tm, f: Value, args: &[Value], cont: Cont) -> Result<Option<Value>, LuaError> {
        if !matches!(f, Value::Func(_)) && self.tm(f, Tm::Call).is_nil() {
            let name = TM_NAMES.get(ev as usize).map_or("?", |n| n.get(2..).unwrap_or(n));
            return Err(LuaError::msg(format!("attempt to call a {} value (metamethod '{name}')", self.obj_type_name(f))));
        }
        let pos = self.call_top();
        self.place_call(pos, f, args)?;
        let saved_ntop = self.ntop;
        self.ntop = self.ntop.max(pos + 1 + args.len());
        let r = self.precall_event(TM_NAMES.get(ev as usize).and_then(|n| n.get(2..)), pos, args.len(), 1, FrameKind::Cont(cont));
        self.ntop = saved_ntop;
        match r? {
            // A Lua frame (or `pcall` of one) delivers its result through `cont`; so does a
            // resumed or yielding coroutine (`wrap`, `coroutine.yield` as a metamethod).
            Called::Lua | Called::Switched => Ok(None),
            Called::Done => Ok(Some(self.st(pos))),
        }
    }

    /// Indexing `t`, which is not a table and has no metamethod, at link `loop_n` of a chain.
    fn index_error(&self, t: Value, loop_n: usize) -> LuaError {
        let ty = self.obj_type_name(t);
        if loop_n == 0 {
            ErrorKind::Type { op: "index", ty, operand: 0 }.into()
        } else {
            LuaError::msg(format!("attempt to index a {ty} value"))
        }
    }

    /// `t[k]` with metamethods.
    pub(crate) fn index(&mut self, t: Value, key: Value, cont: Cont) -> Result<Option<Value>, LuaError> {
        let mut t = t;
        for loop_n in 0..MAX_TAG_LOOP {
            let mm = if let Value::Table(tr) = t {
                if self.heap.tables.get(tr.idx).is_none() {
                    return Ok(Some(Value::Nil));
                }
                let v = self.get_in_settled(tr.idx, key)?;
                let Some(tab) = self.heap.tables.get(tr.idx) else { return Ok(Some(Value::Nil)) };
                if !v.is_nil() {
                    return Ok(Some(v));
                }
                let Some(m) = tab.meta else { return Ok(Some(Value::Nil)) };
                let mm = self.tm_in(m, Tm::Index);
                if mm.is_nil() {
                    return Ok(Some(Value::Nil));
                }
                mm
            } else {
                let mm = self.tm(t, Tm::Index);
                if mm.is_nil() {
                    return Err(self.index_error(t, loop_n));
                }
                mm
            };
            if let Value::Func(_) = mm {
                return self.call_meta(Tm::Index, mm, &[t, key], cont);
            }
            // Each link of a chain is a step, and so is its lookup's probing (S5GC-1).
            self.charge(link_cost(loop_n) + crate::table::take_probes())?;
            t = mm;
        }
        Err(LuaError::msg("'__index' chain too long; possible loop"))
    }

    /// `t[k] = v` with metamethods: true when done now, false when a Lua `__newindex`
    /// frame was pushed.
    pub(crate) fn set_index(&mut self, t: Value, key: Value, val: Value) -> Result<bool, LuaError> {
        let mut t = t;
        for loop_n in 0..MAX_TAG_LOOP {
            let mm = if let Value::Table(tr) = t {
                let Some(tab) = self.heap.tables.get(tr.idx) else { return Ok(true) };
                let mm = match tab.meta {
                    None => Value::Nil,
                    Some(m) => {
                        if !self.get_in_settled(tr.idx, key)?.is_nil() {
                            Value::Nil
                        } else {
                            self.tm_in(m, Tm::NewIndex)
                        }
                    }
                };
                if mm.is_nil() {
                    self.raw_set(tr, key, val)?;
                    return Ok(true);
                }
                mm
            } else {
                let mm = self.tm(t, Tm::NewIndex);
                if mm.is_nil() {
                    return Err(self.index_error(t, loop_n));
                }
                mm
            };
            if let Value::Func(_) = mm {
                return Ok(self.call_meta(Tm::NewIndex, mm, &[t, key, val], Cont::Discard)?.is_some());
            }
            // Each link of a chain is a step, and so is its lookup's probing (S5GC-1).
            self.charge(link_cost(loop_n) + crate::table::take_probes())?;
            t = mm;
        }
        Err(LuaError::msg("'__newindex' chain too long; possible loop"))
    }

    /// `t[k]` from Rust (nested calls for metamethods).
    pub(crate) fn index_nested(&mut self, t: Value, key: Value) -> Result<Value, LuaError> {
        self.settle_key(key)?;
        let mut t = t;
        for link_n in 0..MAX_TAG_LOOP {
            let mm = if let Value::Table(tr) = t {
                let v = self.raw_get(tr, key);
                if !v.is_nil() {
                    // The native reading it may hold it across allocations.
                    self.hold(v);
                    return Ok(v);
                }
                let mm = self.tm(t, Tm::Index);
                if mm.is_nil() {
                    return Ok(Value::Nil);
                }
                mm
            } else {
                let mm = self.tm(t, Tm::Index);
                if mm.is_nil() {
                    return Err(LuaError::Plain(format!("attempt to index a {} value", self.obj_type_name(t))));
                }
                mm
            };
            if let Value::Func(_) = mm {
                return self.call_nested1(mm, &[t, key]);
            }
            // Each link of a chain is a step, and so is its lookup's probing (S5GC-1).
            self.charge(link_cost(link_n) + crate::table::take_probes())?;
            t = mm;
        }
        Err(LuaError::Plain("'__index' chain too long; possible loop".into()))
    }

    /// `t[k] = v` from Rust.
    pub(crate) fn set_index_nested(&mut self, t: Value, key: Value, val: Value) -> Result<(), LuaError> {
        self.settle_key(key)?;
        let mut t = t;
        for link_n in 0..MAX_TAG_LOOP {
            let mm = match t {
                Value::Table(tr) => {
                    let has = !self.raw_get(tr, key).is_nil();
                    let mm = match self.metatable(t) {
                        Some(m) if !has => self.tm_in(m, Tm::NewIndex),
                        _ => Value::Nil,
                    };
                    if mm.is_nil() {
                        return self.raw_set(tr, key, val);
                    }
                    mm
                }
                _ => {
                    let mm = self.tm(t, Tm::NewIndex);
                    if mm.is_nil() {
                        return Err(LuaError::Plain(format!("attempt to index a {} value", self.obj_type_name(t))));
                    }
                    mm
                }
            };
            if let Value::Func(_) = mm {
                self.call_nested(mm, &[t, key, val])?;
                return Ok(());
            }
            // Each link of a chain is a step, and so is its lookup's probing (S5GC-1).
            self.charge(link_cost(link_n) + crate::table::take_probes())?;
            t = mm;
        }
        Err(LuaError::Plain("'__newindex' chain too long; possible loop".into()))
    }

    /// A number from a value, converting strings (Lua's arithmetic coercion).
    pub(crate) fn tonumber(&self, v: Value) -> Option<Value> {
        match v {
            Value::Int(_) | Value::Num(_) => Some(v),
            Value::Str(s) => number::str_to_number(self.bytes(s)),
            _ => None,
        }
    }

    /// `tonumber`, charging fuel for the length of a string it parses: a unit per 16 bytes,
    /// what the slowest numerals cost (a long fraction or digit run takes the float
    /// parser's big-number path; at a unit per 64 bytes they ran 4 to 5x plain code: LR-22).
    pub(crate) fn tonumber_charged(&mut self, v: Value) -> Result<Option<Value>, LuaError> {
        if let Value::Str(s) = v {
            let n = self.bytes(s).len();
            if n >= 16 {
                self.charge(n as u64 / 16)?;
            }
        }
        Ok(self.tonumber(v))
    }

    /// Fuel for comparing or copying `n` bytes.
    #[inline]
    pub(crate) fn charge_bytes(&mut self, n: usize) -> Result<(), LuaError> {
        self.charge(crate::heap::bytes_fuel(n))
    }

    /// An integer from a value (exact floats and numeric strings convert).
    pub(crate) fn tointeger(&self, v: Value) -> Option<i64> {
        match v {
            Value::Int(i) => Some(i),
            Value::Num(f) => number::float_to_int(f),
            Value::Str(_) => self.tonumber(v).and_then(Value::as_int),
            _ => None,
        }
    }

    /// Arithmetic with coercion and metamethods.
    pub(crate) fn arith(&mut self, op: ArithOp, x: Value, y: Value, cont: Cont) -> Result<Option<Value>, LuaError> {
        // A string operand: whatever the string metatable says (its own `__add` … is done
        // here directly; a script's replacement is called).
        if !is_bitwise(op) && self.string_meta.is_some() && (matches!(x, Value::Str(_)) || matches!(y, Value::Str(_))) {
            let ev = arith_tm(op);
            let mm = self.tm2(x, y, ev);
            if mm.is_nil() {
                let culprit = if matches!(x, Value::Int(_) | Value::Num(_)) { 1 } else { 0 };
                let ty = self.obj_type_name(if culprit == 0 { x } else { y });
                return Err(ErrorKind::Type { op: "perform arithmetic on", ty, operand: culprit }.into());
            }
            if !self.is_string_builtin(mm) {
                return self.call_meta(ev, mm, &[x, y], cont);
            }
            return self.string_arith_now(op, x, y).map(Some);
        }
        if is_bitwise(op) {
            let ints = (int_of(x), int_of(y));
            if let (Some(i), Some(j)) = ints {
                return Ok(Some(Value::Int(int_arith(op, i, j)?)));
            }
        } else if let (Some(a), Some(b)) = (self.tonumber_charged(x)?, self.tonumber_charged(y)?) {
            let r = arith_values(op, a, b);
            // With a string operand the work happens in the string library's metamethod,
            // whose errors carry no position.
            if self.string_arith(x) || self.string_arith(y) {
                return r.map(Some).map_err(LuaError::plain);
            }
            return r.map(Some);
        }
        let mut mm = self.tm(x, arith_tm(op));
        if mm.is_nil() && !is_bitwise(op) && self.string_arith(x) {
            return self.string_arith_fail(op, x, y).map(Some);
        }
        if mm.is_nil() {
            mm = self.tm(y, arith_tm(op));
            if mm.is_nil() && !is_bitwise(op) && self.string_arith(y) {
                return self.string_arith_fail(op, x, y).map(Some);
            }
        }
        if !mm.is_nil() {
            return self.call_meta(arith_tm(op), mm, &[x, y], cont);
        }
        if is_bitwise(op) {
            if matches!(x, Value::Int(_) | Value::Num(_)) && matches!(y, Value::Int(_) | Value::Num(_)) {
                return Err(ErrorKind::IntRepr { operand: if int_of(x).is_none() { 0 } else { 1 } }.into());
            }
            let culprit = if matches!(x, Value::Int(_) | Value::Num(_)) { 1 } else { 0 };
            let ty = self.obj_type_name(if culprit == 0 { x } else { y });
            return Err(ErrorKind::Type { op: "perform bitwise operation on", ty, operand: culprit }.into());
        }
        let culprit = if self.tonumber(x).is_some() { 1 } else { 0 };
        let ty = self.obj_type_name(if culprit == 0 { x } else { y });
        Err(ErrorKind::Type { op: "perform arithmetic on", ty, operand: culprit }.into())
    }

    /// Whether `v` is a string whose arithmetic goes through the string library's
    /// metamethods (as in Lua 5.4, when the string library is open).
    fn string_arith(&self, v: Value) -> bool {
        matches!(v, Value::Str(_)) && self.string_meta.is_some()
    }

    /// Whether `f` is one of the string library's own arithmetic metamethods.
    fn is_string_builtin(&self, f: Value) -> bool {
        let Value::Func(r) = f else { return false };
        matches!(self.heap.funcs.get(r.idx), Some(Func::Native { name, .. }) if name.starts_with("string.__"))
    }

    /// The string library's arithmetic, called directly (`getmetatable("").__add(a, b)`).
    pub(crate) fn string_arith_now(&mut self, op: ArithOp, x: Value, y: Value) -> Result<Value, LuaError> {
        if let (Some(a), Some(b)) = (self.tonumber_charged(x)?, self.tonumber_charged(y)?) {
            return arith_values(op, a, b).map_err(LuaError::plain);
        }
        self.string_arith_fail(op, x, y)
    }

    /// The string library's arithmetic metamethod when coercion failed (`trymt`): the
    /// second operand's own metamethod, or "attempt to add a 'string' with a 'table'".
    /// C Lua calls that metamethod from the string library (`lua_call`): it is not
    /// yieldable, error levels count the library function, and a value that is not
    /// callable is reported without a position.
    fn string_arith_fail(&mut self, op: ArithOp, x: Value, y: Value) -> Result<Value, LuaError> {
        let ev = arith_tm(op);
        let mm = if matches!(y, Value::Str(_)) { Value::Nil } else { self.tm(y, ev) };
        if mm.is_nil() {
            let name = TM_NAMES.get(ev as usize).map_or("?", |n| n.get(2..).unwrap_or(n));
            return Err(LuaError::msg(format!("attempt to {name} a '{}' with a '{}'", x.type_name(), y.type_name())));
        }
        self.call_nested1(mm, &[x, y])
    }

    fn unary(&mut self, e: Tm, x: Value, cont: Cont) -> Result<Option<Value>, LuaError> {
        match e {
            Tm::Unm => {
                if let Some(n) = self.tonumber_charged(x)? {
                    return Ok(Some(match n {
                        Value::Int(i) => Value::Int(i.wrapping_neg()),
                        Value::Num(f) => Value::Num(-f),
                        o => o,
                    }));
                }
            }
            _ => {
                if let Some(i) = int_of(x) {
                    return Ok(Some(Value::Int(!i)));
                }
            }
        }
        let mm = self.tm(x, e);
        if e == Tm::Unm && self.string_arith(x) && self.is_string_builtin(mm) {
            return Err(LuaError::msg("attempt to unm a 'string' with a 'string'"));
        }
        if !mm.is_nil() {
            return self.call_meta(e, mm, &[x, x], cont);
        }
        if e == Tm::BNot {
            if let Value::Num(_) = x {
                return Err(ErrorKind::IntRepr { operand: 0 }.into());
            }
            return Err(ErrorKind::Type { op: "perform bitwise operation on", ty: self.obj_type_name(x), operand: 0 }.into());
        }
        Err(ErrorKind::Type { op: "perform arithmetic on", ty: self.obj_type_name(x), operand: 0 }.into())
    }

    /// `#x` with `__len`.
    pub(crate) fn len(&mut self, x: Value, cont: Cont) -> Result<Option<Value>, LuaError> {
        if let Value::Str(s) = x {
            return Ok(Some(Value::Int(self.bytes(s).len() as i64)));
        }
        let mm = self.tm(x, Tm::Len);
        if !mm.is_nil() {
            return self.call_meta(Tm::Len, mm, &[x, x], cont);
        }
        if let Value::Table(t) = x {
            return Ok(Some(Value::Int(self.raw_len(t))));
        }
        Err(ErrorKind::Type { op: "get length of", ty: self.obj_type_name(x), operand: 0 }.into())
    }

    /// `#x` from Rust.
    pub(crate) fn len_nested(&mut self, x: Value) -> Result<Value, LuaError> {
        if let Value::Str(s) = x {
            return Ok(Value::Int(self.bytes(s).len() as i64));
        }
        let mm = self.tm(x, Tm::Len);
        if !mm.is_nil() {
            return self.call_nested1(mm, &[x, x]);
        }
        if let Value::Table(t) = x {
            return Ok(Value::Int(self.raw_len(t)));
        }
        Err(LuaError::Plain(format!("attempt to get length of a {} value", self.obj_type_name(x))))
    }

    fn eq_slow(&mut self, x: Value, y: Value, cont: Cont) -> Result<Option<bool>, LuaError> {
        let mm = self.tm2(x, y, Tm::Eq);
        if mm.is_nil() {
            return Ok(Some(false));
        }
        Ok(self.call_meta(Tm::Eq, mm, &[x, y], cont)?.map(Value::truthy))
    }

    /// Whether two strings are equal (comparing long strings' bytes is charged).
    pub(crate) fn str_eq(&mut self, a: StrRef, b: StrRef) -> Result<bool, LuaError> {
        if let Some(r) = self.heap.str_eq_quick(a, b) {
            return Ok(r);
        }
        let len = self.bytes(a).len();
        self.charge_bytes(len)?;
        // A long pair a piece at a time, each a look at the deadline (S6T-EQ: two 16 MiB
        // strings were compared in one go, 1.5 ms).
        let mut at = 0usize;
        loop {
            let end = at.saturating_add(crate::stdlib::COPY_PIECE).min(len);
            if self.heap.diff_piece(a.idx, b.idx, at, end).is_some() {
                return Ok(false);
            }
            if end >= len {
                return Ok(self.bytes(a).len() == self.bytes(b).len());
            }
            at = end;
            self.check_time()?;
        }
    }

    /// `x == y` from Rust.
    pub(crate) fn eq_nested(&mut self, x: Value, y: Value) -> Result<bool, LuaError> {
        if x.raw_eq(y) {
            return Ok(true);
        }
        match (x, y) {
            (Value::Str(a), Value::Str(b)) => self.str_eq(a, b),
            (Value::Table(_), Value::Table(_)) | (Value::UserData(_), Value::UserData(_)) => {
                let mm = self.tm2(x, y, Tm::Eq);
                if mm.is_nil() {
                    return Ok(false);
                }
                Ok(self.call_nested1(mm, &[x, y])?.truthy())
            }
            _ => Ok(false),
        }
    }

    fn order_slow(&mut self, e: Tm, x: Value, y: Value, cont: Cont) -> Result<Option<bool>, LuaError> {
        let mm = self.tm2(x, y, e);
        if mm.is_nil() && e == Tm::Le && !self.limits.strict_5_4 {
            // `LUA_COMPAT_LT_LE` (stock 5.4 builds): `a <= b` as `not (b < a)`.
            let lt = self.tm2(y, x, Tm::Lt);
            if !lt.is_nil() {
                let cont = match cont {
                    Cont::Cond { k } => Cont::Cond { k: !k },
                    c => c,
                };
                return Ok(self.call_meta(Tm::Le, lt, &[y, x], cont)?.map(|v| !v.truthy()));
            }
        }
        if mm.is_nil() {
            return Err(self.compare_error(x, y));
        }
        Ok(self.call_meta(e, mm, &[x, y], cont)?.map(Value::truthy))
    }

    pub(crate) fn compare_error(&self, x: Value, y: Value) -> LuaError {
        let (t1, t2) = (self.obj_type_name(x), self.obj_type_name(y));
        if t1 == t2 {
            LuaError::msg(format!("attempt to compare two {t1} values"))
        } else {
            LuaError::msg(format!("attempt to compare {t1} with {t2}"))
        }
    }

    /// `x < y` from Rust.
    pub(crate) fn less_than(&mut self, x: Value, y: Value) -> Result<bool, LuaError> {
        Ok(match (x, y) {
            (Value::Int(i), Value::Int(j)) => i < j,
            (Value::Num(f), Value::Num(g)) => f < g,
            (Value::Int(i), Value::Num(g)) => number::int_lt_float(i, g),
            (Value::Num(f), Value::Int(j)) => number::float_lt_int(f, j),
            (Value::Str(a), Value::Str(b)) => {
                let n = self.bytes(a).len().min(self.bytes(b).len());
                self.charge_bytes(n)?;
                self.bytes(a) < self.bytes(b)
            }
            _ => {
                let mut mm = self.tm(x, Tm::Lt);
                if mm.is_nil() {
                    mm = self.tm(y, Tm::Lt);
                }
                if mm.is_nil() {
                    return Err(self.compare_error(x, y).plain());
                }
                self.call_nested1(mm, &[x, y])?.truthy()
            }
        })
    }

    /// Concatenates `n` values from slot `a` into slot `a`: true when done, false when a
    /// Lua `__concat` frame was pushed.
    pub(crate) fn concat(&mut self, a: usize, n: usize) -> Result<bool, LuaError> {
        let mut n = n;
        while n > 1 {
            let x = self.st(a + n - 2);
            let y = self.st(a + n - 1);
            if stringable(x) && stringable(y) {
                // The longest run of strings and numbers ending at the top, joined at once.
                let mut k = n - 1;
                while k > 0 && stringable(self.st(a + k - 1)) {
                    k -= 1;
                }
                let mut total = 0usize;
                for i in k..n {
                    total = total.saturating_add(self.str_len_of(self.st(a + i)));
                }
                if total > self.limits.string_len {
                    return Err(LuaError::Limit(LimitKind::Memory));
                }
                let floats = (k..n).filter(|&i| matches!(self.st(a + i), Value::Num(_))).count() as u64;
                self.charge((total / 64) as u64 + floats * FLOAT_TEXT_FUEL)?;
                // One buffer of the exact size, one copy of each part: a short result is
                // built on the stack and interned; a long one is charged first, then its
                // buffer becomes the string.
                let s = if total <= crate::heap::SHORT_LEN {
                    let mut buf = [0u8; crate::heap::SHORT_LEN];
                    let mut w: &mut [u8] = &mut buf;
                    for i in k..n {
                        self.append_stringable(&mut w, self.st(a + i));
                    }
                    let len = crate::heap::SHORT_LEN - w.len();
                    self.new_str(buf.get(..len).unwrap_or_default())?
                } else {
                    self.charge_mem(crate::heap::str_mem(total))?;
                    let mut buf = Vec::with_capacity(total);
                    for i in k..n {
                        self.append_stringable(&mut buf, self.st(a + i));
                    }
                    let v = Value::Str(self.heap.new_long_charged(buf.into_boxed_slice()));
                    self.hold(v);
                    v
                };
                self.set_st(a + k, s);
                n = k + 1;
                continue;
            }
            let mm = self.tm2(x, y, Tm::Concat);
            if mm.is_nil() {
                let culprit = if stringable(x) { n - 1 } else { n - 2 };
                let v = if stringable(x) { y } else { x };
                return Err(ErrorKind::Type { op: "concatenate", ty: self.obj_type_name(v), operand: culprit as u8 }.into());
            }
            match self.call_meta(Tm::Concat, mm, &[x, y], Cont::Concat { a, n: n - 1 })? {
                Some(v) => {
                    self.set_st(a + n - 2, v);
                    n -= 1;
                }
                None => return Ok(false),
            }
        }
        Ok(true)
    }

    /// The exact length of a string or number as `..` writes it.
    fn str_len_of(&self, v: Value) -> usize {
        stringable_len(&self.heap, v)
    }

    /// Writes a string or number as `..` does (into a buffer sized by `str_len_of`).
    pub(crate) fn append_stringable(&self, out: &mut impl crate::nostd::io::Write, v: Value) {
        write_stringable(&self.heap, out, v);
    }

    /// `tostring(v)`.
    pub(crate) fn tostring(&mut self, v: Value) -> Result<Value, LuaError> {
        let mm = self.tm(v, Tm::ToString);
        if !mm.is_nil() {
            let r = self.call_nested1(mm, &[v])?;
            if !matches!(r, Value::Str(_)) {
                if let Value::Int(_) | Value::Num(_) = r {
                    let mut b = Vec::new();
                    self.append_stringable(&mut b, r);
                    return self.new_str(&b);
                }
                return Err(LuaError::msg("'__tostring' must return a string"));
            }
            return Ok(r);
        }
        let text = match v {
            Value::Str(_) => return Ok(v),
            Value::Nil => "nil".to_owned(),
            Value::Bool(b) => return self.new_str(if b { b"true" } else { b"false" }),
            Value::Int(_) => {
                let mut buf = [0u8; 20];
                let mut w: &mut [u8] = &mut buf;
                self.append_stringable(&mut w, v);
                let n = 20 - w.len();
                return self.new_str(buf.get(..n).unwrap_or_default());
            }
            Value::Num(f) => {
                self.charge(FLOAT_TEXT_FUEL)?;
                number::fmt_float(f)
            }
            _ => {
                // The name (maybe `__name`, read as `luaL_tolstring` reads it) and the
                // address, formatted.
                self.charge(4)?;
                let kind = self.arg_type_name(v);
                format!("{kind}: 0x{:08x}", address_of(v))
            }
        };
        self.new_str(text.as_bytes())
    }
}

/// The exact length of a string or number as `..` writes it.
pub(crate) fn stringable_len(heap: &Heap, v: Value) -> usize {
    match v {
        Value::Str(s) => heap.str_bytes(s).len(),
        Value::Int(i) => i.unsigned_abs().checked_ilog10().map_or(1, |d| d as usize + 1) + usize::from(i < 0),
        Value::Num(f) => number::fmt_float(f).len(),
        _ => 0,
    }
}

/// Writes a string or number as `..` does (into a buffer sized by `stringable_len`).
pub(crate) fn write_stringable(heap: &Heap, out: &mut impl crate::nostd::io::Write, v: Value) {
    let _ = match v {
        Value::Str(s) => out.write_all(heap.str_bytes(s)),
        Value::Int(i) => write_int(out, i),
        Value::Num(f) => out.write_all(number::fmt_float(f).as_bytes()),
        _ => Ok(()),
    };
}

/// An integer's decimal text into `out`.
#[inline]
pub(crate) fn write_int(out: &mut impl crate::nostd::io::Write, i: i64) -> crate::nostd::io::Result<()> {
    let mut digits = [0u8; 20];
    let mut at = digits.len();
    let mut n = i.unsigned_abs();
    loop {
        at -= 1;
        if let Some(d) = digits.get_mut(at) {
            *d = b'0' + (n % 10) as u8;
        }
        n /= 10;
        if n == 0 {
            break;
        }
    }
    if i < 0 {
        let _ = out.write_all(b"-");
    }
    out.write_all(digits.get(at..).unwrap_or_default())
}

/// A stable pseudo-address for `tostring` of objects.
pub(crate) fn address_of(v: Value) -> u64 {
    let (kind, idx) = match v {
        Value::Table(r) => (1u64, r.idx),
        Value::Func(r) => (2, r.idx),
        Value::UserData(r) => (3, r.idx),
        Value::Thread(r) => (4, r.idx),
        Value::Str(r) => (5, r.idx),
        _ => (0, 0),
    };
    0x5500_0000_0000 | (kind << 36) | (u64::from(idx) << 4)
}

fn stringable(v: Value) -> bool {
    matches!(v, Value::Str(_) | Value::Int(_) | Value::Num(_))
}

/// An integer from a number for bitwise operations (strings do not convert, as in 5.4).
fn int_of(v: Value) -> Option<i64> {
    match v {
        Value::Int(i) => Some(i),
        Value::Num(f) => number::float_to_int(f),
        _ => None,
    }
}

#[inline]
fn float_arith(op: ArithOp, f: f64, g: f64) -> Option<Value> {
    Some(Value::Num(match op {
        ArithOp::Add => f + g,
        ArithOp::Sub => f - g,
        ArithOp::Mul => f * g,
        ArithOp::Div => f / g,
        ArithOp::Pow => {
            if g == 2.0 { f * f } else { f.powf(g) }
        }
        ArithOp::IDiv => (f / g).floor(),
        ArithOp::Mod => number::float_mod(f, g),
        _ => return None,
    }))
}

fn int_arith(op: ArithOp, i: i64, j: i64) -> Result<i64, LuaError> {
    Ok(match op {
        ArithOp::Add => i.wrapping_add(j),
        ArithOp::Sub => i.wrapping_sub(j),
        ArithOp::Mul => i.wrapping_mul(j),
        ArithOp::IDiv => {
            if j == 0 {
                return Err(LuaError::msg("attempt to divide by zero"));
            }
            number::int_floor_div(i, j)
        }
        ArithOp::Mod => {
            if j == 0 {
                return Err(LuaError::msg("attempt to perform 'n%0'"));
            }
            number::int_mod(i, j)
        }
        ArithOp::BAnd => i & j,
        ArithOp::BOr => i | j,
        ArithOp::BXor => i ^ j,
        ArithOp::Shl => number::shift_left(i, j),
        ArithOp::Shr => number::shift_left(i, j.wrapping_neg()),
        _ => 0,
    })
}

/// Arithmetic on two numbers.
pub(crate) fn arith_values(op: ArithOp, a: Value, b: Value) -> Result<Value, LuaError> {
    match (a, b) {
        (Value::Int(i), Value::Int(j)) if !matches!(op, ArithOp::Div | ArithOp::Pow) => Ok(Value::Int(int_arith(op, i, j)?)),
        _ => {
            let (f, g) = (a.as_num().unwrap_or(0.0), b.as_num().unwrap_or(0.0));
            if is_bitwise(op) {
                let (Some(i), Some(j)) = (number::float_to_int(f), number::float_to_int(g)) else {
                    return Err(LuaError::msg("number has no integer representation"));
                };
                return Ok(Value::Int(int_arith(op, i, j)?));
            }
            Ok(float_arith(op, f, g).unwrap_or(Value::Nil))
        }
    }
}

/// How a chunk name prints in messages: `=name` as is, `@file` as the file, source text as
/// `[string "first line..."]`.
pub(crate) fn chunk_id(source: &str) -> String {
    // C Lua's chunk name is a C string: it ends at the first zero byte (`load("\0")` is
    // `[string ""]`), S5C-COR-2.
    let source = source.split('\0').next().unwrap_or("");
    if let Some(rest) = source.strip_prefix('=') {
        return truncate_utf8(rest, 59).to_owned();
    }
    if let Some(rest) = source.strip_prefix('@') {
        if rest.len() <= 59 {
            return rest.to_owned();
        }
        let tail_start = rest.len() - 56;
        let mut s = tail_start;
        while !rest.is_char_boundary(s) {
            s += 1;
        }
        return format!("...{}", rest.get(s..).unwrap_or_default());
    }
    let first = source.split('\n').next().unwrap_or("");
    let multi = source.contains('\n');
    const MAX: usize = 60 - 15;
    // C's `luaO_chunkid` keeps the text whole only below the limit; at it, "..." is added.
    if !multi && first.len() < MAX {
        return format!("[string \"{first}\"]");
    }
    let cut = truncate_utf8(first, MAX);
    format!("[string \"{cut}...\"]")
}

/// `s` cut to at most `max` bytes on a character boundary.
/// `b` cut to at most `max` bytes: at a character boundary when it is UTF-8 (as
/// [`truncate_utf8`]), else at `max`.
pub(crate) fn cut_message(b: &[u8], max: usize) -> &[u8] {
    match crate::nostd::str::from_utf8(b) {
        Ok(s) => truncate_utf8(s, max).as_bytes(),
        Err(_) => b.get(..max.min(b.len())).unwrap_or_default(),
    }
}

pub(crate) fn truncate_utf8(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut cut = max;
    while !s.is_char_boundary(cut) {
        cut -= 1;
    }
    s.get(..cut).unwrap_or_default()
}

/// Makes closure `p` of prototype `parent` (running as `closure` with registers from
/// `base` of the thread `cur`, whose open upvalues are `open`): the one place both
/// interpreter paths make closures. Its memory (the function and any upvalues it opens)
/// is charged first; Err(Some(bytes)) when that does not fit (nothing is changed),
/// Err(None) for a missing prototype.
/// Fuel for writing a list of `n` values in one step (registers, results, arguments): free
/// up to 16, then a unit per 8, as the call protocol charges argument and result lists.
#[inline]
pub(crate) fn list_fuel(n: usize) -> u64 {
    if n > 16 { n as u64 / 8 } else { 0 }
}

/// Open upvalues moved (16 bytes each) per unit of fuel when a new one is put below them.
const OPEN_SHIFT_PER_FUEL: usize = 32;

pub(crate) fn new_closure(
    heap: &mut Heap,
    open: &mut Vec<(usize, u32)>,
    cur: ThreadRef,
    parent: u32,
    closure: FuncRef,
    base: usize,
    p: usize,
) -> Result<(Value, u64), Option<usize>> {
    let child = heap.protos.get(parent).and_then(|pp| pp.protos.get(p)).copied().ok_or(None)?;
    // The upvalue descriptions, copied out (making upvalues below needs the heap mutably):
    // into a buffer on the stack for the common few, without an allocation.
    let mut small = [(false, 0u32); 16];
    let big: Vec<(bool, u32)>;
    let descs: &[(bool, u32)] = match heap.protos.get(child) {
        Some(c) if c.upvals.len() <= small.len() => {
            for (s, d) in small.iter_mut().zip(&c.upvals) {
                *s = (d.in_stack, d.idx);
            }
            small.get(..c.upvals.len()).unwrap_or_default()
        }
        Some(c) => {
            big = c.upvals.iter().map(|d| (d.in_stack, d.idx)).collect();
            &big
        }
        None => &[],
    };
    let fresh = descs
        .iter()
        .filter(|(in_stack, idx)| *in_stack && open.binary_search_by(|(i, _)| i.cmp(&(base + *idx as usize))).is_err())
        .count();
    let size = crate::heap::FUNC_BASE + crate::heap::boxed(descs.len() * 8) + fresh * UPVAL_SIZE;
    // S5B-WALK-5: a closure's upvalues (up to 255, each found among the open ones or made)
    // were one unit with the instruction: building a 250-upvalue closure over and over ran
    // at 4x plain code per unit. A unit per 8 upvalues, and 2 more per 8 made, charged by
    // both paths from this count, so fuel stays the same either way.
    let mut extra = ((descs.len() + 2 * fresh) / 8) as u64;
    if !heap.mem.fits(size) {
        return Err(Some(size));
    }
    let _ = heap.mem.charge(size);
    let mut ups = Vec::with_capacity(descs.len());
    // Open upvalues moved up to make room for new ones (SEC-M6).
    let mut shifted = 0usize;
    for &(in_stack, idx) in descs {
        if in_stack {
            let at = base + idx as usize;
            match open.binary_search_by(|(i, _)| i.cmp(&at)) {
                Ok(k) => {
                    let r = open.get(k).map_or(0, |(_, r)| *r);
                    ups.push(FuncRef { idx: r, generation: heap.upvals.gens.get(r as usize).copied().unwrap_or(0) });
                }
                Err(k) => {
                    let r = heap.new_upval_charged(Upval::Open { thread: cur, idx: at });
                    shifted += open.len() - k;
                    open.insert(k, (at, r.idx));
                    ups.push(r);
                }
            }
        } else {
            // The enclosing closure's own upvalue (read in place: making upvalues does not
            // touch the function slab).
            let r = match heap.funcs.get(closure.idx) {
                Some(Func::Lua { upvals, .. }) => upvals.get(idx as usize).copied(),
                _ => None,
            };
            ups.push(r.unwrap_or(FuncRef { idx: u32::MAX, generation: 0 }));
        }
    }
    // SEC-M6: an upvalue made below others already open moves them all up (the list is kept
    // sorted by slot): a frame of 8,000 locals captured from the last down ran at 22x plain
    // code per unit of fuel, more with more locals. Paid by the entries moved (slots, not
    // the seed, decide them, so fuel stays the same on both paths and under any seed).
    extra += (shifted / OPEN_SHIFT_PER_FUEL) as u64;
    let f = heap.new_func_charged(Func::Lua { proto: child, upvals: ups.into_boxed_slice() });
    Ok((Value::Func(f), extra))
}


/// Empties a result list, giving back its room when it is long (`NATIVE_CHUNK` values or
/// more): only lists being placed are counted (SEC-M15), so a long one kept was outside
/// the account (SEC7-1).
pub(crate) fn clear_list(v: &mut Vec<Value>) {
    if v.capacity() > crate::stdlib::NATIVE_CHUNK {
        *v = Vec::new();
    } else {
        v.clear();
    }
}
