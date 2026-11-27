//! The standard library: what scripts get, by capability ([`crate::Libs`]).

#[allow(unused_imports)]
use crate::nostd::prelude::*;
use crate::heap::Heap;
use crate::value::{StrRef, TableRef, Value};
use crate::vm::{Args, Flow, Lua, LuaError, NativeFn};

pub(crate) mod base;
mod coroutine;
mod debuglib;
pub(crate) mod io;
mod math;
mod os;
mod pack;
mod package;
pub(crate) mod pattern;
pub(crate) mod string;
mod strformat;
pub(crate) mod table;
pub(crate) mod utf8;

pub(crate) type R = Result<Flow, LuaError>;

// ---- work paid on every exit ----
//
// A library function that builds output (or scans input) in proportion to its arguments
// and may raise part-way must pay for what it did on the error path too (sweep 4 found
// seven that did not: SEC4-1..7). So that no function has to remember, that work is
// recorded as it is done (`owe`, or an `OutBuf` that records its length when dropped) and
// `Lua::call_native`, the one place every library function is called from, charges it
// when the function returns, whichever way it returns.

crate::thread_local! {
    /// Fuel owed by library functions running on this thread, not yet charged.
    static OWED: crate::nostd::cell::Cell<u64> = const { crate::nostd::cell::Cell::new(0) };
}

/// Records `n` units of fuel of work done, charged when the library function returns.
pub(crate) fn owe(n: u64) {
    OWED.with(|o| o.set(o.get().saturating_add(n)));
}

/// The owed counter (see `owe`): `call_native` takes what a function added to it.
pub(crate) fn owed() -> u64 {
    OWED.with(crate::nostd::cell::Cell::get)
}

/// Sets the owed counter back (after `call_native` took a function's part).
pub(crate) fn set_owed(n: u64) {
    OWED.with(|o| o.set(n));
}

crate::thread_local! {
    /// Bytes of library functions' buffers on this thread given back to the memory account
    /// when the buffer went (see `MemHold`), not yet released: `call_native` releases what
    /// a function added.
    static HELD_BACK: crate::nostd::cell::Cell<usize> = const { crate::nostd::cell::Cell::new(0) };
}

/// The bytes given back (see `HELD_BACK`): `call_native` takes what a function added.
pub(crate) fn held_back() -> usize {
    HELD_BACK.with(crate::nostd::cell::Cell::get)
}

/// Sets the given-back counter (after `call_native` released a function's part).
pub(crate) fn set_held_back(n: usize) {
    HELD_BACK.with(|h| h.set(n));
}

/// A library function's buffer counted against the memory limit while the function holds
/// it (SEC-M1: a buffer held across a callback, `gsub`'s result while its replacement
/// function runs, `load`'s source while its reader runs, was outside the account, so
/// callbacks nested to the depth of library calls held that many buffers of up to
/// `string_len` bytes each: 800 MB of process memory under a 64 MiB limit). `cover` charges
/// its capacity as it grows (before each callback, so every buffer a nested call sees is
/// counted); `give_back` (or dropping it, on an error) returns it. The charge does not
/// pace the collector: the buffer is not an object.
#[derive(Default)]
pub(crate) struct MemHold {
    held: usize,
}

impl MemHold {
    /// Counts `cap` bytes in all (more than before: the difference is charged).
    pub(crate) fn cover(&mut self, lua: &mut Lua, cap: usize) -> Result<(), LuaError> {
        if cap > self.held {
            let n = cap - self.held;
            lua.charge_mem(n)?;
            lua.heap.mem.debt = lua.heap.mem.debt.saturating_sub(isize::try_from(n).unwrap_or(isize::MAX));
            self.held = cap;
        }
        Ok(())
    }

    /// Room in `buf` for `n` bytes more, counted before it is allocated (SEC-M14: grown
    /// first, a buffer was outside the limit until its next payment, a string's length and
    /// its doubling at once).
    #[inline]
    pub(crate) fn reserve(&mut self, lua: &mut Lua, buf: &mut Vec<u8>, n: usize) -> Result<(), LuaError> {
        let need = buf.len().saturating_add(n);
        if need <= buf.capacity() {
            return Ok(());
        }
        self.grow(lua, buf, need)
    }

