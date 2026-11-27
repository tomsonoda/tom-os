//! `debug.traceback` and a read-only `debug.getinfo` (`Libs::DEBUG_INFO`), as C Lua 5.4.7's
//! `ldblib.c`, `lauxlib.c` (`luaL_traceback`) and `ldebug.c` (`lua_getinfo`) make them.
//!
//! Nothing here writes: no hooks, locals, upvalues, metatables of other values, registry, or
//! user values. `getinfo` never hands out a function (its `f` option is refused, and `L`,
//! which needs it in C Lua, too), so a script cannot reach a function further up the stack
//! that it was not given (a trusted caller's closure).

#[allow(unused_imports)]
use crate::nostd::prelude::*;
use super::R;
use crate::bytecode::Instr;
use crate::heap::{CoStatus, Func, ThreadState};
use crate::value::{TableRef, ThreadRef, Value};
use crate::vm::{Act, Args, Lua, LuaError};

/// C Lua's `LEVELS1` and `LEVELS2`: a traceback shows the first 10 levels and the last 11.
const LEVELS1: usize = 10;
const LEVELS2: usize = 11;

// Calibrated against plain code.
/// Entries of the loaded modules looked at to name a function, per unit of fuel
/// (`pushglobalfuncname`).
const NAME_SEARCH_PER_FUEL: u64 = 1;
/// Levels of the stack walked to find the activations, per unit of fuel.
const LEVELS_PER_FUEL: usize = 2;
/// Bytecode or name units read to name a call site, per unit of fuel (`debug::take_work`).
const WORK_PER_FUEL: usize = 4;
/// Fuel for a call's fixed part (its result's strings and table).
const TRACEBACK_FUEL: u64 = 24;
const GETINFO_FUEL: u64 = 100;
/// Fuel per level a traceback shows (its line and name).
const LEVEL_TEXT_FUEL: u64 = 4;
/// Bytes of a traceback's text (and of its message) per unit of fuel.
const TEXT_PER_FUEL: usize = 16;

pub(crate) fn open(lua: &mut Lua) {
    let t = lua.lib_table("debug");
    lua.set_fn(t, "traceback", "debug.traceback", traceback);
    lua.set_fn(t, "getinfo", "debug.getinfo", getinfo);
}

