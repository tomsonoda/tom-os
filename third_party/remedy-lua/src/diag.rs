//! Diagnostics: errors and warnings with where they are (chunk, line, column).

#[allow(unused_imports)]
use crate::nostd::prelude::*;
use crate::nostd::fmt;

/// A place in source text: line and column, both from 1.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Pos {
    /// The line, from 1.
    pub line: u32,
    /// The column (byte in the line), from 1.
    pub col: u32,
}

/// How serious a diagnostic is. More levels may come: match with a `_` arm.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum Severity {
    /// The code cannot run (syntax) or will not do what its annotations say (types).
    Error,
    /// Probably a mistake.
    Warning,
}

/// One message about source text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostic {
    /// How serious.
    pub severity: Severity,
    /// The chunk's name (a file name, usually).
    pub chunk: String,
    /// Where the text it is about starts.
    pub pos: Pos,
    /// Where that text ends: the line and column just past its last character (`pos` when
    /// it is about a point, such as the end of input).
    pub end: Pos,
    /// The line Lua's own error message gives: where C Lua notices the problem, which can
    /// lie after `end` (an undefined `goto` is noticed where its function ends).
    pub line: u32,
    /// What is wrong, for the person who wrote it.
    pub message: String,
    /// The message as bytes when it quotes source that is not UTF-8 (`message` has those
    /// bytes replaced): what Lua's own message holds, as C Lua prints them (S6-FRONT-7).
    raw: Option<Box<[u8]>>,
}

impl Diagnostic {
    pub(crate) fn error(chunk: &str, pos: Pos, msg: impl Into<String>) -> Self {
        Diagnostic { severity: Severity::Error, chunk: chunk.to_owned(), pos, end: pos, line: pos.line, message: msg.into(), raw: None }
    }

    pub(crate) fn warning(chunk: &str, pos: Pos, msg: impl Into<String>) -> Self {
        Diagnostic { severity: Severity::Warning, chunk: chunk.to_owned(), pos, end: pos, line: pos.line, message: msg.into(), raw: None }
    }

    /// An error whose message is `raw` (read as text with invalid UTF-8 replaced).
    pub(crate) fn error_bytes(chunk: &str, pos: Pos, raw: Vec<u8>) -> Self {
        let mut d = Diagnostic::error(chunk, pos, String::from_utf8_lossy(&raw).into_owned());
        if crate::nostd::str::from_utf8(&raw).is_err() {
            d.raw = Some(raw.into_boxed_slice());
        }
        d
    }

    /// The same, about the text from `pos` to `end`.
    pub(crate) fn to(mut self, end: Pos) -> Self {
        self.end = end.max(self.pos);
        self
    }

    /// The same, with Lua's message giving `line`.
    pub(crate) fn lua_line(mut self, line: u32) -> Self {
        self.line = line;
        self
    }

    /// As Lua's own error message reads: `chunk:line: message`.
    pub fn lua_message(&self) -> String {
        format!("{}:{}: {}", self.chunk, self.line, self.message)
    }

    /// Lua's own error message as the bytes C Lua's holds: source it quotes as written.
    pub(crate) fn lua_message_bytes(&self) -> Vec<u8> {
        match &self.raw {
            None => self.lua_message().into_bytes(),
            Some(raw) => {
                let mut b = format!("{}:{}: ", self.chunk, self.line).into_bytes();
                b.extend_from_slice(raw);
                b
            }
        }
    }
}

impl fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let sev = match self.severity {
            Severity::Error => "",
            Severity::Warning => "warning: ",
        };
        write!(f, "{}:{}:{}: {sev}{}", self.chunk, self.pos.line, self.pos.col, self.message)
    }
}

impl crate::nostd::error::Error for Diagnostic {}

// ---- the host's interrupt and deadline while compiling ----

/// Lexer, parser and compiler steps between looks at the watch.
/// (A compiler step can take a microsecond, a local made or a constant found: 4,096
/// steps were 3 to 4 ms between looks; at 1,024, one atomic load per 1,024 steps.)
pub(crate) const COMPILE_WATCH_EVERY: u32 = 1024;

