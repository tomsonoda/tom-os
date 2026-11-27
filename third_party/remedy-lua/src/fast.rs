//! The interpreter's fast loop: the common cases of the common instructions, run with
//! the heap borrowed shared and the running thread's stack borrowed exclusively, so the
//! current function's code, constants and register window are plain locals for the
//! whole stretch, with no lookups per instruction.
//!
//! Anything it cannot finish here (a call, a return, a metamethod, a write that may need
//! the collector's barrier, an error, the fuel running out) it leaves undone: it returns
//! with `pc` at that instruction and the fuel not charged for it, and `Lua::run` executes
//! that one instruction in full before coming back.

#[allow(unused_imports)]
use crate::nostd::prelude::*;
use crate::bytecode::{ArithOp, Instr, MULTI, Rk, WINDOW};
use crate::heap::{BLACK, Func, GRAY, Gray, Heap, Phase, ThreadState, Upval};
use crate::number;
use crate::table::Table;

/// Slot `i` of `t`'s array part, for the loop's reads and writes: on the contiguous build
/// the slice's own `get`, as the loop was compiled before `ArrPart`'s accessors (through
/// `get` the loop's code generation moved: fannkuch +2.5% Ir); `ArrPart::get` where the
/// storage may be segmented.
#[cfg(not(any(target_family = "wasm", feature = "segmented-arrays")))]
macro_rules! arr_slot {
    ($t:expr, $i:expr) => {
        $t.arr.live().get($i)
    };
}
#[cfg(any(target_family = "wasm", feature = "segmented-arrays"))]
macro_rules! arr_slot {
    ($t:expr, $i:expr) => {
        $t.arr.get($i)
    };
}
#[cfg(not(any(target_family = "wasm", feature = "segmented-arrays")))]
macro_rules! arr_slot_mut {
    ($t:expr, $i:expr) => {
        $t.arr.live_mut().get_mut($i)
    };
}
#[cfg(any(target_family = "wasm", feature = "segmented-arrays"))]
macro_rules! arr_slot_mut {
    ($t:expr, $i:expr) => {
        $t.arr.get_mut($i)
    };
}

/// `Table::absent`'s bit for `__newindex`.
const NEWINDEX_BIT: u32 = 1 << (crate::vm::Tm::NewIndex as u8);
use crate::nostd::cell::Cell;
use crate::value::{FuncRef, ThreadRef, Value};
use crate::vm::{Cont, EXTRA_STACK, Frame, FrameKind};

/// `..` over `ops` into `buf` when every operand is a string or number and the result is
/// short (and within the string limit): its length.
fn concat_short(heap: &Heap, ops: &[Value], buf: &mut [u8; crate::heap::SHORT_LEN], limit: usize) -> Option<usize> {
    let mut total = 0usize;
    for &v in ops {
        // (A float's text is charged on the full path: `FLOAT_TEXT_FUEL`.)
        if !matches!(v, Value::Str(_) | Value::Int(_)) {
            return None;
        }
        total += crate::vm::stringable_len(heap, v);
        if total > crate::heap::SHORT_LEN || total > limit {
            return None;
        }
    }
    let mut out: &mut [u8] = buf;
    for &v in ops {
        crate::vm::write_stringable(heap, &mut out, v);
    }
    Some(total)
}

/// What the loop needs to know about the running frame and the state.
pub(crate) struct Ctx {
    pub cur: ThreadRef,
    /// The call depth limit.
    pub call_depth: usize,
    /// Returns may not take the running thread to this many frames or fewer (where
    /// `Lua::run` stops).
    pub stop_depth: usize,
    /// The interned "__index" (for method lookups through a class table).
    pub index_name: Value,
    /// The interned "__eq".
    pub eq_name: Value,
    pub string_meta: Option<crate::value::TableRef>,
    /// The string length limit.
    pub string_len: usize,
    /// `next` and `ipairs`'s iterator: a generic `for` over them runs here.
    pub next_fn: Option<FuncRef>,
    pub ipairs_aux: Option<FuncRef>,
    /// `utf8.codes`' strict and lax iterators (`stdlib::utf8::step`).
    pub utf8_iters: [Option<FuncRef>; 2],
    /// The math functions computed in place (`MATH_FAST` order).
    pub math_fast: [Option<FuncRef>; 6],
    /// A bit per `math_fast` function's slot index, mod 64 (see `math_op`).
    pub math_mask: u64,
    /// `table.insert` and `table.remove`, run in place on plain arrays
    /// (`stdlib::table::in_place`), and their `math_mask` (see `table_op`).
    pub table_fast: [Option<FuncRef>; 2],
    pub table_mask: u64,
}

/// The math library functions a call computes in place, when the callee is exactly the
/// library's function value (an identity check: a rebound `math.sqrt` is called as usual).
pub(crate) const MATH_FAST: [&str; 6] = ["sqrt", "floor", "ceil", "abs", "min", "max"];

/// `math.<MATH_FAST[op]>(args...)` on number arguments, exactly as the library computes it
/// (`stdlib::math`), or None (other arguments, no arguments: the full path, which raises
/// the error or converts strings).
#[inline]
fn math_call(op: usize, args: &[Value]) -> Option<Value> {
    let x = *args.first()?;
    let num = |v: Value| match v {
        Value::Int(i) => Some(i as f64),
        Value::Num(f) => Some(f),
        _ => None,
    };
    let int_or_float = |f: f64| number::float_to_int(f).map_or(Value::Num(f), Value::Int);
    Some(match op {
        0 => Value::Num(num(x)?.sqrt()),
        1 => match x {
            Value::Int(_) => x,
            _ => int_or_float(num(x)?.floor()),
        },
        2 => match x {
            Value::Int(_) => x,
            _ => int_or_float(num(x)?.ceil()),
        },
        3 => match x {
            Value::Int(i) => Value::Int(i.wrapping_abs()),
            _ => Value::Num(num(x)?.abs()),
        },
        4 | 5 => {
            let mut best = x;
            num(x)?;
            for &v in args.get(1..)? {
                let better = if op == 4 { less(v, best, false)? } else { less(best, v, false)? };
                if better {
                    best = v;
                }
            }
            best
        }
        _ => return None,
    })
}

/// Which of `MATH_FAST` `f` is, if any. The table is read through `black_box`: read
/// plainly, the loop's entry loaded (and spilled) all of it every time the loop was
/// entered, which a native call does each time.
#[inline(always)]
fn math_op(ctx: &Ctx, f: FuncRef) -> Option<usize> {
    // Most natives are not one of them: a bit per slot index (mod 64) says which might be.
    if ctx.math_mask & (1 << (f.idx & 63)) == 0 {
        return None;
    }
    crate::nostd::hint::black_box(ctx).math_fast.iter().position(|m| *m == Some(f))
}

/// `table.insert` or `table.remove` (`Ctx::table_fast`), checked as `math_op` checks.
#[inline(always)]
fn table_op(ctx: &Ctx, f: FuncRef) -> Option<usize> {
    if ctx.table_mask & (1 << (f.idx & 63)) == 0 {
        return None;
    }
    crate::nostd::hint::black_box(ctx).table_fast.iter().position(|m| *m == Some(f))
}

/// `Ctx::math_mask` for a `math_fast` table.
pub(crate) fn math_mask(fns: &[Option<FuncRef>]) -> u64 {
    fns.iter().flatten().fold(0, |m, f| m | 1 << (f.idx & 63))
}

#[inline]
fn float_op(op: ArithOp, f: f64, g: f64) -> Option<f64> {
    Some(match op {
        ArithOp::Add => f + g,
        ArithOp::Sub => f - g,
        ArithOp::Mul => f * g,
        ArithOp::Div => f / g,
        ArithOp::Pow => {
            if g == 2.0 {
                f * f
            } else {
                f.powf(g)
            }
        }
        ArithOp::IDiv => (f / g).floor(),
        ArithOp::Mod => number::float_mod(f, g),
        _ => return None,
    })
}