    /// `reserve` past the capacity, out of line: the check above is all a run of text in
    /// `format` costs (called once a spec, a call of this whole body made `format('%% ')`
    /// of 16,384 specs 1.6x the CPU per unit of fuel it was, LR-29e).
    #[cold]
    #[inline(never)]
    fn grow(&mut self, lua: &mut Lua, buf: &mut Vec<u8>, need: usize) -> Result<(), LuaError> {
        // Doubling, as it would grow, but not past the longest string.
        let cap = buf.capacity().saturating_mul(2).min(lua.limits.string_len).max(need);
        self.cover(lua, cap)?;
        if (COPY_PIECE..32 << 20).contains(&buf.capacity()) {
            // Copied a piece at a time between looks, as `realloc` copies one served from the
            // heap (bigger ones are mapped, moved without a copy): 16 MiB grown in one go was
            // 8 to 9.5 ms with no look (LR-29, `format('%s%s')` of 16 MiB strings).
            let mut new = Vec::with_capacity(cap);
            for (i, piece) in buf.chunks(COPY_PIECE).enumerate() {
                if i > 0 {
                    lua.check_time()?;
                }
                new.extend_from_slice(piece);
            }
            *buf = new;
        } else {
            buf.reserve_exact(cap - buf.len());
        }
        self.cover(lua, buf.capacity())
    }

    /// Returns the bytes counted now (the buffer is gone or becomes an object, charged
    /// itself).
    pub(crate) fn give_back(&mut self, lua: &mut Lua) {
        lua.heap.mem.release(crate::nostd::mem::take(&mut self.held));
    }
}

impl Drop for MemHold {
    fn drop(&mut self) {
        if self.held > 0 {
            let n = crate::nostd::mem::take(&mut self.held);
            HELD_BACK.with(|h| h.set(h.get().saturating_add(n)));
        }
    }
}

/// A library function's output buffer: when dropped (on any exit, an error included) or
/// taken (`into_vec`), it owes a unit of fuel per `per_unit` bytes it holds, less what
/// `pay` already charged. Its capacity counts against the memory limit from each `pay`
/// until it goes (`MemHold`).
pub(crate) struct OutBuf {
    buf: Vec<u8>,
    per_unit: usize,
    /// Units charged so far by `pay`.
    paid: u64,
    mem: MemHold,
}

impl OutBuf {
    pub(crate) fn new(per_unit: usize) -> OutBuf {
        OutBuf { buf: Vec::new(), per_unit: per_unit.max(1), paid: 0, mem: MemHold::default() }
    }

    /// Units the bytes held cost and `pay` has not charged.
    fn due(&self) -> u64 {
        ((self.buf.len() / self.per_unit) as u64).saturating_sub(self.paid)
    }

    /// Charges for the bytes added since the last payment. Called as the buffer grows (a
    /// unit per element, conversion or match): what is owed is charged only when the
    /// function returns, so without this one call built up to a whole string's length of
    /// output (`string_len` / `per_unit`, half a million units) past the fuel limit, and a
    /// call with 20,000 units left did 15 to 45 times that much work (S5SEC-3). Also counts
    /// the buffer's growth against the memory limit (SEC-M1): every function that calls
    /// back pays before the callback.
    pub(crate) fn pay(&mut self, lua: &mut Lua) -> Result<(), LuaError> {
        let due = self.due();
        if due > 0 {
            self.paid += due;
            lua.charge(due)?;
        }
        self.mem.cover(lua, self.buf.capacity())
    }

    /// Room for `n` bytes more, counted before it is allocated (`MemHold::reserve`).
    pub(crate) fn reserve(&mut self, lua: &mut Lua, n: usize) -> Result<(), LuaError> {
        self.mem.reserve(lua, &mut self.buf, n)
    }

    /// The bytes, paid for; their memory is given back (the caller makes them a string,
    /// charged itself).
    pub(crate) fn into_vec(mut self, lua: &mut Lua) -> Vec<u8> {
        owe(self.due());
        self.paid = 0;
        self.mem.give_back(lua);
        crate::nostd::mem::take(&mut self.buf)
    }
}

