//! The base library.

#[allow(unused_imports)]
use crate::nostd::prelude::*;
use super::R;
use crate::value::Value;
use crate::vm::{Args, Flow, Lua, LuaError, Tm};
use crate::{Dialect, Libs};

pub(super) fn open(lua: &mut Lua, libs: Libs) {
    let g = lua.globals;
    let fns: [(&str, &'static str, crate::vm::NativeFn); 15] = [
        ("assert", "assert", assert),
        ("error", "error", error),
        ("getmetatable", "getmetatable", getmetatable),
        ("setmetatable", "setmetatable", setmetatable),
        ("rawequal", "rawequal", rawequal),
        ("rawget", "rawget", rawget),
        ("rawlen", "rawlen", rawlen),
        ("rawset", "rawset", rawset),
        ("select", "select", select),
        ("tonumber", "tonumber", tonumber),
        ("tostring", "tostring", tostring),
        ("type", "type", type_),
        ("pcall", "pcall", pcall),
        ("xpcall", "xpcall", xpcall),
        ("warn", "warn", warn),
    ];
    for (name, q, f) in fns {
        lua.set_fn(g, name, q, f);
    }
    // The iterators are upvalues of `ipairs` and `pairs`.
    // (No library table holds it: C Lua names it "?" in messages.)
    let aux = lua.new_native(ipairs_aux, "?", Vec::new()).unwrap_or_default();
    let ip = lua.new_native(ipairs, "ipairs", vec![aux]).unwrap_or_default();
    lua.set_field(g, "ipairs", ip);
    // `pairs` returns the global `next` itself (as C Lua's `pairs(t) == next`).
    let nx = lua.new_native(next, "next", Vec::new()).unwrap_or_default();
    lua.set_field(g, "next", nx);
    let pa = lua.new_native(pairs, "pairs", vec![nx]).unwrap_or_default();
    lua.set_field(g, "pairs", pa);
    // The generic `for` runs these two in place (the fast loop's iterator cases).
    lua.names.next_fn = nx.as_func();
    lua.names.ipairs_aux = aux.as_func();
    lua.ctx_epoch += 1;
    // `tostring` and `type` run in place (`string::in_place`).
    super::string::register_in_place(lua, g, 8, &["tostring", "type"]);
    if libs.contains(Libs::PRINT) {
        lua.set_fn(g, "print", "print", print);
    }
    if libs.contains(Libs::LOAD) {
        lua.set_fn(g, "load", "load", load);
    }
    if libs.contains(Libs::GC) {
        lua.set_fn(g, "collectgarbage", "collectgarbage", collectgarbage);
        let c = lua.new_str(b"collectgarbage").map_or(Value::Nil, |k| lua.raw_get(g, k));
        lua.names.collect_fn = c.as_func();
    }
    lua.set_field(g, "_G", Value::Table(g));
    let v = lua.new_str(b"Lua 5.4").unwrap_or_default();
    lua.set_field(g, "_VERSION", v);
}

fn assert(lua: &mut Lua, a: Args) -> R {
    let v = lua.check_any(a, 0)?;
    if v.truthy() {
        let vals: Vec<Value> = (0..a.n).map(|i| lua.arg(a, i)).collect();
        return lua.ret(&vals);
    }
    let msg = lua.arg(a, 1);
    if a.n < 2 {
        return Err(LuaError::msg("assertion failed!"));
    }
    raise(lua, msg, 1)
}

fn raise(lua: &mut Lua, v: Value, level: i64) -> R {
    if let Value::Str(s) = v
        && level > 0
    {
        lua.charge_error()?;
        let w = lua.where_(usize::try_from(level).unwrap_or(1));
        if !w.is_empty() {
            // The position must not push a message past the string limit: cut it. Paid
            // for before it is copied.
            let room = lua.limits.string_len.saturating_sub(w.len());
            let take = lua.bytes(s).len().min(room);
            lua.charge_bytes(w.len() + take)?;
            let mut b = w.into_bytes();
            b.extend_from_slice(lua.bytes(s).get(..take).unwrap_or_default());
            let m = lua.new_str(&b)?;
            return Err(LuaError::Value(m));
        }
    }
    Err(LuaError::Value(v))
}

fn error(lua: &mut Lua, a: Args) -> R {
    let level = lua.opt_int(a, 1, 1)?;
    let v = lua.arg(a, 0);
    raise(lua, v, level)
}

fn getmetatable(lua: &mut Lua, a: Args) -> R {
    let v = lua.check_any(a, 0)?;
    let Some(mt) = lua.metatable(v) else { return lua.ret1(Value::Nil) };
    let protected = lua.tm_in(mt, Tm::Metatable);
    if !protected.is_nil() {
        return lua.ret1(protected);
    }
    lua.ret1(Value::Table(mt))
}

fn setmetatable(lua: &mut Lua, a: Args) -> R {
    let t = lua.check_table(a, 0)?;
    if a.n < 2 {
        return Err(LuaError::Arg(2, "nil or table expected, got no value".into()));
    }
    let mt = match lua.arg(a, 1) {
        Value::Nil => None,
        Value::Table(m) => Some(m),
        _ => return Err(lua.type_error(a, 1, "nil or table")),
    };
    if let Some(old) = lua.metatable(Value::Table(t))
        && !lua.tm_in(old, Tm::Metatable).is_nil()
    {
        return Err(LuaError::msg("cannot change a protected metatable"));
    }
    if let Some(m) = mt
        && !lua.tm_in(m, Tm::Gc).is_nil()
    {
        return Err(LuaError::msg("__gc metamethods are not supported in RemedyLua"));
    }
    if let Some(tab) = lua.heap.tables.get_mut(t.idx) {
        tab.meta = mt;
    }
    lua.heap.barrier_table(t);
    lua.ret1(Value::Table(t))
}

fn ipairs(lua: &mut Lua, a: Args) -> R {
    let t = lua.check_any(a, 0)?;
    let aux = lua.native_upval(a, 0);
    lua.ret(&[aux, t, Value::Int(0)])
}

fn ipairs_aux(lua: &mut Lua, a: Args) -> R {
    let t = lua.arg(a, 0);
    let i = lua.check_int(a, 1)?.wrapping_add(1);
    let v = match t {
        Value::Table(tr) if lua.metatable(t).is_none() => lua.raw_get(tr, Value::Int(i)),
        _ => lua.index_nested(t, Value::Int(i))?,
    };
    if v.is_nil() { lua.ret1(Value::Nil) } else { lua.ret(&[Value::Int(i), v]) }
}

fn pairs(lua: &mut Lua, a: Args) -> R {
    let t = lua.check_any(a, 0)?;
    let mm = lua.tm(t, Tm::Pairs);
    if !mm.is_nil() {
        return Ok(Flow::Call3 { f: mm, arg: t });
    }
    let next_fn = lua.native_upval(a, 0);
    lua.ret(&[next_fn, t, Value::Nil])
}

fn next(lua: &mut Lua, a: Args) -> R {
    let t = lua.check_table(a, 0)?;
    let k = lua.arg(a, 1);
    // The key `next` gave last is the table's own (`at_next_hint`): nothing to settle.
    let hinted = lua.heap.tables.get(t.idx).is_some_and(|tab| tab.at_next_hint(k).is_some());
    let k = if hinted {
        k
    } else {
        match lua.settle_key(k)? {
            crate::vm::Settled::Absent(_) => k,
            _ => lua.heap.retry_key(k).unwrap_or(k),
        }
    };
    let r = lua.heap.tables.get(t.idx).map(|tab| tab.next_counted(k));
    // Skipping empty slots is work: charged (see `Table::next_counted`).
    if let Some(Ok((_, cost))) = r
        && cost > 0
    {
        lua.charge(cost as u64)?;
    }
    match r {
        Some(Ok((Some((k, v)), _))) => lua.ret(&[k, v]),
        Some(Ok((None, _))) | None => lua.ret1(Value::Nil),
        // Raised inside `next` itself in C Lua: no position.
        Some(Err(())) => Err(LuaError::Plain("invalid key to 'next'".into())),
    }
}

fn rawequal(lua: &mut Lua, a: Args) -> R {
    let x = lua.check_any(a, 0)?;
    let y = lua.check_any(a, 1)?;
    let eq = match (x, y) {
        (Value::Str(sa), Value::Str(sb)) => lua.str_eq(sa, sb)?,
        _ => x.raw_eq(y),
    };
    lua.ret1(Value::Bool(eq))
}

fn rawget(lua: &mut Lua, a: Args) -> R {
    let t = lua.check_table(a, 0)?;
    let k = lua.check_any(a, 1)?;
    let v = lua.get_in_settled(t.idx, k)?;
    lua.ret1(v)
}

fn rawlen(lua: &mut Lua, a: Args) -> R {
    let n = match lua.arg(a, 0) {
        Value::Table(t) => lua.raw_len(t),
        Value::Str(s) => lua.bytes(s).len() as i64,
        _ => return Err(lua.type_error(a, 0, "table or string")),
    };
    lua.ret1(Value::Int(n))
}

fn rawset(lua: &mut Lua, a: Args) -> R {
    let t = lua.check_table(a, 0)?;
    let k = lua.check_any(a, 1)?;
    let v = lua.check_any(a, 2)?;
    // C Lua raises the nil/NaN key error inside `rawset` itself: no position.
    lua.raw_set(t, k, v).map_err(LuaError::plain)?;
    lua.ret1(Value::Table(t))
}

fn select(lua: &mut Lua, a: Args) -> R {
    let n = a.n as i64 - 1;
    // C Lua looks at the first byte only: `select("#x", ...)` counts too (S5COR-17).
    if let Value::Str(s) = lua.arg(a, 0)
        && lua.bytes(s).first() == Some(&b'#')
    {
        return lua.ret1(Value::Int(n));
    }
    let i = lua.check_int(a, 0)?;
    let start = if i < 0 {
        if i < -n {
            return Err(LuaError::Arg(1, "index out of range".into()));
        }
        n + i
    } else if i == 0 {
        return Err(LuaError::Arg(1, "index out of range".into()));
    } else {
        (i - 1).min(n)
    };
    let vals: Vec<Value> = ((start + 1) as usize..a.n).map(|j| lua.arg(a, j)).collect();
    lua.charge(vals.len() as u64 / 8)?;
    lua.ret(&vals)
}

fn tonumber(lua: &mut Lua, a: Args) -> R {
    if lua.arg(a, 1).is_nil() {
        let v = lua.check_any(a, 0)?;
        let n = lua.tonumber_charged(v)?.unwrap_or(Value::Nil);
        return lua.ret1(n);
    }
    let base = lua.check_int(a, 1)?;
    let s = match lua.arg(a, 0) {
        Value::Str(s) => s,
        _ => return Err(lua.type_error(a, 0, "string")),
    };
    if !(2..=36).contains(&base) {
        return Err(LuaError::Arg(2, "base out of range".into()));
    }
    // A unit per 8 bytes read (the same rate as `tonumber` without a base).
    lua.charge(lua.bytes(s).len() as u64 / 8 + 1)?;
    let n = crate::number::str_to_int_base(lua.bytes(s), base as u32);
    lua.ret1(n.map_or(Value::Nil, Value::Int))
}

fn tostring(lua: &mut Lua, a: Args) -> R {
    let v = lua.check_any(a, 0)?;
    let s = lua.tostring(v)?;
    lua.ret1(s)
}

fn type_(lua: &mut Lua, a: Args) -> R {
    let v = lua.check_any(a, 0)?;
    lua.ret1(Value::Str(lua.names.type_of(v)))
}

fn pcall(lua: &mut Lua, a: Args) -> R {
    lua.check_any(a, 0)?;
    Ok(Flow::PCall { handler: None, nargs: a.n - 1 })
}

fn xpcall(lua: &mut Lua, a: Args) -> R {
    let h = lua.arg(a, 1);
    // C: `luaL_checktype(L, 2, LUA_TFUNCTION)`; a callable table is not enough.
    if !matches!(h, Value::Func(_)) {
        return Err(lua.type_error(a, 1, "function"));
    }
    // f, h, args… → f, args…
    for i in 2..a.n {
        let v = lua.arg(a, i);
        lua.set_st(a.base + i - 1, v);
    }
    Ok(Flow::PCall { handler: Some(h), nargs: a.n - 2 })
}

fn print(lua: &mut Lua, a: Args) -> R {
    // Joined only up to what the output limit still allows.
    let room = lua.limits.output.saturating_sub(lua.output_used);
    // Each argument is converted (a number makes a string) even past the output limit:
    // a unit each, paid on every exit (`owe`).
    super::owe(a.n as u64);
    let mut line = Vec::new();
    // Counted against memory while a later argument's `__tostring` runs (SEC-M1).
    let mut hold = super::MemHold::default();
    for i in 0..a.n {
        let v = lua.arg(a, i);
        let s = lua.tostring(v)?;
        if i > 0 {
            line.push(b'\t');
        }
        if let Value::Str(s) = s {
            let b = lua.bytes(s);
            let take = b.len().min(room.saturating_sub(line.len()));
            line.extend_from_slice(b.get(..take).unwrap_or_default());
            lua.charge(b.len() as u64 / 64)?;
            hold.cover(lua, line.capacity())?;
        }
    }
    line.push(b'\n');
    emit_output(lua, &line)?;
    lua.ret(&[])
}

/// Writes to the host's sink or buffer, within the output limit. The buffer is charged to
/// the memory account until the host takes it (`take_output`): with the output limit
/// lifted and no sink, it grew outside the memory limit (sweep 6, from sec/metering).
pub(crate) fn emit_output(lua: &mut Lua, b: &[u8]) -> Result<(), LuaError> {
    // Counted per call from the host, wherever the text goes.
    let mut room = lua.limits.output.saturating_sub(lua.output_used);
    if lua.print_sink.is_none() {
        // Nor may the buffer outgrow it when the host does not take it.
        room = room.min(lua.limits.output.saturating_sub(lua.output.len()));
    }
    let take = b.len().min(room);
    lua.output_used += take;
    let part = b.get(..take).unwrap_or_default();
    match lua.print_sink.as_mut() {
        Some(sink) => sink(&String::from_utf8_lossy(part)),
        None => {
            if lua.output.len() + part.len() > lua.output.capacity() {
                let want = (lua.output.len() + part.len()).max(lua.output.capacity() * 2);
                let grow = want.saturating_sub(lua.output_charged);
                lua.charge_mem(grow)?;
                lua.output_charged += grow;
                lua.output.reserve_exact(want - lua.output.len());
            }
            lua.output.extend_from_slice(part);
        }
    }
    Ok(())
}

fn warn(lua: &mut Lua, a: Args) -> R {
    let first = lua.check_str(a, 0)?;
    // A one-piece message starting with '@' is a control message (C's `checkcontrol`):
    // "@on" and "@off" switch warnings, others are ignored. Warnings start off.
    if a.n == 1 && lua.bytes(first).starts_with(b"@") {
        match lua.bytes(first) {
            b"@on" => lua.warn_on = true,
            b"@off" => lua.warn_on = false,
            _ => {}
        }
        return lua.ret(&[]);
    }
    let room = lua.limits.output.saturating_sub(lua.output_used);
    // Each piece is converted (a number makes a string): a unit each, paid on every exit.
    super::owe(a.n as u64);
    // The line, counted against memory as it is joined (SEC-M8: with the output limit
    // lifted, a hundred arguments of one 4 MiB string joined 400 MB outside the account).
    let mut line = b"Lua warning: ".to_vec();
    let mut hold = super::MemHold::default();
    for i in 0..a.n {
        let s = lua.check_str(a, i)?;
        let b = lua.bytes(s);
        let take = b.len().min(room.saturating_sub(line.len() - 13));
        line.extend_from_slice(b.get(..take).unwrap_or_default());
        lua.charge(b.len() as u64 / 64)?;
        hold.cover(lua, line.capacity())?;
    }
    if lua.warn_on && lua.libs.contains(Libs::PRINT) {
        line.push(b'\n');
        emit_output(lua, &line)?;
    }
    lua.ret(&[])
}

/// Fuel per token `load` compiles (see `token_estimate`).
const LOAD_TOKEN_FUEL: u64 = 6;
/// Fuel for the message of a syntax error (`load` returning nil and the error).
const SYNTAX_ERROR_FUEL: u64 = 400;

/// About how many tokens `src` has, counted in one pass before anything is compiled: each
/// punctuation byte, and each run of letters, digits and `_`.
/// Looks at the host's interrupt and deadline between pieces of this many source bytes
/// (copying and scanning a 6 MB source took several milliseconds with no look).
const SOURCE_PIECE: usize = 1 << 18;

/// (`after` is the byte before `src`, for text read a piece at a time.)
fn token_estimate(lua: &mut Lua, src: &[u8], after: Option<u8>) -> Result<u64, LuaError> {
    let word = |c: u8| c.is_ascii_alphanumeric() || c == b'_';
    let mut n = 0u64;
    let mut prev_word = after.is_some_and(word);
    for piece in src.chunks(SOURCE_PIECE) {
        if src.len() > SOURCE_PIECE {
            lua.check_time()?;
        }
        for &c in piece {
            let w = word(c);
            if (w && !prev_word) || (!w && c.is_ascii_punctuation()) {
                n += 1;
            }
            prev_word = w;
        }
    }
    Ok(n)
}

fn load(lua: &mut Lua, a: Args) -> R {
    // The source is gathered in a buffer of our own, charged to the memory account while
    // it is held (and so taken out of the compile's room), released when `load` returns
    // (S6-FRONT-1: it was not charged, and a reader calling `load` with another reader
    // held one 16 MiB buffer per level, 280x the cap).
    let mut charged = 0;
    let r = load_charged(lua, a, &mut charged);
    lua.heap.mem.release(charged);
    r
}

/// Grows `src` to hold `more` bytes more, charging the capacity it gains to `charged`.
fn grow_source(lua: &mut Lua, src: &mut Vec<u8>, more: usize, charged: &mut usize) -> Result<(), LuaError> {
    let cap = src.capacity();
    if src.len() + more <= cap {
        return Ok(());
    }
    // Doubling (so a reader's many small pieces are copied a few times in all), but never
    // past the longest source a reader may give.
    let want = cap.saturating_mul(2).min(lua.limits.string_len).max(src.len() + more);
    lua.charge_mem(want - cap)?;
    *charged += want - cap;
    src.reserve_exact(want - src.len());
    let got = src.capacity();
    lua.heap.mem.settle(want, got);
    *charged = *charged + got - want;
    Ok(())
}

fn load_charged(lua: &mut Lua, a: Args, charged: &mut usize) -> R {
    let mut chunk = lua.arg(a, 0);
    // A number is a string chunk, as `lua_tolstring` makes it.
    if matches!(chunk, Value::Int(_) | Value::Num(_)) {
        chunk = Value::Str(lua.check_str(a, 0)?);
    }
    // C Lua checks the mode (#3), then the name (#2), then that a non-string chunk is a
    // function (#1), before reading anything (S5C-COR-5: the chunk was checked first, and
    // the name and mode only after a reader had been read whole).
    let mode_arg = lua.opt_str(a, 2)?;
    let name_arg = lua.opt_str(a, 1)?;
    if !matches!(chunk, Value::Str(_) | Value::Func(_)) {
        return Err(lua.type_error(a, 0, "function"));
    }
    let mut src: Vec<u8> = Vec::new();
    let mut reading = None;
    let default_name;
    match chunk {
        Value::Str(s) => {
            let n = lua.bytes(s).len();
            grow_source(lua, &mut src, n, charged)?;
            // A piece at a time between looks at the interrupt and deadline.
            while src.len() < n {
                if n > SOURCE_PIECE {
                    lua.check_time()?;
                }
                let at = src.len();
                src.extend_from_slice(lua.bytes(s).get(at..n.min(at + SOURCE_PIECE)).unwrap_or_default());
            }
            // The name shown (`chunk_id`) reads only the start (the end for `@`): a long
            // source is not converted whole, so a huge `load` reaches its charge and the
            // compile's interrupt checks quickly.
            default_name = if src.len() <= 2048 {
                String::from_utf8_lossy(&src).into_owned()
            } else if src.first() == Some(&b'@') {
                "@...".to_owned() + &String::from_utf8_lossy(src.get(src.len() - 1024..).unwrap_or_default())
            } else {
                String::from_utf8_lossy(src.get(..1024).unwrap_or_default()).into_owned()
            };
        }
        Value::Func(_) => {
            default_name = "=(load)".into();
            // C Lua reads the first character before it checks the mode; the rest is read
            // as the parser needs it (`compile_chunk`).
            let mut st = Reading { f: chunk, charged: 0, error: None, ended: false };
            read_piece(lua, &mut st, &mut src);
            *charged = st.charged;
            reading = Some(st);
        }
        // C Lua: anything not a string must be a reader function.
        _ => return Err(lua.type_error(a, 0, "function")),
    }
    let name = match name_arg {
        Some(s) => {
            // A name is copied and scanned for its display form: charged like source.
            lua.charge(lua.bytes(s).len() as u64 / 2)?;
            String::from_utf8_lossy(lua.bytes(s)).into_owned()
        }
        None => default_name,
    };
    if let Some(e) = reading.as_mut().and_then(|r| r.error.take()) {
        return load_failed(lua, e);
    }
    if let Some(mode) = mode_arg {
        // Scanned whole for 't' (and shown cut to the message limit): charged.
        lua.charge(lua.bytes(mode).len() as u64 / 64)?;
        let b = lua.bytes(mode);
        if !b.contains(&b't') {
            let shown = String::from_utf8_lossy(b.get(..b.len().min(lua.limits.error_len)).unwrap_or_default()).into_owned();
            let text = format!("attempt to load a text chunk (mode is '{shown}')");
            let m = lua.new_str(text.as_bytes())?;
            return lua.ret(&[Value::Nil, m]);
        }
    }
    let env = if a.n >= 4 { lua.arg(a, 3) } else { Value::Table(lua.globals) };
    // `debug.getinfo`'s `source`: the name given, else the source string itself (as in C
    // Lua), else "=(load)" for a reader. No copy: the string is the argument.
    let cname = match (name_arg, chunk) {
        (Some(s), _) | (None, Value::Str(s)) => Value::Str(s),
        _ => Value::Nil,
    };
    let r = match reading.as_mut() {
        Some(st) => compile_text(lua, Src::Reader(st, &mut src), &name, env, cname),
        None => compile_text(lua, Src::Text(&src), &name, env, cname),
    };
    if let Some(st) = reading {
        *charged = st.charged;
        if let Some(e) = st.error {
            return load_failed(lua, e);
        }
    }
    match r? {
        Ok(f) => lua.ret1(f),
        Err(m) => lua.ret(&[Value::Nil, m]),
    }
}

/// `load`'s result when its reader failed: the error as its message (a limit goes on).
fn load_failed(lua: &mut Lua, e: LuaError) -> R {
    let m = lua.normalize(e)?;
    lua.ret(&[Value::Nil, m])
}

/// `load`'s reader as the parser calls it: the function, the memory charged for what it
/// gave (released when `load` returns), and the error that ended the reading.
pub(crate) struct Reading {
    f: Value,
    charged: usize,
    error: Option<LuaError>,
    /// It returned the end, or failed: not called again.
    ended: bool,
}

/// Reads the reader's next piece onto `buf`: false at the end (nil or an empty string) or
/// on an error, kept in `st` (C Lua's `generic_reader`).
pub(crate) fn read_piece(lua: &mut Lua, st: &mut Reading, buf: &mut Vec<u8>) -> bool {
    if st.ended {
        return false;
    }
    let r = (|| {
        let piece = lua.call_nested(st.f, &[])?.first().copied().unwrap_or_default();
        let piece = match piece {
            Value::Int(_) | Value::Num(_) => {
                let text = match piece {
                    Value::Int(i) => i.to_string(),
                    _ => piece.as_num().map(crate::number::fmt_float).unwrap_or_default(),
                };
                lua.new_str(text.as_bytes())?
            }
            Value::Nil => return Ok(false),
            Value::Str(_) => piece,
            // C's `luaL_error`: positioned at load's caller.
            _ => return Err(LuaError::Msg("reader function must return a string".into())),
        };
        let Value::Str(s) = piece else { return Ok(false) };
        let n = lua.bytes(s).len();
        if n == 0 {
            return Ok(false);
        }
        lua.check_len(buf.len() + n)?;
        grow_source(lua, buf, n, &mut st.charged)?;
        // The call, the copy and compiling the piece (`compile_text`'s charge, paid before
        // it is compiled), counted on the text read in all: the same however it is split.
        let (at, after) = (buf.len(), buf.last().copied());
        buf.extend_from_slice(lua.bytes(s));
        let tokens = token_estimate(lua, buf.get(at..).unwrap_or_default(), after)?;
        let copied = buf.len() as u64 / 32 - at as u64 / 32;
        lua.charge(1 + copied + n as u64 + LOAD_TOKEN_FUEL * tokens)?;
        Ok(true)
    })();
    let more = r.unwrap_or_else(|e| {
        st.error = Some(e);
        false
    });
    st.ended = !more;
    more
}

/// What `compile_text` compiles: text, or what a reader gives (read onto the buffer as
/// the parser needs it).
pub(crate) enum Src<'s> {
    Text(&'s [u8]),
    Reader(&'s mut Reading, &'s mut Vec<u8>),
}

impl Src<'_> {
    /// The text, or what has been read.
    pub(crate) fn bytes(&self) -> &[u8] {
        match self {
            Src::Text(s) => s,
            Src::Reader(_, b) => b,
        }
    }
}

