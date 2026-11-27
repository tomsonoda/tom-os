//! The old (recursive) and new (iterative) front ends side by side, for the equivalence
//! tests (feature `front-oracle`). Results are printed so they compare as strings.

#[allow(unused_imports)]
use crate::nostd::prelude::*;
use crate::nostd::fmt::Write;

use crate::bytecode::Proto;
use crate::diag::Diagnostic;
use crate::heap::Heap;
use crate::{Dialect, Libs, Limits, Lua, Value};

fn diag(d: &Diagnostic) -> String {
    format!("{}:{}: [{}:{}-{}:{}] {}", d.chunk, d.line, d.pos.line, d.pos.col, d.end.line, d.end.col, d.message)
}

fn dialect(name: &str) -> Dialect {
    Dialect::for_name(name)
}

/// The syntax tree the recursive parser makes (printed), or its error.
pub fn parse_old(src: &[u8], name: &str) -> Result<String, String> {
    crate::front_oracle::parser::parse(src, name, dialect(name)).map(|b| format!("{b:?}")).map_err(|d| diag(&d))
}

/// The syntax tree the iterative parser makes (printed), or its error.
pub fn parse_new(src: &[u8], name: &str) -> Result<String, String> {
    crate::parser::parse(src, name, dialect(name)).map(|b| format!("{b:?}")).map_err(|d| diag(&d))
}

/// Every prototype the recursive compiler makes (printed), or the first error.
pub fn compile_old(src: &[u8], name: &str) -> Result<String, String> {
    let block = crate::front_oracle::parser::parse(src, name, dialect(name)).map_err(|d| diag(&d))?;
    let mut lua = Lua::new(Limits::unmetered(), Libs::NONE);
    let p = crate::front_oracle::compiler::compile(&block, name, &mut lua.heap, crate::front_oracle::parser::DEFAULT_STACK)
        .map_err(|d| diag(&d))?;
    Ok(dump(&lua.heap, p))
}

/// Every prototype the iterative compiler makes (printed), or the first error, without
/// register reuse along unary and field chains (the one difference from the old compiler,
/// checked by running instead: `run_new`).
pub fn compile_new(src: &[u8], name: &str) -> Result<String, String> {
    compile_new_with(src, name, false)
}

/// `compile_new` with or without that reuse.
pub fn compile_new_with(src: &[u8], name: &str, reuse: bool) -> Result<String, String> {
    let block = crate::parser::parse(src, name, dialect(name)).map_err(|d| diag(&d))?;
    let mut lua = Lua::new(Limits::unmetered(), Libs::NONE);
    let p = crate::compiler::compile_with(&block, name, &mut lua.heap, reuse).map_err(|d| diag(&d))?;
    Ok(dump(&lua.heap, p))
}

/// Runs `src` compiled with or without the reuse (fuel and memory bounded): its results or
/// error, printed.
pub fn run_new(src: &[u8], name: &str, reuse: bool) -> String {
    let block = match crate::parser::parse(src, name, dialect(name)) {
        Ok(b) => b,
        Err(d) => return diag(&d),
    };
    let mut lua = Lua::new(Limits::default().fuel(20_000_000).memory(256 << 20), Libs::STANDARD);
    let p = match crate::compiler::compile_with(&block, name, &mut lua.heap, reuse) {
        Ok(p) => p,
        Err(d) => return diag(&d),
    };
    let env = Value::Table(lua.globals);
    let up = lua.heap.new_upval_charged(crate::heap::Upval::Closed(env));
    let f = Value::Func(lua.heap.new_func_charged(crate::heap::Func::Lua { proto: p, upvals: vec![up].into_boxed_slice() }));
    lua.hold(f);
    match lua.call(f, &[]) {
        Ok(v) => v.iter().map(|x| match x {
            Value::Str(_) => format!("{:?}", lua.to_string_lossy(*x)),
            Value::Int(_) | Value::Num(_) | Value::Bool(_) | Value::Nil => format!("{x:?}"),
            other => other.type_name().to_owned(),
        }).collect::<Vec<_>>().join(", "),
        Err(e) => format!("error: {e}"),
    }
}

fn dump(heap: &Heap, root: u32) -> String {
    let mut out = String::new();
    let mut todo = vec![(root, String::from("main"))];
    while let Some((i, path)) = todo.pop() {
        let Some(p): Option<&Proto> = heap.protos.get(i) else {
            let _ = writeln!(out, "{path}: missing proto {i}");
            continue;
        };
        let _ = writeln!(
            out,
            "== {path}: params {} vararg {} max_stack {} line {} protos {}",
            p.num_params,
            p.is_vararg,
            p.max_stack,
            p.line_defined,
            p.protos.len()
        );
        for (pc, (c, l)) in p.code.iter().zip(&p.lines).enumerate() {
            let _ = writeln!(out, "  {pc} [{l}] {c:?}");
        }
        let _ = writeln!(out, "  lines {}", p.lines.len());
        for k in &p.consts {
            let _ = match k {
                Value::Str(s) => writeln!(out, "  k {:?}", String::from_utf8_lossy(heap.str_bytes(*s))),
                Value::Num(f) => writeln!(out, "  k num {:x}", f.to_bits()),
                other => writeln!(out, "  k {other:?}"),
            };
        }
        for u in &p.upvals {
            let _ = writeln!(out, "  up {} {} {}", u.in_stack, u.idx, u.name);
        }
        for v in &p.locvars {
            let _ = writeln!(out, "  loc {} r{} {}..{}", v.name, v.reg, v.start, v.end);
        }
        for (n, c) in p.protos.iter().enumerate().rev() {
            todo.push((*c, format!("{path}.{n}")));
        }
    }
    out
}

/// Times (parse, compile, drop of the tree) of `src` by the old front end (`old`) or the
/// new one, for `examples/frontbench.rs`.
pub fn time(src: &[u8], name: &str, old: bool) -> Result<[crate::nostd::time::Duration; 3], String> {
    use crate::nostd::time::Instant;
    let mut lua = Lua::new(Limits::unmetered(), Libs::NONE);
    let t = Instant::now();
    let block = if old {
        crate::front_oracle::parser::parse(src, name, dialect(name))
    } else {
        crate::parser::parse(src, name, dialect(name))
    }
    .map_err(|d| diag(&d))?;
    let parse = t.elapsed();
    let t = Instant::now();
    if old {
        crate::front_oracle::compiler::compile(&block, name, &mut lua.heap, usize::MAX)
    } else {
        crate::compiler::compile(&block, name, &mut lua.heap)
    }
    .map_err(|d| diag(&d))?;
    let compile = t.elapsed();
    let t = Instant::now();
    drop(block);
    Ok([parse, compile, t.elapsed()])
}
