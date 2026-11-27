//! The utf8 library.

#[allow(unused_imports)]
use crate::nostd::prelude::*;
use super::R;
use crate::value::Value;
use crate::vm::{Args, Lua, LuaError};

const MAXUNICODE: u32 = 0x10FFFF;
const MAXUTF: u32 = 0x7FFF_FFFF;

pub(super) fn open(lua: &mut Lua) {
    let t = lua.lib_table("utf8");
    let fns: [(&str, &'static str, crate::vm::NativeFn); 4] = [
        ("char", "utf8.char", char_),
        ("codepoint", "utf8.codepoint", codepoint),
        ("len", "utf8.len", len),
        ("offset", "utf8.offset", offset),
    ];
    for (n, q, f) in fns {
        lua.set_fn(t, n, q, f);
    }
    // The iterators `codes` returns, made once: C Lua's are light C functions, equal on
    // every call. (No library table holds them: C Lua names them "?" in messages; S5COR-12.)
    let strict = lua.new_native(|l, a| iter(l, a, true), "?", Vec::new()).unwrap_or_default();
    let lax = lua.new_native(|l, a| iter(l, a, false), "?", Vec::new()).unwrap_or_default();
    let codes_fn = lua.new_native(codes, "utf8.codes", vec![strict, lax]).unwrap_or_default();
    lua.set_field(t, "codes", codes_fn);
    lua.names.utf8_iters = [strict.as_func(), lax.as_func()];
    lua.ctx_epoch += 1;
    let pat = lua.new_str(b"[\x00-\x7F\xC2-\xFD][\x80-\xBF]*").unwrap_or_default();
    lua.set_field(t, "charpattern", pat);
}

/// utf8's own position rule: negative from the end, too negative is 0 (no clamping up).
fn u_posrelat(pos: i64, len: usize) -> i64 {
    if pos >= 0 {
        pos
    } else if pos.unsigned_abs() > len as u64 {
        0
    } else {
        len as i64 + pos + 1
    }
}

fn iscont(c: u8) -> bool {
    c & 0xC0 == 0x80
}

/// Decodes one sequence at the start of `s`: (code, length).
fn decode(s: &[u8], strict: bool) -> Option<(u32, usize)> {
    const LIMITS: [u32; 6] = [!0, 0x80, 0x800, 0x10000, 0x200000, 0x4000000];
    let c = u32::from(*s.first()?);
    if c < 0x80 {
        return Some((c, 1));
    }
    let mut res: u32 = 0;
    let mut count = 0usize;
    let mut cc = c;
    while cc & 0x40 != 0 {
        count += 1;
        let next = u32::from(*s.get(count)?);
        if next & 0xC0 != 0x80 {
            return None;
        }
        res = (res << 6) | (next & 0x3F);
        cc <<= 1;
    }
    // A 0xFF lead takes up to 7 continuation bytes: refuse before shifting by 35.
    if count > 5 {
        return None;
    }
    res |= (cc & 0x7F) << (count * 5);
    if res > MAXUTF || res < *LIMITS.get(count)? {
        return None;
    }
    if strict && (res > MAXUNICODE || (0xD800..=0xDFFF).contains(&res)) {
        return None;
    }
    Some((res, count + 1))
}

fn char_(lua: &mut Lua, a: Args) -> R {
    let mut out = Vec::new();
    // Each argument is read and written: owed, paid on every exit (`owe`).
    super::owe(a.n as u64 / 8);
    for i in 0..a.n {
        let c = lua.check_int(a, i)?;
        if !(0..=i64::from(MAXUTF)).contains(&c) {
            return Err(LuaError::Arg(i + 1, "value out of range".into()));
        }
        crate::lexer::utf8_encode(c as u32, &mut out);
    }
    lua.ret_str(&out)
}

fn codepoint(lua: &mut Lua, a: Args) -> R {
    let s = lua.check_str(a, 0)?;
    let len = lua.bytes(s).len();
    let posi = u_posrelat(lua.opt_int(a, 1, 1)?, len);
    let pose = u_posrelat(lua.opt_int(a, 2, posi)?, len);
    let lax = lua.arg(a, 3).truthy();
    if posi < 1 {
        return Err(LuaError::Arg(2, "out of bounds".into()));
    }
    if pose > len as i64 {
        return Err(LuaError::Arg(3, "out of bounds".into()));
    }
    if posi > pose {
        return lua.ret(&[]);
    }
    let (posi, pose) = (posi as usize, pose as usize);
    if pose - posi >= (i32::MAX as usize).min(lua.limits.stack_slots / 2) {
        return Err(LuaError::msg("string slice too long"));
    }
    // At most one value per byte: memory for that is charged before anything is built.
    let most = (pose - posi + 1).saturating_mul(16).saturating_add(len);
    // Held until the call returns, then counted where the list is placed (SEC-M15); the
    // collector's debt as before.
    lua.charge_mem(most)?;
    lua.heap.mem.release(most);
    let mut hold = super::MemHold::default();
    hold.cover(lua, most)?;
    // Read in place (not copied); the bytes read are charged, also when one is invalid.
    // S6-5b (sweep 6's long property run): a chunk at a time between looks at the host's
    // interrupt and deadline; 4M code points ran 61 to 75 ms past the time limit.
    let mut out = Vec::new();
    let mut i = posi - 1;
    let mut bad = false;
    while i < pose && !bad {
        let bytes = lua.heap.str_bytes(s);
        let stop = out.len() + super::NATIVE_CHUNK;
        while i < pose && out.len() < stop {
            match decode(bytes.get(i..).unwrap_or_default(), !lax) {
                Some((c, n)) => {
                    out.push(Value::Int(i64::from(c)));
                    i += n;
                }
                None => {
                    bad = true;
                    break;
                }
            }
        }
        if i < pose && !bad && let Err(e) = lua.check_time() {
            super::owe(out.len() as u64 / 8 + (i + 1 - posi) as u64 / 16 + 1);
            return Err(e);
        }
    }
    super::owe(out.len() as u64 / 8 + (i + 1 - posi) as u64 / 16 + 1);
    if bad {
        return Err(LuaError::msg("invalid UTF-8 code"));
    }
    // Handed over as the results, not copied into them.
    lua.rets = out;
    Ok(crate::vm::Flow::Return)
}

fn len(lua: &mut Lua, a: Args) -> R {
    let s = lua.check_str(a, 0)?;
    let l = lua.bytes(s).len();
    let posi0 = u_posrelat(lua.opt_int(a, 1, 1)?, l);
    let posj0 = u_posrelat(lua.opt_int(a, 2, -1)?, l);
    let lax = lua.arg(a, 3).truthy();
    let mut posi = posi0 - 1;
    if !(1 <= posi0 && posi <= l as i64) {
        return Err(LuaError::Arg(2, "initial position out of bounds".into()));
    }
    let posj = posj0 - 1;
    if posj >= l as i64 {
        return Err(LuaError::Arg(3, "final position out of bounds".into()));
    }
    let start = posi;
    let mut n = 0i64;
    let mut bad = false;
    // SEC-M4: a chunk at a time between looks at the host's interrupt and deadline (16 MiB
    // ran 12 ms past the time limit).
    let mut next_look = posi + super::NATIVE_CHUNK as i64 * 16;
    while posi <= posj {
        if posi >= next_look {
            lua.check_time()?;
            next_look = posi + super::NATIVE_CHUNK as i64 * 16;
        }
        let bytes = lua.heap.str_bytes(s);
        match decode(bytes.get(posi as usize..).unwrap_or_default(), !lax) {
            Some((_, k)) => {
                posi += k as i64;
                n += 1;
            }
            None => {
                bad = true;
                break;
            }
        }
    }
    // Charged for the bytes read, also when it stops at an invalid one.
    lua.charge(n as u64 / 8 + (posi - start).max(0) as u64 / 16 + 1)?;
    if bad {
        return lua.ret(&[Value::Nil, Value::Int(posi + 1)]);
    }
    lua.ret1(Value::Int(n))
}

fn offset(lua: &mut Lua, a: Args) -> R {
    let s = lua.check_str(a, 0)?;
    let len = lua.bytes(s).len() as i64;
    let mut n = lua.check_int(a, 1)?;
    let default_i = if n >= 0 { 1 } else { len + 1 };
    let i = lua.opt_int(a, 2, default_i)?;
    let bytes = lua.heap.str_bytes(s);
    let mut posi = if i >= 0 { i } else if i.unsigned_abs() > len as u64 { 0 } else { len + i + 1 } - 1;
    if !(0 <= posi && posi <= len) {
        return Err(LuaError::Arg(3, "position out of bounds".into()));
    }
    let at = |p: i64| bytes.get(p as usize).copied().unwrap_or(0);
    if n == 0 {
        let from = posi;
        while posi > 0 && iscont(at(posi)) {
            posi -= 1;
        }
        lua.charge(posi.abs_diff(from) / 16 + 1)?;
        return lua.ret1(Value::Int(posi + 1));
    }
    if iscont(at(posi)) {
        return Err(LuaError::msg("initial position is a continuation byte"));
    }
    let from = posi;
    if n < 0 {
        while n < 0 && posi > 0 {
            loop {
                posi -= 1;
                if !(posi > 0 && iscont(at(posi))) {
                    break;
                }
            }
            n += 1;
        }
    } else {
        n -= 1;
        while n > 0 && posi < len {
            loop {
                posi += 1;
                if !iscont(at(posi)) {
                    break;
                }
            }
            n -= 1;
        }
    }
    // A unit per 16 bytes walked.
    lua.charge(posi.abs_diff(from) / 16 + 1)?;
    if n == 0 { lua.ret1(Value::Int(posi + 1)) } else { lua.ret1(Value::Nil) }
}

fn codes(lua: &mut Lua, a: Args) -> R {
    let s = lua.check_str(a, 0)?;
    let lax = lua.arg(a, 1).truthy();
    if lua.bytes(s).first().is_some_and(|&c| iscont(c)) {
        return Err(LuaError::Arg(1, "invalid UTF-8 code".into()));
    }
    let f = lua.native_upval(a, usize::from(lax));
    lua.ret(&[f, Value::Str(s), Value::Int(0)])
}

fn iter(lua: &mut Lua, a: Args, strict: bool) -> R {
    let s = lua.check_str(a, 0)?;
    // C's `iter_aux` reads the control with an unchecked `lua_tointeger` (anything that is
    // not an integer is 0) as an unsigned index: a negative one is past the end (LR-9).
    let control = lua.tointeger(lua.arg(a, 1)).unwrap_or(0);
    let (cost, r) = step(lua.bytes(s), control, strict);
    if cost > 0 {
        lua.charge(cost)?;
    }
    match r {
        Some(Some((i, c))) => lua.ret(&[Value::Int(i), Value::Int(c)]),
        Some(None) => lua.ret(&[]),
        None => Err(LuaError::msg("invalid UTF-8 code")),
    }
}

/// One step of `codes`' iterators (the fast loop's in place too): its charge, then the
/// next (position, code), Some(None) at the end, None on an invalid sequence.
pub(crate) fn step(bytes: &[u8], control: i64, strict: bool) -> (u64, Option<Option<(i64, i64)>>) {
    let mut i = usize::try_from(control).unwrap_or(usize::MAX);
    let len = bytes.len();
    // Skip the continuations of the current character (from any index, 0 included).
    let from = i;
    while i < len && bytes.get(i).is_some_and(|&b| iscont(b)) {
        i += 1;
    }
    // A unit per 16 continuation bytes skipped (none in valid UTF-8, where a character has
    // at most 3), so a run of them called on directly is paid for.
    let cost = i.saturating_sub(from) as u64 / 16;
    if i >= len {
        return (cost, Some(None));
    }
    let r = decode(bytes.get(i..).unwrap_or_default(), strict)
        .filter(|&(_, n)| !bytes.get(i + n).is_some_and(|&b| iscont(b)))
        .map(|(c, _)| Some((i as i64 + 1, i64::from(c))));
    (cost, r)
}