/// Compiles source text as `load` does, charged the same: the function (with `chunk_name`
/// as its `debug.getinfo` source; nil means `name` itself), or the syntax error's message.
pub(crate) fn compile_text(
    lua: &mut Lua,
    mut src: Src<'_>,
    name: &str,
    env: Value,
    chunk_name: Value,
) -> Result<Result<Value, Value>, LuaError> {
    // Compiling costs about a unit per source byte plus a few per token (calibrated:
    // `examples/fuelcal.rs`; dense code such as deep brackets costs most per byte), paid
    // before (a reader's text as it is read: `read_piece`).
    let pay = |lua: &mut Lua, src: &Src<'_>, extra: u64| {
        let tokens = token_estimate(lua, src.bytes(), None)?;
        lua.charge(src.bytes().len() as u64 + LOAD_TOKEN_FUEL * tokens + extra)
    };
    match src {
        Src::Text(_) => pay(lua, &src, 100)?,
        Src::Reader(..) => lua.charge(100)?,
    }
    let dialect = Dialect::for_name(name.trim_start_matches(['=', '@']));
    let mut r = lua.compile_chunk(&mut src, name, dialect, env);
    if matches!(r, Err(crate::CompileError::Memory)) {
        lua.full_gc()?;
        r = lua.compile_chunk(&mut src, name, dialect, env);
    }
    match r {
        Ok(f) => {
            let cname = match chunk_name {
                Value::Str(_) => chunk_name,
                _ => lua.new_str(name.as_bytes())?,
            };
            lua.set_chunk_name(f, cname);
            Ok(Ok(f))
        }
        Err(crate::CompileError::Memory) => Err(LuaError::Limit(crate::LimitKind::Memory)),
        Err(crate::CompileError::Limit(k)) => Err(LuaError::Limit(k)),
        Err(crate::CompileError::Syntax(d)) => {
            // A syntax error is parsed again up to the error (to report what C Lua would)
            // and its message built: about twice the compile, plus a fixed part.
            pay(lua, &src, SYNTAX_ERROR_FUEL)?;
            let text = d.lua_message_bytes();
            Ok(Err(lua.new_str(&text)?))
        }
    }
}