/// Units of scan work (`compile_work`) that count as one step between looks at the watch.
/// One step can scan thousands of locals, labels or gotos: counted as one, 4,096 of them
/// kept the interrupt and deadline unread for over 100 ms (S5C-FRONT-2).
const COMPILE_WORK_PER_TICK: u64 = 16;

/// The message of the error a stopped compile unwinds with (turned into the limit).
pub(crate) const COMPILE_STOPPED: &str = "compile stopped by the host";

crate::thread_local! {
    /// The running compile's watch (the state's interrupt and deadline), steps since the
    /// last look, and the limit that stopped it.
    static COMPILE_WATCH: crate::nostd::cell::RefCell<Option<(crate::stdlib::pattern::Watch, Option<crate::LimitKind>)>> =
        const { crate::nostd::cell::RefCell::new(None) };
    static COMPILE_TICKS: crate::nostd::cell::Cell<u32> = const { crate::nostd::cell::Cell::new(0) };
    /// The running compile's scan work (`compile_work`) and the most it may do.
    static COMPILE_WORK: crate::nostd::cell::Cell<(u64, u64)> = const { crate::nostd::cell::Cell::new((0, u64::MAX)) };
    /// Whether the running compile must stop (its watch or its scan work said so).
    static COMPILE_STOP: crate::nostd::cell::Cell<bool> = const { crate::nostd::cell::Cell::new(false) };
    /// What the parse reading from `load`'s reader holds as it asks for more, and the room
    /// it has after the reader ran (`parse_reading`).
    static PARSE_READING: crate::nostd::cell::Cell<(usize, Option<usize>)> = const { crate::nostd::cell::Cell::new((0, None)) };
}

/// Exchanged around `load`'s reader: the parser sets what it holds before it reads on,
/// `load` counts that as used while the reader runs and sets the room left after it
/// (SEC-M13: the tree was in no account while the reader ran, so the reader's allocations
/// and an inner `load`'s room came on top of it).
pub(crate) fn parse_reading(set: (Option<usize>, Option<usize>)) -> (usize, Option<usize>) {
    PARSE_READING.with(|c| {
        let (held, room) = c.get();
        c.set((set.0.unwrap_or(held), set.1));
        (held, room)
    })
}

/// What can be freed a piece at a time (`free_deferred`).
pub(crate) trait Piecewise: Send {
    /// Frees up to `n` pieces: whether all of it is freed.
    fn free_some(&mut self, n: usize) -> bool;
    /// About the bytes it holds (charged to the state's memory while it waits).
    fn bytes(&self) -> usize;
}

impl<T: Send> Piecewise for Vec<T> {
    fn bytes(&self) -> usize {
        self.capacity().saturating_mul(crate::nostd::mem::size_of::<T>())
    }

    fn free_some(&mut self, n: usize) -> bool {
        self.truncate(self.len().saturating_sub(n));
        crate::ast::shed(self);
        if self.is_empty() {
            *self = Vec::new();
        }
        self.is_empty()
    }
}

impl Piecewise for crate::ast::Dropper {
    fn free_some(&mut self, n: usize) -> bool {
        self.step(n)
    }
    fn bytes(&self) -> usize {
        self.bytes()
    }
}

/// Bytes past which a vector of the front end grows by `grow_watched`.
/// (1 MB: page faults on fresh memory cost about 0.45 ms per MB, so growing a 16 MB list
/// in one `realloc` took 10 to 25 ms.)
const GROW_WATCHED: usize = 1 << 20;

/// Elements moved between steps while a big vector grows.
const GROW_STRETCH: usize = 1 << 15;

