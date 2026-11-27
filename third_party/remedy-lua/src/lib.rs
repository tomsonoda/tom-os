//! RemedyLua: a Lua 5.4-compatible scripting language, entirely in safe Rust.
//!
//! - **Lua 5.4 semantics**: values, tables with array and hash parts, metatables and
//!   metamethods (`__index`, `__newindex`, `__call`, `__eq`, `__lt`, `__le`, `__concat`,
//!   `__len`, arithmetic and bitwise ones, `__close`, `__tostring`, `__name`, `__pairs`,
//!   `__metatable`, `__mode`), closures with upvalues, varargs, multiple returns, integer
//!   and float subtypes, `goto`, `<const>` and `<close>`, coroutines, `error`/`pcall`/
//!   `xpcall` with any value. `.lua` files load as plain Lua 5.4.
//! - **RemedyLua additions** (`.rlua` files, [`Dialect::RemedyLua`]): optional type
//!   annotations (`local v: number = 1`, function signatures, table shapes, `type`
//!   aliases), checked by a gradual type checker ([`check`]), compound assignment
//!   (`+= -= *= /= //= %= ..=`), `continue`, and string interpolation (`` `hi {name}` ``).
//!   They are pure syntax at run time: they compile to the same bytecode plain Lua would.
//! - **A strict superset of Lua 5.4**: C Lua's fixed implementation limits (200 locals,
//!   255 upvalues, syntax nesting, C stack depth) are gone; only the host's budgets bound
//!   a program.
//! - **Limits in every step** ([`Limits`]): fuel (each instruction, and library functions
//!   charge for their work: `string.rep`, `gsub`, `sort`…), a memory cap counted when
//!   memory is taken, call depth, nesting of library callbacks, string and output sizes.
//!   A call over its fuel or memory stops with [`Error::Limit`]; scripts cannot catch it
//!   (`pcall`, `xpcall`, `coroutine.resume` and `__close` all pass it on).
//! - **An incremental collector**: marking and sweeping run in small steps paid for by
//!   allocation, so scripts run between frames without long pauses.
//! - **A host API**: Rust functions ([`Lua::create_function`], [`Lua::register`]), userdata
//!   ([`Lua::create_userdata`]), owned data in and out ([`Data`]), and capabilities
//!   ([`Libs`]): a host chooses which libraries each script gets. There is no file, process
//!   or network access in the language itself, and `load` takes text only, when allowed.
//!
//! # Embedding: handles and their lifetimes
//!
//! A [`Value`] is `Copy` and does not keep what it names alive. A heap value the host holds
//! is valid while one of these holds:
//!
//! - it is reachable from the script (a global, a table the script keeps);
//! - a [`Root`] holds it ([`Lua::root`]);
//! - the host made or got it outside a call ([`Lua::create_string`], [`Lua::create_table`],
//!   [`Lua::create_function`], [`Lua::create_userdata`], [`Lua::from_data`], [`Lua::get`],
//!   and the results of [`Lua::call`]/[`Lua::exec`]): then it is valid until the host's next
//!   [`Lua::call`] (or `exec`, `call_global`) returns, or until it calls
//!   [`Lua::collect_garbage`]. And so that a host that never calls cannot fill memory with
//!   them, they are let go when memory is full outside a call even after a collection
//!   (then refused as collected);
//! - inside a host function: it is one of the function's arguments, or the function made it
//!   (valid until the function returns). The results of a [`Lua::call`] made inside one are
//!   valid until its next `call`.
//!
//! A stale handle is refused ([`Error::Host`]) or reads as nil; it never reaches another
//! object, and a handle from another state is refused too. A panic that unwinds through a
//! call (a host function's, a clock's, a print sink's) poisons the state: every later call
//! returns [`Error::Host`]. The [reference](https://github.com/AhmiDarrow/RemedyLua/blob/main/docs/reference.md) has the full embedding guide.
//!
//! ```
//! use remedy_lua::{Lua, Limits, Libs, Value};
//! let mut lua = Lua::new(Limits::default(), Libs::STANDARD);
//! let out = lua.exec("local t = {} for i = 1, 3 do t[i] = i * i end return #t, t[3]", "=demo").unwrap();
//! assert!(matches!(out[..], [Value::Int(3), Value::Int(9)]));
//! ```

#![no_std]
#![deny(unsafe_code)]
// Some items are used only by the `front-oracle` feature or by tests.
#![allow(dead_code)]

extern crate alloc;
pub mod nostd;
#[allow(unused_imports)]
use crate::nostd::prelude::*;

mod ast;
mod bytecode;
mod check;
mod compiler;
mod debug;
mod diag;
mod fast;
mod fmt;
mod gctrace;
#[cfg(feature = "front-oracle")]
#[doc(hidden)]
#[path = "front.rs"]
pub mod __front;
#[cfg(feature = "front-oracle")]
mod front_oracle;
mod heap;
mod lexer;
mod number;
mod parser;
mod shardmap;
mod stdlib;
mod table;
mod value;
mod vm;

use crate::nostd::any::Any;
use crate::nostd::sync::atomic::{AtomicBool, Ordering};
use crate::nostd::sync::Arc;

pub use diag::{Diagnostic, Pos, Severity};
pub use value::{FuncRef, StrRef, TableRef, ThreadRef, UdRef, Value};
pub use stdlib::io::{IoError, Stream};
pub use vm::Lua;
#[doc(hidden)]
pub use gctrace::{FULL_BEGAN_MARK, FULL_BEGAN_PAUSE, FULL_BEGAN_SWEEP, TraceOp, TraceRecord};

/// What `io.open` asks for a file (`Lua::set_file_opener`): the name and the mode.
pub(crate) type IoOpener = crate::nostd::sync::Arc<dyn Fn(&str, &str) -> Result<Box<dyn Stream>, IoError> + Send + Sync>;

use vm::{Args, Flow, LuaError};
use lexer::Text;
use stdlib::base::Src;

/// Which language a source is in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Dialect {
    /// Plain Lua 5.4 (`.lua`).
    Lua54,
    /// RemedyLua (`.rlua`): Lua 5.4 plus annotations, compound assignment, `continue` and
    /// interpolation.
    RemedyLua,
}

impl Dialect {
    /// The dialect of a chunk named `name`: RemedyLua for `.rlua` files, else Lua 5.4.
    pub fn for_name(name: &str) -> Dialect {
        if name.to_ascii_lowercase().ends_with(".rlua") { Dialect::RemedyLua } else { Dialect::Lua54 }
    }
}

/// How much a state may use. Fuel is per call from the host; the rest hold always.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Limits {
    /// Steps one call from the host may take: one per instruction, plus what library
    /// functions charge for their work, plus collector work.
    pub fuel: u64,
    /// Bytes the state's objects may hold, with the free slots of each kind's slab (a
    /// collected object's slot stays resident until a new object of its kind reuses it,
    /// or until no object of its kind sits past it). The collector paces on the objects
    /// alone. Besides these, a state keeps at most 64 KiB (or 1/16 of this limit, if less)
    /// of freed table buffers for reuse, uncharged.
    pub memory: usize,
    /// Lua call frames one thread may have (deeper recursion is a `stack overflow` error).
    /// Unlimited by default: frames are memory like the value stack, charged to `memory`,
    /// and a call whose frame does not fit is the same catchable `stack overflow`. Lua-to-Lua
    /// calls use no native stack.
    pub call_depth: usize,
    /// How deeply library functions may call back into Lua (`sort` comparators,
    /// `__tostring`, `gsub` functions inside each other).
    pub native_depth: usize,
    /// Stack slots one thread may have. Unlimited by default: the stack is memory like any
    /// other, charged to `memory` before it grows.
    pub stack_slots: usize,
    /// The longest string a script may make.
    pub string_len: usize,
    /// Bytes `print` may write per call from the host (and, with no sink set, the most
    /// [`Lua::take_output`]'s buffer holds; what does not fit is dropped).
    pub output: usize,
    /// The longest error message kept.
    pub error_len: usize,
    /// Bytes of the native stack one call may use for library functions that call back
    /// into Lua (and for the parser's recursion). Lua-to-Lua calls use none.
    pub stack_bytes: usize,
    /// The seed of the state's hashing and of `math.random`'s first state; `None` (the
    /// default) is a fresh random seed per state, which keeps a script from aiming keys at
    /// one hash slot. A fixed seed is for replays and tests: the same program then gives the
    /// same results, `pairs` order and fuel on every run (see `hash_seed`).
    pub hash_seed: Option<u64>,
    /// Strict Lua 5.4: leave out what stock C Lua 5.4 builds with `LUA_COMPAT_5_3` (its
    /// makefile's default), which RemedyLua offers by default: `__le` emulated with `__lt`
    /// and `math.atan2`/`cosh`/`sinh`/`tanh`/`pow`/`frexp`/`ldexp`/`log10`. Read when the
    /// state is made, and fixed for its life ([`Lua::set_limits`] keeps it).
    pub strict_5_4: bool,
}

impl Default for Limits {
    /// 10 million steps, 64 MiB, 200 nested callbacks within 512 KiB of native stack, call
    /// frames and a stack bounded only by memory, 16 MiB strings, 1 MiB of output, 4 KiB
    /// messages.
    fn default() -> Self {
        Limits {
            fuel: 10_000_000,
            memory: 64 << 20,
            call_depth: usize::MAX,
            native_depth: 200,
            stack_slots: usize::MAX / 64,
            string_len: 16 << 20,
            output: 1 << 20,
            error_len: 4096,
            stack_bytes: 512 << 10,
            hash_seed: None,
            strict_5_4: false,
        }
    }
}

/// `Limits`' plain setters: `name(n)` sets the field `name`.
macro_rules! setters {
    ($($f:ident: $t:ty,)*) => {
        $(
            #[doc = concat!("Sets `", stringify!($f), "`.")]
            pub fn $f(mut self, n: $t) -> Self {
                self.$f = n;
                self
            }
        )*
    };
}

impl Limits {
    /// No budgets at all (for benchmarks and trusted code): fuel and memory are unlimited
    /// and the depth and size limits are as large as the machine allows. The interpreter
    /// still counts, so a state can be measured; nothing a script does can hit a limit
    /// except the native stack guard, which protects the process.
    pub fn unmetered() -> Self {
        Limits {
            fuel: u64::MAX,
            memory: usize::MAX,
            call_depth: usize::MAX,
            stack_slots: usize::MAX / 64,
            string_len: usize::MAX / 2,
            output: usize::MAX,
            error_len: usize::MAX / 2,
            ..Limits::default()
        }
    }

    /// A fixed seed for hashing, `math.random` and `math.randomseed()` without arguments, so
    /// that runs replay exactly: the same program with the same seed gives the same results,
    /// `pairs` order and fuel every time (with the fast loop on or off). For replays and
    /// tests; production code should keep the default, a random seed per state, which
    /// defends against hash flooding. Read when the state is made.
    pub fn hash_seed(mut self, seed: u64) -> Self {
        self.hash_seed = Some(seed);
        self
    }

    /// Sets `strict_5_4`: no Lua 5.3 compatibility (see the field).
    pub fn strict_5_4(mut self, on: bool) -> Self {
        self.strict_5_4 = on;
        self
    }

    setters! {
        fuel: u64,
        memory: usize,
        call_depth: usize,
        native_depth: usize,
        string_len: usize,
        output: usize,
        stack_slots: usize,
        error_len: usize,
        stack_bytes: usize,
    }
}

/// The libraries a script gets: its capabilities. Combine with `|`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Libs(u32);