/// What `lua_getinfo` reports about an activation or a function.
struct Info {
    /// "Lua", "C" or "main".
    what: &'static str,
    /// The chunk name as given (`=[C]` for a Rust function).
    source: Value,
    short_src: String,
    currentline: i64,
    linedefined: i64,
    lastlinedefined: i64,
    nups: i64,
    nparams: i64,
    isvararg: bool,
    /// (namewhat, name) from the call site.
    name: Option<(&'static str, String)>,
    istailcall: bool,
    /// The function itself (never given to the script: only to name it from the loaded
    /// modules).
    func: Option<Value>,
}

/// The thread argument (`getthread`): the thread and where the other arguments start.
fn thread_arg(lua: &Lua, a: Args) -> Result<(ThreadRef, usize), LuaError> {
    match lua.arg(a, 0) {
        Value::Thread(t) if lua.heap.is_live(Value::Thread(t)) => Ok((t, 1)),
        Value::Thread(_) => Err(LuaError::msg("attempt to use a thread whose object was collected")),
        _ => Ok((lua.cur_ref, 0)),
    }
}

fn thread_state(lua: &Lua, t: ThreadRef) -> Option<&ThreadState> {
    if t == lua.cur_ref { Some(&lua.cur) } else { lua.heap.threads.get(t.idx).map(|t| &**t) }
}

/// The activations of thread `t`, newest first (level 0 first), as `lua_getstack` counts
/// them. Charges the walk.
fn levels(lua: &mut Lua, t: ThreadRef) -> Result<Vec<Act>, LuaError> {
    let running = t == lua.cur_ref;
    // Charged before the walk, from its bound (every frame and native of the state, and the
    // levels this adds), so an error part-way cannot skip it.
    // (A native called by `pcall` is an activation for each protected call it sits on.)
    let natives: usize = lua.native_stack.iter().map(|&(.., under)| under + 1).sum();
    let bound = thread_state(lua, t).map_or(0, |ts| ts.frames.len()) + natives + 2;
    lua.charge((bound / LEVELS_PER_FUEL) as u64 + 1)?;
    let err_closing = if running { lua.err_closing } else { None };
    let extra =
        lua.handler_native.filter(|((nt, _, _), _)| running && *nt == t).map(|((_, d, slot), under)| (d, slot, under));
    // S6-2: a run of nested protected calls (`pcall(pcall, ..., f)`) is one frame but as many
    // activations: those past the bound are paid before they are listed.
    let count = thread_state(lua, t).map_or(0, |ts| lua.activation_count(ts, t, err_closing, extra));
    lua.charge((count.saturating_sub(bound) / LEVELS_PER_FUEL) as u64)?;
    let mut acts = match thread_state(lua, t) {
        Some(ts) => {
            let mut acts = lua.activations(ts, t, err_closing, extra);
            // A thread that is not running stopped inside a Rust function (`coroutine.yield`,
            // `coroutine.resume`, a wrap function): C Lua's level 0 there. It is the callee
            // of the top frame's call instruction.
            if !running
                && !matches!(ts.status, CoStatus::Dead)
                && let Some(f) = ts.frames.last()
            {
                let slot = lua.proto(f.proto).and_then(|p| match p.code.get(f.pc.wrapping_sub(1)) {
                    Some(Instr::Call { a, .. }) => Some(f.base + usize::from(*a)),
                    _ => None,
                });
                acts.push(Act::C(slot));
            }
            // One that died by an error raised in a library function keeps that function as
            // a level, above the frame that called it (LIBS-4).
            if !running
                && let Some((d, slot, _)) = ts.died_in
            {
                let at = acts.iter().position(|a| matches!(a, Act::Lua(i) if *i >= d)).unwrap_or(acts.len());
                acts.insert(at, Act::C(Some(slot)));
            }
            acts
        }
        None => Vec::new(),
    };
    // The base where Rust started the thread is no activation; one in the middle (never
    // made by the call protocol) would be a Rust function.
    if acts.first() == Some(&Act::Base) {
        acts.remove(0);
    }
    for a in &mut acts {
        if *a == Act::Base {
            *a = Act::C(None);
        }
    }
    acts.reverse();
    Ok(acts)
}

/// The function value in stack slot `slot` of `ts`, when it is a function.
fn slot_func(ts: &ThreadState, slot: usize) -> Option<Value> {
    if let Some((_, s, f)) = ts.died_in
        && s == slot
    {
        return Some(f);
    }
    match ts.stack.get(slot) {
        Some(v @ Value::Func(_)) => Some(*v),
        _ => None,
    }
}

/// What the instruction at `pc` of a frame's prototype calls as a metamethod, for a
/// metamethod's activation (C's `funcnamefromcode`).
fn metamethod_name(i: &Instr) -> Option<&'static str> {
    use crate::bytecode::ArithOp as A;
    Some(match i {
        Instr::SelfK { .. } | Instr::GetTabUp { .. } | Instr::GetTable { .. } | Instr::GetTableK { .. } => "index",
        Instr::SetTabUp { .. } | Instr::SetTable { .. } | Instr::SetTableK { .. } => "newindex",
        Instr::Add { .. } | Instr::AddK { .. } | Instr::AddI { sub: false, .. } => "add",
        Instr::AddI { sub: true, .. } | Instr::Sub { .. } | Instr::SubK { .. } => "sub",
        Instr::Mul { .. } | Instr::MulK { .. } => "mul",
        Instr::Div { .. } | Instr::DivK { .. } => "div",
        Instr::Mod { .. } | Instr::ModK { .. } => "mod",
        Instr::IDiv { .. } | Instr::IDivK { .. } => "idiv",
        Instr::Pow { .. } | Instr::PowK { .. } => "pow",
        Instr::BAnd { .. } | Instr::BAndK { .. } => "band",
        Instr::BOr { .. } | Instr::BOrK { .. } => "bor",
        Instr::BXor { .. } | Instr::BXorK { .. } => "bxor",
        Instr::Shl { .. } | Instr::ShlK { .. } => "shl",
        Instr::Shr { .. } | Instr::ShrK { .. } => "shr",
        Instr::Arith { op, .. } => match op {
            A::Add => "add",
            A::Sub => "sub",
            A::Mul => "mul",
            A::Mod => "mod",
            A::Pow => "pow",
            A::Div => "div",
            A::IDiv => "idiv",
            A::BAnd => "band",
            A::BOr => "bor",
            A::BXor => "bxor",
            A::Shl => "shl",
            A::Shr => "shr",
        },
        Instr::Unm { .. } => "unm",
        Instr::BNot { .. } => "bnot",
        Instr::Len { .. } => "len",
        Instr::Concat { .. } => "concat",
        Instr::Eq { .. } => "eq",
        Instr::Lt { .. } | Instr::LtI { .. } | Instr::GtI { .. } => "lt",
        Instr::Le { .. } | Instr::LeI { .. } | Instr::GeI { .. } => "le",
        Instr::Return { .. } | Instr::Jmp { close: 1.., .. } => "close",
        _ => return None,
    })
}