/// Grows `v` to hold `cap` elements. A big buffer is moved to the new one a stretch at a
/// time, each a look at the host's interrupt (S6-FRONT-4: growing a 2M-field table
/// constructor's list, 320 MB, took 190 ms in one `realloc`). Whether the compile was told
/// to stop meanwhile: then `v` is left empty, and what it held is freed later.
pub(crate) fn grow_watched<T: Send + 'static>(v: &mut Vec<T>, cap: usize) -> bool {
    if cap <= v.capacity() {
        return false;
    }
    // (With nothing to watch, as for `check` and `format`, at once: `realloc` can often
    // move a big buffer without copying it.)
    if cap.saturating_mul(crate::nostd::mem::size_of::<T>()) < GROW_WATCHED || !watching() {
        v.reserve_exact(cap - v.len());
        return false;
    }
    let mut new = Vec::with_capacity(cap);
    let mut old = crate::nostd::mem::take(v);
    // Reversed in place, then moved from its end, so the old buffer is given back a
    // stretch at a time as it empties (`shed`): freeing it whole once moved (tens of
    // megabytes) was 5 to 10 ms of its own. (A stop leaves it in any order: it is only
    // freed then.)
    let n = old.len();
    let stopped = 'grow: {
        let mut i = 0;
        while i < n / 2 {
            let end = (i + GROW_STRETCH).min(n / 2);
            for k in i..end {
                old.swap(k, n - 1 - k);
            }
            i = end;
            if compile_ticks(COMPILE_WATCH_EVERY) {
                break 'grow true;
            }
        }
        while !old.is_empty() {
            new.extend(crate::nostd::iter::from_fn(|| old.pop()).take(GROW_STRETCH));
            crate::ast::shed(&mut old);
            if compile_ticks(COMPILE_WATCH_EVERY) {
                break 'grow true;
            }
        }
        false
    };
    if stopped {
        defer_pieces(new);
        defer_pieces(old);
        return true;
    }
    *v = new;
    false
}

/// Whether a compile with a watch (the host's interrupt) is running on this thread.
fn watching() -> bool {
    COMPILE_WATCH.with(|w| w.borrow().is_some())
}

/// What a map key holds on the heap besides its slot (for `ShardMap::into_pieces`).
pub(crate) trait KeyHeap {
    fn heap(&self) -> usize {
        0
    }
}

impl KeyHeap for Vec<u8> {
    fn heap(&self) -> usize {
        self.capacity()
    }
}

impl KeyHeap for String {
    fn heap(&self) -> usize {
        self.capacity()
    }
}

impl KeyHeap for crate::nostd::sync::Arc<str> {
    /// Its bytes and counts when this is the last reference (else another owner keeps them).
    fn heap(&self) -> usize {
        if crate::nostd::sync::Arc::strong_count(self) == 1 { self.len() + 16 } else { 0 }
    }
}

/// The bytes the bucket array of a `HashMap` of capacity `cap` takes, as std's (hashbrown)
/// allocates it: a power-of-two number of buckets (`cap` is 7/8 of them, or one less
/// than a small one), each an entry and a control byte, and a group of control bytes more.
pub(crate) fn table_bytes<K, V>(cap: usize) -> usize {
    if cap == 0 {
        return 0;
    }
    let buckets = if cap < 8 { (cap + 1).next_power_of_two() } else { (cap * 8 / 7).next_power_of_two() };
    let entries = (buckets * crate::nostd::mem::size_of::<(K, V)>()).next_multiple_of(16);
    entries + buckets + 16
}

impl<K: KeyHeap + Send, V: Send> Piecewise for crate::shardmap::ShardPieces<K, V> {
    fn free_some(&mut self, n: usize) -> bool {
        crate::shardmap::ShardPieces::free_some(self, n)
    }
    /// Exactly: the shards' bucket arrays and their keys' own bytes.
    fn bytes(&self) -> usize {
        self.held()
    }
}

/// Anything else, freed at once (and its size).
struct Whole(Option<Box<dyn crate::nostd::any::Any + Send>>, usize);

impl Piecewise for Whole {
    fn free_some(&mut self, _: usize) -> bool {
        self.0 = None;
        true
    }
    fn bytes(&self) -> usize {
        self.1
    }
}