impl Libs {
    /// Nothing.
    pub const NONE: Libs = Libs(0);
    /// The base functions (`type`, `pairs`, `pcall`, `setmetatable`, `tostring`…).
    pub const BASE: Libs = Libs(1);
    /// `string`, and the string metatable (`s:upper()`).
    pub const STRING: Libs = Libs(1 << 1);
    /// `table`.
    pub const TABLE: Libs = Libs(1 << 2);
    /// `math`.
    pub const MATH: Libs = Libs(1 << 3);
    /// `utf8`.
    pub const UTF8: Libs = Libs(1 << 4);
    /// `coroutine`.
    pub const COROUTINE: Libs = Libs(1 << 5);
    /// `os.time`, `os.clock`, `os.date`, `os.difftime`, `os.setlocale` (the `"C"` locale
    /// only; `os.getenv` is `ENV`'s).
    pub const OS: Libs = Libs(1 << 6);
    /// `load` (source text only, never bytecode).
    pub const LOAD: Libs = Libs(1 << 7);
    /// `print` (to the host's sink or [`Lua::take_output`]).
    pub const PRINT: Libs = Libs(1 << 8);
    /// `collectgarbage` (count, step, collect; steps are paid for in fuel).
    pub const GC: Libs = Libs(1 << 9);
    /// `debug.traceback` and a read-only `debug.getinfo` (no hooks, locals, upvalues,
    /// registry or functions handed out). Not in `SAFE`, `STANDARD` or `ALL`: a host opts in.
    pub const DEBUG_INFO: Libs = Libs(1 << 10);
    /// `require` and `package` (`loaded`, `preload`, `searchers`), with modules only from the
    /// host ([`Lua::add_module`], [`Lua::set_module_loader`]): no search path, no files. Not
    /// in `SAFE`, `STANDARD` or `ALL`: a host opts in.
    pub const REQUIRE: Libs = Libs(1 << 11);
    /// `os.getenv`, reading only what the host put in the state's map ([`Lua::set_env`]),
    /// never the process environment. Not in `SAFE`, `STANDARD` or `ALL`: a host opts in.
    pub const ENV: Libs = Libs(1 << 12);
    /// `io` over streams the host gives: `io.stdin`, `io.stdout` and `io.stderr` are the
    /// host's ([`Lua::set_stdin`], [`Lua::set_stdout`], [`Lua::set_stderr`]; by default
    /// stdin is empty and the others write where `print` writes), and `io.open` asks the
    /// host's opener ([`Lua::set_file_opener`]): no file system, no process (`io.popen` and
    /// `io.tmpfile` are absent). Not in `SAFE`, `STANDARD` or `ALL`: a host opts in.
    pub const IO: Libs = Libs(1 << 13);
    /// Everything but `load`, `print` and `collectgarbage`: what untrusted code gets.
    pub const SAFE: Libs = Libs(0b111_1111);
    /// The safe set plus `print` and `collectgarbage`.
    pub const STANDARD: Libs = Libs(0b111_1111 | (1 << 8) | (1 << 9));
    /// Every library of the standard set, `load` included (not the opt-in `DEBUG_INFO`,
    /// `REQUIRE`, `ENV` and `IO`).
    pub const ALL: Libs = Libs(0b11_1111_1111);

    /// Both sets (`|` in a `const`).
    pub const fn union(self, other: Libs) -> Libs {
        Libs(self.0 | other.0)
    }

    /// Whether `other` is all included.
    pub fn contains(self, other: Libs) -> bool {
        self.0 & other.0 == other.0
    }
}

impl crate::nostd::ops::BitOr for Libs {
    type Output = Libs;
    fn bitor(self, rhs: Libs) -> Libs {
        Libs(self.0 | rhs.0)
    }
}

impl crate::nostd::ops::Sub for Libs {
    type Output = Libs;
    fn sub(self, rhs: Libs) -> Libs {
        Libs(self.0 & !rhs.0)
    }
}

/// A module's source for `require` (`Libs::REQUIRE`), from the host: Lua source text,
/// compiled as `load` compiles it (text only, charged the same) as chunk `@origin`.
/// `require` gives `origin` as its second result, as C Lua gives a module's file name.
#[derive(Clone, Debug)]
pub struct ModuleSource {
    /// The source text.
    pub code: Arc<[u8]>,
    /// Where it came from, for messages and tracebacks (`origin:3: ...`).
    pub origin: String,
}

impl ModuleSource {
    /// A module's source, shown in messages as `origin`.
    pub fn new(code: impl AsRef<[u8]>, origin: impl Into<String>) -> ModuleSource {
        ModuleSource { code: Arc::from(code.as_ref()), origin: origin.into() }
    }
}

/// The host's module loader (`Lua::set_module_loader`): the source of the module named,
/// or `None` when the host has no such module. It runs inside the script's call, on the
/// host's own time; it gets no access to the state.
pub type ModuleLoader = Arc<dyn Fn(&str) -> Option<ModuleSource> + Send + Sync>;

/// Which limit stopped a call. More kinds may come: match with a `_` arm.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum LimitKind {
    /// It ran out of fuel (took too many steps).
    Fuel,
    /// It used too much memory, or made a string too long.
    Memory,
    /// The host interrupted it ([`Interrupt`]).
    Interrupted,
    /// It ran past the host's time limit ([`Lua::set_time_limit`]).
    Time,
}

/// The host's clock for time limits: the time now, from any fixed start (a wall clock, or
/// a thread's CPU clock).
pub type Clock = crate::nostd::sync::Arc<dyn Fn() -> crate::nostd::time::Duration + Send + Sync>;

/// The `os` library's clocks where the platform has them: the system time, and the time
/// since the state was made. A browser (`wasm32-unknown-unknown`) has neither in `std`:
/// there the host supplies them ([`Lua::set_os_clocks`]), or they read as 0.
#[cfg(not(all(target_arch = "wasm32", target_os = "unknown")))]
pub(crate) fn default_os_clocks() -> (Option<Clock>, Option<Clock>) {
    let start = crate::nostd::time::Instant::now();
    let wall: Clock = Arc::new(|| crate::nostd::time::SystemTime::now().duration_since(crate::nostd::time::UNIX_EPOCH).unwrap_or_default());
    (Some(wall), Some(Arc::new(move || start.elapsed())))
}

#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
pub(crate) fn default_os_clocks() -> (Option<Clock>, Option<Clock>) {
    (None, None)
}

/// A handle that stops the state's running call from any thread: [`Interrupt::interrupt`]
/// makes the call stop with [`Error::Limit`]`(`[`LimitKind::Interrupted`]`)` within a fraction
/// of a millisecond of work, wherever it is (a loop, a long `gsub` or `sort`, a coroutine),
/// and scripts cannot catch it. It stays set until [`Interrupt::reset`], and while it is set
/// every entry into the state (a call, or making a value) stops at once.
#[derive(Clone, Debug)]
pub struct Interrupt(Arc<AtomicBool>);

impl Interrupt {
    /// Asks the running call (and every call after it, until reset) to stop.
    pub fn interrupt(&self) {
        self.0.store(true, Ordering::Relaxed);
    }

    /// Lets calls run again. A host that reuses one state for many calls resets the
    /// handle before each call it wants to run.
    pub fn reset(&self) {
        self.0.store(false, Ordering::Relaxed);
    }

    /// Whether it is set.
    pub fn is_set(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

/// What went wrong.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Error {
    /// The source does not compile.
    Syntax(Diagnostic),
    /// The script raised an error that nothing caught.
    Runtime {
        /// The error, as text (`chunk:line: message` for messages raised by `error` or the
        /// machine; other values are described).
        message: String,
        /// Where it happened.
        traceback: String,
    },
    /// The call ran past a limit and was stopped.
    Limit(LimitKind),
    /// The host used the API wrongly (a value whose object was collected, a value that
    /// cannot become data).
    Host(String),
}

impl Error {
    /// A runtime error with this message, for host functions to return.
    pub fn runtime(message: impl Into<String>) -> Error {
        Error::Runtime { message: message.into(), traceback: String::new() }
    }

    /// The message alone.
    pub fn message(&self) -> String {
        match self {
            Error::Syntax(d) => d.to_string(),
            Error::Runtime { message, .. } => message.clone(),
            Error::Limit(LimitKind::Fuel) => "the script ran too long and was stopped".into(),
            Error::Limit(LimitKind::Memory) => "the script used too much memory and was stopped".into(),
            Error::Limit(LimitKind::Interrupted) => "the script was interrupted by the host".into(),
            Error::Limit(LimitKind::Time) => "the script ran past its time limit and was stopped".into(),
            Error::Host(m) => m.clone(),
        }
    }
}

impl crate::nostd::fmt::Display for Error {
    fn fmt(&self, f: &mut crate::nostd::fmt::Formatter<'_>) -> crate::nostd::fmt::Result {
        f.write_str(&self.message())
    }
}

impl crate::nostd::error::Error for Error {}

/// A value kept alive while this handle lives (values themselves do not keep objects
/// alive; see [`Value`]).
#[derive(Clone, Debug)]
pub struct Root {
    value: Value,
    _alive: Arc<()>,
}

impl Root {
    /// The value.
    pub fn value(&self) -> Value {
        self.value
    }
}

/// Owned data that crosses between Rust and scripts by copy: no functions, userdata or
/// threads; tables become key-value lists in traversal order. More variants may come:
/// match with a `_` arm.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum Data {
    /// `nil`.
    Nil,
    /// A boolean.
    Bool(bool),
    /// An integer.
    Int(i64),
    /// A float.
    Num(f64),
    /// A string (invalid UTF-8 replaced).
    Str(String),
    /// A table's entries.
    Table(Vec<(Data, Data)>),
}

impl Data {
    /// The value under string key `key`, for tables.
    pub fn get(&self, key: &str) -> Option<&Data> {
        match self {
            Data::Table(pairs) => pairs.iter().find(|(k, _)| matches!(k, Data::Str(s) if s == key)).map(|(_, v)| v),
            _ => None,
        }
    }
}

/// The deepest nesting [`Lua::to_data`] follows, whatever [`DataLimits::depth`] asks.
pub const MAX_DATA_DEPTH: usize = 200;

/// Bounds for copying a value out as [`Data`].
#[derive(Clone, Copy, Debug)]
pub struct DataLimits {
    /// Deepest nesting (at most `MAX_DATA_DEPTH`, 200: each level is a native call).
    pub depth: usize,
    /// Most values in all.
    pub nodes: usize,
    /// Most string bytes in all.
    pub bytes: usize,
}

impl Default for DataLimits {
    /// 32 deep, 200 000 values, 8 MiB.
    fn default() -> Self {
        DataLimits { depth: 32, nodes: 200_000, bytes: 8 << 20 }
    }
}

/// Parses `source` (a `.rlua` or `.lua` file, by `name`'s extension) without running
/// anything, and type-checks a `.rlua` file's annotations: its diagnostics. A syntax error
/// is the only diagnostic (the first one). Otherwise, for `.rlua`, the type errors
/// (`Severity::Error`, at most 500), and a `Severity::Warning` if the check stopped at its
/// work or memory bound. A plain `.lua` file has no annotations, so it gets none.
/// Checking never changes how the program runs (see [The type checker](https://github.com/AhmiDarrow/RemedyLua/blob/main/docs/reference.md#the-type-checker)).
///
/// The parse may use 1 KiB per source byte plus 64 MiB ("not enough memory" past it), and
/// the checker as much again for its own tables; its work is linear in the source.
pub fn check(source: &[u8], name: &str) -> Vec<Diagnostic> {
    let dialect = Dialect::for_name(name);
    let memory = parser::host_memory(source.len());
    match parser::parse_within(source, name, dialect, memory) {
        Err(d) => vec![d],
        Ok(block) if dialect == Dialect::RemedyLua => check::check(&block, name, source.len(), memory),
        Ok(_) => Vec::new(),
    }
}

/// Re-indents `source` (four spaces a level, normalized spacing at line starts), keeping
/// comments and everything else as written. Fails when the source does not parse (within
/// the memory [`check`] may use).
pub fn format(source: &str, name: &str) -> Result<String, Diagnostic> {
    fmt::format(source, name, Dialect::for_name(name))
}

fn to_error(lua: &mut Lua, e: LuaError) -> Error {
    match *e.kind() {
        vm::ErrorKind::Limit(k) => Error::Limit(k),
        vm::ErrorKind::Pending => Error::Host("the call is pending".into()),
        vm::ErrorKind::Value(v) => {
            let message = lua.describe_error(v);
            // Kept (rooted) so a host function re-raising it passes the object on.
            lua.last_error = Some((message.clone(), v));
            Error::Runtime { message, traceback: lua.take_traceback() }
        }
        _ => match lua.normalize(e) {
            Ok(v) => {
                let message = lua.describe_error(v);
                Error::Runtime { message, traceback: lua.take_traceback() }
            }
            Err(e) => e.limit().map_or_else(|| Error::runtime("error"), Error::Limit),
        },
    }
}