impl crate::nostd::ops::Deref for OutBuf {
    type Target = Vec<u8>;
    fn deref(&self) -> &Vec<u8> {
        &self.buf
    }
}

impl crate::nostd::ops::DerefMut for OutBuf {
    fn deref_mut(&mut self) -> &mut Vec<u8> {
        &mut self.buf
    }
}

impl Drop for OutBuf {
    fn drop(&mut self) {
        owe(self.due());
    }
}

/// Strings the libraries use often, interned once.
pub(crate) struct Names {
    pub n: StrRef,
    /// `type`'s results, in `TYPE_NAMES` order.
    types: [StrRef; 8],
    /// `next` and `ipairs`'s iterator, which the generic `for` runs in place.
    pub next_fn: Option<crate::value::FuncRef>,
    pub ipairs_aux: Option<crate::value::FuncRef>,
    /// `utf8.codes`' strict and lax iterators, run in place the same way.
    pub utf8_iters: [Option<crate::value::FuncRef>; 2],
    /// `math.sqrt`, `floor`, `ceil`, `abs`, `min`, `max` (`MATH_FAST` order): the fast loop
    /// computes a call to exactly these function values in place.
    pub math_fast: [Option<crate::value::FuncRef>; 6],
    /// `table.insert` and `table.remove` (`table::IN_PLACE` order): the fast loop runs a
    /// call to exactly these function values on a plain array in place.
    pub table_fast: [Option<crate::value::FuncRef>; 2],
    /// `fast::math_mask` of `table_fast`.
    pub table_mask: u64,
    /// `fast::math_mask` of `math_fast`.
    pub math_mask: u64,
    /// `coroutine.yield`, which a call from Lua code runs without `call_native`'s
    /// set-up (`Lua::yield_from_lua`).
    pub yield_fn: Option<crate::value::FuncRef>,
    /// `collectgarbage`: its basic step runs at the instruction that calls it, where the
    /// call can pause (`Lua::collect_step`, SEC7-G1).
    pub collect_fn: Option<crate::value::FuncRef>,
    /// The loaded modules (C's `LUA_LOADED_TABLE`, also in the registry): every library
    /// opened, by name (`_G` for the globals), and what `require` loaded. `debug.traceback`
    /// names functions from it as C Lua does.
    pub loaded: Option<TableRef>,
}

/// Items a native that builds many values handles between looks at the host's interrupt
/// and deadline (`Lua::check_time`, S6-5).
pub(crate) const NATIVE_CHUNK: usize = 1 << 16;

/// Bytes a native copies or scans between looks at the host's interrupt and deadline
/// (about half a millisecond of fresh memory).
pub(crate) const COPY_PIECE: usize = 1 << 20;

const TYPE_NAMES: [&str; 8] = ["nil", "boolean", "number", "string", "table", "function", "userdata", "thread"];

impl Names {
    pub fn new(heap: &mut Heap) -> Names {
        let mk = |h: &mut Heap, s: &str| h.intern(s.as_bytes()).unwrap_or(StrRef { idx: 0, generation: 0 });
        Names { n: mk(heap, "n"), types: TYPE_NAMES.map(|t| mk(heap, t)), next_fn: None, ipairs_aux: None, utf8_iters: [None; 2], math_fast: [None; 6], math_mask: 0, table_fast: [None; 2], table_mask: 0, yield_fn: None, collect_fn: None, loaded: None }
    }

    pub fn mark(&self, heap: &mut Heap) {
        heap.mark(Value::Str(self.n));
        for t in self.types {
            heap.mark(Value::Str(t));
        }
    }

    /// `type(v)`, interned once.
    pub fn type_of(&self, v: Value) -> StrRef {
        self.types.get(type_index(v)).copied().unwrap_or(self.n)
    }

    /// `type`'s results, `type_of` order.
    pub(crate) fn types(&self) -> &[StrRef; 8] {
        &self.types
    }
}

/// `v`'s index in `Names::type_of`'s list.
pub(crate) fn type_index(v: Value) -> usize {
    match v {
        Value::Nil => 0,
        Value::Bool(_) => 1,
        Value::Int(_) | Value::Num(_) => 2,
        Value::Str(_) => 3,
        Value::Table(_) => 4,
        Value::Func(_) => 5,
        Value::UserData(_) => 6,
        Value::Thread(_) => 7,
    }
}

