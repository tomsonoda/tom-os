//! The coroutine library. Switching between coroutines happens inside the execution
//! loop, so coroutines yield from anywhere in Lua code (inside `pcall`, inside Lua
//! metamethods), and each step they take is counted against the caller's fuel.

#[allow(unused_imports)]
use crate::nostd::prelude::*;
use super::R;
use crate::heap::CoStatus;
use crate::value::{ThreadRef, Value};
use crate::vm::{Args, Flow, Lua, LuaError};

pub(super) fn open(lua: &mut Lua) {
    let t = lua.lib_table("coroutine");
    let fns: [(&str, &'static str, crate::vm::NativeFn); 8] = [
        ("create", "coroutine.create", create),
        ("resume", "coroutine.resume", resume),
        ("yield", "coroutine.yield", yield_),
        ("status", "coroutine.status", status),
        ("wrap", "coroutine.wrap", wrap),
        ("isyieldable", "coroutine.isyieldable", isyieldable),
        ("running", "coroutine.running", running),
        ("close", "coroutine.close", close),
    ];
    for (n, q, f) in fns {
        lua.set_fn(t, n, q, f);
    }
    let y = lua.new_str(b"yield").map_or(Value::Nil, |k| lua.raw_get(t, k));
    lua.names.yield_fn = y.as_func();
}

fn check_co(lua: &Lua, a: Args, i: usize) -> Result<ThreadRef, LuaError> {
    match lua.arg(a, i) {
        Value::Thread(t) => Ok(t),
        _ => Err(lua.type_error(a, i, "thread")),
    }
}

fn create(lua: &mut Lua, a: Args) -> R {
    let f = lua.arg(a, 0);
    if !matches!(f, Value::Func(_)) {
        return Err(lua.type_error(a, 0, "function"));
    }
    let co = lua.new_coroutine(f)?;
    lua.ret1(Value::Thread(co))
}

/// The arguments from `from` on, as the values to pass on (`rets`).
fn pass_args(lua: &mut Lua, a: Args, from: usize) {
    lua.rets.clear();
    for i in from..a.n {
        let v = lua.arg(a, i);
        lua.rets.push(v);
    }
}

fn resume(lua: &mut Lua, a: Args) -> R {
    let co = check_co(lua, a, 0)?;
    pass_args(lua, a, 1);
    Ok(Flow::Resume { co, wrap: false })
}

fn yield_(lua: &mut Lua, a: Args) -> R {
    pass_args(lua, a, 0);
    Ok(Flow::Yield)
}

pub(crate) fn status_of(lua: &Lua, co: ThreadRef) -> &'static str {
    if co == lua.cur_ref {
        return "running";
    }
    match lua.heap.threads.get(co.idx).map(|t| t.status) {
        Some(CoStatus::Suspended) => "suspended",
        Some(CoStatus::Normal) => "normal",
        Some(CoStatus::Running) => "running",
        _ => "dead",
    }
}

fn status(lua: &mut Lua, a: Args) -> R {
    let co = check_co(lua, a, 0)?;
    let s = status_of(lua, co);
    lua.ret_str(s.as_bytes())
}

fn wrap(lua: &mut Lua, a: Args) -> R {
    let f = lua.arg(a, 0);
    if !matches!(f, Value::Func(_)) {
        return Err(lua.type_error(a, 0, "function"));
    }
    let co = lua.new_coroutine(f)?;
    let w = lua.new_native(wrapped, "wrap", vec![Value::Thread(co)])?;
    lua.ret1(w)
}

fn wrapped(lua: &mut Lua, a: Args) -> R {
    let Value::Thread(co) = lua.native_upval(a, 0) else { return lua.ret(&[]) };
    pass_args(lua, a, 0);
    Ok(Flow::Resume { co, wrap: true })
}

fn isyieldable(lua: &mut Lua, a: Args) -> R {
    let co = if a.n > 0 { check_co(lua, a, 0)? } else { lua.cur_ref };
    let y = if co == lua.cur_ref {
        co != lua.main_ref && lua.cur.nny == 0
    } else {
        co != lua.main_ref && lua.heap.threads.get(co.idx).is_some_and(|t| t.nny == 0)
    };
    lua.ret1(Value::Bool(y))
}

fn running(lua: &mut Lua, _a: Args) -> R {
    let main = lua.cur_ref == lua.main_ref;
    let t = lua.cur_ref;
    lua.ret(&[Value::Thread(t), Value::Bool(main)])
}

fn close(lua: &mut Lua, a: Args) -> R {
    let co = check_co(lua, a, 0)?;
    match status_of(lua, co) {
        "dead" | "suspended" => {}
        s => return Err(LuaError::msg(format!("cannot close a {s} coroutine"))),
    }
    // Its pending to-be-closed variables close, running on its own stack.
    // The error it died with (if any) goes to its `__close` handlers; one that raises
    // replaces it.
    // As C's `luaE_resetthread`: the frames a coroutine that died by an error kept (for
    // tracebacks, LIBS-4) are gone before its variables close.
    if let Some(t) = lua.heap.threads.get_mut(co.idx)
        && t.status == CoStatus::Dead
    {
        t.frames.clear();
        t.died_in = None;
    }
    let err = lua.heap.threads.get(co.idx).and_then(|t| t.error);
    let has_tbc = lua.heap.threads.get(co.idx).is_some_and(|t| !t.tbc.is_empty());
    let mut result = err;
    if has_tbc {
        let me = lua.cur_ref;
        let was = lua.swap_in_for_close(co);
        let r = lua.close_with_error(0, err.unwrap_or(Value::Nil));
        lua.close_upvals(0);
        lua.swap_out_after_close(me, was);
        match r {
            Ok(v) if !v.is_nil() => result = Some(v),
            Ok(_) => {}
            Err(e) if e.limit().is_some() => return Err(e),
            Err(e) => result = Some(lua.normalize(e)?),
        }
    }
    if let Some(t) = lua.heap.threads.get_mut(co.idx) {
        t.status = CoStatus::Dead;
        t.frames.clear();
        t.tbc.clear();
        t.error = None;
        t.died_in = None;
        t.yield_pending = None;
    }
    // Its stack goes to the next coroutine (LR-8).
    lua.retire_stack(co);
    match result {
        None => lua.ret1(Value::Bool(true)),
        Some(e) => lua.ret(&[Value::Bool(false), e]),
    }
}