/// A state dropped by the host frees what compiles from the host left to free on this
/// thread (`diag::free_deferred`; nothing waits for it to return promptly here).
impl Drop for Lua {
    fn drop(&mut self) {
        crate::diag::free_deferred(&mut self.deferred, || false);
    }
}

impl Lua {
    /// A host function's error, as the script sees it: the very error object when it is
    /// one that just came out of the state (`lua.call` from the host function).
    fn from_host_error(&mut self, e: Error) -> LuaError {
        match e {
            Error::Limit(k) => LuaError::Limit(k),
            Error::Runtime { message, .. } => {
                if let Some((text, v)) = self.last_error.take()
                    && text == message
                    && self.heap.is_live(v)
                {
                    return LuaError::Value(v);
                }
                LuaError::Msg(message)
            }
            other => LuaError::Msg(other.message()),
        }
    }
}

impl Lua {
    /// A state with `limits`, offering `libs`.
    pub fn new(limits: Limits, libs: Libs) -> Lua {
        Lua::new_vm(limits, libs)
    }

    fn describe_error(&self, v: Value) -> String {
        match v {
            Value::Str(s) => {
                let b = self.bytes(s);
                let cut = b.len().min(self.limits.error_len);
                String::from_utf8_lossy(b.get(..cut).unwrap_or_default()).into_owned()
            }
            Value::Int(i) => i.to_string(),
            Value::Num(f) => number::fmt_float(f),
            Value::Nil => "nil".into(),
            // As `lua.c`'s message handler: `luaL_typename`, never a `__name`.
            other => format!("(error object is a {} value)", other.type_name()),
        }
    }

    fn check_live(&mut self, v: Value) -> Result<(), Error> {
        // An atomic step a call before stopped in is finished first: while it is
        // suspended, a weak table's value it is about to clear still reads as live, and
        // stored from the host it was freed in a live table (SEC7-H2). (Only between
        // calls: the outermost entry finishes it, charged there.)
        if self.entry == 0 && self.heap.atomic_suspended() {
            self.enter(|_| Ok(()))?;
        }
        self.check_live_now(v)
    }

    /// `check_live` without finishing a suspended atomic step: for reads, which store
    /// nothing.
    fn check_live_now(&self, v: Value) -> Result<(), Error> {
        // S5API-9: also what refuses a poisoned state in the entries that do not go through
        // `enter` (`set_metatable` wrote into it, `to_data` read it as if nothing happened).
        if self.poisoned {
            return Err(Error::Host("a panic in an earlier call left this state unusable".into()));
        }
        self.not_pending()?;
        if self.heap.is_live(v) { Ok(()) } else { Err(Error::Host("a value whose object was collected".into())) }
    }

    /// While a call is paused at a collector step the state is in the middle of it:
    /// nothing else may run in it, write to it or allocate in it (SEC7-G1).
    fn not_pending(&self) -> Result<(), Error> {
        if self.entry == 0 && (self.pending_call.is_some() || self.pending_start.is_some()) {
            return Err(Error::Host("a call is pending: resume or drop it first".into()));
        }
        Ok(())
    }

    /// Runs `f` as an entry from the host: fuel is reset for the outermost one.
    fn enter<T>(&mut self, f: impl FnOnce(&mut Lua) -> Result<T, LuaError>) -> Result<T, Error> {
        self.not_pending()?;
        self.enter_as(true, f)
    }

    /// `enter`; `fresh` false: a paused call resumed, which keeps its fuel, its deadline
    /// (the absolute one it started with) and what it carried: it is the same call.
    fn enter_as<T>(&mut self, fresh: bool, f: impl FnOnce(&mut Lua) -> Result<T, LuaError>) -> Result<T, Error> {
        if self.poisoned {
            return Err(Error::Host("a panic in an earlier call left this state unusable".into()));
        }
        // The counters below are per thread: a state run from another state's host function
        // keeps the outer state's uncharged steps aside and gives them back when it returns,
        // so neither bills the other (S6-RES-4).
        let mut outer = None;
        if self.entry == 0 {
            // What a compile left to free, before this call's clock.
            self.free_deferred();
            // Probe steps of host-side table access are not the script's.
            outer = Some((crate::table::take_probes(), crate::debug::take_work()));
            // The host's poll starts (SEC7-G1).
            self.heap.poll_start = self.heap.scanned;
            self.heap.poll_objs = 0;
            self.stack_base = stack_pos();
        }
        if self.entry == 0 && fresh {
            let total = i64::try_from(self.limits.fuel).unwrap_or(i64::MAX);
            self.fuel = total.min(vm::FUEL_SLICE);
            self.reserve = total - self.fuel;
            // Collector work under a unit is carried within a call only (S5C-1), so a
            // call's fuel never depends on what calls before it did. Marking work a step
            // did past its budget (`gc_mark_over`) is not dropped, though: it is owed, as an
            // atomic phase's is (`atomic_owed`), and charged by the steps of the calls after
            // (dropped, a host stepping the collector a call at a time had that work
            // uncharged, by an amount that followed the order objects are reached in, so
            // the seed: S6GC-2e-R).
            self.gc_work_carry = 0;
            self.deadline = self.time_limit.as_ref().map(|(clock, d)| clock().saturating_add(*d));
            // What long table resizes look at between pieces.
            let w = crate::stdlib::pattern::Watch::of(self);
            self.heap.mem.deadline = Some(w);
            self.heap.mem.refused = false;
            self.output_used = 0;
            self.limit_hit = None;
            self.stopped_left = None;
            self.last_error = None;
            // The last call's results were valid until now. A long list is not kept: one a
            // limit cut short (or the host's results) was outside the account (SEC7-1).
            crate::vm::clear_list(&mut self.rets);
            crate::vm::clear_list(&mut self.host_rets);
            self.host_rets_base = 0;
        }
        // An interrupt the host set stops every call until it is reset, even one that would
        // finish before its first look at it.
        if self.entry == 0 && self.interrupt.load(Ordering::Relaxed) {
            if let Some((probes, work)) = outer {
                crate::table::put_probes(probes);
                crate::debug::put_work(work);
            }
            self.limit_hit = Some(LimitKind::Interrupted);
            return Err(Error::Limit(LimitKind::Interrupted));
        }
        self.entry += 1;
        // A host that catches a panic from a host function must not get the state back
        // mid-instruction (wrong thread, frames and fuel): it is poisoned, like a mutex.
        let outermost = self.entry == 1;
        let r = match crate::nostd::panic::catch_unwind(crate::nostd::panic::AssertUnwindSafe(|| {
            // An atomic phase a call before stopped at its fuel is finished first (S6GC-2e).
            // (A paused call resumed goes on with its own step instead.)
            if outermost && fresh {
                self.resume_atomic_pausable()?;
            }
            f(self)
        })) {
            Ok(r) => r,
            Err(p) => {
                self.poisoned = true;
                // A host function further out may catch this panic and return normally: the
                // script it returns to must stop at once (uncatchably), not run on.
                self.limit_hit = Some(LimitKind::Interrupted);
                self.fuel = -1;
                self.reserve = 0;
                crate::nostd::panic::resume_unwind(p)
            }
        };
        self.entry -= 1;
        if let Some((probes, work)) = outer {
            // This call's last uncharged steps are host-side now (as at its start).
            crate::table::put_probes(probes);
            crate::debug::put_work(work);
        }
        if self.poisoned {
            return Err(Error::Host("a panic in an earlier call left this state unusable".into()));
        }
        if let Some(k) = r.as_ref().err().and_then(LuaError::limit) {
            // Sticky for the rest of this call from the host. The fuel used stays what ran
            // (the innermost call from the host sees the limit first).
            self.limit_hit = Some(k);
            if self.stopped_left.is_none() {
                self.stopped_left = Some(self.fuel_left());
            }
            self.fuel = -1;
            self.reserve = 0;
        }
        if self.entry == 0 {
            self.clear_idle_stack();
        }
        r.map_err(|e| to_error(self, e))
    }

    /// Compiles `source` as a chunk named `name` (`=name` prints as `name` in messages,
    /// `@file` as a file name); the dialect follows the name's extension. The chunk is a
    /// function: call it to run it.
    pub fn load(&mut self, source: impl AsRef<[u8]>, name: &str) -> Result<Root, Error> {
        let dialect = Dialect::for_name(name.trim_start_matches(['=', '@']));
        self.load_with(source, name, dialect)
    }

    /// [`Lua::load`] in a given dialect.
    pub fn load_with(&mut self, source: impl AsRef<[u8]>, name: &str, dialect: Dialect) -> Result<Root, Error> {
        // Compiling allocates (and may collect) outside `enter`: refused like every other
        // entry once a panic left the state mid-instruction.
        if self.poisoned {
            return Err(Error::Host("a panic in an earlier call left this state unusable".into()));
        }
        let env = Value::Table(self.globals);
        let mut r = self.compile_chunk(&mut Src::Text(source.as_ref()), name, dialect, env);
        if matches!(r, Err(CompileError::Memory)) {
            // Garbage may be what fills memory: collect and try again.
            self.collect_garbage()?;
            r = self.compile_chunk(&mut Src::Text(source.as_ref()), name, dialect, env);
        }
        let f = r.map_err(|e| match e {
            CompileError::Syntax(d) => Error::Syntax(d),
            CompileError::Memory => Error::Limit(LimitKind::Memory),
            CompileError::Limit(k) => Error::Limit(k),
        })?;
        let root = self.root(f);
        // `debug.getinfo`'s `source`. Out of memory, it stays unset (and reads as the
        // short name).
        if let Ok(n) = self.new_str(name.as_bytes()) {
            self.set_chunk_name(f, n);
        }
        Ok(root)
    }

    pub(crate) fn compile_chunk(
        &mut self,
        source: &mut Src<'_>,
        name: &str,
        dialect: Dialect,
        env: Value,
    ) -> Result<Value, CompileError> {
        if self.entry == 0 {
            self.free_deferred();
        }
        if source.bytes().first() == Some(&0x1b) {
            // Named as every other message names the chunk (it said `=t:1:`; S5C-FRONT-8).
            let d = Diagnostic::error(&vm::chunk_id(name), Pos { line: 1, col: 1 }, "attempt to load a binary chunk (bytecode is not accepted)");
            return Err(CompileError::Syntax(d));
        }
        // The host's interrupt stops a long compile too, and so does the running call's
        // deadline (a load from the host outside any call has none).
        let mut watch = crate::stdlib::pattern::Watch::of(self);
        if self.entry == 0 {
            watch = watch.without_deadline();
        }
        // Inside a call, the compiler's scan work is fuel (`diag::compile_work`); a load
        // from the host outside any call has no fuel, only the interrupt.
        let in_call = self.entry > 0;
        let max_work = if in_call { self.fuel_left().saturating_mul(COMPILE_WORK_PER_FUEL) } else { u64::MAX };
        let (r, stop, work) =
            crate::diag::watched(watch, max_work, || self.compile_watched(source, name, dialect, env));
        self.adopt_deferred();
        if in_call
            && work >= COMPILE_WORK_PER_FUEL
            && let Some(k) = self.charge(work / COMPILE_WORK_PER_FUEL).err().as_ref().and_then(vm::LuaError::limit)
        {
            return Err(CompileError::Limit(k));
        }
        match stop {
            Some(k) => Err(CompileError::Limit(k)),
            None => r,
        }
    }

    /// Takes what the compile just run left to free a piece at a time, charged to this
    /// state's memory until it is freed (S6-FRONT-4: it sat uncharged until the next call
    /// from the host). Held for the account only: not debt for the collector to work off.
    fn adopt_deferred(&mut self) {
        for (v, bytes) in crate::diag::take_deferred() {
            self.heap.mem.used = self.heap.mem.used.saturating_add(bytes);
            self.deferred.push((v, bytes));
        }
    }

    /// Frees what compiles left, a piece at a time, until the host's interrupt says to
    /// stop; what is freed is released from the account.
    fn free_deferred(&mut self) {
        if self.deferred.is_empty() {
            return;
        }
        let flag = crate::nostd::sync::Arc::clone(&self.interrupt);
        let released = crate::diag::free_deferred(&mut self.deferred, || flag.load(crate::nostd::sync::atomic::Ordering::Relaxed));
        self.heap.mem.release(released);
    }