/// The name the caller gives activation `idx` of `acts` (newest first), C's
/// `funcnamefromcall`: only a Lua caller names what it called, and a tail call has no name.
fn call_name(lua: &Lua, ts: &ThreadState, acts: &[Act], idx: usize) -> Option<(&'static str, String)> {
    let caller = match acts.get(idx + 1) {
        Some(Act::Lua(j)) => *j,
        _ => return None,
    };
    let cf = ts.frames.get(caller)?;
    let p = lua.proto(cf.proto)?;
    let pc = cf.pc.checked_sub(1)?;
    let ins = p.code.get(pc)?;
    // Registers past one window have no names (as for error messages).
    let named = |reg| if p.frame > 0 { None } else { crate::debug::obj_name(lua, p, pc, reg) };
    match acts.get(idx)? {
        Act::Lua(i) => {
            let f = ts.frames.get(*i)?;
            if f.tail || *i != caller + 1 {
                return None;
            }
            if f.from_lua {
                return match ins {
                    Instr::Call { a, .. } | Instr::TailCall { a, .. } => named(*a),
                    Instr::TForCall { .. } => Some(("for iterator", "for iterator".into())),
                    _ => None,
                };
            }
            match f.kind {
                crate::vm::FrameKind::Cont(c) if c.is_metamethod() => metamethod_name(ins).map(|n| ("metamethod", n.to_owned())),
                _ => None,
            }
        }
        Act::C(Some(slot)) => match ins {
            Instr::Call { a, .. } | Instr::TailCall { a, .. } if cf.base + usize::from(*a) == *slot => named(*a),
            Instr::TForCall { a, .. } if *slot >= cf.base + usize::from(*a) => Some(("for iterator", "for iterator".into())),
            _ => None,
        },
        _ => None,
    }
}

/// `lua_getinfo` on function `f` (no activation: no line, no name).
fn func_info(lua: &mut Lua, f: Option<Value>) -> Result<Info, LuaError> {
    let lua_proto = match f {
        Some(Value::Func(r)) => match lua.heap.funcs.get(r.idx) {
            Some(Func::Lua { proto, upvals }) => Some((*proto, upvals.len())),
            _ => None,
        },
        _ => None,
    };
    let (what, source, short_src, ld, lld, nups, np, va) = if let Some((proto, nups)) = lua_proto
        && let Some(p) = lua.proto(proto)
    {
        let (short_src, chunk_name, ld, lld, np, va) = (
            p.source.to_string(),
            p.chunk_name,
            i64::from(p.line_defined),
            i64::from(p.last_line_defined),
            i64::from(p.num_params),
            p.is_vararg,
        );
        let source = match chunk_name {
            Value::Str(_) => chunk_name,
            // Not recorded (out of memory when it was loaded): the short name.
            _ => lua.new_str(format!("={short_src}").as_bytes())?,
        };
        (if ld == 0 { "main" } else { "Lua" }, source, short_src, ld, lld, nups as i64, np, va)
    } else {
        // C Lua's count for its library functions: only the closures it makes with upvalues
        // have any (LIBS-6: this counted RemedyLua's own, `pairs` and `ipairs` had 1).
        let nups = match f {
            Some(Value::Func(r)) => match lua.heap.funcs.get(r.idx) {
                Some(Func::Native { name, .. }) => c_upvalues(name),
                _ => 0,
            },
            _ => 0,
        };
        ("C", lua.new_str(b"=[C]")?, "[C]".into(), -1, -1, nups, 0, true)
    };
    Ok(Info {
        what,
        source,
        short_src,
        currentline: -1,
        linedefined: ld,
        lastlinedefined: lld,
        nups,
        nparams: np,
        isvararg: va,
        name: None,
        istailcall: false,
        func: f,
    })
}

