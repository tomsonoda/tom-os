# RemedyLua

[![crates.io](https://img.shields.io/crates/v/remedy-lua.svg)](https://crates.io/crates/remedy-lua)
[![docs.rs](https://img.shields.io/docsrs/remedy-lua)](https://docs.rs/remedy-lua)

**A Lua 5.4 superset in safe Rust, built to run untrusted scripts inside applications.**

RemedyLua is a from-scratch implementation of Lua 5.4: lexer, parser, compiler, virtual
machine, incremental garbage collector and standard library, with no `unsafe` code and no
runtime dependencies. Every step a script takes is metered, so a host can run code it does
not trust (UI widgets, bots, game mods) with hard bounds on CPU, memory and output.

RemedyLua was built as the scripting engine for RemedyUI and the applications built on it,
and works as a standalone embeddable engine for any Rust host.

## Features

- **Lua 5.4 compatible.** Plain Lua programs behave exactly as in C Lua 5.4.7: values,
  metatables, closures, integer and float subtypes, `goto`, `<const>` and `<close>`,
  coroutines, error handling, the same error messages and positions. Every test program is
  checked against C Lua 5.4.7 itself.
- **A strict superset.** C Lua's fixed implementation limits (200 locals, 255 upvalues,
  ~200 syntax levels, the C stack) are gone. Only the host's budgets bound a program.
- **Hard, exact limits.** Fuel (an instruction budget that also charges library and
  collector work), a memory cap counted before each allocation, call depth, string and
  output size, host interrupts and time limits. A script cannot catch a limit error, and
  the state stays usable afterwards. Limits are deterministic: the same with or without
  the fast path.
- **Sandboxed by default.** No file system, process, environment or native-code access in
  the language. Libraries are capabilities the host grants per state (`Libs::SAFE`,
  `STANDARD`, `ALL`, and opt-in `REQUIRE`, `IO`, `ENV`, `DEBUG_INFO`). `load` accepts
  source text only; bytecode is refused.
- **Incremental garbage collector.** Marking and sweeping run in small steps paid for by
  allocation, so scripts can run between frames without long pauses.
- **RemedyLua dialect (`.rlua`).** Optional type annotations with a gradual, sound-where-it-
  reports type checker; compound assignment (`+= -= *= /= //= %= ..=`); `continue`; string
  interpolation (`` `hi {name}` ``). All of it compiles to the plain Lua it stands for:
  annotations have zero run-time cost.
- **Embedding API.** Rust host functions and userdata, typed data conversion in and out,
  host-provided modules for `require`, host streams for `io`, output capture, diagnostics,
  and a `Send` state that can move between threads.
- **Runs on WebAssembly.** Builds for `wasm32-unknown-unknown` with no imports.

## Quick start

Add it from crates.io:

```toml
[dependencies]
remedy-lua = "0.1"
```

```rust
use remedy_lua::{Lua, Limits, Libs};

fn main() -> Result<(), remedy_lua::Error> {
    // 1M steps of fuel and 4 MiB of memory per call; untrusted-code libraries only.
    let mut lua = Lua::new(Limits::default().fuel(1_000_000).memory(4 << 20), Libs::SAFE);

    lua.register("host.greet", |lua, args| {
        let who = args.first().and_then(|v| lua.to_string_lossy(*v)).unwrap_or_default();
        Ok(vec![lua.create_string(format!("hi {who}"))?])
    })?;

    let out = lua.exec("return host.greet('you')", "=widget")?;
    println!("{}", lua.to_string_lossy(out[0]).unwrap_or_default());
    Ok(())
}
```

A runaway script stops with `Error::Limit`:

```rust
let err = lua.exec("while true do end", "=loop").unwrap_err();
assert!(matches!(err, remedy_lua::Error::Limit(remedy_lua::LimitKind::Fuel)));
```

Type-check a `.rlua` file from the command line:

```
cargo run -p remedy-lua --example check -- path/to/file.rlua
```

## Performance

On the 21-program benchmark suite, RemedyLua runs at a geometric mean of about **1.2x
C Lua 5.4.7's wall time** (faster than C Lua on string concatenation, formatting and table
churn), with full metering on. Per-benchmark numbers and methodology are in
[`docs/performance.md`](https://github.com/AhmiDarrow/RemedyLua/blob/main/docs/performance.md).

## Documentation

| Document | Contents |
|---|---|
| [`docs/reference.md`](https://github.com/AhmiDarrow/RemedyLua/blob/main/docs/reference.md) | The language reference: the `.rlua` dialect, the type checker, every difference from Lua 5.4, the standard library and its capabilities, budgets, and the embedding guide with an API index. |
| API docs | `cargo doc -p remedy-lua --open`. Every public item is documented. |
| [`docs/known-issues.md`](https://github.com/AhmiDarrow/RemedyLua/blob/main/docs/known-issues.md) | Open issues, their status and workarounds for hosts. |
| [`docs/performance.md`](https://github.com/AhmiDarrow/RemedyLua/blob/main/docs/performance.md) | Benchmark results against C Lua 5.4.7 and how they were measured. |
| [`docs/testing.md`](https://github.com/AhmiDarrow/RemedyLua/blob/main/docs/testing.md) | How RemedyLua is tested, the C Lua oracle, and the gate. |
| [`CHANGELOG.md`](https://github.com/AhmiDarrow/RemedyLua/blob/main/CHANGELOG.md) | Releases and known limitations. |
| [`CONTRIBUTING.md`](https://github.com/AhmiDarrow/RemedyLua/blob/main/CONTRIBUTING.md) | Project rules, building, testing and where the open work is. |

## Status

Version **0.1.2**: near release, and ready for general testing. The language, standard
library, limits and host API are feature-complete and tested against C Lua 5.4.7, including
differential fuzzing. A few known limitations around collector fuel under very tight
budgets are listed in [`docs/known-issues.md`](https://github.com/AhmiDarrow/RemedyLua/blob/main/docs/known-issues.md). The resumable host
call API (`unstable-resumable` feature) is not yet stable. Feedback and bug reports are
welcome.

Requires Rust 1.96 or later (edition 2024).

## License

Licensed under either of [Apache License 2.0](https://github.com/AhmiDarrow/RemedyLua/blob/main/LICENSE-APACHE) or [MIT](https://github.com/AhmiDarrow/RemedyLua/blob/main/LICENSE-MIT), at
your option.