    fn compile_watched(&mut self, source: &mut Src<'_>, name: &str, dialect: Dialect, env: Value) -> Result<Value, CompileError> {
        // From the host, what the compile built is freed at its next call, a piece at a
        // time, watching the interrupt (S6-FRONT-4); inside a call, at once.
        let host = self.entry == 0;
        let was = crate::diag::set_deferring(host);
        let r = self.compile_parsed(source, name, dialect, env, host);
        crate::diag::set_deferring(was);
        r
    }

    fn compile_parsed(&mut self, source: &mut Src<'_>, name: &str, dialect: Dialect, env: Value, host: bool) -> Result<Value, CompileError> {
        let display = vm::chunk_id(name);
        // (The table buffer pool's bytes are room too: the charge after the parse gives
        // them back if it needs them, `Mem::fits`.)
        let room = self.heap.mem.cap.saturating_sub(self.memory_used());
        // A reader is called as the lexer reaches the end of what it gave.
        let parsed = match source {
            Src::Text(s) => parser::parse_counted(Text::Whole(s), &display, dialect, room, host).0,
            Src::Reader(st, buf) => {
                // While the reader runs, what the parse holds counts as used; after, the
                // parse has the room the reader left it (SEC-M13).
                let mut pull = |b: &mut Vec<u8>| {
                    let (held, _) = crate::diag::parse_reading((None, None));
                    self.heap.mem.used = self.heap.mem.used.saturating_add(held);
                    let more = crate::stdlib::base::read_piece(self, st, b);
                    self.heap.mem.used = self.heap.mem.used.saturating_sub(held);
                    crate::diag::parse_reading((None, Some(self.heap.mem.cap.saturating_sub(self.heap.mem.used))));
                    more
                };
                let (r, text) = parser::parse_counted(Text::Read(crate::nostd::mem::take(*buf), Some(&mut pull)), &display, dialect, room, host);
                **buf = text.take();
                r
            }
        };
        let source = source.bytes();
        let (block, tree) = match parsed {
            Ok(b) => b,
            Err(d) if d.message == "not enough memory" => return Err(CompileError::Memory),
            // Stopped by the host: no second parse for the message (`watched` reports it).
            Err(d) if d.message == crate::diag::COMPILE_STOPPED => return Err(CompileError::Syntax(d)),
            Err(d) => return Err(CompileError::Syntax(self.first_error(source, &display, dialect, room, d))),
        };
        // The tree is held while it is compiled: charged, so the compiler's own memory is
        // what is left beside it (S6-FRONT-3: the two were each bounded by the room left,
        // and a compile held up to twice the limit).
        // (Held for the compile only: not debt for the collector to work off.)
        if !self.heap.mem.fits(tree) {
            return Err(CompileError::Memory);
        }
        self.heap.mem.used += tree;
        let compiled = compiler::compile(&block, &display, &mut self.heap);
        self.heap.mem.release(tree);
        let proto = match compiled {
            Ok(p) => {
                if host {
                    crate::diag::defer_tree(block, parser::tree_bytes());
                } else {
                    // Inside a call: now, a node at a time between looks (a 6 MB source's
                    // tree took up to 120 ms to drop in one go).
                    // (Told to stop, the rest is deferred and charged as the host path's is.)
                    crate::diag::free_watched(crate::ast::Dropper::of_block(block).with_bytes(parser::tree_bytes()));
                }
                p
            }
            Err(d) => {
                if d.message == crate::diag::COMPILE_STOPPED || host {
                    // Freed later, so the stopped compile returns at once (S5C-FRONT-12).
                    crate::diag::defer_tree(block, parser::tree_bytes());
                }
                return Err(if d.message == "not enough memory" { CompileError::Memory } else { CompileError::Syntax(d) });
            }
        };
        self.heap.blocked += 1;
        let r = (|| {
            self.heap.mem.charge(heap::UPVAL_SIZE + heap::FUNC_BASE + heap::boxed(8)).map_err(|()| CompileError::Memory)?;
            let up = self.heap.new_upval_charged(heap::Upval::Closed(env));
            Ok(self.heap.new_func_charged(heap::Func::Lua { proto, upvals: vec![up].into_boxed_slice() }))
        })();
        self.heap.blocked -= 1;
        let f = Value::Func(r?);
        self.hold(f);
        Ok(f)
    }

    /// The error C Lua reports for `source`, which has the syntax error `syntax`: C Lua
    /// compiles as it parses, so a compile error before that point comes first.
    fn first_error(&mut self, source: &[u8], name: &str, dialect: Dialect, room: usize, syntax: Diagnostic) -> Diagnostic {
        let Some((block, cut, follow)) = parser::prefix(source, name, dialect, room) else { return syntax };
        match compiler::compile_prefix(&block, name, &mut self.heap, cut, follow) {
            Err(d) if d.message != "not enough memory" && d.line <= syntax.line => d,
            _ => syntax,
        }
    }

    /// Compiles and runs `source`: what it returns.
    pub fn exec(&mut self, source: impl AsRef<[u8]>, name: &str) -> Result<Vec<Value>, Error> {
        let f = self.load(source, name)?;
        self.call(f.value(), &[])
    }

    /// Calls `f` with `args` (a function, or a value with `__call`): its results. Fuel and
    /// the time limit start afresh for a call from the host (not for one made inside a host
    /// function). From the host, the results stay valid until its next `call` returns, and
    /// the values it made before this call are released when it returns; inside a host
    /// function, until that function's next `call` (see [the crate docs](crate)). Root a
    /// value to keep it longer.
    pub fn call(&mut self, f: Value, args: &[Value]) -> Result<Vec<Value>, Error> {
        // The host's poll, traced (SEC7-G1).
        if self.entry == 0 && self.heap.mem.trace.is_some() {
            let p = self.trace_poll();
            let r = self.call_now(f, args);
            if let Some(p) = p {
                self.trace_poll_end(p);
            }
            return r;
        }
        self.call_now(f, args)
    }

    fn call_now(&mut self, f: Value, args: &[Value]) -> Result<Vec<Value>, Error> {
        if let Err(e) = self.check_live(f) {
            // Paused in the entry that finishes a suspended atomic step first (SEC7-G1):
            // the call is pending before it started; it goes on from there.
            if self.entry == 0 && self.gc_pending.is_some() && self.pending_call.is_none() {
                self.pending_start = Some((f, args.to_vec()));
            }
            return Err(e);
        }
        for a in args {
            self.check_live(*a)?;
        }
        let mark = self.scratch.len();
        let out = self.enter(|l| {
            l.scratch.push(f);
            l.scratch.extend_from_slice(args);
            l.call_nested(f, args)
        });
        // Results are kept reachable until the next call (in `rets`), not for the rest of
        // a host function that calls in a loop.
        self.scratch.truncate(mark);
        self.after_host_call(out)
    }

    /// What `call` does with a call's results (a resumed one's too).
    fn after_host_call(&mut self, out: Result<Vec<Value>, Error>) -> Result<Vec<Value>, Error> {
        if self.entry == 0 {
            // From the host itself: what it held until now may go, the results stay.
            self.held.clear();
            self.held.extend(out.iter().flatten().copied().filter(|v| v.is_collectable()));
        }
        let out = out?;
        if self.native_depth > 0 {
            // Inside a host function: valid until its next call (S5API-3). They were kept in
            // `rets`, which any native the host function reached later (a metamethod's
            // `tostring`, run by `get`) cleared, so they could be collected under it.
            self.host_rets.truncate(self.host_rets_base);
            self.host_rets.extend(out.iter().copied().filter(|v| v.is_collectable()));
            // S6-4: each object once (they are uncharged copies, and every host function
            // further out keeps its own): 100,000 results naming one table, kept at each
            // of 74 nested levels, held 128 MB resident against 5.3 MB accounted.
            if self.host_rets.len() - self.host_rets_base > HOST_RETS_COMPACT {
                crate::vm::compact_values(&mut self.host_rets, self.host_rets_base);
            }
        }
        Ok(out)
    }

    /// Starts calling `f` as [`Lua::call`] does, but at most `poll_slots` table slots of
    /// the collector's work in this poll (and in each poll after): when the collector
    /// reaches that, the call pauses and this returns [`CallPoll::Pending`]; the host
    /// resumes it with [`Lua::resume_call`] (in its next frame, say) or drops it with
    /// [`Lua::cancel_call`]. A pause is invisible to the script: its results, its fuel,
    /// where a limit stops it and every effect are those of `call`. While a call is
    /// pending nothing else runs in the state (every entry returns an error).
    #[cfg(feature = "unstable-resumable")]
    pub fn start_call(&mut self, f: Value, args: &[Value], poll_slots: usize) -> CallPoll {
        let r = self.gc_start_sliced(f, args, poll_slots);
        if self.gc_is_pending() { CallPoll::Pending } else { CallPoll::Ready(r) }
    }

    /// The pending call's next poll (see [`Lua::start_call`]); an error when none is.
    #[cfg(feature = "unstable-resumable")]
    pub fn resume_call(&mut self) -> CallPoll {
        let r = self.gc_resume_poll();
        if self.gc_is_pending() { CallPoll::Pending } else { CallPoll::Ready(r) }
    }

    /// Drops the pending call (the collector's step it paused in is finished first,
    /// uncharged): the state is usable again, as after a call stopped by a limit.
    #[cfg(feature = "unstable-resumable")]
    pub fn cancel_call(&mut self) {
        self.gc_drop_pending();
    }

    /// Whether a call is pending (see [`Lua::start_call`]).
    #[cfg(feature = "unstable-resumable")]
    pub fn call_pending(&self) -> bool {
        self.gc_is_pending()
    }

    /// Test probe (SEC7-G1, until the host's entry for it is settled): calls `f` with the
    /// collector's polls bounded to `slots` table slots each, resuming the call each time a
    /// marking step pauses at a poll's end, until it returns. Its result, and the polls it
    /// took. A pause is invisible to the script and to the fuel: the result, the fuel and
    /// where a tight limit stops it are those of `call`.
    #[doc(hidden)]
    pub fn gc_sliced_call(&mut self, f: Value, args: &[Value], slots: usize) -> (Result<Vec<Value>, Error>, u32) {
        self.heap.poll_allow = Some(slots.max(1));
        let mut r = self.call(f, args);
        let mut polls = 1;
        while self.pending_call.is_some() || self.pending_start.is_some() {
            r = self.resume_poll();
            polls += 1;
        }
        self.heap.poll_allow = None;
        (r, polls)
    }

    /// Test probe: `gc_sliced_call`'s first poll only; then `gc_resume_poll` for each
    /// next, or `gc_drop_pending`.
    #[doc(hidden)]
    pub fn gc_start_sliced(&mut self, f: Value, args: &[Value], slots: usize) -> Result<Vec<Value>, Error> {
        self.heap.poll_allow = Some(slots.max(1));
        let r = self.call(f, args);
        if !self.gc_is_pending() {
            self.heap.poll_allow = None;
        }
        r
    }

    /// Test probe: the call `gc_start_sliced` left pending is.
    #[doc(hidden)]
    pub fn gc_is_pending(&self) -> bool {
        self.pending_call.is_some() || self.pending_start.is_some()
    }

    /// Test probe: the next poll of the pending call.
    #[doc(hidden)]
    pub fn gc_resume_poll(&mut self) -> Result<Vec<Value>, Error> {
        let r = self.resume_poll();
        if !self.gc_is_pending() {
            self.heap.poll_allow = None;
        }
        r
    }

    /// Test probe: drops the pending call (its step finished first).
    #[doc(hidden)]
    pub fn gc_drop_pending(&mut self) {
        if self.gc_is_pending() && self.entry == 0 {
            vm::Lua::cancel_nested(self);
            self.clear_idle_stack();
        }
        self.heap.poll_allow = None;
    }

    /// The pending call's next poll: an interrupt set meanwhile drops it, as a stop would.
    fn resume_poll(&mut self) -> Result<Vec<Value>, Error> {
        if !self.gc_is_pending() {
            return Err(Error::Host("no call is pending".into()));
        }
        if self.interrupt.load(Ordering::Relaxed) {
            vm::Lua::cancel_nested(self);
            self.clear_idle_stack();
            self.limit_hit = Some(LimitKind::Interrupted);
            return Err(Error::Limit(LimitKind::Interrupted));
        }
        if self.heap.mem.trace.is_some() {
            let p = self.trace_poll_resumed();
            let r = self.resume_now();
            if let Some(p) = p {
                self.trace_poll_end(p);
            }
            return r;
        }
        self.resume_now()
    }