/// The upvalues C Lua 5.4.7 gives the library function RemedyLua names `name`: `gmatch`'s
/// iterator 3 (`lstrlib.c`), a `coroutine.wrap` function 1 (`lcorolib.c`), `require` and the
/// searchers 1, the package table (`loadlib.c`); every other library function none.
fn c_upvalues(name: &str) -> i64 {
    match name {
        "gmatch_aux" => 3,
        "wrap" | "require" | "searcher_preload" | "searcher_host" => 1,
        _ => 0,
    }
}

/// `lua_getinfo` on activation `idx` of `acts` (newest first) of thread `t`.
fn level_info(lua: &mut Lua, t: ThreadRef, acts: &[Act], idx: usize) -> Result<Info, LuaError> {
    let Some(ts) = thread_state(lua, t) else { return func_info(lua, None) };
    let (func, line, tail) = match acts.get(idx) {
        Some(Act::Lua(i)) => match ts.frames.get(*i) {
            Some(f) => {
                let line = lua.proto(f.proto).map_or(-1, |p| i64::from(p.line(f.pc.saturating_sub(1))));
                (Some(Value::Func(f.closure)), line, f.tail)
            }
            None => (None, -1, false),
        },
        Some(Act::C(Some(slot))) => (slot_func(ts, *slot), -1, false),
        _ => (None, -1, false),
    };
    let name = call_name(lua, ts, acts, idx);
    let mut info = func_info(lua, func)?;
    if info.what != "C" {
        info.currentline = line;
    }
    info.name = name;
    info.istailcall = tail;
    Ok(info)
}

/// C Lua's `pushglobalfuncname`: `f`'s name in the loaded modules (`_G.print` as `print`,
/// `string.format`), looked for two levels deep, string keys only. Each entry looked at
/// is charged.
fn global_name(lua: &mut Lua, f: Value) -> Result<Option<Vec<u8>>, LuaError> {
    let Some(loaded) = lua.loaded_table() else { return Ok(None) };
    // Charged first, for every slot of every table the search may read, whether or not it
    // stops early: where it stops follows the hash seed, and fuel must not.
    let top_slots = lua.heap.tables.get(loaded.idx).map_or(0, crate::table::Table::slots);
    lua.charge(1 + top_slots as u64 / NAME_SEARCH_PER_FUEL)?;
    let inner_slots: usize = lua.heap.tables.get(loaded.idx).map_or(0, |top| {
        top.entries()
            .filter(|(k, _)| matches!(k, Value::Str(_)))
            .filter_map(|(_, v)| match v {
                Value::Table(t) => lua.heap.tables.get(t.idx).map(crate::table::Table::slots),
                _ => None,
            })
            .sum()
    });
    lua.charge(inner_slots as u64 / NAME_SEARCH_PER_FUEL)?;
    let mut found = None;
    if let Some(top) = lua.heap.tables.get(loaded.idx) {
        'search: for (k, v) in top.entries() {
            let Value::Str(ks) = k else { continue };
            if lua.raw_equal(&v, &f) {
                found = Some(lua.bytes(ks).to_vec());
                break;
            }
            if let Value::Table(inner) = v
                && let Some(it) = lua.heap.tables.get(inner.idx)
            {
                for (k2, v2) in it.entries() {
                    if let Value::Str(k2s) = k2
                        && lua.raw_equal(&v2, &f)
                    {
                        let mut name = lua.bytes(ks).to_vec();
                        name.push(b'.');
                        name.extend_from_slice(lua.bytes(k2s));
                        found = Some(name);
                        break 'search;
                    }
                }
            }
        }
    }
    Ok(found.map(|mut n| {
        if n.starts_with(b"_G.") {
            n.drain(..3);
        }
        n
    }))
}

