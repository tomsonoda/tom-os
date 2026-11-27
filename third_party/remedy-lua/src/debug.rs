//! Names for error messages: what a register held (a global, a field, a local…), found
//! by reading the bytecode backwards the way Lua's `ldebug.c` does.

#[allow(unused_imports)]
use crate::nostd::prelude::*;
use crate::bytecode::{Instr, Proto, Reg, Rk};
use crate::value::Value;
use crate::vm::Lua;

crate::thread_local! {
    /// Bytecode, local-variable entries and constant bytes read to name things in error
    /// messages since the last `take_work`: charged by `Lua::charge_error`, so an error
    /// raised over and over at the end of a huge function pays for the scans.
    static WORK: crate::nostd::cell::Cell<usize> = const { crate::nostd::cell::Cell::new(0) };
}

fn add_work(n: usize) {
    WORK.with(|w| w.set(w.get().saturating_add(n)));
}

/// The work counted since the last call, reset to zero.
pub(crate) fn take_work() -> usize {
    WORK.with(|w| w.replace(0))
}

/// Puts back work `take_work` took (see `table::put_probes`).
pub(crate) fn put_work(n: usize) {
    WORK.with(|w| w.set(n));
}

/// An instruction operand an error is about.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Operand {
    Reg(Reg),
    Rk(Rk),
    Upval(Reg),
}

/// The name of the local in register `reg` at `pc`, if one is active.
pub(crate) fn local_name(p: &Proto, pc: usize, reg: Reg) -> Option<String> {
    add_work(p.locvars.len());
    let pc = pc as u32;
    p.locvars
        .iter()
        .rev()
        .find(|lv| lv.reg == reg && lv.start <= pc && pc < lv.end && !lv.name.starts_with('('))
        .map(|lv| lv.name.to_string())
}

pub(crate) fn sets_reg(i: &Instr, reg: Reg) -> bool {
    match *i {
        Instr::Move { a, .. }
        | Instr::LoadK { a, .. }
        | Instr::LoadInt { a, .. }
        | Instr::LoadBool { a, .. }
        | Instr::LoadFalseSkip { a }
        | Instr::GetUpval { a, .. }
        | Instr::GetSelf { a, .. }
        | Instr::GetTabUp { a, .. }
        | Instr::GetTable { a, .. }
        | Instr::GetTableK { a, .. }
        | Instr::NewTable { a, .. }
        | Instr::Arith { a, .. }
        | Instr::Add { a, .. }
        | Instr::AddK { a, .. }
        | Instr::Sub { a, .. }
        | Instr::SubK { a, .. }
        | Instr::Mul { a, .. }
        | Instr::MulK { a, .. }
        | Instr::Div { a, .. }
        | Instr::DivK { a, .. }
        | Instr::Mod { a, .. }
        | Instr::ModK { a, .. }
        | Instr::IDiv { a, .. }
        | Instr::IDivK { a, .. }
        | Instr::AddI { a, .. }
        | Instr::Pow { a, .. }
        | Instr::PowK { a, .. }
        | Instr::BAnd { a, .. }
        | Instr::BAndK { a, .. }
        | Instr::BOr { a, .. }
        | Instr::BOrK { a, .. }
        | Instr::BXor { a, .. }
        | Instr::BXorK { a, .. }
        | Instr::Shl { a, .. }
        | Instr::ShlK { a, .. }
        | Instr::Shr { a, .. }
        | Instr::ShrK { a, .. }
        | Instr::Unm { a, .. }
        | Instr::BNot { a, .. }
        | Instr::Not { a, .. }
        | Instr::Len { a, .. }
        | Instr::Concat { a, .. }
        | Instr::ToStr { a, .. }
        | Instr::TestSet { a, .. }
        | Instr::Closure { a, .. } => a == reg,
        Instr::SelfK { a, .. } => reg == a || reg == a.wrapping_add(1),
        Instr::LoadNil { a, n } => a <= reg && u16::from(reg) <= u16::from(a) + u16::from(n),
        Instr::Call { a, .. } | Instr::TailCall { a, .. } => reg >= a,
        Instr::TForCall { a, .. } => u16::from(reg) >= u16::from(a) + 2,
        Instr::VarArg { a, .. } => reg >= a,
        Instr::ForPrep { a, .. } | Instr::ForLoop { a, .. } => reg >= a && u16::from(reg) <= u16::from(a) + 3,
        Instr::TForLoop { a, .. } => u16::from(reg) == u16::from(a) + 2,
        _ => false,
    }
}

/// The instruction that last set `reg` before `lastpc`, unless a jump makes it uncertain.
fn find_set_reg(p: &Proto, lastpc: usize, reg: Reg) -> Option<usize> {
    add_work(lastpc);
    let mut setreg = None;
    let mut jmptarget = 0usize;
    for (pc, i) in p.code.iter().enumerate().take(lastpc) {
        if let Instr::Jmp { off, .. } = i {
            let dest = (pc as i64 + 1 + i64::from(*off)) as usize;
            if dest <= lastpc && dest > jmptarget {
                jmptarget = dest;
            }
            continue;
        }
        if sets_reg(i, reg) {
            setreg = if pc < jmptarget { None } else { Some(pc) };
        }
    }
    setreg
}

fn const_str(lua: &Lua, p: &Proto, k: usize) -> Option<String> {
    match p.consts.get(k) {
        Some(Value::Str(s)) => {
            // Messages are cut at `error_len`: a longer name would be cut anyway.
            let b = lua.bytes(*s);
            let b = b.get(..b.len().min(lua.limits.error_len)).unwrap_or_default();
            // C Lua prints names with `%s`: they end at the first zero byte.
            let b = b.split(|&c| c == 0).next().unwrap_or_default();
            add_work(b.len());
            Some(String::from_utf8_lossy(b).into_owned())
        }
        _ => None,
    }
}