/// A pause or step multiplier as C Lua keeps it (at most 1023 %, in steps of 4).
fn gc_param(n: i64) -> usize {
    (n.clamp(0, 1023) as usize) / 4 * 4
}

fn collectgarbage(lua: &mut Lua, a: Args) -> R {
    // Only the first bytes can name an option or show in the message: a long string is not
    // copied whole (that was an uncharged copy of up to 16 MB per call). A cut string is
    // still no option's name, since every name is short.
    let opt = match lua.opt_str(a, 0)? {
        Some(s) => {
            let b = lua.bytes(s);
            String::from_utf8_lossy(b.get(..b.len().min(lua.limits.error_len)).unwrap_or_default()).into_owned()
        }
        None => "collect".into(),
    };
    match opt.as_str() {
        "collect" => {
            lua.full_gc()?;
            lua.ret1(Value::Int(0))
        }
        "count" => {
            let kb = lua.memory_used() as f64 / 1024.0;
            lua.ret1(Value::Num(kb))
        }
        "step" => {
            // `n` (cast to `int`, as C does) is 0 for one basic step: the collector's own
            // step, true if the cycle ended or began in it (C's basic step is bigger:
            // it ends the cycle of a small heap). For `n` > 0, C's `incstep` after `n`
            // KiB more debt: steps until that debt and a step size of credit are worked
            // off, `(n * 1024 / 16 + 2^13 / 16) * stepmul` of C's work units, or the cycle
            // ends; true only when it ended. Counted in steps of the usual size (a step
            // does `2^13 * stepmul / 800` units), so neither the fuel nor where the loop
            // stops follows how a cycle's work splits into steps (which follows the hash
            // seed), and each step is charged as it ends. A negative `n` adds no debt: no
            // step.
            let kb = lua.opt_int(a, 1, 0)? as i32;
            if kb < 0 {
                return lua.ret1(Value::Bool(false));
            }
            let before = lua.heap.cycle;
            let stopped = crate::nostd::mem::replace(&mut lua.heap.stopped, false);
            let r = (|| -> Result<bool, LuaError> {
                if kb == 0 {
                    // (Called from Lua code, its step ran at the call, `Lua::collect_step`.)
                    if let Some(at) = lua.gc_call_before.take() {
                        return Ok(lua.heap.phase == crate::heap::Phase::Pause || lua.heap.cycle != at);
                    }
                    lua.heap.mem.debt = lua.heap.mem.debt.max(crate::heap::STEP_SIZE);
                    lua.gc_step()?;
                    return Ok(lua.heap.phase == crate::heap::Phase::Pause || lua.heap.cycle != before);
                }
                if lua.heap.blocked > 0 {
                    return Ok(false);
                }
                let debt = i64::from(kb) * 1024 + i64::try_from(lua.heap.mem.debt.max(0)).unwrap_or(i64::MAX);
                let step = crate::heap::STEP_SIZE as i64;
                let units = (debt / 16 + step / 16).saturating_mul(800);
                let steps = units / step + i64::from(units % step != 0);
                for _ in 0..steps {
                    lua.heap.mem.debt = crate::heap::STEP_SIZE;
                    lua.gc_step()?;
                    if lua.heap.phase == crate::heap::Phase::Pause {
                        return Ok(true);
                    }
                }
                Ok(false)
            })();
            lua.heap.stopped = stopped;
            let done = r?;
            lua.ret1(Value::Bool(done))
        }
        "isrunning" => lua.ret1(Value::Bool(!lua.heap.stopped)),
        "incremental" | "generational" => {
            // Only the incremental collector exists; the mode a script set is remembered,
            // and `incremental`'s pause and step multiplier are used.
            if opt == "incremental" {
                let pause = lua.opt_int(a, 1, 0)?;
                let stepmul = lua.opt_int(a, 2, 0)?;
                if pause != 0 {
                    lua.heap.pause = gc_param(pause);
                }
                if stepmul != 0 {
                    lua.heap.stepmul = gc_param(stepmul);
                }
            }
            let prev = if lua.gc_generational { "generational" } else { "incremental" };
            lua.gc_generational = opt == "generational";
            lua.ret_str(prev.as_bytes())
        }
        "stop" | "restart" => {
            lua.heap.stopped = opt == "stop";
            lua.ret1(Value::Int(0))
        }
        "setpause" | "setstepmul" => {
            let n = gc_param(lua.opt_int(a, 1, 0)?);
            let slot = if opt == "setpause" { &mut lua.heap.pause } else { &mut lua.heap.stepmul };
            let prev = crate::nostd::mem::replace(slot, n);
            lua.ret1(Value::Int(prev as i64))
        }
        other => Err(LuaError::Arg(1, format!("invalid option '{other}'"))),
    }
}