#[inline]
fn int_op(op: ArithOp, i: i64, j: i64) -> Option<Value> {
    Some(match op {
        ArithOp::Add => Value::Int(i.wrapping_add(j)),
        ArithOp::Sub => Value::Int(i.wrapping_sub(j)),
        ArithOp::Mul => Value::Int(i.wrapping_mul(j)),
        ArithOp::Div => Value::Num(i as f64 / j as f64),
        ArithOp::BAnd => Value::Int(i & j),
        ArithOp::BOr => Value::Int(i | j),
        ArithOp::BXor => Value::Int(i ^ j),
        ArithOp::Shl => Value::Int(number::shift_left(i, j)),
        ArithOp::Shr => Value::Int(number::shift_left(i, j.wrapping_neg())),
        ArithOp::IDiv if j != 0 => Value::Int(number::int_floor_div(i, j)),
        ArithOp::Mod if j != 0 => Value::Int(number::int_mod(i, j)),
        ArithOp::Pow => Value::Num((i as f64).powf(j as f64)),
        _ => return None,
    })
}

/// `arith` for the rarer operators (`^` and bitwise), kept out of line: inlined at every
/// one of their opcodes it grows the fast loop and slows the common ones.
#[inline(never)]
fn arith_rare(op: ArithOp, x: Value, y: Value) -> Option<Value> {
    arith(op, x, y)
}

/// `x op y` on numbers, or None (strings, metamethods, errors: the full path).
#[inline(always)]
pub(crate) fn arith(op: ArithOp, x: Value, y: Value) -> Option<Value> {
    match (x, y) {
        (Value::Int(i), Value::Int(j)) => int_op(op, i, j),
        (Value::Num(f), Value::Num(g)) => float_op(op, f, g).map(Value::Num),
        (Value::Int(i), Value::Num(g)) => float_op(op, i as f64, g).map(Value::Num),
        (Value::Num(f), Value::Int(j)) => float_op(op, f, j as f64).map(Value::Num),
        _ => None,
    }
}

/// `x - k` for a constant `k`: Lua compiles a small integer constant as `x + -k`, which
/// differs for -0.0; do the same.
#[inline]
pub(crate) fn sub_k(x: Value, k: Value) -> Option<Value> {
    if let (Value::Num(f), Value::Int(j)) = (x, k)
        && (-127..=127).contains(&j)
    {
        return Some(Value::Num(f + (-j) as f64));
    }
    arith(ArithOp::Sub, x, k)
}

/// `x < y` / `x <= y` on numbers, or None.
#[inline]
pub(crate) fn less(x: Value, y: Value, or_equal: bool) -> Option<bool> {
    Some(match (x, y, or_equal) {
        (Value::Int(i), Value::Int(j), false) => i < j,
        (Value::Int(i), Value::Int(j), true) => i <= j,
        (Value::Num(f), Value::Num(g), false) => f < g,
        (Value::Num(f), Value::Num(g), true) => f <= g,
        (Value::Int(i), Value::Num(g), false) => number::int_lt_float(i, g),
        (Value::Int(i), Value::Num(g), true) => number::int_le_float(i, g),
        (Value::Num(f), Value::Int(j), false) => number::float_lt_int(f, j),
        (Value::Num(f), Value::Int(j), true) => number::float_le_int(f, j),
        _ => return None,
    })
}

/// The collector's barrier for an upvalue just written (as `Heap::barrier_upval`; a
/// macro so it borrows only the fields it touches).
macro_rules! upval_barrier {
    ($heap:expr, $u:expr) => {{
        let u: u32 = $u;
        if $heap.phase == Phase::Propagate
            && let Some(c) = $heap.upvals.color.get_mut(u as usize)
            && *c == BLACK
        {
            *c = GRAY;
            $heap.grayagain.push(Gray::Upval(u));
        }
    }};
}

/// A raw read that missed, tried again for a long string key under its interned copy
/// (as `Heap::get_in`). None: the full path (its interned copy is not known yet, and
/// finding it walks the interner, whose long walks are fuel, S5GC-3).
#[inline]
fn retry(heap: &Heap, tab: &Table, key: Value) -> Option<Value> {
    // Most misses are interned keys (all short strings): no call for them. The rest is
    // out of line, so the dispatch loop it is inlined into four times stays as it was
    // before S5GC-3 (inlined, it cost method-heavy code 10 to 14%).
    match key {
        Value::Str(r) if heap.strings.get(r.idx).is_some_and(|s| !s.interned) => retry_long(heap, tab, r),
        _ => Some(Value::Nil),
    }
}

#[cold]
#[inline(never)]
fn retry_long(heap: &Heap, tab: &Table, r: crate::value::StrRef) -> Option<Value> {
    let k = heap.known_canon(r)?;
    Some(if k == r { Value::Nil } else { tab.get(Value::Str(k)) })
}

/// `tab[key]` without a hint, out of line: inlined in `chain_from`, LLVM computed the
/// float key's normalization (a `floor` call) on every method lookup, string keys too.
#[inline(never)]
fn get_plain(tab: &Table, key: Value) -> Value {
    tab.get(key)
}

/// The rest of `t[key]` after the raw read in `tab` missed: through its chain of
/// `__index` tables (at most 8 links), without metamethod calls. The value and the number
/// of links followed (each is a step, as in `Lua::index`), or None when a function or a
/// non-table is in the way. (The links come back through `links`: a (value, count)
/// pair made the fast loop copy the value through memory byte-wise.) Each class read tries `hint` first (the instruction's), each
/// `__index` read the metatable's own hint.
#[inline]
fn chain_from(heap: &Heap, tab: &Table, key: Value, index_name: Value, hint: Option<&Cell<u32>>, links: &mut i64) -> Option<Value> {
    let mut tab = tab;
    // Links followed (each charged `vm::link_cost` of its place in the walk).
    let mut n = 0usize;
    loop {
        let Some(mt) = tab.meta else { return Some(Value::Nil) };
        let mtab = heap.tables.get(mt.idx)?;
        let next = mtab.get_cached(index_name, &mtab.index_hint);
        if next.is_nil() {
            return Some(Value::Nil);
        }
        if n >= 8 {
            return None;
        }
        // The link, and the probing of the lookup before it (S5GC-1), as `Lua::index`
        // charges them; the last lookup's is the instruction's.
        // (Steps count toward the limit of 8 links: past it, the full path.)
        // (Almost always no probe steps: `links` is small here, so a plain add then.)
        let cost = crate::vm::link_cost(n) as i64;
        n += 1;
        *links = match crate::table::take_probes() {
            0 => *links + cost,
            p => links.saturating_add(cost + i64::try_from(p).unwrap_or(i64::MAX / 2)),
        };
        let Value::Table(tr) = next else { return None };
        tab = heap.tables.get(tr.idx)?;
        let v = match hint {
            Some(h) => tab.get_cached(key, h),
            None => get_plain(tab, key),
        };
        let v = if v.is_nil() { retry(heap, tab, key)? } else { v };
        if !v.is_nil() {
            return Some(v);
        }
    }
}

/// A thread reference as one word, compared in one instruction (field by field, an open
/// upvalue's check against the running thread was two loads and two compares).
#[inline(always)]
fn thread_key(t: ThreadRef) -> u64 {
    u64::from(t.idx) | (u64::from(t.generation) << 32)
}