    fn resume_now(&mut self) -> Result<Vec<Value>, Error> {
        if let Some((f, args)) = self.pending_start.take() {
            // Paused in the entry that finishes a suspended atomic step before a call
            // (`check_live`): that entry goes on; then the call, as `call_now` goes on.
            let out = self.enter_as(false, |l| l.resume_atomic_pausable());
            if self.gc_pending.is_some() && self.pending_call.is_none() {
                self.pending_start = Some((f, args));
                return out.map(|()| Vec::new());
            }
            out?;
            return self.call_now(f, &args);
        }
        let out = self.enter_as(false, vm::Lua::resume_nested);
        self.after_host_call(out)
    }

    fn trace_poll_resumed(&mut self) -> Option<(crate::nostd::time::Duration, usize, i64)> {
        let t = self.heap.mem.trace.as_mut()?;
        t.poll += 1;
        Some((t.now(), self.heap.scanned, 0))
    }

    /// Where the collector stands, and the fuel it is known to need (SEC8-F3 O5): for a host
    /// that gives calls a fixed fuel, to tell a call that will be spent in the collector
    /// from one that will run the script (at the memory limit an allocation's emergency
    /// collection can take hundreds of such calls: SEC8-G3, a known limitation). Two
    /// different quantities:
    /// - `owed_fuel`: work already done and not yet charged. The collector's next steps (or
    ///   next collection) charge it before they do more. Exact.
    /// - `remaining_fuel_at_least`: a floor on the fuel the cycle under way still needs
    ///   past that. Not an estimate of the cost: marking finds the objects it must mark as
    ///   it goes, so while it runs (`more_may_be_found`) the rest is not known ahead.
    /// A full collection (`collectgarbage()`, `collect_garbage`, or the one an allocation
    /// makes at the memory limit) finishes the cycle under way and then runs a whole new
    /// one, which neither figure includes. Reading it enters nothing and charges nothing.
    pub fn collector_status(&self) -> CollectorStatus {
        let phase = match self.heap.phase {
            crate::heap::Phase::Pause => CollectorPhase::Idle,
            crate::heap::Phase::Propagate if self.heap.atomic_suspended() => CollectorPhase::AtomicSuspended,
            crate::heap::Phase::Propagate => CollectorPhase::Marking,
            crate::heap::Phase::Sweep => CollectorPhase::Sweeping,
        };
        let owed = self.gc_mark_over + self.atomic_owed;
        CollectorStatus {
            phase,
            owed_fuel: (owed / vm::WORK_PER_FUEL) as u64,
            remaining_fuel_at_least: (self.heap.sweep_floor() / vm::WORK_PER_FUEL) as u64,
            more_may_be_found: phase != CollectorPhase::Idle,
        }
    }

    /// Test probe (SEC8-F7): the collector's work units owed past a call's fuel since the
    /// state was made (charged by its next steps or collection, never dropped), those of
    /// them a cycle's start charged, and those owed now.
    #[doc(hidden)]
    pub fn gc_owed_probe(&self) -> (u64, u64, u64) {
        (self.gc_owed_probe.0, self.gc_owed_probe.1, (self.gc_mark_over + self.atomic_owed) as u64)
    }

    /// Test probe: the slots grown in the cycle that marking scanned (paid as the table
    /// grew), the marking steps' budgets in work units, since the state was made, and the
    /// units a grown slot spends of a budget (SEC7-G1: the grown slots times that never pass
    /// the budgets by more than a chunk's over any run of calls).
    #[doc(hidden)]
    pub fn gc_pace_probe(&self) -> (u64, u64, u64) {
        (self.heap.probe_prepaid, self.heap.probe_budget, crate::heap::GROWN_SLOT_WORK as u64)
    }

    /// Test probe: traces the collector's work (SEC7-G1) on `clock` in a buffer of
    /// `capacity` records taken now (None: no trace). One record a call from the host
    /// ([`TraceOp::Poll`]), one a collector step, and one an operation traced inside one;
    /// records past the buffer's end are counted, not kept. Changes no fuel.
    #[doc(hidden)]
    pub fn gc_trace(&mut self, clock: Option<Clock>, capacity: usize) {
        self.heap.mem.trace = clock.map(|c| Box::new(gctrace::Trace::new(c, capacity)));
    }

    /// Test probe: the trace's records so far, and how many did not fit (both reset).
    #[doc(hidden)]
    pub fn gc_trace_take(&mut self) -> (Vec<TraceRecord>, u64) {
        self.heap.mem.trace.as_mut().map(|t| t.take()).unwrap_or_default()
    }

    fn trace_poll(&mut self) -> Option<(crate::nostd::time::Duration, usize, i64)> {
        let t = self.heap.mem.trace.as_mut()?;
        t.call += 1;
        t.poll += 1;
        Some((t.now(), self.heap.scanned, 0))
    }

    #[cold]
    fn trace_poll_end(&mut self, (start, scanned, _): (crate::nostd::time::Duration, usize, i64)) {
        let slots = self.heap.scanned.wrapping_sub(scanned) as u64;
        let used = self.limits.fuel.saturating_sub(self.fuel_left());
        if let Some(t) = self.heap.mem.trace.as_mut() {
            t.note(TraceOp::Poll, 0, slots, 0, used, start);
        }
    }

    /// Runs `f` with `vals` kept alive (the operands of a host call while it allocates).
    fn with_held<T>(&mut self, vals: &[Value], f: impl FnOnce(&mut Lua) -> Result<T, LuaError>) -> Result<T, LuaError> {
        let mark = self.scratch.len();
        self.scratch.extend_from_slice(vals);
        let r = f(self);
        self.scratch.truncate(mark);
        r
    }

    /// Keeps a value the host was handed alive: until the running host function returns
    /// (inside one), else until the next call from the host returns.
    fn keep(&mut self, v: Value) -> Value {
        if v.is_collectable() {
            if self.native_depth > 0 {
                self.push_scratch(v);
            } else if self.entry == 0 {
                self.push_held(v);
            }
        }
        v
    }

    /// Calls the global function `name` (or a dotted path, `table.field`, read raw), if
    /// there is one: `Ok(None)` when it is missing or not a function.
    pub fn call_global(&mut self, name: &str, args: &[Value]) -> Result<Option<Vec<Value>>, Error> {
        let f = self.get_path(name)?;
        if !matches!(f, Value::Func(_)) {
            return Ok(None);
        }
        self.call(f, args).map(Some)
    }

    /// Read as `get_global` reads (SEC8-F3 O2): each name looked up, not made.
    fn get_path(&mut self, name: &str) -> Result<Value, Error> {
        self.check_live_now(Value::Nil)?;
        let mut cur = Value::Table(self.globals);
        for part in name.split('.') {
            let Value::Table(t) = cur else { return Ok(Value::Nil) };
            let Some(k) = self.heap.find_interned_quiet(part.as_bytes()) else { return Ok(Value::Nil) };
            cur = crate::table::host_side(|| self.raw_get(t, Value::Str(k)));
        }
        Ok(cur)
    }

    /// The globals table.
    pub fn globals(&self) -> TableRef {
        self.globals
    }

    /// A global, read raw (no metamethods run). A read: it does not enter the state, so it
    /// neither finishes an atomic phase a call before stopped in nor pays for one (made
    /// with `create_string`, the name entered, and while that phase did not fit in the
    /// call's fuel the read failed with `Limit(Fuel)`: SEC8-F3). The name is looked up, not
    /// made: table keys are interned, so a name never made is no global's. (A value read
    /// here and stored from the host is checked then, the phase finished first: SEC7-H2.)
    pub fn get_global(&mut self, name: &str) -> Result<Value, Error> {
        self.check_live_now(Value::Nil)?;
        let Some(k) = self.heap.find_interned_quiet(name.as_bytes()) else { return Ok(Value::Nil) };
        Ok(crate::table::host_side(|| self.raw_get(self.globals, Value::Str(k))))
    }

    /// Sets a global, raw.
    pub fn set_global(&mut self, name: &str, v: Value) -> Result<(), Error> {
        self.check_live(v)?;
        let k = self.create_string(name.as_bytes())?;
        // Making the name may have needed the memory `v` held.
        self.check_live(v)?;
        let g = self.globals;
        self.enter(|l| l.with_held(&[k, v], |l| l.raw_set(g, k, v)))
    }

    /// `t[k]`, raw (nil for a handle whose table was collected).
    pub fn table_get(&self, t: TableRef, k: Value) -> Value {
        if !self.heap.is_live(Value::Table(t)) || !self.heap.is_live(k) {
            return Value::Nil;
        }
        crate::table::host_side(|| self.raw_get(t, k))
    }

    /// `t[k] = v`, raw.
    pub fn table_set(&mut self, t: TableRef, k: Value, v: Value) -> Result<(), Error> {
        self.check_live(Value::Table(t))?;
        self.check_live(k)?;
        self.check_live(v)?;
        self.enter(|l| l.with_held(&[k, v], |l| l.raw_set(t, k, v)))
    }

    /// `t[k]` with metamethods (which run under the call's fuel).
    pub fn get(&mut self, t: Value, k: Value) -> Result<Value, Error> {
        self.check_live(t)?;
        self.check_live(k)?;
        let v = self.enter(|l| l.with_held(&[t, k], |l| l.index_nested(t, k)))?;
        Ok(self.keep(v))
    }

    /// `t[k] = v` with metamethods.
    pub fn set(&mut self, t: Value, k: Value, v: Value) -> Result<(), Error> {
        self.check_live(t)?;
        self.check_live(k)?;
        self.check_live(v)?;
        self.enter(|l| l.with_held(&[t, k, v], |l| l.set_index_nested(t, k, v)))
    }

    /// `#t`, raw (0 for a handle whose table was collected).
    pub fn table_len(&self, t: TableRef) -> i64 {
        if !self.heap.is_live(Value::Table(t)) {
            return 0;
        }
        crate::table::host_side(|| self.raw_len(t))
    }

    /// Every entry of `t`, raw, in traversal order (none for a stale handle).
    pub fn table_pairs(&self, t: TableRef) -> Vec<(Value, Value)> {
        if !self.heap.is_live(Value::Table(t)) {
            return Vec::new();
        }
        crate::table::host_side(|| self.heap.tables.get(t.idx).map(|t| t.entries().collect()).unwrap_or_default())
    }

    /// A new empty table.
    pub fn create_table(&mut self) -> Result<TableRef, Error> {
        let t = self.enter(|l| l.new_table(0, 0))?;
        self.keep(Value::Table(t));
        Ok(t)
    }

    /// A string.
    pub fn create_string(&mut self, s: impl AsRef<[u8]>) -> Result<Value, Error> {
        let b = s.as_ref();
        let v = self.enter(|l| l.new_str(b))?;
        Ok(self.keep(v))
    }

    /// The bytes of a string value.
    pub fn str_bytes(&self, v: Value) -> Option<&[u8]> {
        match v {
            Value::Str(s) if self.heap.is_live(v) => Some(self.bytes(s)),
            _ => None,
        }
    }

    /// Raw equality as Lua's `rawequal` sees it: strings by content (two equal long
    /// strings made apart are different objects, so `Value`'s `==` tells them apart),
    /// `1 == 1.0`, other objects by identity. No metamethods run.
    pub fn raw_equal(&self, a: &Value, b: &Value) -> bool {
        match (*a, *b) {
            (Value::Str(x), Value::Str(y)) if self.heap.is_live(*a) && self.heap.is_live(*b) => {
                self.heap.str_eq_quick(x, y).unwrap_or_else(|| self.bytes(x) == self.bytes(y))
            }
            (x, y) => x.raw_eq(y),
        }
    }

    /// A string value as text (invalid UTF-8 replaced).
    pub fn to_string_lossy(&self, v: Value) -> Option<String> {
        self.str_bytes(v).map(|b| String::from_utf8_lossy(b).into_owned())
    }

    /// Sets a table's metatable.
    pub fn set_metatable(&mut self, t: TableRef, meta: Option<TableRef>) -> Result<(), Error> {
        self.check_live(Value::Table(t))?;
        if let Some(m) = meta {
            self.check_live(Value::Table(m))?;
            if !self.tm_in(m, vm::Tm::Gc).is_nil() {
                return Err(Error::Host("__gc metamethods are not supported".into()));
            }
        }
        if let Some(tab) = self.heap.tables.get_mut(t.idx) {
            tab.meta = meta;
        }
        self.heap.barrier_table(t);
        Ok(())
    }