crate::thread_local! {
    /// What a compile from the host left to free (S5C-FRONT-12, S6-FRONT-4): freed at the
    /// next compile or call from the host instead of before this one returns (freeing a
    /// multi-megabyte source's tokens and syntax tree took tens of milliseconds, after
    /// the interrupt or after a successful compile, with no look at the interrupt).
    static DEFERRED: crate::nostd::cell::RefCell<Deferred> = const { crate::nostd::cell::RefCell::new(Vec::new()) };
}

/// What waits to be freed, each with the bytes charged for it.
pub(crate) type Deferred = Vec<(Box<dyn Piecewise>, usize)>;

/// Pieces freed between looks at the interrupt (`free_deferred`): well under a millisecond.
const FREE_PIECES: usize = 1024;

/// Keeps `v` to free later, at once (`free_deferred`).
pub(crate) fn defer_drop<T: Send + 'static>(v: T) {
    defer_pieces(Whole(Some(Box::new(v)), crate::nostd::mem::size_of::<T>()));
}

/// Keeps `v` to free later, a piece at a time.
pub(crate) fn defer_pieces<T: Piecewise + 'static>(v: T) {
    let bytes = v.bytes();
    DEFERRED.with(|d| d.borrow_mut().push((Box::new(v), bytes)));
}

/// What compiles on this thread deferred since this was last called: the compiling state
/// takes it (to charge and free; `Lua::adopt_deferred`).
pub(crate) fn take_deferred() -> Deferred {
    DEFERRED.with(|d| crate::nostd::mem::take(&mut *d.borrow_mut()))
}

crate::thread_local! {
    /// Whether what the running compile is done with is freed later (`release`): for a
    /// compile from the host, whose next call frees it.
    static DEFERRING: crate::nostd::cell::Cell<bool> = const { crate::nostd::cell::Cell::new(false) };
}

/// Sets whether `release` defers, for the compile about to run: what it was.
pub(crate) fn set_deferring(on: bool) -> bool {
    DEFERRING.with(|d| d.replace(on))
}

/// Frees `v` now, or (in a compile from the host) later, a piece at a time.
pub(crate) fn release<T: Piecewise + 'static>(v: T) {
    if DEFERRING.with(crate::nostd::cell::Cell::get) {
        defer_pieces(v);
    } else {
        // Inside a call: now, but a piece at a time between looks (a `load` in a call of
        // 600,000 string constants freed its maps in one 30 ms stretch).
        free_watched(v);
    }
}

/// Keeps a syntax tree of about `bytes` to free later, a node at a time.
pub(crate) fn defer_tree(b: crate::ast::Block, bytes: usize) {
    defer_pieces(crate::ast::Dropper::of_block(b).with_bytes(bytes));
}

/// Frees `list` a piece at a time until `stop` says to stop (the host's interrupt): what
/// is left is freed at a later call. The bytes of each item freed whole: released.
pub(crate) fn free_deferred(list: &mut Deferred, stop: impl Fn() -> bool) -> usize {
    let mut released = 0usize;
    while let Some((mut v, mut bytes)) = list.pop() {
        while !v.free_some(FREE_PIECES) {
            // What each piece gave back is released as it goes (never more than charged).
            let now = v.bytes().min(bytes);
            released = released.saturating_add(bytes - now);
            bytes = now;
            settle_frees();
            if stop() {
                list.push((v, bytes));
                return released;
            }
        }
        drop(v);
        released = released.saturating_add(bytes);
        settle_frees();
        if stop() {
            return released;
        }
    }
    released
}

/// Frees `v` now, a piece at a time, each a step of the running compile; told to stop,
/// it leaves the rest to `free_deferred`.
pub(crate) fn free_watched<T: Piecewise + 'static>(mut v: T) {
    while !v.free_some(FREE_PIECES * 4) {
        settle_frees();
        if compile_ticks(COMPILE_WATCH_EVERY) {
            defer_pieces(v);
            return;
        }
    }
}