/// Opens `libs` into the globals.
pub(crate) fn open(lua: &mut Lua, libs: crate::Libs) {
    use crate::Libs;
    let saved = lua.heap.mem.headroom;
    lua.heap.mem.headroom = 1 << 20;
    lua.heap.blocked += 1;
    if libs.contains(Libs::BASE) {
        base::open(lua, libs);
    }
    let opens: [(Libs, fn(&mut Lua)); 10] = [
        (Libs::STRING, string::open),
        (Libs::TABLE, table::open),
        (Libs::MATH, math::open),
        (Libs::UTF8, utf8::open),
        (Libs::COROUTINE, coroutine::open),
        (Libs::OS, os::open),
        (Libs::ENV, os::open_env),
        (Libs::IO, io::open),
        (Libs::DEBUG_INFO, debuglib::open),
        (Libs::REQUIRE, package::open),
    ];
    for (lib, open) in opens {
        if libs.contains(lib) {
            open(lua);
        }
    }
    // Each library opened is a loaded module, as `luaL_openlibs` makes it.
    let opened: [(Libs, &str); 11] = [
        (Libs::BASE, "_G"),
        (Libs::REQUIRE, "package"),
        (Libs::COROUTINE, "coroutine"),
        (Libs::TABLE, "table"),
        (Libs::OS, "os"),
        (Libs::ENV, "os"),
        (Libs::IO, "io"),
        (Libs::STRING, "string"),
        (Libs::MATH, "math"),
        (Libs::UTF8, "utf8"),
        (Libs::DEBUG_INFO, "debug"),
    ];
    if let Some(loaded) = lua.ensure_loaded() {
        for (lib, name) in opened {
            if libs.contains(lib) {
                let k = lua.new_str(name.as_bytes()).unwrap_or_default();
                let v = lua.raw_get(lua.globals, k);
                if !v.is_nil() {
                    let _ = lua.raw_set(loaded, k, v);
                }
            }
        }
    }
    lua.heap.blocked -= 1;
    lua.heap.mem.headroom = saved;
}

impl Lua {
    /// The loaded-modules table (see `Names::loaded`), if it was made.
    pub(crate) fn loaded_table(&self) -> Option<TableRef> {
        self.names.loaded.filter(|t| self.heap.is_live(Value::Table(*t)))
    }

    /// The loaded-modules table, made (and kept in the registry) if missing.
    pub(crate) fn ensure_loaded(&mut self) -> Option<TableRef> {
        if let Some(t) = self.loaded_table() {
            return Some(t);
        }
        let t = self.new_table(0, 16).ok()?;
        let k = self.new_str(b"_LOADED").ok()?;
        let reg = self.registry;
        self.raw_set(reg, k, Value::Table(t)).ok()?;
        self.names.loaded = Some(t);
        Some(t)
    }

    /// A library table under global `name` (made if missing).
    pub(crate) fn lib_table(&mut self, name: &str) -> TableRef {
        let k = self.new_str(name.as_bytes()).unwrap_or_default();
        if let Value::Table(t) = self.raw_get(self.globals, k) {
            return t;
        }
        let t = self.new_table(0, 32).unwrap_or(self.globals);
        let g = self.globals;
        let _ = self.raw_set(g, k, Value::Table(t));
        t
    }

    /// Sets `t[name]` to native `f` (`qualified` is its name in messages, `string.rep`).
    pub(crate) fn set_fn(&mut self, t: TableRef, name: &str, qualified: &'static str, f: NativeFn) {
        let k = self.new_str(name.as_bytes()).unwrap_or_default();
        let v = self.new_native(f, qualified, Vec::new()).unwrap_or_default();
        let _ = self.raw_set(t, k, v);
    }

    pub(crate) fn set_field(&mut self, t: TableRef, name: &str, v: Value) {
        let k = self.new_str(name.as_bytes()).unwrap_or_default();
        let _ = self.raw_set(t, k, v);
    }

    // ---- arguments ----

    #[inline]
    pub(crate) fn arg(&self, a: Args, i: usize) -> Value {
        if i < a.n { self.st(a.base + i) } else { Value::Nil }
    }