    /// Keeps `v` alive while the returned handle lives.
    pub fn root(&mut self, v: Value) -> Root {
        let alive = Arc::new(());
        // Dropped roots are forgotten here too (not only during a collection), whenever
        // the list has doubled since it was last pruned: amortized O(1) per root.
        if self.roots.len() >= self.roots_pruned_at.saturating_mul(2).max(64) {
            self.prune_roots();
            self.roots_pruned_at = self.roots.len();
        }
        // Counted (LR-17), whatever the limit says: making a handle cannot fail. Given back
        // when its dropped entry is pruned.
        self.heap.mem.used = self.heap.mem.used.saturating_add(heap::ROOT_BYTES);
        // S5API-8: a stale handle (or another state's) must not keep alive whatever object
        // now has its slot here; the root keeps nothing then.
        let kept = if self.heap.is_live(v) { v } else { Value::Nil };
        self.roots.push((Arc::downgrade(&alive), kept));
        Root { value: v, _alive: alive }
    }

    /// A Rust function scripts can call. It gets the arguments and returns the results,
    /// or an error the script sees as an error (catchable by `pcall` unless it is an
    /// [`Error::Limit`]; an [`Error::Runtime`] that a `lua.call` inside it returned passes
    /// the script's own error value on). It may call back into the state (`lua.call`), and
    /// a script may reach it again while it runs, so it is `Fn + Sync`: keep mutable state
    /// behind a `Mutex` or in [`Lua::app_data_mut`], or use [`Lua::create_function_mut`],
    /// which takes an `FnMut` that need not be `Sync` and refuses to run inside itself. When it returns, the host's interrupt and the
    /// call's deadline are checked, so time spent waiting in it counts. A panic in it
    /// unwinds to the host and poisons the state; catch panics inside it to contain them.
    pub fn create_function(
        &mut self,
        name: &str,
        f: impl Fn(&mut Lua, &[Value]) -> Result<Vec<Value>, Error> + Send + Sync + 'static,
    ) -> Result<Value, Error> {
        let host: heap::HostFn = Arc::new(move |lua: &mut Lua, a: Args| -> Result<Flow, LuaError> {
            let args: Vec<Value> = (0..a.n).map(|i| lua.st(a.base + i)).collect();
            // Its calls' results go above those of the host functions further out.
            let rets_base = crate::nostd::mem::replace(&mut lua.host_rets_base, lua.host_rets.len());
            // The uncharged probe steps and error-naming work (per thread) are the calling
            // script's: set aside while the host function runs, so nothing it does to any
            // state (a compile in another, which does not enter it) is billed here, and put
            // back after (the residual of S6-RES-4).
            let aside = (crate::table::take_probes(), crate::debug::take_work());
            let out = f(lua, &args);
            crate::table::put_probes(aside.0);
            crate::debug::put_work(aside.1);
            lua.host_rets.truncate(lua.host_rets_base);
            lua.host_rets_base = rets_base;
            // Time spent in the host function counts: one that waits (and spends no fuel)
            // must not keep a script past its interrupt or deadline.
            if lua.interrupt.load(Ordering::Relaxed) {
                return Err(LuaError::Limit(LimitKind::Interrupted));
            }
            if let (Some(at), Some((clock, _))) = (lua.deadline, &lua.time_limit)
                && clock() >= at
            {
                return Err(LuaError::Limit(LimitKind::Time));
            }
            let out = match out {
                Ok(out) => out,
                Err(e) => return Err(lua.from_host_error(e)),
            };
            // A value whose object was collected must not reach the script.
            if out.iter().any(|v| !lua.heap.is_live(*v)) {
                return Err(LuaError::msg("a host function returned a value whose object was collected"));
            }
            lua.rets.clear();
            lua.rets.extend(out);
            Ok(Flow::Return)
        });
        // LR-17: its closure's `Arc` (counts, the captured state rounded to 16, the header)
        // and its name's (likewise), not a flat 32.
        let round = |n: usize| n.div_ceil(16) * 16;
        let bytes = round(16 + crate::nostd::mem::size_of_val(&*host)) + heap::ALLOC_OVERHEAD + round(16 + name.len()) + heap::ALLOC_OVERHEAD;
        let name: Arc<str> = Arc::from(name);
        let v = self.enter(|l| {
            l.charge_mem(heap::FUNC_BASE + bytes)?;
            let r = l.heap.new_func_charged(heap::Func::Host { f: host, name, bytes });
            Ok(Value::Func(r))
        })?;
        Ok(self.keep(v))
    }

    /// Makes a Rust function a global (`name`) or a field of a global table (`lib.name`,
    /// the table made if missing).
    pub fn register(
        &mut self,
        name: &str,
        f: impl Fn(&mut Lua, &[Value]) -> Result<Vec<Value>, Error> + Send + Sync + 'static,
    ) -> Result<(), Error> {
        let short = name.rsplit('.').next().unwrap_or(name).to_owned();
        let func = self.create_function(&short, f)?;
        self.register_value(name, func)
    }

    /// [`Lua::register`] for a [`Lua::create_function_mut`] function.
    pub fn register_mut(
        &mut self,
        name: &str,
        f: impl FnMut(&mut Lua, &[Value]) -> Result<Vec<Value>, Error> + Send + 'static,
    ) -> Result<(), Error> {
        let short = name.rsplit('.').next().unwrap_or(name).to_owned();
        let func = self.create_function_mut(&short, f)?;
        self.register_value(name, func)
    }

    /// A Rust function scripts can call, like [`Lua::create_function`], that may keep
    /// mutable state and need not be `Sync`. Because it is `FnMut`, it cannot run inside
    /// itself: when a script reaches it again while it runs (through a callback it calls),
    /// that inner call is an ordinary script error, "a host function was called again while
    /// it ran". `create_function` needs `Sync` because it may run inside itself, and the
    /// state, which is `Send`, shares it between those runs; `Mutex` makes this one `Sync`
    /// without that.
    pub fn create_function_mut(
        &mut self,
        name: &str,
        f: impl FnMut(&mut Lua, &[Value]) -> Result<Vec<Value>, Error> + Send + 'static,
    ) -> Result<Value, Error> {
        let f = crate::nostd::sync::Mutex::new(f);
        self.create_function(name, move |lua, args| match f.try_lock() {
            Ok(mut f) => f(lua, args),
            Err(crate::nostd::sync::TryLockError::WouldBlock) => Err(Error::runtime("a host function was called again while it ran")),
            // A panic in it poisoned the state too: nothing runs here again.
            Err(crate::nostd::sync::TryLockError::Poisoned(_)) => Err(Error::Host("a panic in an earlier call left this state unusable".into())),
        })
    }

    fn register_value(&mut self, name: &str, func: Value) -> Result<(), Error> {
        let func = self.root(func);
        let mut parts: Vec<&str> = name.split('.').collect();
        let last = parts.pop().unwrap_or(name);
        let mut t = self.globals;
        for p in parts {
            let k = self.create_string(p.as_bytes())?;
            t = match self.raw_get(t, k) {
                Value::Table(inner) => inner,
                _ => {
                    let inner = self.create_table()?;
                    self.table_set(t, k, Value::Table(inner))?;
                    inner
                }
            };
        }
        let k = self.create_string(last.as_bytes())?;
        self.table_set(t, k, func.value())
    }

    /// A full userdata holding `data`, with an optional metatable (for methods: set its
    /// `__index`; a `__gc` field never runs). It counts `size_of::<T>()` plus a small
    /// header against the memory limit; declare what `T` owns on the heap (a `Vec`'s
    /// buffer) with [`Lua::set_userdata_size`].
    pub fn create_userdata<T: Any + Send>(&mut self, data: T, meta: Option<TableRef>) -> Result<Value, Error> {
        self.new_userdata_value(Box::new(data), crate::nostd::mem::size_of::<T>(), meta, None)
    }

    /// A userdata of `size` bytes, tracked under `notify`. A tracked one also charges the
    /// slot its key takes in the collected queue (S5API-4), so a script that makes and drops
    /// tracked userdata in a loop is stopped by the memory limit, not the host's memory.
    /// `take_collected` gives the slot back.
    fn new_userdata_value(
        &mut self,
        data: Box<dyn Any + Send>,
        size: usize,
        meta: Option<TableRef>,
        notify: Option<u64>,
    ) -> Result<Value, Error> {
        if let Some(m) = meta {
            self.check_live(Value::Table(m))?;
        }
        let held = meta.map_or(Value::Nil, Value::Table);
        let queued = if notify.is_some() { QUEUE_ENTRY } else { 0 };
        let v = self.enter(|l| {
            l.with_held(&[held], |l| {
                l.charge_mem(heap::UD_BASE + size + queued)?;
                l.heap.mem.release(heap::UD_BASE + size);
                let r = match l.alloc_ud(data, size, meta) {
                    Ok(r) => r,
                    Err(e) => {
                        l.heap.mem.release(queued);
                        return Err(e);
                    }
                };
                if let Some(ud) = l.heap.uds.get_mut(r.idx) {
                    ud.notify = notify;
                }
                Ok(Value::UserData(r))
            })
        })?;
        Ok(self.keep(v))
    }

    /// Sets what the userdata `v` counts against the memory limit, besides a small header:
    /// `bytes`, in place of `size_of::<T>()`. A host whose userdata owns heap memory (a
    /// buffer a script asked for) declares it here, `size_of::<T>() + buffer.capacity()`,
    /// when it grows or shrinks; the difference is charged now, so a script cannot make
    /// the host allocate past its budget. Over the limit (or past `isize::MAX`, the largest
    /// allocation there is): [`Error::Limit`]`(Memory)` and nothing changes (inside a host
    /// function, return the error, so the call stops).
    pub fn set_userdata_size(&mut self, v: Value, bytes: usize) -> Result<(), Error> {
        let Value::UserData(u) = v else { return Err(Error::Host("set_userdata_size takes a userdata".into())) };
        self.check_live(v)?;
        self.enter(|l| {
            // S5API-5: no allocation is this big, and with an unlimited memory cap a size
            // near `usize::MAX` was accepted and overflowed the header added at collection.
            if bytes > MAX_UD_BYTES {
                return Err(LuaError::Limit(LimitKind::Memory));
            }
            l.with_held(&[v], |l| {
                let old = l.heap.uds.get(u.idx).map_or(bytes, |d| d.size);
                if bytes > old {
                    l.charge_mem(bytes - old)?;
                } else {
                    l.heap.mem.release(old - bytes);
                }
                if let Some(d) = l.heap.uds.get_mut(u.idx) {
                    d.size = bytes;
                }
                Ok(())
            })
        })
    }

    /// A userdata like [`Lua::create_userdata`] whose collection the host hears about: once
    /// no script or root can reach it and the collector frees it, `key` is queued for
    /// [`Lua::take_collected`]. For host objects that must be released when scripts drop
    /// their handles; no Lua code runs at collection (there is no `__gc`). It counts 8 bytes
    /// more than an untracked one: the room its key takes in that queue.
    pub fn create_userdata_tracked<T: Any + Send>(&mut self, data: T, meta: Option<TableRef>, key: u64) -> Result<Value, Error> {
        self.new_userdata_value(Box::new(data), crate::nostd::mem::size_of::<T>(), meta, Some(key))
    }

    /// The data of a userdata, if it holds a `T`.
    pub fn userdata<T: Any>(&self, v: Value) -> Option<&T> {
        let Value::UserData(u) = v else { return None };
        if !self.heap.is_live(v) {
            return None;
        }
        self.heap.uds.get(u.idx)?.data.as_ref()?.downcast_ref::<T>()
    }

    /// The data of a userdata, mutably, if it holds a `T`.
    pub fn userdata_mut<T: Any>(&mut self, v: Value) -> Option<&mut T> {
        let Value::UserData(u) = v else { return None };
        if !self.heap.is_live(v) {
            return None;
        }
        self.heap.uds.get_mut(u.idx)?.data.as_mut()?.downcast_mut::<T>()
    }

