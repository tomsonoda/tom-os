# RemedyLua 0.1.2, ported to run without an operating system

This is the source of the `remedy-lua` crate, version 0.1.2
(https://github.com/AhmiDarrow/RemedyLua, commit
686be3c8fd9ac6ba8932c3fb5d67faf053bd38a9, path `crates/remedy-lua`), by Ahmi
Darrow, licensed MIT OR Apache-2.0. The upstream README is in
`README.upstream.md`.

tom-os runs it inside the kernel, so the crate was changed to build with
`#![no_std]`:

- `src/nostd.rs` (new) stands in for the parts of `std` the crate uses:
  hash maps (`hashbrown` with a small keyed hasher), `Mutex` and
  `thread_local!` for a single thread of execution, `Instant`/`SystemTime`
  from the kernel's clocks, a byte-sink `io::Write`, and `f64` math from
  `libm`.
- Every `std::` path became `crate::nostd::`, each file imports
  `crate::nostd::prelude::*`, and hash maps are constructed with
  `default()` instead of `new()`.
- Left out: `src/front_oracle` (a test-only feature), tests, benches and
  examples. `Cargo.toml` lists the two new dependencies.

Nothing else was changed. The port was checked on a host build against the
unported crate and against C Lua 5.4.6 with a set of comparison scripts.