/// Appends `s` as C's `%s` does: up to its first zero byte.
fn add_cstr(out: &mut Vec<u8>, s: &[u8]) {
    out.extend_from_slice(s.get(..cstr_len(s)).unwrap_or_default());
}

/// The bytes of `s` before its first zero byte, eight at a time (a message may be
/// megabytes long).
fn cstr_len(s: &[u8]) -> usize {
    const LO: u64 = 0x0101_0101_0101_0101;
    const HI: u64 = 0x8080_8080_8080_8080;
    let mut chunks = s.chunks_exact(8);
    let mut at = 0;
    for c in chunks.by_ref() {
        let w = u64::from_le_bytes(c.try_into().unwrap_or([1; 8]));
        if w.wrapping_sub(LO) & !w & HI != 0 {
            break;
        }
        at += 8;
    }
    at + s.get(at..).unwrap_or_default().iter().position(|&c| c == 0).unwrap_or(s.len() - at)
}

fn traceback(lua: &mut Lua, a: Args) -> R {
    let (t, arg) = thread_arg(lua, a)?;
    let msg = lua.arg(a, arg);
    let text = match msg {
        Value::Nil => None,
        Value::Str(s) => Some(s),
        Value::Int(_) | Value::Num(_) => Some(lua.check_str(a, arg)?),
        // A message that is not a string goes back untouched.
        other => return lua.ret1(other),
    };
    let default = if t == lua.cur_ref { 1 } else { 0 };
    // `luaL_optinteger`, then cast to C's `int`, as `db_traceback` does.
    let level = lua.opt_int(a, arg + 1, default)? as i32;
    lua.charge(TRACEBACK_FUEL)?;
    let acts = levels(lua, t)?;
    let mut out = super::OutBuf::new(TEXT_PER_FUEL);
    if let Some(m) = text {
        add_cstr(&mut out, lua.heap.str_bytes(m));
        out.push(b'\n');
    }
    out.extend_from_slice(b"stack traceback:");
    if level >= 0 {
        let last = acts.len() as i64 - 1;
        let mut level = i64::from(level);
        let mut limit2show: i64 = if last - level > (LEVELS1 + LEVELS2) as i64 { LEVELS1 as i64 } else { -1 };
        while level >= 0 && level <= last {
            let cur = level as usize;
            level += 1;
            let show = limit2show != 0;
            limit2show -= 1;
            if !show {
                let n = last - level - LEVELS2 as i64 + 1;
                out.extend_from_slice(format!("\n\t...\t(skipping {n} levels)").as_bytes());
                level += n;
                continue;
            }
            lua.charge(LEVEL_TEXT_FUEL)?;
            let info = level_info(lua, t, &acts, cur)?;
            out.extend_from_slice(b"\n\t");
            add_cstr(&mut out, info.short_src.as_bytes());
            if info.currentline > 0 {
                out.extend_from_slice(format!(":{}", info.currentline).as_bytes());
            }
            out.extend_from_slice(b": in ");
            let global = match info.func {
                Some(f) => global_name(lua, f)?,
                None => None,
            };
            if let Some(g) = global {
                out.extend_from_slice(b"function '");
                add_cstr(&mut out, &g);
                out.push(b'\'');
            } else if let Some((what, name)) = &info.name {
                out.extend_from_slice(format!("{what} '").as_bytes());
                add_cstr(&mut out, name.as_bytes());
                out.push(b'\'');
            } else if info.what == "main" {
                out.extend_from_slice(b"main chunk");
            } else if info.what != "C" {
                out.extend_from_slice(b"function <");
                add_cstr(&mut out, info.short_src.as_bytes());
                out.extend_from_slice(format!(":{}>", info.linedefined).as_bytes());
            } else {
                out.push(b'?');
            }
            if info.istailcall {
                out.extend_from_slice(b"\n\t(...tail calls...)");
            }
            out.pay(lua)?;
        }
    }
    lua.charge((crate::debug::take_work() / WORK_PER_FUEL) as u64)?;
    let b = out.into_vec(lua);
    lua.check_len(b.len())?;
    lua.ret_owned(b)
}