    /// The type name for "expected, got" messages (`luaL_typeerror`'s).
    fn got(&self, a: Args, i: usize) -> String {
        if i >= a.n { "no value".into() } else { self.arg_type_name(self.arg(a, i)) }
    }

    pub(crate) fn type_error(&self, a: Args, i: usize, expected: &str) -> LuaError {
        LuaError::Arg(i + 1, format!("{expected} expected, got {}", self.got(a, i)))
    }

    pub(crate) fn check_any(&self, a: Args, i: usize) -> Result<Value, LuaError> {
        if i >= a.n { Err(LuaError::Arg(i + 1, "value expected".into())) } else { Ok(self.arg(a, i)) }
    }

    pub(crate) fn check_table(&self, a: Args, i: usize) -> Result<TableRef, LuaError> {
        match self.arg(a, i) {
            Value::Table(t) => Ok(t),
            _ => Err(self.type_error(a, i, "table")),
        }
    }

    pub(crate) fn check_int(&mut self, a: Args, i: usize) -> Result<i64, LuaError> {
        match self.tonumber_charged(self.arg(a, i))? {
            Some(Value::Int(n)) => Ok(n),
            Some(Value::Num(f)) => {
                crate::number::float_to_int(f).ok_or_else(|| LuaError::Arg(i + 1, "number has no integer representation".into()))
            }
            _ => Err(self.type_error(a, i, "number")),
        }
    }

    pub(crate) fn opt_int(&mut self, a: Args, i: usize, d: i64) -> Result<i64, LuaError> {
        if self.arg(a, i).is_nil() { Ok(d) } else { self.check_int(a, i) }
    }

    pub(crate) fn check_num(&mut self, a: Args, i: usize) -> Result<f64, LuaError> {
        match self.tonumber_charged(self.arg(a, i))? {
            Some(v) => Ok(v.as_num().unwrap_or(0.0)),
            None => Err(self.type_error(a, i, "number")),
        }
    }

    /// A string argument (numbers convert, as in Lua).
    #[inline]
    pub(crate) fn check_str(&mut self, a: Args, i: usize) -> Result<StrRef, LuaError> {
        match self.arg(a, i) {
            Value::Str(s) => Ok(s),
            _ => self.check_str_conv(a, i),
        }
    }

    /// `check_str` for an argument that is not a string.
    #[inline(never)]
    fn check_str_conv(&mut self, a: Args, i: usize) -> Result<StrRef, LuaError> {
        match self.arg(a, i) {
            Value::Str(s) => Ok(s),
            v @ (Value::Int(_) | Value::Num(_)) => {
                let mut b = Vec::new();
                match v {
                    Value::Int(n) => b.extend_from_slice(n.to_string().as_bytes()),
                    Value::Num(f) => {
                        // Writing a float costs as much as eight instructions: `warn` with
                        // 200k float arguments ran at 12x plain code per unit (S5SEC-2).
                        owe(crate::vm::FLOAT_TEXT_FUEL);
                        b.extend_from_slice(crate::number::fmt_float(f).as_bytes());
                    }
                    _ => {}
                }
                match self.new_str(&b)? {
                    Value::Str(s) => Ok(s),
                    _ => Err(self.type_error(a, i, "string")),
                }
            }
            _ => Err(self.type_error(a, i, "string")),
        }
    }

    /// Refuses a string longer than `string_len` (C Lua's "not enough memory").
    #[inline]
    pub(crate) fn check_len(&self, n: usize) -> Result<(), LuaError> {
        if n > self.limits.string_len { Err(LuaError::Limit(crate::LimitKind::Memory)) } else { Ok(()) }
    }

    pub(crate) fn opt_str(&mut self, a: Args, i: usize) -> Result<Option<StrRef>, LuaError> {
        if self.arg(a, i).is_nil() { Ok(None) } else { self.check_str(a, i).map(Some) }
    }

    #[inline]
    pub(crate) fn ret(&mut self, vals: &[Value]) -> R {
        self.rets.clear();
        self.rets.extend_from_slice(vals);
        Ok(Flow::Return)
    }

    #[inline]
    pub(crate) fn ret1(&mut self, v: Value) -> R {
        self.rets.clear();
        self.rets.push(v);
        Ok(Flow::Return)
    }