/// Asks the allocator for a block larger than the small ones just freed, and gives it back:
/// an allocator that keeps small freed blocks on quick lists (glibc's) merges them on such
/// a request, so it does that for each piece now, not for all of a tree at once at some
/// later allocation (S6-FRONT-4: the first sizeable allocation after freeing a 3M-node
/// tree took 300 ms).
fn settle_frees() {
    drop(crate::nostd::hint::black_box(Vec::<u8>::with_capacity(4096)));
}

/// Runs a compile under `watch`, with at most `max_work` units of scan work: the result,
/// the limit that stopped it (if one did), and the scan work done.
pub(crate) fn watched<T>(
    watch: crate::stdlib::pattern::Watch,
    max_work: u64,
    f: impl FnOnce() -> T,
) -> (T, Option<crate::LimitKind>, u64) {
    let outer = COMPILE_WATCH.with(|w| w.replace(Some((watch, None))));
    let outer_work = COMPILE_WORK.with(|w| w.replace((0, max_work)));
    let outer_stop = COMPILE_STOP.with(|s| s.replace(false));
    let r = f();
    COMPILE_STOP.with(|s| s.set(outer_stop));
    let (work, _) = COMPILE_WORK.with(|w| w.replace(outer_work));
    let stop = COMPILE_WATCH.with(|w| w.replace(outer)).and_then(|(_, k)| k);
    (r, stop, work)
}

/// Scan work in the compiler that grows with the program, not with its length: looking a
/// name up among the active locals and upvalues, labels and pending gotos, and the like.
/// (With no limit on locals, labels or gotos, these made compiling quadratic: 40,000
/// locals took 5 s against about 2M units of fuel for `load`'s up-front charge.) Counted
/// and paid for as fuel by the caller; past the most a compile may do, it stops on the
/// fuel limit at the next step. Every `COMPILE_WORK_PER_TICK` units are also a step
/// between looks at the host's interrupt and deadline.
#[inline]
pub(crate) fn compile_work(n: usize) {
    let steps = COMPILE_WORK.with(|w| {
        let (before, max) = w.get();
        let done = before.saturating_add(n as u64);
        w.set((done, max));
        if done > max {
            COMPILE_STOP.with(|s| s.set(true));
            COMPILE_WATCH.with(|c| {
                if let Some((_, stop)) = c.borrow_mut().as_mut() {
                    stop.get_or_insert(crate::LimitKind::Fuel);
                }
            });
        }
        done / COMPILE_WORK_PER_TICK - before / COMPILE_WORK_PER_TICK
    });
    if steps > 0 {
        // A stop found here is kept (`COMPILE_STOP`): the next step reports it.
        compile_ticks(u32::try_from(steps).unwrap_or(u32::MAX));
    }
}

/// Whether the running compile was told to stop (by its watch or its scan work).
#[inline]
pub(crate) fn compile_stopped() -> bool {
    COMPILE_STOP.with(crate::nostd::cell::Cell::get)
}

/// One step of the lexer, parser or compiler: true when the host's interrupt or deadline
/// says to stop (looked at every `COMPILE_WATCH_EVERY` steps).
#[inline]
pub(crate) fn compile_tick() -> bool {
    compile_ticks(1)
}

/// `n` steps of the lexer, parser or compiler at once (a token that took long to read):
/// true when the host's interrupt or deadline says to stop.
#[inline]
pub(crate) fn compile_ticks(n: u32) -> bool {
    let (before, t) = COMPILE_TICKS.with(|c| {
        let before = c.get();
        let t = before.wrapping_add(n.max(1));
        c.set(t);
        (before, t)
    });
    if t / COMPILE_WATCH_EVERY == before / COMPILE_WATCH_EVERY {
        // Past its scan work, or told to stop during it, a compile stops at once.
        return COMPILE_STOP.with(crate::nostd::cell::Cell::get);
    }
    let stop = COMPILE_WATCH.with(|w| {
        let mut w = w.borrow_mut();
        let Some((watch, stop)) = w.as_mut() else { return false };
        if stop.is_none() {
            *stop = watch.stop();
        }
        stop.is_some()
    });
    if stop {
        COMPILE_STOP.with(|s| s.set(true));
    }
    stop
}
