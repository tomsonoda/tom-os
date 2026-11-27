//! The table library. Every function honors `__index`, `__newindex` and `__len` like
//! Lua 5.4's, and every loop is charged fuel per element.

#[allow(unused_imports)]
use crate::nostd::prelude::*;
use super::R;
use crate::value::Value;
use crate::vm::{Args, Lua, LuaError, Tm};

pub(super) fn open(lua: &mut Lua) {
    let t = lua.lib_table("table");
    let fns: [(&str, &'static str, crate::vm::NativeFn); 7] = [
        ("insert", "table.insert", insert),
        ("remove", "table.remove", remove),
        ("move", "table.move", move_),
        ("concat", "table.concat", concat),
        ("unpack", "table.unpack", unpack),
        ("pack", "table.pack", pack),
        ("sort", "table.sort", sort),
    ];
    for (n, q, f) in fns {
        lua.set_fn(t, n, q, f);
    }
    // The functions the fast loop runs in place, by identity (see `in_place`).
    for (i, n) in IN_PLACE.iter().enumerate() {
        let f = lua.new_str(n.as_bytes()).map_or(Value::Nil, |k| lua.raw_get(t, k));
        if let Some(slot) = lua.names.table_fast.get_mut(i) {
            *slot = f.as_func();
        }
    }
    lua.names.table_mask = crate::fast::math_mask(&lua.names.table_fast);
    lua.ctx_epoch += 1;
}

/// The table functions a call runs in place in the fast loop, when the callee is exactly
/// the library's function value (`Names::table_fast`).
pub(crate) const IN_PLACE: [&str; 2] = ["insert", "remove"];

/// `table.insert(t, v)` (`op` 0) or `table.remove(t)` (`op` 1) on a table without a
/// metatable whose slot is in the array part, done in place with exactly the effects of
/// calling `insert` or `remove` (below): the same border (`#t`, its hint moved the same
/// way), the same slot written, the collector's barrier as `raw_set` decides it, and the
/// same fuel: the call's step (the caller's) and the border's probe steps (`seti` charges
/// them). The result (nil for `insert`), its count, and the probe steps to charge; None
/// (nothing changed but the border hint, which the full path's `#t` leaves the same) for
/// anything else, or when `fuel` would not cover the steps (the full path then stops at
/// the same point). Nothing here allocates: the caller makes sure no collector step is
/// due and the stack has the room `call_native` would make.
#[inline(never)]
pub(crate) fn in_place(heap: (&mut crate::heap::Slab<crate::table::Table>, crate::heap::Phase, &mut Vec<crate::heap::Gray>, u32), op: usize, args: &[Value], fuel: i64) -> Option<(Value, usize, i64)> {
    let (tables, phase, grayagain, part) = heap;
    let (&Value::Table(tr), want_args) = (args.first()?, if op == 0 { 2 } else { 1 }) else { return None };
    // (The table the collector traverses in chunks: `raw_set` marks what a write there
    // overwrites and writes, S6GC-2e; the full path.)
    if args.len() != want_args || tr.idx == part {
        return None;
    }
    let tab = tables.get_mut(tr.idx)?;
    // Slots resizes copied, not charged yet: `raw_set` would charge them; the full path.
    if tab.meta.is_some() || tab.resized >= 16 {
        return None;
    }
    // `getn` (its probe steps are charged with `seti`'s).
    let n = tab.border();
    let probes = i64::try_from(crate::table::take_probes()).unwrap_or(i64::MAX / 2);
    if fuel <= probes + 1 {
        return None;
    }
    let (pos, val) = match op {
        0 => (n.wrapping_add(1), *args.get(1)?),
        // (`remove` on an empty table looks at t[0]: the full path.)
        _ if n >= 1 => (n, Value::Nil),
        _ => return None,
    };
    // `seti`: `raw_set` -> `Table::set`, an array slot (`arr_index`, which may move the
    // border hint as `set` does).
    let u = tab.arr_index(pos)?;
    let slot = tab.arr.get_mut(u)?;
    let old = crate::nostd::mem::replace(slot, val);
    tab.absent.set(0);
    // (`Heap::barrier_table`.)
    if val.is_collectable()
        && phase == crate::heap::Phase::Propagate
        && let Some(c) = tables.color.get_mut(tr.idx as usize)
        && *c == crate::heap::BLACK
    {
        *c = crate::heap::GRAY;
        grayagain.push(crate::heap::Gray::Table(tr.idx));
    }
    // (`remove`'s `geti` read the same slot: an array hit, no probe steps.)
    Some(if op == 0 { (Value::Nil, 0, probes) } else { (old, 1, probes) })
}

/// The table argument `i`, which must be a table or have every metamethod in `need` (as
/// C Lua's `checktab`: a string has `__index` but no `__len`).
fn check_tab(lua: &Lua, a: Args, i: usize, need: &[Tm]) -> Result<Value, LuaError> {
    let v = lua.arg(a, i);
    if let Value::Table(_) = v {
        return Ok(v);
    }
    if lua.metatable(v).is_some() && need.iter().all(|&e| !lua.tm(v, e).is_nil()) {
        return Ok(v);
    }
    Err(lua.type_error(a, i, "table"))
}

const RWL: &[Tm] = &[Tm::Index, Tm::NewIndex, Tm::Len];

/// `t[i]`, kept alive (in the native's scratch roots) while the native holds it.
fn geti(lua: &mut Lua, t: Value, i: i64) -> Result<Value, LuaError> {
    let v = geti_used_now(lua, t, i)?;
    lua.hold(v);
    Ok(v)
}

/// `t[i]` for a caller that uses it at once, before anything can run Lua code or collect
/// (`concat` copies its bytes out): not kept. Keeping each of `concat`'s elements made the
/// scratch compaction (S5API-1) hash 300,000 distinct strings to find no repeats, a sixth
/// of a `table.concat`-heavy program's time.
#[inline]
fn geti_used_now(lua: &mut Lua, t: Value, i: i64) -> Result<Value, LuaError> {
    let v = match t {
        Value::Table(tr) if lua.metatable(t).is_none() => {
            // A hit in the array part probes nothing: no thread-local read for it (what
            // may be pending is charged at the next one, the same fuel).
            if let Some(v) = lua.heap.tables.get(tr.idx).and_then(|tab| in_array(tab, i)) {
                return Ok(v);
            }
            lua.raw_get(tr, Value::Int(i))
        }
        _ => lua.index_nested(t, Value::Int(i))?,
    };
    // The lookup's probe steps, charged before the next one (loops here may do many).
    lua.charge_probes()?;
    Ok(v)
}

/// `t[i]` when `i` is in the array part (where nothing is probed).
#[inline]
fn in_array(tab: &crate::table::Table, i: i64) -> Option<Value> {
    if (i as u64).wrapping_sub(1) < tab.arr.live_len() as u64 { Some(tab.get_int_inline(i)) } else { None }
}

fn seti(lua: &mut Lua, t: Value, i: i64, v: Value) -> Result<(), LuaError> {
    match t {
        Value::Table(tr) if lua.metatable(t).is_none() => lua.raw_set(tr, Value::Int(i), v),
        _ => lua.set_index_nested(t, Value::Int(i), v),
    }?;
    lua.charge_probes()
}

/// `dst[to] = src[from]`, as `geti` then `seti` do it: for tables without a metatable whose
/// slots are in the array part, straight from slot to slot with each step of the two calls
/// (the border hint moved by the read and the write, the collector's barrier, resizes'
/// slots charged, the probe steps charged), but without keeping the value in the scratch
/// roots: nothing runs Lua code or collects between the read and the write. A shift of a
/// long array (`table.remove(t, 1)`) spent most of its time keeping each value.
#[inline(always)]
fn copy_elem(lua: &mut Lua, src: Value, from: i64, dst: Value, to: i64) -> Result<(), LuaError> {
    // (Not into the table the collector traverses in chunks: `raw_set` marks what a write
    // there overwrites and writes, S6GC-2e.)
    if let (Value::Table(sr), Value::Table(dr)) = (src, dst)
        && dr.idx != lua.heap.part
        && lua.metatable(src).is_none()
        && lua.metatable(dst).is_none()
        && let Some(v) = lua.heap.tables.get(sr.idx).and_then(|tab| in_array(tab, from))
    {
        // `seti`: `raw_set` -> `Table::set` into an array slot.
        let heap = &mut lua.heap;
        let written = match heap.tables.get_mut(dr.idx) {
            Some(tab) => {
                tab.absent.set(0);
                match tab.arr_index(to).and_then(|u| tab.arr.get_mut(u)) {
                    Some(slot) => {
                        *slot = v;
                        true
                    }
                    None => false,
                }
            }
            None => false,
        };
        if !written {
            return seti(lua, dst, to, v);
        }
        if v.is_collectable() {
            lua.heap.barrier_table(dr);
        }
        let heap = &mut lua.heap;
        let resized = heap.tables.get_mut(dr.idx).map_or(0, |tab| tab.take_resized(&heap.mem));
        // (Each caller charged the element's step just before, which looks at the fuel and
        // the interrupt: with no probe steps, `charge_probes` would do nothing more.)
        let probes = crate::table::take_probes() + if resized >= 16 { resized as u64 / 16 } else { 0 };
        return if probes == 0 { Ok(()) } else { lua.charge(probes) };
    }
    let v = geti(lua, src, from)?;
    seti(lua, dst, to, v)
}

/// `#t` as an integer.
fn getn(lua: &mut Lua, t: Value) -> Result<i64, LuaError> {
    match t {
        Value::Table(tr) if lua.metatable(t).is_none() => Ok(lua.raw_len(tr)),
        // C's `luaL_len` takes the result through `lua_tointegerx`: "3" and 3.0 count.
        _ => {
            let n = lua.len_nested(t)?;
            lua.tointeger(n).ok_or_else(|| LuaError::msg("object length is not an integer"))
        }
    }
}

fn insert(lua: &mut Lua, a: Args) -> R {
    let t = check_tab(lua, a, 0, RWL)?;
    let e = getn(lua, t)?.wrapping_add(1);
    match a.n {
        2 => {
            let v = lua.arg(a, 1);
            seti(lua, t, e, v)?;
        }
        3 => {
            let pos = lua.check_int(a, 1)?;
            if (pos as u64).wrapping_sub(1) >= e as u64 {
                return Err(LuaError::Arg(2, "position out of bounds".into()));
            }
            let mut i = e;
            while i > pos {
                lua.charge(1)?;
                copy_elem(lua, t, i - 1, t, i)?;
                i -= 1;
            }
            let v = lua.arg(a, 2);
            seti(lua, t, pos, v)?;
        }
        _ => return Err(LuaError::msg("wrong number of arguments to 'insert'")),
    }
    lua.ret(&[])
}

fn remove(lua: &mut Lua, a: Args) -> R {
    let t = check_tab(lua, a, 0, RWL)?;
    let size = getn(lua, t)?;
    let mut pos = lua.opt_int(a, 1, size)?;
    if a.n >= 2 && pos != size && (pos as u64).wrapping_sub(1) > size as u64 {
        return Err(LuaError::Arg(2, "position out of bounds".into()));
    }
    let v = geti(lua, t, pos)?;
    while pos < size {
        lua.charge(1)?;
        copy_elem(lua, t, pos + 1, t, pos)?;
        pos += 1;
    }
    seti(lua, t, pos, Value::Nil)?;
    lua.ret1(v)
}

fn move_(lua: &mut Lua, a: Args) -> R {
    // C Lua's order: the integers first, then the tables.
    let f = lua.check_int(a, 1)?;
    let e = lua.check_int(a, 2)?;
    let t = lua.check_int(a, 3)?;
    let a1 = check_tab(lua, a, 0, &[Tm::Index])?;
    let to_self = lua.arg(a, 4).is_nil();
    let a2 = check_tab(lua, a, if to_self { 0 } else { 4 }, &[Tm::NewIndex])?;
    if e >= f {
        if !(f > 0 || e < i64::MAX + f) {
            return Err(LuaError::Arg(3, "too many elements to move".into()));
        }
        if t > i64::MAX - (e - f) {
            return Err(LuaError::Arg(4, "destination wrap around".into()));
        }
        let n = e - f;
        // As C Lua: `lua_compare` (so `__eq` may run) only when the ranges overlap.
        let forward = t > e || t <= f || (!to_self && !lua.eq_nested(a1, a2)?);
        for k in 0..=n {
            let i = if forward { k } else { n - k };
            lua.charge(1)?;
            copy_elem(lua, a1, f + i, a2, t + i)?;
        }
    }
    lua.ret1(a2)
}

fn concat(lua: &mut Lua, a: Args) -> R {
    let t = check_tab(lua, a, 0, &[Tm::Index, Tm::Len])?;
    let sep = match lua.opt_str(a, 1)? {
        Some(s) => lua.bytes(s).to_vec(),
        None => Vec::new(),
    };
    let i = lua.opt_int(a, 2, 1)?;
    let last = if lua.arg(a, 3).is_nil() { getn(lua, t)? } else { lua.check_int(a, 3)? };
    // The bytes copied are paid for on every exit (`OutBuf`), a bad value at the end too.
    let mut out = super::OutBuf::new(64);
    concat_into(lua, t, &sep, i, last, &mut out)?;
    let b = out.into_vec(lua);
    lua.ret_owned(b)
}

fn concat_into(lua: &mut Lua, t: Value, sep: &[u8], i: i64, last: i64, out: &mut super::OutBuf) -> Result<(), LuaError> {
    let mut k = i;
    while k <= last {
        lua.charge(1)?;
        let v = geti_used_now(lua, t, k)?;
        if let Value::Num(_) = v {
            lua.charge(crate::vm::FLOAT_TEXT_FUEL)?;
        }
        match v {
            // LR-29: a long string a piece at a time, each paid for as it is copied, with a
            // look at the stop between pieces.
            Value::Str(s) if lua.bytes(s).len() > super::COPY_PIECE => {
                let len = lua.bytes(s).len();
                let mut at = 0;
                while at < len {
                    let end = (at + super::COPY_PIECE).min(len);
                    out.extend_from_slice(lua.bytes(s).get(at..end).unwrap_or_default());
                    out.pay(lua)?;
                    at = end;
                    if at < len {
                        lua.check_time()?;
                    }
                }
            }
            Value::Str(_) | Value::Int(_) | Value::Num(_) => lua.append_stringable(&mut **out, v),
            other => {
                // C's `addfield` names it with `luaL_typename`: the plain type, never a
                // `__name` (LR-5).
                return Err(LuaError::msg(format!(
                    "invalid value ({}) at index {k} in table for 'concat'",
                    other.type_name()
                )));
            }
        }
        if k != last {
            out.extend_from_slice(sep);
        }
        out.pay(lua)?;
        lua.check_len(out.len())?;
        if k == i64::MAX {
            break;
        }
        k += 1;
    }
    Ok(())
}

fn unpack(lua: &mut Lua, a: Args) -> R {
    let t = lua.arg(a, 0);
    let i = lua.opt_int(a, 1, 1)?;
    let e = if lua.arg(a, 2).is_nil() { getn(lua, t)? } else { lua.check_int(a, 2)? };
    if i > e {
        return lua.ret(&[]);
    }
    let n = (e as i128 - i as i128 + 1) as u128;
    if n >= (lua.limits.stack_slots as u128) / 2 || n >= i32::MAX as u128 {
        return Err(LuaError::msg("too many results to unpack"));
    }
    lua.charge(n as u64)?;
    lua.charge_mem(n as usize * 16)?;
    lua.heap.mem.release(n as usize * 16);
    if let Value::Table(tr) = t
        && lua.metatable(t).is_none()
    {
        // A plain table: its values straight into the results. Reading runs no Lua code
        // and allocates nothing, so nothing needs holding meanwhile (as `geti_used_now`),
        // and the probe steps are charged as `geti` charges them: after each lookup
        // outside the array part.
        lua.rets.clear();
        // (The room checked above, no more: grown by doubling it was up to twice that.)
        lua.rets.reserve_exact(n as usize);
        let mut k = i;
        loop {
            let v = match lua.heap.tables.get(tr.idx).and_then(|tab| in_array(tab, k)) {
                Some(v) => v,
                None => {
                    let v = lua.raw_get(tr, Value::Int(k));
                    lua.charge_probes()?;
                    v
                }
            };
            lua.rets.push(v);
            if k == e {
                break;
            }
            k += 1;
            if lua.rets.len().is_multiple_of(super::NATIVE_CHUNK) {
                lua.check_time()?;
            }
        }
        return Ok(crate::vm::Flow::Return);
    }
    // The values gathered count against memory while `__index` runs (SEC-M3: an `__index`
    // that unpacked the same table again held one uncounted list per level), and are
    // given back as they move to the stack, which is charged itself.
    let mut hold = super::MemHold::default();
    hold.cover(lua, n as usize * 16)?;
    let mut out = Vec::with_capacity(n as usize);
    let mut k = i;
    loop {
        out.push(geti(lua, t, k)?);
        if k == e {
            break;
        }
        k += 1;
        // S6-5: the deadline is looked at as it goes (charged up front, 3M values ran 170
        // to 350 ms past a 20 ms time limit).
        if out.len() % super::NATIVE_CHUNK == 0 {
            lua.check_time()?;
        }
    }
    hold.give_back(lua);
    lua.ret(&out)
}

fn pack(lua: &mut Lua, a: Args) -> R {
    let t = lua.new_table(a.n, 1)?;
    lua.charge(a.n as u64)?;
    // Into the array part made for them: what a `raw_set` of each does, in one copy (the
    // table was made just now, so no barrier can be due).
    let heap = &mut lua.heap;
    if let (Some(tab), Some(vals)) = (heap.tables.get_mut(t.idx), lua.cur.stack.get(a.base..a.base + a.n)) {
        tab.set_list(0, vals, &mut heap.mem).map_err(|_| LuaError::Limit(crate::LimitKind::Memory))?;
    }
    let n = Value::Str(lua.names.n);
    lua.raw_set(t, n, Value::Int(a.n as i64))?;
    lua.ret1(Value::Table(t))
}

struct Sorter {
    comp: Value,
}

impl Sorter {
    fn less(&self, lua: &mut Lua, x: Value, y: Value) -> Result<bool, LuaError> {
        lua.charge(1)?;
        if self.comp.is_nil() {
            lua.less_than(x, y)
        } else {
            Ok(lua.call_nested1(self.comp, &[x, y])?.truthy())
        }
    }
}

/// The table `auxsort` sorts, in place as C Lua's (`lua_geti`/`lua_seti` each time): a
/// plain table through its parts directly, one with a metatable through its metamethods
/// (checked at every access: a comparator may set one). A value the sort holds is kept in
/// one of four root slots (`roots..roots + 4` in the native's scratch) while comparators run.
struct InPlace {
    t: Value,
    roots: usize,
}

impl InPlace {
    #[inline]
    fn get(&mut self, lua: &mut Lua, i: usize, slot: usize) -> Result<Value, LuaError> {
        let mut in_arr = false;
        // (One lookup of the table for its metatable and its slot.)
        let plain = match self.t {
            Value::Table(tr) => match lua.heap.tables.get(tr.idx) {
                Some(tab) if tab.meta.is_none() => {
                    in_arr = (i as u64).wrapping_sub(1) < tab.arr.live_len() as u64;
                    Some(tab.get_int_inline(i as i64))
                }
                Some(_) => None,
                None => Some(Value::Nil),
            },
            _ => None,
        };
        let v = match plain {
            Some(v) => v,
            None => lua.index_nested(self.t, Value::Int(i as i64))?,
        };
        if let Some(r) = lua.scratch.get_mut(self.roots + slot) {
            *r = v;
        }
        // Each lookup's probe steps (S5GC-1: a sort of keys at the end of long integer
        // chains was charged the longest walk once, at 30x plain code per unit of fuel).
        // An array-part read probes nothing: no thread-local read for it (what may be
        // pending is charged at the next one, the same fuel).
        if !in_arr {
            lua.charge_probes()?;
        }
        Ok(v)
    }

    /// `t[i] = vi` then `t[j] = vj` (C Lua's `set2`).
    fn set2(&mut self, lua: &mut Lua, i: usize, vi: Value, j: usize, vj: Value) -> Result<(), LuaError> {
        for (k, v) in [(i, vi), (j, vj)] {
            match self.t {
                Value::Table(tr) if lua.metatable(self.t).is_none() => {
                    // A slot of the array part: written straight (with the collector's
                    // barrier); anything else, and any write into the table the collector
                    // traverses in chunks (S6GC-2e), as any raw write.
                    let heap = &mut lua.heap;
                    let done = match heap.tables.get_mut(tr.idx) {
                        // (`Table::set` for an array slot: `absent` cleared, the slot
                        // written.)
                        Some(tab) if tr.idx != heap.part => match tab.arr_index(k as i64) {
                            Some(u) => {
                                tab.absent.set(0);
                                match tab.arr.get_mut(u) {
                                    Some(s) => {
                                        *s = v;
                                        true
                                    }
                                    None => false,
                                }
                            }
                            None => false,
                        },
                        _ => false,
                    };
                    if !done {
                        lua.raw_set(tr, Value::Int(k as i64), v)?;
                        lua.charge_probes()?;
                    } else if v.is_collectable() {
                        lua.heap.barrier_table(tr);
                    }
                }
                t => {
                    lua.set_index_nested(t, Value::Int(k as i64), v)?;
                    lua.charge_probes()?;
                }
            }
        }
        Ok(())
    }
}

/// Sorts slots `lo..=up`: Lua's algorithm with its reads and writes in C Lua's order, so a
/// bad order function is detected the same way and a failed sort leaves the same order.
fn auxsort(lua: &mut Lua, s: &Sorter, v: &mut InPlace, lo: usize, up: usize, mut rnd: u64) -> Result<(), LuaError> {
    let bad = || LuaError::msg("invalid order function for sorting");
    let (mut lo, mut up) = (lo, up);
    while lo < up {
        let (a_lo, a_up) = (v.get(lua, lo, 1)?, v.get(lua, up, 2)?);
        if s.less(lua, a_up, a_lo)? {
            v.set2(lua, lo, a_up, up, a_lo)?;
        }
        if up - lo == 1 {
            break;
        }
        let mut p = if up - lo < 100 || rnd == 0 {
            (lo + up) / 2
        } else {
            let r4 = (up - lo) / 4;
            (rnd as usize) % (r4 * 2) + lo + r4
        };
        let (a_p, a_lo) = (v.get(lua, p, 3)?, v.get(lua, lo, 1)?);
        if s.less(lua, a_p, a_lo)? {
            v.set2(lua, p, a_lo, lo, a_p)?;
        } else {
            let a_up = v.get(lua, up, 2)?;
            if s.less(lua, a_up, a_p)? {
                v.set2(lua, p, a_up, up, a_p)?;
            }
        }
        if up - lo == 2 {
            break;
        }
        let pivot = v.get(lua, p, 0)?;
        let a_up1 = v.get(lua, up - 1, 1)?;
        v.set2(lua, p, a_up1, up - 1, pivot)?;
        // Partition: a[lo..i] <= pivot <= a[j..up], a[up - 1] == pivot.
        let mut i = lo;
        let mut j = up - 1;
        loop {
            let ai = loop {
                i += 1;
                let ai = v.get(lua, i, 1)?;
                if !s.less(lua, ai, pivot)? {
                    break ai;
                }
                if i == up - 1 {
                    return Err(bad());
                }
            };
            let aj = loop {
                j -= 1;
                let aj = v.get(lua, j, 2)?;
                if !s.less(lua, pivot, aj)? {
                    break aj;
                }
                if j < i {
                    return Err(bad());
                }
            };
            if j < i {
                v.set2(lua, up - 1, ai, i, pivot)?;
                break;
            }
            v.set2(lua, i, aj, j, ai)?;
        }
        p = i;
        let n;
        if p - lo < up - p {
            if p > lo {
                auxsort(lua, s, v, lo, p - 1, rnd)?;
            }
            n = p - lo;
            lo = p + 1;
        } else {
            auxsort(lua, s, v, p + 1, up, rnd)?;
            n = up - p;
            if p == 0 {
                break;
            }
            up = p - 1;
        }
        if up > lo && (up - lo) / 128 > n {
            rnd = lua.next_random() | 1;
        }
    }
    Ok(())
}

fn sort(lua: &mut Lua, a: Args) -> R {
    let t = check_tab(lua, a, 0, RWL)?;
    let n = getn(lua, t)?;
    if n > 1 {
        if n >= i64::from(i32::MAX) {
            return Err(LuaError::Arg(1, "array too big".into()));
        }
        let comp = lua.arg(a, 1);
        if !comp.is_nil() && !matches!(comp, Value::Func(_)) {
            return Err(lua.type_error(a, 1, "function"));
        }
        lua.charge(n as u64)?;
        let roots = lua.scratch.len();
        lua.scratch.extend([Value::Nil; 4]);
        // The root slots are found by position: compaction stays above them (S5API-1).
        lua.scratch_floor = lua.scratch.len();
        auxsort(lua, &Sorter { comp }, &mut InPlace { t, roots }, 1, n as usize, 0)?;
    }
    lua.ret(&[])
}

