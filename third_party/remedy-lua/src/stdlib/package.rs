//! `require` and `package` (`Libs::REQUIRE`), as C Lua 5.4.7's `loadlib.c` makes them, with
//! modules only from the host: `package.loaded`, `package.preload`, `package.searchers`
//! (the preload searcher, then the host's), the loader's second value, C Lua's messages
//! for a module that is not found, and no cycle detection (as in C Lua, a cycle recurses
//! until the call depth runs out). There is no search path, no file and no C library:
//! `path`, `cpath`, `config`, `searchpath` and `loadlib` are absent.

#[allow(unused_imports)]
use crate::nostd::prelude::*;
use super::R;
use crate::value::{StrRef, Value};
use crate::vm::{Args, Lua, LuaError};

/// Bytes of a module's name or messages per unit of fuel.
const TEXT_PER_FUEL: usize = 8;
/// Fuel for `require`'s fixed part (the lookups in the loaded table), and for loading a
/// module (each searcher called, the loader called, the results stored).
const REQUIRE_FUEL: u64 = 10;
const LOAD_MODULE_FUEL: u64 = 16;
/// Fuel for a searcher's fixed part.
const SEARCHER_FUEL: u64 = 6;

pub(crate) fn open(lua: &mut Lua) {
    let Some(loaded) = lua.ensure_loaded() else { return };
    let pkg = lua.lib_table("package");
    lua.set_field(pkg, "loaded", Value::Table(loaded));
    // The preload table, kept in the registry too (C's `LUA_PRELOAD_TABLE`): the preload
    // searcher reads that one, whatever a script puts in `package.preload`.
    let preload = lua.new_table(0, 4).unwrap_or(pkg);
    let reg = lua.registry;
    let k = lua.new_str(b"_PRELOAD").unwrap_or_default();
    let _ = lua.raw_set(reg, k, Value::Table(preload));
    lua.set_field(pkg, "preload", Value::Table(preload));
    let searchers = lua.new_table(2, 0).unwrap_or(pkg);
    for (i, (f, name)) in [(searcher_preload as crate::vm::NativeFn, "searcher_preload"), (searcher_host, "searcher_host")]
        .into_iter()
        .enumerate()
    {
        // Each searcher holds the package table (as in C Lua).
        let v = lua.new_native(f, name, vec![Value::Table(pkg)]).unwrap_or_default();
        let _ = lua.raw_set(searchers, Value::Int(i as i64 + 1), v);
    }
    lua.set_field(pkg, "searchers", Value::Table(searchers));
    let req = lua.new_native(require, "require", vec![Value::Table(pkg)]).unwrap_or_default();
    let g = lua.globals;
    lua.set_field(g, "require", req);
}

/// A registry table by name (`_LOADED`, `_PRELOAD`).
fn registry_table(lua: &mut Lua, name: &[u8]) -> Result<Value, LuaError> {
    let k = lua.new_str(name)?;
    Ok(lua.raw_get(lua.registry, k))
}

/// The module name argument: its bytes up to the first zero (C's `%s` in messages).
fn shown(lua: &Lua, s: StrRef) -> Vec<u8> {
    lua.bytes(s).split(|&c| c == 0).next().unwrap_or_default().to_vec()
}