    /// Copies a value out as owned [`Data`], raw (no metamethod runs: `__pairs` and
    /// `__index` are ignored), within `limits`. A function, userdata or thread, or a table
    /// nested deeper than `limits.depth` (a table that holds itself, say), is an
    /// [`Error::Host`].
    pub fn to_data(&self, v: Value, limits: DataLimits) -> Result<Data, Error> {
        self.check_live_now(v)?;
        let mut budget = (limits.nodes, limits.bytes);
        // Each level of nesting is a native call: capped, so a host asking for any depth
        // cannot overflow its stack on a script's deeply nested table.
        crate::table::host_side(|| self.to_data_inner(v, 0, limits.depth.min(MAX_DATA_DEPTH), &mut budget))
    }

    fn to_data_inner(&self, v: Value, depth: usize, max: usize, budget: &mut (usize, usize)) -> Result<Data, Error> {
        if depth > max {
            return Err(Error::Host("table nested too deeply".into()));
        }
        if budget.0 == 0 {
            return Err(Error::Host("too much data".into()));
        }
        budget.0 -= 1;
        Ok(match v {
            Value::Nil => Data::Nil,
            Value::Bool(b) => Data::Bool(b),
            Value::Int(i) => Data::Int(i),
            Value::Num(f) => Data::Num(f),
            Value::Str(s) => {
                let b = self.bytes(s);
                if b.len() > budget.1 {
                    return Err(Error::Host("too much data".into()));
                }
                budget.1 -= b.len();
                Data::Str(String::from_utf8_lossy(b).into_owned())
            }
            Value::Table(t) => {
                let mut out = Vec::new();
                // Read in place: copying every entry first allocated the whole table natively
                // (uncharged) before the node budget could stop it.
                let Some(tab) = self.heap.tables.get(t.idx).filter(|_| self.heap.is_live(Value::Table(t))) else {
                    return Ok(Data::Table(out));
                };
                for (k, v) in tab.entries() {
                    let k = self.to_data_inner(k, depth + 1, max, budget)?;
                    let v = self.to_data_inner(v, depth + 1, max, budget)?;
                    out.push((k, v));
                }
                Data::Table(out)
            }
            other => return Err(Error::Host(format!("a {} cannot be copied out", other.type_name()))),
        })
    }

    /// Makes a value from owned [`Data`] (nil and NaN keys are skipped), nested at most 64
    /// deep; its size is bounded by the memory limit only. [`Lua::from_data_with`] bounds it.
    pub fn from_data(&mut self, d: &Data) -> Result<Value, Error> {
        self.from_data_with(d, DataLimits { depth: MAX_FROM_DATA_DEPTH, nodes: usize::MAX, bytes: usize::MAX })
    }

    /// [`Lua::from_data`] within `limits`, checked before anything is made: deeper (than
    /// `limits.depth`, and never more than 64), more values or more string bytes is an
    /// [`Error::Host`], as for [`Lua::to_data`].
    pub fn from_data_with(&mut self, d: &Data, limits: DataLimits) -> Result<Value, Error> {
        fn fits(d: &Data, depth: usize, max: usize, budget: &mut (usize, usize)) -> Result<(), Error> {
            if depth > max {
                return Err(Error::Host("data nested too deeply".into()));
            }
            if budget.0 == 0 {
                return Err(Error::Host("too much data".into()));
            }
            budget.0 -= 1;
            match d {
                Data::Str(s) => {
                    budget.1 = budget.1.checked_sub(s.len()).ok_or_else(|| Error::Host("too much data".into()))?;
                }
                Data::Table(pairs) => {
                    for (k, v) in pairs {
                        fits(k, depth + 1, max, budget)?;
                        fits(v, depth + 1, max, budget)?;
                    }
                }
                _ => {}
            }
            Ok(())
        }
        // S5API-6: never deeper than `from_data_inner` goes (64), so data that passes is made
        // (it failed half-made, as a runtime error), and `fits` recurses no deeper.
        fits(d, 0, limits.depth.min(MAX_FROM_DATA_DEPTH), &mut (limits.nodes, limits.bytes))?;
        self.enter(|l| {
            let base = l.scratch.len();
            l.native_depth += 1;
            let r = l.from_data_inner(d, 0);
            l.native_depth -= 1;
            let r = r.map(|v| {
                l.scratch.truncate(base);
                v
            });
            l.scratch.truncate(base);
            r
        })
        .map(|v| self.keep(v))
    }

    fn from_data_inner(&mut self, d: &Data, depth: usize) -> Result<Value, LuaError> {
        if depth > MAX_FROM_DATA_DEPTH {
            return Err(LuaError::msg("data nested too deeply"));
        }
        Ok(match d {
            Data::Nil => Value::Nil,
            Data::Bool(b) => Value::Bool(*b),
            Data::Int(i) => Value::Int(*i),
            Data::Num(f) => Value::Num(*f),
            Data::Str(s) => self.new_str(s.as_bytes())?,
            Data::Table(pairs) => {
                let t = self.new_table(0, pairs.len().min(1 << 16))?;
                for (k, v) in pairs {
                    if matches!(k, Data::Nil) || matches!(k, Data::Num(f) if f.is_nan()) {
                        continue;
                    }
                    let kv = self.from_data_inner(k, depth + 1)?;
                    let vv = self.from_data_inner(v, depth + 1)?;
                    self.raw_set(t, kv, vv)?;
                }
                Value::Table(t)
            }
        })
    }

    /// Turns `warn` output on or off, as a script's `warn("@on")` / `warn("@off")` does.
    /// Warnings start off, as in C Lua's `luaL_newstate`; they go where `print` goes, and
    /// only with the `PRINT` library allowed. A script can still switch them afterwards.
    pub fn set_warnings(&mut self, on: bool) {
        self.warn_on = on;
    }

    /// Test switch: with `false`, every instruction runs through the full interpreter
    /// path instead of the fast loop (results, fuel and memory must not change). Also off
    /// for every state when the environment has `REMEDYLUA_TEST_NO_FAST_LOOP`.
    #[doc(hidden)]
    pub fn set_fast_loop(&mut self, on: bool) {
        self.fast = on;
    }

    /// Test probe: library functions called so far through the interpreter's general native
    /// call (a call the fast loop runs in place does not count).
    #[doc(hidden)]
    pub fn native_calls(&self) -> u64 {
        self.native_calls
    }

    /// Test mode: instructions the full path has run while the fast loop was on (the ones
    /// the fast loop left to it), so a test can check which operations stay in the loop.
    #[doc(hidden)]
    pub fn full_steps(&self) -> u64 {
        self.full_steps
    }

    /// Test mode: a whole collection at every allocation, to shake out values the
    /// interpreter holds without rooting them. Very slow.
    #[doc(hidden)]
    pub fn set_gc_stress(&mut self, on: bool) {
        self.heap.mem.stress = on;
    }

    /// Test probe: the most slots of one table the collector scanned in one go (a whole
    /// table, or one chunk of a big one: at most 4,096 while marking is incremental,
    /// S6GC-2e-R) since this was last called.
    #[doc(hidden)]
    pub fn gc_most_slots_at_once(&mut self) -> usize {
        crate::nostd::mem::take(&mut self.heap.most_at_once)
    }

    /// Test probe: the array slots copied into new buffers by growths on this thread, and the
    /// array parts made segmented, since the last read.
    #[doc(hidden)]
    pub fn table_slots_copied(&mut self) -> (u64, u64) {
        crate::table::take_copied()
    }

    /// Test probe: the slots growths read under a fuel watch (made a piece at a time within
    /// the call's fuel, SEC8-F4) on this thread since the last read, a piece at a time.
    #[doc(hidden)]
    pub fn table_slots_watched(&mut self) -> u64 {
        crate::table::take_watched()
    }

    /// Test probe: the slots of new array tails a resize made on this thread since the last
    /// read (a resize given up and gone on with makes each once).
    #[doc(hidden)]
    pub fn table_slots_staged(&mut self) -> u64 {
        crate::table::take_staged()
    }

    /// Test probe (segmented storage): the objects slabs' storage moved on this thread since
    /// the last read (none once past a slab's first chunk).
    #[cfg(any(target_family = "wasm", feature = "segmented-arrays"))]
    #[doc(hidden)]
    pub fn slab_objects_moved(&mut self) -> u64 {
        crate::heap::take_moved()
    }

    /// Test probe: the most table slots the atomic phase scanned between two looks at the
    /// interrupt and the deadline since the last read.
    #[doc(hidden)]
    pub fn gc_most_slots_between_looks(&mut self) -> usize {
        crate::nostd::mem::take(&mut self.heap.most_between_looks)
    }

    /// Test probe: sizes of the state's bookkeeping that `memory_used` does not count
    /// (so the soak tests can see it return to its baseline).
    #[doc(hidden)]
    pub fn internal_sizes(&self) -> Vec<(&'static str, usize)> {
        let h = &self.heap;
        let mut v = vec![
            ("interned", h.interner.count),
            ("interner_slots", h.interner.slots.len()),
            ("interner_moving", h.interner.old.len()),
            ("roots", self.roots.len()),
            ("scratch", self.scratch.len()),
            ("held", self.held.len()),
            ("rets", self.rets.len()),
            ("host_rets", self.host_rets.len()),
            ("collected", h.collected.len()),
            ("gc_scratch_bytes", h.scratch_bytes()),
            ("app_data", self.app_data.len()),
            ("stack", self.cur.stack.len()),
        ];
        // Each slab's objects, slots, and free slots below its last object.
        fn counts<T>(s: &heap::Slab<T>) -> [usize; 3] {
            [s.slots() - s.free.len(), s.slot_capacity(), s.free.len()]
        }
        let slabs = [
            ("strings", counts(&h.strings)),
            ("tables", counts(&h.tables)),
            ("funcs", counts(&h.funcs)),
            ("upvals", counts(&h.upvals)),
            ("uds", counts(&h.uds)),
            ("threads", counts(&h.threads)),
            ("protos", counts(&h.protos)),
        ];
        v.extend(slabs.iter().map(|&(n, [live, _, _])| (n, live)));
        v.push(("slab_slots", slabs.iter().map(|&(_, [_, cap, _])| cap).sum()));
        // SEC-M5: free slots below the last object of their slab, and their bytes
        // (counted in `memory_used`).
        v.push(("free_slots", slabs.iter().map(|&(_, [_, _, free])| free).sum()));
        v.push(("slack_bytes", h.mem.slack));
        v
    }

    /// Bytes the state's objects hold now, free slots included (they are resident: a
    /// slab keeps the slots below its last object).
    pub fn memory_used(&self) -> usize {
        // (Without the table buffer pool: it counts against the limit, but what it holds
        // depends on when it was last given back, which the two paths may not share.)
        self.heap.mem.used - self.heap.mem.pool.bytes
    }

    /// The limits.
    pub fn limits(&self) -> Limits {
        self.limits
    }

    /// Changes the limits (the memory cap applies to what is allocated from now on). The
    /// dialect and seed are the state's for life: `strict_5_4` and `hash_seed` keep the
    /// values the state was made with (the math table and `__le` must agree, and the
    /// hashing has already used the seed), whatever `limits` says.
    pub fn set_limits(&mut self, limits: Limits) {
        let (strict, seed) = (self.limits.strict_5_4, self.limits.hash_seed);
        self.limits = Limits { strict_5_4: strict, hash_seed: seed, ..limits };
        self.limits.stack_bytes = self.limits.stack_bytes.min(test_stack_cap());
        self.heap.mem.cap = limits.memory;
        // The fast loop's context copies limits (`Lua::ctx_epoch`).
        self.ctx_epoch += 1;
    }

    /// Fuel left in the current (or last) call.
    pub fn fuel_left(&self) -> u64 {
        if let Some(n) = self.stopped_left {
            return n;
        }
        // (A charge past the slice is owed by the reserve even when the call stopped before
        // `refuel` took it from there: LR-29d, a resize finished in a sliced call was
        // reported as the slice alone, 16,384 units of its 3,670,016.)
        u64::try_from(self.fuel.saturating_add(self.reserve.max(0)).max(0)).unwrap_or(0)
    }

    /// The state's interrupt: a handle to keep (on any thread) and set to stop the running
    /// call. Every handle from the same state is the same interrupt.
    pub fn interrupt_handle(&self) -> Interrupt {
        Interrupt(Arc::clone(&self.interrupt))
    }

