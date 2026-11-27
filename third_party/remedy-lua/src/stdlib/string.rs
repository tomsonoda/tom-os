//! The string library.

#[allow(unused_imports)]
use crate::nostd::prelude::*;
use super::R;
use super::pattern::{Cap, MatchState, Matcher, Watch, find_plain, no_specials};
use crate::LimitKind;
use crate::value::{StrRef, Value};
use crate::bytecode::ArithOp;
use crate::vm::{Args, Lua, LuaError};

pub(super) fn open(lua: &mut Lua) {
    let t = lua.lib_table("string");
    let fns: [(&str, &'static str, crate::vm::NativeFn); 17] = [
        ("len", "string.len", len),
        ("sub", "string.sub", sub),
        ("upper", "string.upper", upper),
        ("lower", "string.lower", lower),
        ("rep", "string.rep", rep),
        ("reverse", "string.reverse", reverse),
        ("byte", "string.byte", byte),
        ("char", "string.char", char_),
        ("find", "string.find", find),
        ("match", "string.match", match_),
        ("gmatch", "string.gmatch", gmatch),
        ("gsub", "string.gsub", gsub),
        ("format", "string.format", super::strformat::format),
        ("pack", "string.pack", super::pack::pack),
        ("unpack", "string.unpack", super::pack::unpack),
        ("packsize", "string.packsize", super::pack::packsize),
        ("dump", "string.dump", dump),
    ];
    for (n, q, f) in fns {
        lua.set_fn(t, n, q, f);
    }
    // The functions a call from Lua code runs in place, by identity (see `in_place`).
    register_in_place(lua, t, 0, IN_PLACE.get(..8).unwrap_or_default());
    // Strings index the string table: `s:upper()`; arithmetic on strings converts them
    // (these metamethods, as in Lua 5.4's string library).
    let Ok(meta) = lua.new_table(0, 16) else { return };
    lua.set_field(meta, "__index", Value::Table(t));
    let mms: [(&str, &'static str, crate::vm::NativeFn); 8] = [
        ("__add", "string.__add", |l, a| str_arith(l, a, ArithOp::Add)),
        ("__sub", "string.__sub", |l, a| str_arith(l, a, ArithOp::Sub)),
        ("__mul", "string.__mul", |l, a| str_arith(l, a, ArithOp::Mul)),
        ("__mod", "string.__mod", |l, a| str_arith(l, a, ArithOp::Mod)),
        ("__pow", "string.__pow", |l, a| str_arith(l, a, ArithOp::Pow)),
        ("__div", "string.__div", |l, a| str_arith(l, a, ArithOp::Div)),
        ("__idiv", "string.__idiv", |l, a| str_arith(l, a, ArithOp::IDiv)),
        ("__unm", "string.__unm", str_unm),
    ];
    for (n, q, f) in mms {
        lua.set_fn(meta, n, q, f);
    }
    lua.string_meta = Some(meta);
    lua.ctx_epoch += 1;
}

/// `posrelatI`: a start position (1-based; 0 and too-negative become 1).
pub(crate) fn posrelat_i(pos: i64, len: usize) -> usize {
    if pos > 0 {
        usize::try_from(pos).unwrap_or(usize::MAX)
    } else if pos == 0 {
        1
    } else if pos.unsigned_abs() > len as u64 {
        1
    } else {
        len - pos.unsigned_abs() as usize + 1
    }
}

/// `getendpos`: an end position, clamped to the length.
pub(crate) fn end_pos(pos: i64, len: usize) -> usize {
    if pos > len as i64 {
        len
    } else if pos >= 0 {
        pos as usize
    } else if pos.unsigned_abs() > len as u64 {
        0
    } else {
        len - pos.unsigned_abs() as usize + 1
    }
}

fn len(lua: &mut Lua, a: Args) -> R {
    let s = lua.check_str(a, 0)?;
    let n = lua.bytes(s).len() as i64;
    lua.ret1(Value::Int(n))
}

fn sub(lua: &mut Lua, a: Args) -> R {
    let s = lua.check_str(a, 0)?;
    let l = lua.bytes(s).len();
    let i = posrelat_i(lua.check_int(a, 1)?, l);
    let j = end_pos(lua.opt_int(a, 2, -1)?, l);
    if i > j {
        return lua.ret_str(b"");
    }
    lua.charge((j + 1).saturating_sub(i) as u64 / 64)?;
    lua.ret_derived(s, i - 1, j, |src, dest| dest.copy_from_slice(src))
}

fn map_bytes(lua: &mut Lua, a: Args, f: impl Fn(u8) -> u8) -> R {
    let s = lua.check_str(a, 0)?;
    let l = lua.bytes(s).len();
    lua.charge(l as u64 / 32)?;
    lua.ret_mapped(s, |src, at, dest| {
        for (d, &c) in dest.iter_mut().zip(src.get(at..).unwrap_or_default()) {
            *d = f(c);
        }
    })
}

fn upper(lua: &mut Lua, a: Args) -> R {
    map_bytes(lua, a, |c| c.to_ascii_uppercase())
}

fn lower(lua: &mut Lua, a: Args) -> R {
    map_bytes(lua, a, |c| c.to_ascii_lowercase())
}

fn reverse(lua: &mut Lua, a: Args) -> R {
    let s = lua.check_str(a, 0)?;
    let l = lua.bytes(s).len();
    lua.charge(l as u64 / 32)?;
    lua.ret_mapped(s, |src, at, dest| {
        let end = src.len().saturating_sub(at);
        let from = end.saturating_sub(dest.len());
        for (d, &c) in dest.iter_mut().zip(src.get(from..end).unwrap_or_default().iter().rev()) {
            *d = c;
        }
    })
}

/// Bytes `string.rep` copies between looks at the host's interrupt and deadline (S6-5; 4
/// MiB of fresh memory was 2 ms and more).
const REP_CHUNK: usize = super::COPY_PIECE;

fn rep(lua: &mut Lua, a: Args) -> R {
    let s = lua.check_str(a, 0)?;
    let n = lua.check_int(a, 1)?;
    let sep = lua.opt_str(a, 2)?;
    let (ls, lsep) = (lua.bytes(s).len(), sep.map_or(0, |x| lua.bytes(x).len()));
    if n <= 0 || (ls == 0 && lsep == 0) {
        return lua.ret_str(b"");
    }
    let total = (ls as u128 + lsep as u128) * n as u128 - lsep as u128;
    if total > lua.limits.string_len as u128 {
        return Err(LuaError::msg("resulting string too large"));
    }
    let total = total as usize;
    // Copied piece by piece: a unit per 8 bytes (calibrated: `examples/fuelcal.rs`).
    lua.charge(total as u64 / 8 + 1)?;
    // S6-5: one piece and separator, then the part written so far doubled: a byte at a
    // time, 16 MB of one-byte pieces ran 160 to 256 ms past a 20 ms time limit (and 4 to 6x
    // plain code per unit of fuel). A long result is built a few megabytes at a time
    // between looks at the host's interrupt and deadline.
    let first = |h: &crate::heap::Heap, dest: &mut [u8]| {
        let (sb, sepb) = (h.str_bytes(s), sep.map_or(&[][..], |x| h.str_bytes(x)));
        let unit = (ls + lsep).min(dest.len());
        let (a, b) = dest.split_at_mut(ls.min(unit));
        a.copy_from_slice(sb.get(..a.len()).unwrap_or_default());
        if let Some(b) = b.get_mut(..unit - a.len()) {
            b.copy_from_slice(sepb.get(..b.len()).unwrap_or_default());
        }
        unit
    };
    // The part written so far repeats every `unit` bytes: the next bytes are those from
    // `done % unit` on. (Copying from 0, as a doubling does, is right only while `done`
    // is a whole number of units: capping a step at `most` broke that, and a result over
    // 16 MiB, or any built with the first piece a megabyte at a time, came out wrong.)
    let unit = ls + lsep;
    let double = |dest: &mut [u8], done: usize, most: usize| {
        let r = done % unit;
        let k = (done - r).min(dest.len() - done).min(most);
        dest.copy_within(r..r + k, done);
        done + k
    };
    if total <= REP_CHUNK {
        let v = lua.new_built(total, |h, dest| {
            let mut done = first(h, dest);
            while done < dest.len() {
                done = double(dest, done, usize::MAX);
            }
        })?;
        return lua.ret1(v);
    }
    let charged = crate::heap::str_mem(total);
    lua.charge_mem(charged)?;
    let mut buf = vec![0u8; total];
    // The first piece and separator a megabyte at a time too (`rep(s, 1)` of 16 MiB
    // copied it all before the first look: 9 ms past a time limit).
    let mut done = 0;
    for (part, len) in [(Some(s), ls), (sep, lsep)] {
        let Some(part) = part else { continue };
        let mut at = 0;
        while at < len && done < total {
            let end = at.saturating_add(REP_CHUNK).min(len).min(at + (total - done));
            let src = lua.heap.str_bytes(part).get(at..end).unwrap_or_default();
            if let Some(d) = buf.get_mut(done..done + src.len()) {
                d.copy_from_slice(src);
            }
            done += end - at;
            at = end;
            if let Err(e) = lua.check_time() {
                lua.heap.mem.release(charged);
                return Err(e);
            }
        }
    }
    while done < total {
        done = double(&mut buf, done, REP_CHUNK);
        if let Err(e) = lua.check_time() {
            lua.heap.mem.release(charged);
            return Err(e);
        }
    }
    let v = Value::Str(lua.heap.new_long_charged(buf.into_boxed_slice()));
    lua.hold(v);
    lua.ret1(v)
}

fn byte(lua: &mut Lua, a: Args) -> R {
    let s = lua.check_str(a, 0)?;
    let l = lua.bytes(s).len();
    let raw = lua.opt_int(a, 1, 1)?;
    let pe = end_pos(lua.opt_int(a, 2, raw)?, l);
    let pi = posrelat_i(raw, l);
    if pi > pe {
        return lua.ret(&[]);
    }
    if pi == pe {
        let c = lua.bytes(s).get(pi - 1).copied().unwrap_or(0);
        return lua.ret1(Value::Int(i64::from(c)));
    }
    if pe - pi >= lua.limits.stack_slots / 2 {
        return Err(LuaError::msg("string slice too long"));
    }
    // The values' memory is charged before they are built.
    // Held until the call returns, then counted where the list is placed (SEC-M15); the
    // collector's debt as before.
    let most = (pe - pi + 1).saturating_mul(16);
    lua.charge_mem(most)?;
    lua.heap.mem.release(most);
    let mut hold = super::MemHold::default();
    hold.cover(lua, most)?;
    // S6-5: built straight into the results (no copy made first), looking at the host's
    // interrupt and deadline a chunk at a time; 3M values ran 54 to 74 ms past a 20 ms
    // time limit. The same fuel.
    let n = pe - pi + 1;
    lua.charge(n as u64 / 8)?;
    let mut rets = crate::nostd::mem::take(&mut lua.rets);
    rets.clear();
    rets.reserve(n);
    let mut at = pi - 1;
    while at < pe {
        let end = (at + super::NATIVE_CHUNK).min(pe);
        rets.extend(lua.bytes(s).get(at..end).unwrap_or_default().iter().map(|&c| Value::Int(i64::from(c))));
        at = end;
        if at < pe
            && let Err(e) = lua.check_time()
        {
            lua.rets = rets;
            return Err(e);
        }
    }
    lua.rets = rets;
    Ok(crate::vm::Flow::Return)
}

fn char_(lua: &mut Lua, a: Args) -> R {
    // Each argument is read and written: owed, paid on every exit (`owe`).
    super::owe(a.n as u64 / 8);
    let mut out = Vec::new();
    let mut short = [0u8; crate::heap::SHORT_LEN];
    for i in 0..a.n {
        let c = lua.check_int(a, i)?;
        if !(0..=255).contains(&c) {
            return Err(LuaError::Arg(i + 1, "value out of range".into()));
        }
        match short.get_mut(i) {
            Some(b) if a.n <= crate::heap::SHORT_LEN => *b = c as u8,
            _ => out.push(c as u8),
        }
    }
    if a.n <= crate::heap::SHORT_LEN {
        return lua.ret_str(short.get(..a.n).unwrap_or_default());
    }
    lua.ret_str(&out)
}

fn dump(_lua: &mut Lua, _a: Args) -> R {
    Err(LuaError::msg("unable to dump given function (RemedyLua has no bytecode format)"))
}

/// Steps a pattern call may take before "pattern too complex".
/// Matcher steps per unit of fuel: a step costs about what a few instructions do.
const STEPS_PER_FUEL: u64 = 4;

fn max_steps(len: usize) -> u64 {
    4_000_000 + 64 * len as u64
}

/// The step budget of one match: `max_steps`, or less when the fuel left runs out first
/// (the match then fails, and charging its steps raises the fuel limit), so one call
/// never runs far past the fuel.
fn step_cap(lua: &Lua, len: usize) -> u64 {
    let fuel = lua.fuel_left();
    max_steps(len).min(fuel.saturating_add(1).saturating_mul(STEPS_PER_FUEL))
}

fn pat_err(e: super::pattern::PatError) -> LuaError {
    LuaError::Msg(e.0)
}

fn cap_value(lua: &mut Lua, s: StrRef, c: Cap) -> Result<Value, LuaError> {
    match c {
        Cap::Pos(p) => Ok(Value::Int(p as i64)),
        Cap::Str(x, y) => {
            lua.new_derived(s, x, y, |src, dest| dest.copy_from_slice(src))
        }
    }
}

fn find_aux(lua: &mut Lua, a: Args, find: bool) -> R {
    let s = lua.check_str(a, 0)?;
    let p = lua.check_str(a, 1)?;
    let ls = lua.bytes(s).len();
    let init = posrelat_i(lua.opt_int(a, 2, 1)?, ls);
    if init > ls + 1 {
        return lua.ret1(Value::Nil);
    }
    let plain = find && (lua.arg(a, 3).truthy() || no_specials(lua.bytes(p)));
    if plain {
        // A unit of fuel per 64 bytes looked at; the search stops soon after the fuel
        // left is spent (the charge then fails).
        let budget = lua.fuel_left().saturating_mul(64).saturating_add(64);
        let (r, work, stop) = find_plain(lua.bytes(s), lua.bytes(p), init - 1, budget, &Watch::of(lua));
        lua.charge(work / 64 + 1)?;
        if let Some(k) = stop {
            return Err(LuaError::Limit(k));
        }
        return match r {
            Some(i) => {
                let lp = lua.bytes(p).len();
                lua.ret(&[Value::Int(i as i64 + 1), Value::Int((i + lp) as i64)])
            }
            None => lua.ret1(Value::Nil),
        };
    }
    let Some((st, e, caps)) = search(lua, s, p, init - 1, true, -1, !find, 1)? else { return lua.ret1(Value::Nil) };
    let mut out = [Value::Nil; super::pattern::MAX_CAPTURES + 2];
    let mut n = 0;
    if find {
        let [a, b, ..] = &mut out;
        (*a, *b) = (Value::Int(st as i64 + 1), Value::Int(e as i64));
        n = 2;
    }
    for c in caps.iter() {
        let v = cap_value(lua, s, c)?;
        if let Some(slot) = out.get_mut(n) {
            *slot = v;
        }
        n += 1;
    }
    lua.ret(out.get(..n).unwrap_or_default())
}

fn find(lua: &mut Lua, a: Args) -> R {
    find_aux(lua, a, true)
}

fn match_(lua: &mut Lua, a: Args) -> R {
    find_aux(lua, a, false)
}

/// Matches pattern `p` in `s` from `s1` on (only there when `anchor_ok` and `p` starts with
/// `^`), skipping a match that ends at `last`: its start, end and captures (`whole`: the
/// whole match when there are none). The matcher's steps, plus `setup`, are charged first.
#[allow(clippy::too_many_arguments)]
#[inline(always)]
fn search(
    lua: &mut Lua,
    s: StrRef,
    p: StrRef,
    s1: usize,
    anchor_ok: bool,
    last: i64,
    whole: bool,
    setup: u64,
) -> Result<Option<(usize, usize, super::pattern::Captures)>, LuaError> {
    // Match with the subject and pattern borrowed, then build the results.
    let cap = step_cap(lua, lua.bytes(s).len());
    // One matcher state for every call (`Lua::match_state`).
    let mut st = lua.match_state.take().unwrap_or_else(|| Box::new(MatchState::new(0, Watch::default())));
    st.reuse(cap, Watch::of(lua));
    let (found, steps, stop) = search_in(lua.bytes(s), lua.bytes(p), s1, anchor_ok, last, whole, &mut st);
    lua.match_state = Some(st);
    // The steps are charged before a pattern error is raised: a script catching "pattern
    // too complex" still pays for the search.
    lua.charge(steps / STEPS_PER_FUEL + setup)?;
    if let Some(k) = stop {
        return Err(LuaError::Limit(k));
    }
    found.transpose().map_err(pat_err)
}

/// A match result: where it starts, where it ends and its captures.
type Found = Option<Result<(usize, usize, super::pattern::Captures), super::pattern::PatError>>;

/// `search`'s matching, with the subject `src` and pattern `pat` borrowed and the matcher
/// state `st` made ready: the match, the steps it took and the limit that stopped it.
/// Shared with the in-place `find` and `match` (`in_place`), so both take the same steps.
#[allow(clippy::too_many_arguments)]
#[inline(always)]
fn search_in(src: &[u8], pat: &[u8], mut s1: usize, anchor_ok: bool, last: i64, whole: bool, st: &mut MatchState) -> (Found, u64, Option<LimitKind>) {
    let anchor = anchor_ok && pat.first() == Some(&b'^');
    let pat = if anchor { pat.get(1..).unwrap_or_default() } else { pat };
    let mut m = Matcher::new(src, pat, st);
    let mut found = None;
    while s1 <= src.len() {
        match m.attempt(s1, !anchor) {
            Err(e) => {
                found = Some(Err(e));
                break;
            }
            Ok(Some(e)) if e as i64 != last => {
                found = Some(m.captures(s1, e, whole).map(|caps| (s1, e, caps)));
                break;
            }
            _ => {}
        }
        if anchor {
            break;
        }
        s1 += 1;
    }
    (found, m.steps, m.stop)
}

fn gmatch(lua: &mut Lua, a: Args) -> R {
    let s = lua.check_str(a, 0)?;
    let p = lua.check_str(a, 1)?;
    let ls = lua.bytes(s).len();
    let init = posrelat_i(lua.opt_int(a, 2, 1)?, ls);
    let start = if init > ls + 1 { ls + 1 } else { init - 1 };
    let f = lua.new_native(
        gmatch_aux,
        "gmatch_aux",
        vec![Value::Str(s), Value::Str(p), Value::Int(start as i64), Value::Int(-1)],
    )?;
    lua.ret1(f)
}

fn gmatch_aux(lua: &mut Lua, a: Args) -> R {
    let (Value::Str(s), Value::Str(p)) = (lua.native_upval(a, 0), lua.native_upval(a, 1)) else {
        return lua.ret1(Value::Nil);
    };
    let pos = lua.native_upval(a, 2).as_int().unwrap_or(0).max(0) as usize;
    let last = lua.native_upval(a, 3).as_int().unwrap_or(-1);
    // Each call of the iterator also costs a native call's setup: 2 units, not 1.
    let Some((_, e, caps)) = search(lua, s, p, pos, false, last, true, 2)? else {
        lua.set_native_upval(a, 2, Value::Int(lua.bytes(s).len() as i64 + 1));
        // No values at all, as C Lua (`select('#', it())` is 0).
        return lua.ret(&[]);
    };
    lua.set_native_upval(a, 2, Value::Int(e as i64));
    lua.set_native_upval(a, 3, Value::Int(e as i64));
    let mut out = [Value::Nil; super::pattern::MAX_CAPTURES];
    let mut n = 0;
    for (slot, c) in out.iter_mut().zip(caps.iter()) {
        *slot = cap_value(lua, s, c)?;
        n += 1;
    }
    lua.ret(out.get(..n).unwrap_or_default())
}

fn gsub(lua: &mut Lua, a: Args) -> R {
    // The search and the bytes copied are paid for on every exit (`OutBuf`, `owe`): a
    // replacement function that errors after the output has grown, or an error in the
    // pattern, is not free.
    let mut out = super::OutBuf::new(64);
    let mut steps = 0u64;
    let r = gsub_into(lua, a, &mut out, &mut steps);
    super::owe(steps / STEPS_PER_FUEL + 1);
    let n = r?;
    let b = out.into_vec(lua);
    let r = lua.new_owned(b)?;
    lua.ret(&[r, Value::Int(n)])
}

/// The bytes replacement `rs` makes for the match `pos..e` (sized before it is copied).
fn repl_len(rs: &[u8], m: &Matcher<'_>, pos: usize, e: usize) -> usize {
    let (mut n, mut i) = (0usize, 0);
    while i < rs.len() {
        if rs.get(i) != Some(&b'%') {
            n += 1;
            i += 1;
            continue;
        }
        n += match rs.get(i + 1).copied().unwrap_or(0) {
            b'0' => e - pos,
            d @ b'1'..=b'9' => match m.capture(usize::from(d - b'1'), pos, e) {
                Ok(Cap::Str(x, y)) => y - x,
                Ok(Cap::Pos(p)) => p.to_string().len(),
                Err(_) => 0,
            },
            _ => 1,
        };
        i += 2;
    }
    n
}

fn gsub_into(lua: &mut Lua, a: Args, out: &mut super::OutBuf, steps_total: &mut u64) -> Result<i64, LuaError> {
    let s = lua.check_str(a, 0)?;
    let p = lua.check_str(a, 1)?;
    let repl = lua.arg(a, 2);
    match repl {
        Value::Str(_) | Value::Int(_) | Value::Num(_) | Value::Table(_) | Value::Func(_) => {}
        _ => return Err(lua.type_error(a, 2, "string/function/table")),
    }
    let repl_str = match repl {
        Value::Int(_) | Value::Num(_) => Some(lua.check_str(a, 2)?),
        Value::Str(r) => Some(r),
        _ => None,
    };
    let ls = lua.bytes(s).len();
    let max_n = lua.opt_int(a, 3, ls as i64 + 1)?;
    let anchor = lua.bytes(p).first() == Some(&b'^');
    let calls = matches!(repl, Value::Table(_) | Value::Func(_));
    let limit = lua.limits.string_len;
    let mut n = 0i64;
    let mut pos = 0usize;
    let mut last: Option<usize> = None;
    // One matcher for the whole call (each attempt starts afresh: `reset`, its own step
    // budget). The subject, pattern and replacement are read in place, not copied: the
    // matcher is bound to them while it runs and set aside (bound to nothing, its
    // captures kept) whenever Lua runs or fuel is charged.
    let mut state = MatchState::new(step_cap(lua, ls), Watch::of(lua));
    // LR-29: a replacement's copies are made a piece at a time between looks at the stop,
    // and none past the string limit (whose error comes once the replacement is read, as
    // before: a later invalid '%' is still that error).
    let watch = Watch::of(lua);
    let (mut since, mut over) = (0usize, false);
    // A match attempt whose result waits for the fuel to be charged first.
    let mut carried: Option<Option<usize>> = None;
    enum Stop {
        End,
        Fail(super::pattern::PatError),
        Charge,
        Call(usize),
        /// Room for this many bytes more, paid for before they are copied (SEC-M14).
        Reserve(usize),
    }
    loop {
        let stop = {
            let src = lua.heap.str_bytes(s);
            let pat_all = lua.heap.str_bytes(p);
            let pat = if anchor { pat_all.get(1..).unwrap_or_default() } else { pat_all };
            let rs = repl_str.map_or(&[][..], |r| lua.heap.str_bytes(r));
            let mut m = Matcher::new(src, pat, &mut state);
            loop {
                let r = match carried.take() {
                    Some(r) => r,
                    None => {
                        if n >= max_n {
                            break Stop::End;
                        }
                        m.steps = 0;
                        let r = m.attempt(pos, !anchor);
                        *steps_total += m.steps;
                        let r = match r {
                            Ok(r) => r,
                            Err(e) => break Stop::Fail(e),
                        };
                        if *steps_total > 1 << 12 {
                            carried = Some(r);
                            break Stop::Charge;
                        }
                        r
                    }
                };
                match r {
                    Some(e) if Some(e) != last => {
                        if !calls {
                            // (No more than the string limit: past it, the copy stops with
                            // its error. Asking for more could never be met: a spin.)
                            let need = repl_len(rs, &m, pos, e).min((limit + 1).saturating_sub(out.len()));
                            if out.len().saturating_add(need) > out.capacity() {
                                carried = Some(r);
                                break Stop::Reserve(need);
                            }
                        }
                        n += 1;
                        if calls {
                            break Stop::Call(e);
                        }
                        let whole = src.get(pos..e).unwrap_or_default();
                        // Reading the replacement: a step per byte (each `%n` is a copy).
                        *steps_total += rs.len() as u64;
                        let mut put = |out: &mut super::OutBuf, b: &[u8]| -> Result<(), LuaError> {
                            over = over || out.len() + b.len() > limit;
                            for piece in b.chunks(super::COPY_PIECE).take_while(|_| !over) {
                                out.extend_from_slice(piece);
                                since += piece.len();
                                if since >= super::COPY_PIECE {
                                    since = 0;
                                    if let Some(k) = watch.stop() {
                                        return Err(LuaError::Limit(k));
                                    }
                                }
                            }
                            Ok(())
                        };
                        let mut i = 0;
                        while i < rs.len() {
                            let c = rs.get(i).copied().unwrap_or(0);
                            if c != b'%' {
                                // A run of text, copied as a capture is (S6R-8: byte by byte,
                                // 16 MiB of it went by with no look at the stop).
                                let run = rs.get(i..).unwrap_or_default().iter().position(|&b| b == b'%').map_or(rs.len(), |k| i + k);
                                put(out, rs.get(i..run).unwrap_or_default())?;
                                i = run;
                                continue;
                            }
                            i += 1;
                            let d = rs.get(i).copied().unwrap_or(0);
                            if d == b'%' {
                                out.push(b'%');
                            } else if d.is_ascii_digit() {
                                if d == b'0' {
                                    put(out, whole)?;
                                } else {
                                    let idx = usize::from(d - b'1');
                                    match m.capture(idx, pos, e).map_err(pat_err)? {
                                        Cap::Str(x, y) => put(out, src.get(x..y).unwrap_or_default())?,
                                        Cap::Pos(p) => put(out, p.to_string().as_bytes())?,
                                    }
                                }
                            } else {
                                return Err(LuaError::msg("invalid use of '%' in replacement string"));
                            }
                            i += 1;
                        }
                        pos = e;
                        last = Some(e);
                    }
                    _ => {
                        if pos < src.len() && out.len() == out.capacity() {
                            carried = Some(r);
                            break Stop::Reserve(1);
                        }
                        if pos < src.len() {
                            out.push(src.get(pos).copied().unwrap_or(0));
                            pos += 1;
                        } else {
                            break Stop::End;
                        }
                    }
                }
                if over || out.len() > limit {
                    return Err(LuaError::Limit(LimitKind::Memory));
                }
                if anchor {
                    break Stop::End;
                }
            }
        };
        match stop {
            Stop::End => break,
            Stop::Fail(e) => {
                // Paid for by the caller before it is raised (see `find_aux`).
                if let Some(k) = state.stop {
                    return Err(LuaError::Limit(k));
                }
                return Err(pat_err(e));
            }
            Stop::Reserve(need) => out.reserve(lua, need)?,
            Stop::Charge => {
                lua.charge(*steps_total / STEPS_PER_FUEL)?;
                *steps_total = 0;
                out.pay(lua)?;
            }
            Stop::Call(e) => {
                out.pay(lua)?;
                // Each replacement through a table or function: the captures made and
                // the lookup or call, a few instructions' worth.
                *steps_total += 4 * STEPS_PER_FUEL;
                // The captures (the whole match when there are none), as values. A table is
                // indexed by the first only, and only it is read (C Lua's `add_value` pushes
                // one capture there): an unfinished later one is no error then (LR-3 repro).
                let mut args = [Value::Nil; super::pattern::MAX_CAPTURES];
                let ncap = if matches!(repl, Value::Table(_)) { 1 } else { state.level.max(1) };
                for (i, slot) in args.iter_mut().enumerate().take(ncap) {
                    *slot = match state.capture(i, pos, e).map_err(pat_err)? {
                        Cap::Pos(p) => Value::Int(p as i64),
                        Cap::Str(x, y) => lua.new_derived(s, x, y, |src, dest| dest.copy_from_slice(src))?,
                    };
                }
                let v = if let Value::Table(_) = repl {
                    let v = lua.index_nested(repl, args.first().copied().unwrap_or_default())?;
                    // Each match's lookup probing (S5GC-1), not the longest once.
                    lua.charge_probes()?;
                    v
                } else {
                    lua.call_nested1(repl, args.get(..ncap).unwrap_or_default())?
                };
                // A string is checked and its room paid for before it is copied, a piece at a
                // time between looks at the stop (SEC7-N2: copied whole first, up to a string
                // limit was outside the account, in one copy with no look).
                match v {
                    Value::Nil | Value::Bool(false) => copy_out(lua, out, s, pos, e, limit)?,
                    Value::Str(r) => copy_out(lua, out, r, 0, usize::MAX, limit)?,
                    Value::Int(_) | Value::Num(_) => lua.append_stringable(&mut **out, v),
                    other => {
                        return Err(LuaError::msg(format!("invalid replacement value (a {})", other.type_name())));
                    }
                }
                pos = e;
                last = Some(e);
                if out.len() > limit {
                    return Err(LuaError::Limit(LimitKind::Memory));
                }
                if anchor {
                    break;
                }
            }
        }
    }
    copy_out(lua, out, s, pos, usize::MAX, limit)?;
    Ok(n)
}

/// Bytes `from..to` of `s` onto gsub's output: past the string limit its memory error,
/// else the room paid for first and the bytes copied a piece at a time between looks at
/// the stop (`extend_watched`).
fn copy_out(lua: &mut Lua, out: &mut super::OutBuf, s: StrRef, from: usize, to: usize, limit: usize) -> Result<(), LuaError> {
    let n = lua.bytes(s).len().min(to).saturating_sub(from);
    if out.len().saturating_add(n) > limit {
        return Err(LuaError::Limit(LimitKind::Memory));
    }
    out.reserve(lua, n)?;
    lua.extend_watched(&mut **out, s, from, from + n)
}

/// The string metatable's arithmetic (`__add` …): numbers from strings, else the other
/// operand's metamethod, else an error (C Lua's `trymt`).
fn str_arith(lua: &mut Lua, a: Args, op: ArithOp) -> R {
    let (x, y) = (lua.arg(a, 0), lua.arg(a, 1));
    let v = lua.string_arith_now(op, x, y)?;
    lua.ret1(v)
}

fn str_unm(lua: &mut Lua, a: Args) -> R {
    let x = lua.arg(a, 0);
    match lua.tonumber_charged(x)? {
        Some(Value::Int(i)) => lua.ret1(Value::Int(i.wrapping_neg())),
        Some(Value::Num(f)) => lua.ret1(Value::Num(-f)),
        _ => Err(LuaError::msg(format!("attempt to unm a '{}' with a '{}'", x.type_name(), lua.arg(a, 1).type_name()))),
    }
}

/// The library functions a call from Lua code runs in place (`Lua::call_in_place`), in
/// this order: the string library's, then `tostring` and `type`.
pub(crate) const IN_PLACE: [&str; 10] = ["byte", "sub", "find", "match", "upper", "lower", "rep", "char", "tostring", "type"];

/// Marks the functions of `t` named `names` as `IN_PLACE`'s from `first` on
/// (`Func::Native::in_place`): a call to exactly that function value runs in place.
pub(crate) fn register_in_place(lua: &mut Lua, t: crate::value::TableRef, first: usize, names: &[&str]) {
    for (i, n) in names.iter().enumerate() {
        let f = lua.new_str(n.as_bytes()).map_or(Value::Nil, |k| lua.raw_get(t, k));
        if let Some(fr) = f.as_func()
            && let Some(crate::heap::Func::Native { in_place, .. }) = lua.heap.funcs.get_mut(fr.idx)
        {
            *in_place = u8::try_from(first + i + 1).unwrap_or(0);
        }
    }
}

/// Matcher steps and plain-search work an in-place `find` or `match` may do: below the
/// first look at the host's interrupt and deadline, so the full path would not look either.
const IN_PLACE_STEPS: u64 = super::pattern::WATCH_EVERY - 1;
const IN_PLACE_WORK: u64 = super::pattern::WATCH_EVERY * 16 - 1;

/// `IN_PLACE[op](args...)` without `call_native`'s set-up: up to two results, how many, and
/// the fuel charged besides the call's step; or None in any but the common case (an error,
/// a conversion, a metamethod, a long result, a look at the interrupt or deadline, the fuel
/// in hand `fuel` not covering what is charged before anything is made), when
/// `call_native` makes the call from the start and takes the same steps: nothing is made
/// or kept then, and the pending probe steps are as they were.
///
/// The fuel is the library function's: its charges, and the interner's walk steps
/// `new_str` charges right after a string is found or made. Only those may take the fuel
/// in hand below zero, where the caller then takes the next slice or stops.
pub(crate) fn in_place(
    heap: &mut crate::heap::Heap,
    types: &[StrRef; 8],
    ms: &mut Option<Box<MatchState>>,
    op: usize,
    args: &[Value],
    fuel: i64,
    string_len: usize,
) -> Option<([Value; 2], usize, i64)> {
    // (Probe steps pending would be charged at the call's step on the full path.)
    if crate::table::take_probes() != 0 {
        return None;
    }
    let arg = |i: usize| args.get(i).copied().unwrap_or(Value::Nil);
    let opt_int = |i: usize, d: i64| match arg(i) {
        Value::Nil => Some(d),
        Value::Int(n) => Some(n),
        _ => None,
    };
    let one = |v: Value| Some(([v, Value::Nil], 1, 0));
    let r = (|| match (op, arg(0)) {
        // tostring of a number, a boolean or nil (none has a metatable), as `Lua::tostring`.
        // char: integer bytes (owed a unit per 8 arguments, none for the four taken here).
        (7, _) => {
            let mut buf = [0u8; 4];
            for (d, v) in buf.iter_mut().zip(args) {
                let Value::Int(c @ 0..=255) = *v else { return None };
                *d = c as u8;
            }
            made(heap, buf.get(..args.len())?, 0, fuel, string_len)
        }
        (8, v) => {
            args.first()?;
            let mut buf = [0u8; 24];
            let (text, pre): (&[u8], u64) = match v {
                Value::Int(i) => {
                    let mut w: &mut [u8] = &mut buf;
                    let _ = crate::vm::write_int(&mut w, i);
                    let n = 24 - w.len();
                    (buf.get(..n)?, 0)
                }
                Value::Num(f) => {
                    let t = crate::number::fmt_float(f);
                    let b = buf.get_mut(..t.len())?;
                    b.copy_from_slice(t.as_bytes());
                    (b, crate::vm::FLOAT_TEXT_FUEL)
                }
                Value::Bool(b) => (if b { b"true" } else { b"false" }, 0),
                Value::Nil => (b"nil", 0),
                _ => return None,
            };
            made(heap, text, pre, fuel, string_len)
        }
        (9, v) => {
            args.first()?;
            one(Value::Str(*types.get(crate::stdlib::type_index(v))?))
        }
        (_, Value::Str(s)) => {
            let l = heap.str_bytes(s).len();
            match op {
                // byte: one position (more: the full path, which charges their memory).
                0 => {
                    let raw = opt_int(1, 1)?;
                    let (pe, pi) = (end_pos(opt_int(2, raw)?, l), posrelat_i(raw, l));
                    if pi > pe {
                        return Some(([Value::Nil; 2], 0, 0));
                    }
                    (pi == pe).then_some(())?;
                    one(Value::Int(i64::from(*heap.str_bytes(s).get(pi - 1)?)))
                }
                1 => {
                    let Value::Int(i) = arg(1) else { return None };
                    let (i, j) = (posrelat_i(i, l), end_pos(opt_int(2, -1)?, l));
                    if i > j {
                        return made(heap, b"", 0, fuel, string_len);
                    }
                    let (buf, n) = short_copy(heap.str_bytes(s).get(i - 1..j)?)?;
                    made(heap, buf.get(..n)?, (j + 1 - i) as u64 / 64, fuel, string_len)
                }
                // upper, lower: a unit per 32 bytes.
                4 | 5 => {
                    let (mut buf, n) = short_copy(heap.str_bytes(s))?;
                    for c in buf.iter_mut() {
                        *c = if op == 4 { c.to_ascii_uppercase() } else { c.to_ascii_lowercase() };
                    }
                    made(heap, buf.get(..n)?, l as u64 / 32, fuel, string_len)
                }
                // rep: a unit per 8 bytes, and one.
                6 => {
                    let Value::Int(n) = arg(1) else { return None };
                    let sep = match arg(2) {
                        Value::Nil => b"" as &[u8],
                        Value::Str(x) => heap.str_bytes(x),
                        _ => return None,
                    };
                    if n <= 0 || (l == 0 && sep.is_empty()) {
                        return made(heap, b"", 0, fuel, string_len);
                    }
                    let total = (l as u128 + sep.len() as u128) * n as u128 - sep.len() as u128;
                    if total > crate::heap::SHORT_LEN as u128 {
                        return None;
                    }
                    let mut buf = [0u8; crate::heap::SHORT_LEN];
                    let mut w: &mut [u8] = &mut buf;
                    for k in 0..n {
                        if k > 0 {
                            crate::nostd::io::Write::write_all(&mut w, sep).ok()?;
                        }
                        crate::nostd::io::Write::write_all(&mut w, heap.str_bytes(s)).ok()?;
                    }
                    made(heap, buf.get(..total as usize)?, total as u64 / 8 + 1, fuel, string_len)
                }
                // find, match
                _ => {
                    let Value::Str(p) = arg(1) else { return None };
                    let init = posrelat_i(opt_int(2, 1)?, l);
                    if init > l + 1 {
                        return one(Value::Nil);
                    }
                    let find = op == 2;
                    if find && (arg(3).truthy() || no_specials(heap.str_bytes(p))) {
                        let (hay, needle) = (heap.str_bytes(s), heap.str_bytes(p));
                        // (The library's budget counts the reserve too: it stops no search
                        // this one finishes.)
                        // Capped, and none whose scan alone passes the cap: the full path would
                        // repeat it (SEC7-N5, a 1 MiB miss was searched twice, 2x the CPU).
                        let budget = u64::try_from(fuel).ok()?.saturating_mul(64).saturating_add(64).min(IN_PLACE_WORK);
                        if hay.len() - (init - 1) > IN_PLACE_WORK as usize {
                            return None;
                        }
                        let (r, work, _) = find_plain(hay, needle, init - 1, budget, &Watch::NONE);
                        if work > budget {
                            return None;
                        }
                        let out = match r {
                            Some(i) => ([Value::Int(i as i64 + 1), Value::Int((i + needle.len()) as i64)], 2),
                            None => ([Value::Nil; 2], 1),
                        };
                        return in_budget(out.0, out.1, work / 64 + 1, fuel);
                    }
                    // The library's step cap (`step_cap`, the fuel in hand only), or fewer;
                    // the watch kept for the library's next call (never looked at here).
                    let cap = max_steps(l).min(u64::try_from(fuel).ok()?.saturating_add(1).saturating_mul(STEPS_PER_FUEL)).min(IN_PLACE_STEPS);
                    let mut st = ms.take().unwrap_or_else(|| Box::new(MatchState::new(0, Watch::default())));
                    st.reuse_keeping_watch(cap);
                    let (found, steps, _) = search_in(heap.str_bytes(s), heap.str_bytes(p), init - 1, true, -1, !find, &mut st);
                    *ms = Some(st);
                    // (Past the cap: an error here that the full path may not raise.)
                    if steps > cap {
                        return None;
                    }
                    let cost = steps / STEPS_PER_FUEL + 1;
                    let Some((st, e, caps)) = found.transpose().ok()? else { return in_budget([Value::Nil; 2], 1, cost, fuel) };
                    let (mut out, mut n) = if find { ([Value::Int(st as i64 + 1), Value::Int(e as i64)], 2) } else { ([Value::Nil; 2], 0) };
                    // (String captures found interned only: making them, the full path
                    // charges each as it goes, a point at which it may stop.)
                    for c in caps.iter() {
                        *out.get_mut(n)? = match c {
                            Cap::Pos(p) => Value::Int(p as i64),
                            Cap::Str(x, y) => {
                                let (buf, len) = short_copy(heap.str_bytes(s).get(x..y)?)?;
                                (len <= string_len).then_some(())?;
                                Value::Str(heap.find_short(buf.get(..len)?)?)
                            }
                        };
                        n += 1;
                    }
                    in_budget(out, n, cost, fuel)
                }
            }
        }
        _ => None,
    })();
    if r.is_none() {
        let _ = crate::table::take_probes();
    }
    r
}

/// `b` (at most `SHORT_LEN` bytes) copied out of the heap, and its length.
fn short_copy(b: &[u8]) -> Option<([u8; crate::heap::SHORT_LEN], usize)> {
    let mut buf = [0u8; crate::heap::SHORT_LEN];
    buf.get_mut(..b.len())?.copy_from_slice(b);
    Some((buf, b.len()))
}

/// The results, when the fuel in hand covers the call's charges and the interner's walk
/// steps (none may run out of fuel here, where the full path would refuel): else None.
fn in_budget(out: [Value; 2], n: usize, cost: u64, fuel: i64) -> Option<([Value; 2], usize, i64)> {
    let total = i64::try_from(cost.saturating_add(crate::table::take_probes())).unwrap_or(i64::MAX);
    (total < fuel).then_some((out, n, total))
}

/// The one result `bytes` as `new_str` makes it (short, interned), after `pre` fuel charged
/// first, which the fuel in hand must cover (else None, nothing made). None too past the
/// string limit (an error) or with its memory refused (the full path collects first).
fn made(heap: &mut crate::heap::Heap, bytes: &[u8], pre: u64, fuel: i64, string_len: usize) -> Option<([Value; 2], usize, i64)> {
    if bytes.len() > string_len || bytes.len() > crate::heap::SHORT_LEN || i64::try_from(pre).ok()? >= fuel {
        return None;
    }
    let r = heap.intern(bytes).ok()?;
    Some(([Value::Str(r), Value::Nil], 1, i64::try_from(pre.saturating_add(crate::table::take_probes())).unwrap_or(i64::MAX)))
}