fn require(lua: &mut Lua, a: Args) -> R {
    let name = lua.check_str(a, 0)?;
    lua.charge(REQUIRE_FUEL + (lua.bytes(name).len() / TEXT_PER_FUEL) as u64)?;
    let loaded = registry_table(lua, b"_LOADED")?;
    let have = lua.index_nested(loaded, Value::Str(name))?;
    if have.truthy() {
        return lua.ret1(have);
    }
    // `findloader`: each searcher in turn, its messages collected.
    lua.charge(LOAD_MODULE_FUEL)?;
    let pkg = lua.native_upval(a, 0);
    let skey = lua.new_str(b"searchers")?;
    let searchers = match lua.index_nested(pkg, skey)? {
        Value::Table(t) => t,
        _ => return Err(LuaError::msg("'package.searchers' must be a table")),
    };
    lua.hold(Value::Table(searchers));
    let mut msg = super::OutBuf::new(TEXT_PER_FUEL);
    let mut i = 1i64;
    let (loader, data) = loop {
        let s = lua.raw_get(searchers, Value::Int(i));
        i += 1;
        lua.charge(1)?;
        if s.is_nil() {
            let mut text = b"module '".to_vec();
            text.extend_from_slice(&shown(lua, name));
            text.extend_from_slice(b"' not found:");
            text.extend_from_slice(&msg);
            msg.pay(lua)?;
            return Err(lua.msg_bytes(&[&text]));
        }
        let (r1, r2) = {
            let out = lua.call_nested(s, &[Value::Str(name)])?;
            (out.first().copied().unwrap_or_default(), out.get(1).copied().unwrap_or_default())
        };
        match r1 {
            Value::Func(_) => break (r1, r2),
            Value::Str(m) => {
                msg.extend_from_slice(b"\n\t");
                msg.extend_from_slice(lua.heap.str_bytes(m));
            }
            Value::Int(_) | Value::Num(_) => {
                // `lua_isstring`: a number is a message too.
                msg.extend_from_slice(b"\n\t");
                let text = match r1 {
                    Value::Int(n) => n.to_string(),
                    Value::Num(f) => crate::number::fmt_float(f),
                    _ => String::new(),
                };
                msg.extend_from_slice(text.as_bytes());
            }
            _ => {}
        }
        lua.check_len(msg.len())?;
        msg.pay(lua)?;
    };
    // Given back before the loader runs.
    let _ = msg.into_vec(lua);
    lua.hold(loader);
    lua.hold(data);
    // Only the first result is used: the rest goes before `package.loaded`'s `__newindex`
    // can run (SEC-M1: a loader returning a million values held them all, uncounted, at
    // every level of a nested `require`).
    let result = lua.call_nested1(loader, &[Value::Str(name), data])?;
    if !result.is_nil() {
        lua.set_index_nested(loaded, Value::Str(name), result)?;
    }
    let mut module = lua.index_nested(loaded, Value::Str(name))?;
    if module.is_nil() {
        module = Value::Bool(true);
        lua.set_index_nested(loaded, Value::Str(name), module)?;
    }
    lua.ret(&[module, data])
}

/// `package.searchers[1]`: the registry's preload table (C's `searcher_preload`).
fn searcher_preload(lua: &mut Lua, a: Args) -> R {
    let name = lua.check_str(a, 0)?;
    lua.charge(SEARCHER_FUEL)?;
    let preload = registry_table(lua, b"_PRELOAD")?;
    let f = lua.index_nested(preload, Value::Str(name))?;
    if f.is_nil() {
        let mut text = b"no field package.preload['".to_vec();
        text.extend_from_slice(&shown(lua, name));
        text.extend_from_slice(b"']");
        super::owe((text.len() / TEXT_PER_FUEL) as u64);
        return lua.ret_str(&text);
    }
    let tag = lua.new_str(b":preload:")?;
    lua.ret(&[f, tag])
}

/// `package.searchers[2]`: the host's modules (`Lua::add_module`, then the loader given to
/// `Lua::set_module_loader`). A module is source text, compiled as `load` compiles it
/// (charged the same), as chunk `@origin`; the loader's second value is the origin.
fn searcher_host(lua: &mut Lua, a: Args) -> R {
    let name = lua.check_str(a, 0)?;
    let key = lua.bytes(name).to_vec();
    lua.charge(SEARCHER_FUEL + (key.len() / TEXT_PER_FUEL) as u64)?;
    let found = match lua.modules.get(&key) {
        Some(m) => Some(m.clone()),
        None => match (lua.module_loader.clone(), crate::nostd::str::from_utf8(&key)) {
            (Some(f), Ok(n)) => f(n),
            _ => None,
        },
    };
    let Some(m) = found else {
        let mut text = b"no host module '".to_vec();
        text.extend_from_slice(&shown(lua, name));
        text.push(b'\'');
        super::owe((text.len() / TEXT_PER_FUEL) as u64);
        return lua.ret_str(&text);
    };
    lua.check_len(m.code.len())?;
    let chunk = format!("@{}", m.origin);
    let env = Value::Table(lua.globals);
    let cname = lua.new_str(chunk.as_bytes())?;
    lua.hold(cname);
    match super::base::compile_text(lua, super::base::Src::Text(&m.code), &chunk, env, cname)? {
        Ok(f) => {
            let origin = lua.new_str(m.origin.as_bytes())?;
            lua.ret(&[f, origin])
        }
        Err(e) => {
            // C's `checkload`: "error loading module '%s' from file '%s':\n\t%s".
            let mut text = b"error loading module '".to_vec();
            text.extend_from_slice(&shown(lua, name));
            text.extend_from_slice(b"' from host module '");
            text.extend_from_slice(m.origin.as_bytes());
            text.extend_from_slice(b"':\n\t");
            if let Value::Str(s) = e {
                text.extend_from_slice(lua.bytes(s));
            }
            super::owe((text.len() / TEXT_PER_FUEL) as u64);
            Err(lua.msg_bytes(&[&text]))
        }
    }
}