    pub(crate) fn ret_str(&mut self, b: &[u8]) -> R {
        let v = self.new_str(b)?;
        self.ret1(v)
    }

    /// Appends bytes `from..to` of `s` to `out` a piece at a time (`COPY_PIECE`), looking at
    /// the host's interrupt and deadline between pieces: a 16 MiB string copied in one go
    /// ran 6 to 9 ms past a time limit (`string.pack('s')`, `string.format('%s')`).
    pub(crate) fn extend_watched(&mut self, out: &mut Vec<u8>, s: StrRef, from: usize, to: usize) -> Result<(), LuaError> {
        let to = to.min(self.heap.str_bytes(s).len());
        if from >= to {
            return Ok(());
        }
        if to - from > COPY_PIECE {
            // Room once: growing by doublings copied what was there again each time.
            out.reserve(to - from);
        }
        let mut at = from;
        while at < to {
            let end = at.saturating_add(COPY_PIECE).min(to);
            out.extend_from_slice(self.heap.str_bytes(s).get(at..end).unwrap_or_default());
            at = end;
            if at < to {
                self.check_time()?;
            }
        }
        Ok(())
    }

    /// The string of bytes `from..to` of `s` (as `new_derived` makes it): a long one is
    /// copied a piece at a time between looks at the host's interrupt and deadline, its
    /// charge given back if a look stops it (`string.unpack('s')` of 16 MiB ran 7 ms past
    /// a time limit).
    pub(crate) fn new_copied(&mut self, s: StrRef, from: usize, to: usize) -> Result<Value, LuaError> {
        let len = self.heap.str_bytes(s).get(from..to).map_or(0, <[u8]>::len);
        if len <= COPY_PIECE {
            return self.new_derived(s, from, to, |src, dest| dest.copy_from_slice(src));
        }
        let charged = crate::heap::str_mem(len);
        self.charge_mem(charged)?;
        let mut buf = Vec::with_capacity(len);
        if let Err(e) = self.extend_watched(&mut buf, s, from, to) {
            self.heap.mem.release(charged);
            return Err(e);
        }
        let v = Value::Str(self.heap.new_long_charged(buf.into_boxed_slice()));
        self.hold(v);
        Ok(v)
    }

    /// A string from a buffer the library built: a long one becomes the string as it is
    /// (charged now, before the string exists), not copied again.
    pub(crate) fn new_owned(&mut self, b: Vec<u8>) -> Result<Value, LuaError> {
        if b.len() <= crate::heap::SHORT_LEN || b.len() > self.limits.string_len {
            return self.new_str(&b);
        }
        self.charge_mem(crate::heap::str_mem(b.len()))?;
        let v = Value::Str(self.heap.new_long_charged(b.into_boxed_slice()));
        self.hold(v);
        Ok(v)
    }

    pub(crate) fn ret_owned(&mut self, b: Vec<u8>) -> R {
        let v = self.new_owned(b)?;
        self.ret1(v)
    }

    /// Returns a string made from bytes `from..to` of `s` (see `new_derived`).
    pub(crate) fn ret_derived(&mut self, s: StrRef, from: usize, to: usize, fill: impl Fn(&[u8], &mut [u8])) -> R {
        let v = self.new_derived(s, from, to, fill)?;
        self.ret1(v)
    }

    /// A string made from bytes `from..to` of `s` by `fill(source, dest)` (`dest` as long
    /// as the source).
    pub(crate) fn new_derived(&mut self, s: StrRef, from: usize, to: usize, fill: impl Fn(&[u8], &mut [u8])) -> Result<Value, LuaError> {
        // The source's real length, so `dest` always matches it.
        let len = self.heap.str_bytes(s).get(from..to).map_or(0, <[u8]>::len);
        self.new_built(len, |h, dest| fill(h.str_bytes(s).get(from..to).unwrap_or_default(), dest))
    }