    /// Stops each call from the host once it has run `limit` by `clock` (looked at every few
    /// thousand steps, so the stop comes within a fraction of a millisecond of work past it):
    /// [`Error::Limit`]`(`[`LimitKind::Time`]`)`. `None` removes the time limit.
    pub fn set_time_limit(&mut self, limit: Option<(Clock, crate::nostd::time::Duration)>) {
        self.time_limit = limit;
    }

    /// Keeps `data` with the state: the host's own data for its functions to reach
    /// ([`Lua::app_data_mut`]), one value per type, replacing the last of that type. `Send`
    /// because the state is `Send` (it may move to another thread with what it holds).
    pub fn set_app_data<T: Any + Send>(&mut self, data: T) {
        self.app_data.insert(crate::nostd::any::TypeId::of::<T>(), Box::new(data));
    }

    /// The host's data of type `T`, if set.
    pub fn app_data<T: Any>(&self) -> Option<&T> {
        self.app_data.get(&crate::nostd::any::TypeId::of::<T>())?.downcast_ref::<T>()
    }

    /// The host's data of type `T`, mutably, if set.
    pub fn app_data_mut<T: Any>(&mut self) -> Option<&mut T> {
        self.app_data.get_mut(&crate::nostd::any::TypeId::of::<T>())?.downcast_mut::<T>()
    }

    /// Takes the host's data of type `T` out of the state.
    pub fn take_app_data<T: Any>(&mut self) -> Option<T> {
        let b = self.app_data.remove(&crate::nostd::any::TypeId::of::<T>())?;
        b.downcast::<T>().ok().map(|b| *b)
    }

    /// The keys of tracked userdata ([`Lua::create_userdata_tracked`]) the collector has
    /// freed since the last take, oldest first. Nothing runs in Lua when one is collected:
    /// the host drains this queue when it chooses (after a call, or a collection). Each key
    /// waiting here counts 8 bytes against the memory limit (charged when its userdata was
    /// made) until it is taken, so a host that tracks userdata drains it after each call.
    pub fn take_collected(&mut self) -> Vec<u64> {
        let keys = crate::nostd::mem::take(&mut self.heap.collected);
        // Each key's slot was charged when its userdata was made (S5API-4).
        self.heap.mem.release(keys.len().saturating_mul(QUEUE_ENTRY));
        keys
    }

    /// Runs a complete collection now. From the host (not inside a host function) it also
    /// lets go of the values held for the host since its last call (see
    /// [the crate docs](crate)): root what must survive it.
    pub fn collect_garbage(&mut self) -> Result<(), Error> {
        if self.entry == 0 {
            self.held.clear();
        }
        self.enter(|l| l.full_gc())
    }

    /// Takes what `print` (and `warn`) wrote since the last take; empty when a sink is set.
    /// Each call from the host may write at most [`Limits::output`] bytes; more is dropped.
    pub fn take_output(&mut self) -> String {
        // The buffer leaves the state's memory with the host.
        self.heap.mem.release(crate::nostd::mem::take(&mut self.output_charged));
        String::from_utf8_lossy(&crate::nostd::mem::take(&mut self.output)).into_owned()
    }

    /// Sends `print` (and `warn`) output to `sink` instead of the buffer, within
    /// [`Limits::output`] per call. The sink runs inside the call: a panic in it poisons
    /// the state.
    pub fn set_print(&mut self, sink: impl FnMut(&str) + Send + 'static) {
        self.print_sink = Some(Box::new(sink));
    }

    /// Drops the sink [`Lua::set_print`] set: output goes to the buffer again
    /// ([`Lua::take_output`]).
    pub fn clear_print(&mut self) {
        self.print_sink = None;
    }

    /// Sets the clocks of the `os` library: `wall`, the time since the Unix epoch, for
    /// `os.time` and `os.date`; `cpu`, the time the program has run, for `os.clock`. By
    /// default they are the system's (the process clock from when the state was made), and
    /// in a browser (`wasm32-unknown-unknown`, which has no clock in `std`) there are none.
    /// `None` makes that clock read as 0, which also makes a replay independent of the time.
    pub fn set_os_clocks(&mut self, wall: Option<Clock>, cpu: Option<Clock>) {
        self.os_clocks = (wall, cpu);
    }

    /// Sets what `os.getenv(name)` returns (`Libs::ENV`): `Some(value)` adds or replaces
    /// `name`, `None` removes it. The map starts empty, and nothing else reaches it: the
    /// process environment is never read. Neither copy counts against the memory limit; the
    /// string `os.getenv` makes does.
    pub fn set_env(&mut self, name: impl AsRef<[u8]>, value: Option<&[u8]>) {
        let name = name.as_ref().to_vec();
        match value {
            Some(v) => {
                self.env_name_max = self.env_name_max.max(name.len());
                self.env.insert(name, v.into());
            }
            None => {
                self.env.remove(&name);
            }
        }
    }

    /// Sets `io.stdin` (`Libs::IO`): what `io.read` and `io.lines()` read by default. `None`
    /// is the default, an empty stream.
    pub fn set_stdin(&mut self, stream: Option<Box<dyn Stream>>) {
        self.io_std[0] = stream.map(|s| stdlib::io::FileState::new(stdlib::io::Backing::Stream(s)));
    }

    /// Sets `io.stdout` (`Libs::IO`): where `io.write` writes by default. `None` is the
    /// default: where `print` writes (the host's sink or [`Lua::take_output`]).
    pub fn set_stdout(&mut self, stream: Option<Box<dyn Stream>>) {
        self.io_std[1] = stream.map(|s| stdlib::io::FileState::new(stdlib::io::Backing::Stream(s)));
    }

    /// Sets `io.stderr` (`Libs::IO`). `None` is the default: where `print` writes.
    pub fn set_stderr(&mut self, stream: Option<Box<dyn Stream>>) {
        self.io_std[2] = stream.map(|s| stdlib::io::FileState::new(stdlib::io::Backing::Stream(s)));
    }

    /// Sets what `io.open(name, mode)` (and `io.lines(name)`, `io.input(name)`,
    /// `io.output(name)`) asks for a file (`Libs::IO`): the name (up to its first zero
    /// byte; one that is not UTF-8 is never asked for) and a mode `io.open` accepted (C's
    /// `[rwa]%+?b*`). An error is the script's `nil, "name: message", code`. `None`
    /// (the default) has every name fail with "No such file or directory".
    pub fn set_file_opener(&mut self, opener: Option<impl Fn(&str, &str) -> Result<Box<dyn Stream>, IoError> + Send + Sync + 'static>) {
        self.io_opener = opener.map(|f| Arc::new(f) as IoOpener);
    }

    /// Empties `os.getenv`'s map ([`Lua::set_env`]).
    pub fn clear_env(&mut self) {
        self.env.clear();
    }

    /// Sets the time zone `os.date` uses for local time, in seconds east of UTC.
    pub fn set_utc_offset(&mut self, seconds: i64) {
        self.utc_offset = seconds;
    }

    /// Registers module `name` for `require` (`Libs::REQUIRE`): `require(name)` compiles
    /// `code` as chunk `@name` (unless `package.loaded` or `package.preload` has it first).
    /// The host's copy is not counted against the memory limit; what compiling it makes is.
    pub fn add_module(&mut self, name: &str, code: impl AsRef<[u8]>) {
        self.modules.insert(name.as_bytes().to_vec(), ModuleSource::new(code, name));
    }

    /// Registers module `name` with its own origin (see [`ModuleSource`]).
    pub fn add_module_source(&mut self, name: &str, source: ModuleSource) {
        self.modules.insert(name.as_bytes().to_vec(), source);
    }

    /// Sets the loader `require` asks for modules not registered with `add_module`
    /// (`None` removes it). A name that is not UTF-8 is never asked for.
    pub fn set_module_loader(&mut self, loader: Option<impl Fn(&str) -> Option<ModuleSource> + Send + Sync + 'static>) {
        self.module_loader = loader.map(|f| Arc::new(f) as ModuleLoader);
    }

    /// Opens more libraries (a capability granted later).
    pub fn open_libs(&mut self, libs: Libs) {
        self.libs = self.libs | libs;
        stdlib::open(self, libs);
    }
}

/// The deepest nesting [`Lua::from_data`] makes.
const MAX_FROM_DATA_DEPTH: usize = 64;

/// Bytes a tracked userdata's key takes in the collected queue (charged while queued).
const QUEUE_ENTRY: usize = crate::nostd::mem::size_of::<u64>();

/// The most bytes [`Lua::set_userdata_size`] accepts (no allocation is larger).
const MAX_UD_BYTES: usize = isize::MAX as usize - heap::UD_BASE;

/// Units of compiler scan work per unit of fuel (`diag::compile_work`).
const COMPILE_WORK_PER_FUEL: u64 = 8;

pub(crate) enum CompileError {
    Syntax(Diagnostic),
    Memory,
    /// The host's interrupt or deadline stopped it.
    Limit(LimitKind),
}

impl Lua {
    pub(crate) fn alloc_ud(&mut self, data: Box<dyn Any + Send>, size: usize, meta: Option<TableRef>) -> Result<UdRef, LuaError> {
        match self.heap.new_userdata(data, size, meta) {
            Ok(r) => Ok(r),
            Err(()) => Err(LuaError::Limit(LimitKind::Memory)),
        }
    }
}

/// Roughly where the native stack is now (the address of a local), to measure how much a
/// call uses. Safe: the address is only compared, never dereferenced.
#[inline(never)]
pub(crate) fn stack_pos() -> usize {
    let marker = 0u8;
    crate::nostd::hint::black_box(&marker) as *const u8 as usize
}

crate::thread_local! {
    /// Test switch (`set_test_stack_cap`): the most `Limits::stack_bytes` may be for states
    /// made or re-limited on this thread.
    static TEST_STACK_CAP: crate::nostd::cell::Cell<usize> = const { crate::nostd::cell::Cell::new(usize::MAX) };
}

/// Test switch: states made (or given new limits) on this thread use at most `bytes` of
/// native stack for library callbacks, whatever their `Limits::stack_bytes`. The test
/// suites run fuzzers and stress modes on small-stack threads with it, so any recursion
/// whose depth depends on a script's data overflows in a test, not in production.
#[doc(hidden)]
pub fn set_test_stack_cap(bytes: Option<usize>) {
    TEST_STACK_CAP.with(|c| c.set(bytes.unwrap_or(usize::MAX)));
}

pub(crate) fn test_stack_cap() -> usize {
    TEST_STACK_CAP.with(crate::nostd::cell::Cell::get)
}

/// Results of a host function's call past which its part of `host_rets` keeps each object
/// once (S6-4).
const HOST_RETS_COMPACT: usize = 256;

/// The collector's phase (`Lua::collector_status`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum CollectorPhase {
    /// Between cycles: none under way.
    Idle,
    /// Marking what is reachable, a step at a time.
    Marking,
    /// The end of marking (the atomic phase), suspended at a call's fuel: the next call
    /// from the host finishes it before any script code runs, charged to that call.
    AtomicSuspended,
    /// Freeing what marking did not reach, a step at a time.
    Sweeping,
}

/// What `Lua::collector_status` reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct CollectorStatus {
    /// Where the cycle under way is.
    pub phase: CollectorPhase,
    /// Fuel for work already done and not yet charged: the collector's next steps or
    /// collection charge it first. Exact (work under one unit of fuel is not counted).
    pub owed_fuel: u64,
    /// At least this much more fuel finishes the cycle under way (0 when `Idle`). A floor:
    /// the sweep's unit per object for the slabs it has not reached, nothing for marking.
    pub remaining_fuel_at_least: u64,
    /// Whether the cycle can need more than `remaining_fuel_at_least` (every phase but
    /// `Idle`): marking finds the objects it must mark as it goes, the atomic phase what
    /// the writes since gave it, and the sweep walks the interner for each dead interned
    /// string.
    pub more_may_be_found: bool,
}

/// How a call started with [`Lua::start_call`] stands after a poll.
#[cfg(feature = "unstable-resumable")]
#[derive(Debug)]
pub enum CallPoll {
    /// It returned (or stopped): what [`Lua::call`] would have returned.
    Ready(Result<Vec<Value>, Error>),
    /// It paused at the end of the poll: resume it with [`Lua::resume_call`].
    Pending,
}