fn upval_name(p: &Proto, u: Reg) -> String {
    p.upvals.get(usize::from(u)).map_or_else(|| "?".to_owned(), |d| d.name.to_string())
}

/// What register `reg` held at `pc`: (kind, name), e.g. ("global", "print").
///
/// A loop, not recursion: `Move` chains and keys held by registers are followed back
/// through the code, as deep as a program makes them (no native stack per step).
pub(crate) fn obj_name(lua: &Lua, p: &Proto, pc: usize, reg: Reg) -> Option<(&'static str, String)> {
    let (mut pc, mut reg) = (pc, reg);
    loop {
        if let Some(n) = local_name(p, pc, reg) {
            return Some(("local", n));
        }
        let set = find_set_reg(p, pc, reg)?;
        match *p.code.get(set)? {
            Instr::Move { a, b } if b < a => (pc, reg) = (set, b),
            _ => return obj_name_at(lua, p, set),
        }
    }
}

/// Whether the table in register `reg` at `pc` is named `_ENV` (C's `isEnv`): a local, an
/// upvalue (`_ENV.x` loads the upvalue into a register first), a constant or a field of
/// that name, following `Move`s back. S5C-COR-4: only a local counted, so `_ENV.f()` was
/// "field 'f'" where C Lua says "global 'f'". A loop over `Move`s only: no recursion.
fn reg_is_env(lua: &Lua, p: &Proto, pc: usize, reg: Reg) -> bool {
    let (mut pc, mut reg) = (pc, reg);
    loop {
        if let Some(n) = local_name(p, pc, reg) {
            return n == "_ENV";
        }
        let Some(set) = find_set_reg(p, pc, reg) else { return false };
        match p.code.get(set).copied() {
            Some(Instr::Move { a, b }) if b < a => (pc, reg) = (set, b),
            Some(Instr::GetUpval { u, .. } | Instr::GetSelf { u, .. }) => return upval_name(p, u) == "_ENV",
            Some(Instr::LoadK { k, .. }) => return const_str(lua, p, k as usize).as_deref() == Some("_ENV"),
            Some(Instr::GetTabUp { k, .. } | Instr::GetTableK { k, .. }) => {
                return const_str(lua, p, usize::from(k)).as_deref() == Some("_ENV");
            }
            _ => return false,
        }
    }
}

/// The constant a register held at `pc` (C's `rname` case), following `Move`s back.
fn const_in(lua: &Lua, p: &Proto, pc: usize, reg: Reg) -> Option<String> {
    let (mut pc, mut reg) = (pc, reg);
    loop {
        if local_name(p, pc, reg).is_some() {
            return None;
        }
        let set = find_set_reg(p, pc, reg)?;
        match *p.code.get(set)? {
            Instr::Move { a, b } if b < a => (pc, reg) = (set, b),
            Instr::LoadK { k, .. } => return const_str(lua, p, k as usize),
            _ => return None,
        }
    }
}

/// `obj_name` for the instruction at `set`, which set the register (not a `Move`).
fn obj_name_at(lua: &Lua, p: &Proto, set: usize) -> Option<(&'static str, String)> {
    match *p.code.get(set)? {
        Instr::GetTabUp { u, k, .. } => {
            let name = const_str(lua, p, usize::from(k))?;
            let kind = if upval_name(p, u) == "_ENV" { "global" } else { "field" };
            Some((kind, name))
        }
        Instr::GetTableK { b, k, .. } => {
            let name = match p.consts.get(usize::from(k)) {
                // C Lua's GETI takes keys 0..=255; others go through a register: "?".
                Some(Value::Int(i)) if (0..=255).contains(i) => "integer index".into(),
                Some(Value::Int(_)) => "?".into(),
                _ => const_str(lua, p, usize::from(k)).unwrap_or_else(|| "?".into()),
            };
            let is_env = reg_is_env(lua, p, set, b);
            Some((if is_env { "global" } else { "field" }, name))
        }
        Instr::GetTable { c, .. } => {
            // C's `rname`: the key's name only when the register holds a constant (a
            // local holding one gives "?").
            let name = const_in(lua, p, set, c).unwrap_or_else(|| "?".into());
            Some(("field", name))
        }
        Instr::GetUpval { u, .. } | Instr::GetSelf { u, .. } => Some(("upvalue", upval_name(p, u))),
        Instr::LoadK { k, .. } => const_str(lua, p, k as usize).map(|s| ("constant", s)),
        Instr::SelfK { k, .. } => Some(("method", const_str(lua, p, usize::from(k))?)),
        _ => None,
    }
}

/// ` (global 'x')` and the like, or nothing.
pub(crate) fn var_info(lua: &Lua, p: &Proto, pc: usize, o: Operand) -> String {
    // Registers past one window are reached through `Window` and `MoveAbs`: no names.
    if p.frame > 0 {
        return String::new();
    }
    let found = match o {
        Operand::Upval(u) => Some(("upvalue", upval_name(p, u))),
        Operand::Reg(r) => obj_name(lua, p, pc, r),
        Operand::Rk(rk) => {
            if rk.is_const() {
                const_str(lua, p, rk.index()).map(|s| ("constant", s))
            } else {
                obj_name(lua, p, pc, rk.index() as Reg)
            }
        }
    };
    match found {
        Some((kind, name)) => format!(" ({kind} '{name}')"),
        None => String::new(),
    }
}
