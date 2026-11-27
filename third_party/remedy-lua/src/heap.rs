//! The heap: typed slabs of objects addressed by index and generation, the string interner,
//! the memory account, and the incremental mark-and-sweep collector.
//!
//! Objects are never moved; a freed slot is reset to an empty object and its generation
//! bumps, so a stale reference reads an empty object (never another type, never memory that
//! is gone) and the host API can tell it is stale.
//!
//! The collector is incremental: marking and sweeping run in small steps between
//! instructions, paid for by allocation (each byte allocated buys a fixed amount of
//! collector work) and charged to the script's fuel. New objects made while marking are
//! gray; writes into black tables and upvalues turn them gray again (a backward barrier);
//! thread stacks are marked again in the atomic step that ends marking. Weak tables
//! (`__mode`) are cleared in the atomic step, with ephemeron semantics for weak keys.

#[allow(unused_imports)]
use crate::nostd::prelude::*;
use crate::nostd::any::Any;
use crate::nostd::sync::Arc;

use crate::bytecode::Proto;
use crate::table::{SLOT_SIZE, Table};
use crate::value::{FuncRef, StrRef, TableRef, ThreadRef, UdRef, Value};
use crate::vm::{Frame, NativeFn};

pub(crate) const WHITE: u8 = 0;
pub(crate) const GRAY: u8 = 1;
pub(crate) const BLACK: u8 = 2;
pub(crate) const FREE: u8 = 3;
/// Units of the collector's work between looks at the host's interrupt and the deadline,
/// within a step: about 0.2 ms of ephemeron marking alone, and some 1 ms on a loaded
/// machine (16,384 of them, 4 to 7 ms there: S6GC-2e-R).
const ATOMIC_CHECK: usize = 1 << 12;

/// Bytes of a slab's room given back at a time (`Slab::shrink_piece`): about half a
/// millisecond of giving memory back to the system.
const SHRINK_BYTES: usize = 8 << 20;

/// Bytes of a slot's generation (counted while `trim` keeps one for a cut slot).
const GEN_BYTES: usize = 4;
/// Distance from a slab's first generation past which `trim` keeps cut generations
/// instead of raising the floor over them.
const FLOOR_LIMIT: u32 = 1 << 31;

/// The memory account: bytes the script's objects hold, against the cap.
pub(crate) struct Mem {
    /// The collector's trace, when the host turned it on (`Lua::gc_trace`, SEC7-G1).
    pub trace: Option<Box<crate::gctrace::Trace>>,
    /// Bytes held: the objects' and their slabs' free slots (`slack` of them), what the
    /// cap limits.
    pub used: usize,
    pub cap: usize,
    /// Bytes allocated since the collector last did work (negative: credit).
    pub debt: isize,
    /// Set when an allocation was refused.
    pub refused: bool,
    /// Extra room the host gets while it builds values to pass in.
    pub headroom: usize,
    /// Test mode: every allocation runs a whole collection.
    pub stress: bool,
    /// Buffers of swept tables, reused by new tables.
    pub pool: Pool,
    /// Bytes of the slabs' free slots (`Slab::slot_bytes` each), from the sweep that
    /// frees a slot until an allocation reuses it or `trim` cuts it off. Resident, so
    /// counted in `used` (SEC-M5: one survivor at a slab's end kept every kind's slab at
    /// its peak, uncounted, about the cap per kind). The collector paces on `live` (used
    /// less this), so the fuel a program costs does not follow its free slots.
    pub slack: usize,
    /// The collection cycle (`Heap::cycle`), for `Table::gc_snap`.
    pub cycle: u32,
    /// Marking is under way in `cycle` (from its start to the end of the atomic step), and
    /// what reaching an object costs in it (`Heap::obj_work`): what a table grown past its
    /// size at the cycle's start pays for the marking of its growth (`Table::take_resized`).
    pub marking: bool,
    pub obj_work: usize,
    /// What the table being traversed in chunks (`Table::gc_watch`) did besides the writes
    /// themselves (`Heap::part`).
    pub watch: Watch,
    /// The running call's watch (the host's interrupt and deadline), for the long table
    /// operations to look at between pieces of their work (`stopped`); set at each entry
    /// from the host.
    pub deadline: Option<crate::stdlib::pattern::Watch>,
    /// The fuel the running call has left, set before a write that may rehash
    /// (`Table::rehash`): unlimited outside a call.
    pub fuel: usize,
    /// Under a fuel watch (a growth the call's fuel cannot pay for whole, SEC8-F4): the most
    /// `Table::resized` may reach before the growth stops for the call's fuel, a piece at a
    /// time, keeping what it made (`table::look_w`). `None`: no watch.
    pub fuel_watch: Option<usize>,
    /// Whether `fuel` is the call's own (the full path's write): only then may a growth it
    /// cannot pay for whole be made under a fuel watch. The fast loop's `fuel` is its slice:
    /// a growth past it is left to the full path, as before.
    pub fuel_watchable: bool,
    /// What resizes given up on tables made (`Table::partial` holds the index past one):
    /// their bytes are in `used` until they go.
    pub partials: Vec<Option<Box<crate::table::Partial>>>,
    /// Big buffers being freed a piece at a time between looks (`park`), oldest first:
    /// their bytes are in `used` (as slack) until their pages go.
    pub freeing: Vec<Parked>,
}

/// Changes to the table traversed in chunks while marking is incremental that move
/// entries, noted by the table for the collector (`Heap::part_written`, `Heap::part_step`).
pub(crate) struct Watch {
    /// Entries (key, value) an insertion moved to another node: from a node not traversed
    /// yet to one traversed already, nothing else would mark them.
    pub moved: Vec<Value>,
    /// The table's parts were rebuilt or resized (entries moved wholesale): where the
    /// traversal stands means nothing any more, but below `alen_floor`.
    pub restructured: bool,
    /// The least length its array part had while it was rebuilt: a rebuild moves no entry
    /// of the array below its old and new lengths, and puts every entry it moves past both
    /// (into the array's new slots, or the hash parts, which come after it).
    pub alen_floor: usize,
}

impl Default for Watch {
    fn default() -> Self {
        Watch { moved: Vec::new(), restructured: false, alen_floor: usize::MAX }
    }
}

/// Array and hash buffers of swept tables, kept to make new tables without the system
/// allocator. Invisible to the account (a buffer is reused only at the exact size it is
/// charged for); bounded per size, and emptied by every full collection, so the process
/// gives the memory back too. Its bytes count as used (as slack: resident, not live, so
/// the collector's pacing ignores them) and it gives them back before any charge or fit
/// would be refused, so a limit refuses exactly what it refused without it. It holds at
/// most `POOL_BYTES` or 1/16 of the limit, whichever is less.
#[derive(Default)]
pub(crate) struct Pool {
    /// By capacity.
    arrs: Vec<Vec<Vec<Value>>>,
    /// By length (a power of two).
    nodes: Vec<Vec<Vec<crate::table::Node>>>,
    /// Bytes of buffers held.
    pub(crate) bytes: usize,
}

/// A buffer this big is freed a piece at a time (`Mem::park`).
pub(crate) const PARK_BYTES: usize = 32 << 20;

/// What `Mem::unpark` frees between two looks.
const PARK_PIECE: usize = 512 << 10;

/// A buffer being freed a piece at a time (`Mem::park`).
pub(crate) enum Parked {
    Nodes(Vec<crate::table::Node>),
    Slots(Vec<Value>),
    Bytes(Vec<u8>),
}

impl Parked {
    fn bytes(&self) -> usize {
        match self {
            Parked::Nodes(v) => v.capacity() * size_of::<crate::table::Node>(),
            Parked::Slots(v) => v.capacity() * size_of::<Value>(),
            Parked::Bytes(v) => v.capacity(),
        }
    }

    /// A piece freed: whether none is left.
    fn shrink(&mut self) -> bool {
        fn piece<T>(v: &mut Vec<T>) -> bool {
            let keep = v.len().saturating_sub(PARK_PIECE / size_of::<T>().max(1));
            v.truncate(keep);
            v.shrink_to(keep);
            v.capacity() == 0
        }
        match self {
            Parked::Nodes(v) => piece(v),
            Parked::Slots(v) => piece(v),
            Parked::Bytes(v) => piece(v),
        }
    }
}

/// Largest array capacity and hash size kept, and how many buffers of one size.
pub(crate) const POOL_MAX: usize = 64;
const POOL_PER_SIZE: usize = 1 << 16;
/// Most bytes the pool holds: 16 MiB per state (a burst of dead tables reused, not freed
/// and allocated again: about 4% of binary_trees, 7% of gc_churn and table_churn).
pub(crate) const POOL_BYTES: usize = 16 << 20;

impl Pool {
    /// Buffers held (for tests).
    #[cfg(test)]
    pub fn held(&self) -> usize {
        self.arrs.iter().map(Vec::len).sum::<usize>() + self.nodes.iter().map(Vec::len).sum::<usize>()
    }

    /// An empty array buffer of capacity exactly `cap`.
    pub(crate) fn arr(&mut self, cap: usize) -> Vec<Value> {
        match self.arrs.get_mut(cap).and_then(Vec::pop) {
            Some(v) => {
                self.bytes -= cap * crate::nostd::mem::size_of::<Value>();
                v
            }
            None => Vec::with_capacity(cap),
        }
    }

    /// A hash part of `slots` empty nodes.
    pub(crate) fn nodes(&mut self, slots: usize) -> Vec<crate::table::Node> {
        match self.nodes.get_mut(slots).and_then(Vec::pop) {
            Some(mut v) => {
                self.bytes -= slots * crate::nostd::mem::size_of::<crate::table::Node>();
                v.resize(slots, crate::table::Node::default());
                v
            }
            None => vec![crate::table::Node::default(); slots],
        }
    }

    /// Keeps the buffers of a table that died, while the pool holds under `limit` bytes.
    pub(crate) fn give(&mut self, t: &mut Table, limit: usize) {
        let limit = limit.min(POOL_BYTES);
        let mut arr = t.arr.take_backing();
        let cap = arr.capacity();
        let size = cap * crate::nostd::mem::size_of::<Value>();
        if (1..=POOL_MAX).contains(&cap) && self.bytes + size <= limit {
            if self.arrs.len() <= cap {
                self.arrs.resize_with(cap + 1, Vec::new);
            }
            if let Some(class) = self.arrs.get_mut(cap)
                && class.len() < POOL_PER_SIZE
            {
                arr.clear();
                class.push(arr);
                self.bytes += size;
            }
        }
        let mut nodes = crate::nostd::mem::take(&mut t.nodes);
        let len = nodes.len();
        let size = len * crate::nostd::mem::size_of::<crate::table::Node>();
        if (1..=POOL_MAX).contains(&len) && nodes.capacity() == len && self.bytes + size <= limit {
            if self.nodes.len() <= len {
                self.nodes.resize_with(len + 1, Vec::new);
            }
            if let Some(class) = self.nodes.get_mut(len)
                && class.len() < POOL_PER_SIZE
            {
                nodes.clear();
                class.push(nodes);
                self.bytes += size;
            }
        }
    }
}

impl Mem {
    /// The limit that stops the running call now (its interrupt or deadline), if any: for
    /// a table resize to give up between pieces, the table left as it was.
    #[cold]
    pub(crate) fn stopped(&self) -> Option<crate::LimitKind> {
        self.deadline.as_ref().and_then(crate::stdlib::pattern::Watch::stop)
    }

    /// A buffer of `PARK_BYTES` or more freed a piece at a time between looks at the stop,
    /// in the account until its pages go: freeing one is the kernel's unmapping, about 5 ms
    /// per 100 MiB, with no look (LR-29b: a resize's old part of 2^21 nodes, 2 to 3 ms past
    /// its last look, 26 ms at 512 MiB). As much as goes before a stop is freed now; the
    /// rest at the next looks (`Lua::refuel`). No fuel (a free never was), and not live
    /// (slack): the collector's pacing is as when it was freed whole.
    pub(crate) fn park(&mut self, p: Parked) {
        self.slack_in(p.bytes());
        self.freeing.push(p);
        self.unpark();
    }

    /// The parked buffers freed `PARK_PIECE` bytes at a time (shrunk in place, their tail
    /// pages unmapped: no copy), oldest first, a look at the stop after each piece.
    pub(crate) fn unpark(&mut self) {
        while let Some(p) = self.freeing.first_mut() {
            let (before, gone) = (p.bytes(), p.shrink());
            let freed = before - p.bytes();
            if gone {
                self.freeing.remove(0);
            }
            self.slack_out(freed);
            if self.stopped().is_some() {
                return;
            }
        }
    }

    /// When an operation to trace starts (None: no trace).
    #[inline]
    pub(crate) fn trace_begin(&self) -> Option<crate::nostd::time::Duration> {
        self.trace.as_ref().map(|t| t.now())
    }

    /// Records an operation that started at `start` (`trace_begin`), in the step under way.
    #[cold]
    pub(crate) fn trace_note(&mut self, op: crate::gctrace::TraceOp, slots: u64, bytes: u64, start: Option<crate::nostd::time::Duration>) {
        if let (Some(t), Some(start)) = (self.trace.as_mut(), start) {
            let step = t.step;
            t.note(op, step, slots, bytes, 0, start);
        }
    }

    /// A new account under `cap`.
    pub(crate) fn new(cap: usize, debt: isize) -> Mem {
        Mem { used: 0, cap, debt, refused: false, headroom: 0, stress: false, pool: Pool::default(), slack: 0, cycle: 0, marking: false, obj_work: 0, watch: Watch::default(), deadline: None, fuel: usize::MAX, fuel_watch: None, fuel_watchable: false, partials: Vec::new(), freeing: Vec::new(), trace: None }
    }

    /// Bytes the live objects hold (what the collector paces on).
    #[inline]
    pub fn live(&self) -> usize {
        self.used.saturating_sub(self.slack)
    }

    /// Whether `n` more bytes fit under the cap (the pool given back first if they would
    /// not).
    #[inline]
    pub fn fits(&mut self, n: usize) -> bool {
        self.used.saturating_add(n) <= self.cap.saturating_add(self.headroom) || self.fits_drained(n)
    }

    #[cold]
    fn fits_drained(&mut self, n: usize) -> bool {
        self.drain_pool();
        self.used.saturating_add(n) <= self.cap.saturating_add(self.headroom)
    }

    /// The pool's buffers freed and its bytes out of the account.
    #[cold]
    pub(crate) fn drain_pool(&mut self) {
        self.slack_out(self.pool.bytes);
        self.pool = Pool::default();
    }

    /// `Pool::arr`, the buffer's bytes out of the slack.
    #[inline]
    pub(crate) fn pool_arr(&mut self, cap: usize) -> Vec<Value> {
        let held = self.pool.bytes;
        let v = self.pool.arr(cap);
        self.slack_out(held - self.pool.bytes);
        v
    }

    /// `Pool::nodes`, the buffer's bytes out of the slack.
    #[inline]
    pub(crate) fn pool_nodes(&mut self, slots: usize) -> Vec<crate::table::Node> {
        let held = self.pool.bytes;
        let v = self.pool.nodes(slots);
        self.slack_out(held - self.pool.bytes);
        v
    }

    /// `Pool::give`, the buffers kept counted as slack.
    pub(crate) fn pool_give(&mut self, t: &mut Table) {
        let held = self.pool.bytes;
        self.pool.give(t, self.cap / 16);
        self.slack_in(self.pool.bytes - held);
    }

    /// A slot of `n` bytes freed (into the slack) or reused or cut (out of it).
    #[inline]
    pub(crate) fn slack_in(&mut self, n: usize) {
        // (Bounded by the slabs' slots: no overflow.)
        self.slack += n;
        self.used += n;
    }

    #[inline]
    pub(crate) fn slack_out(&mut self, n: usize) {
        self.slack -= n;
        self.used = self.used.saturating_sub(n);
    }

    /// Accounts for `n` more bytes, or refuses when they would pass the cap.
    #[inline]
    pub fn charge(&mut self, n: usize) -> Result<(), ()> {
        let after = self.used.saturating_add(n);
        if after > self.cap.saturating_add(self.headroom) {
            return self.charge_drained(n);
        }
        self.used = after;
        self.debt = self.debt.saturating_add(isize::try_from(n).unwrap_or(isize::MAX));
        if self.stress {
            self.debt = self.debt.max(1);
        }
        Ok(())
    }

    /// `charge` past the cap: the pool given back, then the charge or the refusal.
    #[cold]
    #[inline(never)]
    fn charge_drained(&mut self, n: usize) -> Result<(), ()> {
        if self.pool.bytes == 0 || !self.fits_drained(n) {
            self.refused = true;
            return Err(());
        }
        self.charge(n)
    }

    /// Accounts for `n` fewer bytes.
    #[inline]
    pub fn release(&mut self, n: usize) {
        self.used = self.used.saturating_sub(n);
    }

    /// Settles the account of an object charged `charged` that now holds `actual`.
    pub fn settle(&mut self, charged: usize, actual: usize) {
        if actual > charged {
            self.used = self.used.saturating_add(actual - charged);
        } else {
            self.used = self.used.saturating_sub(charged - actual);
        }
    }
}

/// A string: its bytes, their hash and whether it is the interner's copy. Short strings
/// (at most [`SHORT_LEN`] bytes) are always interned, so two of them are equal exactly when
/// they are the same object. A long string is made without hashing or interning, as in C
/// Lua; it is hashed and interned only when used as a table key (tables hold interned keys
/// only), so equal long strings may be different objects: compare them by content.
#[derive(Default)]
pub(crate) struct LStr {
    pub bytes: Bytes,
    /// The seeded hash of the bytes; 0 until computed.
    pub(crate) hash: crate::nostd::cell::Cell<u64>,
    pub interned: bool,
    /// The interned copy last found for a long string that is not interned itself, so a
    /// key lookup compares its bytes once, not once per lookup (checked live before use).
    twin: crate::nostd::cell::Cell<Option<StrRef>>,
}

pub(crate) const SHORT_LEN: usize = 40;

/// Bytes a string keeps inside its object, without an allocation of their own.
const INLINE: usize = 22;

/// A string's bytes: short ones (most strings a script makes) inside the object, longer
/// ones in their own allocation.
pub(crate) enum Bytes {
    Inline { len: u8, buf: [u8; INLINE] },
    Heap(Box<[u8]>),
}

impl Default for Bytes {
    fn default() -> Self {
        Bytes::Inline { len: 0, buf: [0; INLINE] }
    }
}

impl crate::nostd::ops::Deref for Bytes {
    type Target = [u8];
    #[inline]
    fn deref(&self) -> &[u8] {
        match self {
            Bytes::Inline { len, buf } => buf.get(..usize::from(*len)).unwrap_or_default(),
            Bytes::Heap(b) => b,
        }
    }
}

impl From<&[u8]> for Bytes {
    fn from(b: &[u8]) -> Self {
        let mut buf = [0; INLINE];
        match buf.get_mut(..b.len()) {
            Some(dst) => {
                dst.copy_from_slice(b);
                Bytes::Inline { len: b.len() as u8, buf }
            }
            None => Bytes::Heap(b.into()),
        }
    }
}

impl From<Box<[u8]>> for Bytes {
    fn from(b: Box<[u8]>) -> Self {
        if b.len() <= INLINE { Bytes::from(&*b) } else { Bytes::Heap(b) }
    }
}
/// A string object's own memory: its slab entry (`LStr`, generation, color, occupancy) and
/// its interner slot, measured resident at about 64 bytes (sweep 6: the old 40 charged a
/// 7-byte string 47 bytes against about 70 resident).
pub(crate) const STR_BASE: usize = 64;

/// The memory a string of `len` bytes is charged (and released when swept): the object,
/// plus, past the bytes kept inline, the allocation's header and rounding (16 bytes).
#[inline]
pub(crate) const fn str_mem(len: usize) -> usize {
    STR_BASE + len + if len > INLINE { 16 } else { 0 }
}
pub(crate) const FUNC_BASE: usize = 48;

/// What a boxed slice of `bytes` bytes takes (SEC-M9: a closure's upvalue list was charged
/// its 8 bytes a reference, against the allocation's header and rounding: a closure over
/// one upvalue held 32 bytes for 8 charged). Empty boxes allocate nothing.
#[inline]
pub(crate) const fn boxed(bytes: usize) -> usize {
    if bytes == 0 { 0 } else { bytes.div_ceil(16) * 16 + ALLOC_OVERHEAD }
}
pub(crate) const UPVAL_SIZE: usize = 40;
/// A userdata's own record (LR-17: it was a flat 48 against about 90 bytes): its slab slot
/// (the record, its generation and color) and the box holding the host's value (the
/// allocation's header and least block; the value's own bytes are `size`).
pub(crate) const UD_BASE: usize = crate::nostd::mem::size_of::<UserData>() + 5 + 32;
/// What one allocation costs beyond its payload: the allocator's header and rounding.
pub(crate) const ALLOC_OVERHEAD: usize = 16;
/// A host handle's root (LR-17): its `Arc<()>` (counts and the allocation's header) and
/// its entry in `Lua::roots`.
pub(crate) const ROOT_BYTES: usize = 16 + ALLOC_OVERHEAD + crate::nostd::mem::size_of::<(crate::nostd::sync::Weak<()>, crate::value::Value)>();
/// A thread's own record: its state (boxed: the slab slot holds the pointer) and the box's
/// allocation, at least 256. It was a flat 256 while the state had grown to 272 bytes
/// (304 now): the charge follows the type.
pub(crate) const THREAD_BASE: usize = {
    let need = crate::nostd::mem::size_of::<ThreadState>() + crate::nostd::mem::size_of::<Box<ThreadState>>() + 16;
    if need > 256 { need } else { 256 }
};

/// A host function: reentrant, so it may be called again from script code it calls.
pub(crate) type HostFn = Arc<dyn Fn(&mut crate::vm::Vm, crate::vm::Args) -> Result<crate::vm::Flow, crate::vm::LuaError> + Send + Sync>;

/// A function object.
pub(crate) enum Func {
    Lua { proto: u32, upvals: Box<[crate::value::FuncRef]> },
    /// `in_place`: 1 + its index in `stdlib::string::IN_PLACE` for a library function a
    /// call from Lua code runs in place (`Lua::call_in_place`), else 0.
    Native { f: NativeFn, upvals: Box<[Value]>, name: &'static str, in_place: u8 },
    /// `bytes`: what it is charged beyond `FUNC_BASE` (its closure's and name's allocations,
    /// LR-17), given back when it is swept.
    Host { f: HostFn, name: Arc<str>, bytes: usize },
}

impl Default for Func {
    fn default() -> Self {
        Func::Native { f: crate::vm::native_dead, upvals: Box::new([]), name: "?", in_place: 0 }
    }
}

/// An upvalue: open (a live register of a thread) or closed (its own value). Upvalues are
/// kept in the function slab's sibling, `upvals`; a Lua closure lists them by index.
#[derive(Clone, Copy)]
pub(crate) enum Upval {
    Open { thread: ThreadRef, idx: usize },
    Closed(Value),
}

impl Default for Upval {
    fn default() -> Self {
        Upval::Closed(Value::Nil)
    }
}

/// A full userdata.
#[derive(Default)]
pub(crate) struct UserData {
    pub data: Option<Box<dyn Any + Send>>,
    pub meta: Option<TableRef>,
    pub size: usize,
    /// The host's key to report when this userdata is collected (opt-in).
    pub notify: Option<u64>,
}

/// A coroutine's status.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub(crate) enum CoStatus {
    /// Made, or yielded: can be resumed.
    #[default]
    Suspended,
    /// Running now.
    Running,
    /// Resumed another coroutine and waits for it.
    Normal,
    /// Finished or failed.
    Dead,
}

/// Where the results of a pending call go when a thread continues: the stack slot of the
/// called function and how many results the caller wants (-1: all).
#[derive(Clone, Copy, Debug)]
pub(crate) struct Pending {
    /// The resume (or wrap) call came from Lua code (for error positions).
    pub from_lua: bool,
    pub func: usize,
    pub nresults: i32,
    /// `coroutine.wrap`: errors propagate instead of returning `false, err`.
    pub wrap: bool,
    /// How the call's results are placed (a `pcall` of `resume` or a wrap function adds
    /// `true` and catches).
    pub kind: crate::vm::FrameKind,
}

/// A thread's state: its stack, frames and open upvalues.
#[derive(Default)]
pub(crate) struct ThreadState {
    pub stack: Vec<Value>,
    pub frames: Vec<Frame>,
    /// Open upvalues, sorted by stack index.
    pub open_upvals: Vec<(usize, u32)>,
    /// Stack slots of to-be-closed variables, in order.
    pub tbc: Vec<usize>,
    pub status: CoStatus,
    /// Who resumed it.
    pub resumer: Option<ThreadRef>,
    /// An upper bound of the C levels (C Lua's `nCcalls`) the threads that resumed it hold,
    /// set by each resume (`Lua::c_bound_here`): below C Lua's limit, a resume need not count
    /// them exactly. A resumer cannot run while this thread does, so it stays valid until
    /// the next resume.
    pub c_bound: usize,
    /// The `coroutine.yield` call waiting for the next resume's values.
    pub yield_pending: Option<Pending>,
    /// The `coroutine.resume` call waiting for this thread's next yield or end.
    pub resume_pending: Option<Pending>,
    /// Execution loops entered from Rust while this thread ran: it cannot yield past them.
    pub nny: u32,
    /// Not started: the body sits in stack[0].
    pub fresh: bool,
    /// The error that killed it.
    pub error: Option<Value>,
    /// Died by an error raised in a library function: that function's depth, stack slot
    /// and value (a level of its traceback, as in C Lua; LIBS-4).
    pub died_in: Option<(usize, usize, Value)>,
    /// Frames below this index were there when it was last resumed after a yield: C Lua
    /// "unrolls" their C levels then (a protected call's C function is gone from the C
    /// stack; its continuation runs from the resume), so they no longer count towards
    /// `Lua::c_levels` (S5B-OPEN-3). Lowered when a protected or continued frame is pushed
    /// below it (`push_frame`: only such frames count, and only the slow path makes them).
    pub unrolled: usize,
}