/// Runs the running thread `th` from its top frame's `pc` while it can, Lua-to-Lua calls
/// and returns included; see the module comment. Leaves each frame's `pc` saved. True
/// when it switched frames.
#[allow(clippy::too_many_lines, reason = "the fast loop's dispatch is one match")]
#[allow(clippy::indexing_slicing, reason = "u8 registers index the 256-slot window: never out of bounds")]
#[inline(never)]
pub(crate) fn run<const W: usize>(heap: &mut Heap, th: &mut ThreadState, top: &mut usize, ctx: &Ctx, fuel: &mut i64) -> bool {
    let ThreadState { stack, frames, open_upvals, tbc, .. } = th;
    // Probe steps of the instruction run in full just before: charged here, as the full
    // path charges them with the next instruction.
    let mut f = match crate::table::take_probes() {
        0 => *fuel,
        n => fuel.saturating_sub(i64::try_from(n).unwrap_or(i64::MAX)),
    };
    // A register's slot in the window (in range by construction: see `WINDOW`).
    let ix = |r: u16| usize::from(r) & (W - 1);
    // Whether a call or return switched frames here.
    let mut moved = false;
    // (`top` is read through `black_box` where a variable count needs it: read plainly,
    // LLVM loaded it at every dispatch, for the few instructions that use it.)
    // Frames a call here may push up to: the depth limit, or the list's room (a full list
    // grows on the full path, which charges it: S5GC-7). Only the full path grows it.
    let frame_room = frames.capacity().min(ctx.call_depth);
    'frame: loop {
    let Some(fr) = frames.last() else { break 'frame };
    let (pidx, base, mut at) = (fr.proto, fr.base, fr.pc);
    let Some(p) = heap.protos.get(pidx) else { break 'frame };
    let code = &p.code[..];
    // Constants, hints and upvalues are read through `p` and the heap when used: loading
    // them here would cost every call and return.
    let stack_len = stack.len();
    let (below, frame) = stack.split_at_mut(base);
    // A function of the other width runs in the other instantiation (back in `Lua::run`).
    if p.window() != W {
        moved = true;
        break 'frame;
    }
    let Some(w) = frame.get_mut(..W) else { break 'frame };
    let Ok(w): Result<&mut [Value; W], _> = w.try_into() else { break 'frame };

    // The running function, read from its frame where used (kept in a local, it was one
    // more value spilled at every frame entry).
    macro_rules! closure {
        () => {
            match frames.last() {
                Some(fr) => fr.closure,
                None => break,
            }
        };
    }
    macro_rules! upval_ref {
        ($u:expr) => {
            match heap.funcs.get(closure!().idx) {
                Some(Func::Lua { upvals, .. }) => upvals.get(usize::from($u)).copied(),
                _ => None,
            }
        };
    }
    // An upvalue's value: closed, or open on this thread's stack (below the window or in it).
    macro_rules! upval {
        ($u:expr) => {{
            let Some(r) = upval_ref!($u) else { break };
            match heap.upvals.get(r.idx) {
                Some(Upval::Closed(v)) => *v,
                Some(Upval::Open { thread, idx }) if thread_key(*thread) == thread_key(ctx.cur) => {
                    if let Some(v) = below.get(*idx) {
                        *v
                    } else if let Some(v) = idx.checked_sub(base).and_then(|i| w.get(i)) {
                        *v
                    } else {
                        break;
                    }
                }
                _ => break,
            }
        }};
    }
    macro_rules! k {
        ($k:expr) => {
            match p.consts.get($k as usize) {
                Some(v) => *v,
                None => break,
            }
        };
    }
    // `t[key] = val` in place (see `Table::set_existing`), with the collector's barrier.
    macro_rules! store {
        ($t:expr, $key:expr, $val:expr, $hint_at:expr) => {{
            let Value::Table(tr) = $t else { break };
            // The table the collector traverses in chunks: the full path (`raw_set`), which
            // marks what a write overwrites (S6GC-2e).
            if tr.idx == heap.part {
                break;
            }
            let (key, val): (Value, Value) = ($key, $val);
            let Some(tab) = heap.tables.get_mut(tr.idx) else { break };
            // A resize given up on it is kept: the full path keeps the write for it too.
            if tab.absent.get() & crate::table::RESUMABLE != 0 {
                break;
            }
            // A live field at the hinted node: no `__newindex` to ask. Else
            // `set_existing`, where a metatable known to lack `__newindex` cannot
            // intercept the write either. The barrier as `raw_set` decides it (a string
            // key counts), so the collector's work, and its fuel, are the full path's.
            // (Below Lua's border hint `alimit`: past it, `set_existing` moves the hint as
            // `luaH_getint` does.)
            let mark = if let Value::Int(i) = key
                && (i as u64).wrapping_sub(1) < tab.arr.alimit.get() as u64
                && let Some(s) = arr_slot_mut!(tab, (i as u64).wrapping_sub(1) as usize)
                && (!s.is_nil() || tab.meta.is_none())
            {
                // A slot of the array part: live, or no metatable to ask.
                *s = val;
                tab.absent.set(0);
                val.is_collectable()
            } else {
                // A live field at the hinted node holds this very key, so it is interned
                // (tables hold interned keys only): no need to look the string up first.
                let hint = p.hints.get($hint_at);
                if hint.is_some_and(|h| tab.set_at_hint(key, val, h.get())) {
                    val.is_collectable() || key.is_collectable()
                } else {
                // A long string key not interned yet is interned on the full path
                // (`raw_set`).
                if let Value::Str(r) = key
                    && !heap.strings.get(r.idx).is_some_and(|s| s.interned)
                {
                    break;
                }
                let meta = tab.meta;
                let in_arr = matches!(key, Value::Int(i) if (i as u64).wrapping_sub(1) < tab.arr.live_len() as u64);
                let newindex_absent = meta.is_some_and(|mt| heap.tables.get(mt.idx).is_some_and(|m| m.absent.get() & NEWINDEX_BIT != 0));
                // The fuel left once this instruction is charged (the full path charges it
                // first); a rehash past it is left to the full path, which has the reserve.
                heap.mem.fuel = usize::try_from(f - 1).unwrap_or(0);
                let Some(tab) = heap.tables.get_mut(tr.idx) else { break };
                let Some(slot) = tab.set_existing(key, val, hint.map_or(0, crate::nostd::cell::Cell::get), newindex_absent, &mut heap.mem) else { break };
                if let (Some(h), Some(s)) = (hint, slot) {
                    h.set(s);
                }
                // A hash part grown in place: its copying is fuel, as on the full path.
                let resized = tab.take_resized(&heap.mem);
                if resized >= 16 {
                    f -= (resized / 16) as i64;
                }
                if !in_arr {
                    probes!(key);
                }
                val.is_collectable() || key.is_collectable()
                }
            };
            if mark
                && heap.phase == Phase::Propagate
                && let Some(c) = heap.tables.color.get_mut(tr.idx as usize)
                && *c == BLACK
            {
                *c = GRAY;
                heap.grayagain.push(Gray::Table(tr.idx));
            }
        }};
    }
    // A test: when it holds, the jump after it (always there) runs in the same dispatch,
    // charged as its own step; when not, that jump is skipped.
    macro_rules! cond {
        ($holds:expr) => {
            if !$holds {
                at += 1;
            } else if f > 1
                && let Some(&Instr::Jmp { off, close: 0 }) = code.get(at + 1)
            {
                f -= 1;
                at = ((at + 1) as i64).wrapping_add(i64::from(off)) as usize;
            }
        };
    }
    macro_rules! rk {
        ($x:expr) => {{
            let x: Rk = $x;
            if x.is_const() { k!(x.index()) } else { w[x.index() & (W - 1)] }
        }};
    }
    // `less` of two operands, two registers (the common case) told apart from constants
    // by one test of both bits: `rk!` on each was a branch and a jump each.
    macro_rules! less2 {
        ($b:expr, $c:expr, $eq:expr) => {
            if ($b.0 | $c.0) & Rk::CONST == 0 { less(w[ix($b.0)], w[ix($c.0)], $eq) } else { less(rk!($b), rk!($c), $eq) }
        };
    }
    // The probe steps of the table work this instruction did (see `table::PROBES`):
    // charged by the instructions that can look up integer keys, as they finish.
    // Only lookups of integer (or float) keys outside the array part can probe, so `$key`
    // skips the thread-local read for the common string and array keys.
    // (Almost always none: a test and a branch then, not the clamped subtraction.)
    macro_rules! probes {
        () => {{
            let n = crate::table::take_probes();
            if n != 0 {
                f = f.saturating_sub(i64::try_from(n).unwrap_or(i64::MAX));
            }
        }};
        ($key:expr) => {
            if !matches!($key, Value::Str(_)) {
                probes!();
            }
        };
    }
    // A table field read: the value, or leave it to the full path (a metatable to ask).
    macro_rules! field {
        ($t:expr, $key:expr, $hint_at:expr) => {{
            let Value::Table(tr) = $t else { break };
            // The key read once (a constant's `k!` pasted at each use below was a bounds
            // check and a load at each).
            let key: Value = $key;
            let Some(tab) = heap.tables.get(tr.idx) else { break };
            let hint = p.hints.get($hint_at);
            // The common hits first, each a few loads: a string key at its hinted node, an
            // integer key inside the array part (Lua's `alimit`), with a live value. They
            // probe nothing and follow no `__index`, as the general read below would not.
            if let (Value::Str(_), Some(h)) = (key, hint)
                && let Some(v) = tab.at_hint(key, h.get())
                && !v.is_nil()
            {
                v
            } else if let Value::Int(i) = key
                && (i as u64).wrapping_sub(1) < tab.arr.alimit.get() as u64
                && let Some(&v) = arr_slot!(tab, (i as u64).wrapping_sub(1) as usize)
                && !v.is_nil()
            {
                v
            } else {
                let v = match hint {
                    Some(h) => tab.get_cached(key, h),
                    None => tab.get(key),
                };
                let v = if v.is_nil() { let Some(r) = retry(heap, tab, key) else { break }; r } else { v };
                if v.is_nil() && tab.meta.is_some() {
                    let mut links = 0;
                    let Some(v) = chain_from(heap, tab, key, ctx.index_name, hint, &mut links) else { break };
                    if f <= links + 1 {
                        break;
                    }
                    f -= links;
                    probes!(key);
                    v
                } else {
                    // A hit in the array part probes nothing (no thread-local read for it, as
                    // in `GetTable`).
                    match key {
                        Value::Str(_) => {}
                        Value::Int(i) if (i as u64).wrapping_sub(1) < tab.arr.live_len() as u64 => {}
                        _ => probes!(),
                    }
                    v
                }
            }
        }};
    }

    // `w[a] = t[k]` for a constant `k`: `field!`'s two hits first (a string at its hinted
    // node, an integer inside `alimit`, each with a live value: no probe, no `__index`),
    // each branch writing the register itself. A value merged from the branches went
    // through the stack as two 8-byte stores and one 16-byte load, which cannot be
    // forwarded: a stall on every field read.
    macro_rules! read_field {
        ($a:expr, $t:expr, $k:expr) => {{
            let t: Value = $t;
            let hit = match (t, p.consts.get($k as usize)) {
                (Value::Table(tr), Some(&key)) => heap.tables.get(tr.idx).and_then(|tab| match key {
                    Value::Str(_) => p.hints.get(at).and_then(|h| tab.at_hint(key, h.get())),
                    Value::Int(i) if (i as u64).wrapping_sub(1) < tab.arr.alimit.get() as u64 => arr_slot!(tab, (i as u64 - 1) as usize).copied(),
                    _ => None,
                }),
                _ => None,
            };
            match hit {
                Some(v) if !v.is_nil() => w[ix($a)] = v,
                _ => w[ix($a)] = field!(t, k!($k), at),
            }
        }};
    }

    loop {
        if f <= 0 {
            break;
        }
        let Some(&ins) = code.get(at) else { break };
        match ins {
            Instr::Move { a, b } => w[ix(a)] = w[ix(b)].copied(),
            Instr::LoadK { a, k } => w[ix(a)] = k!(k),
            Instr::LoadInt { a, i } => w[ix(a)] = Value::Int(i64::from(i)),
            Instr::LoadBool { a, v } => w[ix(a)] = Value::Bool(v),
            Instr::LoadFalseSkip { a } => {
                w[ix(a)] = Value::Bool(false);
                at += 1;
            }
            Instr::LoadNil { a, n } => {
                let (a, n) = (usize::from(a), usize::from(n));
                // Past 16 registers it is paid (SEC-M7): on the full path.
                if n >= 16 {
                    break;
                }
                let Some(regs) = w.get_mut(a..=a + n) else { break };
                regs.fill(Value::Nil);
            }
            Instr::GetUpval { a, u } => w[ix(a)] = upval!(u),
            Instr::GetSelf { a, .. } => w[ix(a)] = Value::Func(closure!()),
            Instr::GetTabUp { a, u, k } => read_field!(a, upval!(u), k),
            Instr::GetTable { a, b, c } => {
                let key = w[ix(c)];
                let Value::Table(tr) = w[ix(b)] else { break };
                let Some(tab) = heap.tables.get(tr.idx) else { break };
                // A live slot of the array part: the common case, read in place.
                if let Value::Int(i) = key
                    && (i as u64).wrapping_sub(1) < tab.arr.alimit.get() as u64
                    && let Some(&v) = arr_slot!(tab, (i as u64).wrapping_sub(1) as usize)
                    && !v.is_nil()
                {
                    w[ix(a)] = v;
                } else {
                // A hit in the array part probes nothing (no thread-local read for it).
                let in_arr = matches!(key, Value::Int(i) if (i as u64).wrapping_sub(1) < tab.arr.live_len() as u64);
                let mut v = tab.get(key);
                if v.is_nil() {
                    let Some(r) = retry(heap, tab, key) else { break };
                    v = r;
                }
                let mut chained = false;
                if v.is_nil() && tab.meta.is_some() {
                    let mut links = 0;
                    let Some(cv) = chain_from(heap, tab, key, ctx.index_name, None, &mut links) else { break };
                    if f <= links + 1 {
                        break;
                    }
                    f -= links;
                    v = cv;
                    chained = true;
                }
                w[ix(a)] = v;
                if chained || !in_arr {
                    probes!(key);
                }
                }
            }
            Instr::GetTableK { a, b, k } => read_field!(a, w[ix(b)], k),
            Instr::NewTable { a, arr, hash } => {
                // The collector's turn, or a cap that would refuse: the full path.
                let (arr, asize, hash) = crate::bytecode::newtable_sizes(arr, hash);
                if heap.gc_due() || !heap.mem.fits(crate::table::new_cost(arr, hash)) {
                    break;
                }
                let Ok(tr) = Heap::new_table_in::<true>(&mut heap.mem, &mut heap.tables, &mut heap.born, (heap.phase, heap.sweep, heap.table_seed), arr, hash) else { break };
                if let Some(t) = heap.tables.get_mut(tr.idx) {
                    t.init_sizes(asize, hash);
                }
                w[ix(a)] = Value::Table(tr);
            }
            Instr::SelfK { a, b, k } => {
                let obj = w[ix(b)];
                let key = k!(k);
                let method = match obj {
                    Value::Table(tr) => {
                        // The instance, then its classes (`__index` tables).
                        let Some(tab) = heap.tables.get(tr.idx) else { break };
                        let hint = p.hints.get(at);
                        let v = match hint {
                            Some(h) => tab.get_cached(key, h),
                            None => tab.get(key),
                        };
                        let v = if v.is_nil() { let Some(r) = retry(heap, tab, key) else { break }; r } else { v };
                        if v.is_nil()
                            && let Some(mt) = tab.meta
                        {
                            // The common case, one link: the metatable's `__index` a class
                            // table holding the method, each read at its hint alone. Nothing
                            // is searched or probed, so when it is not that case `chain_from`
                            // starts over as before; when it is, the link costs what
                            // `chain_from` charges for it.
                            let class = heap
                                .tables
                                .get(mt.idx)
                                .and_then(|m| m.at_hint(ctx.index_name, m.index_hint.get()))
                                .and_then(|c| if let Value::Table(cr) = c { heap.tables.get(cr.idx) } else { None });
                            let hit = match (class, hint) {
                                (Some(c), Some(h)) => c.at_hint(key, h.get()).filter(|v| !v.is_nil()),
                                _ => None,
                            };
                            let mut links = 0;
                            let v = match hit {
                                Some(v) => {
                                    links = crate::vm::LINK_COST as i64 + i64::try_from(crate::table::take_probes()).unwrap_or(i64::MAX / 2);
                                    v
                                }
                                None => {
                                    let Some(v) = chain_from(heap, tab, key, ctx.index_name, hint, &mut links) else { break };
                                    v
                                }
                            };
                            if f <= links + 1 {
                                break;
                            }
                            f -= links;
                            v
                        } else {
                            v
                        }
                    }
                    Value::Str(_) => {
                        let Some(mt) = crate::nostd::hint::black_box(ctx).string_meta else { break };
                        let Some(mtab) = heap.tables.get(mt.idx) else { break };
                        let lib = mtab.get_cached(ctx.index_name, &mtab.index_hint);
                        let v = field!(lib, key, at);
                        // The string library through `__index`: one link, as in `index`.
                        if v.is_nil() || f <= crate::vm::LINK_COST as i64 + 1 {
                            break;
                        }
                        f -= crate::vm::LINK_COST as i64;
                        v
                    }
                    _ => break,
                };
                let a = usize::from(a);
                w[(a + 1) & (W - 1)] = obj;
                // Word by word: `method` is merged from the branches above, and a 16-byte
                // reload of it over two 8-byte stores cannot be forwarded.
                w[a] = method.copied();
            }
            Instr::Arith { op, a, b, c } => {
                let (x, y) = (rk!(b), rk!(c));
                let v = if op == ArithOp::Sub && c.is_const() { sub_k(x, y) } else { arith(op, x, y) };
                let Some(v) = v else { break };
                w[ix(a)] = v;
            }
            Instr::Add { a, b, c } => {
                let Some(v) = arith(ArithOp::Add, w[ix(b)], w[ix(c)]) else { break };
                w[ix(a)] = v;
            }
            Instr::AddK { a, b, k } => {
                let Some(v) = arith(ArithOp::Add, w[ix(b)], k!(k)) else { break };
                w[ix(a)] = v;
            }
            Instr::Sub { a, b, c } => {
                let Some(v) = arith(ArithOp::Sub, w[ix(b)], w[ix(c)]) else { break };
                w[ix(a)] = v;
            }
            Instr::SubK { a, b, k } => {
                let Some(v) = sub_k(w[ix(b)], k!(k)) else { break };
                w[ix(a)] = v;
            }
            Instr::Mul { a, b, c } => {
                let Some(v) = arith(ArithOp::Mul, w[ix(b)], w[ix(c)]) else { break };
                w[ix(a)] = v;
            }
            Instr::MulK { a, b, k } => {
                let Some(v) = arith(ArithOp::Mul, w[ix(b)], k!(k)) else { break };
                w[ix(a)] = v;
            }
            Instr::Div { a, b, c } => {
                let Some(v) = arith(ArithOp::Div, w[ix(b)], w[ix(c)]) else { break };
                w[ix(a)] = v;
            }
            Instr::DivK { a, b, k } => {
                let Some(v) = arith(ArithOp::Div, w[ix(b)], k!(k)) else { break };
                w[ix(a)] = v;
            }
            Instr::Mod { a, b, c } => {
                let Some(v) = arith(ArithOp::Mod, w[ix(b)], w[ix(c)]) else { break };
                w[ix(a)] = v;
            }
            Instr::ModK { a, b, k } => {
                let Some(v) = arith(ArithOp::Mod, w[ix(b)], k!(k)) else { break };
                w[ix(a)] = v;
            }
            Instr::IDiv { a, b, c } => {
                let Some(v) = arith(ArithOp::IDiv, w[ix(b)], w[ix(c)]) else { break };
                w[ix(a)] = v;
            }
            Instr::IDivK { a, b, k } => {
                let Some(v) = arith(ArithOp::IDiv, w[ix(b)], k!(k)) else { break };
                w[ix(a)] = v;
            }
            Instr::Unm { a, b } => {
                w[ix(a)] = match w[ix(b)] {
                    Value::Int(i) => Value::Int(i.wrapping_neg()),
                    Value::Num(f) => Value::Num(-f),
                    _ => break,
                }
            }
            Instr::BNot { a, b } => {
                let Value::Int(i) = w[ix(b)] else { break };
                w[ix(a)] = Value::Int(!i);
            }
            Instr::Not { a, b } => w[ix(a)] = Value::Bool(!w[ix(b)].truthy()),
            Instr::Len { a, b } => {
                let n = match w[ix(b)] {
                    Value::Str(s) => heap.strings.get(s.idx).map_or(0, |s| s.bytes.len()) as i64,
                    Value::Table(tr) => match heap.tables.get(tr.idx) {
                        Some(t) if t.meta.is_none() => t.border(),
                        _ => break,
                    },
                    _ => break,
                };
                w[ix(a)] = Value::Int(n);
                probes!();
            }
            Instr::Jmp { off, close: 0 } => at = (at as i64).wrapping_add(i64::from(off)) as usize,
            Instr::AddI { a, b, i, .. } => {
                // (`x - k` is `x + -k`, as Lua computes it: -0.0 stays as Lua has it.)
                let Some(v) = arith(ArithOp::Add, w[ix(b)], Value::Int(i64::from(i))) else { break };
                w[ix(a)] = v;
            }
            Instr::Pow { a, b, c } => {
                let Some(v) = arith_rare(ArithOp::Pow, w[ix(b)], w[ix(c)]) else { break };
                w[ix(a)] = v;
            }
            Instr::PowK { a, b, k } => {
                let Some(v) = arith_rare(ArithOp::Pow, w[ix(b)], k!(k)) else { break };
                w[ix(a)] = v;
            }
            Instr::BAnd { a, b, c } => {
                let Some(v) = arith_rare(ArithOp::BAnd, w[ix(b)], w[ix(c)]) else { break };
                w[ix(a)] = v;
            }
            Instr::BAndK { a, b, k } => {
                let Some(v) = arith_rare(ArithOp::BAnd, w[ix(b)], k!(k)) else { break };
                w[ix(a)] = v;
            }
            Instr::BOr { a, b, c } => {
                let Some(v) = arith_rare(ArithOp::BOr, w[ix(b)], w[ix(c)]) else { break };
                w[ix(a)] = v;
            }
            Instr::BOrK { a, b, k } => {
                let Some(v) = arith_rare(ArithOp::BOr, w[ix(b)], k!(k)) else { break };
                w[ix(a)] = v;
            }
            Instr::BXor { a, b, c } => {
                let Some(v) = arith_rare(ArithOp::BXor, w[ix(b)], w[ix(c)]) else { break };
                w[ix(a)] = v;
            }
            Instr::BXorK { a, b, k } => {
                let Some(v) = arith_rare(ArithOp::BXor, w[ix(b)], k!(k)) else { break };
                w[ix(a)] = v;
            }
            Instr::Shl { a, b, c } => {
                let Some(v) = arith_rare(ArithOp::Shl, w[ix(b)], w[ix(c)]) else { break };
                w[ix(a)] = v;
            }
            Instr::ShlK { a, b, k } => {
                let Some(v) = arith_rare(ArithOp::Shl, w[ix(b)], k!(k)) else { break };
                w[ix(a)] = v;
            }
            Instr::Shr { a, b, c } => {
                let Some(v) = arith_rare(ArithOp::Shr, w[ix(b)], w[ix(c)]) else { break };
                w[ix(a)] = v;
            }
            Instr::ShrK { a, b, k } => {
                let Some(v) = arith_rare(ArithOp::Shr, w[ix(b)], k!(k)) else { break };
                w[ix(a)] = v;
            }
            Instr::EqK { a, k, eq } => {
                let (x, y) = (w[ix(a)], k!(k));
                let res = x.raw_eq(y)
                    || match (x, y) {
                        // Long strings' bytes: compared (and charged) on the full path.
                        (Value::Str(sa), Value::Str(sb)) => match heap.str_eq_quick(sa, sb) {
                            Some(r) => r,
                            None => break,
                        },
                        _ => false,
                    };
                cond!(res == eq);
            }
            Instr::EqI { a, i, eq } => {
                cond!(w[ix(a)].raw_eq(Value::Int(i64::from(i))) == eq);
            }
            Instr::LtI { a, i, k } => {
                let Some(res) = less(w[ix(a)], Value::Int(i64::from(i)), false) else { break };
                cond!(res == k);
            }
            Instr::LeI { a, i, k } => {
                let Some(res) = less(w[ix(a)], Value::Int(i64::from(i)), true) else { break };
                cond!(res == k);
            }
            Instr::GtI { a, i, k } => {
                let Some(res) = less(Value::Int(i64::from(i)), w[ix(a)], false) else { break };
                cond!(res == k);
            }
            Instr::GeI { a, i, k } => {
                let Some(res) = less(Value::Int(i64::from(i)), w[ix(a)], true) else { break };
                cond!(res == k);
            }
            Instr::Eq { b, c, k } => {
                let (x, y) = (rk!(b), rk!(c));
                let res = if x.raw_eq(y) {
                    true
                } else {
                    match (x, y) {
                        // Equal tables or userdata may say so through `__eq`.
                        (Value::Table(t1), Value::Table(t2)) => {
                            // `__eq` from either metatable decides; without one, identity.
                            let has_eq = |t: crate::value::TableRef| match heap.tables.get(t.idx) {
                                None => true,
                                Some(t) => t.meta.is_some_and(|m| {
                                    heap.tables.get(m.idx).is_none_or(|m| !m.get_str(crate::nostd::hint::black_box(ctx).eq_name).is_nil())
                                }),
                            };
                            if has_eq(t1) || has_eq(t2) {
                                break;
                            }
                            false
                        }
                        (Value::UserData(_), Value::UserData(_)) => break,
                        // Long strings' bytes: compared (and charged) on the full path.
                        (Value::Str(sa), Value::Str(sb)) => match heap.str_eq_quick(sa, sb) {
                            Some(r) => r,
                            None => break,
                        },
                        _ => false,
                    }
                };
                cond!(res == k);
            }
            Instr::Lt { b, c, k } => {
                let Some(res) = less2!(b, c, false) else { break };
                cond!(res == k);
            }
            Instr::Le { b, c, k } => {
                let Some(res) = less2!(b, c, true) else { break };
                cond!(res == k);
            }
            Instr::Test { a, k } => {
                cond!(w[ix(a)].truthy() == k);
            }
            Instr::TestSet { a, b, k } => {
                let v = w[ix(b)];
                if v.truthy() == k {
                    w[ix(a)] = v;
                } else {
                    at += 1;
                }
            }
            Instr::ForLoop { a, off } => {
                // The four registers as one slice: one bounds check, the values tested
                // and updated in place, and the index and count (whose tags cannot change)
                // stored as payloads alone. (The compiler keeps `a + 3` inside the window;
                // a window too short for it would leave to the full path.)
                let Some(regs) = w.get_mut(usize::from(a)..usize::from(a) + 4) else { break };
                match regs {
                    [Value::Int(idx), Value::Int(count), Value::Int(step), ctl] => {
                        if *count as u64 > 0 {
                            let i = idx.wrapping_add(*step);
                            *count = (*count as u64 - 1) as i64;
                            *idx = i;
                            *ctl = Value::Int(i);
                            at = (at as i64).wrapping_add(i64::from(off)) as usize;
                        }
                    }
                    [Value::Num(idx), Value::Num(limit), Value::Num(step), ctl] => {
                        let i = *idx + *step;
                        if if *step > 0.0 { i <= *limit } else { *limit <= i } {
                            *idx = i;
                            *ctl = Value::Num(i);
                            at = (at as i64).wrapping_add(i64::from(off)) as usize;
                        }
                    }
                    _ => break,
                }
            }
            // `..` of strings and numbers whose result is a short string: interned here
            // (memory refused, a collector step due, a long result or another operand:
            // the full path).
            Instr::Concat { a, n } => {
                if heap.gc_due() {
                    break;
                }
                let (a, n) = (usize::from(a), usize::from(n));
                let mut buf = [0u8; crate::heap::SHORT_LEN];
                let Some(len) = w.get(a..a + n).and_then(|ops| concat_short(heap, ops, &mut buf, crate::nostd::hint::black_box(ctx).string_len)) else { break };
                let parts = (heap.seed, heap.phase, heap.sweep);
                let made = Heap::intern_in(&mut heap.strings, &mut heap.interner, &mut heap.mem, parts, buf.get(..len).unwrap_or_default(), None);
                let Ok(r) = made else { break };
                w[ix(a as u16)] = Value::Str(r);
                // The interner's long walks (S5GC-3), as the full path charges them.
                probes!();
            }
            // The generic `for` over `next` (`pairs`) or `ipairs` on a plain table: the
            // iterator's work done here, with the same results, fuel and stack as calling
            // it (else the full path calls it).
            Instr::TForCall { a, c } => {
                let ra = usize::from(a);
                let Value::Func(it) = w[ix(a)] else { break };
                let (t, ctl) = (w[(ra + 1) & (W - 1)], w[(ra + 2) & (W - 1)]);
                // (Other states first: as `coroutine.wrap`'s, which leave at once.)
                if !matches!(t, Value::Table(_) | Value::Str(_)) {
                    break;
                }
                // (MULTI, all results: only past a 16-bit count, so only in wide functions; on
                // the full path. The narrow instantiation folds the test away.)
                if W != crate::vm::WINDOW && c == MULTI {
                    break;
                }
                let wanted = usize::from(c);
                // More than 16 results are paid (`move_results`, SEC-M7): on the full path.
                if wanted > 16 {
                    break;
                }
                // The stack the call would make sure of (`call_native`).
                if stack_len < base + ra + 7 + EXTRA_STACK || stack_len < base + ra + 6 + wanted.max(2) {
                    break;
                }
                // (The context read through `black_box`: hoisted, the loop's entry loads
                // and spills it each time, a native call's cost.)
                let cx = crate::nostd::hint::black_box(ctx);
                let (k, v) = match t {
                    Value::Str(r) => {
                        let Some(lax) = cx.utf8_iters.iter().position(|&g| g == Some(it)) else { break };
                        let (Value::Int(i), Some(st)) = (ctl, heap.strings.get(r.idx)) else { break };
                        let (cost, Some(r)) = crate::stdlib::utf8::step(&st.bytes, i, lax == 0) else { break };
                        if cost > 0 {
                            let cost = cost as i64;
                            if f <= cost + 1 {
                                break;
                            }
                            f -= cost;
                        }
                        r.map_or((Value::Nil, Value::Nil), |(i, c)| (Value::Int(i), Value::Int(c)))
                    }
                    Value::Table(tr) => {
                        let Some(tab) = heap.tables.get(tr.idx) else { break };
                        if Some(it) == cx.next_fn {
                        // A long key's interned copy only when known (as `retry`): finding it
                        // walks the interner, whose steps a break here dropped while the copy it
                        // found stayed, so the full path's `next` found it free (LR-26).
                        let key = match ctl {
                            Value::Str(_) if tab.at_next_hint(ctl).is_some() => ctl,
                            Value::Str(r) => match heap.known_canon(r) {
                                Some(k) => Value::Str(k),
                                None => break,
                            },
                            k => k,
                        };
                        let Ok((r, cost)) = tab.next_counted(key) else { break };
                        if cost > 0 {
                            let cost = cost as i64;
                            if f <= cost + 1 {
                                break;
                            }
                            f -= cost;
                        }
                        r.unwrap_or((Value::Nil, Value::Nil))
                    } else if Some(it) == cx.ipairs_aux && tab.meta.is_none() {
                        let Value::Int(i) = ctl else { break };
                        let i = i.wrapping_add(1);
                        match tab.get_int_inline(i) {
                            Value::Nil => (Value::Nil, Value::Nil),
                            v => (Value::Int(i), v),
                        }
                    } else {
                        break;
                    }
                    }
                    _ => break,
                };
                // As the call leaves them: the iterator, state and control copied up, then
                // the results over them (one nil when done) and nils up to `c`.
                let n = if k.is_nil() { 1 } else { 2 };
                w[(ra + 4) & (W - 1)] = k;
                w[(ra + 5) & (W - 1)] = if n == 2 { v } else { t };
                w[(ra + 6) & (W - 1)] = ctl;
                for i in n..wanted {
                    w[(ra + 4 + i) & (W - 1)] = Value::Nil;
                }
                probes!();
            }
            Instr::TForLoop { a, off } => {
                let ra = usize::from(a);
                let v = w[(ra + 4) & (W - 1)];
                if !v.is_nil() {
                    w[(ra + 2) & (W - 1)] = v;
                    at = (at as i64).wrapping_add(i64::from(off)) as usize;
                }
            }
            Instr::ForPrep { a, off } => {
                let ra = usize::from(a);
                let (Value::Int(i), Value::Int(l), Value::Int(s)) = (w[ra], w[(ra + 1) & (W - 1)], w[(ra + 2) & (W - 1)]) else {
                    break;
                };
                if s == 0 {
                    break;
                }
                if if s > 0 { i > l } else { i < l } {
                    at = (at as i64).wrapping_add(i64::from(off)) as usize;
                } else {
                    let count = if s > 0 {
                        (l as u64).wrapping_sub(i as u64) / (s as u64)
                    } else {
                        let s1 = (s.wrapping_add(1)).wrapping_neg() as u64 + 1;
                        (i as u64).wrapping_sub(l as u64) / s1
                    };
                    w[(ra + 1) & (W - 1)] = Value::Int(count as i64);
                    w[(ra + 3) & (W - 1)] = Value::Int(i);
                }
            }
            Instr::SetUpval { a, u } => {
                let v = w[ix(a)];
                let Some(r) = upval_ref!(u) else { break };
                match heap.upvals.get(r.idx) {
                    Some(Upval::Closed(_)) => {
                        let Some(slot) = heap.upvals.get_mut(r.idx) else { break };
                        *slot = Upval::Closed(v);
                        if v.is_collectable() {
                            upval_barrier!(heap, r.idx);
                        }
                    }
                    Some(Upval::Open { thread, idx }) if thread_key(*thread) == thread_key(ctx.cur) => {
                        let idx = *idx;
                        if let Some(s) = below.get_mut(idx) {
                            *s = v;
                        } else if let Some(s) = idx.checked_sub(base).and_then(|i| w.get_mut(i)) {
                            *s = v;
                        } else {
                            break;
                        }
                    }
                    _ => break,
                }
            }
            Instr::TailCall { a, b } => {
                let Value::Func(fref) = w[ix(a)] else { break };
                let Some(Func::Lua { proto, .. }) = heap.funcs.get(fref.idx) else { break };
                let proto = *proto;
                let Some(cp) = heap.protos.get(proto) else { break };
                let func = base + usize::from(a);
                let nargs = if b == MULTI { crate::nostd::hint::black_box(&*top).saturating_sub(func + 1) } else { usize::from(b) };
                let np = cp.num_params as usize;
                let Some(fr) = frames.last() else { break };
                let (ffunc, nres, from_lua) = (fr.func, fr.nresults, fr.from_lua);
                let nb = ffunc + 1;
                // (More than 16 missing parameters are paid: the full path, SEC-M7.)
                if nargs > 16
                    || np.saturating_sub(nargs) > 16
                    || (cp.is_vararg && nargs > np)
                    || !matches!(fr.kind, FrameKind::Normal)
                    || fr.handler.is_some()
                    || open_upvals.last().is_some_and(|(i, _)| *i >= base)
                    || tbc.last().is_some_and(|i| *i >= base)
                    || nb + cp.window() + EXTRA_STACK > stack.len()
                {
                    break;
                }
                stack.copy_within(func..func + 1 + nargs, ffunc);
                if let Some(missing) = stack.get_mut(nb + nargs..nb + np.max(nargs)) {
                    missing.fill(Value::Nil);
                }
                frames.pop();
                frames.push(Frame {
                    closure: fref,
                    proto,
                    func: ffunc,
                    base: nb,
                    pc: 0,
                    nresults: nres,
                    vstart: nb + np,
                    vcount: 0,
                    kind: FrameKind::Normal,
                    from_lua,
                    tail: true,
                    handler: None,
                });
                f -= 1;
                moved = true;
                continue 'frame;
            }
            Instr::Closure { a, p } => {
                // A collector step is due first: the full path does it.
                if heap.gc_due() {
                    break;
                }
                let Ok((v, extra)) = crate::vm::new_closure(heap, open_upvals, ctx.cur, pidx, closure!(), base, p as usize) else { break };
                if let Some(s) = stack.get_mut(base + usize::from(a)) {
                    *s = v;
                }
                if let Some(fr) = frames.last_mut() {
                    fr.pc = at + 1;
                }
                // Its upvalues, as the full path charges them (S5B-WALK-5).
                f -= 1 + i64::try_from(extra).unwrap_or(i64::MAX / 2);
                continue 'frame;
            }
            // A constructor's positional items into the array part `NewTable` made room
            // for (`Table::set_list_in_place`: anything else, a buffer to grow, a hash part
            // to look in or resize slots to charge, takes the full path), with the barrier
            // as the full path makes it (for every list).
            Instr::SetList { a, n, off } => {
                let ra = usize::from(a);
                let count = if n == crate::bytecode::SETLIST_MULTI { top.saturating_sub(base + ra + 1) } else { usize::from(n) };
                let Value::Table(tr) = w[ix(a)] else { break };
                // The table the collector traverses in chunks: the full path, which marks
                // what the list overwrites and writes (S6GC-2e).
                if tr.idx == heap.part {
                    break;
                }
                let Some(vals) = w.get(ra + 1..ra + 1 + count) else { break };
                let Some(tab) = heap.tables.get_mut(tr.idx) else { break };
                if !tab.set_list_in_place(off as usize, vals, &mut heap.mem) {
                    break;
                }
                if heap.phase == Phase::Propagate
                    && let Some(c) = heap.tables.color.get_mut(tr.idx as usize)
                    && *c == BLACK
                {
                    *c = GRAY;
                    heap.grayagain.push(Gray::Table(tr.idx));
                }
            }
            Instr::SetTabUp { u, k, v } => store!(upval!(u), k!(k), rk!(v), at),
            Instr::SetTable { a, b, c } => store!(w[ix(a)], w[ix(b)], rk!(c), usize::MAX),
            Instr::SetTableK { a, k, c } => store!(w[ix(a)], k!(k), rk!(c), at),
            Instr::Call { a, b, c } => {
                let Value::Func(fref) = w[ix(a)] else { break };
                let callee = heap.funcs.get(fref.idx);
                if !matches!(callee, Some(Func::Lua { .. })) {
                    // Not a Lua function: one of the math functions, `table.insert` or
                    // `table.remove`, or the full path.
                    let Some(op) = math_op(ctx, fref) else {
                        let Some(op) = table_op(ctx, fref) else { break };
                        // As the math functions below: the same results, fuel and stack as
                        // calling it, no allocation (so no collector step after it).
                        let func = base + usize::from(a);
                        let nargs = if b == MULTI { crate::nostd::hint::black_box(&*top).saturating_sub(func + 1) } else { usize::from(b) };
                        let want = if c == MULTI { 1 } else { usize::from(c) };
                        // (More than 16 results wanted are paid: the full path, SEC-M7.)
                        if heap.gc_due() || stack_len < func + 1 + nargs + EXTRA_STACK || stack_len < func + want.max(1) + 2 || nargs > 2 || want > 16 {
                            break;
                        }
                        let a = usize::from(a);
                        let args = [w[(a + 1) & (W - 1)], w[(a + 2) & (W - 1)]];
                        let Some((r, n, cost)) = crate::stdlib::table::in_place((&mut heap.tables, heap.phase, &mut heap.grayagain, heap.part), op, &args[..nargs], f) else { break };
                        f -= cost;
                        if n == 1 {
                            w[a & (W - 1)] = r;
                        }
                        // (All results wanted: just the result, the slots past it as
                        // they were.)
                        let fill = if c == MULTI { n } else { usize::from(c) };
                        for i in n..fill {
                            w[(a + i) & (W - 1)] = Value::Nil;
                        }
                        if c == MULTI {
                            *top = func + n;
                        }
                        f -= 1;
                        at = at.wrapping_add(1);
                        continue;
                    };
                    // A math function computed here, with the same results, fuel and stack
                    // as calling it (`call_native`: one step, no allocation, so the
                    // collector's turn after the call only when it is due already).
                    let func = base + usize::from(a);
                    let nargs = if b == MULTI { crate::nostd::hint::black_box(&*top).saturating_sub(func + 1) } else { usize::from(b) };
                    let want = if c == MULTI { 1 } else { usize::from(c) };
                    // (More than 16 results wanted are paid: the full path, SEC-M7.)
                    if heap.gc_due() || stack_len < func + 1 + nargs + EXTRA_STACK || stack_len < func + want.max(1) + 2 || nargs > 16 || want > 16 {
                        break;
                    }
                    let mut args = [Value::Nil; 16];
                    for (i, v) in args.iter_mut().enumerate().take(nargs) {
                        *v = w[(usize::from(a) + 1 + i) & (W - 1)];
                    }
                    let Some(r) = math_call(op, &args[..nargs]) else { break };
                    let a = usize::from(a);
                    w[a & (W - 1)] = r;
                    for i in 1..want {
                        w[(a + i) & (W - 1)] = Value::Nil;
                    }
                    if c == MULTI {
                        *top = func + 1;
                    }
                } else {
                let Some(Func::Lua { proto, .. }) = callee else { break };
                let proto = *proto;
                let Some(cp) = heap.protos.get(proto) else { break };
                let func = base + usize::from(a);
                let nargs = if b == MULTI { crate::nostd::hint::black_box(&*top).saturating_sub(func + 1) } else { usize::from(b) };
                let np = cp.num_params as usize;
                let nb = func + 1;
                // (A full frame list grows on the full path, which charges it: S5GC-7.)
                // (A callee of the other width is called on the full path, so the room
                // needed is this loop's own window, a constant. More than 16 missing
                // parameters are paid: the full path too, SEC-M7.)
                if (cp.is_vararg && nargs > np)
                    || cp.wide() != (W > WINDOW)
                    || frames.len() >= frame_room
                    || nb + W + EXTRA_STACK > stack.len()
                    || np.saturating_sub(nargs) > 16
                {
                    break;
                }
                if nargs < np
                    && let Some(missing) = stack.get_mut(nb + nargs..nb + np)
                {
                    missing.fill(Value::Nil);
                }
                if let Some(fr) = frames.last_mut() {
                    fr.pc = at + 1;
                }
                frames.push(Frame {
                    closure: fref,
                    proto,
                    func,
                    base: nb,
                    pc: 0,
                    nresults: if c == MULTI { -1 } else { i32::from(c) },
                    vstart: nb + np,
                    vcount: 0,
                    kind: FrameKind::Normal,
                    from_lua: true,
                    tail: false,
                    handler: None,
                });
                f -= 1;
                moved = true;
                continue 'frame;
                }
            }
            // `return x` and `return` (`Return1`, `Return0`: no varargs, nothing to close,
            // `Proto::specialize_returns`) to a caller wanting at most one result: the value
            // (or nil) to the function's slot, the frame popped. No count and no `top` to
            // read (all results wanted: one, so `top` is the slot after it). Else the full
            // path, as for `Return`.
            Instr::Return1 { a } => {
                let Some(fr) = frames.last() else { break };
                // (Out of `Lua::run` to the Rust that called, above another frame, as `Return`.)
                let leaving = frames.len() <= ctx.stop_depth + 1;
                // A metamethod's frame: its continuation applied as for `Return`.
                if let FrameKind::Cont(c @ (Cont::Store(_) | Cont::Discard | Cont::Cond { .. })) = fr.kind
                    && !leaving
                    && open_upvals.last().is_none_or(|&(i, _)| i < base)
                {
                    let v = w[ix(a)];
                    match c {
                        Cont::Store(slot) => {
                            let Some(d) = below.get_mut(slot) else { break };
                            *d = v;
                        }
                        Cont::Cond { k } if v.truthy() != k => {
                            let Some(caller) = frames.len().checked_sub(2).and_then(|i| frames.get_mut(i)) else { break };
                            caller.pc += 1;
                        }
                        _ => {}
                    }
                    frames.pop();
                    f -= 1;
                    moved = true;
                    continue 'frame;
                }
                if fr.nresults > 1
                    || !(fr.from_lua || leaving)
                    || (leaving && frames.len() <= 1)
                    || !matches!(fr.kind, FrameKind::Normal)
                    || open_upvals.last().is_some_and(|&(i, _)| i >= base)
                {
                    break;
                }
                let (func, wanted) = (fr.func, fr.nresults);
                let Some(d) = below.get_mut(func) else { break };
                *d = w[ix(a)];
                if wanted < 0 {
                    *top = func + 1;
                }
                frames.pop();
                f -= 1;
                moved = true;
                if leaving {
                    break 'frame;
                }
                continue 'frame;
            }
            Instr::Return0 => {
                let Some(fr) = frames.last() else { break };
                // (Out of `Lua::run` to the Rust that called, above another frame, as `Return`.)
                let leaving = frames.len() <= ctx.stop_depth + 1;
                if fr.nresults > 1
                    || !(fr.from_lua || leaving)
                    || (leaving && frames.len() <= 1)
                    || !matches!(fr.kind, FrameKind::Normal)
                    || open_upvals.last().is_some_and(|&(i, _)| i >= base)
                {
                    break;
                }
                let (func, wanted) = (fr.func, fr.nresults);
                if wanted == 1 {
                    let Some(d) = below.get_mut(func) else { break };
                    *d = Value::Nil;
                } else if wanted < 0 {
                    *top = func;
                }
                frames.pop();
                f -= 1;
                moved = true;
                if leaving {
                    break 'frame;
                }
                continue 'frame;
            }
            Instr::Return { a, b } => {
                let n = if b == MULTI { crate::nostd::hint::black_box(&*top).saturating_sub(base + usize::from(a)) } else { usize::from(b) };
                let Some(fr) = frames.last() else { break };
                // Back to a Lua caller here, or out of `Lua::run` to the Rust that called
                // (a callback, above another frame: under the last one a coroutine finishes,
                // on the full path), when nothing needs closing.
                // (More than 16 results wanted are paid: the full path, SEC-M7.)
                let leaving = frames.len() <= ctx.stop_depth + 1;
                // A metamethod's return to the instruction that called it (`call_meta`), with
                // nothing to close: its first result stored, dropped, or deciding the caller's
                // jump, as `apply_cont` does (one step, as any return). Other kinds: the full
                // path.
                if !matches!(fr.kind, FrameKind::Normal) {
                    let FrameKind::Cont(c @ (Cont::Store(_) | Cont::Discard | Cont::Cond { .. })) = fr.kind else { break };
                    if leaving
                        || open_upvals.last().is_some_and(|&(i, _)| i >= base)
                        || tbc.last().is_some_and(|i| *i >= base)
                    {
                        break;
                    }
                    let v = if n > 0 { w[ix(a)] } else { Value::Nil };
                    match c {
                        Cont::Store(slot) => {
                            let Some(d) = below.get_mut(slot) else { break };
                            *d = v;
                        }
                        Cont::Cond { k } if v.truthy() != k => {
                            let Some(caller) = frames.len().checked_sub(2).and_then(|i| frames.get_mut(i)) else { break };
                            caller.pc += 1;
                        }
                        _ => {}
                    }
                    frames.pop();
                    f -= 1;
                    moved = true;
                    continue 'frame;
                }
                if n > 16
                    || fr.nresults > 16
                    || !(fr.from_lua || leaving)
                    || (leaving && frames.len() <= 1)
                    || tbc.last().is_some_and(|i| *i >= base)
                {
                    break;
                }
                let (func, wanted) = (fr.func, fr.nresults);
                let want = if wanted < 0 { n } else { wanted as usize };
                if n == 1 && want <= 1 && open_upvals.last().is_none_or(|&(i, _)| i < base) {
                    // One result (`return x`, as C Lua's RETURN1), wanted or not, and no
                    // upvalue to close: from the window to the function's slot below it.
                    let Some(d) = below.get_mut(func) else { break };
                    *d = w[ix(a)];
                } else {
                    let src = base + usize::from(a);
                    // Upvalues of this frame's locals take their values now (as `close_upvals`).
                    while let Some(&(idx, r)) = open_upvals.last() {
                        if idx < base {
                            break;
                        }
                        open_upvals.pop();
                        let v = stack.get(idx).copied().unwrap_or_default();
                        if let Some(slot) = heap.upvals.get_mut(r) {
                            *slot = Upval::Closed(v);
                        }
                        if v.is_collectable() {
                            upval_barrier!(heap, r);
                        }
                    }
                    if func + want.max(n) >= stack.len() {
                        break;
                    }
                    // (A plain loop: results are few, and `copy_within` is a call. Copied slot
                    // to slot, then nils: a value chosen by a branch went through the stack
                    // frame byte-wise and stalled the store.) All `n` results are copied even
                    // when fewer are wanted, as the full path does: the slots past them are
                    // still scanned by the collector.
                    for i in 0..n {
                        if let Some(&v) = stack.get(src + i)
                            && let Some(d) = stack.get_mut(func + i)
                        {
                            *d = v;
                        }
                    }
                    if let Some(rest) = stack.get_mut(func + n..func + want) {
                        rest.fill(Value::Nil);
                    }
                }
                if wanted < 0 {
                    *top = func + n;
                }
                frames.pop();
                f -= 1;
                moved = true;
                if leaving {
                    break 'frame;
                }
                continue 'frame;
            }
            // Every other instruction (named, not `_`: with the match exhaustive the jump
            // table covers every opcode and the dispatch needs no range check).
            Instr::Jmp { .. }
            | Instr::Close { .. }
            | Instr::MoveAbs { .. }
            | Instr::PadNil { .. }
            | Instr::SetTop { .. }
            | Instr::TForPrep { .. }
            | Instr::Tbc { .. }
            | Instr::ToStr { .. }
            | Instr::VarArg { .. }
            | Instr::Window { .. } => break,
        }
        // Done: charged, and on to the next (jumps above already moved `at` past it).
        // (A jump to the first instruction leaves `at` one below zero: wrapping.)
        f -= 1;
        at = at.wrapping_add(1);
    }
    if let Some(fr) = frames.last_mut() {
        fr.pc = at;
    }
    break 'frame;
    }
    // Steps of an instruction left undone (it runs again in full, and counts there).
    crate::table::take_probes();
    *fuel = f;
    moved
}