    /// [`Lua::new_derived`] for a byte-by-byte map, `fill(source, at, dest)` writing
    /// `dest` as bytes `at..` of the result: a long one is filled a chunk at a time between
    /// looks at the host's interrupt and deadline (SEC-M4: `upper`, `lower` and `reverse`
    /// of a 16 MiB string ran 7 to 12 ms past the time limit), its charge given back if a
    /// look stops it.
    pub(crate) fn ret_mapped(&mut self, s: StrRef, fill: impl Fn(&[u8], usize, &mut [u8])) -> R {
        const CHUNK: usize = 1 << 20;
        let len = self.heap.str_bytes(s).len();
        if len <= CHUNK {
            return self.ret_derived(s, 0, len, |src, dest| fill(src, 0, dest));
        }
        self.charge_mem(crate::heap::str_mem(len))?;
        let mut buf = vec![0u8; len].into_boxed_slice();
        for (k, dest) in buf.chunks_mut(CHUNK).enumerate() {
            if k > 0
                && let Err(e) = self.check_time()
            {
                self.heap.mem.release(crate::heap::str_mem(len));
                return Err(e);
            }
            fill(self.heap.str_bytes(s), k * CHUNK, dest);
        }
        let v = Value::Str(self.heap.new_long_charged(buf));
        self.hold(v);
        self.ret1(v)
    }

    /// A string of `len` bytes (within the string limit) written by `fill(heap, dest)`,
    /// copied once: a short one is built on the stack and interned; a long one is charged
    /// first, then built in its own buffer.
    pub(crate) fn new_built(&mut self, len: usize, fill: impl Fn(&Heap, &mut [u8])) -> Result<Value, LuaError> {
        if len <= crate::heap::SHORT_LEN {
            let mut buf = [0u8; crate::heap::SHORT_LEN];
            let dest = buf.get_mut(..len).unwrap_or_default();
            fill(&self.heap, dest);
            return self.new_str(dest);
        }
        self.charge_mem(crate::heap::str_mem(len))?;
        let mut buf = vec![0u8; len].into_boxed_slice();
        fill(&self.heap, &mut buf);
        let v = Value::Str(self.heap.new_long_charged(buf));
        self.hold(v);
        Ok(v)
    }

    /// The native's own upvalue `i`.
    pub(crate) fn native_upval(&self, a: Args, i: usize) -> Value {
        let Value::Func(f) = self.st(a.func) else { return Value::Nil };
        match self.heap.funcs.get(f.idx) {
            Some(crate::heap::Func::Native { upvals, .. }) => upvals.get(i).copied().unwrap_or_default(),
            _ => Value::Nil,
        }
    }

    /// `luaL_checkoption(L, i, def, names)`: the index of the name argument `i` gives (read
    /// up to a zero byte, as C compares it).
    pub(crate) fn check_option(&mut self, a: Args, i: usize, def: Option<&str>, names: &[&str]) -> Result<usize, LuaError> {
        let name = match (self.arg(a, i), def) {
            (Value::Nil, Some(d)) => d.as_bytes().to_vec(),
            _ => {
                let s = self.check_str(a, i)?;
                self.bytes(s).split(|&c| c == 0).next().unwrap_or_default().to_vec()
            }
        };
        names.iter().position(|n| n.as_bytes() == name).ok_or_else(|| {
            let shown = String::from_utf8_lossy(name.get(..name.len().min(self.limits.error_len)).unwrap_or_default()).into_owned();
            LuaError::Arg(i + 1, format!("invalid option '{shown}'"))
        })
    }

    /// How many upvalues the native has.
    pub(crate) fn native_upvals_len(&self, a: Args) -> usize {
        let Value::Func(f) = self.st(a.func) else { return 0 };
        match self.heap.funcs.get(f.idx) {
            Some(crate::heap::Func::Native { upvals, .. }) => upvals.len(),
            _ => 0,
        }
    }

    /// Sets the native's own upvalue `i` (iterators keep their position there).
    pub(crate) fn set_native_upval(&mut self, a: Args, i: usize, v: Value) {
        let Value::Func(f) = self.st(a.func) else { return };
        if let Some(crate::heap::Func::Native { upvals, .. }) = self.heap.funcs.get_mut(f.idx)
            && let Some(s) = upvals.get_mut(i)
        {
            *s = v;
        }
        if v.is_collectable() {
            // Native closures are traversed whole: mark it again if marking passed it.
            if self.heap.phase == crate::heap::Phase::Propagate {
                self.heap.mark(v);
            }
        }
    }
}