/// Bytes charged per call frame record as the frame list grows (S5GC-7): the record's
/// size when this was set. The record is smaller now (128 bytes); the charge stays, so
/// memory accounting, the collector's pacing and with them fuel are unchanged.
pub(crate) const FRAME_BYTES: usize = 136;
const _: () = assert!(crate::nostd::mem::size_of::<Frame>() <= FRAME_BYTES);

/// Gives back a frame list's room past twice what it holds (after deep recursion), as
/// the stack's is (S5GC-7).
pub(crate) fn shrink_frames(frames: &mut Vec<Frame>, mem: &mut Mem) {
    let cap = frames.capacity();
    let want = frames.len() * 2;
    if cap <= want * 2 {
        return;
    }
    frames.shrink_to(want);
    mem.release(cap.saturating_sub(frames.capacity()) * FRAME_BYTES);
}

impl ThreadState {
    pub fn mem_size(&self) -> usize {
        THREAD_BASE + self.stack.capacity() * SLOT_SIZE + self.frames.capacity() * FRAME_BYTES
    }
}

/// A per-slot vector of a slab that grows without copying all of itself in one push: from
/// 3/4 full it is copied to one of twice its capacity a stretch at a time (writes to what is
/// copied go to both), and that one takes its place when it is full. (Grown by `Vec`'s
/// doubling, the generations and free-list positions of a slab of 1M strings were copied
/// in the push that made the 1,048,577th, 2.4 ms each and 5 ms together, past the
/// host's interrupt; `items` grows in place, its buffer being the system's.)
pub(crate) struct Ahead<T> {
    cur: Vec<T>,
    /// The copy of `cur`'s first `next.len()` elements, with room for twice `cur`'s.
    next: Vec<T>,
    /// The length at which a push next copies a stretch, swaps, or lets `cur` grow by
    /// itself (`push_ahead`): one comparison for every other push.
    mark: usize,
}

/// Elements of a per-slot vector past which it grows ahead (`Ahead`), the pushes between
/// copied stretches, and a stretch: all of it is copied within `capacity / 16` pushes, so a
/// full vector's copy lacks at most the last `AHEAD_EVERY - 1` elements. (A stretch at
/// every push past 3/4 full cost string_build 4% more instructions; one every 64 behind a
/// single comparison, 0.0 to 0.25% across string_ops, string_build, json and gc_churn.)
const AHEAD_FROM: usize = 1 << 16;
const AHEAD_EVERY: usize = 64;
const AHEAD_STRETCH: usize = 16 * AHEAD_EVERY;

impl<T: Copy> Ahead<T> {
    const fn new() -> Self {
        Ahead { cur: Vec::new(), next: Vec::new(), mark: 0 }
    }

    #[inline]
    pub(crate) fn push(&mut self, v: T) {
        if self.cur.len() < self.mark {
            self.cur.push(v);
        } else {
            self.push_ahead(v);
        }
    }

    /// `push` past 3/4 full (every `AHEAD_EVERY`th): with the next stretch copied, or the
    /// copy taking `cur`'s place.
    #[inline(never)]
    fn push_ahead(&mut self, v: T) {
        let cap = self.cur.capacity();
        if self.cur.len() == cap && cap >= AHEAD_FROM {
            // Full: the copy takes its place (finished first if it is not, as after a
            // `shrink_to`).
            if self.next.capacity() <= cap {
                self.next = Vec::with_capacity(2 * cap);
            }
            let from = self.next.len();
            self.next.extend_from_slice(self.cur.get(from..).unwrap_or_default());
            self.cur = crate::nostd::mem::take(&mut self.next);
        }
        self.cur.push(v);
        let cap = self.cur.capacity();
        if self.cur.len() * 4 >= cap * 3 && cap >= AHEAD_FROM {
            if self.next.capacity() <= cap {
                self.next = Vec::with_capacity(2 * cap);
            }
            let from = self.next.len();
            let to = (from + AHEAD_STRETCH).min(self.cur.len());
            self.next.extend_from_slice(self.cur.get(from..to).unwrap_or_default());
        }
        let len = self.cur.len();
        self.mark = if cap < AHEAD_FROM {
            // Small: grows by itself, when full.
            cap
        } else if len * 4 < cap * 3 {
            (cap * 3).div_ceil(4)
        } else {
            (len + AHEAD_EVERY).min(cap)
        };
    }

    /// Sets element `i` (if there is one), in the copy too.
    pub(crate) fn set(&mut self, i: usize, v: T) {
        if let Some(x) = self.cur.get_mut(i) {
            *x = v;
        }
        if let Some(x) = self.next.get_mut(i) {
            *x = v;
        }
    }

    pub(crate) fn truncate(&mut self, n: usize) {
        self.cur.truncate(n);
        self.next.truncate(n);
        self.mark = self.mark.min(n);
    }

    pub(crate) fn capacity(&self) -> usize {
        self.cur.capacity()
    }

    /// Gives back room past `n` elements (and the copy, which starts again later).
    pub(crate) fn shrink_to(&mut self, n: usize) {
        self.next = Vec::new();
        self.cur.shrink_to(n);
        self.mark = 0;
    }
}

impl<T> crate::nostd::ops::Deref for Ahead<T> {
    type Target = [T];
    fn deref(&self) -> &[T] {
        &self.cur
    }
}

/// A slab's objects: one `Vec` (its growth is the system's `realloc`, which moves a big
/// one's pages without a copy), or on wasm, where `realloc` copies (19 ms for 32 MiB),
/// chunks that never move (`Chunked`; natively under the `segmented-arrays` test build).
#[cfg(not(any(target_family = "wasm", feature = "segmented-arrays")))]
type Items<T> = Vec<T>;
#[cfg(any(target_family = "wasm", feature = "segmented-arrays"))]
type Items<T> = Chunked<T>;

/// Objects in chunks of `CHUNK` that never move once full: the first grows as a `Vec`
/// does up to `CHUNK`, the rest are made at `CHUNK` and filled (SEC8-S1: a slab of 2M
/// tables doubled from 144 to 288 MiB in one `realloc`, a copy on wasm).
#[cfg(any(target_family = "wasm", feature = "segmented-arrays"))]
pub(crate) struct Chunked<T> {
    chunks: Vec<Vec<T>>,
    len: usize,
}

#[cfg(feature = "segmented-arrays")]
const CHUNK_SHIFT: u32 = 10;
#[cfg(all(target_family = "wasm", not(feature = "segmented-arrays")))]
const CHUNK_SHIFT: u32 = 16;
#[cfg(any(target_family = "wasm", feature = "segmented-arrays"))]
const CHUNK: usize = 1 << CHUNK_SHIFT;

#[cfg(any(target_family = "wasm", feature = "segmented-arrays"))]
crate::thread_local! {
    /// Objects a slab's storage moved on this thread (only its first chunk's growth can),
    /// for `Lua::slab_objects_moved`.
    static MOVED: crate::nostd::cell::Cell<u64> = const { crate::nostd::cell::Cell::new(0) };
}

/// The objects slabs' storage moved since the last call (a test probe).
#[cfg(any(target_family = "wasm", feature = "segmented-arrays"))]
pub(crate) fn take_moved() -> u64 {
    MOVED.with(crate::nostd::cell::Cell::take)
}

#[cfg(any(target_family = "wasm", feature = "segmented-arrays"))]
impl<T> Chunked<T> {
    const fn new() -> Self {
        Chunked { chunks: Vec::new(), len: 0 }
    }

    #[inline]
    fn len(&self) -> usize {
        self.len
    }

    fn capacity(&self) -> usize {
        match self.chunks.split_first() {
            Some((first, rest)) => first.capacity() + rest.len() * CHUNK,
            None => 0,
        }
    }

    #[inline]
    fn get(&self, i: usize) -> Option<&T> {
        self.chunks.get(i >> CHUNK_SHIFT)?.get(i & (CHUNK - 1))
    }

    #[inline]
    fn get_mut(&mut self, i: usize) -> Option<&mut T> {
        self.chunks.get_mut(i >> CHUNK_SHIFT)?.get_mut(i & (CHUNK - 1))
    }

    fn push(&mut self, v: T) {
        let k = self.len >> CHUNK_SHIFT;
        if k == self.chunks.len() {
            // (A new chunk made whole past the first: nothing in it moves again.)
            self.chunks.push(if k == 0 { Vec::new() } else { Vec::with_capacity(CHUNK) });
        }
        if let Some(c) = self.chunks.get_mut(k) {
            if c.len() == c.capacity() {
                MOVED.with(|m| m.set(m.get() + c.len() as u64));
            }
            c.push(v);
            self.len += 1;
        }
    }

    fn truncate(&mut self, n: usize) {
        if n >= self.len {
            return;
        }
        self.chunks.truncate(n.div_ceil(CHUNK).max(1));
        let base = (self.chunks.len() - 1) << CHUNK_SHIFT;
        if let Some(c) = self.chunks.last_mut() {
            c.truncate(n - base);
        }
        self.len = n;
    }

    /// Room past `n` slots given back: the chunks past them, and the first's own.
    fn shrink_to(&mut self, n: usize) {
        let keep = n.max(self.len).div_ceil(CHUNK).max(1);
        self.chunks.truncate(keep);
        if let [first] = &mut self.chunks[..] {
            first.shrink_to(n);
        }
    }

    fn swap(&mut self, a: usize, b: usize) {
        let (ka, kb) = (a >> CHUNK_SHIFT, b >> CHUNK_SHIFT);
        let (ia, ib) = (a & (CHUNK - 1), b & (CHUNK - 1));
        if ka == kb {
            if let Some(c) = self.chunks.get_mut(ka) {
                c.swap(ia, ib);
            }
            return;
        }
        let (lo, hi, il, ih) = if ka < kb { (ka, kb, ia, ib) } else { (kb, ka, ib, ia) };
        let (left, right) = self.chunks.split_at_mut(hi);
        if let (Some(x), Some(y)) = (left.get_mut(lo).and_then(|c| c.get_mut(il)), right.first_mut().and_then(|c| c.get_mut(ih))) {
            crate::nostd::mem::swap(x, y);
        }
    }
}

/// A slab of one object kind.
pub(crate) struct Slab<T> {
    items: Items<T>,
    pub gens: Ahead<u32>,
    pub color: Vec<u8>,
    pub free: Vec<u32>,
    /// The generation new slots start at: per state (so another state's handles do not
    /// match), raised by `trim` past every generation it cuts.
    floor: u32,
    /// The first `floor`: generations are compared by their distance from it.
    origin: u32,
    /// A bit per slot holding an object, and a bit per word of those that is not zero:
    /// the collector's walks skip free slots 4,096 at a time (`next_used`, S5GC-2).
    used: Vec<u64>,
    used_words: Vec<u64>,
    /// Each free slot's position in `free`, so `trim` takes the slots it cuts out of the
    /// free list one by one. As long as `items`; read only for free slots (an allocation
    /// leaves it stale).
    free_at: Ahead<u32>,
    /// Bytes a free slot holds, counted in `Mem::slack` (SEC-M5).
    pub(crate) slot_bytes: usize,
}


impl<T> Slab<T> {
    /// The object in slot `i` (a slot index, not a handle: no generation checked).
    #[inline]
    pub(crate) fn at(&self, i: usize) -> Option<&T> {
        self.items.get(i)
    }

    /// The object in slot `i`, to change in place.
    #[inline]
    pub(crate) fn at_mut(&mut self, i: usize) -> Option<&mut T> {
        self.items.get_mut(i)
    }

    /// The slots: the objects and the free slots below the last.
    #[inline]
    pub(crate) fn slots(&self) -> usize {
        self.items.len()
    }

    /// Slots there is room for before the storage grows.
    pub(crate) fn slot_capacity(&self) -> usize {
        self.items.capacity()
    }

    /// The objects in slots `a` and `b` swapped (the running thread's state and another's).
    pub(crate) fn swap_slots(&mut self, a: usize, b: usize) {
        self.items.swap(a, b);
    }
}

impl<T: Default> Slab<T> {
    /// Bytes a free slot holds: its item, generation and color, its free-list entry and
    /// position, and its bits in the bitmaps (rounded up to a byte).
    pub(crate) const SLOT_BYTES: usize = crate::nostd::mem::size_of::<T>() + 4 + 1 + 4 + 4 + 1;

    fn new(first_gen: u32) -> Self {
        Self::with_slot_extra(first_gen, 0)
    }

    /// A slab whose free slot holds `extra` bytes more than `SLOT_BYTES` (an empty item
    /// that allocates: a free thread slot holds a fresh `Box<ThreadState>`).
    fn with_slot_extra(first_gen: u32, extra: usize) -> Self {
        Slab {
            slot_bytes: Self::SLOT_BYTES + extra,
            free_at: Ahead::new(),
            items: Items::new(),
            gens: Ahead::new(),
            color: Vec::new(),
            free: Vec::new(),
            floor: first_gen,
            origin: first_gen,
            used: Vec::new(),
            used_words: Vec::new(),
        }
    }

    fn set_used(&mut self, i: usize) {
        let (w, b) = (i / 64, i % 64);
        if self.used.len() <= w {
            self.used.resize(w + 1, 0);
            self.used_words.resize(w / 64 + 1, 0);
        }
        if let Some(x) = self.used.get_mut(w) {
            *x |= 1 << b;
        }
        if let Some(x) = self.used_words.get_mut(w / 64) {
            *x |= 1 << (w % 64);
        }
    }

    fn clear_used(&mut self, i: usize) {
        let (w, b) = (i / 64, i % 64);
        if let Some(x) = self.used.get_mut(w) {
            *x &= !(1 << b);
            if *x == 0
                && let Some(y) = self.used_words.get_mut(w / 64)
            {
                *y &= !(1 << (w % 64));
            }
        }
    }

    /// The first slot at or after `from` that holds an object (`items.len()` if none).
    /// Free slots used to be walked one by one at no cost (S5GC-2): a slab left with one
    /// live object in 1,000 after a spike made every collection cycle walk all of it, 15x
    /// to 30x plain code per unit of fuel under a 256 MB limit and more under a larger one
    /// (charging the walk instead would make a program's fuel depend on what the state
    /// did before).
    pub(crate) fn next_used(&self, from: usize) -> usize {
        let end = self.slots();
        let mut w = from / 64;
        if let Some(&x) = self.used.get(w) {
            let m = x & (!0u64).checked_shl((from % 64) as u32).unwrap_or(0);
            if m != 0 {
                return (w * 64 + m.trailing_zeros() as usize).min(end);
            }
        }
        w += 1;
        loop {
            let Some(&s) = self.used_words.get(w / 64) else { return end };
            let m = s & (!0u64).checked_shl((w % 64) as u32).unwrap_or(0);
            if m != 0 {
                let w2 = (w / 64) * 64 + m.trailing_zeros() as usize;
                let x = self.used.get(w2).copied().unwrap_or(0);
                return (w2 * 64 + x.trailing_zeros() as usize).min(end);
            }
            w = (w / 64 + 1) * 64;
        }
    }

    /// One past the last slot that holds an object (0 if none). S5B-WALK-2: `trim` walked
    /// the free slots at the end one by one after every sweep, and gave nothing back while
    /// they were under a quarter of the slab: a slab of 2 million with its last live object
    /// at 80% walked 400,000 free slots a cycle for no work, 5x plain code per unit of fuel
    /// at a short pause and more under a larger memory limit. The bitmap skips them 4,096
    /// at a time, as `next_used` does.
    fn end_of_used(&self) -> usize {
        for (wi, &s) in self.used_words.iter().enumerate().rev() {
            if s != 0 {
                let w = wi * 64 + 63 - s.leading_zeros() as usize;
                let x = self.used.get(w).copied().unwrap_or(0);
                return (w * 64 + 64 - x.leading_zeros() as usize).min(self.slots());
            }
        }
        0
    }

    /// Gives back the free slots at the end (after a spike of objects died), so the
    /// process's memory returns to what is live instead of staying at its peak.
    ///
    /// After an incremental cycle only a tail of a quarter of the slab (or 1,024 slots)
    /// is cut, so a slab that shrinks and grows each cycle reuses its slots instead of
    /// giving them back and making them again. `all` (a full collection) cuts every free
    /// slot at the end, however few: they are counted against the cap while they stay
    /// (SEC-M5), so a full collection leaves the account where it was before the objects
    /// were made. The work is the slots cut, each of which an allocation paid for: each
    /// leaves the free list through its position in `free_at` (a `retain` walked the whole
    /// free list, the cut slots or not).
    ///
    /// `trim_piece` until nothing is left to cut: the whole cut in one go, with no look
    /// at a stop. The unit tests drive a slab this way; the collector cuts piece by
    /// piece through `Heap::trim_slabs` (SEC7-H3).
    #[cfg(test)]
    fn trim(&mut self, mem: &mut Mem, all: bool) {
        while self.trim_piece(mem, all, usize::MAX).1 {}
    }

    /// At most `most` slots off the end: the slots cut, and whether more are to be cut
    /// (`all` then, the cut decided). (S6GC: 2,000,000 slots cut in one go
    /// took 60 ms without a look at the deadline.)
    fn trim_piece(&mut self, mem: &mut Mem, all: bool, most: usize) -> (usize, bool) {
        let len = self.slots();
        let end = self.end_of_used();
        if end >= len {
            return if all { self.shrink_piece() } else { (0, false) };
        }
        if !all && len - end < (len / 4).max(1024) {
            return (0, false);
        }
        let keep = end.max(len.saturating_sub(most));
        for j in keep..len {
            let Some(&pos) = self.free_at.get(j) else { continue };
            let pos = pos as usize;
            if pos >= self.free.len() {
                continue;
            }
            self.free.swap_remove(pos);
            if let Some(&moved) = self.free.get(pos)
                && self.free_at.get(moved as usize).is_some()
            {
                self.free_at.set(moved as usize, pos as u32);
            }
        }
        // A slot made again past `keep` must not get back a generation a stale handle to
        // one of its objects holds (SEC4-10). The floor goes past every cut generation (by
        // distance from `origin`, so a state whose generations start near `u32::MAX` wraps
        // nothing), and the cut generations go. A generation moves at most once a cycle,
        // so the floor reaches `FLOOR_LIMIT` only after some 2^31 cycles that cut slots;
        // from there `gens` keeps the cut slots' generations instead (each made again
        // goes on from its own, as before SEC-M5), 4 bytes a slot, counted. (Raising the
        // floor saturating at `u32::MAX` once gave every slot made again the same
        // generation, so stale handles matched again.)
        let dist = |g: u32| g.wrapping_sub(self.origin);
        let most = self.gens.get(keep..len).unwrap_or_default().iter().map(|&g| dist(g)).max().unwrap_or(0);
        mem.slack_out((len - keep) * self.slot_bytes);
        if self.gens.len() == len && most < FLOOR_LIMIT {
            if most >= dist(self.floor) {
                self.floor = self.origin.wrapping_add(most + 1);
            }
            self.gens.truncate(keep);
            if self.gens.capacity() > keep * 2 {
                self.gens.shrink_to(keep + keep / 2);
            }
        } else {
            mem.slack_in((len - keep) * GEN_BYTES);
        }
        self.items.truncate(keep);
        self.color.truncate(keep);
        self.free_at.truncate(keep);
        // (The cut slots are free: their bits are clear already.)
        self.used.truncate(keep.div_ceil(64));
        self.used_words.truncate(self.used.len().div_ceil(64));
        if keep > end {
            return (len - keep, true);
        }
        let (w, more) = self.shrink_piece();
        (len - keep + w, more)
    }

    /// Gives back the slots' room past twice those used, down to 1.5 times, at most
    /// `SHRINK_BYTES` of it at a time: the slots' worth given back (a unit a page), and
    /// whether more is to be. (S6GC: the 2,097,152-slot room of a table slab, 300 MB, given back in
    /// one go took 15 to 22 ms.)
    fn shrink_piece(&mut self) -> (usize, bool) {
        let keep = self.slots();
        let cap = self.slot_capacity();
        if cap <= keep * 2 {
            if self.free.capacity() > self.free.len() * 2 + 64 {
                self.free.shrink_to_fit();
            }
            return (0, false);
        }
        let size = crate::nostd::mem::size_of::<T>().max(1);
        let to = (keep + keep / 2).max(cap - (SHRINK_BYTES / size).min(cap));
        self.free_at.shrink_to(to);
        self.items.shrink_to(to);
        self.color.shrink_to(to);
        ((cap - to) * size / 4096, true)
    }

    fn alloc(&mut self, item: T, mem: &mut Mem, color_for: impl Fn(usize) -> u8) -> (u32, u32) {
        if let Some((i, generation, slot)) = self.take_free(mem, &color_for) {
            *slot = item;
            return (i, generation);
        }
        let i = self.slots();
        self.set_used(i);
        self.items.push(item);
        self.free_at.push(0);
        let generation = match self.gens.get(i) {
            Some(&g) => {
                // A cut slot's kept generation: the object's now (no longer slack).
                mem.slack_out(GEN_BYTES);
                g
            }
            None => {
                self.gens.push(self.floor);
                self.floor
            }
        };
        self.color.push(color_for(i));
        (i as u32, generation)
    }

    /// A freed slot taken for a new object (it holds `T::default()`, for the caller to fill
    /// in place: a table built elsewhere and moved in was two copies and a drop), with its
    /// index and generation.
    fn take_free(&mut self, mem: &mut Mem, color_for: impl Fn(usize) -> u8) -> Option<(u32, u32, &mut T)> {
        let i = self.free.pop()?;
        let iu = i as usize;
        *self.color.get_mut(iu)? = color_for(iu);
        mem.slack_out(self.slot_bytes);
        self.set_used(iu);
        let generation = self.gens.get(iu).copied().unwrap_or(0);
        Some((i, generation, self.at_mut(iu)?))
    }

    #[inline]
    pub fn get(&self, idx: u32) -> Option<&T> {
        self.at(idx as usize)
    }

    #[inline]
    pub fn get_mut(&mut self, idx: u32) -> Option<&mut T> {
        self.at_mut(idx as usize)
    }


    pub fn live(&self, idx: u32, generation: u32) -> bool {
        self.gens.get(idx as usize) == Some(&generation)
            && self.color.get(idx as usize).is_some_and(|&c| c != FREE)
    }

    /// Frees slot `i`, which holds an object.
    fn free_slot(&mut self, i: usize, mem: &mut Mem) {
        mem.slack_in(self.slot_bytes);
        if let Some(slot) = self.at_mut(i) {
            *slot = T::default();
        }
        if let Some(&g) = self.gens.get(i) {
            self.gens.set(i, g.wrapping_add(1));
        }
        if let Some(c) = self.color.get_mut(i) {
            *c = FREE;
        }
        self.clear_used(i);
        let at = self.free.len() as u32;
        self.free_at.set(i, at);
        self.free.push(i as u32);
    }
}

/// The collector's phase.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Phase {
    Pause,
    Propagate,
    Sweep,
}

/// Objects made while marking is incremental (gray, as marked), and the work they owe.
///
/// COV-5: each was traversed as collector work, and how many are made while marking lasts
/// follows when it ends, which follows the order objects are reached in (the hash seed):
/// 259 closures and upvalues more under one seed than another, 1,570 units of fuel. Every
/// allocation pays its traversal as made instead, in any phase, and that traversal is not
/// counted.
#[derive(Default)]
pub(crate) struct Born {
    /// Trace register-owned table constructors from their owners under sustained pressure.
    pub trace_new_tables: bool,
    pub list: Vec<Gray>,
    /// The work of every object made, and how much of it was charged (`take_work`) and
    /// left to the collector's steps (`take_step`): a marking step leaves the latter of
    /// its budget to the other objects, as gray objects took their part before (their
    /// actual traversals follow when marking runs, so the seed; this, the allocations).
    total: usize,
    charged: usize,
    stepped: usize,
    /// `Heap::obj_work`, for what a traversal costs.
    pub obj_work: usize,
}

/// Born work charged as it comes, not only by collector steps (none run in a pause, or
/// with the collector stopped): a call would make objects its fuel does not pay for.
const BORN_DUE: usize = 1 << 12;

impl Born {
    /// Object `g` made, its traversal walking `walked` slots, upvalues or constants.
    #[inline]
    pub(crate) fn add(&mut self, phase: Phase, g: Gray, walked: usize) {
        let per = if matches!(g, Gray::Table(_)) { 1 + self.obj_work / 4 } else { 1 };
        self.total += self.obj_work + 1 + walked * per;
        if phase == Phase::Propagate {
            self.list.push(g);
        }
    }

    /// The work not charged yet.
    pub(crate) fn pending(&self) -> usize {
        self.total - self.charged
    }

    /// The work not charged yet, now charged.
    pub(crate) fn take_work(&mut self) -> usize {
        self.total - crate::nostd::mem::replace(&mut self.charged, self.total)
    }