fn getinfo(lua: &mut Lua, a: Args) -> R {
    let (t, arg) = thread_arg(lua, a)?;
    // C's default is "flnSrtu"; `f` is not given here.
    let opts: Vec<u8> = match lua.opt_str(a, arg + 1)? {
        Some(s) => lua.bytes(s).to_vec(),
        None => b"lnSrtu".to_vec(),
    };
    lua.charge(GETINFO_FUEL + opts.len() as u64 / 4)?;
    if opts.first() == Some(&b'>') {
        return Err(LuaError::Arg(arg + 2, "invalid option '>'".into()));
    }
    let target = lua.arg(a, arg);
    let info = if let Value::Func(_) = target {
        func_info(lua, Some(target))?
    } else {
        let level = lua.check_int(a, arg)? as i32;
        let acts = levels(lua, t)?;
        if level < 0 || level as usize >= acts.len() {
            return lua.ret1(Value::Nil);
        }
        let i = level_info(lua, t, &acts, level as usize)?;
        lua.charge((crate::debug::take_work() / WORK_PER_FUEL) as u64)?;
        i
    };
    // `lua_getinfo`'s options (up to the first zero byte, as C reads them), in one pass.
    let mut seen = [false; 128];
    for &c in opts.get(..cstr_len(&opts)).unwrap_or_default() {
        match c {
            b'S' | b'l' | b'u' | b'n' | b'r' | b't' => {
                if let Some(x) = seen.get_mut(usize::from(c)) {
                    *x = true;
                }
            }
            b'f' | b'L' => {
                return Err(LuaError::Arg(arg + 2, format!("option '{}' is not available", c as char)));
            }
            _ => return Err(LuaError::Arg(arg + 2, "invalid option".into())),
        }
    }
    let has = |c: u8| seen.get(usize::from(c)).copied().unwrap_or(false);
    let tab = lua.new_table(0, 12)?;
    lua.hold(Value::Table(tab));
    if has(b'S') {
        set(lua, tab, "source", info.source)?;
        let s = lua.new_str(info.short_src.as_bytes())?;
        set(lua, tab, "short_src", s)?;
        set(lua, tab, "linedefined", Value::Int(info.linedefined))?;
        set(lua, tab, "lastlinedefined", Value::Int(info.lastlinedefined))?;
        let w = lua.new_str(info.what.as_bytes())?;
        set(lua, tab, "what", w)?;
    }
    if has(b'l') {
        set(lua, tab, "currentline", Value::Int(info.currentline))?;
    }
    if has(b'u') {
        set(lua, tab, "nups", Value::Int(info.nups))?;
        set(lua, tab, "nparams", Value::Int(info.nparams))?;
        set(lua, tab, "isvararg", Value::Bool(info.isvararg))?;
    }
    if has(b'n') {
        if let Some((_, n)) = &info.name {
            let v = lua.new_str(n.as_bytes())?;
            set(lua, tab, "name", v)?;
        }
        let v = lua.new_str(info.name.as_ref().map_or("", |(w, _)| *w).as_bytes())?;
        set(lua, tab, "namewhat", v)?;
    }
    if has(b'r') {
        // Only hooks transfer values (and there are none).
        set(lua, tab, "ftransfer", Value::Int(0))?;
        set(lua, tab, "ntransfer", Value::Int(0))?;
    }
    if has(b't') {
        set(lua, tab, "istailcall", Value::Bool(info.istailcall))?;
    }
    lua.ret1(Value::Table(tab))
}

fn set(lua: &mut Lua, t: TableRef, k: &str, v: Value) -> Result<(), LuaError> {
    let key = lua.new_str(k.as_bytes())?;
    lua.raw_set(t, key, v)
}