    /// The work since the last collector step.
    pub(crate) fn take_step(&mut self) -> usize {
        self.total - crate::nostd::mem::replace(&mut self.stepped, self.total)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Gray {
    Proto(u32),
    Table(u32),
    /// The rest of a big table's traversal, from slot `.1` (`TABLE_CHUNK`, S6GC-2e).
    TablePart(u32, u32),
    Func(u32),
    Upval(u32),
    Ud(u32),
    Thread(u32),
}

/// Each key names a slab page. Keeping bits instead of individual work items
/// avoids copying or sifting the full frontier when a large table is scanned.
#[derive(Default)]
struct OrderedGray {
    pages: crate::nostd::collections::BTreeMap<Gray, Box<GrayPage>>,
    // Finish the page selected at a dequeue before selecting the next one.
    // The first discovery in a hash scan must never choose the active page.
    active: Option<(Gray, Box<GrayPage>)>,
}

struct GrayPage {
    words: [u64; 64],
    nonempty: u64,
}

impl Gray {
    fn index(self) -> u32 {
        match self {
            Self::Proto(i) | Self::Table(i) | Self::TablePart(i, _) | Self::Func(i)
            | Self::Upval(i) | Self::Ud(i) | Self::Thread(i) => i,
        }
    }

    fn at(self, i: u32) -> Self {
        match self {
            Self::Proto(_) => Self::Proto(i),
            Self::Table(_) => Self::Table(i),
            Self::TablePart(_, from) => Self::TablePart(i, from),
            Self::Func(_) => Self::Func(i),
            Self::Upval(_) => Self::Upval(i),
            Self::Ud(_) => Self::Ud(i),
            Self::Thread(_) => Self::Thread(i),
        }
    }
}

impl OrderedGray {
    fn push(&mut self, g: Gray) {
        let i = g.index();
        let base = g.at(i & !4095);
        let page = match self.active.as_mut() {
            Some((key, p)) if *key == base => p,
            _ => self.pages.entry(base).or_insert_with(|| Box::new(GrayPage { words: [0; 64], nonempty: 0 })),
        };
        let w = (i >> 6) & 63;
        if let Some(bits) = page.words.get_mut(w as usize) {
            *bits |= 1 << (i & 63);
            page.nonempty |= 1 << w;
        }
    }

    fn pop(&mut self) -> Option<Gray> {
        if self.active.is_none() {
            self.active = self.pages.pop_last();
        }
        let (base, page) = self.active.as_mut()?;
        let w = page.nonempty.trailing_zeros();
        let bits = page.words.get_mut(w as usize)?;
        let i = base.index() + w * 64 + bits.trailing_zeros();
        *bits &= *bits - 1;
        if *bits == 0 {
            page.nonempty &= !(1 << w);
        }
        let g = base.at(i);
        if page.nonempty == 0 {
            self.active = None;
        }
        Some(g)
    }

    fn is_empty(&self) -> bool {
        self.active.is_none() && self.pages.is_empty()
    }

    fn clear(&mut self) {
        self.pages.clear();
        self.active = None;
    }
}

/// The string interner: an open-addressing set of string slots keyed by content.
///
/// A regrow does not place every string again at once: it sets the slots aside (`old`)
/// and each new string moves a bounded number of them to the new slots, while lookups and
/// removals walk both (S6T-REGROW: a regrow placed every string in one insert, with no
/// look at the host's interrupt or deadline in a script or a host `load`: 15 to 17 ms at
/// 600,000 strings, 52 to 58 ms at 1.6 million, where making 2M strings went 51 to 82 ms
/// between looks at the deadline; now the longest insert is under a millisecond, giving
/// back the old slots, and the interner keeps nothing waiting).
pub(crate) struct Interner {
    /// Each slot: a string's index, inverted (the low 32 bits: `EMPTY` is 0, so empty
    /// slots are zeroes, written ahead as `spare` or zeroed by the system; `TOMB`), and
    /// the high 32 bits of its hash, so a walk compares without reading the strings.
    pub(crate) slots: Vec<u64>,
    /// The slots before the last regrow while their strings move to `slots` (empty when
    /// all have moved), in order: a moved one is left removed (`TOMB`), so walks go on past
    /// it. (In order, as a whole regrow placed them: their homes in `slots` follow theirs
    /// in `old`, and so the new slots fill a page at a time; moved from the end down, so
    /// the end could be cut off as it went, they took twice as long.)
    pub(crate) old: Vec<u64>,
    /// `old`'s salt, and the next of its slots to move.
    old_salt: u64,
    old_next: usize,
    /// Slots of `old` each new string moves: all of it moves within `slots.len() / 16`
    /// new strings, well before the next regrow (at least `slots.len() / 8` away). (Within
    /// `slots.len() / 64`, the moves crowded into the fuel slices of the move: 2.7 to 3.2 ms
    /// between looks at the deadline making 2M strings; 1.9 to 2.1 ms now. The fuel is the
    /// count's, `regrow_share`, either way.)
    moves: usize,
    /// The next regrow's slots, written a stretch at a time as the slots fill up from 5/8
    /// toward 3/4 (`write_spare`). Slots zeroed by the system are mapped a page at a
    /// time, but a page read before it is first written is mapped twice (once to read,
    /// again to write), and walks read the slots all over: making 2M strings took 16,000
    /// more page faults and 75 ms more than with slots written at once; written at once,
    /// 32 MB of slots kept the interrupt waiting 12 ms.
    spare: Vec<u64>,
    /// The strings in both.
    pub(crate) count: usize,
    /// The most there were since the last sweep ended, as the atomic step found them (the
    /// cycle's dead strings not yet swept: a cycle's most): slots are given back only when
    /// that is an eighth of them (a count that falls at each sweep and climbs again before
    /// the next, strings made and dropped, would give them back and grow them again every
    /// cycle: concat's 600,000 short strings, +2% instructions). (Not kept at each insert:
    /// +0.4% on string_build.)
    pub(crate) peak: usize,
    /// The bytes of the slots a shrink set aside, counted until they are given back.
    shrunk: usize,
    /// The removed slots in `slots`.
    tombs: usize,
    /// Mixed into each hash's home slot: a new one (and the slots placed again) when an
    /// insert walks past `walk_bound`, as a table's hash part takes a new seed (S5GC-3).
    salt: u64,
    /// The length of `find`'s last walk (in `slots`), for `insert`.
    walked: crate::nostd::cell::Cell<usize>,
    /// Strings inserted since the slots were last placed.
    since_salt: usize,
    /// The interned strings of `GROUP_FROM` bytes or more by whole hash and length, where
    /// two or more have one (`Crit`).
    groups: crate::nostd::collections::HashMap<(u64, usize), Crit>,
    /// Where the bytes of the last string not found (by whole hash and length) first
    /// differ from the member they could have equaled, and that member's byte there: for
    /// `insert` of that string just after, which so compares nothing again.
    last_diff: crate::nostd::cell::Cell<Option<(u64, usize, u32, usize, u8)>>,
}

/// S5GC-3: interner walks were free, and a script that knows the hash seed
/// (`Limits::hash_seed`, or recovered from `math.random`: S5GC-4) makes strings with any
/// hash it likes: the byte chunks of a string fold into its hash one after another, so a
/// chunk can cancel the one before and any number of strings share one hash. With 4,000
/// of them, each `..` that made one again walked them all (290x plain code per unit of
/// fuel), and interning them was quadratic (in `load` too). Now:
/// - strings aimed at one run by their low bits only spread again under a new salt when
///   an insert walks past `walk_bound` (charged as the rebuild it is);
/// - a walk is fuel for its steps past `walk_bound`, and for every step over a string
///   with the same hash bits (which random strings never meet, so neither depends on the
///   seed in practice);
///
/// Both are charged with the table probe steps (and, in a compile, as its scan work).
#[cold]
fn intern_excess(n: usize) {
    crate::table::add_summed(n as u64);
    crate::diag::compile_work(n);
}

/// The farthest an interner walk goes before it is fuel or makes a new salt: past what
/// random strings reach at the interner's 3/4 load (open addressing: some 270 slots at
/// 2^20).
#[inline]
fn walk_bound(len: usize) -> usize {
    64 + 16 * len.max(2).ilog2() as usize
}

/// The fuel each new string pays toward placing the interned strings again (`regrow`) in
/// an interner holding `count` strings. A regrow places every string at a random slot,
/// reading its hash from the string (30 to 40 ns a string at a million: cache misses), and
/// regrows come at most every `len / 4` new strings (a regrow leaves at most half the
/// slots used, the next comes when they are 3/4 used, and only a new string uses a slot),
/// so a new string costs at most two placements, beside its own walk's misses. Regrows
/// were free but for their excess walks: making strings that die beside two million live
/// ones ran at 2.7x plain code per unit of fuel with the fast loop and 3.2x without (2.2
/// to 2.7x beside a million, 3.2 to 3.9x beside three million elsewhere); 1.8x and 2.1x
/// with a share of 4 past a million, but 2.5 to 2.6x again with the rest of the tests
/// running beside it (the walks and placements miss caches the other processes share).
/// The share now grows with the interner's size, as a table's `place_cost` does: 0.8 to
/// 1.0x alone, 0.4 to 1.2x beside the whole suite (the regrow moving a stretch at a time
/// with the spare slots written ahead, `Interner`). Charging a regrow when it happens
/// would follow the seed (whether a new string takes a removed one's slot depends on the
/// layout, and with it when the slots fill up: one program's regrows came at 538,068 to
/// 538,793 strings across seeds), so each new string pays its share by the count, which
/// is the program's; so does the stretch it moves, by the same count.
#[inline]
fn regrow_share(count: usize) -> usize {
    match count {
        c if c >= 1 << 21 => 10,
        c if c >= 1 << 20 => 8,
        c if c >= 1 << 19 => 5,
        c if c >= 1 << 18 => 3,
        _ => 0,
    }
}

/// The fuel a walk of `steps`, `same` of them over strings with the same hash bits, costs.
#[inline]
fn walk_cost(steps: usize, same: usize, len: usize) -> usize {
    steps.saturating_sub(walk_bound(len)).max(same)
}

/// A slot's string index.
#[inline]
fn slot_idx(s: u64) -> u32 {
    !(s as u32)
}

/// The slot holding string `idx`, whose hash is `hash`.
#[inline]
fn slot_of(hash: u64, idx: u32) -> u64 {
    slot_bits(hash) | u64::from(!idx)
}

/// The bits of a hash a slot keeps.
#[inline]
fn slot_bits(hash: u64) -> u64 {
    hash & !0xffff_ffff
}

/// The slot a hash's walk starts at, in slots of `mask + 1` under `salt`.
#[inline]
fn home_in(hash: u64, salt: u64, mask: usize) -> usize {
    (fold_mul(hash ^ salt, 0x9E37_79B9_7F4A_7C15) as usize) & mask
}

/// Walks `slots` from `i` for the string with these bytes: its index, or the slot to put it
/// in (the first removed one on the way, else the empty one that ends the walk); with the
/// steps walked and how many were over strings with the same hash bits. For bytes of
/// `GROUP_FROM` or more it stops at the first string of the same whole hash and length
/// without comparing them: which of those it is, if any, is the group's to say (`Crit`).
#[inline(always)]
fn walk(
    slots: &[u64],
    mut i: usize,
    mask: usize,
    strings: &Slab<LStr>,
    hash: u64,
    bytes: &[u8],
) -> (Result<u32, usize>, usize, usize) {
    let bits = slot_bits(hash);
    let mut first_tomb = None;
    let (mut steps, mut same) = (0usize, 0usize);
    let r = loop {
        let s = slots.get(i).copied().unwrap_or(EMPTY);
        if s == EMPTY {
            break Err(first_tomb.unwrap_or(i));
        }
        if s == TOMB {
            first_tomb.get_or_insert(i);
        } else if slot_bits(s) == bits {
            if let Some(st) = strings.get(slot_idx(s))
                && st.hash.get() == hash
                && (if bytes.len() >= GROUP_FROM { st.bytes.len() == bytes.len() } else { *st.bytes == *bytes })
            {
                break Ok(slot_idx(s));
            }
            same += 1;
        }
        i = (i + 1) & mask;
        steps += 1;
    };
    (r, steps, same)
}

/// Walks `slots` from `i` to the slot a new string with this hash goes in (the first
/// removed one, else the empty one that ends the walk), past every string: the slot, the
/// steps and how many were over strings with the same hash bits.
fn walk_free(slots: &[u64], mut i: usize, mask: usize, hash: u64) -> (usize, usize, usize) {
    let mut first_tomb = None;
    let (mut steps, mut same) = (0usize, 0usize);
    loop {
        let s = slots.get(i).copied().unwrap_or(EMPTY);
        if s == EMPTY {
            return (first_tomb.unwrap_or(i), steps, same);
        }
        if s == TOMB {
            first_tomb.get_or_insert(i);
        } else if slot_bits(s) == slot_bits(hash) {
            same += 1;
        }
        i = (i + 1) & mask;
        steps += 1;
    }
}

/// Bytes from which interned strings of one whole hash and length are told apart by a
/// crit-bit tree (`Crit`) instead of by comparing each one's bytes in turn (S6T-EQ): under
/// a known seed, a program can make any number of strings with one hash and length, and
/// a lookup compared its bytes with each (any number of keys' worth of bytes, free). With
/// the tree a lookup compares them with one string at most. (Below, a comparison costs
/// about what a walk's step does, and each is charged as one: `walk_cost`.)
pub(crate) const GROUP_FROM: usize = 128;

/// Fuel for comparing or copying `n` bytes (`Lua::charge_bytes`, and the interner's
/// comparisons, SEC-M11).
#[inline]
pub(crate) fn bytes_fuel(n: usize) -> u64 {
    if n > 256 { n as u64 / 64 } else { 0 }
}

/// The interned strings of one whole hash and length (`GROUP_FROM` bytes or more), when
/// there are two or more: a crit-bit tree over their bytes, each node the first byte and
/// bit where its two sides differ. Looking a string up reads one byte a node and ends at
/// the one member it can equal. A program not aimed at the seed has none (a lone string
/// of its hash and length needs none: kept for each, they made 200-byte keys 21% slower).
enum Crit {
    Leaf(u32),
    Node { at: usize, bit: u8, kids: Box<[Crit; 2]> },
}

/// The side of a crit-bit node (byte `at`, bit `bit`) bytes `b` go to.
#[inline]
fn crit_side(b: &[u8], at: usize, bit: u8) -> bool {
    b.get(at).is_some_and(|&x| x & bit != 0)
}

/// The first byte where `a` and `b` differ (None: equal), comparing `from..to` only.
pub(crate) fn first_diff(a: &[u8], b: &[u8], from: usize, to: usize) -> Option<usize> {
    #[cfg(test)]
    COMPARED.with(|c| c.set(c.get() + to.saturating_sub(from)));
    // (Either out of range: they differ, at `from`.)
    let (Some(x), Some(y)) = (a.get(from..to), b.get(from..to)) else { return Some(from) };
    // (Slices compared whole, then by chunks to the byte: a byte at a time made looking
    // up 200-byte keys 23% slower.)
    if x == y {
        return None;
    }
    let mut off = from;
    for (p, q) in x.chunks(256).zip(y.chunks(256)) {
        if p != q {
            return p.iter().zip(q).position(|(m, n)| m != n).map(|k| off + k);
        }
        off += p.len();
    }
    Some(off)
}

#[cfg(test)]
crate::thread_local! {
    /// Bytes `first_diff` was asked to compare (for the tests).
    static COMPARED: crate::nostd::cell::Cell<usize> = const { crate::nostd::cell::Cell::new(0) };
}

/// Memory a crit-bit node takes (its two sides' box) and a group's map entry: charged as
/// they are made (sweep 6 review: under a collision attack, about 48 bytes per string
/// were outside the account).
const CRIT_NODE: usize = 2 * crate::nostd::mem::size_of::<Crit>() + 16;
const GROUP_ENTRY: usize = crate::nostd::mem::size_of::<((u64, usize), Crit)>() + 8;

impl Crit {
    /// The member bytes `b` can equal. Each node walked is a walk's step, charged: with the
    /// seed known a tree can be a chain as deep as its strings (S6R-5), and the slots'
    /// walk, which charged each string of the same hash bits, now stops at the first.
    fn best(&self, b: &[u8]) -> u32 {
        let mut n = self;
        let mut depth = 0;
        loop {
            match n {
                Crit::Leaf(i) => {
                    intern_excess(depth);
                    return *i;
                }
                Crit::Node { at, bit, kids } => {
                    depth += 1;
                    let [k0, k1] = &**kids;
                    n = if crit_side(b, *at, *bit) { k1 } else { k0 };
                }
            }
        }
    }

    /// Adds member `idx` (bytes `b`), which first differs from `best(b)` at byte `at`,
    /// where that member has byte `other`.
    fn insert(&mut self, idx: u32, b: &[u8], at: usize, other: u8) {
        let x = b.get(at).copied().unwrap_or(0) ^ other;
        let bit = 0x80u8 >> x.leading_zeros().min(7);
        let mut n = self;
        let mut depth = 0;
        loop {
            let side = match &*n {
                Crit::Node { at: a, bit: c, .. } if *a < at || (*a == at && *c > bit) => crit_side(b, *a, *c),
                _ => break,
            };
            depth += 1;
            let Crit::Node { kids, .. } = n else { break };
            let [k0, k1] = &mut **kids;
            n = if side { k1 } else { k0 };
        }
        intern_excess(depth);
        let old = crate::nostd::mem::replace(n, Crit::Leaf(u32::MAX));
        let kids = if crit_side(b, at, bit) { [old, Crit::Leaf(idx)] } else { [Crit::Leaf(idx), old] };
        *n = Crit::Node { at, bit, kids: Box::new(kids) };
    }

    /// Takes member `idx` (bytes `b`) out of a tree of two or more: the nodes walked, and
    /// whether it was there (a node went).
    fn remove(&mut self, idx: u32, b: &[u8]) -> (usize, bool) {
        let mut n = self;
        let mut depth = 0;
        loop {
            depth += 1;
            let (side, hit) = match &*n {
                Crit::Node { at, bit, kids } => {
                    let side = crit_side(b, *at, *bit);
                    let [k0, k1] = &**kids;
                    (side, matches!(if side { k1 } else { k0 }, Crit::Leaf(i) if *i == idx))
                }
                Crit::Leaf(_) => return (depth, false),
            };
            if hit {
                // The node goes, its other side in its place.
                if let Crit::Node { kids, .. } = n {
                    let [k0, k1] = &mut **kids;
                    *n = crate::nostd::mem::replace(if side { k0 } else { k1 }, Crit::Leaf(u32::MAX));
                }
                return (depth, true);
            }
            let Crit::Node { kids, .. } = n else { return (depth, false) };
            let [k0, k1] = &mut **kids;
            n = if side { k1 } else { k0 };
        }
    }
}

/// Freed a node at a time: a script that knows the seed can make its tree a chain as deep
/// as it has strings, and dropped a level per call it overflowed the native stack (S6R-5).
impl Drop for Crit {
    fn drop(&mut self) {
        let Crit::Node { kids, .. } = self else { return };
        let mut todo: Vec<Crit> = kids.iter_mut().map(|k| crate::nostd::mem::replace(k, Crit::Leaf(0))).collect();
        while let Some(mut n) = todo.pop() {
            if let Crit::Node { kids, .. } = &mut n {
                todo.extend(kids.iter_mut().map(|k| crate::nostd::mem::replace(k, Crit::Leaf(0))));
            }
            // (`n` goes here, its sides leaves now.)
        }
    }
}

/// Walks `slots` from `i` for string `idx` (hash `hash`) and marks its slot removed:
/// whether it was there, the steps walked and how many were over the same hash bits.
fn remove_in(slots: &mut [u64], mut i: usize, mask: usize, hash: u64, idx: u32) -> (bool, usize, usize) {
    let (mut steps, mut same) = (0, 0);
    loop {
        match slots.get(i).copied() {
            None | Some(EMPTY) => return (false, steps, same),
            Some(s) if s != TOMB && slot_idx(s) == idx => {
                if let Some(slot) = slots.get_mut(i) {
                    *slot = TOMB;
                }
                return (true, steps, same);
            }
            Some(s) => {
                if s != TOMB && slot_bits(s) == slot_bits(hash) {
                    same += 1;
                }
                i = (i + 1) & mask;
                steps += 1;
            }
        }
    }
}

impl Interner {
    fn new() -> Interner {
        Interner {
            slots: vec![EMPTY; 256],
            old: Vec::new(),
            old_salt: 0,
            old_next: 0,
            moves: 0,
            spare: Vec::new(),
            count: 0,
            peak: 0,
            shrunk: 0,
            tombs: 0,
            salt: 0,
            walked: crate::nostd::cell::Cell::new(0),
            since_salt: 0,
            groups: crate::nostd::collections::HashMap::<_, _, crate::nostd::collections::hash_map::RandomState>::default(),
            last_diff: crate::nostd::cell::Cell::new(None),
        }
    }

    /// The slot a hash's walk starts at.
    #[inline]
    fn home(&self, hash: u64) -> usize {
        home_in(hash, self.salt, self.slots.len() - 1)
    }

    /// Where a walk in `old` for this hash starts, while strings are left there.
    #[inline]
    fn old_home(&self, hash: u64) -> Option<usize> {
        (!self.old.is_empty()).then(|| home_in(hash, self.old_salt, self.old.len() - 1))
    }

    /// The interned string with these bytes (its index), or the slot to put it in.
    #[inline(always)]
    fn find(&self, strings: &Slab<LStr>, hash: u64, bytes: &[u8]) -> Result<u32, usize> {
        let len = self.slots.len();
        let (r, steps, same) = walk(&self.slots, self.home(hash), len - 1, strings, hash, bytes);
        self.walked.set(steps - same);
        let mut cost = walk_cost(steps, same, len);
        let r = match r {
            Err(slot) if !self.old.is_empty() => match self.old_home(hash) {
                Some(i) => {
                    let (o, steps, same) = walk(&self.old, i, self.old.len() - 1, strings, hash, bytes);
                    cost += walk_cost(steps, same, self.old.len());
                    o.map_err(|_| slot)
                }
                None => Err(slot),
            },
            r => r,
        };
        let r = match r {
            Ok(c) if bytes.len() >= GROUP_FROM => {
                // A string of the same whole hash and length: the one member these bytes can
                // equal, compared once.
                let best = if self.groups.is_empty() { c } else { self.group_best(strings, hash, bytes).unwrap_or(c) };
                let other = strings.get(best).map_or(&[][..], |st| &st.bytes[..]);
                let diff = first_diff(bytes, other, 0, bytes.len());
                // SEC-M11: the bytes compared are paid for (with the probe steps). A miss
                // records nothing, so a key that differs from an interned one of its whole
                // hash and length only at its end (made with a seed the script knows) was
                // compared to its end at every lookup, free. Only strings of one whole hash
                // and length are compared, so fuel follows no seed for a program not aimed
                // at one.
                cost += bytes_fuel(diff.unwrap_or(bytes.len())) as usize;
                match diff {
                    None => Ok(best),
                    Some(at) => {
                        let ob = other.get(at).copied().unwrap_or(0);
                        self.last_diff.set(Some((hash, bytes.len(), best, at, ob)));
                        let (slot, c) = self.free_slot(hash);
                        cost += c;
                        Err(slot)
                    }
                }
            }
            r => r,
        };
        if cost > 0 {
            intern_excess(cost);
        }
        r
    }

    /// The interned string `bytes` (`GROUP_FROM` or more, of this whole hash) can equal: the
    /// member of their group its bytes lead to, else the lone one of that hash and length.
    fn group_best(&self, strings: &Slab<LStr>, hash: u64, bytes: &[u8]) -> Option<u32> {
        if let Some(g) = self.groups.get(&(hash, bytes.len())) {
            return Some(g.best(bytes));
        }
        // (Uncharged: the lookup that follows walks the same slots, charged as before.)
        let (r, _, _) = walk(&self.slots, self.home(hash), self.slots.len() - 1, strings, hash, bytes);
        match r {
            Ok(c) => Some(c),
            Err(_) => {
                let i = self.old_home(hash)?;
                walk(&self.old, i, self.old.len() - 1, strings, hash, bytes).0.ok()
            }
        }
    }

    /// The slot a new string with this hash goes in, past the strings of its hash: the slot
    /// and the walk's cost.
    fn free_slot(&self, hash: u64) -> (usize, usize) {
        let len = self.slots.len();
        let (slot, steps, same) = walk_free(&self.slots, self.home(hash), len - 1, hash);
        (slot, walk_cost(steps, same, len))
    }

    /// The slot for a string of `GROUP_FROM` bytes or more known to be in no slots (its bytes
    /// compared with the one member they could equal: first differing at `at`, where that
    /// member has `other`), for `insert` just after.
    fn slot_for_absent(&self, hash: u64, len: usize, best: Option<(u32, usize, u8)>) -> usize {
        if let Some((m, at, ob)) = best {
            self.last_diff.set(Some((hash, len, m, at, ob)));
        }
        let (slot, cost) = self.free_slot(hash);
        if cost > 0 {
            intern_excess(cost);
        }
        slot
    }

    /// Puts string `idx` in `slot` (from `find`, just before): the bytes its group took.
    fn insert(&mut self, strings: &Slab<LStr>, slot: usize, idx: u32) -> usize {
        let mut held = 0;
        let hash = strings.get(idx).map_or(0, |s| s.hash.get());
        if let Some(st) = strings.get(idx)
            && st.bytes.len() >= GROUP_FROM
        {
            let b = &st.bytes[..];
            let hint = self.last_diff.take();
            match if self.groups.is_empty() { None } else { self.groups.get_mut(&(hash, b.len())) } {
                None => {
                    // A second of this hash and length (the lookup just before compared it
                    // with the first): their group.
                    if let Some((h, l, m, at, ob)) = hint
                        && h == hash
                        && l == b.len()
                    {
                        let mut g = Crit::Leaf(m);
                        g.insert(idx, b, at, ob);
                        self.groups.insert((hash, b.len()), g);
                        held = GROUP_ENTRY + CRIT_NODE;
                    }
                }
                Some(g) => {
                    let best = g.best(b);
                    // Where these bytes first differ from the member they could equal: from
                    // the lookup just before (none compares them again), else compared now.
                    let diff = match hint {
                        Some((h, l, m, at, ob)) if h == hash && l == b.len() && m == best => Some((at, ob)),
                        _ => strings.get(best).and_then(|o| first_diff(b, &o.bytes, 0, b.len()).map(|at| (at, o.bytes.get(at).copied().unwrap_or(0)))),
                    };
                    if let Some((at, ob)) = diff {
                        g.insert(idx, b, at, ob);
                        held = CRIT_NODE;
                    }
                }
            }
        }
        if let Some(s) = self.slots.get_mut(slot) {
            if *s == TOMB {
                self.tombs -= 1;
            }
            *s = slot_of(hash, idx);
        }
        self.count += 1;
        let share = regrow_share(self.count);
        if share > 0 {
            intern_excess(share);
        }
        if !self.old.is_empty() {
            self.move_some(strings, self.moves);
        }
        let used = self.count + self.tombs;
        if used * 8 > self.slots.len() * 5 {
            self.write_spare(used);
        }
        if used * 4 > self.slots.len() * 3 {
            self.regrow(strings, ((self.count + 1) * 2).next_power_of_two().max(256), self.salt);
        } else if self.walked.get() > walk_bound(self.slots.len()) && self.since_salt > self.slots.len() / 8 {
            // Strings aimed at one run (by the low bits of their hashes): a new salt, from
            // this one so runs replay, and every string placed again, charged as the work
            // it is. At most once per `len / 8` strings: strings with one whole hash stay
            // together under any salt, and walks past the bound are charged anyway.
            intern_excess(self.slots.len() / 16);
            self.regrow(strings, self.slots.len(), mix_seed(self.salt, 0x5a17));
        }
        self.since_salt += 1;
        held
    }

    /// Takes string `idx` (hash `hash`) out: the work its walk costs past a plain one, and
    /// the bytes its group gave back.
    fn remove(&mut self, strings: &Slab<LStr>, hash: u64, idx: u32) -> (usize, usize) {
        let mut crit = 0;
        let mut freed = 0;
        if !self.groups.is_empty()
            && let Some(st) = strings.get(idx)
            && st.bytes.len() >= GROUP_FROM
            && let Some(g) = self.groups.get_mut(&(hash, st.bytes.len()))
        {
            let (walked, went) = g.remove(idx, &st.bytes);
            crit = walked;
            freed = if went { CRIT_NODE } else { 0 };
            // One left: alone of its hash and length again.
            if matches!(g, Crit::Leaf(_)) {
                self.groups.remove(&(hash, st.bytes.len()));
                freed += GROUP_ENTRY;
            }
        }
        let len = self.slots.len();
        let home = self.home(hash);
        let (found, steps, same) = remove_in(&mut self.slots, home, len - 1, hash, idx);
        let mut cost = walk_cost(steps, same, len) + crit;
        if found {
            self.count -= 1;
            self.tombs += 1;
        } else if let Some(i) = self.old_home(hash) {
            let mask = self.old.len() - 1;
            let (found, steps, same) = remove_in(&mut self.old, i, mask, hash, idx);
            cost += walk_cost(steps, same, mask + 1);
            if found {
                self.count -= 1;
            }
        }
        (cost, freed)
    }

    /// Sets the slots aside for new ones of `size` under `salt`: their strings move a few
    /// at a time (`move_some`).
    fn regrow(&mut self, strings: &Slab<LStr>, size: usize, salt: u64) {
        if !self.old.is_empty() {
            // (Not reached: the slots set aside before are moved well before this.)
            self.move_some(strings, usize::MAX);
        }
        let new = if self.spare.len() >= size {
            let mut v = crate::nostd::mem::take(&mut self.spare);
            if v.len() > size {
                // (Fewer strings than the spare was written for: its end goes back.)
                v.truncate(size);
                v.shrink_to_fit();
            }
            v
        } else {
            vec![EMPTY; size]
        };
        let old = crate::nostd::mem::replace(&mut self.slots, new);
        self.moves = (16 * old.len()).div_ceil(size) + 1;
        self.old_salt = self.salt;
        self.old = old;
        self.old_next = 0;
        self.salt = salt;
        self.tombs = 0;
        self.since_salt = 0;
    }

    /// Writes the next stretch of the spare slots, `used` of the slots being used: as many
    /// as the next regrow can want (the most strings there can be by then, when every new
    /// string until then is kept), all of them within `slots.len() / 8` new strings, before
    /// the slots are 3/4 used.
    fn write_spare(&mut self, used: usize) {
        let want = self.spare_want(used);
        if self.spare.len() >= want {
            return;
        }
        if self.spare.capacity() < want {
            self.spare = Vec::with_capacity(want);
        }
        let n = (self.spare.len() + 32).min(want);
        self.spare.resize(n, EMPTY);
    }

    /// The most slots the next regrow can want, `used` of the slots being used.
    fn spare_want(&self, used: usize) -> usize {
        let len = self.slots.len();
        let most = self.count + (len * 3 / 4 + 1).saturating_sub(used);
        ((most + 1) * 2).next_power_of_two().clamp(256, 2 * len)
    }

    /// Moves up to `n` of `old`'s slots to `slots` (`n` units of work).
    fn move_some(&mut self, strings: &Slab<LStr>, n: usize) {
        let mask = self.slots.len() - 1;
        let mut excess = 0usize;
        let end = self.old_next.saturating_add(n).min(self.old.len());
        while self.old_next < end {
            let Some(x) = self.old.get_mut(self.old_next) else { break };
            self.old_next += 1;
            let s = *x;
            if s == EMPTY || s == TOMB {
                continue;
            }
            // (Empty ones stay empty, so walks there end where they did.)
            *x = TOMB;
            let h = strings.get(slot_idx(s)).map_or(0, |x| x.hash.get());
            let mut i = self.home(h);
            let (mut steps, mut same) = (0usize, 0usize);
            while let Some(&x) = self.slots.get(i)
                && x != EMPTY
                && x != TOMB
            {
                if slot_bits(x) == slot_bits(s) {
                    same += 1;
                }
                i = (i + 1) & mask;
                steps += 1;
            }
            excess += walk_cost(steps, same, mask + 1);
            if let Some(slot) = self.slots.get_mut(i) {
                if *slot == TOMB {
                    self.tombs -= 1;
                }
                *slot = s;
            }
        }
        if self.old_next >= self.old.len() {
            // All moved: kept to be written again as the spare when the next regrow can
            // use it (strings dying as fast as new ones come regrow at one size), else
            // given back. (Given back and taken again at every regrow, gc_churn's large
            // frees and the allocator's merging of small blocks at each nearly doubled.)
            let mut v = crate::nostd::mem::take(&mut self.old);
            // (Not after a shrink: the next regrow wants at most twice the slots.)
            if v.capacity() >= self.spare_want(self.count + self.tombs) && v.capacity() > self.spare.capacity() && v.capacity() <= 2 * self.slots.len() {
                v.clear();
                self.spare = v;
            }
        }
        if excess > 0 {
            intern_excess(excess);
        }
    }
}

/// An empty interner slot, and one whose string was removed (by their index part).
const EMPTY: u64 = 0;
const TOMB: u64 = 1;

#[cfg(test)]
mod interner_tests {
    use super::{EMPTY, Heap, TOMB, slot_idx, tail_word};

    /// A hash's tail word is the bytes' little-endian word for every length (as the loop
    /// it replaced made it): the same hashes, so the same layouts and fuel.
    #[test]
    fn tail_words_are_the_bytes_little_endian() {
        let bytes: Vec<u8> = (0u8..12).map(|i| i.wrapping_mul(37) ^ 0xa5).collect();
        for n in 0..=bytes.len() {
            let t = bytes.get(..n).unwrap_or_default();
            let want = t.iter().take(8).enumerate().fold(0u64, |w, (i, &x)| w | u64::from(x) << (8 * i));
            assert_eq!(tail_word(t), want, "{n} bytes");
        }
    }

    /// S6T-EQ (security): strings of one whole hash and length, made with the seed known
    /// (each's last two words cancel: the second undoes what the first did to the hash),
    /// any number of them interned. Interning one, looking one up (found or not) and
    /// taking one out compares at most one string's worth of bytes (a lookup compared
    /// them with each in turn), and every lookup finds just the one it should.
    #[test]
    fn strings_of_one_hash_compare_one_string_at_most() {
        let mut h = Heap::new(1 << 30, 0x5eed);
        let len = 4096usize;
        let seed = h.seed;
        let fill = vec![b'x'; len - 16];
        let hp = super::hash_words(seed, super::hash_start(seed, len), &fill);
        let (k1, k2) = (seed ^ 0xa076_1d64_78bd_642f, seed.rotate_left(32) ^ 0xe703_7ed1_a0b4_28db);
        let make = |i: u64| {
            let w1 = 0x4141_4141_0000_0000 | i;
            let h1 = super::fold_mul(hp ^ w1 ^ k1, k2);
            let w2 = h1 ^ 0x1234_5678_9abc_def0;
            let mut b = fill.clone();
            b.extend_from_slice(&w1.to_le_bytes());
            b.extend_from_slice(&w2.to_le_bytes());
            b
        };
        let all: Vec<Vec<u8>> = (0..65).map(make).collect();
        let hash = super::hash_bytes(seed, &all[0]);
        assert!(all.iter().all(|b| super::hash_bytes(seed, b) == hash && b.len() == len), "one hash");
        let compared = || super::COMPARED.with(crate::nostd::cell::Cell::get);
        let mut refs = Vec::new();
        for b in &all[..64] {
            let before = compared();
            refs.push(h.intern(b).unwrap());
            assert!(compared() - before <= len, "interning compared {} bytes", compared() - before);
        }
        for (i, b) in all[..64].iter().enumerate() {
            let before = compared();
            let hb = super::hash_bytes(seed, b);
            assert_eq!(h.interner.find(&h.strings, hb, b), Ok(refs[i].idx), "string {i}");
            assert!(compared() - before <= len, "a lookup compared {} bytes", compared() - before);
        }
        let before = compared();
        assert!(h.interner.find(&h.strings, hash, &all[64]).is_err(), "the one not interned");
        assert!(compared() - before <= len, "a lookup compared {} bytes", compared() - before);
        // Some taken out (nothing compared), the rest still found, those gone not.
        let before = compared();
        for r in refs.iter().step_by(3) {
            h.unintern(r.idx);
        }
        assert_eq!(compared(), before, "taking out compares nothing");
        for (i, b) in all[..64].iter().enumerate() {
            let found = h.interner.find(&h.strings, hash, b);
            if i % 3 == 0 {
                assert!(found.is_err(), "string {i} taken out");
            } else {
                assert_eq!(found, Ok(refs[i].idx), "string {i}");
            }
        }
    }

    /// S6T-REGROW: while a regrow's slots move, every string is found in whichever slots
    /// hold it, a string taken out of the old slots is gone (and counted out once), each
    /// new string moves a bounded stretch, and the move ends well before the next regrow.
    #[test]
    fn strings_move_a_stretch_at_a_time_and_stay_found() {
        let mut h = Heap::new(1 << 30, 0x5eed);
        let base = h.interner.count;
        let name = |i: usize| format!("s{i}").into_bytes();
        let mut refs = Vec::new();
        let (mut moves_seen, mut removed) = (0usize, Vec::new());
        for i in 0..200_000 {
            let was = h.interner.old.len();
            refs.push(h.intern(&name(i)).unwrap());
            let i_old = &h.interner;
            if !i_old.old.is_empty() {
                moves_seen += 1;
                // Bounded: at most `moves` old slots per new string.
                assert!(
                    i_old.old_next <= i_old.old.len()
                        && i_old.moves <= 16 * i_old.old.len() / i_old.slots.len() + 1
                );
                if was == 0 {
                    assert_eq!(i_old.old.len() * 2, i_old.slots.len(), "a regrow at {i} doubled");
                }
                // Takes out a string still in the old slots, now and then.
                if i % 97 == 0
                    && let Some(&s) =
                        i_old.old.iter().skip(i_old.old_next).find(|&&s| s != EMPTY && s != TOMB)
                {
                    let before = h.interner.count;
                    h.unintern(slot_idx(s));
                    assert_eq!(h.interner.count, before - 1);
                    removed.push(slot_idx(s));
                }
            }
        }
        assert!(moves_seen > 100, "moves seen in {moves_seen} inserts");
        assert!(!removed.is_empty());
        for (i, r) in refs.iter().enumerate() {
            let hash = h.strings.get(r.idx).unwrap().hash.get();
            let found = h.interner.find(&h.strings, hash, &name(i));
            if removed.contains(&r.idx) {
                assert!(found.is_err(), "string {i} found after its removal");
            } else {
                assert_eq!(found, Ok(r.idx), "string {i}");
            }
        }
        assert_eq!(h.interner.count, base + refs.len() - removed.len());
        let live =
            h.interner.slots.iter().chain(&h.interner.old).filter(|&&s| s != EMPTY && s != TOMB).count();
        assert_eq!(live, h.interner.count, "the count is the strings in the slots");
    }
}

/// The objects and the collector.
pub(crate) struct Heap {
    pub strings: Slab<LStr>,
    pub tables: Slab<Table>,
    pub funcs: Slab<Func>,
    pub upvals: Slab<Upval>,
    pub uds: Slab<UserData>,
    pub threads: Slab<Box<ThreadState>>,
    /// Function prototypes (code, constants, nested prototypes).
    pub protos: Slab<Proto>,
    pub(crate) interner: Interner,
    pub mem: Mem,
    pub(crate) seed: u64,
    /// The seed of table key hashing (see `value::hash_key`), given to each table made.
    pub(crate) table_seed: u64,
    /// Keys of host-tracked userdata collected since the host last drained them.
    pub collected: Vec<u64>,
    pub phase: Phase,
    gray: Vec<Gray>,
    /// New objects discovered during a chunked table scan (including intervening
    /// writes) or ephemeron marking. Only their set is stable across hash layouts.
    /// Drain by kind and slab index once the active scan and the stack are done.
    ordered_gray: Box<OrderedGray>,
    pub born: Born,
    pub grayagain: Vec<Gray>,
    /// Weak tables met while marking: (table, weak keys, weak values).
    weak: Vec<(u32, bool, bool)>,
    /// Sweep position: (slab number, index).
    pub(crate) sweep: (u8, usize),
    /// Cycle number, for prototypes' marks.
    pub cycle: u32,
    /// Bytes live after the last cycle.
    pub estimate: usize,
    /// No collection may run (the compiler holds unrooted constants).
    pub blocked: u32,
    /// `collectgarbage("stop")`: no automatic steps.
    pub stopped: bool,
    /// `setpause`: the next cycle starts when the heap reaches this percentage of what
    /// was live after the last one.
    pub pause: usize,
    /// `setstepmul`: collector work per byte allocated, in percent.
    pub stepmul: usize,
    /// Interned `__mode` key for weak-table checks.
    pub mode_key: Option<StrRef>,
    /// While the atomic step settles ephemerons: values waiting for their (unmarked) key,
    /// by key object, so marking a key marks its values at once (linear, not quadratic).
    /// Ephemeron values waiting for their (unmarked) key: the head of each key's list in
    /// `eph_links` (value, next; `u32::MAX` ends a list). Flat, so clearing it after a
    /// cycle frees nothing per entry (a map of a `Vec` per key took 30 ms to drop for
    /// 600k keys, past the interrupt).
    eph_waiting: crate::nostd::collections::HashMap<(u8, u32), u32, EphHash>,
    eph_links: Vec<(Value, u32)>,
    eph_active: bool,
    /// Where a suspended atomic phase stands (see `atomic`).
    atomic_stage: Option<AtomicStage>,
    /// Work per object reached in this cycle (`obj_work`, set when it starts).
    obj_work: usize,
    /// Buffers a traversal collects into, kept so marking allocates nothing.
    trav_vals: Vec<Value>,
    trav_ups: Vec<u32>,
    /// The table traversed `TABLE_CHUNK` slots a step while marking is incremental
    /// (`NO_PART`: none; see `part_step`), its next slot, its weak mode as it was when its
    /// traversal started, and whether that traversal started over once already.
    pub(crate) part: u32,
    /// Big tables reached while another was traversed in chunks, waiting their turn.
    part_queue: Vec<u32>,
    part_at: usize,
    /// Slots of its snapshot (`Table::slots_at`) its chunks have paid for.
    part_paid: usize,
    part_mode: (bool, bool),
    /// The most slots of a table one traversal scanned in one go (a whole table, or one
    /// chunk of one) since the host last read it (`Lua::gc_most_slots_at_once`).
    pub(crate) most_at_once: usize,
    /// Slots the atomic phase's traversals have scanned (`traverse_table`): its looks at
    /// the interrupt and the deadline go by them as well as by its work.
    pub(crate) scanned: usize,
    /// The most slots it scanned between two looks since the host last read it
    /// (`Lua::gc_most_slots_between_looks`).
    pub(crate) most_between_looks: usize,
    /// The slots a host poll may scan before a marking step that may pause does
    /// (`pause_ok`, set by the machine where the call can be resumed as it stands), and
    /// `scanned` when the poll started. None: steps run to their end. A paused step's loop
    /// state, and that it paused (SEC7-G1).
    pub(crate) poll_allow: Option<usize>,
    pub(crate) poll_start: usize,
    /// Objects traversed (and the clearing's work) since the poll started: a poll is
    /// bounded by them with its slots (`poll_over`).
    pub(crate) poll_objs: usize,
    /// The table slots a marking step scans at most (`STEP_SLOTS` for the steps of an
    /// incremental cycle; none for a full collection): a step's work leaves out the slots a
    /// table grew by while marking (their marking was paid as it grew), so a step bounded by
    /// its work alone scanned a weak array's 2^21 slots in one go, 10 ms (SEC7-G1).
    pub(crate) step_slots: usize,
    /// The atomic step paused at the end of the host's poll: its counters (work, its next
    /// looks, where its scan count started, its last look), so it goes on as it stood.
    pub(crate) atomic_saved: Option<[usize; 5]>,
    pub(crate) pause_ok: bool,
    pub(crate) paused: bool,
    prop_saved: Option<(usize, Prop)>,
    /// What a marking step's prepaid slots (`part_step`) took past its budget, taken from
    /// the next step's: they pace the steps as work does, so a call scans no more of them
    /// than its steps' budgets cover (SEC7-G1, the plain call's pause).
    pace_carry: usize,
    /// Test probe (`Lua::gc_pace_probe`): the grown slots scanned, and the marking steps'
    /// budgets (work units), since the state was made.
    pub(crate) probe_prepaid: u64,
    pub(crate) probe_budget: u64,
}

/// Where a marking step's loop stands (`Heap::propagate_from`).
#[derive(Clone, Copy, Debug)]
struct Prop {
    done: usize,
    next: usize,
    redone: usize,
    slots: usize,
    /// The work units the slots it scanned past what it pays for stand for (`pre_paid`).
    pre: usize,
    /// The step's budget as given, and the pacing carried into it (`pace_carry`).
    given: usize,
    carry: usize,
}

/// The work units a slot grown in the cycle spends of a marking step's budget when a
/// chunked traversal scans it (uncharged: its marking was paid as the table grew). What
/// scanning one costs, against what a unit of fuel costs, measured (SEC7-G1, the plain
/// call's pause; release build, 5dc4ba3, `s6gc_2e_a_big_table_is_marked_a_chunk_a_step`'s
/// workload, thread CPU, 3 runs a loop): 14.2 to 16.3 ns a grown slot (the 16 steps over the
/// 1,048,766 slots T grew by) against 27.4 to 35.8 ns a fuel unit (the median call's CPU
/// over its 200,000 units), so 1.82 to 2.30 units a slot (`WORK_PER_FUEL` 4). Paced at a
/// charged slot's rate instead (6 units there), the cycle took longer, more garbage floated
/// and the tables slab doubled once more (302 to 604 MB) within the growth report.
pub(crate) const GROWN_SLOT_WORK: usize = 2;

/// `Heap::part` when no table is traversed in chunks.
pub(crate) const NO_PART: u32 = u32::MAX;

/// Whether marking an entry (key `k`, value `v`) of the table traversed in chunks, of weak
/// mode (`wk`, `wv`), marks its key and its value. Decided by the entry alone, never by the
/// part it sits in, so the same wherever a write or a rebuild put it: a weak part is not
/// marked (strings always are), and in an ephemeron table (weak keys only) a value is
/// marked when its key is a string or no object, as the atomic step would mark it (whose
/// keys are other objects is left to that step: their marks then follow the order).
fn entry_marks(k: Value, v: Value, (wk, wv): (bool, bool)) -> (bool, bool) {
    let str_k = matches!(k, Value::Str(_));
    let mk = !wk || str_k;
    let mv = matches!(v, Value::Str(_)) || (!wv && (!wk || str_k || !k.is_collectable()));
    (mk, mv)
}

/// Collector work units per byte allocated.
/// Bytes allocated between steps.
pub(crate) const STEP_SIZE: isize = 8 * 1024;

/// The 64-bit halves of `a * b`, folded (xor): a multiply whose every output bit depends on
/// every input bit.
#[inline]
fn fold_mul(a: u64, b: u64) -> u64 {
    let r = u128::from(a) * u128::from(b);
    (r as u64) ^ ((r >> 64) as u64)
}

/// A string's hash for the interner, keyed by the heap's seed: each 8 bytes go through a
/// folded multiply by a seed-derived factor. (The FxHash-style `(rotl(h, 5) ^ w) * K` it
/// replaces had collisions that hold for every seed: flipping the top bit of one word and
/// bit 4 of the next leaves the hash unchanged, so a script could make any number of
/// colliding strings without knowing the seed.)
#[inline]
fn hash_bytes(seed: u64, b: &[u8]) -> u64 {
    let whole = b.len() & !7;
    let h = hash_words(seed, hash_start(seed, b.len()), b.get(..whole).unwrap_or_default());
    hash_finish(seed, h, b.get(whole..).unwrap_or_default())
}

/// `hash_bytes` in parts (for a long string hashed a piece at a time, `Heap::hash_piece`):
/// the state before any bytes of a string of `len`,
#[inline]
fn hash_start(seed: u64, len: usize) -> u64 {
    seed ^ (len as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15)
}

/// the state after `words` (whole 8-byte words: the bytes before them so too),
#[inline]
fn hash_words(seed: u64, mut h: u64, words: &[u8]) -> u64 {
    let (k1, k2) = (seed ^ 0xa076_1d64_78bd_642f, seed.rotate_left(32) ^ 0xe703_7ed1_a0b4_28db);
    for c in words.chunks_exact(8) {
        let mut w = [0u8; 8];
        w.copy_from_slice(c);
        h = fold_mul(h ^ u64::from_le_bytes(w) ^ k1, k2);
    }
    h
}

/// Up to 8 bytes as a little-endian word (zero above them). (Shifted in a byte at a time
/// in a loop, the tail of a short string's hash cost more than its words; copied into a
/// word, it became a `memcpy` call.)
#[inline]
fn tail_word(t: &[u8]) -> u64 {
    let w = |b: &[u8]| b.iter().rev().fold(0u64, |w, &x| w << 8 | u64::from(x));
    match *t {
        [] => 0,
        [a] => u64::from(a),
        [a, b] => w(&[a, b]),
        [a, b, c] => w(&[a, b, c]),
        [a, b, c, d] => w(&[a, b, c, d]),
        [a, b, c, d, e] => w(&[a, b, c, d, e]),
        [a, b, c, d, e, f] => w(&[a, b, c, d, e, f]),
        [a, b, c, d, e, f, g] => w(&[a, b, c, d, e, f, g]),
        [a, b, c, d, e, f, g, h, ..] => u64::from_le_bytes([a, b, c, d, e, f, g, h]),
    }
}

/// and the hash, from the state and the last (under 8) bytes.
#[inline]
fn hash_finish(seed: u64, h: u64, tail: &[u8]) -> u64 {
    let (k1, k2) = (seed ^ 0xa076_1d64_78bd_642f, seed.rotate_left(32) ^ 0xe703_7ed1_a0b4_28db);
    let h = fold_mul(h ^ tail_word(tail) ^ k1, k2);
    // Never 0, which a string's `hash` keeps for "not computed yet": a script that knows
    // the seed can make a string hash to 0, which was then hashed again at every use (and
    // forever by `settle_long_key`, S6R-7).
    fold_mul(h ^ k2, k1 | 1).max(1)
}


impl Heap {
    pub fn new(cap: usize, seed: u64) -> Heap {
        // Each state's generations start elsewhere, so a handle from another state almost
        // never passes as one of this state's objects (`is_live`). Never script-visible.
        static STATES: crate::nostd::sync::atomic::AtomicU32 = crate::nostd::sync::atomic::AtomicU32::new(1);
        let g = STATES.fetch_add(1, crate::nostd::sync::atomic::Ordering::Relaxed).wrapping_mul(0x9E37_79B9);
        Heap {
            strings: Slab::new(g),
            tables: Slab::new(g),
            funcs: Slab::new(g),
            upvals: Slab::new(g),
            uds: Slab::new(g),
            threads: Slab::with_slot_extra(g, THREAD_BASE),
            protos: Slab::new(g),
            interner: Interner::new(),
            mem: Mem::new(cap, -STEP_SIZE),
            seed,
            table_seed: seed.rotate_left(29) ^ 0x9E37_79B9_7F4A_7C15,
            collected: Vec::new(),
            phase: Phase::Pause,
            gray: Vec::new(),
            ordered_gray: Default::default(),
            born: Born::default(),
            grayagain: Vec::new(),
            weak: Vec::new(),
            sweep: (0, 0),
            cycle: 1,
            estimate: 0,
            blocked: 0,
            mode_key: None,
            eph_waiting: crate::nostd::collections::HashMap::with_hasher(EphHash(mix_seed(seed, 0xe9))),
            eph_links: Vec::new(),
            eph_active: false,
            atomic_stage: None,
            obj_work: 2,
            trav_vals: Vec::new(),
            trav_ups: Vec::new(),
            part: NO_PART,
            part_queue: Vec::new(),
            part_at: 0,
            part_paid: 0,
            part_mode: (false, false),
            most_at_once: 0,
            scanned: 0,
            most_between_looks: 0,
            poll_allow: None,
            poll_start: 0,
            poll_objs: 0,
            step_slots: usize::MAX,
            atomic_saved: None,
            pause_ok: false,
            paused: false,
            prop_saved: None,
            pace_carry: 0,
            probe_prepaid: 0,
            probe_budget: 0,
            stopped: false,
            pause: 200,
            stepmul: 100,
        }
    }

    /// The color a new object in slab `slab` at `idx` starts with: white between cycles,
    /// gray while marking (it is traversed before marking ends), and while sweeping black
    /// where the sweep has not reached yet (so it survives) or white where it has.
    fn new_color(phase: Phase, sweep: (u8, usize), slab: u8, idx: usize) -> u8 {
        match phase {
            Phase::Pause => WHITE,
            Phase::Propagate => GRAY,
            Phase::Sweep => {
                if slab < sweep.0 || (slab == sweep.0 && idx < sweep.1) {
                    WHITE
                } else {
                    BLACK
                }
            }
        }
    }

    // ---- strings ----

    /// The interned string with these bytes, made if new (of any length). Charges memory.
    pub fn intern(&mut self, bytes: &[u8]) -> Result<StrRef, ()> {
        self.intern_hashed(bytes, None)
    }

    /// `intern` of bytes whose hash may be known already (`hash_piece`).
    #[inline]
    pub fn intern_hashed(&mut self, bytes: &[u8], hash: Option<u64>) -> Result<StrRef, ()> {
        let (seed, phase, sweep) = (self.seed, self.phase, self.sweep);
        Self::intern_in(&mut self.strings, &mut self.interner, &mut self.mem, (seed, phase, sweep), bytes, hash)
    }

    /// `intern` on the parts of the heap it uses (the fast loop holds the others borrowed):
    /// the string slab, the interner, the memory account and (seed, phase, sweep).
    pub(crate) fn intern_in(
        strings: &mut Slab<LStr>,
        interner: &mut Interner,
        mem: &mut Mem,
        (seed, phase, sweep): (u64, Phase, (u8, usize)),
        bytes: &[u8],
        hash: Option<u64>,
    ) -> Result<StrRef, ()> {
        let hash = hash.unwrap_or_else(|| hash_bytes(seed, bytes));
        let slot = match interner.find(strings, hash, bytes) {
            Ok(s) => return Ok(Self::keep_in(strings, phase, sweep, s)),
            Err(slot) => slot,
        };
        mem.charge(str_mem(bytes.len()))?;
        let lstr = LStr { bytes: bytes.into(), hash: crate::nostd::cell::Cell::new(hash), interned: true, twin: crate::nostd::cell::Cell::new(None) };
        let r = Self::alloc_str_in(strings, mem, phase, sweep, lstr);
        mem.used = mem.used.saturating_add(interner.insert(strings, slot, r.idx));
        Ok(r)
    }

    /// A string: interned when short, else a new object (not hashed). Charges memory.
    #[inline]
    pub fn new_str(&mut self, bytes: &[u8]) -> Result<StrRef, ()> {
        if bytes.len() <= SHORT_LEN {
            return self.intern(bytes);
        }
        self.mem.charge(str_mem(bytes.len()))?;
        Ok(self.new_long_charged(bytes.into()))
    }

    /// A long string whose memory (`str_mem(len)`) is charged already: its bytes are
    /// moved in, not copied.
    pub fn new_long_charged(&mut self, bytes: Box<[u8]>) -> StrRef {
        self.alloc_str(LStr { bytes: bytes.into(), hash: crate::nostd::cell::Cell::new(0), interned: false, twin: crate::nostd::cell::Cell::new(None) })
    }

    fn alloc_str(&mut self, s: LStr) -> StrRef {
        Self::alloc_str_in(&mut self.strings, &mut self.mem, self.phase, self.sweep, s)
    }

    fn alloc_str_in(strings: &mut Slab<LStr>, mem: &mut Mem, phase: Phase, sweep: (u8, usize), s: LStr) -> StrRef {
        let (idx, generation) = strings.alloc(s, mem, |i| Self::new_color(phase, sweep, 0, i));
        // Strings have nothing to traverse: gray is black for them.
        if let Some(c) = strings.color.get_mut(idx as usize)
            && *c == GRAY
        {
            *c = BLACK;
        }
        StrRef { idx, generation }
    }

    /// The interned string of `bytes` (short), kept as `intern` keeps a string it finds,
    /// or None when there is none (nothing made; the walk's steps are counted as
    /// `intern`'s are).
    pub(crate) fn find_short(&mut self, bytes: &[u8]) -> Option<StrRef> {
        let s = self.interner.find(&self.strings, hash_bytes(self.seed, bytes), bytes).ok()?;
        Some(Self::keep_in(&mut self.strings, self.phase, self.sweep, s))
    }

    /// The interned string of `bytes` (any length), found without making it, keeping it
    /// or leaving anything a later interner call reads (`walked`, `last_diff`): for a host
    /// read that does not enter the state (SEC8-F3). Table keys are interned, so a key
    /// with these bytes is this string or there is none.
    pub(crate) fn find_interned_quiet(&self, bytes: &[u8]) -> Option<StrRef> {
        let (walked, diff) = (self.interner.walked.get(), self.interner.last_diff.get());
        let r = self.interner.find(&self.strings, hash_bytes(self.seed, bytes), bytes).ok();
        self.interner.walked.set(walked);
        self.interner.last_diff.set(diff);
        r.map(|s| StrRef { idx: s, generation: self.strings.gens.get(s as usize).copied().unwrap_or(0) })
    }

    fn find_interned(&self, hash: u64, bytes: &[u8]) -> Result<u32, usize> {
        self.interner.find(&self.strings, hash, bytes)
    }

    /// A reference to interned string `s`, found again: while the collector runs, it must
    /// not be swept now.
    pub(crate) fn keep(&mut self, s: u32) -> StrRef {
        Self::keep_in(&mut self.strings, self.phase, self.sweep, s)
    }

    /// `keep` on the string slab alone (for the fast loop, which holds other parts of the
    /// heap borrowed).
    pub(crate) fn keep_in(strings: &mut Slab<LStr>, phase: Phase, sweep: (u8, usize), s: u32) -> StrRef {
        let generation = strings.gens.get(s as usize).copied().unwrap_or(0);
        if phase == Phase::Sweep
            && let Some(c) = strings.color.get_mut(s as usize)
            && *c == WHITE
            && (sweep.0 == 0 && (s as usize) >= sweep.1)
        {
            *c = BLACK;
        }
        if phase == Phase::Propagate
            && let Some(c) = strings.color.get_mut(s as usize)
            && *c == WHITE
        {
            *c = BLACK;
        }
        StrRef { idx: s, generation }
    }

    fn insert_interned(&mut self, slot: usize, idx: u32) {
        let held = self.interner.insert(&self.strings, slot, idx);
        self.mem.used = self.mem.used.saturating_add(held);
    }

    /// A string's hash, computed on first use (at most once per string, so its cost is
    /// covered by what making the string cost).
    fn str_hash(&self, s: &LStr) -> u64 {
        let h = s.hash.get();
        if h != 0 {
            return h;
        }
        let h = hash_bytes(self.seed, &s.bytes);
        s.hash.set(h);
        h
    }

    /// Whether looking `r` up as a key could hash or compare a long run of bytes in one go: a
    /// long string, not interned, whose interned copy is not known (`Lua::settle_key`).
    #[inline]
    pub fn key_needs_settling(&self, r: StrRef) -> bool {
        self.strings.get(r.idx).is_some_and(|s| !s.interned && s.bytes.len() > crate::stdlib::COPY_PIECE) && self.known_canon(r).is_none()
    }

    /// Hashes the next `COPY_PIECE` bytes of `b` from `run` (the state and the bytes done,
    /// None at the start): the hash (as `hash_bytes` makes it) once all are done, for a
    /// long string hashed between looks at the host's interrupt and deadline.
    pub fn hash_piece(&self, b: &[u8], run: &mut Option<(u64, usize)>) -> Option<u64> {
        let (h, at) = run.unwrap_or((hash_start(self.seed, b.len()), 0));
        let whole = b.len() & !7;
        let end = at.saturating_add(crate::stdlib::COPY_PIECE).min(whole);
        let h = hash_words(self.seed, h, b.get(at..end).unwrap_or_default());
        if end < whole {
            *run = Some((h, end));
            return None;
        }
        Some(hash_finish(self.seed, h, b.get(whole..).unwrap_or_default()))
    }

    /// The one interned string that string `r` (its hash known: `hash_piece`) can equal:
    /// the member of its group (`Crit`) its bytes lead to.
    pub fn key_best(&self, r: StrRef) -> Option<u32> {
        let s = self.strings.get(r.idx)?;
        self.interner.group_best(&self.strings, s.hash.get(), &s.bytes)
    }

    /// The first byte in `from..to` where strings `a` and `b` differ (None: none there).
    pub fn diff_piece(&self, a: u32, b: u32, from: usize, to: usize) -> Option<usize> {
        let (Some(x), Some(y)) = (self.strings.get(a), self.strings.get(b)) else { return Some(from) };
        first_diff(&x.bytes, &y.bytes, from, to)
    }

    /// Interns string `r` (`GROUP_FROM` bytes or more, its hash known) that was compared with
    /// the one interned string it could equal and is not it (`best`: that string and the
    /// first byte where they differ; None: there was none), comparing nothing again.
    pub fn canon_absent(&mut self, r: StrRef, best: Option<(u32, usize)>) -> StrRef {
        let Some(s) = self.strings.get(r.idx) else { return r };
        let (hash, len) = (s.hash.get(), s.bytes.len());
        let best = best.map(|(m, at)| (m, at, self.strings.get(m).and_then(|o| o.bytes.get(at).copied()).unwrap_or(0)));
        let slot = self.interner.slot_for_absent(hash, len, best);
        if let Some(s) = self.strings.get_mut(r.idx) {
            s.interned = true;
        }
        self.insert_interned(slot, r.idx);
        r
    }

    /// Notes interned string `idx` as the copy of `r` (its bytes compared equal).
    pub fn set_twin(&self, r: StrRef, idx: u32) {
        if let Some(s) = self.strings.get(r.idx) {
            s.twin.set(Some(StrRef { idx, generation: self.strings.gens.get(idx as usize).copied().unwrap_or(0) }));
        }
    }

    /// The interned string equal to `r`, if any (`r` itself once interned). For reads
    /// only: a table holds interned keys, so a long string that has no interned copy is in
    /// no table.
    pub fn find_canon(&self, r: StrRef) -> Option<StrRef> {
        let s = self.strings.get(r.idx)?;
        if s.interned {
            return Some(r);
        }
        if let Some(t) = s.twin.get()
            && self.strings.live(t.idx, t.generation)
        {
            return Some(t);
        }
        let idx = self.find_interned(self.str_hash(s), &s.bytes).ok()?;
        let t = StrRef { idx, generation: self.strings.gens.get(idx as usize).copied().unwrap_or(0) };
        s.twin.set(Some(t));
        Some(t)
    }

    /// `find_canon` when it is known without walking the interner: `r` itself once
    /// interned, or the copy found before (for the fast loop, S5GC-3). None: not known.
    #[inline]
    pub fn known_canon(&self, r: StrRef) -> Option<StrRef> {
        let s = self.strings.get(r.idx)?;
        if s.interned {
            return Some(r);
        }
        s.twin.get().filter(|t| self.strings.live(t.idx, t.generation))
    }

    /// The interned string equal to `r`, interning `r` if there is none (for a table key).
    pub fn canon(&mut self, r: StrRef) -> StrRef {
        let Some(s) = self.strings.get(r.idx) else { return r };
        if s.interned {
            return r;
        }
        if let Some(t) = s.twin.get()
            && self.strings.live(t.idx, t.generation)
        {
            return self.keep(t.idx);
        }
        // (One lookup: a second compared a long string's bytes again, S6T-EQ.)
        match self.find_interned(self.str_hash(s), &s.bytes) {
            Ok(i) => {
                let t = StrRef { idx: i, generation: self.strings.gens.get(i as usize).copied().unwrap_or(0) };
                s.twin.set(Some(t));
                self.keep(i)
            }
            Err(slot) => {
                if let Some(s) = self.strings.get_mut(r.idx) {
                    s.interned = true;
                }
                self.insert_interned(slot, r.idx);
                r
            }
        }
    }

    /// The key to look up again after `key` missed in a table: the interned copy of a long
    /// string that is not interned itself.
    #[inline]
    pub fn retry_key(&self, key: Value) -> Option<Value> {
        let Value::Str(r) = key else { return None };
        let c = self.find_canon(r)?;
        (c != r).then_some(Value::Str(c))
    }

    /// `tab[key]`, raw, for a key that may be a long string not interned yet.
    #[inline]
    pub fn get_in(&self, tab: &Table, key: Value) -> Value {
        let v = tab.get(key);
        match v {
            Value::Nil => self.retry_key(key).map_or(v, |k| tab.get(k)),
            _ => v,
        }
    }

    /// Whether two different string objects are equal, when that is known without comparing
    /// their bytes (None: compare them). Interned strings equal only themselves.
    #[inline]
    pub fn str_eq_quick(&self, a: StrRef, b: StrRef) -> Option<bool> {
        if a == b {
            return Some(true);
        }
        let (Some(x), Some(y)) = (self.strings.get(a.idx), self.strings.get(b.idx)) else { return Some(false) };
        let (hx, hy) = (x.hash.get(), y.hash.get());
        if x.bytes.len() != y.bytes.len() || (x.interned && y.interned) || (hx != 0 && hy != 0 && hx != hy) {
            return Some(false);
        }
        None
    }

    /// Takes string `idx` out of the interner: the collector work its walk costs past a
    /// plain one (S5GC-3).
    fn unintern(&mut self, idx: u32) -> usize {
        let h = self.strings.get(idx).map(|x| x.hash.get()).unwrap_or(0);
        let (cost, freed) = self.interner.remove(&self.strings, h, idx);
        self.mem.release(freed);
        cost
    }

    #[inline]
    pub fn str_bytes(&self, r: StrRef) -> &[u8] {
        self.strings.get(r.idx).map(|s| &*s.bytes).unwrap_or_default()
    }

    // ---- objects ----

    /// A collector step is due, or the work of the objects made since the last.
    #[inline]
    pub fn gc_due(&self) -> bool {
        self.mem.debt > 0 || self.born.pending() >= BORN_DUE
    }

    pub fn new_table(&mut self, arr: usize, hash: usize) -> Result<TableRef, ()> {
        Self::new_table_in::<false>(&mut self.mem, &mut self.tables, &mut self.born, (self.phase, self.sweep, self.table_seed), arr, hash)
    }

    /// [`Heap::new_table`] on the fields it touches (for the fast loop, which holds the
    /// prototypes borrowed).
    pub(crate) fn new_table_in<const REGISTER: bool>(
        mem: &mut Mem,
        tables: &mut Slab<Table>,
        born: &mut Born,
        (phase, sweep, seed): (Phase, (u8, usize), u64),
        arr: usize,
        hash: usize,
    ) -> Result<TableRef, ()> {
        let cost = crate::table::new_cost(arr, hash);
        // A bytecode constructor is in its destination register before collection can
        // run again. Under pressure its owners (including that stack) trace it like any
        // other table. Host and native constructors retain their normal birth color.
        let phase = if REGISTER && born.trace_new_tables && phase == Phase::Propagate { Phase::Pause } else { phase };
        let color = |i| Self::new_color(phase, sweep, 1, i);
        let free = if mem.charge(cost).is_ok() {
            tables.take_free(mem, color)
        } else {
            // Free slab slots are already charged to this state. At the cap a
            // replacement only needs room for its net growth. Check the final
            // account before consuming the slot so a refusal changes no handle.
            let fits = mem.used.checked_sub(tables.slot_bytes)
                .and_then(|used| used.checked_add(cost))
                .is_some_and(|used| used <= mem.cap.saturating_add(mem.headroom));
            if !fits {
                return Err(());
            }
            let slot = tables.take_free(mem, color).ok_or(())?;
            mem.charge(cost)?;
            Some(slot)
        };
        let slots = crate::table::hash_slots(hash);
        let walked = arr + slots;
        let (arr, nodes, live) = (mem.pool_arr(arr), mem.pool_nodes(slots), crate::table::LiveBits::for_len(slots));
        let (idx, generation) = match free {
            Some((i, generation, t)) => {
                (t.seed, t.arr, t.nodes, t.live) = (seed, arr.into(), nodes, live);
                (i, generation)
            }
            None => tables.alloc(Table { seed, arr: arr.into(), nodes, live, ..Table::default() }, mem, color),
        };
        born.add(phase, Gray::Table(idx), walked);
        Ok(TableRef { idx, generation })
    }

    /// A function whose memory the caller charged already.
    pub fn new_func_charged(&mut self, f: Func) -> FuncRef {
        let (phase, sweep) = (self.phase, self.sweep);
        let n = match &f {
            Func::Lua { upvals, .. } => upvals.len(),
            Func::Native { upvals, .. } => upvals.len(),
            Func::Host { .. } => 0,
        };
        let (idx, generation) = self.funcs.alloc(f, &mut self.mem, |i| Self::new_color(phase, sweep, 2, i));
        self.born.add(phase, Gray::Func(idx), n);
        FuncRef { idx, generation }
    }

    /// A prototype whose memory the caller charged already.
    pub fn new_proto_charged(&mut self, p: Proto) -> u32 {
        let (phase, sweep) = (self.phase, self.sweep);
        let n = p.consts.len() + p.protos.len();
        let (idx, _) = self.protos.alloc(p, &mut self.mem, |i| Self::new_color(phase, sweep, 6, i));
        self.born.add(phase, Gray::Proto(idx), n);
        idx
    }

    /// An upvalue whose memory the caller charged already. Upvalue references reuse
    /// [`FuncRef`]'s shape (index and generation).
    pub fn new_upval_charged(&mut self, u: Upval) -> FuncRef {
        let (phase, sweep) = (self.phase, self.sweep);
        let (idx, generation) = self.upvals.alloc(u, &mut self.mem, |i| Self::new_color(phase, sweep, 3, i));
        self.born.add(phase, Gray::Upval(idx), 0);
        FuncRef { idx, generation }
    }

    pub fn new_userdata(&mut self, data: Box<dyn Any + Send>, size: usize, meta: Option<TableRef>) -> Result<UdRef, ()> {
        self.mem.charge(UD_BASE + size)?;
        let (phase, sweep) = (self.phase, self.sweep);
        let (idx, generation) =
            self.uds.alloc(UserData { data: Some(data), meta, size, notify: None }, &mut self.mem, |i| Self::new_color(phase, sweep, 4, i));
        self.born.add(phase, Gray::Ud(idx), 0);
        Ok(UdRef { idx, generation })
    }

    pub fn new_thread(&mut self, state: ThreadState) -> Result<ThreadRef, ()> {
        self.mem.charge(state.mem_size())?;
        Ok(self.place_thread(state))
    }

    /// A thread whose memory (`ThreadState::mem_size`) was charged already.
    pub fn place_thread(&mut self, state: ThreadState) -> ThreadRef {
        let (phase, sweep) = (self.phase, self.sweep);
        let n = state.stack.len() + state.frames.len() + state.open_upvals.len();
        let (idx, generation) = self.threads.alloc(Box::new(state), &mut self.mem, |i| Self::new_color(phase, sweep, 5, i));
        self.born.add(phase, Gray::Thread(idx), n);
        ThreadRef { idx, generation }
    }

    /// Whether `v` names an object that is still alive (for values the host passes in).
    pub fn is_live(&self, v: Value) -> bool {
        let (slab, idx, live, color) = match v {
            Value::Str(r) => (0, r.idx, self.strings.live(r.idx, r.generation), &self.strings.color),
            Value::Table(r) => (1, r.idx, self.tables.live(r.idx, r.generation), &self.tables.color),
            Value::Func(r) => (2, r.idx, self.funcs.live(r.idx, r.generation), &self.funcs.color),
            Value::UserData(r) => (4, r.idx, self.uds.live(r.idx, r.generation), &self.uds.color),
            Value::Thread(r) => (5, r.idx, self.threads.live(r.idx, r.generation), &self.threads.color),
            _ => return true,
        };
        // Found dead by the last marking and not swept yet: still in its slot, but the sweep
        // will free it, so a handle to it must not be stored anywhere (sweep slab order).
        let doomed = self.phase == Phase::Sweep
            && (slab, idx as usize) >= self.sweep
            && color.get(idx as usize) == Some(&WHITE);
        live && !doomed
    }

    // ---- barriers ----

    /// A table was written: if marking already passed it, it is marked again later.
    #[inline]
    pub fn barrier_table(&mut self, t: TableRef) {
        if self.phase == Phase::Propagate
            && let Some(c) = self.tables.color.get_mut(t.idx as usize)
            && *c == BLACK
        {
            if t.idx == self.part {
                // Traversed in chunks: its entries were marked as they were written
                // (`part_written`); a new metatable is marked here.
                self.part_meta();
                return;
            }
            *c = GRAY;
            self.grayagain.push(Gray::Table(t.idx));
        }
    }

    /// The metatable of the table traversed in chunks, which may have changed.
    #[cold]
    fn part_meta(&mut self) {
        if let Some(m) = self.tables.get(self.part).and_then(|t| t.meta) {
            self.mark(Value::Table(m));
        }
    }

    // ---- the table traversed in chunks (S6GC-2e) ----
    //
    // While marking is incremental, a table of more than `TABLE_CHUNK` slots is traversed
    // that many slots a step (`part_step`), so no step takes a big table's traversal whole
    // (42 to 45 ms for 800,000 entries). The script runs between its chunks, so each write
    // into it then goes through `part_writing` and `part_written`, which make what marking
    // keeps of it exactly what it held when its traversal started plus everything written
    // into it since (a snapshot-at-the-beginning barrier, as Yuasa's):
    // - the entry a write overwrites or removes is marked before it goes (`part_writing`):
    //   so every entry the table held at the start is marked, by the traversal or then;
    // - the entry written is marked as it comes (`part_written`), and so is any entry the
    //   write moved to another node (`Watch::moved`), wherever it lands;
    // - when the table's parts are rebuilt or resized (`Watch::restructured`) after the
    //   traversal passed the array entries no rebuild moves (`Watch::alen_floor`), it
    //   starts over: every entry the table holds then is in that set already, and every
    //   entry of the start no longer there was marked as it went, at every rebuild. (A
    //   script cannot keep it from ending: a rebuild allocates the table's new parts, and
    //   the steps that allocation owes traverse them again.)
    // Its chunks go one after another, before any other gray object, so when its last one
    // comes follows the work alone; writes after it go to `grayagain`, as for any table.
    // None of this depends on which slot an entry sits in, so what is marked, and the work
    // (charged by the slots each chunk spans, never by what they hold), follow neither the
    // layout nor the hash seed (as traversing it in chunks alone did: entries overwritten
    // between chunks were marked or not by which chunk their node fell in). No log is kept:
    // an object turns gray once a cycle at most, so the gray list stays bounded by the live
    // heap whatever the script writes (a log of overwritten values grew without bound,
    // uncharged, under `t[i] = t[j]`, which allocates nothing and so runs no step).

    /// A big table reached while marking is incremental: traversed in chunks, or waiting
    /// its turn while another is (gray, so writes into it need no barrier yet).
    #[cold]
    #[inline(never)]
    fn big_table(&mut self, i: u32) -> usize {
        if self.part == NO_PART {
            return self.begin_part(i);
        }
        self.part_queue.push(i);
        1
    }

    /// Objects that can turn gray (tables, functions, userdata, threads) the slabs hold.
    fn live_objects(&self) -> usize {
        let live = |items: usize, free: usize| items.saturating_sub(free);
        live(self.tables.slots(), self.tables.free.len())
            + live(self.funcs.slots(), self.funcs.free.len())
            + live(self.uds.slots(), self.uds.free.len())
            + live(self.threads.slots(), self.threads.free.len())
    }

    /// Starts traversing table `i` in chunks: the work.
    fn begin_part(&mut self, i: u32) -> usize {
        if let Some(c) = self.tables.color.get_mut(i as usize) {
            *c = BLACK;
        }
        let meta = self.tables.get(i).and_then(|t| t.meta);
        if let Some(m) = meta {
            self.mark(Value::Table(m));
        }
        // Its weak mode as it is now holds for the whole traversal: a mode changed between
        // chunks would mark the chunks before and after differently, by the layout.
        self.part_mode = self.weak_mode(meta);
        self.part = i;
        self.part_at = 0;
        self.part_paid = 0;
        self.mem.watch.moved.clear();
        self.mem.watch.restructured = false;
        self.mem.watch.alen_floor = usize::MAX;
        if let Some(t) = self.tables.at_mut(i as usize) {
            t.gc_watch = true;
        }
        1
    }

    /// No table is traversed in chunks any more.
    fn end_part(&mut self) {
        if let Some(t) = self.tables.at_mut(self.part as usize) {
            t.gc_watch = false;
        }
        self.part = NO_PART;
        self.mem.watch.moved.clear();
        self.mem.watch.restructured = false;
        self.mem.watch.alen_floor = usize::MAX;
    }

    /// The next chunk of the table traversed in chunks: the work, by the slots it spans.
    /// The next chunk of the table traversed in chunks: (the work it charged, the slots it
    /// scanned, the slots of those grown in the cycle). Past the slots paid for (a table
    /// grown in the cycle, or a traversal a rebuild started over) a chunk charges nothing
    /// and still scans its slots, so they are counted apart (SEC7-G1): the charge and the
    /// grown slots drive the step's end, the slots what a poll costs. (The grown ones spent
    /// no step's budget at first: a call took a step at 64 Ki slots, the cap, one after
    /// another, 16 ms in one call for a hash part grown from 2^20 to 2^21 nodes.)
    fn part_step(&mut self) -> (usize, usize, usize) {
        let i = self.part;
        let (wk, wv) = self.part_mode;
        let floor = crate::nostd::mem::replace(&mut self.mem.watch.alen_floor, usize::MAX);
        let alen_now = self.tables.at(i as usize).map_or(0, |t| t.arr.live_len());
        // A rebuild moved entries behind the traversal: it starts again, in chunks as
        // before (S6R-2: after a second rebuild the rest was marked whole, 37 ms at 1.5
        // million keys). (Traversed no further than the array part's entries no rebuild
        // moved: nothing moved behind it, the common case of an array growing while it is
        // traversed.)
        if crate::nostd::mem::take(&mut self.mem.watch.restructured) && self.part_at > floor.min(alen_now) {
            self.part_at = 0;
        }
        let total = self.tables.at(i as usize).map_or(0, Table::slots);
        let from = self.part_at.min(total);
        let to = from.saturating_add(TABLE_CHUNK).min(total);
        self.mark_chunk(i, (from, to), total, (wk, wv));
        self.most_at_once = self.most_at_once.max(to - from);
        self.scanned = self.scanned.wrapping_add(to - from);
        // Writes after its last chunk go to `grayagain`, as for any table traversed.
        self.part_at = to;
        // By its slots when the cycle started (`Table::slots_at`), each paid once: its size
        // when reached, its chunks and how often a rebuild started them over follow when
        // it was reached, so the hash seed (COV-5, as `traverse_table`). A traversal
        // started over is paid by the rebuild that did it.
        let snap = self.tables.get(i).map_or(0, |t| t.slots_at(self.cycle));
        let upto = if to >= total { snap } else { to.min(snap) };
        let paid = upto.saturating_sub(self.part_paid);
        let work = paid * (1 + self.obj_work / 4);
        self.part_paid = self.part_paid.max(upto);
        if to >= total {
            self.finish_part();
        }
        // (The slots it scanned past the table's size when the cycle started: grown in the
        // cycle, paid as they grew, `propagate_from`. Where those are follows the table's
        // sizes alone; a traversal a rebuild started over is not counted, since where it
        // had got to followed when it was reached, so the seed.)
        (work, to - from, to.saturating_sub(from.max(snap)))
    }

    /// A chunk of the table traversed in chunks marked (`mark_slots`), compiled on its own:
    /// inlined into `propagate`, its loop over the array part took twice the instructions.
    #[inline(never)]
    fn mark_chunk(&mut self, i: u32, range: (usize, usize), total: usize, mode: (bool, bool)) {
        let alen = self.tables.at(i as usize).map_or(0, |t| t.arr.live_len());
        self.mark_slots::<true>(i, range, (alen, total), mode);
    }

    /// Ends the traversal in chunks (its last chunk done).
    fn finish_part(&mut self) {
        let i = self.part;
        let (wk, wv) = self.part_mode;
        self.end_part();
        if wk || wv {
            // Weak tables are revisited in the atomic step (their ephemerons too).
            self.weak.push((i, wk, wv));
            if let Some(c) = self.tables.color.get_mut(i as usize) {
                *c = GRAY;
            }
        }
    }

    /// An entry (key, value) of the table traversed in chunks, marked as its traversal
    /// would mark it (weak parts not; strings always).
    fn part_mark(&mut self, k: Value, v: Value) {
        let (mk, mv) = entry_marks(k, v, self.part_mode);
        if mk {
            self.mark(k);
        }
        if mv {
            self.mark(v);
        }
    }

    /// Before any write of `key` into table `t` while it is traversed in chunks (a value,
    /// nil or a new key): the entry it overwrites or removes is marked. The key as the
    /// table holds it, or `key` when it holds none.
    #[cold]
    pub fn part_writing(&mut self, t: u32, key: Value) -> Value {
        // (Its lookup walks what the write's own does, and a lookup's probe steps count
        // the longest walk, not the sum: it costs the script nothing more.)
        match self.tables.get(t).and_then(|tab| tab.entry(key)) {
            Some((k, old)) => {
                if !old.is_nil() {
                    self.part_mark(k, old);
                }
                k
            }
            None => key,
        }
    }

    /// After that write of `val` at `key` (as `part_writing` gave it): the entry written is
    /// marked, and any entry the write moved to another node.
    #[cold]
    pub fn part_written(&mut self, t: u32, key: Value, val: Value) {
        if self.part != t {
            return;
        }
        if !val.is_nil() {
            self.part_mark(key, val);
        }
        let mut moved = crate::nostd::mem::take(&mut self.mem.watch.moved);
        let mut it = moved.iter().copied();
        while let (Some(k), Some(v)) = (it.next(), it.next()) {
            self.part_mark(k, v);
        }
        moved.clear();
        self.mem.watch.moved = moved;
    }

    /// An upvalue was written.
    #[inline]
    pub fn barrier_upval(&mut self, u: u32) {
        if self.phase == Phase::Propagate
            && let Some(c) = self.upvals.color.get_mut(u as usize)
            && *c == BLACK
        {
            *c = GRAY;
            self.grayagain.push(Gray::Upval(u));
        }
    }

    // ---- marking ----

    /// Marks a value reachable.
    #[inline]
    pub fn mark(&mut self, v: Value) {
        match v {
            Value::Str(r) => {
                if let Some(c) = self.strings.color.get_mut(r.idx as usize)
                    && *c == WHITE
                {
                    *c = BLACK;
                }
            }
            Value::Table(r) => self.mark_gray(Gray::Table(r.idx)),
            Value::Func(r) => self.mark_gray(Gray::Func(r.idx)),
            Value::UserData(r) => self.mark_gray(Gray::Ud(r.idx)),
            Value::Thread(r) => self.mark_gray(Gray::Thread(r.idx)),
            _ => {}
        }
    }

    /// Marks the ephemeron values that waited for `g` as their key. Called when `g` is
    /// traversed, not when it is marked: marking them at once recursed through
    /// `mark`, one native frame per link, so a long ephemeron chain (key -> value that is
    /// the next key) overflowed the native stack and killed the process. Every marked key
    /// is traversed (it goes on the gray list), so none is missed.
    fn release_waiting(&mut self, g: Gray) {
        if !self.eph_active || self.eph_waiting.is_empty() {
            return;
        }
        let key = match g {
            Gray::Table(i) => (1, i),
            Gray::Func(i) => (2, i),
            Gray::Ud(i) => (4, i),
            Gray::Thread(i) => (5, i),
            Gray::Upval(_) | Gray::Proto(_) | Gray::TablePart(..) => return,
        };
        let mut i = self.eph_waiting.remove(&key).unwrap_or(u32::MAX);
        while let Some(&(v, next)) = self.eph_links.get(i as usize) {
            self.mark(v);
            i = next;
        }
    }

    #[inline]
    fn mark_gray(&mut self, g: Gray) {
        let color = match g {
            Gray::TablePart(..) => return,
            Gray::Proto(i) => self.protos.color.get_mut(i as usize),
            Gray::Table(i) => self.tables.color.get_mut(i as usize),
            Gray::Func(i) => self.funcs.color.get_mut(i as usize),
            Gray::Upval(i) => self.upvals.color.get_mut(i as usize),
            Gray::Ud(i) => self.uds.color.get_mut(i as usize),
            Gray::Thread(i) => self.threads.color.get_mut(i as usize),
        };
        if let Some(c) = color
            && *c == WHITE
        {
            *c = GRAY;
            if self.part == NO_PART && !self.eph_active {
                self.gray.push(g);
            } else {
                self.ordered_gray.push(g);
            }
        }
    }

    fn pop_gray(&mut self) -> Option<Gray> {
        self.gray.pop().or_else(|| self.ordered_gray.pop())
    }

    fn gray_empty(&self) -> bool {
        self.gray.is_empty() && self.ordered_gray.is_empty()
    }

    fn is_marked(&self, v: Value) -> bool {
        let c = match v {
            Value::Str(r) => self.strings.color.get(r.idx as usize),
            Value::Table(r) => self.tables.color.get(r.idx as usize),
            Value::Func(r) => self.funcs.color.get(r.idx as usize),
            Value::UserData(r) => self.uds.color.get(r.idx as usize),
            Value::Thread(r) => self.threads.color.get(r.idx as usize),
            _ => return true,
        };
        c.is_some_and(|&c| c == GRAY || c == BLACK)
    }

    /// The weak mode of a table with metatable `meta`: (weak keys, weak values).
    /// S5B-WALK-3: the `__mode` string was scanned whole for every table traversed, free:
    /// 200 tables under a 4 MiB `__mode` ran at 110x plain code per unit of fuel (far more
    /// at a short pause). C Lua reads a mode only from a short string, up to its first zero
    /// byte (`traversetable`: `ttisshrstring`, `strchr`); so does this, which bounds the
    /// scan at `SHORT_LEN` bytes and gives C Lua's answer for a long or zero-cut mode.
    #[inline(always)]
    fn weak_mode(&self, meta: Option<TableRef>) -> (bool, bool) {
        let (Some(m), Some(key)) = (meta, self.mode_key) else { return (false, false) };
        let Some(mt) = self.tables.get(m.idx) else { return (false, false) };
        match mt.get_str(Value::Str(key)) {
            Value::Str(s) if self.str_bytes(s).len() <= SHORT_LEN => {
                let b = self.str_bytes(s);
                let b = b.split(|&c| c == 0).next().unwrap_or_default();
                (b.contains(&b'k'), b.contains(&b'v'))
            }
            _ => (false, false),
        }
    }

    /// Traverses one gray object: marks what it holds. Returns the work done.
    /// Traverses table `i`'s slots from `from` (array part, then hash nodes); in the atomic
    /// phase at most `TABLE_CHUNK` of them, the rest going back on the gray list
    /// (`Gray::TablePart`), so the phase can stop at a call's fuel inside a big table
    /// (S6GC-2e).
    fn traverse_table(&mut self, i: u32, from: usize) -> usize {
        let cycle = self.cycle;
        let Some((meta, alen, total, snap)) =
            self.tables.at(i as usize).map(|t| (t.meta, t.arr.live_len(), t.arr.live_len() + t.hash_len(), t.slots_at(cycle)))
        else {
            return 1;
        };
        if from == 0 && !self.eph_active && total > TABLE_CHUNK {
            return self.big_table(i);
        }
        if from == 0 {
            if let Some(c) = self.tables.color.get_mut(i as usize) {
                *c = BLACK;
            }
            if let Some(m) = meta {
                self.mark(Value::Table(m));
            }
        }
        let (wk, wv) = self.weak_mode(meta);
        // In chunks of `TABLE_CHUNK` inside the atomic phase, where no script runs between
        // them (`Gray::TablePart`); while marking is incremental a big table is traversed in
        // chunks by `part_step` instead, so here it is whole.
        // (An ephemeron table's entries cost `EPH_ENTRY_WORK + 4 * obj_work` each there: in
        // chunks of 256, one chunk is about as much work as the atomic phase does between
        // two looks at the interrupt and the deadline, not 20 times that.)
        let chunk = match (self.eph_active, wk && !wv) {
            (true, true) => EPH_CHUNK,
            (true, false) => TABLE_CHUNK,
            (false, _) => usize::MAX,
        };
        let to = from.saturating_add(chunk).min(total);
        let (nodes_from, nodes_to) = (from.saturating_sub(alen), to.saturating_sub(alen));
        let gray_start = self.gray.len();
        self.mark_slots::<false>(i, (from, to), (alen, total), (wk, wv));
        // Children of a whole hash table must enter the stack in a stable order:
        // a mutator write between two child traversals can change which old value
        // survives this cycle (SEC7-G2). Whole tables span at most TABLE_CHUNK
        // slots. Array-only scans already visit their entries in index order.
        if total > alen && self.gray.len() > gray_start + 1
            && let Some(children) = self.gray.get_mut(gray_start..)
        {
            children.sort_unstable();
        }
        self.most_at_once = self.most_at_once.max(to.saturating_sub(from));
        self.scanned = self.scanned.wrapping_add(to.saturating_sub(from));
        if to < total {
            self.gray.push(Gray::TablePart(i, u32::try_from(to).unwrap_or(u32::MAX)));
        } else if wk || wv {
            self.weak.push((i, wk, wv));
            // Weak tables are revisited in the atomic step.
            if let Some(c) = self.tables.color.get_mut(i as usize) {
                *c = GRAY;
            }
        }
        // By its slots when the cycle started or it was made (`Table::gc_snap`), never by
        // what it holds or its size when reached: when that is follows the order objects are
        // reached in, so the hash seed (S6GC-2e by what it held, COV-5 by its size, in the
        // atomic step too). Its growth is paid by the resizes. Charged a part at a time.
        let slots = to.min(snap) - from.min(snap) + if to >= total { snap.saturating_sub(total) } else { 0 };
        let mut work = usize::from(from == 0) + slots * (1 + self.obj_work / 4);
        if wk && !wv {
            // Ephemeron: a value is marked when its key is.
            work += self.mark_ephemeron(i, nodes_from, nodes_to);
        }
        work
    }

    /// Marks what table `i` holds in slots `from..to` (array part, then hash nodes) as a
    /// table of weak mode (`wk`, `wv`), entries as `entry_marks` decides when `PART` (the
    /// table traversed in chunks): the objects other than strings marked. (A constant, so
    /// the traversal of a whole table is the code it was before chunks.)
    #[inline(always)]
    fn mark_slots<const PART: bool>(
        &mut self,
        i: u32,
        (from, to): (usize, usize),
        (alen, total): (usize, usize),
        (wk, wv): (bool, bool),
    ) -> usize {
        // Collected first, so the table is not borrowed while marking. Strings are
        // values, never weak: they are always marked.
        let is_str = |v: &Value| matches!(v, Value::Str(_));
        let node = |n: &crate::table::Node, todo: &mut Vec<Value>| {
            if n.val.is_nil() {
                return;
            }
            if PART {
                let (mk, mv) = entry_marks(n.key(), n.val, (wk, wv));
                if mk {
                    todo.push(n.key());
                }
                if mv {
                    todo.push(n.val);
                }
                return;
            }
            if !wk || is_str(&n.key()) {
                todo.push(n.key());
            }
            if (!wv && !wk) || is_str(&n.val) {
                todo.push(n.val);
            }
        };
        // S6GC-M1: marked `TRAV_CHUNK` slots at a time, so the buffer stays small (a table
        // of 2^23 slots holding one shared table filled it with 128 MiB of copies,
        // uncharged and kept).
        let mut todo = crate::nostd::mem::take(&mut self.trav_vals);
        let mut marked = 0;
        let mut at = from;
        while at < to {
            let end = (at + TRAV_CHUNK).min(to);
            if let Some(t) = self.tables.at(i as usize) {
                for (_, part) in t.arr.chunks(at..end) {
                    todo.extend(part.iter().copied().filter(|v| v.is_collectable() && (!wv || is_str(v))));
                }
                let (named, ints) = t.hash_parts();
                if at == 0 && end == total {
                    // The whole table at once (most tables): finding positions in the hash
                    // parts made marking small tables a quarter slower.
                    named.iter().for_each(|n| node(n, &mut todo));
                    ints.iter().for_each(|n| node(n, &mut todo));
                } else if end > alen {
                    let (nf, nt) = (at.saturating_sub(alen), end - alen);
                    // (The two parts' slices, not `hash_nodes().skip()`: a chain stepped
                    // node by node cost a chunk of a big table twice its marking.)
                    let k = named.len();
                    named.get(nf.min(k)..nt.min(k)).unwrap_or_default().iter().for_each(|n| node(n, &mut todo));
                    ints.get(nf.saturating_sub(k)..nt.saturating_sub(k)).unwrap_or_default().iter().for_each(|n| node(n, &mut todo));
                }
            }
            // Each object marked is a mark read at a random place (hash order follows no
            // allocation order): in a big heap a cache miss, paid by the live heap's size
            // (S6GC-2d: a strong table of 800,000 objects ran at 3x plain code per unit).
            marked += todo.iter().filter(|v| !matches!(v, Value::Str(_))).count();
            for v in todo.drain(..) {
                self.mark(v);
            }
            at = end;
        }
        self.trav_vals = todo;
        marked
    }

    fn traverse(&mut self, g: Gray, running: Option<&ThreadState>) -> usize {
        self.release_waiting(g);
        match g {
            Gray::Proto(i) => {
                if let Some(c) = self.protos.color.get_mut(i as usize) {
                    *c = BLACK;
                }
                let (consts, children, walked): (Vec<Value>, Vec<u32>, usize) = match self.protos.get(i) {
                    Some(p) => (
                        p.consts.iter().copied().chain(crate::nostd::iter::once(p.chunk_name)).filter(|v| v.is_collectable()).collect(),
                        p.protos.clone(),
                        p.consts.len(),
                    ),
                    None => (Vec::new(), Vec::new(), 0),
                };
                // Every constant looked at counts, not only the objects among them
                // (S5B-WALK-1).
                let work = 1 + walked + children.len();
                for k in consts {
                    self.mark(k);
                }
                for c in children {
                    self.mark_gray(Gray::Proto(c));
                }
                work
            }
            Gray::Table(i) => self.traverse_table(i, 0),
            // Finished even when a write sent the table back to gray (it is traversed again
            // whole in the atomic step): skipping the rest would save work by where the
            // write fell, which follows the order objects are reached in, so the seed.
            Gray::TablePart(i, from) => self.traverse_table(i, from as usize),
            Gray::Func(i) => {
                if let Some(c) = self.funcs.color.get_mut(i as usize) {
                    *c = BLACK;
                }
                let mut todo_up = crate::nostd::mem::take(&mut self.trav_ups);
                let mut todo_vals = crate::nostd::mem::take(&mut self.trav_vals);
                let mut proto = None;
                match self.funcs.get(i) {
                    Some(Func::Lua { proto: p, upvals }) => {
                        proto = Some(*p);
                        todo_up.extend(upvals.iter().map(|u| u.idx));
                    }
                    Some(Func::Native { upvals, .. }) => todo_vals.extend(upvals.iter().copied()),
                    _ => {}
                }
                if let Some(p) = proto {
                    self.mark_gray(Gray::Proto(p));
                }
                let work = 1 + todo_up.len() + todo_vals.len();
                for u in todo_up.drain(..) {
                    self.mark_gray(Gray::Upval(u));
                }
                for v in todo_vals.drain(..) {
                    self.mark(v);
                }
                (self.trav_ups, self.trav_vals) = (todo_up, todo_vals);
                work
            }
            Gray::Upval(i) => {
                if let Some(c) = self.upvals.color.get_mut(i as usize) {
                    *c = BLACK;
                }
                match self.upvals.get(i).copied() {
                    Some(Upval::Closed(v)) => self.mark(v),
                    Some(Upval::Open { thread, .. }) => self.mark(Value::Thread(thread)),
                    None => {}
                }
                1
            }
            Gray::Ud(i) => {
                if let Some(c) = self.uds.color.get_mut(i as usize) {
                    *c = BLACK;
                }
                if let Some(m) = self.uds.get(i).and_then(|u| u.meta) {
                    self.mark(Value::Table(m));
                }
                1
            }
            Gray::Thread(i) => {
                // Threads stay gray: their stacks change without barriers, so they are
                // marked again in the atomic step.
                let mut todo: Vec<Value> = Vec::new();
                let mut protos: Vec<u32> = Vec::new();
                let mut ups: Vec<u32> = Vec::new();
                // S6GC-M2: of a suspended or dead coroutine only the live part of the stack
                // is marked (C Lua's `traversethread`): stale slots above it held garbage
                // (a weak table kept it; six parked coroutines each with a dead 16 MiB
                // string ran out of a 64 MiB cap). The atomic step clears them, so a value
                // left there is not found again when the coroutine runs.
                let mut walked = 0;
                if let Some(t) = self.threads.get(i) {
                    let live = parked_live(t, &self.protos);
                    walked = collect_thread(t, live.unwrap_or(usize::MAX), &mut todo, &mut protos, &mut ups);
                    if self.eph_active
                        && let Some(live) = live
                        && let Some(t) = self.threads.get_mut(i)
                    {
                        t.stack.get_mut(live..).unwrap_or_default().fill(Value::Nil);
                    }
                }
                let _ = running;
                let work = 1 + walked;
                for v in todo {
                    self.mark(v);
                }
                for p in protos {
                    self.mark_gray(Gray::Proto(p));
                }
                for u in ups {
                    self.mark_gray(Gray::Upval(u));
                }
                work
            }
        }
    }

    /// An ephemeron table: marks values whose keys are marked; while the atomic step
    /// settles ephemerons, the others wait for their key (`eph_waiting`). Returns the work:
    /// in the atomic step `EPH_ENTRY_WORK` an entry (S5B-COST-3).
    fn mark_ephemeron(&mut self, i: u32, from: usize, to: usize) -> usize {
        let Some(t) = self.tables.at(i as usize) else { return 0 };
        let mut todo = Vec::new();
        for n in t.hash_nodes().skip(from).take(to.saturating_sub(from)) {
            if !n.val.is_nil() && n.val.is_collectable() {
                todo.push((n.key(), n.val));
            }
        }
        // In the atomic step an entry is looked up in `eph_waiting`, linked, found again and
        // its value marked, each at a random place, and outside it its key's mark is read
        // at a random place: in a big heap these miss the caches as reaching an object
        // does, so they pay `obj_work` too (by the live heap's size, never its layout;
        // S6GC-2d: an 800,000-entry chain ran at 3x a strong one per unit of fuel).
        let work = if self.eph_active { todo.len() * (EPH_ENTRY_WORK + 4 * self.obj_work) } else { todo.len() * (1 + self.obj_work / 2) };
        // While marking is incremental, values are left to the atomic step, all of them:
        // marked here when their key happened to be marked already, whether a value was
        // reached then (and, a weak table or thread, traversed again there) or only there
        // followed the order objects are reached in, so the seed (SEC7-H1: 400 weak-table
        // values cost 5960 to 6191 by the seed).
        if !self.eph_active {
            return work;
        }
        for (k, v) in todo {
            if self.is_marked(k) {
                if !self.is_marked(v) {
                    self.mark(v);
                }
            } else {
                let key = match k {
                    Value::Table(r) => (1, r.idx),
                    Value::Func(r) => (2, r.idx),
                    Value::UserData(r) => (4, r.idx),
                    Value::Thread(r) => (5, r.idx),
                    _ => continue,
                };
                let head = self.eph_waiting.get(&key).copied().unwrap_or(u32::MAX);
                self.eph_links.push((v, head));
                self.eph_waiting.insert(key, (self.eph_links.len() - 1) as u32);
            }
        }
        work
    }

    /// Marks the running thread's state (kept outside the slab while it runs): the work,
    /// the slots and records walked (S5B-WALK-1).
    pub fn mark_thread_state(&mut self, t: &ThreadState) -> usize {
        let mut todo = Vec::new();
        let mut protos = Vec::new();
        let mut ups = Vec::new();
        let walked = collect_thread(t, usize::MAX, &mut todo, &mut protos, &mut ups);
        for v in todo {
            self.mark(v);
        }
        for p in protos {
            self.mark_gray(Gray::Proto(p));
        }
        for u in ups {
            self.mark_gray(Gray::Upval(u));
        }
        walked
    }

    /// Starts a cycle: everything white, roots to be marked by the caller.
    pub fn start_cycle(&mut self) {
        self.pace_carry = 0;
        self.cycle = self.cycle.wrapping_add(1).max(1);
        self.mem.cycle = self.cycle;
        let live = |items: usize, free: usize| items.saturating_sub(free);
        self.obj_work = obj_work(
            live(self.tables.slots(), self.tables.free.len())
                + live(self.funcs.slots(), self.funcs.free.len())
                + live(self.threads.slots(), self.threads.free.len())
                + live(self.protos.slots(), self.protos.free.len()),
        );
        self.born.obj_work = self.obj_work;
        (self.mem.marking, self.mem.obj_work) = (true, self.obj_work);
        self.end_part();
        self.part_queue.clear();
        self.gray.clear();
        self.ordered_gray.clear();
        self.born.list.clear();
        self.grayagain.clear();
        self.weak.clear();
        self.phase = Phase::Propagate;
    }

    /// Does up to `budget` units of marking; true when the gray list is empty.
    /// (Stopped early by `stop`: the third is true, the step's work done so far.) The
    /// fourth: the table slots it scanned, counted apart from its work (SEC7-G1).
    pub fn propagate(&mut self, budget: usize, stop: &Stop<'_>) -> (bool, usize, bool, usize) {
        self.traverse_born();
        let carry = crate::nostd::mem::take(&mut self.pace_carry);
        self.probe_budget += budget as u64;
        let st = Prop { done: 0, next: ATOMIC_CHECK, redone: 0, slots: 0, pre: 0, given: budget, carry };
        self.propagate_paced(budget.saturating_sub(carry), stop, st)
    }

    /// `propagate_from`, then what its prepaid slots took past the budget carried to the
    /// next step (none once marking has nothing left). What it charges is its work alone:
    /// the prepaid slots were paid as the table grew (or by the rebuild that sent the
    /// traversal back), so they spend the budget without being charged again.
    fn propagate_paced(&mut self, limit: usize, stop: &Stop<'_>, st: Prop) -> (bool, usize, bool, usize) {
        let (given, carry) = (st.given, st.carry);
        let (empty, done, cut, slots, pre) = self.propagate_from(limit, stop, st);
        if !self.paused {
            // (Past the budget, what the work took is the caller's to carry, `gc_mark_over`;
            // what the prepaid slots took, this.)
            self.pace_carry = if empty { 0 } else { (done + pre + carry).saturating_sub(given).saturating_sub(done.saturating_sub(given)) };
        }
        (empty, done, cut, slots)
    }

    /// Goes on with the marking step that paused at the end of the host's poll (`paused`),
    /// inside its loop, as it stood: nothing it did is done again, its budget is the one it
    /// had. It looks at the interrupt and the deadline first (SEC7-G1).
    pub fn propagate_resume(&mut self, stop: &Stop<'_>) -> (bool, usize, bool, usize) {
        let Some((budget, mut st)) = self.prop_saved.take() else { return (false, 0, false, 0) };
        st.next = st.done + st.redone;
        self.propagate_paced(budget, stop, st)
    }

    fn propagate_from(&mut self, budget: usize, stop: &Stop<'_>, st: Prop) -> (bool, usize, bool, usize, usize) {
        // (A chunk a rebuild sent back over is not charged again: counted in `redone` for
        // the looks at the stop all the same; `slots`: the slots scanned, apart from the
        // work, `part_step`.)
        let Prop { mut done, mut next, mut redone, mut slots, mut pre, given, carry } = st;
        while done + pre < budget {
            if done + redone >= next {
                next = done + redone + ATOMIC_CHECK;
                if stop.interrupted() || stop.past_deadline() {
                    return (false, done, true, slots, pre);
                }
            }
            // The table traversed in chunks goes on first, chunk after chunk, as one
            // traversal of it whole did: when its last chunk comes then follows the work
            // alone. (Taking what its chunks mark first made that, and the order of the rest
            // among the script's writes, follow the layout, and with them the fuel.)
            if self.part != NO_PART {
                let (w, n, prepaid) = self.part_step();
                done += w;
                slots += n;
                pre += prepaid * GROWN_SLOT_WORK;
                self.probe_prepaid += prepaid as u64;
                if w == 0 {
                    redone += TABLE_CHUNK;
                }
                // Its slots too bound a step (`step_slots`): it ends here, its work done.
                if slots >= self.step_slots && self.part != NO_PART {
                    return (false, done, false, slots, pre);
                }
                // At the end of the host's poll, after a chunk (never inside one), a step
                // that may pause does: its state kept for `propagate_resume`.
                if self.pause_ok && done + pre < budget && self.poll_allow.is_some_and(|a| self.scanned.wrapping_sub(self.poll_start) >= a) {
                    self.prop_saved = Some((budget, Prop { done, next, redone, slots, pre, given, carry }));
                    self.paused = true;
                    return (false, done, false, slots, pre);
                }
            } else if let Some(g) = self.pop_gray() {
                let at = self.scanned;
                done += self.obj_work + self.traverse(g, None);
                slots += self.scanned.wrapping_sub(at);
                self.poll_objs += 1;
                // At the end of the host's poll, after an object too (SEC7-G1: a step of
                // 200,000 small objects took 9.5 ms between chunks).
                if done + pre < budget && self.poll_over() {
                    self.prop_saved = Some((budget, Prop { done, next, redone, slots, pre, given, carry }));
                    self.paused = true;
                    return (false, done, false, slots, pre);
                }
            } else if let Some(t) = self.part_queue.pop() {
                // (Its start was charged when it was reached, as for one started at once:
                // which it is follows the order tables are reached in.)
                self.begin_part(t);
            } else {
                return (true, done, false, slots, pre);
            }
        }
        (self.gray_empty() && self.part == NO_PART && self.part_queue.is_empty(), done, false, slots, pre)
    }

    /// Whether a step that may pause (`pause_ok`) is at the end of the host's poll: its
    /// slots scanned and objects traversed since the poll started reach `poll_allow`.
    #[inline]
    pub(crate) fn poll_over(&self) -> bool {
        self.pause_ok && self.poll_allow.is_some_and(|a| self.scanned.wrapping_sub(self.poll_start) + self.poll_objs >= a)
    }

    /// The objects made since the last step, traversed (paid for when they were made).
    fn traverse_born(&mut self) {
        while let Some(g) = self.born.list.pop() {
            self.traverse(g, None);
        }
    }

    /// Ends marking (roots already marked again by the caller): drains every gray object,
    /// settles ephemerons, clears weak tables, and starts the sweep.
    ///
    /// The atomic phase: the mutator must not run inside it, and its work grows with the
    /// heap (a weak table or ephemerons of a million entries take tens of milliseconds). It
    /// stops two ways:
    /// - after `allow` units of work (the fuel the running call has left): it is suspended
    ///   (`Atomic::Suspended`) where it stands, the call stops on its fuel limit, and the
    ///   next entry from the host resumes it before any script code runs (S6GC-2e: run in
    ///   one go, a script ran up to one whole atomic phase past its fuel, 460 ms over an
    ///   800,000-entry ephemeron chain, growing with the memory cap). Objects written while
    ///   it is suspended (by the host) are barriered as during marking and marked again
    ///   when it resumes.
    /// - on the host's interrupt, looked at every `ATOMIC_CHECK` units: the cycle is
    ///   abandoned (`abandon_cycle`), so a stuck host never keeps a half-done cycle.
    /// - at the time limit's deadline, looked at as often: suspended as at the allowance.
    pub fn atomic(&mut self, stop: &Stop<'_>, allow: usize) -> Atomic {
        // (Paused at the end of the host's poll: its counters as they stood.)
        let saved = self.atomic_saved.take();
        let [mut work, mut next, saved_next, saved_scanned, saved_looked] = saved.unwrap_or([0, ATOMIC_CHECK, 0, 0, 0]);
        let stage = match self.atomic_stage.take() {
            None => {
                let again = crate::nostd::mem::take(&mut self.grayagain);
                for g in again {
                    // Barriered objects are gray; force a traversal.
                    self.force_gray(g);
                }
                // Weak tables are traversed again too.
                let weak = crate::nostd::mem::take(&mut self.weak);
                for (t, _, _) in &weak {
                    self.force_gray(Gray::Table(*t));
                }
                // Room for the ephemerons waiting for their keys, taken now (at most an entry a
                // slot of the ephemeron tables known, at most the objects there are): grown
                // by doubling as they came, a step of 256 entries rehashed 300,000 of them,
                // 4.5 ms without a look at the deadline (S6GC-2e-R). Given back when the
                // cycle ends, as the rest of the collector's scratch.
                let eph: usize = weak
                    .iter()
                    .filter(|&&(_, wk, wv)| wk && !wv)
                    .map(|&(t, _, _)| self.tables.at(t as usize).map_or(0, Table::slots))
                    .sum();
                let room = eph.min(self.live_objects());
                self.eph_waiting.reserve(room);
                self.eph_links.reserve(room);
                // Threads are marked again (they were left gray).
                // (Skipping free slots: S5GC-2.)
                let mut threads: Vec<u32> = Vec::new();
                let mut i = self.threads.next_used(0);
                while i < self.threads.slots() {
                    if self.threads.color.get(i).is_some_and(|&c| c == GRAY || c == BLACK) {
                        threads.push(i as u32);
                    }
                    i = self.threads.next_used(i + 1);
                }
                work += threads.len();
                for t in threads {
                    self.gray.push(Gray::Thread(t));
                }
                // Ephemeron values wait for their keys from here on: marking a key marks them.
                self.eph_active = true;
                AtomicStage::Mark
            }
            Some(AtomicStage::Mark) => {
                // Resumed: what the host wrote while it was suspended is marked again.
                let again = crate::nostd::mem::take(&mut self.grayagain);
                for g in again {
                    self.force_gray(g);
                }
                AtomicStage::Mark
            }
            Some(clear) => clear,
        };
        let (weak, at) = match stage {
            AtomicStage::Mark => {
                self.traverse_born();
                // By the slots scanned too: a table's slots past its size when the cycle
                // started add no work (their marking was paid as it grew), so a piece of
                // them ran on to the next look, 3.6 ms between two reads of the deadline,
                // 18 ms in a call (SEC7-G1).
                // (Its own count: the clearing below looks by its work alone.)
                let (scanned, mut looked, mut mark_next) = if saved.is_some() { (saved_scanned, saved_looked, saved_next) } else { (self.scanned, self.scanned, next) };
                while let Some(g) = self.pop_gray() {
                    work += self.obj_work + self.traverse(g, None);
                    self.poll_objs += 1;
                    let seen = self.scanned.wrapping_sub(scanned);
                    let mut look = false;
                    if work + seen >= mark_next {
                        look = true;
                        mark_next = work + seen + ATOMIC_CHECK;
                        next = work + ATOMIC_CHECK;
                        self.most_between_looks = self.most_between_looks.max(self.scanned.wrapping_sub(looked));
                        looked = self.scanned;
                        if stop.interrupted() {
                            self.abandon_cycle();
                            return Atomic::Interrupted(work);
                        }
                        if stop.past_deadline() {
                            self.atomic_stage = Some(AtomicStage::Mark);
                            return Atomic::TimedOut(work);
                        }
                    }
                    if work > allow {
                        self.atomic_stage = Some(AtomicStage::Mark);
                        return Atomic::Suspended(work);
                    }
                    // At the end of the host's poll, at a look, after the allowance's test
                    // (as it stood, the resume goes on with the next object: SEC7-G1).
                    if look && self.poll_over() && !self.gray_empty() {
                        self.atomic_stage = Some(AtomicStage::Mark);
                        self.atomic_saved = Some([work, next, mark_next, scanned, looked]);
                        return Atomic::Paused;
                    }
                }
                self.eph_active = false;
                self.eph_waiting.clear();
                self.eph_links.clear();
                (crate::nostd::mem::take(&mut self.weak), (0, 0))
            }
            AtomicStage::Clear(weak, at, pos) => (weak, (at, pos)),
        };
        let (mut at, mut pos) = at;
        // Clear weak entries whose key or value was not marked.
        while let Some(&(t, wk, wv)) = weak.get(at) {
            let (w, more) = self.clear_weak(t, wk, wv, pos);
            work += w;
            self.poll_objs += w;
            match more {
                Some(p) => pos = p,
                None => {
                    at += 1;
                    pos = 0;
                }
            }
            let mut look = false;
            if work >= next {
                look = true;
                next = work + ATOMIC_CHECK;
                if stop.interrupted() {
                    // Tables not cleared yet must be before anything is swept: abandoning
                    // is the only safe stop here (clearing early is harmless: the entries
                    // cleared were unreachable).
                    self.abandon_cycle();
                    return Atomic::Interrupted(work);
                }
                if stop.past_deadline() && at < weak.len() {
                    self.atomic_stage = Some(AtomicStage::Clear(weak, at, pos));
                    return Atomic::TimedOut(work);
                }
            }
            if work > allow && at < weak.len() {
                self.atomic_stage = Some(AtomicStage::Clear(weak, at, pos));
                return Atomic::Suspended(work);
            }
            if look && self.poll_over() && at < weak.len() {
                self.atomic_saved = Some([work, next, 0, 0, 0]);
                self.atomic_stage = Some(AtomicStage::Clear(weak, at, pos));
                return Atomic::Paused;
            }
        }
        self.phase = Phase::Sweep;
        self.sweep = (0, 0);
        self.mem.marking = false;
        self.interner.peak = self.interner.peak.max(self.interner.count);
        self.trim_scratch();
        Atomic::Done(work)
    }

    /// The sweep's work units still to come in this cycle, at least (`Lua::collector_status`):
    /// a unit a used slot, for each slab the sweep has not reached (all of them while
    /// marking). The slab it is in, a dead interned string's walk and the interner's shrink
    /// are not counted: the floor, not the cost.
    pub(crate) fn sweep_floor(&self) -> usize {
        let from = match self.phase {
            Phase::Pause => return 0,
            Phase::Propagate => 0,
            Phase::Sweep => usize::from(self.sweep.0) + 1,
        };
        let used = |slots: usize, free: usize| slots.saturating_sub(free);
        let counts = [
            used(self.strings.slots(), self.strings.free.len()),
            used(self.tables.slots(), self.tables.free.len()),
            used(self.funcs.slots(), self.funcs.free.len()),
            used(self.upvals.slots(), self.upvals.free.len()),
            used(self.uds.slots(), self.uds.free.len()),
            used(self.threads.slots(), self.threads.free.len()),
            used(self.protos.slots(), self.protos.free.len()),
        ];
        counts.iter().skip(from).sum()
    }

    /// Whether an atomic phase is suspended (see `atomic`): it must be finished before any
    /// script code runs.
    #[inline]
    pub fn atomic_suspended(&self) -> bool {
        self.atomic_stage.is_some()
    }

    /// Gives up the cycle in progress: every object is white again (as after a sweep) and
    /// the next step starts a new cycle from the roots. Nothing is freed, so nothing a
    /// script or the host holds can go stale.
    pub fn abandon_cycle(&mut self) {
        fn whiten(color: &mut [u8]) {
            for c in color.iter_mut().filter(|c| **c != FREE) {
                *c = WHITE;
            }
        }
        whiten(&mut self.strings.color);
        whiten(&mut self.tables.color);
        whiten(&mut self.funcs.color);
        whiten(&mut self.upvals.color);
        whiten(&mut self.uds.color);
        whiten(&mut self.threads.color);
        whiten(&mut self.protos.color);
        self.end_part();
        self.part_queue.clear();
        self.gray.clear();
        self.ordered_gray.clear();
        self.born.list.clear();
        self.grayagain.clear();
        self.weak.clear();
        self.eph_waiting.clear();
        self.eph_links.clear();
        self.eph_active = false;
        self.atomic_stage = None;
        self.atomic_saved = None;
        self.phase = Phase::Pause;
        self.mem.marking = false;
        self.trim_scratch();
    }

    /// Gives back the collector's scratch buffers grown past `SCRATCH_KEEP` (S6GC-M1: kept
    /// at their peak, uncharged, they held 147 MB resident past the account after one
    /// cycle over a table of 2^23 slots).
    pub(crate) fn trim_scratch(&mut self) {
        trim_vec(&mut self.gray);
        self.ordered_gray.clear();
        trim_vec(&mut self.born.list);
        trim_vec(&mut self.grayagain);
        trim_vec(&mut self.weak);
        trim_vec(&mut self.eph_links);
        trim_vec(&mut self.trav_vals);
        trim_vec(&mut self.trav_ups);
        trim_vec(&mut self.mem.watch.moved);
        trim_vec(&mut self.part_queue);
        self.eph_waiting.clear();
        if self.eph_waiting.capacity() * 16 > SCRATCH_KEEP {
            self.eph_waiting = crate::nostd::collections::HashMap::with_hasher(*self.eph_waiting.hasher());
        }
    }

    /// Bytes of capacity the collector's scratch buffers hold (a test probe).
    pub(crate) fn scratch_bytes(&self) -> usize {
        fn b<T>(v: &Vec<T>) -> usize {
            v.capacity() * crate::nostd::mem::size_of::<T>()
        }
        (self.ordered_gray.pages.len() + usize::from(self.ordered_gray.active.is_some())) * crate::nostd::mem::size_of::<GrayPage>() + b(&self.gray) + b(&self.grayagain) + b(&self.weak) + b(&self.eph_links) + b(&self.trav_vals) + b(&self.trav_ups) + b(&self.mem.watch.moved) + b(&self.part_queue) + self.eph_waiting.capacity() * 16
    }

    fn force_gray(&mut self, g: Gray) {
        let c = match g {
            Gray::TablePart(..) => return,
            Gray::Proto(i) => self.protos.color.get_mut(i as usize),
            Gray::Table(i) => self.tables.color.get_mut(i as usize),
            Gray::Func(i) => self.funcs.color.get_mut(i as usize),
            Gray::Upval(i) => self.upvals.color.get_mut(i as usize),
            Gray::Ud(i) => self.uds.color.get_mut(i as usize),
            Gray::Thread(i) => self.threads.color.get_mut(i as usize),
        };
        if let Some(c) = c
            && *c != FREE
        {
            *c = GRAY;
            self.gray.push(g);
        }
    }

    /// Clears weak table `t`'s dead entries from slot `from` (array part, then hash
    /// nodes), at most `TABLE_CHUNK` slots: the work, and where to go on (None when done).
    fn clear_weak(&mut self, t: u32, wk: bool, wv: bool, from: usize) -> (usize, Option<usize>) {
        // Strings are values, not objects that go away: they count as marked.
        let dead = |h: &Heap, v: Value| v.is_collectable() && !matches!(v, Value::Str(_)) && !h.is_marked(v);
        let Some(tab) = self.tables.at(t as usize) else { return (0, None) };
        let alen = tab.arr.live_len();
        let total = alen + tab.hash_len();
        let to = from.saturating_add(TABLE_CHUNK).min(total);
        let mut kill_arr = Vec::new();
        let mut kill_node = Vec::new();
        for (at, part) in tab.arr.chunks(from..to).filter(|_| wv) {
            for (j, v) in part.iter().enumerate() {
                if dead(self, *v) {
                    kill_arr.push(at + j);
                }
            }
        }
        let (nodes_from, nodes_to) = (from.saturating_sub(alen), to.saturating_sub(alen));
        for (j, n) in tab.hash_nodes().enumerate().skip(nodes_from).take(nodes_to - nodes_from) {
            if n.val.is_nil() {
                continue;
            }
            if (wk && dead(self, n.key())) || (wv && dead(self, n.val)) {
                kill_node.push(j);
            }
        }
        let work = to.saturating_sub(from);
        if let Some(tab) = self.tables.at_mut(t as usize) {
            // (A write: a resize given up on it cannot go on, `table::RESUMABLE`.)
            if !(kill_arr.is_empty() && kill_node.is_empty()) {
                tab.absent.set(0);
                tab.unkept(&mut self.mem);
            }
            for j in kill_arr {
                if let Some(s) = tab.arr.get_mut(j) {
                    *s = Value::Nil;
                }
            }
            for j in kill_node {
                if let Some(n) = tab.hash_node_mut(j) {
                    n.val = Value::Nil;
                }
            }
        }
        if to < total {
            return (work, Some(to));
        }
        if let Some(c) = self.tables.color.get_mut(t as usize) {
            *c = BLACK;
        }
        (work, None)
    }

    /// Sweeps up to `budget` slots; true when the sweep is over. Returns the work done.
    /// (Stopped early by `stop`: the third is true.)
    pub fn sweep_step(&mut self, budget: usize, stop: &Stop<'_>) -> (bool, usize, bool) {
        let mut done = 0;
        // Slots cut from slabs' ends: not work (free slots change neither the pacing nor
        // the fuel), but counted to the next look at the stop.
        let mut cut_all = 0;
        let mut next = ATOMIC_CHECK;
        while done < budget {
            if done + cut_all >= next {
                next = done + cut_all + ATOMIC_CHECK;
                if stop.interrupted() || stop.past_deadline() {
                    return (false, done, true);
                }
            }
            let (slab, idx) = self.sweep;
            let len = match slab {
                0 => self.strings.slots(),
                1 => self.tables.slots(),
                2 => self.funcs.slots(),
                3 => self.upvals.slots(),
                4 => self.uds.slots(),
                5 => self.threads.slots(),
                6 => self.protos.slots(),
                _ => {
                    // The interner's slots given back when most of its strings died (SEC7-H4:
                    // kept at their peak, 4M slots stayed resident, 32 MB outside the
                    // account). The strings move a piece at a time as the sweep's work (a
                    // quarter unit a slot), over as many steps as it takes, the slots set
                    // aside counted meanwhile (a script runs between steps). (Those a regrow
                    // set aside move as new strings come, paid for by them: `regrow_share`.)
                    let i = &mut self.interner;
                    if i.old.is_empty() && i.slots.len() > SHRINK_FROM && i.peak * 8 < i.slots.len() {
                        i.regrow(&self.strings, ((i.count + 1) * 2).next_power_of_two().max(256), i.salt);
                        i.shrunk = i.old.capacity() * 8;
                        self.mem.used += i.shrunk;
                    }
                    i.peak = i.count;
                    if i.shrunk > 0 && !i.old.is_empty() {
                        i.move_some(&self.strings, TABLE_CHUNK);
                        done += TABLE_CHUNK / 4;
                        continue;
                    }
                    self.mem.used -= crate::nostd::mem::take(&mut i.shrunk).min(self.mem.used);
                    self.phase = Phase::Pause;
                    self.estimate = self.mem.live();
                    return (true, done, false);
                }
            };
            if idx >= len {
                // The slab's free tail is cut a piece at a time, with looks at the stop
                // between them; `usize::MAX` marks the cut under way (and keeps what the
                // script makes meanwhile white: this slab is swept).
                let (cut, more) = self.trim_piece(slab, idx == usize::MAX);
                cut_all += cut;
                self.sweep = if more { (slab, usize::MAX) } else { (slab + 1, 0) };
                continue;
            }
            // Work counts objects, not free slots: the same program costs the same fuel
            // however the slabs were left by earlier runs. Runs of free slots are skipped
            // (S5GC-2).
            match self.sweep_one(slab, idx) {
                0 => self.sweep.1 = self.next_used(slab, idx),
                w => {
                    done += w;
                    self.sweep.1 += 1;
                }
            }
        }
        (false, done, false)
    }

    /// `Slab::trim_piece` of slab number `slab`, `TABLE_CHUNK` slots at most.
    fn trim_piece(&mut self, slab: u8, all: bool) -> (usize, bool) {
        let m = &mut self.mem;
        match slab {
            0 => self.strings.trim_piece(m, all, TABLE_CHUNK),
            1 => self.tables.trim_piece(m, all, TABLE_CHUNK),
            2 => self.funcs.trim_piece(m, all, TABLE_CHUNK),
            3 => self.upvals.trim_piece(m, all, TABLE_CHUNK),
            4 => self.uds.trim_piece(m, all, TABLE_CHUNK),
            5 => self.threads.trim_piece(m, all, TABLE_CHUNK),
            _ => self.protos.trim_piece(m, all, TABLE_CHUNK),
        }
    }

    /// `Slab::next_used` of slab number `slab`.
    fn next_used(&self, slab: u8, from: usize) -> usize {
        match slab {
            0 => self.strings.next_used(from),
            1 => self.tables.next_used(from),
            2 => self.funcs.next_used(from),
            3 => self.upvals.next_used(from),
            4 => self.uds.next_used(from),
            5 => self.threads.next_used(from),
            _ => self.protos.next_used(from),
        }
    }

    /// Sweeps one slot: the work done (0 when it was free; an interned string's removal
    /// from the interner walks its run, S5GC-3).
    fn sweep_one(&mut self, slab: u8, i: usize) -> usize {
        macro_rules! sweep {
            ($s:expr, $size:expr) => {{
                if $s.color.get(i) == Some(&FREE) {
                    return 0;
                }
                match $s.color.get(i).copied() {
                    Some(WHITE) => {
                        let size = $s.at(i).map($size).unwrap_or(0);
                        self.mem.release(size);
                        $s.free_slot(i, &mut self.mem);
                    }
                    Some(GRAY) | Some(BLACK) => {
                        if let Some(c) = $s.color.get_mut(i) {
                            *c = WHITE;
                        }
                    }
                    _ => {}
                }
            }};
        }
        let mut work = 1;
        match slab {
            0 => {
                if self.strings.color.get(i) == Some(&WHITE) && self.strings.at(i).is_some_and(|s| s.interned) {
                    work += self.unintern(i as u32);
                }
                sweep!(self.strings, |s: &LStr| str_mem(s.bytes.len()))
            }
            1 => {
                // A dead table's buffers go to the pool (after its size is released).
                if self.tables.color.get(i) == Some(&WHITE) {
                    let size = self.tables.at(i).map_or(0, Table::mem_size);
                    self.mem.release(size);
                    if let Some(t) = self.tables.at_mut(i) {
                        if t.partial != 0 {
                            crate::table::drop_partial(&mut self.mem, crate::nostd::mem::take(&mut t.partial));
                        }
                        self.mem.pool_give(t);
                    }
                    self.tables.free_slot(i, &mut self.mem);
                } else {
                    sweep!(self.tables, |t: &Table| t.mem_size())
                }
            }
            2 => sweep!(self.funcs, |f: &Func| FUNC_BASE
                + match f {
                    Func::Lua { upvals, .. } => boxed(upvals.len() * 8),
                    Func::Native { upvals, .. } => boxed(upvals.len() * 16),
                    Func::Host { bytes, .. } => *bytes,
                }),
            3 => sweep!(self.upvals, |_: &Upval| UPVAL_SIZE),
            4 => {
                // A userdata the host asked about is reported (its key queued; nothing runs).
                if self.uds.color.get(i) == Some(&WHITE)
                    && let Some(key) = self.uds.at(i).and_then(|u| u.notify)
                {
                    self.collected.push(key);
                }
                sweep!(self.uds, |u: &UserData| UD_BASE + u.size)
            }
            5 => sweep!(self.threads, |t| t.mem_size()),
            6 => sweep!(self.protos, |p: &Proto| p.mem_size()),
            _ => {}
        }
        work
    }

    /// Gives back the stack of coroutines that are not running: a suspended one keeps only
    /// its top frame's `frame_size` (its register window, 256 slots, is made again when it
    /// is resumed), a dead one nothing. Parked coroutines then cost about what C Lua's do,
    /// while a running frame keeps its full window (no bounds checks in the fast loop).
    pub fn shrink_parked(&mut self) {
        let Heap { threads, protos, mem, .. } = self;
        // (Skipping free slots: S5GC-2.)
        let mut next = threads.next_used(0);
        while next < threads.slots() {
            let i = next;
            next = threads.next_used(i + 1);
            let c = threads.color.get(i).copied();
            let (Some(t), Some(c)) = (threads.at_mut(i), c) else { break };
            if c == FREE || t.fresh {
                continue;
            }
            shrink_frames(&mut t.frames, mem);
            if t.status == CoStatus::Dead && t.frames.capacity() > 0 && t.frames.is_empty() {
                // Its frame records too (S5GC-7).
                mem.release(t.frames.capacity() * FRAME_BYTES);
                t.frames = Vec::new();
            }
            let want = match parked_live(t, protos) {
                Some(0) => 0,
                Some(live) => live + crate::vm::EXTRA_STACK,
                None => continue,
            };
            let cap = t.stack.capacity();
            if cap < want * 2 + 16 {
                continue;
            }
            // S6GC-H2: into a fresh allocation of exactly that size, not `shrink_to`: the
            // allocator shrinks in place and leaves the window's tail as a hole too small
            // for the next coroutine's window, so parked coroutines kept ~5x the memory cap
            // resident. A copy frees the whole old chunk for the next window to reuse.
            let keep = want.min(t.stack.len());
            let mut fresh = Vec::with_capacity(want);
            fresh.extend_from_slice(t.stack.get(..keep).unwrap_or_default());
            t.stack = fresh;
            mem.release(cap.saturating_sub(t.stack.capacity()) * SLOT_SIZE);
        }
    }

    /// Cuts every free slot at the end of each slab (after a full collection: SEC-M5).
    /// A piece at a time, with a look at `stop` between pieces: true when it said to stop
    /// (SEC7-H3: a tail under a quarter of its slab was cut in one go, 12 to 16 ms).
    pub fn trim_slabs(&mut self, stop: &Stop<'_>) -> bool {
        for slab in 0..7 {
            while self.trim_piece(slab, true).1 {
                if stop.interrupted() || stop.past_deadline() {
                    return true;
                }
            }
        }
        false
    }

    /// Work units a step does for the debt accumulated.
    pub fn step_budget(&self) -> usize {
        (self.mem.debt.max(STEP_SIZE).saturating_mul(self.stepmul as isize) / 800).max(64) as usize
    }
}

/// Where the live part of a coroutine's stack ends while it is not running (C Lua's
/// `L->top` for it); None for a thread whose whole stack counts (running, normal, fresh).
///
/// A suspended one needs its top frame's registers, the slot its yield's results go to,
/// and its open upvalues and pending to-be-closed variables; a dead one only the last two
/// (S5B-SOAK-1: one that died with an error keeps its pending to-be-closed variables for
/// `coroutine.close`, as Lua 5.4 does not unwind it). Varargs sit below their frame's base.
pub(crate) fn parked_live(t: &ThreadState, protos: &Slab<Proto>) -> Option<usize> {
    if t.fresh {
        return None;
    }
    let mut live = match t.status {
        CoStatus::Dead => 0,
        CoStatus::Suspended => {
            let f = t.frames.last()?;
            let p = protos.get(f.proto)?;
            let mut live = f.base + p.frame_size();
            if let Some(y) = t.yield_pending {
                live = live.max(y.func + 1);
            }
            live
        }
        _ => return None,
    };
    if let Some(&(i, _)) = t.open_upvals.last() {
        live = live.max(i + 1);
    }
    if let Some(&i) = t.tbc.last() {
        live = live.max(i + 1);
    }
    Some(live)
}

/// Everything a thread's state references, of its stack the first `live` slots; returns
/// how many stack slots, frames and open upvalues it walked (all of its stack counts, so
/// the work is the same whatever part is live). S5B-WALK-1 (found again by the cost fuzzer as S5B-COST-1): the work
/// counted was only the objects found, so a stack of a million integers (a vararg frame, a suspended coroutine) was walked twice a cycle
/// for one unit, and with a short pause (a cycle every few kilobytes allocated) a script
/// ran at 100x to 500x plain code per unit of fuel, growing with the stack it kept.
fn collect_thread(t: &ThreadState, live: usize, todo: &mut Vec<Value>, protos: &mut Vec<u32>, ups: &mut Vec<u32>) -> usize {
    let stack = t.stack.get(..live.min(t.stack.len())).unwrap_or_default();
    todo.extend(stack.iter().copied().filter(|v| v.is_collectable()));
    for f in &t.frames {
        todo.push(Value::Func(f.closure));
        protos.push(f.proto);
        if let Some(h) = f.handler {
            todo.push(h);
        }
    }
    ups.extend(t.open_upvals.iter().map(|(_, u)| *u));
    if let Some(r) = t.resumer {
        todo.push(Value::Thread(r));
    }
    if let Some(e) = t.error {
        todo.push(e);
    }
    if let Some((_, _, f)) = t.died_in {
        todo.push(f);
    }
    t.stack.len() + t.frames.len() + t.open_upvals.len()
}

/// Collector work an ephemeron entry costs in the atomic step: its key's list in
/// `eph_waiting` looked up and extended, and the lookup when its key is traversed.
///
/// S5B-COST-3: an entry was one unit of work like a strong table's, but the atomic step
/// hashes its key into `eph_waiting` (SipHash, so a script cannot aim keys at one bucket)
/// once to wait and once more when the key is traversed, each a cache miss in a big map:
/// `collectgarbage('step')` over a weak-keyed table ran at 5x plain code per unit of fuel
/// at 100,000 entries and 13x at 800,000 (a strong table 5x at 800,000). Charged by entry,
/// waiting or not: which entries wait depends on the order the hash parts are walked, so
/// on the seed, and fuel must not.
const EPH_ENTRY_WORK: usize = 12;

/// The most slots of one table a traversal or a weak table's clearing does in one go
/// (S6GC-2e): a table of 800,000 entries took 30 to 50 ms whole.
const TABLE_CHUNK: usize = 4096;
/// The table slots a step of an incremental cycle scans at most (`Heap::step_slots`).
pub(crate) const STEP_SLOTS: usize = 16 * TABLE_CHUNK;
/// Interner slots past which a sweep gives most of them back when an eighth or fewer are
/// in use (`sweep_step`).
const SHRINK_FROM: usize = 1 << 12;

/// Collector work for reaching an object at all, on top of its slots (S6GC-2), by the
/// heap's size when the cycle starts (`obj_work`): a list of small tables linked in an
/// order unrelated to their slots costs a cache and TLB miss per link, and at `1 + slots`
/// work marking 2^19 of them ran at 20x plain code per unit of fuel (30x at 2^21, growing
/// with the heap), while a small heap in the caches costs a fraction of that. Follows the
/// count of object slots only, so fuel stays independent of the hash seed and the layout.
/// (The top bucket was 16, set against plain code that the fast loop has since made faster
/// (about 12 ns a unit here): `sweep6::s6gc_2_marking_scattered_objects_is_paid` then sat
/// at 3 to 4.5x and failed its 5x one run in three. At 20 it is 1.6 to 2.3x.)
fn obj_work(slots: usize) -> usize {
    match slots {
        0..=0x7fff => 2,
        0x8000..=0x1_ffff => 4,
        0x2_0000..=0x3_ffff => 8,
        _ => 20,
    }
}

/// Entries of an ephemeron table the atomic phase traverses in one go (S6GC-2e-R: 4,096
/// of them were 300,000 units, 3 to 5 ms without a look at the deadline).
const EPH_CHUNK: usize = 256;

/// Slots of a table collected into `trav_vals` at a time (S6GC-M1).
const TRAV_CHUNK: usize = 4096;

/// Bytes of capacity a collector scratch buffer keeps past a cycle (S6GC-M1): the gray
/// list and the ephemeron lists grow with the heap and are not charged, so past this they
/// are given back when the cycle ends instead of being kept at their peak.
const SCRATCH_KEEP: usize = 64 * 1024;

/// `v`, emptied, given back when its capacity is past `SCRATCH_KEEP`.
fn trim_vec<T>(v: &mut Vec<T>) {
    v.clear();
    if v.capacity() * crate::nostd::mem::size_of::<T>() > SCRATCH_KEEP {
        *v = Vec::new();
    }
}

/// Seed number `n` of a state made from `base` (its string, table and random seeds).
pub(crate) fn mix_seed(base: u64, n: u64) -> u64 {
    let mut h = base ^ n.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    h ^= h >> 33;
    h = h.wrapping_mul(0xff51_afd7_ed55_8ccd);
    h ^= h >> 33;
    h = h.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
    h ^ (h >> 33)
}

/// Seed number `n` of a state made from `base`, for a seed a script can read back (the
/// random generator's): `mix_seed` is a bijection a script can run backwards to `base`
/// and so to the hash seeds (S5GC-4). Two independent mixes folded through a 128-bit
/// product have no such inverse.
pub(crate) fn oneway_seed(base: u64, n: u64) -> u64 {
    fold_mul(mix_seed(base, n), mix_seed(base, n ^ 0x5a5a_0000) | 1)
}

/// A process-unique seed for string hashing, so hash-flooding inputs differ per heap.
pub(crate) fn random_seed() -> u64 {
    use crate::nostd::hash::{BuildHasher, Hasher};
    let mut h = crate::nostd::collections::hash_map::RandomState::new().build_hasher();
    h.write_u64(0x5eed);
    h.finish()
}

#[cfg(test)]
mod tests {
    use crate::{Libs, Limits, Lua};

    /// SEC-M11: under a seed a script knows (the reference says to keep a fixed seed secret;
    /// S5B-OPEN-2 bounds what aimed keys cost), two long strings of one hash and length that
    /// differ only at their end. Looking the uninterned one up compared it with the interned
    /// one to that end at every lookup (S6T-KEY's settling records a twin only when equal),
    /// free: 4 MiB a lookup for a unit or two. The bytes compared on a miss are charged now.
    #[test]
    fn a_long_key_compared_and_missed_pays_for_the_bytes() {
        // Past `COPY_PIECE` (settled a piece at a time) and under it (the interner's lookup).
        for len in [4 << 20, (1 << 20) - 64, 64 << 10] {
            long_key_miss(len);
        }
    }

    fn long_key_miss(len: usize) {
        let mut fuels = Vec::new();
        for (fast, get) in [(true, "t[b]"), (false, "t[b]"), (true, "rawget(t, b)"), (false, "rawget(t, b)")] {
            fuels.push(long_key_miss_in(len, fast, get));
        }
        assert!(fuels[0] == fuels[1] && fuels[2] == fuels[3], "{len}: fast and full differ: {fuels:?}");
    }

    fn long_key_miss_in(len: usize, fast: bool, get: &str) -> u64 {
        let mut lua = Lua::new(Limits::default().memory(256 << 20).fuel(1 << 40).hash_seed(0x5eed), Libs::ALL);
        lua.set_fast_loop(fast);
        let seed = lua.heap.seed;
        let fill = vec![b'x'; len - 16];
        let hp = super::hash_words(seed, super::hash_start(seed, len), &fill);
        let (k1, k2) = (seed ^ 0xa076_1d64_78bd_642f, seed.rotate_left(32) ^ 0xe703_7ed1_a0b4_28db);
        let make = |i: u64| {
            let w1 = 0x4141_4141_0000_0000 | i;
            let h1 = super::fold_mul(hp ^ w1 ^ k1, k2);
            let w2 = h1 ^ 0x1234_5678_9abc_def0;
            let mut b = fill.clone();
            b.extend_from_slice(&w1.to_le_bytes());
            b.extend_from_slice(&w2.to_le_bytes());
            b
        };
        let (a, b) = (make(1), make(2));
        assert_eq!(super::hash_bytes(seed, &a), super::hash_bytes(seed, &b), "one hash");
        let f = lua.load(format!("local t, a, b = ... t[a] = true local n = 0 for i = 1, 200 do if {get} then n = n + 1 end end return n"), "=t").unwrap();
        let t = lua.create_table().unwrap();
        let sa = lua.create_string(&a).unwrap();
        let sb = lua.create_string(&b).unwrap();
        let before = lua.fuel_left();
        let r = lua.call(f.value(), &[crate::Value::Table(t), sa, sb]).unwrap();
        assert_eq!(r, [crate::Value::Int(0)]);
        let per_lookup = (before - lua.fuel_left()) / 200;
        assert!(per_lookup >= (len / 1024) as u64, "{get}, fast {fast}: {per_lookup} fuel a lookup comparing {len} bytes");
        per_lookup
    }

    /// SEC4-10: a slot cut off by `trim` and made again never gets back a generation a
    /// stale handle holds, even when generations wrap past `u32::MAX` (the old floor
    /// saturated there, so every slot made again had the same generation).
    #[test]
    fn trimmed_slots_never_reuse_a_generation() {
        let mut s: super::Slab<u8> = super::Slab::new(u32::MAX - 1);
        let mut m = super::Mem::new(usize::MAX, 0);
        let n = 4096;
        let mut stale = Vec::new();
        for round in 0..4 {
            let handles: Vec<(u32, u32)> = (0..n).map(|_| s.alloc(round, &mut m, |_| super::WHITE)).collect();
            for &(i, _) in handles.iter().rev() {
                s.free_slot(i as usize, &mut m);
            }
            s.trim(&mut m, round % 2 == 0);
            assert!(s.slots() < n, "trimmed");
            // SEC-M5: nothing of the cut slots stays (their generations neither).
            assert_eq!((s.gens.len(), m.slack, m.used), (s.slots(), 0, 0), "round {round}");
            stale.extend(handles);
            for &(i, g) in &stale {
                assert!(!s.live(i, g), "round {round}: stale handle ({i}, {g}) live after trim");
            }
        }
        // Made again after the trims: no stale handle matches a new object.
        let fresh: Vec<(u32, u32)> = (0..n).map(|_| s.alloc(9, &mut m, |_| super::WHITE)).collect();
        for &(i, g) in &stale {
            assert!(!fresh.contains(&(i, g)), "stale ({i}, {g}) matches a new object");
        }
    }

    /// SEC-M5: past `FLOOR_LIMIT` the cut slots' generations are kept, and counted, and
    /// still no stale handle matches; a slot made again takes its generation out of the
    /// account.
    #[test]
    fn cut_generations_kept_past_the_floor_limit_are_counted() {
        let mut s: super::Slab<u8> = super::Slab::new(7);
        s.floor = s.origin.wrapping_add(super::FLOOR_LIMIT - 3);
        let mut m = super::Mem::new(usize::MAX, 0);
        let n = 3000;
        let mut stale = Vec::new();
        for round in 0..6 {
            let handles: Vec<(u32, u32)> = (0..n).map(|_| s.alloc(1, &mut m, |_| super::WHITE)).collect();
            for &(i, _) in &handles {
                s.free_slot(i as usize, &mut m);
            }
            s.trim(&mut m, true);
            assert_eq!(s.slots(), 0, "round {round}");
            let kept = s.gens.len();
            assert_eq!((m.slack, m.used), (kept * super::GEN_BYTES, kept * super::GEN_BYTES), "round {round}");
            stale.extend(handles);
            for &(i, g) in &stale {
                assert!(!s.live(i, g), "round {round}: stale handle ({i}, {g}) live");
            }
        }
        assert_eq!(s.gens.len(), n, "the cut generations are kept");
        let fresh: Vec<(u32, u32)> = (0..n).map(|_| s.alloc(9, &mut m, |_| super::WHITE)).collect();
        assert_eq!((m.slack, m.used), (0, 0));
        for &(i, g) in &stale {
            assert!(!fresh.contains(&(i, g)), "stale ({i}, {g}) matches a new object");
        }
    }

    /// S6T-SLAB: an `Ahead` holds what a `Vec` given the same pushes, writes, cuts and
    /// shrinks holds, across many growths past `AHEAD_FROM`.
    #[test]
    fn ahead_holds_what_a_vec_holds() {
        let mut a: super::Ahead<u32> = super::Ahead::new();
        let mut v: Vec<u32> = Vec::new();
        let mut x = 0x9e37_79b9_7f4a_7c15u64;
        let mut rnd = || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        for round in 0..60 {
            for _ in 0..(rnd() % 40_000) {
                let n = rnd() as u32;
                a.push(n);
                v.push(n);
            }
            for _ in 0..(rnd() % 2000) {
                let i = (rnd() % (v.len() as u64 + 1)) as usize;
                let n = rnd() as u32;
                a.set(i, n);
                if let Some(y) = v.get_mut(i) {
                    *y = n;
                }
            }
            match rnd() % 8 {
                0 => {
                    let n = (rnd() % (v.len() as u64 + 1)) as usize;
                    a.truncate(n);
                    v.truncate(n);
                }
                1 => {
                    let n = v.len() + v.len() / 2;
                    a.shrink_to(n);
                }
                _ => {}
            }
            assert_eq!(&*a, &v[..], "round {round}");
        }
    }

    /// S6T-SLAB (high): a slab's per-slot vectors grew by doubling, so the push that made a
    /// slab of 1M strings one bigger copied their generations and free-list positions at
    /// once (5 ms, past the host's interrupt). Now, whenever a push finds one full, its copy
    /// of twice the room lacks at most a few elements (no push copies more than a stretch),
    /// and through allocations, frees and cuts every live handle stays live, every free
    /// slot's position is right, and the account holds the free slots' bytes.
    #[test]
    fn slab_slots_grow_a_stretch_at_a_time() {
        let mut s: super::Slab<u8> = super::Slab::new(1);
        let mut m = super::Mem::new(usize::MAX, 0);
        let mut x = 0x2545_f491_4f6c_dd1du64;
        let mut rnd = || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        let mut live: Vec<(u32, u32)> = Vec::new();
        let mut growths = 0;
        for round in 0..24 {
            for _ in 0..(rnd() % 60_000) {
                for v in [&s.gens, &s.free_at] {
                    if v.len() == v.capacity() && v.capacity() >= super::AHEAD_FROM {
                        assert!(
                            v.next.len() + super::AHEAD_EVERY > v.len(),
                            "round {round}: a full vector's copy lacks {} elements",
                            v.len() - v.next.len()
                        );
                        growths += 1;
                    }
                }
                live.push(s.alloc(0, &mut m, |_| super::WHITE));
            }
            for _ in 0..(rnd() % (live.len() as u64 / 2 + 1)) {
                let k = (rnd() % live.len() as u64) as usize;
                let (i, _) = live.swap_remove(k);
                s.free_slot(i as usize, &mut m);
            }
            if round % 4 == 3 {
                s.trim(&mut m, round % 8 == 7);
            }
            for &(i, g) in &live {
                assert!(s.live(i, g), "round {round}: ({i}, {g}) live");
            }
            for (pos, &i) in s.free.iter().enumerate() {
                assert_eq!(s.free_at.get(i as usize), Some(&(pos as u32)), "round {round}: free_at");
            }
            assert_eq!(
                m.slack,
                s.free.len() * s.slot_bytes + (s.gens.len() - s.slots()) * super::GEN_BYTES,
                "round {round}: slack"
            );
        }
        assert!(growths >= 2, "{growths} growths past AHEAD_FROM");
    }

    /// S5GC-2: `next_used` finds what a slot-by-slot walk finds, through allocations,
    /// frees and trims.
    #[test]
    fn next_used_skips_exactly_the_free_slots() {
        let mut s: super::Slab<u8> = super::Slab::new(1);
        let mut m = super::Mem::new(usize::MAX, 0);
        let mut x = 0x2545_f491_4f6c_dd1du64;
        let mut rnd = || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        for round in 0..40 {
            for _ in 0..(rnd() % 9000) {
                s.alloc(0, &mut m, |_| super::WHITE);
            }
            let len = s.slots();
            for _ in 0..(rnd() % (len as u64 + 1)) {
                let i = (rnd() % len as u64) as usize;
                if s.color.get(i) != Some(&super::FREE) {
                    s.free_slot(i, &mut m);
                }
            }
            if round % 3 == 0 {
                s.trim(&mut m, round % 2 == 0);
            }
            let len = s.slots();
            // S5B-WALK-2: the end of the used slots, from the bitmap.
            let end = (0..len).rev().find(|&i| s.color.get(i) != Some(&super::FREE)).map_or(0, |i| i + 1);
            assert_eq!(s.end_of_used(), end, "round {round}");
            // SEC-M5: the account holds exactly the free slots' bytes.
            assert_eq!(
                m.slack,
                s.free.len() * s.slot_bytes + (s.gens.len() - s.slots()) * super::GEN_BYTES,
                "round {round}: slack"
            );
            for (pos, &i) in s.free.iter().enumerate() {
                assert_eq!(s.free_at.get(i as usize), Some(&(pos as u32)), "round {round}: free_at");
            }
            if round % 6 == 0 {
                assert_eq!(s.end_of_used(), len, "round {round}: trim cuts every free slot at the end");
            }
            assert_eq!(s.free.len(), (0..len).filter(|&i| s.color.get(i) == Some(&super::FREE)).count(), "round {round}");
            for from in (0..=len + 70).step_by(1 + (rnd() % 37) as usize) {
                let want = (from..len).find(|&i| s.color.get(i) != Some(&super::FREE)).unwrap_or(len);
                assert_eq!(s.next_used(from), want, "round {round} from {from}");
            }
        }
    }

    /// At the memory cap the slot recovered by a table constructor counts toward
    /// that constructor alone. A failed attempt must leave it available for retry.
    #[test]
    fn a_reused_table_slot_only_needs_room_for_net_growth() {
        let mut lua = Lua::new(Limits::default(), Libs::ALL);
        let old = lua.heap.new_table(0, 0).unwrap();
        let keep = lua.heap.new_table(0, 0).unwrap();
        let _root = lua.root(crate::Value::Table(keep));
        lua.collect_garbage().unwrap();
        let h = &mut lua.heap;
        assert!(!h.tables.live(old.idx, old.generation));
        assert_eq!(h.tables.free.last(), Some(&old.idx));

        let (used, debt, slack) = (h.mem.used, h.mem.debt, h.mem.slack);
        let cost = crate::table::new_cost(16, 0);
        let exact = used - h.tables.slot_bytes + cost;
        h.mem.cap = exact - 1;
        assert!(h.new_table(16, 0).is_err());
        assert_eq!((h.mem.used, h.mem.debt, h.mem.slack), (used, debt, slack));
        assert_eq!(h.tables.free.last(), Some(&old.idx));

        h.mem.cap = exact;
        let new = h.new_table(16, 0).unwrap();
        assert_eq!(new.idx, old.idx);
        assert_ne!(new.generation, old.generation);
        assert_eq!(h.mem.used, exact);
        assert_eq!(h.mem.slack, slack - h.tables.slot_bytes);
        assert_eq!(h.mem.debt, debt + cost as isize);
        assert!(h.new_table(16, 0).is_err());
    }

    /// Dead tables' buffers are reused, and a full collection empties the pool, so the
    /// process's memory returns too; the account returns to its baseline throughout.
    #[test]
    fn table_buffer_pool_fills_and_empties() {
        let mut lua = Lua::new(Limits::default(), Libs::ALL);
        lua.exec("local x = 0 for i = 1, 3 do x = x + i end", "=t").unwrap();
        lua.collect_garbage().unwrap();
        let base = lua.memory_used();
        // Tables that die, then incremental steps until a cycle ends: their buffers are kept.
        lua.exec("keep = {} for i = 1, 2000 do keep[i] = {i, i, {x = i}} end keep = nil", "=t").unwrap();
        lua.exec("for _ = 1, 2 do repeat until collectgarbage('step') end", "=t").unwrap();
        assert!(lua.heap.mem.pool.held() > 0);
        lua.collect_garbage().unwrap();
        assert_eq!(lua.heap.mem.pool.held(), 0);
        assert_eq!(lua.memory_used(), base);
    }

    /// The pool's bytes count as used (not as live, and not in `memory_used`), and a charge
    /// gives them back before it is refused: a charge of exactly the room left without the
    /// pool succeeds, emptying it, and one byte more is refused, as with no pool.
    #[test]
    fn table_buffer_pool_is_charged_and_given_back() {
        let mut lua = Lua::new(Limits::default().memory(4 << 20), Libs::ALL);
        lua.exec("for r = 1, 3 do local t = {} for i = 1, 3000 do t[i] = {i, i} end end", "=t").unwrap();
        lua.exec("for _ = 1, 2 do repeat until collectgarbage('step') end", "=t").unwrap();
        let pooled = lua.heap.mem.pool.bytes;
        assert!(pooled > 0);
        assert_eq!(lua.heap.mem.used, lua.memory_used() + pooled);
        let room = lua.heap.mem.cap + lua.heap.mem.headroom - lua.memory_used();
        assert!(!lua.heap.mem.fits(room + 1));
        assert_eq!(lua.heap.mem.pool.bytes, 0);
        lua.exec("for _ = 1, 2 do repeat until collectgarbage('step') end", "=t").unwrap();
        lua.exec("local t = {} for i = 1, 3000 do t[i] = {i, i} end t = nil for _ = 1, 2 do repeat until collectgarbage('step') end", "=t").unwrap();
        assert!(lua.heap.mem.pool.bytes > 0);
        let room = lua.heap.mem.cap + lua.heap.mem.headroom - lua.memory_used();
        assert!(lua.heap.mem.charge(room).is_ok());
        assert_eq!(lua.heap.mem.pool.bytes, 0);
        assert!(lua.heap.mem.charge(1).is_err());
    }

    /// The pool is bounded: `POOL_BYTES`, or 1/16 of the memory limit when that is less
    /// (SEC3-16), and its bytes count as used.
    #[test]
    fn table_buffer_pool_is_bounded() {
        for (cap, most) in [(512 << 20, super::POOL_BYTES), (256 << 10, (256 << 10) / 16)] {
            let mut lua = Lua::new(Limits::default().memory(cap), Libs::ALL);
            for _ in 0..20 {
                lua.exec("local t = {} for i = 1, 400 do t[i] = {1, 2, 3, x = 1, y = 2, z = 3} end", "=t").unwrap();
                lua.exec("for _ = 1, 2 do repeat until collectgarbage('step') end", "=t").unwrap();
                assert!(lua.heap.mem.pool.bytes <= most, "{} > {most}", lua.heap.mem.pool.bytes);
                assert!(lua.heap.mem.slack >= lua.heap.mem.pool.bytes);
            }
            assert!(lua.heap.mem.pool.held() > 0);
        }
    }
}

/// The hasher of `Heap::eph_waiting` (keys are slab indices): one multiply and a final mix,
/// keyed by the state's seed so a script cannot aim indices at one bucket. SipHash was a
/// fifth of the atomic step's instructions over a big ephemeron table.
#[derive(Clone, Copy)]
pub(crate) struct EphHash(u64);

impl crate::nostd::hash::BuildHasher for EphHash {
    type Hasher = EphHasher;
    fn build_hasher(&self) -> EphHasher {
        EphHasher(self.0)
    }
}

pub(crate) struct EphHasher(u64);

impl crate::nostd::hash::Hasher for EphHasher {
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.write_u8(b);
        }
    }
    fn write_u8(&mut self, n: u8) {
        self.0 = (self.0 ^ u64::from(n)).wrapping_mul(0x9e37_79b9_7f4a_7c15);
    }
    fn write_u32(&mut self, n: u32) {
        self.0 = (self.0 ^ u64::from(n)).wrapping_mul(0x9e37_79b9_7f4a_7c15);
    }
    fn finish(&self) -> u64 {
        mix_seed(self.0, 1)
    }
}

/// Where a suspended atomic phase stands (`Heap::atomic`): still marking (ephemerons
/// waiting for their keys), or clearing weak tables from the one at an index, from a slot.
pub(crate) enum AtomicStage {
    Mark,
    Clear(Vec<(u32, bool, bool)>, usize, usize),
}

/// What stops the collector's work part-way, looked at every `ATOMIC_CHECK` units of it:
/// the host's interrupt, and the deadline of its time limit (S6GC-2e-R: the atomic phase
/// and a long marking step read neither clock nor deadline, so a call ran 3 to 7 ms past
/// its time limit between two reads of it).
pub(crate) struct Stop<'a> {
    pub interrupt: &'a crate::nostd::sync::atomic::AtomicBool,
    pub deadline: Option<(crate::nostd::time::Duration, &'a crate::Clock)>,
}

impl Stop<'_> {
    fn interrupted(&self) -> bool {
        self.interrupt.load(crate::nostd::sync::atomic::Ordering::Relaxed)
    }

    fn past_deadline(&self) -> bool {
        self.deadline.is_some_and(|(at, clock)| clock() >= at)
    }
}

/// How an atomic phase step ended, with the work it did.
pub(crate) enum Atomic {
    /// Marking is over: the sweep starts.
    Done(usize),
    /// Stopped at its allowance; resumed by the next call (`Heap::atomic`).
    Suspended(usize),
    /// Stopped at the time limit's deadline: suspended as at the allowance, the call stops
    /// on its time limit.
    TimedOut(usize),
    /// The host's interrupt: the cycle was abandoned.
    Interrupted(usize),
    /// Paused at the end of the host's poll (`Heap::poll_over`), its counters kept
    /// (`atomic_saved`): nothing charged; it goes on as it stood (SEC7-G1).
    Paused,
}
