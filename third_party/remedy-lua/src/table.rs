//! Tables, sized exactly as C Lua sizes them (`ltable.c`), so `#t` on a table with holes
//! finds C Lua's border: an array part for the keys `1..=arr.len()` (Lua's real array
//! size, nil slots included) with Lua's border hint (`alimit`), and two hash parts.
//!
//! - Integer keys outside the array part live in `IntPart`, laid out exactly as Lua lays
//!   out its node array (main position `k mod (size-1)|1`, chains, Brent's moves,
//!   `lastfree`). While every hash key is an integer this part IS Lua's node array, so a
//!   new key rehashes exactly when Lua's does. Neighbouring keys take neighbouring slots
//!   (the cache stays warm); a script can make keys collide, so every chain step past the
//!   first is fuel (`PROBES`).
//! - Every other key lives in `nodes`, Lua's chained scatter table (main position, chains
//!   of the keys that share it, Brent's moves, free nodes from the top) under a seeded
//!   hash, with two homes per key (a key joins the shorter of their chains): a lookup walks
//!   only the keys of its two homes, which random keys keep under `CHAIN_FREE` steps past
//!   each home at any size (S5GC-5, S5B-OPEN-2); a longer walk is fuel, and keys aimed at
//!   one pair of homes reseed the part (see `CHAIN_FREE`). Once such a key is in the hash part,
//!   Lua's own placement depends on its random seed, so only the count of Lua's keyed
//!   nodes is kept (`ckeys` of `csize`: a new key rehashes when all are keyed), and the
//!   integer keys of the hash part move to `nodes` too. C Lua itself may rehash such a
//!   table at different times run to run, so its border after deletions is not
//!   reproducible there either.
//!
//! Removing a key leaves it in place with a nil value (Lua finds it again and rewrites it
//! in place), so `next` keeps working while a traversal clears fields.

#[allow(unused_imports)]
use crate::nostd::prelude::*;
use crate::heap::Mem;
use crate::number;
use crate::value::{TableRef, Value, hash_key, normalize_key};

/// A hash slot: a key (packed: its payload and type, so a lookup compares two words),
/// whether the key is in its main position (its home: the head of its chain), the offset
/// of the next node of its chain in `Table::nodes` (0 at a chain's end; the integer part
/// keeps its links beside the nodes), and the value. A node whose key is nil was never
/// used; one with a key and a nil value lost its value (lookups still find it).
///
/// In `Table::nodes` (two homes per key, see `CHAIN_FREE`): `flags` holds `SPILL` (of the
/// slot, whatever node is in it: a key whose first home is this slot lives in its second
/// home's chain), `SECOND` (of the node: its key lives in its second home's chain) and
/// `CHAINED` (a chain's head with a node after it), and a chain's head keeps the chain's
/// length in `clen` (saturating).
#[derive(Clone, Copy, Default)]
pub(crate) struct Node {
    kbits: u64,
    ktag: u8,
    main: bool,
    flags: u8,
    clen: u8,
    next: i32,
    pub val: Value,
}

/// `Node::flags`: some key whose first home is this slot lives in its second home's chain
/// (a lookup that misses the first chain goes on to the second only then). Belongs to the
/// slot: kept when another node is put there (`Table::put`).
const SPILL: u8 = 1;
/// `Node::flags`: this node's key lives in its second home's chain (moves with the node).
const SECOND: u8 = 2;
/// `Node::flags`: this node heads a chain of two or more (heads never move). With `SPILL`,
/// the one byte a lookup that misses the first home node reads to know it is done.
const CHAINED: u8 = 4;

/// A key's second home in a part of `mask + 1` slots (a power of two, at least 2), from its
/// hash `h` and first home `i` (`h & mask`): another slot (the high half of the hash picks
/// it among those of the other parity).
#[inline]
fn second_home(i: usize, h: u64, mask: usize) -> usize {
    i ^ ((((h >> 32) as usize) & mask) | 1)
}

const _: () = assert!(crate::nostd::mem::size_of::<Node>() == NODE_SIZE);

/// A key's type as a node packs it (`Value`'s variant order; 0, nil, is a node never used).
const K_NIL: u8 = 0;

/// An object reference's bits as a node packs them.
#[inline]
fn ref_bits(idx: u32, generation: u32) -> u64 {
    u64::from(idx) | (u64::from(generation) << 32)
}

/// A string key packed (`pack`), for the field paths.
#[inline]
fn pack_str(r: crate::value::StrRef) -> (u8, u64) {
    (4, ref_bits(r.idx, r.generation))
}

/// A normalized key packed as a node holds it: equal keys (raw equality of normalized
/// keys: an integer and a float never meet, a float key is never integral, -0.0 or NaN)
/// pack to equal pairs.
#[inline]
fn pack(v: Value) -> (u8, u64) {
    let r = ref_bits;
    match v {
        Value::Nil => (K_NIL, 0),
        Value::Bool(b) => (1, u64::from(b)),
        Value::Int(i) => (2, i as u64),
        Value::Num(f) => (3, f.to_bits()),
        Value::Str(x) => pack_str(x),
        Value::Table(x) => (5, r(x.idx, x.generation)),
        Value::Func(x) => (6, r(x.idx, x.generation)),
        Value::UserData(x) => (7, r(x.idx, x.generation)),
        Value::Thread(x) => (8, r(x.idx, x.generation)),
    }
}

/// The key a packed pair stands for (`pack` backwards).
#[inline]
fn unpack((tag, bits): (u8, u64)) -> Value {
    let (idx, generation) = (bits as u32, (bits >> 32) as u32);
    match tag {
        1 => Value::Bool(bits != 0),
        2 => Value::Int(bits as i64),
        3 => Value::Num(f64::from_bits(bits)),
        4 => Value::Str(crate::value::StrRef { idx, generation }),
        5 => Value::Table(TableRef { idx, generation }),
        6 => Value::Func(crate::value::FuncRef { idx, generation }),
        7 => Value::UserData(crate::value::UdRef { idx, generation }),
        8 => Value::Thread(crate::value::ThreadRef { idx, generation }),
        _ => Value::Nil,
    }
}

impl Node {
    #[inline]
    fn new(key: Value, val: Value) -> Node {
        let (ktag, kbits) = pack(key);
        Node { kbits, ktag, main: false, flags: 0, clen: 0, next: 0, val }
    }

    /// The key (nil for a node never used).
    #[inline]
    pub fn key(&self) -> Value {
        unpack((self.ktag, self.kbits))
    }

    /// Never used (no key).
    #[inline]
    fn is_free(&self) -> bool {
        self.ktag == K_NIL
    }

    /// Holds normalized key `(tag, bits)` (packed).
    #[inline]
    fn holds(&self, (tag, bits): (u8, u64)) -> bool {
        self.ktag == tag && self.kbits == bits
    }

    /// Holds the integer key `k`.
    #[inline]
    fn holds_int(&self, k: i64) -> bool {
        self.holds((2, k as u64))
    }
}

/// Bytes a node and an array slot take.
pub(crate) const NODE_SIZE: usize = 32;
pub(crate) const SLOT_SIZE: usize = 16;
// A slot is a `Value` (two words, whatever its tag's width: `value.rs`).
const _: () = assert!(crate::nostd::mem::size_of::<Value>() == SLOT_SIZE);
pub(crate) const TABLE_BASE: usize = 152;
// The charge covers the table's own slab slot (object, generation and color bytes).
const _: () = assert!(TABLE_BASE >= crate::nostd::mem::size_of::<Table>() + 5);
/// Bytes an integer-part node takes (a node and its chain link).
const INODE_SIZE: usize = NODE_SIZE + 4;

/// Parts of `nodes` of at most this many slots keep no `LiveBits`: `next` scans them whole.
const LIVE_MIN: usize = 64;

/// Words of `LiveBits` for a part of `len` slots (every level).
fn live_words(len: usize) -> usize {
    if len <= LIVE_MIN {
        return 0;
    }
    let (mut n, mut total) = (len, 0);
    loop {
        n = n.div_ceil(64);
        total += n;
        if n <= 1 {
            return total;
        }
    }
}

/// Bytes a part of `nodes` of `len` slots takes: its nodes and its `LiveBits`.
#[inline]
fn part_bytes(len: usize) -> usize {
    len * NODE_SIZE + live_words(len) * 8
}

/// S6T-2: a bit per node of `Table::nodes` that may hold a value, for `next`: set by every
/// write that gives a node a value (a new key, a key's value put back after it was removed,
/// a node moved), and cleared only by `next` when it finds the value gone, so a node that
/// holds a value always has its bit. Each level above the first has a bit per word of the
/// one below that is not zero (exactly), so `next` skips 64 slots per word of a level and
/// a gap between keys costs a few words whatever its length. Clearing a removed key's bit
/// once is paid by the write that removed it.
#[derive(Default)]
pub(crate) struct LiveBits {
    levels: Vec<Vec<crate::nostd::cell::Cell<u64>>>,
    /// The node `next` last returned: `next` from its key looks there first (self-checking:
    /// finding the key again was a cache miss a step over a big part, 10x plain code per
    /// unit, LR-32). Kept here, with the part's bits: a small part is found in its caches.
    hint: crate::nostd::cell::Cell<u32>,
}

impl LiveBits {
    /// The bits of an empty part of `len` slots (none at `LIVE_MIN` or fewer).
    pub(crate) fn for_len(len: usize) -> Option<Box<LiveBits>> {
        if len <= LIVE_MIN {
            return None;
        }
        let mut levels = Vec::new();
        let mut n = len;
        loop {
            n = n.div_ceil(64);
            levels.push(vec![crate::nostd::cell::Cell::new(0u64); n]);
            if n <= 1 {
                break;
            }
        }
        Some(Box::new(LiveBits { levels, hint: crate::nostd::cell::Cell::new(0) }))
    }

    /// Node `i` may hold a value.
    #[inline]
    fn set(&self, mut i: usize) {
        for level in &self.levels {
            let Some(w) = level.get(i / 64) else { return };
            let (v, bit) = (w.get(), 1u64 << (i % 64));
            if v & bit != 0 {
                // (Set here, so in every level above too.)
                return;
            }
            w.set(v | bit);
            i /= 64;
        }
    }

    /// Node `i` holds no value.
    fn clear(&self, mut i: usize) {
        for level in &self.levels {
            let Some(w) = level.get(i / 64) else { return };
            let v = w.get() & !(1u64 << (i % 64));
            w.set(v);
            if v != 0 {
                return;
            }
            i /= 64;
        }
    }

    /// The first node at or after `from` whose bit is set.
    fn next_set(&self, from: usize) -> Option<usize> {
        let mut l = 0;
        let mut p = from;
        loop {
            let level = self.levels.get(l)?;
            let w = level.get(p / 64)?.get();
            let m = w & (!0u64).checked_shl((p % 64) as u32).unwrap_or(0);
            if m != 0 {
                p = (p / 64) * 64 + m.trailing_zeros() as usize;
                // Down to the first level: each word below a set bit is not zero.
                while l > 0 {
                    l -= 1;
                    let w = self.levels.get(l)?.get(p)?.get();
                    if w == 0 {
                        return None;
                    }
                    p = p * 64 + w.trailing_zeros() as usize;
                }
                return Some(p);
            }
            // None left in this word: the next word, one level up.
            p = p / 64 + 1;
            l += 1;
        }
    }
}

/// What a key placed by a rehash costs in `resized` slots (a slot is 1/16 of a unit of
/// fuel): its hash or its main position's division, and a store at a random place.
///
/// S5B-COST-2: a rehash was charged a slot per slot of each part once, but it reads
/// every part several times (counting the keys, `computesizes`, the keys leaving the
/// array, collecting the movers) and then places every key at random. A table one key
/// short of a power of two rehashes at every new key (as C Lua's does: its sizes are
/// kept so `#` finds C Lua's border), so a loop adding and removing one new key ran at
/// 8x plain code per unit of fuel with 4,000 integer keys and 30x with a million (47x
/// with a string key put in and taken out each time, which moves the integer keys to
/// the other part and back). Every pass is charged now (`rehash`, `resize`,
/// `to_counting`, `rebuild_nodes`), and each key placed `PLACE_COST` slots. The counts
/// follow only what the table holds, never the hash layout, so fuel stays the same run
/// to run.
const PLACE_COST: usize = 8;

/// `PLACE_COST` for a key placed in a part of `nodes` of `slots` slots. S6T-3: a key goes
/// to a random place (its homes, a free node), a cache miss or two once the part is past
/// the caches: a reseed of a part of 2^21 slots (64 MB) took 70 to 145 ns a key against
/// the 12 ns it was charged, so 11 keys aimed at one pair of homes put in and taken out
/// of a table of 1.5 million keys ran at 6 to 11x plain code per unit of fuel (2.4x at
/// 2^16 slots, 3.1x at 2^18, 5.7x at 2^20); at most 1.8x now. The size is the program's,
/// never the seed's, so the fuel still is. (12 from 2^20 slots, not 6: under a loaded
/// machine the storm at 2^21 reached 3.2x at 6 and 3.0x at 8; a placement there takes about
/// 100 ns, a cache miss, and the new part's pages are faulted in, so `place_again` charges
/// the part by the same factor.)
#[inline]
/// What a rebuild of `nodes` from `old` to `len` slots charges to `resized`: the old part
/// read (`rebuild_nodes`) and the placements (`place_again`).
fn rebuild_cost(old: usize, len: usize) -> usize {
    old + len * (place_cost(len) / PLACE_COST) + place_cost(len) * (old * 3 / 4)
}

fn place_cost(slots: usize) -> usize {
    PLACE_COST
        * match slots {
            s if s >= 1 << 20 => 12,
            s if s >= 1 << 18 => 4,
            s if s >= 1 << 16 => 2,
            _ => 1,
        }
}

/// Probe steps an integer key's lookup in a `nodes` part of `slots` slots counts
/// (`Table::int_in_nodes`): none while the part stays in the caches.
/// (Three past 2^20 slots: two read 4.1x plain code under load, past the 4x bound of
/// `perf_tables::moves_over_a_big_part_of_other_keys_cost_what_they_are_charged`.)
#[inline]
fn nodes_lookup_cost(slots: usize) -> u64 {
    match slots {
        s if s >= 1 << 20 => 3,
        s if s >= 1 << 17 => 1,
        _ => 0,
    }
}

/// `PLACE_COST` for a key a rehash places in an integer part of `slots` slots. Neighbouring
/// keys go to neighbouring slots, so a placement misses the caches less often than in
/// `nodes` (`place_cost`), but the part is a buffer mapped afresh at every rehash: past the
/// caches each key pays for its share of the page faults and of writing a part that no
/// longer fits. A table of a million integer keys one short of a power of two (it
/// rehashes at every new key) ran at 1.9 to 2.4x plain code per unit of fuel with the
/// fast loop here and 3.0x on a machine with slower memory (the residual of S5B-COST-2);
/// with these costs, about 0.8x here. The size is the program's, so the fuel still is.
#[inline]
fn int_place_cost(slots: usize) -> usize {
    PLACE_COST
        * match slots {
            s if s >= 1 << 20 => 4,
            s if s >= 1 << 18 => 3,
            s if s >= 1 << 16 => 2,
            _ => 1,
        }
}

crate::thread_local! {
    /// The longest chain walk in an integer part (steps past the first node) of any
    /// lookup, insert or `next` on this thread since it was last charged: the interpreter
    /// charges it with each instruction, the table library after each element
    /// (`take_probes`). The longest rather than the sum, so a lookup the full path repeats
    /// (or the fast loop tried first) costs the same either way; an instruction does a
    /// bounded number of lookups, so the charge stays proportional to the work. Integer
    /// layout depends only on the program, so the fuel is the same run to run.
    static PROBES: crate::nostd::cell::Cell<u64> = const { crate::nostd::cell::Cell::new(0) };
    /// Slots of new array tails made on this thread (`grow_arr`), for `Lua::table_slots_staged`.
    static STAGED: crate::nostd::cell::Cell<u64> = const { crate::nostd::cell::Cell::new(0) };
    /// Slots of an array part copied into a new buffer on this thread (a growth's reserve),
    /// and the array parts made segmented, for `Lua::table_slots_copied`.
    static COPIED: crate::nostd::cell::Cell<(u64, u64)> = const { crate::nostd::cell::Cell::new((0, 0)) };
}

crate::thread_local! {
    /// Slots read under a fuel watch on this thread, a `FUEL_PIECE` at a time (a probe).
    static WATCHED: crate::nostd::cell::Cell<u64> = const { crate::nostd::cell::Cell::new(0) };
}

/// The slots read under a fuel watch since the last call (a test probe).
pub(crate) fn take_watched() -> u64 {
    WATCHED.with(crate::nostd::cell::Cell::take)
}

/// The slots copied by growths and the array parts made segmented since the last call (a
/// test probe).
pub(crate) fn take_copied() -> (u64, u64) {
    COPIED.with(crate::nostd::cell::Cell::take)
}

#[cfg(any(target_family = "wasm", feature = "segmented-arrays"))]
fn count_segmented() {
    COPIED.with(|c| c.set((c.get().0, c.get().1 + 1)));
}

fn count_copied(n: usize) {
    COPIED.with(|c| c.set((c.get().0 + n as u64, c.get().1)));
}

/// The slots of new array tails made since the last call (a test probe).
pub(crate) fn take_staged() -> u64 {
    STAGED.with(crate::nostd::cell::Cell::take)
}

/// The uncharged probe steps (see `PROBES`), reset to zero.
#[inline]
pub(crate) fn take_probes() -> u64 {
    PROBES.with(|p| {
        let n = p.get();
        if n != 0 {
            p.set(0);
        }
        n
    })
}

/// Puts back probe steps `take_probes` took (an entry from the host keeps the steps of a
/// state further out, whose host function it runs in, apart from its own: S6-RES-4).
pub(crate) fn put_probes(n: u64) {
    PROBES.with(|p| p.set(n));
}

/// Runs host-side table access (`Lua::table_get` and the like, which do not enter the
/// state) without billing its probe steps to whatever script runs on this thread.
pub(crate) fn host_side<R>(f: impl FnOnce() -> R) -> R {
    let saved = take_probes();
    let r = f();
    put_probes(saved);
    r
}

#[cold]
fn add_probes(n: u64) {
    PROBES.with(|p| p.set(p.get().max(n)));
}

/// Probe steps that add up rather than taking the longest (`PROBES` keeps the longest
/// lookup): the string interner's long walks (S5GC-3), each a different string made or
/// looked up. Charged with the other probe steps.
#[cold]
pub(crate) fn add_summed(n: u64) {
    PROBES.with(|p| p.set(p.get().saturating_add(n)));
}

/// Probe steps of several lookups that each count (S5GC-1): `PROBES` holds the longest
/// of one lookup, so a native or an `__index` chain that looks up many keys in one
/// charge summed nothing (a sort of 64 keys at the end of integer chains of 500 ran at
/// 30x plain code per unit of fuel; an `__index` loop through a table at 100x). Runs `f`
/// with each lookup's steps (`take_probes` after it) added up; the total is left as the
/// one lookup's, so a repeat of the whole (the fast loop tried first) counts once.
pub(crate) fn summed<R>(f: impl FnOnce(&mut dyn FnMut()) -> R) -> R {
    let before = take_probes();
    let mut total = 0u64;
    let r = f(&mut || total = total.saturating_add(take_probes()));
    let total = total.saturating_add(take_probes());
    PROBES.with(|p| p.set(before.max(total)));
    r
}

/// Whether `x` is a power of two (or zero), as Lua's `ispow2`.
#[inline]
fn ispow2(x: usize) -> bool {
    x & x.wrapping_sub(1) == 0
}

/// Lua's node count for `n` hash keys: 0, or the power of two at or above `n`
/// (`setnodevector`).
#[inline]
fn lua_nodes(n: usize) -> usize {
    if n == 0 { 0 } else { n.next_power_of_two() }
}

/// Largest key Lua's array part takes (`MAXASIZE` on 64-bit: 2^31).
const MAXASIZE: u64 = 1 << 31;

/// Counts integer key `k` in `nums` (by `ceil(log2(k))`) when it may live in an array
/// part (`countint`).
#[inline]
fn countint(k: i64, nums: &mut [usize; 33]) -> bool {
    let u = k as u64;
    if u.wrapping_sub(1) < MAXASIZE {
        let b = (64 - (u - 1).leading_zeros()) as usize;
        if let Some(c) = nums.get_mut(b) {
            *c += 1;
        }
        true
    } else {
        false
    }
}

/// Takes back a `countint` that counted `k`.
fn uncount(k: i64, nums: &mut [usize; 33]) {
    let b = (64 - ((k as u64) - 1).leading_zeros()) as usize;
    if let Some(c) = nums.get_mut(b) {
        *c -= 1;
    }
}

/// Lua's `computesizes`: the array size (the largest power of two more than half used),
/// and in `na` how many keys go there.
fn computesizes(nums: &[usize; 33], na: &mut usize) -> usize {
    let (mut a, mut n, mut optimal) = (0usize, 0usize, 0usize);
    let mut twotoi: u32 = 1;
    let mut i = 0usize;
    while twotoi > 0 && *na > (twotoi / 2) as usize {
        a += nums.get(i).copied().unwrap_or(0);
        if a > (twotoi / 2) as usize {
            optimal = twotoi as usize;
            n = a;
        }
        i += 1;
        twotoi = twotoi.wrapping_mul(2);
    }
    *na = n;
    optimal
}

/// Lua's `binsearch` in the array part: a border in `[i, j)`, `arr[i-1]` present (or `i`
/// 0) and `arr[j-1]` absent.
fn binsearch(arr: &ArrPart, mut i: usize, mut j: usize) -> usize {
    while j - i > 1 {
        let m = (i + j) / 2;
        if arr.get(m - 1).is_none_or(|v| v.is_nil()) {
            j = m;
        } else {
            i = m;
        }
    }
    i
}

/// Hash slots the `nodes` part gets for `n` keys.
#[inline]
pub(crate) fn hash_slots(n: usize) -> usize {
    if n == 0 { 0 } else { (n * 4 / 3 + 1).next_power_of_two().max(4) }
}

/// The chain steps past a home node a lookup in `nodes` takes free, in each of the key's
/// two chains (S5GC-5, S5B-OPEN-2); each step past them is fuel, and an insert that makes
/// a chain longer than `CHAIN_FREE + 1` reseeds the part.
///
/// Every key has two homes (`second_home`: the other one of a pair picked by the hash) and
/// joins the shorter of their chains (the first on a tie; `SPILL` marks the first home of
/// a key that went to its second, so a lookup that misses goes on only then). A chain holds
/// exactly the keys that chose its head (Brent's moves keep it so, and a node's key never
/// changes until the part is rebuilt), so for keys a script cannot aim (the seeded hash) the
/// chains are the bins of two-choice balls-into-bins at a load of at most 3/4 (removed keys
/// included), whose tail is doubly exponential: a home has a chain of `i + 1` only if both
/// homes of a key had `i`, so the share of chains of `i + 1` is at most 3/4 of the square of
/// the share of `i`. Measured with the real mixer (2^27 inserts at every size from 2^2 to
/// 2^22 slots; S5B-OPEN-2): no chain past 4, chains of 4 made by about
/// 3 inserts in 10^7, so a chain of 5 by about 10^-13 to 10^-14 of inserts and one of
/// `CHAIN_FREE + 2` = 6 by about 10^-26 or less: random keys never walk a charged step or
/// reseed, and fuel stays independent of the seed (`tests::chain_lengths_of_random_keys`).
///
/// One chain (Lua's own layout, sweep 5b) has a binomial tail instead: random keys reached
/// chains of 9-11, so 16 steps had to be free, and keys aimed at one home (a known seed:
/// every table of a state shares it) put into many tables cost 7-20x plain code per unit
/// of fuel through `__index` chains, a step being a dependent load on cold memory. Two
/// chains of 5 are walked in step with each other (their loads overlap), so the walk a
/// lookup takes free is 5 dependent loads at most.
pub(crate) const CHAIN_FREE: usize = 4;

/// Slots after a key's home searched for a free node before the top (`Table::free_pos`).
const NEAR_FREE: usize = 8;

/// Bytes a new table sized for `arr` array and `hash` hash entries is charged.
#[inline]
pub(crate) fn new_cost(arr: usize, hash: usize) -> usize {
    TABLE_BASE + arr * SLOT_SIZE + part_bytes(hash_slots(hash))
}

/// A hash part's buffer back to the state's pool (`Pool::give` takes a table's buffers).
fn give_nodes(mem: &mut Mem, nodes: Vec<Node>) {
    if nodes.capacity() * NODE_SIZE >= crate::heap::PARK_BYTES {
        mem.park(crate::heap::Parked::Nodes(nodes));
    } else if nodes.capacity() > 0 {
        let mut t = Table { nodes, ..Table::default() };
        mem.pool_give(&mut t);
    }
}

/// An array buffer back to the state's pool.
fn give_arr(mem: &mut Mem, arr: Vec<Value>) {
    if arr.capacity() > 0 {
        let mut t = Table { arr: arr.into(), ..Table::default() };
        mem.pool_give(&mut t);
    }
}

/// `arr` moved into a buffer of exactly `cap` slots from the pool (the old one goes back).
/// A buffer past the pool's sizes grows where it is instead (`realloc`): a new buffer and a
/// copy at each doubling faulted every page in afresh, and filling a million-slot array one
/// key at a time ran at 2.9x C Lua's time (whose `realloc` remaps a big block). The
/// capacity is exactly `cap` either way, so the account is the same.
fn regrow_arr(mem: &mut Mem, arr: &mut Vec<Value>, cap: usize) {
    if arr.capacity() > crate::heap::POOL_MAX {
        // (A `realloc`: the kernel moves a big one's pages natively; wasm copies them.)
        count_copied(arr.len());
        arr.reserve_exact(cap.saturating_sub(arr.len()));
        return;
    }
    let mut new = mem.pool_arr(cap);
    count_copied(arr.len());
    new.extend_from_slice(arr);
    give_arr(mem, crate::nostd::mem::replace(arr, new));
}

/// Slots of a resize's work between looks at the host's interrupt and deadline
/// (`Mem::stopped`): about half a megabyte of nodes. A resize of a million-key part, placed or
/// filled in one go, ran 15 to 64 ms with no look; a stop found between pieces gives the
/// resize up and leaves the table as it was (`SetError::Stopped`).
const RESIZE_PIECE: usize = 1 << 14;

/// What `Table::resize_scan` found: whether a key other than an integer has a value, the
/// live nodes of `nodes`, and the integer keys leaving the array part (all, and those of
/// them in `nodes`).
#[derive(Clone, Copy, Default)]
struct Scan {
    strings: bool,
    live_before: usize,
    ints_out: usize,
    nodes_ints_out: usize,
}

/// Between pieces of a resize (every `RESIZE_PIECE` steps `i`): the limit that stops it.
#[inline]
fn look(mem: &Mem, i: usize) -> Result<(), SetError> {
    if i % RESIZE_PIECE == RESIZE_PIECE - 1
        && let Some(k) = mem.stopped()
    {
        return Err(SetError::Stopped(k));
    }
    Ok(())
}

/// Slots between a growth's looks at its fuel under a fuel watch (`Mem::fuel_watch`): a
/// piece of `RESIZE_PIECE` can cost more than a call's fuel (1.2M units placing 2^21 slots).
const FUEL_PIECE: usize = 1 << 9;

/// A rehash's census, in units of `resized` a slot (16 a unit of fuel): what a slot read
/// takes against plain code, 2 to 4.7 ns against 0.18 ns a unit on the fast loop (SEC8-F5).
/// At 1 it read 320,000 slots in a call of 20,000 fuel,
/// 1 ms (SEC7-T4).
const CENSUS_UNITS: usize = 4;

/// The most `Table::resized` may reach in a call of `fuel` after attempts that paid `paid`:
/// what the call's checks allow (`resized / 16 > fuel` refused), less what is paid.
fn watch_most(fuel: usize, paid: usize) -> usize {
    fuel.saturating_mul(16).saturating_add(15).saturating_add(paid)
}

/// `look`, and under a fuel watch a stop before the next piece when its work would take
/// `Table::resized` past the watch: `units(at)` is the growth's charge stopped at `at`
/// (SEC8-F4: refused whole, a growth bigger than a call's fuel was never made). Checked
/// every `FUEL_PIECE` slots, and the clock only where `look` reads it.
#[inline]
fn look_w(mem: &Mem, i: usize, units: impl Fn(usize) -> usize) -> Result<(), SetError> {
    if i % FUEL_PIECE != FUEL_PIECE - 1 {
        return Ok(());
    }
    if let Some(most) = mem.fuel_watch {
        WATCHED.with(|w| w.set(w.get() + FUEL_PIECE as u64));
        if units(i + 1 + FUEL_PIECE) > most {
            return Err(SetError::Stopped(crate::LimitKind::Fuel));
        }
    }
    look(mem, i)
}

/// `look_w` in a pass made for a fuel watch (`W`), else `look`: a pass's loop without the
/// watch is the one it was (two copies of it, chosen before it starts).
#[inline(always)]
fn look_at<const W: bool>(mem: &Mem, i: usize, units: impl Fn(usize) -> usize) -> Result<(), SetError> {
    if W { look_w(mem, i, units) } else { look(mem, i) }
}

/// `Table::absent`: the table is unwritten since a resize was given up on it, so what that
/// resize made (`Table::partial`) is still exactly what it would make now. Every write
/// clears `absent` (it caches the metamethods a metatable lacks), so knowing costs no
/// write anything. (Events take bits 0 to 28.)
pub(crate) const RESUMABLE: u32 = 1 << 31;

/// What a resize given up between pieces (the interrupt or deadline) had done, for the next
/// one to go on from (sweep6's review: run to the end unwatched instead, a host slicing a
/// script into 1 ms calls saw one call of the whole resize, 64 ms for a million float keys).
/// Each part is checked against what it was made for, and the whole is used only while
/// the table is unwritten (`RESUMABLE`).
#[derive(Default)]
pub(crate) struct Partial {
    /// `hash_insert`'s count of the live nodes: the nodes counted, the live among them.
    count: Option<(usize, usize)>,
    /// `to_counting`'s count of the integer part's live keys under a fuel watch: how far it
    /// got and the live keys so far.
    ints_count: Option<(usize, usize)>,
    /// What the attempts paid includes the marking of the growth, paid ahead of it
    /// (`Table::prepay_marking`): the attempt that ends it goes the watched way, which
    /// counts that marking as paid.
    prepaid: bool,
    /// A new `nodes` (`place_again`).
    nodes: Option<NodesBuild>,
    /// The rehash's count of the keys (`Table::census`) and its integer keys by the power
    /// of two above them.
    census: Option<(Census, [usize; 33])>,
    /// `resize_scan` for an array part of the first size: how far it got, what it found.
    scan: Option<(usize, usize, Scan)>,
    /// `resize`'s new parts.
    resize: Option<ResizeBuild>,
    /// Charged for the work the attempts given up did (`Table::resumed`).
    paid: usize,
}

/// A resize being made (`Table::resize`), for an array part of `asize` slots and an
/// integer part of `len` (`exact`: every hash key an integer): the array's new buffer being
/// copied (its new tail is in the array's storage, hidden: `ArrPart`), the new integer part, the old keys read for
/// placing (`seen`), those placed in the integer part and their chain steps.
#[derive(Default)]
struct ResizeBuild {
    asize: usize,
    len: usize,
    exact: bool,
    copy: Option<Vec<Value>>,
    ints: Option<IntPart>,
    seen: usize,
    placed: usize,
    probes: u64,
    /// Charged for it (the new buffer's and the integer part's growth).
    bytes: usize,
}

/// The rehash's count of the keys (Lua's `numusearray` and `numusehash`): the slots read
/// (the array part, then `nodes`, then the integer part), the integer keys that fit an
/// array part, and all the keys.
#[derive(Clone, Copy, Default)]
struct Census {
    at: usize,
    na: usize,
    total: usize,
}

impl NodesBuild {
    fn new(len: usize, bytes: usize) -> NodesBuild {
        NodesBuild { len, nodes: Vec::new(), live: LiveBits::for_len(len), used: 0, free_top: 0, at: 0, longest: 0, bytes, ints_cost: 0 }
    }
}

/// What a resize has to go on from: none, unless it is going on from one given up. Each
/// step works in its own variables and leaves what it made here only when it is given up,
/// so a resize that is not costs nothing for it.
type Kept = Option<Box<Partial>>;

/// `Kept`, made if there was none (a step being given up).
#[cold]
fn stash(part: &mut Kept) -> &mut Partial {
    part.get_or_insert_with(Box::default)
}

/// A table's `Partial` kept in `Mem::partials`: the index past it.
fn keep_partial(mem: &mut Mem, p: Box<Partial>) -> u32 {
    let i = mem.partials.iter().position(Option::is_none).unwrap_or(mem.partials.len());
    if i == mem.partials.len() {
        mem.partials.push(None);
    }
    if let Some(slot) = mem.partials.get_mut(i) {
        *slot = Some(p);
    }
    (i + 1) as u32
}

/// The `Partial` at index past `h`, taken out.
fn take_partial(mem: &mut Mem, h: u32) -> Option<Box<Partial>> {
    mem.partials.get_mut((h as usize).wrapping_sub(1))?.take()
}

/// A table's `Partial` gone (the table died): its bytes released.
#[cold]
pub(crate) fn drop_partial(mem: &mut Mem, h: u32) {
    if let Some(p) = take_partial(mem, h) {
        mem.release(p.bytes());
    }
}

impl Partial {
    /// The bytes charged for the parts it holds.
    fn bytes(&self) -> usize {
        self.nodes.as_ref().map_or(0, |b| b.bytes) + self.resize.as_ref().map_or(0, |b| b.bytes)
    }
}

/// A new `nodes` being made: the part so far (`nodes` short of `len` while it is still
/// being made) and how many of the old part's nodes are placed in it (`at`).
struct NodesBuild {
    len: usize,
    nodes: Vec<Node>,
    live: Option<Box<LiveBits>>,
    used: u32,
    free_top: u32,
    at: usize,
    longest: usize,
    /// Charged for it (the growth from the old part's size).
    bytes: usize,
    /// Placing the integer part's keys too, after the old part's (`to_counting` under a
    /// fuel watch, SEC8-F4): their charge, 0 if not.
    ints_cost: usize,
}

/// A new `nodes` of `len` slots stopped with `made` slots made and `at` keys placed, of the
/// old part's `old_len` and then the integer part's `more` (charged `ints_cost` in all):
/// `rebuild_cost`'s terms as far as they got (SEC8-F4).
fn build_units(len: usize, old_len: usize, more: usize, ints_cost: usize, made: usize, at: usize) -> usize {
    let (a, pc) = (at.min(old_len), place_cost(len));
    made * (pc / PLACE_COST) + a + pc * (a * 3 / 4) + if more == 0 { 0 } else { ints_cost * (at - a) / more }
}

/// `v` made into `len` empty nodes (as `Pool::nodes`): a big part a piece at a time between
/// looks, going on from as far as a part given up got.
/// (`W`: under a fuel watch, made a `FUEL_PIECE` at a time, each one the call can pay for,
/// `units` the charge with so many made, the clock read as without the watch.)
fn make_nodes<const W: bool>(v: &mut Vec<Node>, len: usize, mem: &mut Mem, units: impl Fn(usize) -> usize) -> Result<(), SetError> {
    if v.is_empty() && len <= RESIZE_PIECE && !W {
        *v = mem.pool_nodes(len);
        return Ok(());
    }
    v.reserve_exact(len - v.len());
    while v.len() < len {
        let n = (len - v.len()).min(if W { FUEL_PIECE } else { RESIZE_PIECE });
        if W && mem.fuel_watch.is_some_and(|most| units(v.len() + n) > most) {
            return Err(SetError::Stopped(crate::LimitKind::Fuel));
        }
        let before = v.len();
        v.extend(crate::nostd::iter::repeat_n(Node::default(), n));
        if v.len() < len
            && (!W || v.len() / RESIZE_PIECE != before / RESIZE_PIECE)
            && let Some(k) = mem.stopped()
        {
            return Err(SetError::Stopped(k));
        }
    }
    Ok(())
}

/// `regrow_arr` between looks: a buffer of 1 to 32 MiB is copied into the new one a piece
/// at a time (`realloc` copies one that size anyway: the allocator maps only bigger ones,
/// which it moves without a copy, and 16 MiB copied in one go was 7 ms with no look). Given
/// up, `arr` is as it was.
fn regrow_arr_watched(mem: &mut Mem, arr: &mut Vec<Value>, cap: usize, copy: &mut Option<Vec<Value>>) -> Result<(), SetError> {
    let bytes = arr.capacity().saturating_mul(SLOT_SIZE);
    if !(1 << 20..32 << 20).contains(&bytes) {
        regrow_arr(mem, arr, cap);
        return Ok(());
    }
    // (Going on from a copy given up, at least a piece each time.)
    let kept = copy.take().filter(|v| v.capacity() == cap);
    // The old buffer stays beside the new one until the copy ends: counted meanwhile (as
    // slack, so the collector paces as before), and a growth both do not fit in is refused
    // before anything is copied (uncounted, an uninterrupted 16 MiB copy held 32 MiB
    // against an account of 16). A copy given up and gone on with is counted with it already
    // (`resize`'s `held`).
    let old = if kept.is_some() { 0 } else { bytes };
    if !mem.fits(old) {
        return Err(SetError::Memory);
    }
    mem.slack_in(old);
    let mut new = kept.unwrap_or_else(|| Vec::with_capacity(cap));
    let start = new.len();
    while new.len() < arr.len() {
        if new.len() > start
            && let Some(k) = mem.stopped()
        {
            *copy = Some(new);
            // (Given up, `resize` counts the copy whole beside the old buffer.)
            mem.slack_out(old);
            return Err(SetError::Stopped(k));
        }
        let end = (new.len() + RESIZE_PIECE).min(arr.len());
        count_copied(end - new.len());
        new.extend_from_slice(arr.get(new.len()..end).unwrap_or_default());
    }
    // (The old buffer freed whole: a free, not a copy.)
    *arr = new;
    mem.slack_out(old);
    Ok(())
}

/// The integer hash part, laid out as Lua's node array: `next` links (offsets, 0 at a
/// chain's end) beside the nodes, and `lastfree` going down to the free ones. A node whose
/// key is nil was never used; one with a key and a nil value lost its value (Lua still
/// finds it, and may give its place to a new key whose main position it is).
#[derive(Default)]
pub(crate) struct IntPart {
    pub nodes: Vec<Node>,
    pub next: Vec<i32>,
    pub lastfree: usize,
}

impl IntPart {
    fn with_len(n: usize, mem: &mut Mem) -> IntPart {
        IntPart { nodes: mem.pool_nodes(n), next: vec![0; n], lastfree: n }
    }

    /// Lua's main position of integer key `k` (`hashint`).
    #[inline]
    fn main(&self, k: i64) -> usize {
        ((k as u64) % (((self.nodes.len() - 1) | 1) as u64)) as usize
    }

    /// The node holding key `k`, with or without a value (chain steps counted).
    #[inline]
    fn find(&self, k: i64) -> Option<usize> {
        if self.nodes.is_empty() {
            return None;
        }
        let mut j = self.main(k);
        let mut steps = 0u64;
        let r = loop {
            let n = self.nodes.get(j)?;
            if n.holds_int(k) {
                break Some(j);
            }
            let nx = self.next.get(j).copied().unwrap_or(0);
            if nx == 0 {
                break None;
            }
            j = (j as i64 + i64::from(nx)) as usize;
            steps += 1;
        };
        if steps != 0 {
            add_probes(steps);
        }
        r
    }

    /// Lua's `luaH_newkey` for a key `k` known absent: false when there is no free node
    /// (Lua rehashes then; only `lastfree` moved).
    /// An entry the colliding node held moves to another node: its value is left in
    /// `moved` (for the collector's `Watch`).
    fn place(&mut self, k: i64, val: Value, moved: &mut Option<Value>) -> bool {
        if self.nodes.is_empty() {
            return false;
        }
        let mut mp = self.main(k);
        if self.nodes.get(mp).is_some_and(|n| !n.val.is_nil()) {
            // `getfreepos`: down from `lastfree` to a node never used.
            let f = loop {
                if self.lastfree == 0 {
                    return false;
                }
                self.lastfree -= 1;
                if self.nodes.get(self.lastfree).is_some_and(Node::is_free) {
                    break self.lastfree;
                }
            };
            let Value::Int(ck) = self.nodes.get(mp).map_or(Value::Nil, Node::key) else { return false };
            let other = self.main(ck);
            let mpnext = self.next.get(mp).copied().unwrap_or(0);
            if other != mp {
                // The colliding node is out of its main position: it moves to `f`, the new
                // key takes `mp`.
                let mut o = other;
                let mut steps = 0u64;
                loop {
                    let nx = self.next.get(o).copied().unwrap_or(0);
                    let to = (o as i64 + i64::from(nx)) as usize;
                    if nx == 0 || to == mp {
                        break;
                    }
                    o = to;
                    steps += 1;
                }
                if steps != 0 {
                    add_probes(steps);
                }
                if let Some(x) = self.next.get_mut(o) {
                    *x = (f as i64 - o as i64) as i32;
                }
                let m = self.nodes.get(mp).copied().unwrap_or_default();
                *moved = Some(m.val);
                if let Some(n) = self.nodes.get_mut(f) {
                    *n = m;
                }
                let fnext = if mpnext != 0 { mpnext + (mp as i64 - f as i64) as i32 } else { 0 };
                if let Some(x) = self.next.get_mut(f) {
                    *x = fnext;
                }
                if let Some(x) = self.next.get_mut(mp) {
                    *x = 0;
                }
            } else {
                // In its main position: the new key goes to `f`, chained after `mp`.
                if mpnext != 0
                    && let Some(x) = self.next.get_mut(f)
                {
                    *x = (mp as i64 + i64::from(mpnext) - f as i64) as i32;
                }
                if let Some(x) = self.next.get_mut(mp) {
                    *x = (f as i64 - mp as i64) as i32;
                }
                mp = f;
            }
        }
        if let Some(n) = self.nodes.get_mut(mp) {
            *n = Node::new(Value::Int(k), val);
        }
        true
    }

    /// Nodes holding a key, with or without a value.
    fn keyed(&self) -> usize {
        self.nodes.iter().filter(|n| !n.is_free()).count()
    }
}

/// A table's array part and its buffer: the fields are private, so every use says which it
/// means, the array part (`live*`, the first `len` slots) or the storage a resize works on
/// (`backing*`). Past `len`, a resize given up keeps the new tail it was making (`stage`),
/// hidden, for the next one to go on from in place.
#[derive(Default)]
pub(crate) struct ArrPart {
    buf: Buf,
    len: u32,
    /// Lua's `alimit`: the array size, or a border hint below it. (Here, so the table
    /// stays as big as it was: 2^31 slots at most, `MAXASIZE`.)
    pub alimit: Limit,
}

/// A count of slots of an array part (`MAXASIZE` at most), as a `usize`.
#[derive(Default)]
pub(crate) struct Limit(crate::nostd::cell::Cell<u32>);

impl Limit {
    #[inline]
    pub(crate) fn get(&self) -> usize {
        self.0.get() as usize
    }

    #[inline]
    pub(crate) fn set(&self, n: usize) {
        self.0.set(u32::try_from(n).unwrap_or(u32::MAX));
    }
}

/// The array's storage: one buffer, or (wasm, and the `segmented-arrays` test build) a
/// segmented one past a size (`seg`).
#[cfg(not(any(target_family = "wasm", feature = "segmented-arrays")))]
type Buf = Vec<Value>;
#[cfg(any(target_family = "wasm", feature = "segmented-arrays"))]
type Buf = seg::Store;

/// `ArrPart::backing_mut`: the storage with no hidden tail, the array part as long as it is
/// once the change is made.
#[cfg(not(any(target_family = "wasm", feature = "segmented-arrays")))]
pub(crate) struct Backing<'a>(&'a mut ArrPart);

#[cfg(not(any(target_family = "wasm", feature = "segmented-arrays")))]
impl crate::nostd::ops::Deref for Backing<'_> {
    type Target = Vec<Value>;
    fn deref(&self) -> &Vec<Value> {
        &self.0.buf
    }
}

#[cfg(not(any(target_family = "wasm", feature = "segmented-arrays")))]
impl crate::nostd::ops::DerefMut for Backing<'_> {
    fn deref_mut(&mut self) -> &mut Vec<Value> {
        &mut self.0.buf
    }
}

#[cfg(not(any(target_family = "wasm", feature = "segmented-arrays")))]
impl Drop for Backing<'_> {
    fn drop(&mut self) {
        self.0.len = self.0.buf.len() as u32;
    }
}

#[cfg(not(any(target_family = "wasm", feature = "segmented-arrays")))]
impl ArrPart {
    /// The array part (contiguous on this build; outside `ArrPart`, `get`, `iter` and
    /// `chunks`, so a segmented one works the same, but for the fast loop's `arr_slot!`).
    #[inline(always)]
    pub(crate) fn live(&self) -> &[Value] {
        self.buf.get(..self.len as usize).unwrap_or_default()
    }

    /// The array part, to write in place.
    #[inline(always)]
    pub(crate) fn live_mut(&mut self) -> &mut [Value] {
        self.buf.get_mut(..self.len as usize).unwrap_or_default()
    }

    /// Slot `i` of the array part.
    #[inline(always)]
    pub(crate) fn get(&self, i: usize) -> Option<&Value> {
        self.live().get(i)
    }

    /// Slot `i` of the array part, to write in place.
    #[inline(always)]
    pub(crate) fn get_mut(&mut self, i: usize) -> Option<&mut Value> {
        self.live_mut().get_mut(i)
    }

    /// The array part's slots in order.
    #[inline]
    pub(crate) fn iter(&self) -> impl Iterator<Item = &Value> {
        self.live().iter()
    }

    /// The array part's slots from `at` with their indices.
    #[inline]
    pub(crate) fn iter_from(&self, at: usize) -> impl Iterator<Item = (usize, &Value)> {
        self.live().iter().enumerate().skip(at)
    }

    /// The array part cut to `n` slots (a hidden tail dropped).
    #[inline]
    fn truncate(&mut self, n: usize) {
        self.backing_mut().truncate(n);
    }

    /// The array part's buffer grown to hold `cap` slots (`regrow_arr`, a hidden tail dropped).
    fn regrow(&mut self, mem: &mut Mem, cap: usize) {
        regrow_arr(mem, &mut self.backing_mut(), cap);
    }

    /// A hidden tail dropped (`resize`'s fresh build).
    fn drop_hidden(&mut self) {
        self.backing_mut();
    }

    /// The storage grown to `cap` slots for a resize, a 1 to 32 MiB buffer a piece at a time
    /// (`regrow_arr_watched`).
    fn reserve_watched(&mut self, mem: &mut Mem, cap: usize, copy: &mut Option<Vec<Value>>) -> Result<(), SetError> {
        regrow_arr_watched(mem, &mut self.buf, cap, copy)
    }

    /// The array part's slots in `r` (cut to it), a contiguous piece at a time, each with
    /// the index of its first slot.
    #[inline]
    pub(crate) fn chunks(&self, r: crate::nostd::ops::Range<usize>) -> impl Iterator<Item = (usize, &[Value])> {
        let (from, to) = (r.start.min(self.live_len()), r.end.min(self.live_len()));
        self.live().get(from..to.max(from)).map(|c| (from, c)).into_iter()
    }

    /// The buffer's capacity once the storage holds `n` slots (`resize`'s charge).
    #[inline]
    fn capacity_for(&self, n: usize) -> usize {
        n.max(self.capacity())
    }

    /// The capacity a constructor's list growing to `last` slots reserves (`set_list`: by
    /// half again, so the copies stay linear, S6T-1).
    #[inline]
    fn grown_capacity(&self, last: usize) -> usize {
        last.max(self.capacity() + self.capacity() / 2)
    }

    /// `vals` written into the array part from slot `off` (false: past its end).
    #[inline]
    fn copy_in(&mut self, off: usize, vals: &[Value]) -> bool {
        match self.live_mut().get_mut(off..off + vals.len()) {
            Some(slots) => {
                slots.copy_from_slice(vals);
                true
            }
            None => false,
        }
    }

    /// Slot `i` of the storage, the hidden tail's too (a resize placing a value there).
    #[inline]
    fn stage_get_mut(&mut self, i: usize) -> Option<&mut Value> {
        self.buf.get_mut(i)
    }

    /// The hidden tail grown to `to` slots of the storage, nil.
    #[inline]
    fn stage_extend(&mut self, to: usize) {
        self.buf.resize(to, Value::Nil);
    }

    /// The array part made `n` slots, a hidden tail dropped (new slots nil).
    #[inline]
    fn resize_nil(&mut self, n: usize) {
        self.backing_mut().resize(n, Value::Nil);
    }

    /// The array part's size (Lua's real array size).
    #[inline]
    pub(crate) fn live_len(&self) -> usize {
        self.len as usize
    }

    /// The buffer's capacity in slots (what the account charges).
    #[inline]
    pub(crate) fn capacity(&self) -> usize {
        self.buf.capacity()
    }

    /// The storage's length in slots, for the code that resizes the array part.
    #[inline]
    fn backing_len(&self) -> usize {
        self.buf.len()
    }

    /// The storage, for the code that resizes the array part: a hidden tail dropped first,
    /// and the array part all of it after.
    #[inline]
    fn backing_mut(&mut self) -> Backing<'_> {
        self.buf.truncate(self.len as usize);
        Backing(self)
    }

    /// The storage with the hidden tail a resize is making past the array part (`resize`).
    #[inline]
    fn stage(&mut self) -> &mut Vec<Value> {
        &mut self.buf
    }

    /// The hidden tail made the array part (a resize ends).
    fn commit(&mut self) {
        self.len = self.buf.len() as u32;
    }

    /// Slot `k - 1` of the hidden tail, if it holds one (`keep_write`).
    fn staged_mut(&mut self, k: i64) -> Option<&mut Value> {
        let i = usize::try_from(k).ok()?.checked_sub(1)?;
        if i < self.live_len() { None } else { self.buf.get_mut(i) }
    }

    /// The storage, taken (a table that died: its buffer to the pool).
    pub(crate) fn take_backing(&mut self) -> Vec<Value> {
        self.len = 0;
        crate::nostd::mem::take(&mut self.buf)
    }
}

impl From<Vec<Value>> for ArrPart {
    fn from(buf: Vec<Value>) -> ArrPart {
        #[cfg(any(target_family = "wasm", feature = "segmented-arrays"))]
        let buf = seg::Store::Flat(buf);
        ArrPart { len: buf.len() as u32, buf, alimit: Limit::default() }
    }
}

/// The array part over segmented storage: the same methods, a buffer of `seg::FROM` bytes
/// or more grown by blocks after it instead of being moved (on wasm `realloc` copies:
/// 19 ms for 32 MiB, LR-29f).
#[cfg(any(target_family = "wasm", feature = "segmented-arrays"))]
impl ArrPart {
    #[inline]
    pub(crate) fn get(&self, i: usize) -> Option<&Value> {
        if i < self.live_len() { self.buf.get(i) } else { None }
    }

    #[inline]
    pub(crate) fn get_mut(&mut self, i: usize) -> Option<&mut Value> {
        if i < self.live_len() { self.buf.get_mut(i) } else { None }
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = &Value> {
        self.chunks(0..self.live_len()).flat_map(|(_, c)| c.iter())
    }

    pub(crate) fn iter_from(&self, at: usize) -> impl Iterator<Item = (usize, &Value)> {
        self.chunks(at..self.live_len()).flat_map(|(b, c)| c.iter().enumerate().map(move |(k, v)| (b + k, v)))
    }

    fn truncate(&mut self, n: usize) {
        self.resize_nil(n.min(self.live_len()));
    }

    /// Past `seg::FROM`, the storage is made segmented (its buffer kept as block 0) instead
    /// of grown: the blocks come as it does.
    fn regrow(&mut self, mem: &mut Mem, cap: usize) {
        if self.segmented() {
            self.buf.segment();
        } else if let seg::Store::Flat(v) = &mut self.buf {
            regrow_arr(mem, v, cap);
        }
    }

    fn drop_hidden(&mut self) {
        self.buf.resize_nil(self.live_len());
    }

    fn reserve_watched(&mut self, mem: &mut Mem, cap: usize, copy: &mut Option<Vec<Value>>) -> Result<(), SetError> {
        if self.segmented() {
            self.buf.segment();
            return Ok(());
        }
        match &mut self.buf {
            seg::Store::Flat(v) => regrow_arr_watched(mem, v, cap, copy),
            seg::Store::Seg(_) => Ok(()),
        }
    }

    /// Whether the storage grows by blocks: segmented already, or its buffer `seg::FROM`
    /// bytes or more.
    fn segmented(&self) -> bool {
        matches!(self.buf, seg::Store::Seg(_)) || self.buf.capacity() * SLOT_SIZE >= seg::FROM
    }

    fn capacity_for(&self, n: usize) -> usize {
        if self.segmented() { self.buf.capacity_for(n) } else { n.max(self.capacity()) }
    }

    fn grown_capacity(&self, last: usize) -> usize {
        if self.segmented() { self.buf.capacity_for(last) } else { last.max(self.capacity() + self.capacity() / 2) }
    }

    pub(crate) fn chunks(&self, r: crate::nostd::ops::Range<usize>) -> impl Iterator<Item = (usize, &[Value])> {
        self.buf.chunks(r.start.min(self.live_len()), r.end.min(self.live_len()))
    }

    fn copy_in(&mut self, off: usize, vals: &[Value]) -> bool {
        if off + vals.len() > self.live_len() {
            return false;
        }
        for (i, v) in vals.iter().enumerate() {
            if let Some(s) = self.buf.get_mut(off + i) {
                *s = *v;
            }
        }
        true
    }

    fn stage_get_mut(&mut self, i: usize) -> Option<&mut Value> {
        self.buf.get_mut(i)
    }

    fn stage_extend(&mut self, to: usize) {
        self.buf.resize_nil(to);
    }

    fn resize_nil(&mut self, n: usize) {
        self.buf.resize_nil(self.live_len());
        self.buf.resize_nil(n);
        self.len = n as u32;
    }

    #[inline]
    pub(crate) fn live_len(&self) -> usize {
        self.len as usize
    }

    #[inline]
    pub(crate) fn capacity(&self) -> usize {
        self.buf.capacity()
    }

    #[inline]
    fn backing_len(&self) -> usize {
        self.buf.len()
    }

    fn commit(&mut self) {
        self.len = self.buf.len() as u32;
    }

    fn staged_mut(&mut self, k: i64) -> Option<&mut Value> {
        let i = usize::try_from(k).ok()?.checked_sub(1)?;
        if i < self.live_len() { None } else { self.buf.get_mut(i) }
    }

    pub(crate) fn take_backing(&mut self) -> Vec<Value> {
        self.len = 0;
        self.buf.take_first()
    }
}

/// Segmented array storage (wasm; natively the `segmented-arrays` test build, smaller):
/// block 0, the buffer the array had (kept, never copied), then blocks of `BLOCK` slots,
/// each made whole and nil, `used` slots of them in the storage.
#[cfg(any(target_family = "wasm", feature = "segmented-arrays"))]
mod seg {
    use crate::value::Value;

    #[cfg(feature = "segmented-arrays")]
    const SHIFT: u32 = 12;
    #[cfg(not(feature = "segmented-arrays"))]
    const SHIFT: u32 = 16;
    pub(super) const BLOCK: usize = 1 << SHIFT;
    /// The buffer's size from which it grows by blocks: 2 MiB on wasm, where a copy costs
    /// ~0.6 ms a MiB and a 2 MiB one stays under the 2 ms slice (SEG-W1).
    #[cfg(feature = "segmented-arrays")]
    pub(super) const FROM: usize = 64 << 10;
    #[cfg(not(feature = "segmented-arrays"))]
    pub(super) const FROM: usize = 2 << 20;

    pub(crate) struct Segs {
        first: Vec<Value>,
        blocks: Vec<Box<[Value]>>,
        used: usize,
    }

    pub(crate) enum Store {
        Flat(Vec<Value>),
        Seg(Box<Segs>),
    }

    impl Default for Store {
        fn default() -> Store {
            Store::Flat(Vec::new())
        }
    }

    impl Segs {
        #[inline]
        fn tail(&self, i: usize) -> Option<&Value> {
            let j = i.checked_sub(self.first.len())?;
            if j >= self.used {
                return None;
            }
            self.blocks.get(j >> SHIFT)?.get(j & (BLOCK - 1))
        }

        #[inline]
        fn tail_mut(&mut self, i: usize) -> Option<&mut Value> {
            let j = i.checked_sub(self.first.len())?;
            if j >= self.used {
                return None;
            }
            self.blocks.get_mut(j >> SHIFT)?.get_mut(j & (BLOCK - 1))
        }
    }

    impl Store {
        pub(super) fn len(&self) -> usize {
            match self {
                Store::Flat(v) => v.len(),
                Store::Seg(s) => s.first.len() + s.used,
            }
        }

        pub(super) fn capacity(&self) -> usize {
            match self {
                Store::Flat(v) => v.capacity(),
                Store::Seg(s) => s.first.capacity() + s.blocks.len() * BLOCK,
            }
        }

        /// The capacity once segmented and holding `n` slots: block 0's, and whole blocks.
        pub(super) fn capacity_for(&self, n: usize) -> usize {
            let (first, cap) = match self {
                Store::Flat(v) => (v.len(), v.capacity()),
                Store::Seg(s) => (s.first.len(), s.first.capacity()),
            };
            cap + n.saturating_sub(first).div_ceil(BLOCK) * BLOCK
        }

        #[inline]
        pub(super) fn get(&self, i: usize) -> Option<&Value> {
            match self {
                Store::Flat(v) => v.get(i),
                Store::Seg(s) => s.first.get(i).or_else(|| s.tail(i)),
            }
        }

        #[inline]
        pub(super) fn get_mut(&mut self, i: usize) -> Option<&mut Value> {
            match self {
                Store::Flat(v) => v.get_mut(i),
                Store::Seg(s) => {
                    if i < s.first.len() {
                        s.first.get_mut(i)
                    } else {
                        s.tail_mut(i)
                    }
                }
            }
        }

        /// Slots `from..to` a contiguous piece at a time, each with its first index.
        pub(super) fn chunks(&self, from: usize, to: usize) -> impl Iterator<Item = (usize, &[Value])> {
            let (first, blocks, used): (&[Value], &[Box<[Value]>], usize) = match self {
                Store::Flat(v) => (v, &[], 0),
                Store::Seg(s) => (&s.first, &s.blocks, s.used),
            };
            let fl = first.len();
            let head = first.get(from.min(fl)..to.min(fl).max(from.min(fl))).map(|c| (from.min(fl), c));
            let (a, b) = (from.max(fl) - fl, (to.max(fl) - fl).min(used));
            let k = (a >> SHIFT)..if b > a { ((b - 1) >> SHIFT) + 1 } else { a >> SHIFT };
            let tail = k.filter_map(move |k| {
                let lo = (k << SHIFT).max(a);
                let hi = ((k + 1) << SHIFT).min(b);
                blocks.get(k)?.get(lo - (k << SHIFT)..hi - (k << SHIFT)).map(|c| (fl + lo, c))
            });
            head.into_iter().filter(|c| !c.1.is_empty()).chain(tail)
        }

        /// Made segmented: the buffer kept as block 0 (moved, not copied).
        pub(super) fn segment(&mut self) {
            if let Store::Flat(v) = self {
                let first = crate::nostd::mem::take(v);
                super::count_segmented();
                *self = Store::Seg(Box::new(Segs { first, blocks: Vec::new(), used: 0 }));
            }
        }

        /// `n` slots, the new ones nil (blocks made whole as they are needed; past block 0
        /// cut back, it is one buffer again).
        pub(super) fn resize_nil(&mut self, n: usize) {
            match self {
                Store::Flat(v) => v.resize(n, Value::Nil),
                Store::Seg(s) => {
                    let fl = s.first.len();
                    if n <= fl {
                        let mut first = crate::nostd::mem::take(&mut s.first);
                        first.truncate(n);
                        *self = Store::Flat(first);
                        return;
                    }
                    let used = n - fl;
                    s.blocks.truncate(used.div_ceil(BLOCK));
                    // (Slots past `used` are left as they were: nil again when taken in.)
                    let mut j = s.used.min(used);
                    while j < used {
                        let k = j >> SHIFT;
                        if k == s.blocks.len() {
                            s.blocks.push(vec![Value::Nil; BLOCK].into_boxed_slice());
                            j = (k + 1) << SHIFT;
                            continue;
                        }
                        let hi = ((k + 1) << SHIFT).min(used);
                        if let Some(b) = s.blocks.get_mut(k).and_then(|b| b.get_mut(j - (k << SHIFT)..hi - (k << SHIFT))) {
                            b.fill(Value::Nil);
                        }
                        j = hi;
                    }
                    s.used = used;
                }
            }
        }

        /// Block 0 (a dead table's buffer, to the pool); the blocks are freed.
        pub(super) fn take_first(&mut self) -> Vec<Value> {
            match crate::nostd::mem::take(self) {
                Store::Flat(v) => v,
                Store::Seg(s) => s.first,
            }
        }
    }
}

/// A table.
#[derive(Default)]
pub(crate) struct Table {
    /// The state's key-hashing seed (`Heap::table_seed`).
    pub seed: u64,
    /// The array part: keys `1..=arr.live_len()` (Lua's real array size), nil slots
    /// included. Its buffer is reached only through `ArrPart`'s methods, which say whether
    /// a use means the array part (`live`) or its storage (`backing`).
    pub arr: ArrPart,
    /// The hash part for keys other than integers.
    pub nodes: Vec<Node>,
    /// Slots of `nodes` with a key (live or dead). A new key always takes a node never
    /// used (a removed key's node is not reused for another key until the part is
    /// rebuilt), so this counts the keys inserted since: the rebuilds it times never
    /// depend on the seed (S5B-OPEN-1: open addressing put a new key in the first removed
    /// key's slot on its way, so the count, and with it the fuel of a churning table,
    /// depended on the seed).
    pub used: u32,
    /// Nodes at the top of `nodes` known used: free nodes for colliding keys are taken
    /// below them (Lua's `lastfree`, as `nodes.len() - free_top`).
    pub free_top: u32,
    /// The hash part for integer keys outside the array part (none until one comes).
    pub ints: Option<Box<IntPart>>,
    /// Lua's `BITRAS`: `alimit` is a hint, not the array size.
    pub limit_hint: crate::nostd::cell::Cell<bool>,
    /// Lua's node array is no longer the integer part (another key came): its size and
    /// keyed nodes are counted in `csize`/`ckeys` instead.
    pub counting: bool,
    /// What the last resize given up on this table had done (`RESUMABLE`), in
    /// `Mem::partials` at the index past it (0: none). (An index, so a table that dies has
    /// nothing to drop: a box here cost every table's drop a call.)
    pub partial: u32,
    pub csize: u32,
    pub ckeys: u32,
    pub meta: Option<TableRef>,
    /// Metamethods known absent from this table when it is a metatable (bit per event);
    /// cleared on any write to the table.
    pub absent: crate::nostd::cell::Cell<u32>,
    /// The node `__index` was last found in, when this table is a metatable: tried first
    /// (self-checking like an instruction's hint, so no write can make it stale).
    pub index_hint: crate::nostd::cell::Cell<u32>,
    /// Slots copied by resizes not yet charged as fuel: the caller of `set` charges them
    /// (`take_resized`). Depends only on what the table holds, never on hash layout, so
    /// fuel stays the same run to run.
    pub resized: usize,
    /// The nodes of `nodes` that may hold a value, for `next` (parts past `LIVE_MIN` slots).
    pub live: Option<Box<LiveBits>>,
    /// The collector is traversing this table in chunks while marking is incremental
    /// (`Heap::part`): an insertion that moves an entry to another node, and any rebuild or
    /// resize of its parts, is noted in `Mem::watch`.
    pub gc_watch: bool,
    /// A chain of `nodes` came out longer than `CHAIN_FREE + 1` (keys aimed at the seed): the
    /// next insert that can pay for it reseeds the part (`reseed`).
    pub reseed_due: bool,
    /// Its slots when collection cycle `.0` started, taken before its first rebuild or
    /// resize in a cycle (one made since has not changed since it was made): what marking
    /// charges for it (COV-5).
    pub gc_snap: (u32, u32, u32),
}

/// The table the collector traverses in chunks, its array part `alen` slots now, is about
/// to be rebuilt or resized.
#[cold]
#[inline(never)]
fn restructured(mem: &mut Mem, alen: usize) {
    mem.watch.restructured = true;
    mem.watch.alen_floor = mem.watch.alen_floor.min(alen);
}

/// An entry moved to another node of the table the collector traverses in chunks.
#[cold]
#[inline(never)]
fn note_moved(mem: &mut Mem, k: Value, v: Value) {
    mem.watch.moved.extend([k, v]);
}

/// Why a write failed.
pub(crate) enum SetError {
    NilKey,
    NanKey,
    Memory,
    /// A resize gave up between pieces for the host's interrupt or deadline: the table is
    /// as it was before the write.
    Stopped(crate::LimitKind),
    /// A rehash the call's fuel cannot pay for: refused before it touches the table.
    Fuel,
}

impl Table {
    /// The table's slots held: its array buffer's capacity and its node parts' sizes.
    #[inline]
    pub(crate) fn capacity_slots(&self) -> usize {
        self.arr.capacity() + self.nodes.len() + self.ints.as_ref().map_or(0, |p| p.nodes.len())
    }

    /// A change of `capacity_slots()` from `before`: every buffer a table gains or gives
    /// up passes here (`place_again`, `make_ints`, `to_counting`, `resize`, `set_list`; a
    /// table's making and death are the heap's).
    #[inline]
    fn reshaped(&self, mem: &mut Mem, before: usize) {
        // table_slots hook (SEC7-G2):
        // `mem.table_slots(self.capacity_slots() as isize - before as isize)`.
        let _ = (mem, before);
    }

    /// Sizes a new table as Lua's `luaH_resize(t, arr, hash)` does for a constructor or
    /// `lua_createtable`: `arr` nil slots, Lua's node array for `hash` keys (counted: the
    /// keys to come are mostly fields). Within the capacity the table was made with
    /// (`Heap::new_table_in`).
    pub fn init_sizes(&mut self, arr: usize, hash: usize) {
        self.arr.resize_nil(arr);
        self.arr.alimit.set(arr);
        self.limit_hint.set(false);
        self.csize = lua_nodes(hash) as u32;
        self.ckeys = 0;
        // Lua's node array is the integer part until another key comes; made when the
        // first integer key needs it.
        self.counting = false;
    }

    /// The bytes it holds.
    pub fn mem_size(&self) -> usize {
        TABLE_BASE
            + self.arr.capacity() * SLOT_SIZE
            + part_bytes(self.nodes.len())
            + self.int_len() * INODE_SIZE
    }

    #[inline]
    fn int_len(&self) -> usize {
        self.ints.as_ref().map_or(0, |p| p.nodes.len())
    }

    #[inline]
    fn int_nodes(&self) -> &[Node] {
        self.ints.as_ref().map_or(&[], |p| &p.nodes)
    }

    /// The two hash parts' nodes as `hash_nodes` walks them: other keys, then integers.
    pub fn hash_parts(&self) -> (&[Node], &[Node]) {
        (&self.nodes, self.int_nodes())
    }

    /// Both hash parts' nodes (for the collector and traversal), other keys first.
    pub fn hash_nodes(&self) -> impl Iterator<Item = &Node> {
        self.nodes.iter().chain(self.int_nodes())
    }

    /// Slots in all its parts: what a walk over its entries reads (`entries`).
    /// Its size is about to change in collection cycle `cycle`: kept if it is the first
    /// change in it (`gc_snap`).
    #[inline]
    pub fn snap_size(&mut self, cycle: u32) {
        if self.gc_snap.0 != cycle {
            let n = u32::try_from(self.slots()).unwrap_or(u32::MAX);
            self.gc_snap = (cycle, n, n);
        }
    }

    /// Its slots when cycle `cycle` started (`gc_snap`).
    pub fn slots_at(&self, cycle: u32) -> usize {
        if self.gc_snap.0 == cycle { self.gc_snap.1 as usize } else { self.slots() }
    }

    pub fn slots(&self) -> usize {
        self.arr.live_len() + self.hash_len()
    }

    /// Slots in both hash parts.
    pub fn hash_len(&self) -> usize {
        self.nodes.len() + self.int_len()
    }

    /// Node `j` of `hash_nodes`.
    pub fn hash_node_mut(&mut self, j: usize) -> Option<&mut Node> {
        match j.checked_sub(self.nodes.len()) {
            None => self.nodes.get_mut(j),
            Some(k) => self.ints.as_mut()?.nodes.get_mut(k),
        }
    }

    /// Lua's `alimit` is the array size (`limitequalsasize`).
    #[inline]
    fn limit_is_size(&self) -> bool {
        !self.limit_hint.get() || ispow2(self.arr.alimit.get())
    }

    /// Lua's `ispow2realasize`.
    #[inline]
    fn pow2_size(&self) -> bool {
        self.limit_hint.get() || ispow2(self.arr.alimit.get())
    }

    /// Where integer key `i` is in the array part, as `luaH_getint` decides (moving the
    /// border hint up to `i` when it is past the hint but in the array).
    #[inline]
    pub fn arr_index(&self, i: i64) -> Option<usize> {
        let u = (i as u64).wrapping_sub(1);
        let al = self.arr.alimit.get() as u64;
        if u < al {
            return Some(u as usize);
        }
        if self.limit_hint.get() && (u & !al.wrapping_sub(1)) < al && u < self.arr.live_len() as u64 {
            self.arr.alimit.set(i as usize);
            return Some(u as usize);
        }
        None
    }

    #[inline]
    pub fn get(&self, key: Value) -> Value {
        match key {
            Value::Int(i) => self.get_int(i),
            Value::Str(_) => self.get_hash(key),
            Value::Nil => Value::Nil,
            Value::Num(f) => match number::float_to_int(f) {
                Some(i) => self.get_int(i),
                None => self.get_hash(key),
            },
            _ => self.get_hash(key),
        }
    }

    /// `get_int` with its array part inline, for the fast loop's and the table library's
    /// loops (a call at each array read was most of their cost). Not `get_int` itself:
    /// inlined into `Lua::run` it grew that frame, which every nested call stacks.
    #[inline(always)]
    pub fn get_int_inline(&self, i: i64) -> Value {
        match self.arr_index(i) {
            Some(u) => self.arr.get(u).copied().unwrap_or_default(),
            None => self.get_int(i),
        }
    }

    #[inline]
    pub fn get_int(&self, i: i64) -> Value {
        if let Some(u) = self.arr_index(i) {
            return self.arr.get(u).copied().unwrap_or_default();
        }
        match self.ints.as_ref() {
            Some(p) => match p.find(i) {
                Some(j) => p.nodes.get(j).map(|n| n.val).unwrap_or_default(),
                None => Value::Nil,
            },
            // Counting: integer keys of the hash part are in `nodes`.
            None if !self.nodes.is_empty() => {
                self.int_in_nodes();
                self.get_hash(Value::Int(i))
            }
            None => Value::Nil,
        }
    }

    #[inline]
    pub fn get_str(&self, key: Value) -> Value {
        self.get_hash(key)
    }

    /// `t[key]` trying node `hint` first (an instruction's inline cache); also the node
    /// the key is in, for the next time.
    #[inline]
    pub fn get_hinted(&self, key: Value, hint: u32) -> (Value, Option<u32>) {
        let Value::Str(r) = key else { return (self.get(key), None) };
        let k = pack_str(r);
        if let Some(n) = self.nodes.get(hint as usize)
            && n.holds(k)
        {
            return (n.val, Some(hint));
        }
        match self.find_packed(key, k) {
            Some(i) => (self.nodes.get(i).map(|n| n.val).unwrap_or_default(), Some(i as u32)),
            None => (Value::Nil, None),
        }
    }

    /// The value at node `hint` when it holds string key `key`, else None: no search, so
    /// nothing probed (a method lookup's one-link case in the fast loop).
    #[inline(always)]
    pub fn at_hint(&self, key: Value, hint: u32) -> Option<Value> {
        let Value::Str(r) = key else { return None };
        self.nodes.get(hint as usize).filter(|n| n.holds(pack_str(r))).map(|n| n.val)
    }

    /// `t[key]` trying node `hint` first and keeping the node found there (an
    /// instruction's inline cache). Returns the value alone: a (value, slot) pair made the
    /// fast loop copy the value through memory byte-wise.
    #[inline(always)]
    pub fn get_cached(&self, key: Value, hint: &crate::nostd::cell::Cell<u32>) -> Value {
        let Value::Str(r) = key else {
            return match key {
                Value::Int(i) => self.get_int_inline(i),
                _ => self.get(key),
            };
        };
        let k = pack_str(r);
        if let Some(n) = self.nodes.get(hint.get() as usize)
            && n.holds(k)
        {
            return n.val;
        }
        self.get_cached_find(key, k, hint)
    }

    /// `get_cached` past the hint (out of line: the fast loop inlines `get_cached` at
    /// every field read).
    #[inline(never)]
    fn get_cached_find(&self, key: Value, k: (u8, u64), hint: &crate::nostd::cell::Cell<u32>) -> Value {
        match self.find_packed(key, k) {
            Some(i) => {
                hint.set(i as u32);
                self.nodes.get(i).map(|n| n.val).unwrap_or_default()
            }
            None => Value::Nil,
        }
    }

    /// The slot of `key` (normalized, not an integer of the integer part) in `nodes`: the
    /// first home node, then its chain and the second home's (see `CHAIN_FREE`).
    #[inline]
    fn find(&self, key: Value) -> Option<usize> {
        self.find_packed(key, pack(key))
    }

    /// `find` with the key packed already.
    #[inline]
    fn find_packed(&self, key: Value, k: (u8, u64)) -> Option<usize> {
        let len = self.nodes.len();
        if len == 0 {
            return None;
        }
        let i = (hash_key(key, self.seed) as usize) & (len - 1);
        let n = self.nodes.get(i)?;
        if n.holds(k) {
            return Some(i);
        }
        // Nothing after the home node (no chain of this home goes on from it) and no key of
        // this home in its second home's chain.
        if n.flags & (CHAINED | SPILL) == 0 {
            return None;
        }
        self.find_chain(i, k)
    }

    /// `find` past the first home node: its chain and, when some key of this home went to
    /// its second home (`SPILL`), the second home's chain, walked in step (the loads of the
    /// two overlap). Each step past `CHAIN_FREE` in a chain is fuel (S5GC-5). Out of line,
    /// so the field paths that inline `find` stay small.
    #[inline(never)]
    fn find_chain(&self, i: usize, k: (u8, u64)) -> Option<usize> {
        let at = |from: usize, off: i32| (from as i64 + i64::from(off)) as usize;
        let home = self.nodes.get(i)?;
        let mut a = (home.flags & CHAINED != 0).then(|| at(i, home.next));
        let mut b = None;
        if home.flags & SPILL != 0 {
            let j = second_home(i, hash_key(unpack(k), self.seed), self.nodes.len() - 1);
            if let Some(n) = self.nodes.get(j)
                && n.main
            {
                if n.holds(k) {
                    return Some(j);
                }
                b = (n.next != 0).then(|| at(j, n.next));
            }
        }
        let (mut sa, mut sb) = (0usize, 0usize);
        let r = loop {
            if let Some(x) = a {
                sa += 1;
                let n = self.nodes.get(x)?;
                if n.holds(k) {
                    break Some(x);
                }
                a = (n.next != 0).then(|| at(x, n.next));
            }
            if let Some(x) = b {
                sb += 1;
                let n = self.nodes.get(x)?;
                if n.holds(k) {
                    break Some(x);
                }
                b = (n.next != 0).then(|| at(x, n.next));
            }
            if a.is_none() && b.is_none() {
                break None;
            }
        };
        let over = sa.saturating_sub(CHAIN_FREE) + sb.saturating_sub(CHAIN_FREE);
        if over > 0 {
            add_probes(over as u64);
        }
        r
    }

    /// An integer key looked up in `nodes` (Lua's node array counting other keys): its home
    /// is anywhere in the part, a cache miss once the part is past the caches, where an
    /// integer part keeps neighbouring keys together. A `table.move` of a million integer
    /// keys over a table of string keys (2^21 slots) ran at 8.3x plain code per unit of
    /// fuel with the fast loop and 7.3x without (`sweep3_sec::fuel_tracks_time`, bound 10x;
    /// `perf_tables::probe_moves`): such a lookup counts probe steps by the part's size
    /// now (`nodes_lookup_cost`), charged as any lookup's steps are. The size is the
    /// program's, never the seed's, so the fuel still is.
    #[inline]
    fn int_in_nodes(&self) {
        let c = nodes_lookup_cost(self.nodes.len());
        if c > 0 {
            add_probes(c);
        }
    }

    #[inline]
    fn get_hash(&self, key: Value) -> Value {
        match self.find(key) {
            Some(i) => self.nodes.get(i).map(|n| n.val).unwrap_or_default(),
            None => Value::Nil,
        }
    }

    /// `t[key] = val` for a string key whose live node is node `hint`: the common field
    /// write, checked in a few loads. False: not that case (see `set_existing`). The key
    /// is already in the table, so only the value can need the collector's barrier.
    #[inline]
    pub fn set_at_hint(&mut self, key: Value, val: Value, hint: u32) -> bool {
        match self.nodes.get_mut(hint as usize) {
            Some(n) if matches!(key, Value::Str(r) if n.holds(pack_str(r))) && !n.val.is_nil() => {
                n.val = val;
                self.absent.set(0);
                true
            }
            _ => false,
        }
    }

    /// `t[key] = val` in place, when no rehash and no `__newindex` is involved: an integer
    /// key in the array part, a key the table has (with or without a value), or a new key
    /// while Lua's node array has room (string or integer keys only; node `hint` tried
    /// first). When `__newindex` may intercept (`newindex_absent` false and a metatable)
    /// the slot must hold a value already. None: not done (take the full path); else the
    /// `nodes` slot written, if any.
    #[inline]
    pub fn set_existing(&mut self, key: Value, val: Value, hint: u32, newindex_absent: bool, mem: &mut Mem) -> Option<Option<u32>> {
        let has_meta = self.meta.is_some() && !newindex_absent;
        let slot = match key {
            Value::Int(i) => {
                if let Some(u) = self.arr_index(i) {
                    let s = self.arr.get_mut(u)?;
                    if has_meta && s.is_nil() {
                        return None;
                    }
                    *s = val;
                    self.absent.set(0);
                    return Some(None);
                }
                if let Some(p) = self.ints.as_mut() {
                    if let Some(j) = p.find(i) {
                        let n = p.nodes.get_mut(j)?;
                        if has_meta && n.val.is_nil() {
                            return None;
                        }
                        n.val = val;
                        self.absent.set(0);
                        return Some(None);
                    }
                    return self.insert_in_place(key, val, has_meta, mem);
                }
                self.int_in_nodes();
                match self.find(key) {
                    Some(j) => j,
                    None => return self.insert_in_place(key, val, has_meta, mem),
                }
            }
            Value::Str(r) => match self.nodes.get(hint as usize) {
                Some(n) if n.holds(pack_str(r)) => hint as usize,
                _ => match self.find_packed(key, pack_str(r)) {
                    Some(i) => i,
                    None => return self.insert_in_place(key, val, has_meta, mem),
                },
            },
            _ => return None,
        };
        let n = self.nodes.get_mut(slot)?;
        let revived = n.val.is_nil();
        if has_meta && revived {
            return None;
        }
        n.val = val;
        if revived {
            self.revived(slot);
        }
        self.absent.set(0);
        Some(Some(slot as u32))
    }

    /// Node `j` took an entry another key displaced, in a table the collector traverses in
    /// chunks (out of line: the insert path stays as small as without it).
    #[cold]
    #[inline(never)]
    fn moved_node(&self, j: usize, mem: &mut Mem) {
        // (After a rebuild nothing needs a note: the traversal starts over.)
        if !mem.watch.restructured
            && let Some(n) = self.nodes.get(j)
        {
            note_moved(mem, n.key(), n.val);
        }
    }

    /// Its parts are about to be rebuilt or resized: noted for the collector when it is
    /// being traversed in chunks (`gc_watch`).
    #[inline]
    fn restructuring(&mut self, mem: &mut Mem) {
        self.snap_size(mem.cycle);
        if self.gc_watch {
            restructured(mem, self.arr.live_len());
        }
    }

    /// The key as the table holds it and its value (nil for a removed key's node), for a
    /// key the table has a slot for; None otherwise. (A long string key the table holds
    /// may be another object with the same bytes than the one asked with.)
    pub fn entry(&self, key: Value) -> Option<(Value, Value)> {
        let key = normalize_key(key);
        if let Value::Int(i) = key {
            if let Some(u) = self.arr_index(i) {
                return self.arr.get(u).map(|v| (key, *v));
            }
            if let Some(p) = self.ints.as_ref() {
                return p.find(i).and_then(|j| p.nodes.get(j)).map(|n| (key, n.val));
            }
        }
        let n = self.nodes.get(self.find(key)?)?;
        Some((n.key(), n.val))
    }

    /// Node `i` of `nodes` may hold a value again (`LiveBits`).
    #[cold]
    fn revived(&self, i: usize) {
        if let Some(b) = &self.live {
            b.set(i);
        }
    }

    /// The slots resizes copied since the last charge, once there are 16 or more (for
    /// fuel, a unit per 16); fewer stay counted, so the fuel never depends on which
    /// writes (fast loop or full path) happened to come between resizes.
    #[inline]
    /// What `take_resized` will charge for this table's growth to `slots` (`slots()`'s
    /// count) during marking, known before the growth is made (LR-29d: its pre-checks).
    fn marking_cost(&self, mem: &Mem, slots: usize) -> usize {
        // (From the snapshot the growth's `restructuring` takes: this cycle's, or the size now.)
        let base = if self.gc_snap.0 == mem.cycle { self.gc_snap.2 as usize } else { self.slots() };
        if mem.marking && slots > base { (slots - base) * (1 + mem.obj_work / 4) * 4 } else { 0 }
    }

    /// `take_resized` in whole units of fuel, the rest left for the call that ends the resize
    /// (a resize given up pays for what it did: what that call pays is the rest of its cost,
    /// and dropped there a remainder made a sliced resize a unit cheaper, LR-29d).
    pub fn take_units(&mut self, mem: &Mem) -> u64 {
        let r = self.take_resized(mem);
        self.resized += r % 16;
        (r / 16) as u64
    }

    pub fn take_resized(&mut self, mem: &Mem) -> usize {
        // SEC7-G1: grown past its size when the cycle started while marking is under way,
        // its growth is marked uncharged (marking is charged by that size, COV-5): its
        // marking is paid here, as collector work (`WORK_PER_FUEL` 4 a unit of fuel, 16 a
        // slot of `resized`), once per slot a cycle. The program's sizes, never the seed's.
        if mem.marking && self.gc_snap.0 == mem.cycle {
            let now = u32::try_from(self.slots()).unwrap_or(u32::MAX);
            if now > self.gc_snap.2 {
                self.resized += (now - self.gc_snap.2) as usize * (1 + mem.obj_work / 4) * 4;
                self.gc_snap.2 = now;
            }
        }
        if self.resized < 16 { 0 } else { crate::nostd::mem::take(&mut self.resized) }
    }

    /// A key known absent (and outside the array part), for `set_existing`: in place
    /// when Lua's node array has room, as `set` decides (so the table ends up the same),
    /// nothing to do for nil, else None (the full path rehashes).
    #[inline]
    fn insert_in_place(&mut self, key: Value, val: Value, has_meta: bool, mem: &mut Mem) -> Option<Option<u32>> {
        if has_meta {
            return None;
        }
        if val.is_nil() {
            return Some(None);
        }
        // Exactly `set`'s new key, rehash included, when the memory cap has room for the
        // most it could take (charged as it goes; the full path raises the error
        // otherwise, so nothing here can fail half-way).
        let most = 64 * (self.arr.live_len() + 1) + 512 * (self.hash_len() + 2);
        if !mem.fits(most) {
            return None;
        }
        if let Value::Str(_) = key
            && self.counting
            && self.ckeys < self.csize
            && (self.used as usize + 1) * 4 <= self.nodes.len() * 3
        {
            // The common case: room in `nodes`, the slot for the instruction's hint.
            self.ckeys += 1;
            self.absent.set(0);
            let slot = self.hash_insert(key, val, mem).ok()?;
            return Some(Some(slot as u32));
        }
        let kept = self.absent.get() & RESUMABLE;
        self.absent.set(0);
        self.new_key_kept(key, val, kept, mem).ok()?;
        Some(None)
    }

    /// Writes `t[key] = val` without metamethods (`luaH_set`).
    pub fn set(&mut self, key: Value, val: Value, mem: &mut Mem) -> Result<(), SetError> {
        let kept = self.absent.get() & RESUMABLE;
        self.absent.set(0);
        let key = match key {
            Value::Nil => return Err(SetError::NilKey),
            Value::Num(f) if f.is_nan() => return Err(SetError::NanKey),
            k => normalize_key(k),
        };
        if let Value::Int(i) = key {
            if let Some(u) = self.arr_index(i) {
                if let Some(s) = self.arr.get_mut(u) {
                    let live = !crate::nostd::mem::replace(s, val).is_nil();
                    self.keep_write(kept, live, key, val, mem);
                }
                return Ok(());
            }
            if let Some(p) = self.ints.as_mut()
                && let Some(j) = p.find(i)
            {
                if let Some(n) = p.nodes.get_mut(j) {
                    let live = !crate::nostd::mem::replace(&mut n.val, val).is_nil();
                    self.keep_write(kept, live, key, val, mem);
                }
                return Ok(());
            }
        }
        if self.ints.is_none() && matches!(key, Value::Int(_)) {
            self.int_in_nodes();
        }
        if (self.ints.is_none() || !matches!(key, Value::Int(_)))
            && let Some(i) = self.find(key)
        {
            if let Some(n) = self.nodes.get_mut(i) {
                let revived = n.val.is_nil();
                n.val = val;
                if revived {
                    self.revived(i);
                }
                self.keep_write(kept, !revived, key, val, mem);
            }
            return Ok(());
        }
        if val.is_nil() {
            if kept != 0 {
                self.unkept(mem);
            }
            return Ok(());
        }
        self.new_key_kept(key, val, kept, mem)
    }

    /// A value written over a value of `key` (`live`) while a resize given up on this table
    /// is kept (`kept`, the write's `RESUMABLE`): its keys and counts are still right, so it
    /// stays, the value written into what it made too (SEC7-T2: thrown away, a script
    /// writing a field before its resizing write started over in every 1 ms call, forever).
    /// (Its lookups are not fuel: whether a resize is kept depends on the clock. One past
    /// `CHAIN_FREE + 1` steps, keys aimed at the new part, and the part is thrown away.)
    #[inline]
    fn keep_write(&mut self, kept: u32, live: bool, key: Value, val: Value, mem: &mut Mem) {
        if kept == 0 {
            return;
        }
        if !live || val.is_nil() {
            return self.unkept(mem);
        }
        let Some(Some(p)) = mem.partials.get_mut((self.partial as usize).wrapping_sub(1)) else { return };
        let probes = take_probes();
        if let (Some(rb), Value::Int(i)) = (p.resize.as_mut(), key) {
            if let Some(s) = rb.copy.as_mut().and_then(|c| c.get_mut((i as u64).wrapping_sub(1) as usize)) {
                *s = val;
            }
            // (Placed in the new tail already: its value there too.)
            if let Some(s) = self.arr.staged_mut(i) {
                *s = val;
            }
            if let Some(q) = rb.ints.as_mut()
                && let Some(n) = q.find(i).and_then(|j| q.nodes.get_mut(j))
            {
                n.val = val;
            }
        }
        if let Some(b) = p.nodes.as_mut() {
            crate::nostd::mem::swap(&mut self.nodes, &mut b.nodes);
            if let Some(n) = self.find(key).and_then(|j| self.nodes.get_mut(j)) {
                n.val = val;
            }
            crate::nostd::mem::swap(&mut self.nodes, &mut b.nodes);
        }
        let walked = take_probes();
        put_probes(probes);
        if walked <= (CHAIN_FREE + 1) as u64 {
            self.absent.set(kept);
        } else {
            self.unkept(mem);
        }
    }

    /// A resize given up on this table, thrown away by a write (`RESUMABLE` cleared): what
    /// it made (its `Partial` and the hidden tail of the array part) is released at that
    /// write, not held until the next resize finds it stale.
    #[cold]
    pub(crate) fn unkept(&mut self, mem: &mut Mem) {
        if self.partial != 0 {
            drop_partial(mem, crate::nostd::mem::take(&mut self.partial));
            let (slots, size) = (self.capacity_slots(), self.mem_size());
            self.arr.drop_hidden();
            self.reshaped(mem, slots);
            mem.release(size.saturating_sub(self.mem_size()));
        }
    }

    /// `new_key`, a resize given up on this table before going on from what it made when
    /// `kept` (`RESUMABLE`, which the write cleared): nothing is written yet.
    #[inline]
    fn new_key_kept(&mut self, key: Value, val: Value, kept: u32, mem: &mut Mem) -> Result<(), SetError> {
        self.absent.set(kept);
        let r = self.new_key(key, val, mem);
        if r.is_ok() {
            self.absent.set(0);
        }
        r
    }

    /// Lua's node array stops being the integer part (another key comes): its keyed
    /// nodes are counted from now on, and the integer keys move to `nodes`.
    /// `fuel`: what the call has left, the move refused (`SetError::Fuel`) before it
    /// touches the table when it costs more; `usize::MAX` once a rehash has paid for it.
    fn to_counting(&mut self, fuel: usize, mem: &mut Mem) -> Result<(), SetError> {
        let Some(p) = self.ints.as_ref() else {
            self.restructuring(mem);
            self.ckeys = 0;
            self.counting = true;
            return Ok(());
        };
        // Refused before the count when its three passes alone cost more: refused below all
        // the same, but after a pass over every key, in every call (SEC7-T4: 10 to 11x
        // plain code per unit of fuel).
        // (A move given up for the fuel and kept goes on from what it made: SEC8-F4.)
        if 3 * p.nodes.len() / 16 > fuel || (self.partial != 0 && self.absent.get() & RESUMABLE != 0) {
            return self.counting_refused(fuel, mem);
        }
        let live = p.nodes.iter().filter(|n| !n.val.is_nil()).count();
        let len = hash_slots(live + self.live_nodes() + 1);
        // Charged after it is done, a string key put in a table of 2^17 integer keys moved
        // them for 418,000 units under a call's 100,000 (10x plain code per unit of fuel,
        // cost_fuzz_quick's toggle). The count is the program's, never the seed's.
        // (Three passes, keyed, live and moved, and the placements: S5B-COST-2.)
        let cost = 3 * p.nodes.len() + place_cost(len.max(self.nodes.len())) * live;
        // With the rebuild of `nodes` that makes room for them and the marking of its
        // growth (LR-29d: charged after, unchecked).
        let room = len > self.nodes.len() || (self.used as usize + live + 1) * 4 > self.nodes.len() * 3;
        let after = if room { len.max(self.nodes.len()) } else { self.nodes.len() };
        let rebuild = if room { rebuild_cost(self.nodes.len(), after) } else { 0 };
        if (self.resized + cost + rebuild + self.marking_cost(mem, self.arr.live_len() + after)) / 16 > fuel {
            return self.counting_refused(fuel, mem);
        }
        let keyed = p.keyed() as u32;
        self.restructuring(mem);
        self.ckeys = keyed;
        if let Some(p) = self.ints.take() {
            // Room for them all in nodes never used (removed keys hold theirs).
            if room
                && let Err(e) = self.resumed(mem, |t, part, mem| t.rebuild_nodes::<false, false>(len.max(t.nodes.len()), 0, part, mem))
            {
                self.ints = Some(p);
                return Err(e);
            }
            mem.release(p.nodes.len() * INODE_SIZE);
            self.resized += cost;
            let mut longest = 0;
            for n in &p.nodes {
                if !n.val.is_nil() {
                    longest = longest.max(self.raw_insert(n.key(), n.val).1);
                }
            }
            self.reseed_due |= longest > CHAIN_FREE + 1;
            let p = *p;
            let before = self.capacity_slots() + p.nodes.len();
            give_nodes(mem, p.nodes);
            self.reshaped(mem, before);
        }
        self.counting = true;
        Ok(())
    }

    /// Lua's node array as the integer part, made when the first integer key needs it.
    /// `to_counting` refused for the call's fuel: on the full path, when the key move needs
    /// a new `nodes` (`room`), made under a fuel watch instead (SEC8-F4): the integer part's
    /// keys placed into the new part after the old part's, a piece at a time within each
    /// call's fuel, the table as it was until the new part is in; the counts made once and
    /// kept. Charged once, as `to_counting` charges it.
    #[cold]
    #[inline(never)]
    fn counting_refused(&mut self, fuel: usize, mem: &mut Mem) -> Result<(), SetError> {
        let Some(pn) = self.ints.as_ref().filter(|_| mem.fuel_watchable).map(|p| p.nodes.len()) else { return Err(SetError::Fuel) };
        self.resumed(mem, |t, part, mem| {
            let (base, paid) = (t.resized, part.as_ref().map_or(0, |p| p.paid));
            // (A room for the marking is made below, once the new part's size is known.)
            mem.fuel_watch = Some(watch_most(fuel, paid));
            let r = t.counting_watched(pn, base, part, mem);
            mem.fuel_watch = None;
            r
        })?;
        // In: the integer part's keys are in `nodes` with the rest; the part goes.
        if let Some(p) = self.ints.take() {
            self.ckeys = p.keyed() as u32;
            mem.release(p.nodes.len() * INODE_SIZE);
            let p = *p;
            let before = self.capacity_slots() + p.nodes.len();
            give_nodes(mem, p.nodes);
            self.reshaped(mem, before);
        }
        self.counting = true;
        Ok(())
    }

    /// `counting_refused`'s attempt under the watch: the integer part's live keys counted
    /// (one of `to_counting`'s three passes, charged as far as it got), then the new part.
    fn counting_watched(&mut self, pn: usize, base: usize, part: &mut Kept, mem: &mut Mem) -> Result<(), SetError> {
        let (mut at, mut live) = part.as_mut().and_then(|p| p.ints_count.take()).unwrap_or_default();
        let most = mem.fuel_watch.unwrap_or(usize::MAX);
        while at < pn {
            let end = (at + FUEL_PIECE).min(pn);
            if base + end > most {
                self.resized += at;
                stash(part).ints_count = Some((at, live));
                return Err(SetError::Stopped(crate::LimitKind::Fuel));
            }
            live += self.ints.as_ref().map_or(0, |p| p.nodes.get(at..end).unwrap_or_default().iter().filter(|n| !n.val.is_nil()).count());
            WATCHED.with(|w| w.set(w.get() + (end - at) as u64));
            if end / RESIZE_PIECE != at / RESIZE_PIECE
                && let Some(k) = mem.stopped()
            {
                at = end;
                stash(part).ints_count = Some((at, live));
                return Err(SetError::Stopped(k));
            }
            at = end;
        }
        let len = hash_slots(live + self.live_nodes() + 1);
        let room = len > self.nodes.len() || (self.used as usize + live + 1) * 4 > self.nodes.len() * 3;
        let after = len.max(self.nodes.len());
        let cost = 3 * pn + place_cost(after) * live;
        // (Without the room for the move, it goes into `nodes` in place: refused whole.)
        if !room {
            stash(part).ints_count = Some((at, live));
            return Err(SetError::Fuel);
        }
        // (The count charged whole in every attempt, as its pass is in `cost`: the placements
        // are the rest of it. The marking of the growth paid ahead: `prepay_marking`.)
        self.resized += pn;
        let marking = self.marking_cost(mem, self.arr.live_len() + after);
        let r = self.prepay_marking(marking, part, mem).and_then(|()| self.rebuild_nodes::<true, true>(after, cost - pn, part, mem));
        match r {
            Ok(()) => {
                self.resized += cost - pn;
                self.marking_settled(mem);
            }
            Err(_) => stash(part).ints_count = Some((at, live)),
        }
        r
    }

    fn make_ints(&mut self, mem: &mut Mem) -> Result<(), SetError> {
        if self.ints.is_none() && self.csize > 0 {
            self.restructuring(mem);
            mem.charge(self.csize as usize * INODE_SIZE).map_err(|()| SetError::Memory)?;
            let before = self.capacity_slots();
            self.ints = Some(Box::new(IntPart::with_len(self.csize as usize, mem)));
            self.reshaped(mem, before);
        }
        Ok(())
    }

    /// A key known absent (`luaH_newkey`): placed while Lua's node array has room, else
    /// after a rehash (which may take it into the array part).
    fn new_key(&mut self, key: Value, val: Value, mem: &mut Mem) -> Result<(), SetError> {
        if !self.counting {
            if let Value::Int(i) = key {
                self.make_ints(mem)?;
                let mut moved = None;
                if self.ints.as_mut().is_some_and(|p| p.place(i, val, &mut moved)) {
                    if let (true, Some(v)) = (self.gc_watch, moved) {
                        note_moved(mem, Value::Nil, v);
                    }
                    return Ok(());
                }
                self.rehash(key, mem)?;
                return self.set_after_rehash(key, val, mem);
            }
            self.to_counting(mem.fuel, mem)?;
        }
        if self.ckeys >= self.csize {
            self.rehash(key, mem)?;
            return self.set_after_rehash(key, val, mem);
        }
        // (Counted once in: a growth refused for the call's fuel leaves it out, for the full
        // path to put in again.)
        self.hash_insert(key, val, mem)?;
        self.ckeys += 1;
        Ok(())
    }

    /// After a rehash for `key`: into the array part, or placed (the rehash made room).
    fn set_after_rehash(&mut self, key: Value, val: Value, mem: &mut Mem) -> Result<(), SetError> {
        if let Value::Int(i) = key {
            if let Some(u) = self.arr_index(i) {
                if let Some(s) = self.arr.get_mut(u) {
                    *s = val;
                }
                return Ok(());
            }
            if !self.counting {
                self.make_ints(mem)?;
                // (After a rehash, noted as such: a moved entry needs no note.)
                if self.ints.as_mut().is_some_and(|p| p.place(i, val, &mut None)) {
                    return Ok(());
                }
                self.to_counting(usize::MAX, mem)?;
            }
        }
        // (Counted once in: a growth refused for the call's fuel leaves it out, for the full
        // path to put in again.)
        self.hash_insert(key, val, mem)?;
        self.ckeys += 1;
        Ok(())
    }

    /// Puts a key known absent into `nodes` (counting), growing it when full or
    /// reseeding it when keys are aimed at one home (Lua's count of its keyed nodes is the
    /// caller's). The slot it went into.
    fn hash_insert(&mut self, key: Value, val: Value, mem: &mut Mem) -> Result<usize, SetError> {
        if (self.used as usize + 1) * 4 > self.nodes.len() * 3 {
            self.grow_nodes(mem)?;
        } else if self.reseed_due {
            // Keys aimed at one home (a script that knows or guessed the seed: random keys
            // never get here, see `CHAIN_FREE`): a new seed, derived from this one so runs
            // replay, and the part placed again under it, charged as a resize. Known only
            // once a key is in, so done by the next insert, refused as a rehash is when the
            // call cannot pay for it (LR-29d: charged after, unchecked); walks past
            // `CHAIN_FREE` meanwhile are fuel. Once per insert at most.
            let len = self.nodes.len();
            if (self.resized + rebuild_cost(len, len)) / 16 > mem.fuel {
                return Err(SetError::Fuel);
            }
            self.reseed(mem);
        }
        let (slot, chain, moved) = self.raw_insert(key, val);
        if self.gc_watch && moved != usize::MAX {
            self.moved_node(moved, mem);
        }
        self.reseed_due |= chain > CHAIN_FREE + 1;
        Ok(slot)
    }

    /// `nodes` full: rebuilt for its live keys and one more (out of line, so the insert
    /// path stays as small as without it).
    #[inline(never)]
    fn grow_nodes(&mut self, mem: &mut Mem) -> Result<(), SetError> {
        self.resumed(mem, |t, part, mem| {
            // Refused before its count pass when the call cannot pay for it, as a rehash is
            // (SEC7-T4): charged after its work, a new key into a full part of 2^21 slots
            // ran 6,619,144 units in a call of 20,000 (LR-29d). Its charge at most: the new
            // part for the most keys the old one holds (3/4), as `rebuild_nodes` and
            // `place_again` count it (the program's sizes, never the seed's).
            let old = t.nodes.len();
            let len = hash_slots(old * 3 / 4 + 1);
            let grown = t.arr.live_len() + t.int_len() + len;
            if (t.resized + rebuild_cost(old, len) + t.marking_cost(mem, grown)) / 16 > mem.fuel || part.as_ref().is_some_and(|p| p.prepaid) {
                return t.grow_refused(part, mem);
            }
            t.grow_counted::<false>(part, mem)
        })
    }

    /// `grow_nodes`'s count pass and rebuild (`W`: under a fuel watch).
    #[inline(always)]
    fn grow_counted<const W: bool>(&mut self, part: &mut Kept, mem: &mut Mem) -> Result<(), SetError> {
        // (Counted between looks too: a part of 2^21 slots is 1.4 ms to read.)
        let (mut at, mut live) = part.as_mut().and_then(|p| p.count.take()).unwrap_or_default();
        while let Some(n) = self.nodes.get(at) {
            live += usize::from(!n.val.is_nil());
            at += 1;
            if let Err(e) = look(mem, at - 1) {
                stash(part).count = Some((at, live));
                return Err(e);
            }
        }
        let len = hash_slots(live + 1);
        // (Under a fuel watch the marking of the growth is paid ahead: `prepay_marking`.)
        let r = if W { self.prepay_marking(self.marking_cost(mem, self.arr.live_len() + self.int_len() + len), part, mem) } else { Ok(()) };
        let r = r.and_then(|()| self.rebuild_nodes::<W, false>(len, 0, part, mem));
        match r {
            Ok(()) if W => self.marking_settled(mem),
            Ok(()) => {}
            Err(_) => stash(part).count = Some((at, live)),
        }
        r
    }

    /// A growth of `nodes` refused for the call's fuel: on the full path, made under a fuel
    /// watch instead, a piece at a time within the call's fuel, kept between calls and
    /// charged once (SEC8-F4: refused whole, it was never made under a fuel below its cost).
    /// Room is left for the marking of its growth, charged when it is in.
    #[cold]
    #[inline(never)]
    fn grow_refused(&mut self, part: &mut Kept, mem: &mut Mem) -> Result<(), SetError> {
        if !mem.fuel_watchable {
            return Err(SetError::Fuel);
        }
        mem.fuel_watch = Some(watch_most(mem.fuel, part.as_ref().map_or(0, |p| p.paid)));
        let r = self.grow_counted::<true>(part, mem);
        mem.fuel_watch = None;
        r
    }

    fn live_nodes(&self) -> usize {
        self.nodes.iter().filter(|n| !n.val.is_nil()).count()
    }

    /// `nodes` rebuilt at `len` slots (keys with a value only); under a new seed when a
    /// chain came out longer than `CHAIN_FREE + 1` (keys aimed at the seed for every size).
    /// Given up between pieces, the table is as it was and the new part, as far as it got,
    /// stays in `part`.
    /// (`I`: the integer part's keys placed into it too, charged `ints_cost`.)
    fn rebuild_nodes<const W: bool, const I: bool>(&mut self, len: usize, ints_cost: usize, part: &mut Kept, mem: &mut Mem) -> Result<(), SetError> {
        let old_len = self.nodes.len();
        let mut b = match part.as_mut().and_then(|p| p.nodes.take()) {
            Some(b) if b.len == len && b.ints_cost == if I { ints_cost } else { 0 } => b,
            stale => {
                if let Some(b) = stale {
                    mem.release(b.bytes);
                }
                // (Charged the growth first; a smaller part's bytes go when it is in.)
                let bytes = part_bytes(len).saturating_sub(part_bytes(old_len));
                mem.charge(bytes).map_err(|()| SetError::Memory)?;
                let mut b = NodesBuild::new(len, bytes);
                if I {
                    b.ints_cost = ints_cost;
                }
                b
            }
        };
        match self.place_again::<W, I>(&mut b, mem, true) {
            Ok(longest) => {
                // The old part gone, what was charged for the new one beyond its size back.
                mem.release((part_bytes(old_len) + b.bytes).saturating_sub(part_bytes(len)));
                // The old part's slots copied.
                self.resized += old_len;
                self.reseed_due = longest > CHAIN_FREE + 1;
                Ok(())
            }
            Err(e) => {
                // Stopped, the pieces done charged: the new part if made, and the old part's
                // slots read and placed so far (`rebuild_cost`'s terms, `resumed`).
                if matches!(e, SetError::Stopped(_)) {
                    // (Under a fuel watch, the pieces of the new part made so far: SEC8-F4.)
                    let made = if b.nodes.len() == len || W { b.nodes.len() } else { 0 };
                    let more = if I { self.int_len() } else { 0 };
                    self.resized += build_units(len, old_len, more, b.ints_cost, made, b.at);
                }
                // Kept, the new part is there beside the old one: counted whole, not by its
                // growth alone (SEC7-T3: a stopped rebuild of 2^21 slots held 64 MB past
                // the account until it was taken up again).
                let whole = part_bytes(len);
                if b.bytes < whole {
                    mem.settle(b.bytes, whole);
                    b.bytes = whole;
                }
                stash(part).nodes = Some(b);
                Err(e)
            }
        }
    }

    /// A new seed for `nodes` (from this one, so runs replay), and every key placed again
    /// under it at the same size, charged as a resize.
    fn reseed(&mut self, mem: &mut Mem) {
        self.seed = crate::heap::mix_seed(self.seed, 0x007e_5eed);
        let len = self.nodes.len();
        self.resized += len;
        self.reseed_due = false;
        // (Not given up: keys aimed at one home only, and the key that made the chain is
        // in already.)
        let _ = self.place_again::<false, false>(&mut NodesBuild::new(len, 0), mem, false);
    }

    /// The keys with a value placed again in the new `nodes` `b` (the old one back to the
    /// pool): the longest chain made.
    /// `watched`: between pieces, a stop from the host's interrupt or deadline gives it up,
    /// the old part kept as it was and `b` as far as it got (made a piece at a time too).
    /// (`W`: under a fuel watch, made and placed a piece at a time within it, `look_w`.)
    /// (`I`: the integer part's keys placed after the old part's, `NodesBuild::ints_cost`.)
    fn place_again<const W: bool, const I: bool>(&mut self, b: &mut NodesBuild, mem: &mut Mem, watched: bool) -> Result<usize, SetError> {
        self.restructuring(mem);
        // (Charged so far when stopped with `made` new slots made and `at` old slots placed:
        // `rebuild_cost`'s terms, as `rebuild_nodes` charges a stop.)
        let (base, old_len) = (self.resized, self.nodes.len());
        // (Out of the table while they are placed; back when it gives up or ends.)
        let ints = if I { self.ints.take() } else { None };
        let more: &[Node] = ints.as_ref().map_or(&[], |p| &p.nodes);
        let (all, ints_cost) = (old_len + more.len(), b.ints_cost);
        let units = |made: usize, at: usize| base + build_units(b.len, old_len, more.len(), ints_cost, made, at);
        if !watched {
            b.nodes = mem.pool_nodes(b.len);
        } else {
            let made = make_nodes::<W>(&mut b.nodes, b.len, mem, |made| units(made, b.at));
            if W && made.is_ok() && mem.fuel_watch.is_some_and(|most| units(b.len, (b.at + FUEL_PIECE).min(all)) > most) {
                if I {
                    self.ints = ints;
                }
                return Err(SetError::Stopped(crate::LimitKind::Fuel));
            }
            if let Err(e) = made {
                if I {
                    self.ints = ints;
                }
                return Err(e);
            }
        }
        let old = crate::nostd::mem::replace(&mut self.nodes, crate::nostd::mem::take(&mut b.nodes));
        let old_live = crate::nostd::mem::replace(&mut self.live, b.live.take());
        let old_counts = (self.used, self.free_top);
        (self.used, self.free_top) = (b.used, b.free_top);
        for i in b.at..all {
            let Some(n) = (if I && i >= old.len() { more.get(i - old.len()) } else { old.get(i) }).copied() else { break };
            if !n.val.is_nil() {
                b.longest = b.longest.max(self.raw_insert(n.key(), n.val).1);
            }
            // (The last look just before the new part is kept: until then it can be given up.)
            let last = i + 1 == all;
            if watched && let Err(e) = if last { look(mem, RESIZE_PIECE - 1) } else { look_at::<W>(mem, i, |at| units(b.len, at.min(all))) } {
                b.nodes = crate::nostd::mem::replace(&mut self.nodes, old);
                b.live = crate::nostd::mem::replace(&mut self.live, old_live);
                (b.used, b.free_top) = (self.used, self.free_top);
                (self.used, self.free_top) = old_counts;
                b.at = i + 1;
                if I {
                    self.ints = ints;
                }
                return Err(e);
            }
        }
        if I {
                    self.ints = ints;
                }
        // The new part made, and the placements (S5B-COST-2), counted as the most a part
        // of `old_len` slots holds when it grows (3/4) rather than the keys it held: when
        // it grows depends on the slots its dead keys took, so on the seed, and fuel must
        // not.
        // (The new part by the same size factor: past the caches it is memory mapped and
        // faulted in afresh, 22 to 28 ms for 2^21 slots, which at a slot a unit was 13x
        // plain code per unit.)
        let len = b.len;
        self.resized += len * (place_cost(len) / PLACE_COST) + place_cost(len) * (old.len() * 3 / 4);
        let before = self.capacity_slots() - len + old.len();
        give_nodes(mem, old);
        self.reshaped(mem, before);
        Ok(b.longest)
    }

    /// A node never used for a key colliding at `near`: one of the next `NEAR_FREE` slots
    /// (the same few cache lines, so a chain's nodes are mostly together), else going down
    /// from the top (Lua's `getfreepos`). Nodes above `free_top` never become free again
    /// (keys stay until a rebuild), so one exists while `used < nodes.len()`.
    fn free_pos(&mut self, near: usize) -> Option<usize> {
        let len = self.nodes.len();
        let mask = len.wrapping_sub(1);
        for d in 1..=NEAR_FREE.min(len) {
            let j = (near + d) & mask;
            if self.nodes.get(j).is_some_and(Node::is_free) {
                return Some(j);
            }
        }
        while (self.free_top as usize) < len {
            self.free_top += 1;
            let j = len - self.free_top as usize;
            if self.nodes.get(j).is_some_and(Node::is_free) {
                return Some(j);
            }
        }
        None
    }

    /// Node `n` put into slot `j` of `nodes`, the slot keeping its `SPILL` mark.
    #[inline]
    fn put(&mut self, j: usize, mut n: Node) {
        if let Some(x) = self.nodes.get_mut(j) {
            n.flags = (n.flags & !SPILL) | (x.flags & SPILL);
            *x = n;
        }
        if let Some(b) = &self.live {
            b.set(j);
        }
    }

    /// The length of the chain headed at slot `j` (0 when no key has that home).
    #[inline]
    fn chain_len(&self, j: usize) -> u8 {
        self.nodes.get(j).map_or(0, |n| if n.main { n.clen } else { 0 })
    }

    /// The home a node's key chose (the head of its chain), from the hash.
    fn chosen_home(&self, n: &Node, mask: usize) -> usize {
        let h = hash_key(n.key(), self.seed);
        let i = (h as usize) & mask;
        if n.flags & SECOND != 0 { second_home(i, h, mask) } else { i }
    }

    /// Lua's `luaH_newkey` in `nodes` for a key (not an integer of the integer part) known
    /// absent, with a node never used to spare (`used < nodes.len()`, which every caller
    /// keeps). The key chooses the shorter chain of its two homes (the first on a tie; see
    /// `CHAIN_FREE`). It goes to that home node if free; if the home holds a key from
    /// another home, that one moves to a free node and the new key takes its home; else the
    /// new key goes to a free node, second in its home's chain. The slot it went into, the
    /// length of its chain then, and the node another key moved to (`usize::MAX`: none).
    fn raw_insert(&mut self, key: Value, val: Value) -> (usize, usize, usize) {
        let len = self.nodes.len();
        let mask = len.wrapping_sub(1);
        let h = hash_key(key, self.seed);
        let first = (h as usize) & mask;
        let mut new = Node::new(key, val);
        // (A free first home wins without looking at the second.)
        let la = self.chain_len(first);
        let second = if la == 0 { first } else { second_home(first, h, mask) };
        let mp = if la != 0 && self.chain_len(second) < la {
            if let Some(n) = self.nodes.get_mut(first) {
                n.flags |= SPILL;
            }
            new.flags = SECOND;
            second
        } else {
            first
        };
        let Some(home) = self.nodes.get(mp).copied() else { return (0, 0, usize::MAX) };
        self.used += 1;
        if home.is_free() {
            new.main = true;
            new.clen = 1;
            self.put(mp, new);
            return (mp, 1, usize::MAX);
        }
        let Some(f) = self.free_pos(mp) else {
            // (Unreachable: callers keep a free node.)
            self.used -= 1;
            return (mp, 0, usize::MAX);
        };
        let at = |from: usize, off: i32| (from as i64 + i64::from(off)) as usize;
        let rel = |to: usize, from: usize| (to as i64 - from as i64) as i32;
        if !home.main {
            let other = self.chosen_home(&home, mask);
            // The home holds a key from `other`'s chain: it moves to `f`, relinked from the
            // node before it; the new key starts its own chain at home.
            let mut o = other;
            let mut steps = 0usize;
            loop {
                let nx = self.nodes.get(o).map_or(0, |n| n.next);
                if nx == 0 || at(o, nx) == mp {
                    break;
                }
                o = at(o, nx);
                steps += 1;
            }
            if steps > CHAIN_FREE {
                add_probes((steps - CHAIN_FREE) as u64);
            }
            if let Some(n) = self.nodes.get_mut(o) {
                n.next = rel(f, o);
            }
            let mut moved = home;
            moved.next = if home.next != 0 { rel(at(mp, home.next), f) } else { 0 };
            self.put(f, moved);
            new.main = true;
            new.clen = 1;
            self.put(mp, new);
            return (mp, 1, f);
        }
        // Second in the chain at home.
        let chain = home.clen.saturating_add(1);
        new.next = if home.next != 0 { rel(at(mp, home.next), f) } else { 0 };
        self.put(f, new);
        if let Some(x) = self.nodes.get_mut(mp) {
            x.next = rel(f, mp);
            x.clen = chain;
            x.flags |= CHAINED;
        }
        (f, usize::from(chain), usize::MAX)
    }

    /// Lua's `rehash` for a new key `extra`: counts the integer keys, picks the array
    /// size (`computesizes`) and resizes.
    fn rehash(&mut self, extra: Value, mem: &mut Mem) -> Result<(), SetError> {
        // Given up between pieces (the interrupt or deadline), the table is as it was: the
        // border hint too (the work done stays charged, `resumed`).
        let saved = (self.arr.alimit.get(), self.limit_hint.get());
        let r = self.resumed(mem, |t, part, mem| t.rehash_watched::<false>(extra, part, mem));
        if matches!(r, Err(SetError::Stopped(_))) {
            self.arr.alimit.set(saved.0);
            self.limit_hint.set(saved.1);
        }
        r
    }

    /// Runs a resize `f` that looks at the interrupt and deadline between pieces and gives
    /// up there, the table as it was: given `Partial`, what the last one given up on this
    /// table made, while the table is unwritten since (`RESUMABLE`), and keeping what it
    /// makes when it is given up too (or refused for fuel before it begins).
    fn resumed<R>(&mut self, mem: &mut Mem, f: impl FnOnce(&mut Table, &mut Kept, &mut Mem) -> Result<R, SetError>) -> Result<R, SetError> {
        let mut part = match crate::nostd::mem::take(&mut self.partial) {
            0 => None,
            h => match take_partial(mem, h) {
                Some(p) if self.absent.get() & RESUMABLE != 0 => Some(p),
                stale => {
                    if let Some(p) = stale {
                        mem.release(p.bytes());
                    }
                    None
                }
            },
        };
        self.absent.set(self.absent.get() & !RESUMABLE);
        let (before, paid) = (self.resized, part.as_ref().map_or(0, |p| p.paid));
        let r = f(self, &mut part, mem);
        // (Stopped for the fuel, it takes the call's fuel, as a refusal does: a stop's leftover
        // would be where in the call the resize came, which the seed moves. Its whole cost is
        // more, so it is paid ahead, and the attempt that ends it pays the rest. And no more:
        // a call's first piece is made before its first look, so under a call's fuel below a
        // piece's it went past it, counted paid but charged only the call's fuel, R5. Not
        // counted paid, it is charged to the attempts after. A stop before any work of its own
        // (a piece, or the marking paid ahead, past what the call has left) is paid ahead the
        // same: taken for a refusal, nothing kept paid, and a growth whose next piece was more
        // than a call's fuel never went in, SEC7-T4's key move at 1,000 a call.)
        if let Err(SetError::Stopped(crate::LimitKind::Fuel)) = r {
            self.resized = watch_most(mem.fuel, paid);
        }
        // What an attempt given up did stays charged (refunded, a resize given up and then
        // thrown away by a write was never paid: LR-29d), and once: an attempt charges every
        // stage it got through, those of the attempts before it too, so the charge is past
        // what they paid. The one that ends it pays the rest of the uninterrupted cost.
        // (A stage done in the table is not done again, so not charged again, but nothing
        // gives up after one: `resize` moves keys into `nodes` only when its array part
        // shrinks, and only a growing one is filled between looks.)
        let did = self.resized.saturating_sub(before);
        self.resized = before + did.saturating_sub(paid);
        if let (Err(SetError::Stopped(_)), Some(p)) = (&r, part.as_mut()) {
            p.paid = paid.max(did);
        }
        if let Some(p) = part {
            if matches!(r, Err(SetError::Stopped(_) | SetError::Fuel)) {
                if self.partial != 0 {
                    drop_partial(mem, self.partial);
                }
                self.partial = keep_partial(mem, p);
                self.absent.set(self.absent.get() | RESUMABLE);
            } else {
                mem.release(p.bytes());
            }
        }
        r
    }

    fn rehash_watched<const W: bool>(&mut self, extra: Value, part: &mut Kept, mem: &mut Mem) -> Result<(), SetError> {
        // Refused before its passes when it cannot pay for them alone: the counting pass is
        // in its charge (below), so such a call is refused there all the same, but only
        // after reading every part, again in every call the table was written between
        // (SEC7-T4: 33 to 39x plain code per unit of fuel, at a budget of 20,000).
        // Past it, on the full path, made a piece at a time within the call's fuel under a fuel
        // watch, kept between calls and charged once (SEC8-F4: refused whole, it was never made
        // under a fuel below its cost, about half a unit of fuel a key): each pass charges as
        // far as it got when it stops for the fuel. (`W`: the passes made for the watch.)
        if !W && ((self.resized + CENSUS_UNITS * (self.arr.live_len() + self.hash_len())) / 16 > mem.fuel || part.as_ref().is_some_and(|p| p.prepaid)) {
            return self.rehash_refused(extra, part, mem);
        }
        let (mut c, mut nums) = (Census::default(), [0usize; 33]);
        if let Some(kept) = part.as_mut().and_then(|p| p.census.take()) {
            (c, nums) = kept;
        }
        if let Err(e) = self.census::<W>(&mut c, &mut nums, mem) {
            if W && matches!(e, SetError::Stopped(crate::LimitKind::Fuel)) {
                self.resized += CENSUS_UNITS * c.at;
            }
            stash(part).census = Some((c, nums));
            return Err(e);
        }
        // (`extra` counted for the sizes, then taken out: the census stays the table's.)
        let extra_in = matches!(extra, Value::Int(k) if countint(k, &mut nums));
        let mut na = c.na + usize::from(extra_in);
        let total = c.total + 1;
        let asize = computesizes(&nums, &mut na);
        if let (true, Value::Int(k)) = (extra_in, extra) {
            uncount(k, &mut nums);
        }
        let (nhash, other_key) = (total - na, !matches!(extra, Value::Int(_)));
        let r = (|| {
            let count = CENSUS_UNITS * (self.arr.live_len() + self.hash_len());
            let scan = match self.resize_scan::<W>(asize, part, mem) {
                Err(SetError::Stopped(crate::LimitKind::Fuel)) if W => return Err(self.scan_stopped(asize, count, part)),
                r => r?,
            };
            // Charged after it was done, a rehash of a million keys (500,000 units) ran in
            // full past a call's fuel. Its charge is worked out first now, from what the
            // passes above found (the program's keys, never the seed's): the counting pass
            // over every part (S5B-COST-2) and `resize_cost`. A call that cannot pay stops
            // with nothing changed.
            let cost = count + self.resize_cost(asize, nhash, other_key, &scan, mem);
            // (The marking of the growth is paid ahead with the passes when the call cannot pay
            // for the whole, and once in it is marked paid, the watched way: `prepay_marking`.)
            let marking = if W { self.resize_marking(asize, nhash, other_key, &scan, mem) } else { 0 };
            let r = if (self.resized + cost) / 16 > mem.fuel
                && let Some(e) = self.short(self.resized, cost, count + self.resize_passes(asize, 0), self.resize_marking(asize, nhash, other_key, &scan, mem), part, mem)
            {
                Err(e)
            } else {
                self.resized += count;
                self.arr.alimit.set(self.arr.live_len());
                self.limit_hint.set(false);
                let r = self.resize(asize, nhash, other_key, scan, part, mem);
                if W && r.is_ok() {
                    self.resized += marking;
                    self.marking_settled(mem);
                }
                r
            };
            if r.is_err() {
                stash(part).scan = Some((asize, usize::MAX, scan));
            }
            r
        })();
        if matches!(r, Err(SetError::Stopped(_) | SetError::Fuel)) {
            stash(part).census = Some((c, nums));
        }
        r
    }

    /// A rehash refused before its passes for the call's fuel: on the full path, made under a
    /// fuel watch instead (what the attempts before paid counting towards it).
    #[cold]
    #[inline(never)]
    fn rehash_refused(&mut self, extra: Value, part: &mut Kept, mem: &mut Mem) -> Result<(), SetError> {
        if !mem.fuel_watchable {
            return Err(SetError::Fuel);
        }
        mem.fuel_watch = Some(watch_most(mem.fuel, part.as_ref().map_or(0, |p| p.paid)));
        let r = self.rehash_watched::<true>(extra, part, mem);
        mem.fuel_watch = None;
        r
    }

    /// `census`'s pass over the array part from `at`, a piece at a time: a piece of
    /// `FUEL_PIECE` slots from a multiple of it past the first is in one bucket of `nums`
    /// (its keys share their highest bit), so it is counted whole, and looked at once, at its
    /// last slot, as slot by slot (1.0 ms a call of 20,000 fuel over 2^22 slots, SEC7-T4's
    /// shape, was 1.5x what this takes). The slots before the first such piece and after the
    /// last, one by one. Out of line, under a fuel watch only (`census`).
    #[inline(never)]
    fn census_pieces<const W: bool>(&self, at: usize, nums: &mut [usize; 33], na: &mut usize, mem: &Mem) -> Result<(), (SetError, usize)> {
        let a = self.arr.live_len();
        let units = |k: usize| self.resized + CENSUS_UNITS * k.min(a);
        let from = at.max(FUEL_PIECE).next_multiple_of(FUEL_PIECE).min(a);
        let to = from + (a - from) / FUEL_PIECE * FUEL_PIECE;
        for (j, v) in self.arr.iter_from(at).take(from.saturating_sub(at)) {
            if !v.is_nil() {
                countint(j as i64 + 1, nums);
                *na += 1;
            }
            look_at::<W>(mem, j, units).map_err(|e| (e, j + 1))?;
        }
        for j in (from.max(at)..to).step_by(FUEL_PIECE) {
            let live: usize = self.arr.chunks(j..j + FUEL_PIECE).map(|(_, c)| c.iter().filter(|v| !v.is_nil()).count()).sum();
            if let Some(b) = nums.get_mut((64 - (j as u64).leading_zeros()) as usize) {
                *b += live;
            }
            *na += live;
            let last = j + FUEL_PIECE - 1;
            look_at::<W>(mem, last, units).map_err(|e| (e, last + 1))?;
        }
        for (j, v) in self.arr.iter_from(to.max(at)) {
            if !v.is_nil() {
                countint(j as i64 + 1, nums);
                *na += 1;
            }
            look_at::<W>(mem, j, units).map_err(|e| (e, j + 1))?;
        }
        Ok(())
    }

    /// The rehash's count of the keys, between looks, going on from where `c` got. (Counted
    /// in locals, `c` written when it ends or gives up.)
    /// (`W`: under a fuel watch, its charge as far as it got, `look_w`.)
    fn census<const W: bool>(&self, c: &mut Census, nums: &mut [usize; 33], mem: &Mem) -> Result<(), SetError> {
        let units = |at: usize| self.resized + CENSUS_UNITS * at;
        let at = c.at;
        let (a, n) = (self.arr.live_len(), self.nodes.len());
        let (mut na, mut total) = (c.na, c.total);
        let r = (|| {
            // (Under a fuel watch, past two pieces, counted a piece at a time: `census_pieces`.
            // Without a watch the pass is the loop it was: the check cost every small rehash.)
            if W && a > at.saturating_add(2 * FUEL_PIECE) {
                self.census_pieces::<W>(at, nums, &mut na, mem)?;
            } else {
                for (j, v) in self.arr.iter_from(at) {
                    if !v.is_nil() {
                        countint(j as i64 + 1, nums);
                        na += 1;
                    }
                    look_at::<W>(mem, j, |k| units(k.min(a))).map_err(|e| (e, j + 1))?;
                }
            }
            total += na - c.na;
            for (i, nd) in self.nodes.iter().enumerate().skip(at.max(a) - a) {
                total += usize::from(!nd.val.is_nil());
                look_at::<W>(mem, i, |k| units(a + k.min(n))).map_err(|e| (e, a + i + 1))?;
            }
            for (j, nd) in self.int_nodes().iter().enumerate().skip(at.max(a + n) - a - n) {
                if let (Value::Int(k), false) = (nd.key(), nd.val.is_nil()) {
                    total += 1;
                    if countint(k, nums) {
                        na += 1;
                    }
                }
                look_at::<W>(mem, j, |k| units(a + n + k)).map_err(|e| (e, a + n + j + 1))?;
            }
            Ok(())
        })();
        match r {
            Ok(()) => {
                c.at = a + n + self.int_len();
                (c.na, c.total) = (na, total);
                Ok(())
            }
            Err((e, at)) => {
                // (Given up in the array part, `total` has not taken its keys yet.)
                if at <= a {
                    total += na - c.na;
                }
                (c.at, c.na, c.total) = (at, na, total);
                Err(e)
            }
        }
    }

    /// `resize`'s first passes, read only: one over each part, between looks (the passes
    /// that look for other keys and count the integer keys leaving were 4 reads of a big
    /// `nodes`, 6 ms with no look).
    /// (`W`: under a fuel watch, charged as the census and `scan_units` as far as it got.)
    fn resize_scan<const W: bool>(&self, asize: usize, part: &mut Kept, mem: &Mem) -> Result<Scan, SetError> {
        // (Going on from a scan given up for this array size; `usize::MAX`: one done.)
        let (at, mut s) = match part.as_mut().and_then(|p| p.scan.take()) {
            Some((a, at, s)) if a == asize => (at, s),
            _ => (0, Scan::default()),
        };
        if at == usize::MAX {
            return Ok(s);
        }
        let (n, ilen) = (self.nodes.len(), self.int_len());
        let units = |k: usize| self.resized + CENSUS_UNITS * (self.arr.live_len() + self.hash_len()) + self.scan_units(asize, k);
        let r = (|| {
            for (i, nd) in self.nodes.iter().enumerate().skip(at) {
                if !nd.val.is_nil() {
                    s.live_before += 1;
                    match nd.key() {
                        Value::Int(k) => s.nodes_ints_out += usize::from((k as u64).wrapping_sub(1) >= asize as u64),
                        _ => s.strings = true,
                    }
                }
                look_at::<W>(mem, i, units).map_err(|e| (e, i + 1))?;
            }
            for (i, nd) in self.int_nodes().iter().enumerate().skip(at.max(n) - n) {
                s.ints_out += usize::from(matches!(nd.key(), Value::Int(k) if !nd.val.is_nil() && (k as u64).wrapping_sub(1) >= asize as u64));
                look_at::<W>(mem, i, |k| units(n + k)).map_err(|e| (e, n + i + 1))?;
            }
            for (i, v) in self.arr.iter_from(asize.max(at.max(n + ilen) - n - ilen)) {
                s.ints_out += usize::from(!v.is_nil());
                look_at::<W>(mem, i, |k| units(n + ilen + k - asize)).map_err(|e| (e, n + ilen + i + 1))?;
            }
            Ok(())
        })();
        if let Err((e, at)) = r {
            stash(part).scan = Some((asize, at, s));
            return Err(e);
        }
        s.ints_out += s.nodes_ints_out;
        Ok(s)
    }

    /// Under a fuel watch, the marking of the growth while the collector marks (`marking`,
    /// else charged by `take_resized` once the growth is in, whole: a growth whose marking
    /// was more than a call's fuel never went in, R1) paid ahead of the work, as far as the
    /// call's fuel goes, and in every attempt (as the attempts' charges are counted).
    fn prepay_marking(&mut self, marking: usize, part: &mut Kept, mem: &Mem) -> Result<(), SetError> {
        if marking == 0 {
            return Ok(());
        }
        stash(part).prepaid = true;
        let most = mem.fuel_watch.unwrap_or(usize::MAX);
        if self.resized + marking > most {
            return Err(SetError::Stopped(crate::LimitKind::Fuel));
        }
        self.resized += marking;
        Ok(())
    }

    /// A growth in whose marking was paid ahead (`prepay_marking`): `take_resized` charges
    /// it no more.
    fn marking_settled(&mut self, mem: &Mem) {
        if mem.marking && self.gc_snap.0 == mem.cycle {
            self.gc_snap.2 = self.gc_snap.2.max(u32::try_from(self.slots()).unwrap_or(u32::MAX));
        }
    }

    /// A growth the call's fuel cannot pay for `cost` after `base`: refused (`Fuel`); on the
    /// full path, gone on with when what the attempts before paid makes up the rest (`None`),
    /// else stopped for the fuel with `done` charged as far as the call's fuel goes, to wait
    /// for a call that can pay the rest (SEC8-F4).
    #[cold]
    #[inline(never)]
    fn short(&mut self, base: usize, cost: usize, done: usize, marking: usize, part: &mut Kept, mem: &Mem) -> Option<SetError> {
        if !mem.fuel_watchable {
            return Some(SetError::Fuel);
        }
        let most = mem.fuel_watch.unwrap_or_else(|| watch_most(mem.fuel, part.as_ref().map_or(0, |p| p.paid)));
        if base + cost <= most {
            return None;
        }
        // (The marking of the growth paid ahead with the rest: `prepay_marking`.)
        if marking > 0 {
            stash(part).prepaid = true;
        }
        self.resized += (done + marking).min(most.saturating_sub(base));
        Some(SetError::Stopped(crate::LimitKind::Fuel))
    }

    /// The scan stopped for the fuel: the census and the scan as far as it got charged.
    #[cold]
    #[inline(never)]
    fn scan_stopped(&mut self, asize: usize, count: usize, part: &Kept) -> SetError {
        let at = part.as_ref().and_then(|p| p.scan.as_ref()).map_or(0, |s| s.1);
        self.resized += count + self.scan_units(asize, at);
        SetError::Stopped(crate::LimitKind::Fuel)
    }

    /// The scan's charge stopped at `at` (SEC8-F4): the least of `resize`'s passes, which it
    /// stands in for, as far as it got.
    fn scan_units(&self, asize: usize, at: usize) -> usize {
        let len = self.nodes.len() + self.int_len() + self.arr.live_len().saturating_sub(asize);
        self.resize_passes(asize, 0) * at.min(len) / len.max(1)
    }

    /// What `resize` charges to `resized`, and `take_resized` after it: its passes, the room
    /// it makes in `nodes` for the keys moving in, the keys it places, and the marking of
    /// its growth while the collector marks.
    fn resize_cost(&self, asize: usize, nhash: usize, other_key: bool, s: &Scan, mem: &Mem) -> usize {
        let new_len = self.new_int_len(nhash, other_key, s);
        let exact = !s.strings && !other_key;
        // The room `resize` makes in `nodes` for the keys moving in, first (LR-29d: it was
        // charged after, unchecked), and those keys placed at its new size.
        let inserts = if exact { 0 } else { s.ints_out - s.nodes_ints_out };
        let nodes = if inserts > 0 && (self.used as usize + inserts) * 4 > self.nodes.len() * 3 { hash_slots(s.live_before + inserts) } else { self.nodes.len() };
        let rebuild = if nodes == self.nodes.len() { 0 } else { rebuild_cost(self.nodes.len(), nodes) };
        self.resize_passes(asize, new_len)
            + rebuild
            + if exact { int_place_cost(new_len) * s.ints_out } else { place_cost(nodes) * inserts }
            + self.resize_marking(asize, nhash, other_key, s, mem)
    }

    /// `resize_cost`'s marking of the growth while the collector marks.
    fn resize_marking(&self, asize: usize, nhash: usize, other_key: bool, s: &Scan, mem: &Mem) -> usize {
        let new_len = self.new_int_len(nhash, other_key, s);
        let exact = !s.strings && !other_key;
        let inserts = if exact { 0 } else { s.ints_out - s.nodes_ints_out };
        let nodes = if inserts > 0 && (self.used as usize + inserts) * 4 > self.nodes.len() * 3 { hash_slots(s.live_before + inserts) } else { self.nodes.len() };
        self.marking_cost(mem, asize + new_len + if exact { 0 } else { nodes })
    }

    /// `resize`'s passes: each part once, and the passes that look for other keys and count
    /// the integer keys leaving (S5B-COST-2).
    fn resize_passes(&self, asize: usize, new_len: usize) -> usize {
        let (old_asize, old_len) = (self.arr.live_len(), self.int_len());
        old_asize.max(asize) + 2 * old_len + new_len + 3 * self.nodes.len() + old_asize.saturating_sub(asize)
    }

    /// The integer part `resize` makes: Lua's node array for `nhash` keys, while every
    /// hash key is an integer and some go there (made when the first needs it otherwise).
    fn new_int_len(&self, nhash: usize, other_key: bool, s: &Scan) -> usize {
        if !s.strings && !other_key && s.ints_out > 0 { lua_nodes(nhash) } else { 0 }
    }

    /// Lua's `luaH_resize`: the array part to exactly `asize` slots, Lua's node array to
    /// hold `nhash` keys. While every hash key is an integer (`other_key` false, no other
    /// key with a value) the integer part becomes Lua's node array, keys placed in Lua's
    /// order (the vanishing slice of the array, then the old nodes by index); else every
    /// hash key is in `nodes` and Lua's keyed nodes are counted. Keys whose value was
    /// removed go, as in Lua.
    fn resize(&mut self, asize: usize, nhash: usize, other_key: bool, scan: Scan, part: &mut Kept, mem: &mut Mem) -> Result<(), SetError> {
        self.restructuring(mem);
        let old_asize = self.arr.live_len();
        let csize = lua_nodes(nhash);
        let in_new = |k: i64| (k as u64).wrapping_sub(1) < asize as u64;
        let Scan { strings, live_before, ints_out, nodes_ints_out } = scan;
        let exact = !strings && !other_key;
        let old_len = self.int_len();
        let new_len = self.new_int_len(nhash, other_key, &scan);
        // What a resize to these sizes given up on this table made goes on (charged then).
        let mut rb = match part.as_mut().and_then(|p| p.resize.take()) {
            Some(b) if (b.asize, b.len, b.exact) == (asize, new_len, exact) => b,
            stale => {
                if let Some(b) = stale {
                    mem.release(b.bytes);
                }
                // (The new tail a resize given up made, thrown away with it, and its bytes:
                // segmented, its blocks go, SEC8-F6.)
                let (slots, size) = (self.capacity_slots(), self.mem_size());
                self.arr.drop_hidden();
                self.reshaped(mem, slots);
                mem.release(size.saturating_sub(self.mem_size()));
                ResizeBuild { asize, len: new_len, exact, ..ResizeBuild::default() }
            }
        };
        // Room in `nodes` for every key moving into it (from the array part and the old
        // integer part), made before anything changes, as C Lua's resize allocates first:
        // grown while they went in, a refused charge past the swap of parts dropped the
        // rest of them, the table's keys lost (SEC7-T1). Refused or stopped here, the table
        // is as it was. (The same rebuild an insert's growth makes, charged as it is.)
        let inserts = if exact { 0 } else { ints_out - nodes_ints_out };
        if inserts > 0 && (self.used as usize + inserts) * 4 > self.nodes.len() * 3 {
            if let Err(e) = self.rebuild_nodes::<false, false>(hash_slots(live_before + inserts), 0, part, mem) {
                stash(part).resize = Some(rb);
                return Err(e);
            }
        }
        // (The rebuild above passed its own slots to `reshaped`.)
        let slots0 = self.capacity_slots();
        let held = crate::nostd::mem::take(&mut rb.bytes);
        let resized0 = self.resized;
        // (The placements are charged as they are made.)
        self.resized += self.resize_passes(asize, new_len);
        // Charged before anything changes (`nodes` may still grow below, charged there).
        let before = self.mem_size();
        let old_size = self.arr.capacity() * SLOT_SIZE + old_len * INODE_SIZE;
        let new_size = self.arr.capacity_for(asize) * SLOT_SIZE + new_len * INODE_SIZE;
        let charged = new_size.saturating_sub(old_size).saturating_sub(held);
        if mem.charge(charged).is_err() {
            mem.release(held);
            return Err(SetError::Memory);
        }
        // Every step before the parts are swapped in can be given up between pieces (the
        // interrupt or deadline): the table is then as it was but for its array buffer's
        // capacity, and what was made is kept in `rb` for the next resize to go on from.
        let r = if exact { self.build_exact(&mut rb, old_asize, mem) } else { self.build_tail(&mut rb, old_asize, mem) };
        if let Err(e) = r {
            // (The new tail stays, hidden past the array part, for the next one: made again
            // from the start, its first pieces were 19 ms with no look at 2^21 slots.)
            self.resized = resized0;
            // (Kept: the new buffer being copied and the new integer part, whole: they are
            // there beside the old ones, SEC7-T3.)
            rb.bytes = rb.copy.as_ref().map_or(0, |c| c.capacity() * SLOT_SIZE) + rb.ints.as_ref().map_or(0, |_| new_len * INODE_SIZE);
            mem.settle(before + held + charged, self.mem_size() + rb.bytes);
            self.reshaped(mem, slots0);
            stash(part).resize = Some(rb);
            return Err(e);
        }
        let (placed, ints) = (rb.placed, rb.ints.take());
        let old_ints = self.ints.take();
        self.arr.commit();
        if exact {
            // Integer keys in `nodes` (it held no other key with a value) went to the new
            // parts; the part itself goes.
            if asize < old_asize {
                self.arr.truncate(asize);
            }
            give_nodes(mem, crate::nostd::mem::take(&mut self.nodes));
            self.live = None;
            self.used = 0;
            self.free_top = 0;
            self.arr.alimit.set(asize);
            self.limit_hint.set(false);
            self.resized += int_place_cost(new_len) * placed;
            self.ints = ints.filter(|_| new_len != 0).map(Box::new);
            self.counting = false;
            self.csize = csize as u32;
            self.ckeys = 0;
        } else {
            let mut moving: Vec<(Value, Value)> = Vec::new();
            if asize < old_asize {
                for j in asize..old_asize {
                    let v = self.arr.get(j).copied().unwrap_or_default();
                    if !v.is_nil() {
                        moving.push((Value::Int((j + 1) as i64), v));
                    }
                }
                self.arr.truncate(asize);
            }
            // From here the parts change: nothing below gives up (the inserts' own rebuilds
            // run unwatched).
            let deadline = mem.deadline.take();
            let mut live = live_before;
            self.arr.alimit.set(asize);
            self.limit_hint.set(false);
            if let Some(old) = &old_ints {
                for n in &old.nodes {
                    if let (Value::Int(_), false) = (n.key(), n.val.is_nil()) {
                        moving.push((n.key(), n.val));
                    }
                }
            }
            // Integer keys of `nodes` the array now covers move in; the rest stays.
            if asize > old_asize {
                for n in &mut self.nodes {
                    if let Value::Int(i) = n.key()
                        && !n.val.is_nil()
                        && in_new(i)
                    {
                        // (Its value is in the array already, `build_tail`.)
                        n.val = Value::Nil;
                        live -= 1;
                    }
                }
            }
            self.counting = true;
            self.csize = csize as u32;
            for (k, v) in moving {
                match k {
                    Value::Int(i) if in_new(i) => {
                        if let Some(s) = self.arr.get_mut((i - 1) as usize) {
                            *s = v;
                        }
                    }
                    _ => {
                        // (From the array or the old integer part: not in `nodes` yet.)
                        self.resized += place_cost(self.nodes.len());
                        // (`nodes` has room for every key moving in: made above. No grow, so
                        // no error, past the swap: one here lost the rest of `moving`.)
                        if let Err(e) = self.hash_insert(k, v, mem) {
                            mem.deadline = deadline;
                            return Err(e);
                        }
                        live += 1;
                    }
                }
            }
            mem.deadline = deadline;
            // (The live nodes, counted as they moved: a pass over a big part was 1.4 ms.)
            debug_assert_eq!(live, self.live_nodes());
            self.ckeys = live as u32;
        }
        if let Some(old) = old_ints {
            give_nodes(mem, old.nodes);
        }
        // What it holds now against what was charged: the difference is settled exactly.
        mem.settle(before + held + charged, self.mem_size());
        self.reshaped(mem, slots0);
        Ok(())
    }

    /// The array part grown to `rb.asize` slots: a buffer of 1 to 32 MiB copied a piece at
    /// a time (`regrow_arr_watched`), then the new tail a piece at a time, its pages faulted
    /// in as it is written (16 ms for 2^21 slots in one go). Given up, what it made stays
    /// hidden past the array part and the next one goes on from there (it was cut off and
    /// filled again first with no look: 19 ms at 2^21 slots, the gate's 1 ms slices).
    #[inline]
    fn grow_arr(&mut self, rb: &mut ResizeBuild, mem: &mut Mem) -> Result<(), SetError> {
        let asize = rb.asize;
        if asize > self.arr.capacity() {
            let t0 = mem.trace_begin();
            let r = self.arr.reserve_watched(mem, asize, &mut rb.copy);
            mem.trace_note(crate::gctrace::TraceOp::Reserve, 0, (asize * SLOT_SIZE) as u64, t0);
            r?;
        }
        while self.arr.backing_len() < asize {
            let n = (asize - self.arr.backing_len()).min(RESIZE_PIECE);
            let to = self.arr.backing_len() + n;
            let t0 = mem.trace_begin();
            self.arr.stage_extend(to);
            // (Its slots 0: a record's slots are those the collector scanned.)
            mem.trace_note(crate::gctrace::TraceOp::Resize, 0, (n * SLOT_SIZE) as u64, t0);
            STAGED.with(|s| s.set(s.get() + n as u64));
            if to < asize
                && let Some(k) = mem.stopped()
            {
                return Err(SetError::Stopped(k));
            }
        }
        Ok(())
    }

    /// `resize` while every hash key is an integer: the integer part made, the array part
    /// grown, and the keys placed straight from where they are (the residual of
    /// S5B-COST-2: a list of them all, grown by doubling, was most of a rehash's page
    /// faults at a million keys), in Lua's order: the vanishing slice of the array, then
    /// the old integer part by index, then the integer keys of `nodes`. Each placement's
    /// chain steps count (S5GC-1). Going on from `rb`, the new array tail's values placed
    /// before the give-up are in it still (`keep_write` writes there too).
    fn build_exact(&mut self, rb: &mut ResizeBuild, old_asize: usize, mem: &mut Mem) -> Result<(), SetError> {
        let (asize, len) = (rb.asize, rb.len);
        if len > 0 {
            let p = rb.ints.get_or_insert_with(|| IntPart { nodes: Vec::new(), next: Vec::new(), lastfree: len });
            make_nodes::<false>(&mut p.nodes, len, mem, |_| 0)?;
            if p.next.len() != len {
                p.next = vec![0; len];
            }
        }
        if asize > old_asize {
            self.grow_arr(rb, mem)?;
        }
        let shrink = old_asize.saturating_sub(asize);
        let ilen = self.int_len();
        let src = |t: &Table, i: usize| -> (Value, Value) {
            if i < shrink {
                (Value::Int((asize + i + 1) as i64), t.arr.get(asize + i).copied().unwrap_or_default())
            } else {
                let n = if i < shrink + ilen { t.int_nodes().get(i - shrink) } else { t.nodes.get(i - shrink - ilen) };
                n.map_or((Value::Nil, Value::Nil), |n| (n.key(), n.val))
            }
        };
        let total = shrink + ilen + self.nodes.len();
        let before = take_probes();
        let (mut probes, mut placed) = (rb.probes, rb.placed);
        for i in rb.seen..total {
            if let (Value::Int(k), v) = src(self, i)
                && !v.is_nil()
            {
                if (k as u64).wrapping_sub(1) < asize as u64 {
                    if let Some(s) = self.arr.stage_get_mut((k - 1) as usize) {
                        *s = v;
                    }
                } else if let Some(p) = rb.ints.as_mut() {
                    p.place(k, v, &mut None);
                    placed += 1;
                    probes = probes.saturating_add(take_probes());
                }
            }
            if let Err(e) = look(mem, i) {
                (rb.seen, rb.probes, rb.placed) = (i + 1, probes, placed);
                put_probes(before);
                return Err(e);
            }
        }
        (rb.seen, rb.placed) = (total, placed);
        put_probes(before.max(probes));
        Ok(())
    }

    /// `resize` with other keys in `nodes`, up to where the parts change: the array part
    /// grown and the values of `nodes`' integer keys it now covers put in its new tail
    /// (`nodes` untouched until they are taken out of it). Going on from `rb`, those read
    /// before the give-up are in the hidden tail still.
    #[inline]
    fn build_tail(&mut self, rb: &mut ResizeBuild, old_asize: usize, mem: &mut Mem) -> Result<(), SetError> {
        let asize = rb.asize;
        if asize <= old_asize {
            return Ok(());
        }
        self.grow_arr(rb, mem)?;
        for i in rb.seen..self.nodes.len() {
            let Some(n) = self.nodes.get(i).copied() else { break };
            if let Value::Int(k) = n.key()
                && !n.val.is_nil()
                && (k as u64).wrapping_sub(1) < asize as u64
                && let Some(s) = self.arr.stage_get_mut((k - 1) as usize)
            {
                *s = n.val;
            }
            if let Err(e) = look(mem, i) {
                rb.seen = i + 1;
                return Err(e);
            }
        }
        Ok(())
    }

    /// `#t`: Lua's `luaH_getn`, border hint and all, so the border is C Lua's.
    #[inline]
    pub fn border(&self) -> i64 {
        match self.border_quick() {
            Some(b) => b,
            None => self.border_full(),
        }
    }

    /// `border`'s common cases, inline (nothing moves, nothing probes): the slot at the hint
    /// present and the one after it absent in the array part, or the array part full with
    /// no hash part to look in. None: `border_full`.
    #[inline]
    fn border_quick(&self) -> Option<i64> {
        let limit = self.arr.alimit.get();
        let at = |k: usize| self.arr.get(k).is_some_and(|v| !v.is_nil());
        if limit == 0 || !at(limit - 1) {
            return None;
        }
        if !self.limit_is_size() {
            return (!at(limit)).then_some(limit as i64);
        }
        (self.ints.is_none() && self.nodes.is_empty()).then_some(limit as i64)
    }

    #[inline(never)]
    fn border_full(&self) -> i64 {
        let arr = &self.arr;
        let empty = |k: usize| arr.get(k).is_none_or(|v| v.is_nil());
        let mut limit = self.arr.alimit.get();
        if limit > 0 && empty(limit - 1) {
            // A border before the hint.
            if limit >= 2 && !empty(limit - 2) {
                if self.pow2_size() && !ispow2(limit - 1) {
                    self.arr.alimit.set(limit - 1);
                    self.limit_hint.set(true);
                }
                return (limit - 1) as i64;
            }
            let border = binsearch(arr, 0, limit);
            if self.pow2_size() && border > arr.live_len() / 2 {
                self.arr.alimit.set(border);
                self.limit_hint.set(true);
            }
            return border as i64;
        }
        if !self.limit_is_size() {
            // The hint is below the array's size.
            if empty(limit) {
                return limit as i64;
            }
            limit = arr.live_len();
            if empty(limit - 1) {
                let border = binsearch(arr, self.arr.alimit.get(), limit);
                self.arr.alimit.set(border);
                return border as i64;
            }
            self.arr.alimit.set(limit);
        }
        if self.get_int(limit as i64 + 1).is_nil() {
            return limit as i64;
        }
        self.hash_search(limit as u64) as i64
    }

    /// Lua's `hash_search`: `j` is present (or 0) and `j + 1` too; doubles until absent,
    /// then a binary search. Each lookup's chain steps count (S5GC-1: up to 126 lookups
    /// were charged as the longest one).
    fn hash_search(&self, j: u64) -> u64 {
        summed(|step| {
            let mut absent = |k: u64| {
                let nil = self.get_int(k as i64).is_nil();
                step();
                nil
            };
            let mut i;
            let mut j = j.max(1);
            let max = i64::MAX as u64;
            loop {
                i = j;
                if j <= max / 2 {
                    j *= 2;
                } else {
                    j = max;
                    if absent(j) {
                        break;
                    }
                    return j;
                }
                if absent(j) {
                    break;
                }
            }
            while j - i > 1 {
                let m = (i + j) / 2;
                if absent(m) {
                    j = m;
                } else {
                    i = m;
                }
            }
            i
        })
    }

    /// The entry after `key` in traversal order (`next`) and the fuel its scan costs: `Ok(None)`
    /// at the end, `Err` for a key the table does not have. Order: the array part, the other keys,
    /// the integer keys.
    ///
    /// Skipping empty slots is work. In the array part (whose layout never depends on the
    /// seed) a unit per 16 positions crossed, counted from the start of the array, so a
    /// whole traversal costs the same however the calls split it, and a dense one costs
    /// nothing. (A free allowance of 1024 slots a call let `pairs` over an array with an
    /// entry every 1024 slots run at 30x plain code per unit of fuel.) Among the other keys
    /// gaps follow the seed, so they are free: `LiveBits` skips them 64 slots a word (S6T-2:
    /// a unit per 16 slots of a gap past 64 made a traversal's fuel follow the seed). In the
    /// integer part (whose layout never depends on the seed) a run past 64 empty slots
    /// costs a unit per 16.
    /// The node `next` last returned, when it holds `key` (`next_hint`): `key` is then the
    /// table's own key, interned if a string.
    #[inline]
    pub fn at_next_hint(&self, key: Value) -> Option<usize> {
        let h = self.live.as_ref()?.hint.get() as usize;
        self.nodes.get(h).filter(|n| n.holds(pack(key))).map(|_| h)
    }

    pub fn next_counted(&self, key: Value) -> Result<(Option<(Value, Value)>, usize), ()> {
        let arr_cost = |from: usize, n: usize| if n > 1 { ((from + n) >> 4) - (from >> 4) } else { 0 };
        let mut scanned = 0usize;
        let mut cost = 0usize;
        // Where to go on in `hash_nodes`.
        let start: usize;
        let key = normalize_key(key);
        let in_arr = match key {
            Value::Nil => Some(0),
            Value::Int(i) if i >= 1 && (i as u64) <= self.arr.live_len() as u64 => Some(i as usize),
            _ => None,
        };
        if let Some(from) = in_arr {
            let r = self.scan_arr(from, &mut scanned);
            cost += arr_cost(from, scanned);
            if let Some(r) = r {
                return Ok((Some(r), cost));
            }
            scanned = 0;
            start = 0;
        } else {
            match key {
                Value::Int(i) if self.ints.is_some() => match self.ints.as_ref().and_then(|p| p.find(i)) {
                    Some(j) => start = self.nodes.len() + j + 1,
                    None => return Err(()),
                },
                k => match self.at_next_hint(k).or_else(|| self.find(k)) {
                    Some(j) => start = j + 1,
                    None => return Err(()),
                },
            }
        }
        let nlen = self.nodes.len();
        let mut j = start;
        if j < nlen {
            match &self.live {
                Some(bits) => {
                    while let Some(p) = bits.next_set(j) {
                        match self.nodes.get(p) {
                            Some(n) if !n.val.is_nil() => {
                                bits.hint.set(p as u32);
                                return Ok((Some((n.key(), n.val)), cost));
                            }
                            Some(_) => bits.clear(p),
                            None => break,
                        }
                        j = p + 1;
                    }
                }
                None => {
                    for n in self.nodes.iter().skip(j) {
                        if !n.val.is_nil() {
                            return Ok((Some((n.key(), n.val)), cost));
                        }
                    }
                }
            }
            j = nlen;
        }
        let hash_cost = |n: usize| if n > 64 { n / 16 } else { 0 };
        for n in self.int_nodes().iter().skip(j - nlen) {
            scanned += 1;
            if !n.val.is_nil() {
                return Ok((Some((n.key(), n.val)), cost + hash_cost(scanned)));
            }
        }
        Ok((None, cost + hash_cost(scanned)))
    }

    /// Whether the hash part holds a live integer key in `from + 1..=to` (the slots an
    /// array part of `from` grows to take; keys `1..=from` are in the array part already).
    /// S6T-1: this scanned every node, free, at each 50-item flush of a constructor (and an
    /// integer part sent every flush through a full resize), so one with 200,000 named
    /// fields and more than `u16::MAX` items ran at 50x plain code per unit of fuel. Now the
    /// cheaper of looking each key up and scanning, charged (`resized`).
    fn ints_in_hash(&mut self, from: usize, to: usize) -> bool {
        let span = to.saturating_sub(from);
        let live = |n: &Node| !n.val.is_nil() && matches!(n.key(), Value::Int(k) if (k as u64).wrapping_sub(1) < to as u64);
        let keys = (from + 1..=to).map(|k| k as i64);
        let (len, found) = match self.ints.as_ref() {
            Some(p) if span <= p.nodes.len() / 4 => {
                (span * 4, keys.clone().any(|k| p.find(k).is_some_and(|j| p.nodes.get(j).is_some_and(|n| !n.val.is_nil()))))
            }
            Some(p) => (p.nodes.len(), p.nodes.iter().any(live)),
            None if self.nodes.is_empty() => (0, false),
            None if span <= self.nodes.len() / 4 => {
                (span * 4, keys.clone().any(|k| self.find(Value::Int(k)).is_some_and(|i| self.nodes.get(i).is_some_and(|n| !n.val.is_nil()))))
            }
            None => (self.nodes.len(), self.nodes.iter().any(live)),
        };
        self.resized += len;
        found
    }

    fn scan_arr(&self, from: usize, scanned: &mut usize) -> Option<(Value, Value)> {
        for (j, v) in self.arr.iter_from(from) {
            *scanned += 1;
            if !v.is_nil() {
                return Some((Value::Int(j as i64 + 1), *v));
            }
        }
        None
    }

    /// Every live entry (for the collector and for copying out).
    pub fn entries(&self) -> impl Iterator<Item = (Value, Value)> + '_ {
        let a = self.arr.iter().enumerate().filter(|(_, v)| !v.is_nil()).map(|(i, v)| (Value::Int(i as i64 + 1), *v));
        let h = self.hash_nodes().filter(|n| !n.val.is_nil()).map(|n| (n.key(), n.val));
        a.chain(h)
    }

    /// Stores `vals` at keys `off+1…` (a table constructor's list part), as Lua's
    /// `set_list` for the fast loop, when it allocates nothing, probes nothing and leaves
    /// the resize slots under 16 (so `raw_set`'s charge after it is nothing): the list in
    /// the array part, or grown into the buffer's room with no hash part to look in
    /// (`ints_in_hash` finds nothing and scans nothing then). False: nothing done.
    #[inline]
    pub fn set_list_in_place(&mut self, off: usize, vals: &[Value], mem: &mut Mem) -> bool {
        let last = off + vals.len();
        let len = self.arr.live_len();
        let grow = last.saturating_sub(len);
        if self.resized + grow >= 16 || (grow > 0 && (last > self.arr.capacity() || self.ints.is_some() || !self.nodes.is_empty())) {
            return false;
        }
        self.absent.set(0);
        if grow > 0 {
            self.snap_size(mem.cycle);
            self.resized += grow;
            let before = self.capacity_slots();
            self.arr.resize_nil(last);
            self.reshaped(mem, before);
            self.arr.alimit.set(last);
            self.limit_hint.set(false);
        }
        self.arr.copy_in(off, vals)
    }

    /// `SETLIST`: the array part grows to take them all (`luaH_resizearray`, Lua's node
    /// array kept), then they go straight in, nils included.
    pub fn set_list(&mut self, off: usize, vals: &[Value], mem: &mut Mem) -> Result<(), SetError> {
        let kept = self.absent.get() & RESUMABLE;
        self.absent.set(0);
        let last = off + vals.len();
        if last > self.arr.live_len() {
            // Refused before anything is done when the call cannot pay for it (LR-29d: the
            // scan for integer keys, the copy and the new slots were charged after): their
            // charge as `ints_in_hash` and the growth below count it, and the marking of the
            // growth while the collector marks.
            let span = last - self.arr.live_len();
            let h = if self.ints.is_some() { self.int_len() } else { self.nodes.len() };
            let scan = if h == 0 || span <= h / 4 { span.min(h) * 4 } else { h };
            let copy = if last > self.arr.capacity() { self.arr.live_len() } else { 0 };
            if (self.resized + scan + copy + span + self.marking_cost(mem, last + self.hash_len())) / 16 > mem.fuel {
                return Err(SetError::Fuel);
            }
            self.restructuring(mem);
            if !self.ints_in_hash(self.arr.live_len(), last) {
                // Only the array part changes (`luaH_resizearray` with nothing to move).
                // A constructor past the `u16::MAX` items `NewTable` presizes grows 50 at a
                // time: the buffer grows by half again (charged), so the copies stay
                // linear in the list (S6T-1).
                let before = self.capacity_slots();
                if last > self.arr.capacity() {
                    let cap = self.arr.grown_capacity(last);
                    mem.charge((cap - self.arr.capacity()) * SLOT_SIZE).map_err(|()| SetError::Memory)?;
                    self.resized += self.arr.live_len();
                    self.arr.regrow(mem, cap);
                }
                self.resized += last - self.arr.live_len();
                self.arr.resize_nil(last);
                self.reshaped(mem, before);
                self.arr.alimit.set(last);
                self.limit_hint.set(false);
            } else {
                self.arr.alimit.set(self.arr.live_len());
                self.limit_hint.set(false);
                let csize = self.csize as usize;
                let other = self.counting;
                self.absent.set(kept);
                self.resumed(mem, |t, part, mem| {
                    let scan = t.resize_scan::<false>(last, part, mem)?;
                    // (As a rehash's: refused before it changes anything, LR-29d.)
                    let r = if (t.resized + t.resize_cost(last, csize, other, &scan, mem)) / 16 > mem.fuel {
                        Err(SetError::Fuel)
                    } else {
                        t.resize(last, csize, other, scan, part, mem)
                    };
                    if r.is_err() {
                        stash(part).scan = Some((last, usize::MAX, scan));
                    }
                    r
                })?;
            }
        }
        for (i, v) in vals.iter().enumerate() {
            if let Some(s) = self.arr.get_mut(off + i) {
                *s = *v;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]
    use super::*;
    use crate::heap::Mem;
    use crate::value::StrRef;

    fn mem() -> Mem {
        Mem::new(usize::MAX / 4, 0)
    }

    fn home(t: &Table, n: &Node) -> usize {
        t.chosen_home(n, t.nodes.len() - 1)
    }

    /// Every chain in `nodes` holds exactly the keys that chose its head, its head keeps
    /// its length, and the first home of every key in its second home's chain is marked
    /// `SPILL`: the longest.
    fn check_chains(t: &Table) -> usize {
        let mut longest = 0;
        let mut seen = 0;
        let mask = t.nodes.len().wrapping_sub(1);
        for (i, n) in t.nodes.iter().enumerate() {
            if n.is_free() {
                continue;
            }
            assert_eq!(n.main, home(t, n) == i, "main flag");
            assert_eq!(n.flags & CHAINED != 0, n.main && n.next != 0, "chained flag");
            if n.flags & SECOND != 0 {
                let first = (hash_key(n.key(), t.seed) as usize) & mask;
                assert!(t.nodes[first].flags & SPILL != 0, "spill mark");
            }
            if !n.main {
                continue;
            }
            let mut j = i;
            let mut len = 0;
            loop {
                let m = &t.nodes[j];
                assert_eq!(home(t, m), i, "a chain holds only its home's keys");
                len += 1;
                if m.next == 0 {
                    break;
                }
                j = (j as i64 + i64::from(m.next)) as usize;
            }
            assert_eq!(usize::from(n.clen), len, "chain length");
            seen += len;
            longest = longest.max(len);
        }
        assert_eq!(seen, t.nodes.iter().filter(|n| !n.is_free()).count(), "every node is in its home's chain");
        assert_eq!(seen, t.used as usize);
        longest
    }

    /// S5GC-5, S5B-OPEN-2: chains of random keys (floats, strings, tables, with removals)
    /// stay at most 4 long (two-choice: see `CHAIN_FREE`), no insert reseeds, and every key
    /// is found.
    #[test]
    fn chain_lengths_of_random_keys() {
        let mut worst = 0;
        for s in 0..24u64 {
            let mut m = mem();
            let seed = crate::heap::mix_seed(s, 99);
            let mut t = Table { seed, ..Table::default() };
            let n = 3 << 14;
            let key = |i: u64| match (s + i) % 3 {
                0 => Value::Num(i as f64 + 0.25),
                1 => Value::Str(StrRef { idx: (i * 7 + 3) as u32, generation: 1 }),
                _ => Value::Table(TableRef { idx: i as u32, generation: (s % 5) as u32 }),
            };
            for i in 0..n {
                assert!(t.set(key(i), Value::Int(i as i64), &mut m).is_ok());
                if i % 5 == 4 {
                    assert!(t.set(key(i - 2), Value::Nil, &mut m).is_ok());
                }
            }
            assert_eq!(t.seed, seed, "random keys reseeded the part");
            worst = worst.max(check_chains(&t));
            for i in 0..n {
                let want = if i % 5 == 2 && i + 2 < n { Value::Nil } else { Value::Int(i as i64) };
                assert!(t.get(key(i)).raw_eq(want), "key {i}");
            }
        }
        assert!(worst <= 4, "longest chain {worst}");
    }

    /// Where each key of `t` sits: its node in `nodes`, or in the integer part (offset past
    /// `nodes`). (None for a key in the array part.)
    fn positions(t: &Table, keys: &[Value]) -> Vec<Option<usize>> {
        keys.iter()
            .map(|&k| match k {
                Value::Int(i) if t.arr_index(i).is_some() => None,
                Value::Int(i) if t.ints.is_some() => t.ints.as_ref().and_then(|p| p.find(i)).map(|j| t.nodes.len() + j),
                k => t.find(k),
            })
            .collect()
    }

    /// S6GC-2e-R: while the collector traverses a table in chunks (`gc_watch`), an insertion
    /// that moves another entry to a different node must note it (`Mem::watch.moved`):
    /// moved from a node not traversed yet to one traversed already, nothing else marks it.
    /// Every entry whose node changed on an insertion without a rebuild is checked to be
    /// noted, for keys in `nodes` and in the integer part.
    #[test]
    fn a_watched_table_notes_every_entry_an_insertion_moves() {
        for ints in [false, true] {
            let mut m = mem();
            let mut t = Table { seed: crate::heap::mix_seed(7, 99), gc_watch: true, ..Table::default() };
            let key = |i: u64| {
                if ints { Value::Int(((i * 2654435761) % 1000003 + 100000) as i64) } else { Value::Table(TableRef { idx: (i * 7 + 3) as u32, generation: 1 }) }
            };
            let val = |i: u64| Value::Table(TableRef { idx: 100_000 + i as u32, generation: 1 });
            let mut keys = Vec::new();
            let mut moves = 0;
            for i in 0..3000u64 {
                let before = positions(&t, &keys);
                m.watch.moved.clear();
                m.watch.restructured = false;
                assert!(t.set(key(i), val(i), &mut m).is_ok());
                keys.push(key(i));
                if m.watch.restructured {
                    continue;
                }
                let after = positions(&t, &keys);
                for (j, (b, a)) in before.iter().zip(&after).enumerate() {
                    if b != a {
                        moves += 1;
                        let v = val(j as u64);
                        assert!(m.watch.moved.chunks(2).any(|p| p.get(1).is_some_and(|x| x.raw_eq(v))), "ints {ints}: key {j} moved {b:?} -> {a:?} on insert {i}, not noted");
                    }
                }
            }
            assert!(moves > 100, "ints {ints}: only {moves} entries moved");
        }
    }

    /// S6T-2: `LiveBits::next_set` finds what a slot-by-slot walk over the set bits finds,
    /// through sets and clears at sizes of one to four levels.
    #[test]
    fn live_bits_find_exactly_the_set_bits() {
        for len in [65, 100, 4096, 4097, 300_000] {
            let b = LiveBits::for_len(len).unwrap();
            let mut want = vec![false; len];
            let mut x = 0x9e37_79b9_7f4a_7c15u64 ^ len as u64;
            for round in 0..6 {
                for _ in 0..(len / 3).max(50) {
                    x ^= x << 13;
                    x ^= x >> 7;
                    x ^= x << 17;
                    let i = (x as usize) % len;
                    // Sparse after the first rounds, so long gaps cross the upper levels.
                    if round < 2 || x.is_multiple_of(97) {
                        b.set(i);
                        want[i] = true;
                    } else {
                        b.clear(i);
                        want[i] = false;
                    }
                }
                let mut from = 0;
                loop {
                    let w = (from..len).find(|&i| want[i]);
                    assert_eq!(b.next_set(from), w, "len {len} round {round} from {from}");
                    match w {
                        Some(i) => from = i + 1 + (x as usize % 3),
                        None => break,
                    }
                }
            }
        }
    }

    /// SEC7-T2 (ee9a299): a value written over a value goes into the new
    /// integer part a kept resize made, its lookup not fuel; one walking a chain of keys
    /// aimed at that part (unbounded until it is done: no reseed there) throws it away instead.
    #[test]
    fn a_kept_resize_writes_only_through_short_chains() {
        for aimed in [3i64, 20] {
            let mut m = mem();
            let mut t = Table::default();
            assert!(t.set(Value::Int(1), Value::Int(0), &mut m).is_ok());
            // Keys with the home of 1 in 64 slots (1 + 63 j): 1 goes second, each after it
            // in front of it.
            let mut q = IntPart::with_len(64, &mut m);
            for k in crate::nostd::iter::once(64).chain([1]).chain((2..aimed).map(|j| 1 + 63 * j)) {
                assert!(q.place(k, Value::Int(0), &mut None));
            }
            let p = Partial { resize: Some(ResizeBuild { ints: Some(q), ..ResizeBuild::default() }), ..Partial::default() };
            t.partial = keep_partial(&mut m, Box::new(p));
            t.absent.set(RESUMABLE);
            let before = take_probes();
            assert!(t.set(Value::Int(1), Value::Int(5), &mut m).is_ok());
            assert_eq!(take_probes(), 0, "aimed {aimed}: no fuel");
            put_probes(before);
            let kept = mem_partial(&m, t.partial).and_then(|p| p.resize.as_ref()?.ints.as_ref().map(|q| q.find(1).map(|j| q.nodes[j].val)));
            if aimed == 3 {
                assert_ne!(t.absent.get() & RESUMABLE, 0, "kept");
                assert!(kept.flatten().is_some_and(|v| v.raw_eq(Value::Int(5))), "written");
            } else {
                assert_eq!(t.absent.get() & RESUMABLE, 0, "aimed {aimed}: thrown away");
            }
        }
    }

    fn mem_partial(m: &Mem, h: u32) -> Option<&Partial> {
        m.partials.get((h as usize).wrapping_sub(1))?.as_deref()
    }

    /// Keys aimed at both their homes (one pair at every size up to 64 slots): chains stop
    /// at `CHAIN_FREE + 1` (the insert past it reseeds, charged as a resize) and every key
    /// is still found.
    #[test]
    fn aimed_keys_reseed() {
        let mut m = mem();
        let mut t = Table { seed: 7, ..Table::default() };
        let seed = t.seed;
        let mut aimed = Vec::new();
        let mut x = 0.5f64;
        while aimed.len() < 40 {
            x += 1.0;
            let h = hash_key(Value::Num(x), seed);
            if h & 63 == 5 && (h >> 32) & 62 == 0 {
                aimed.push(x);
            }
        }
        for (i, &f) in aimed.iter().enumerate() {
            assert!(t.set(Value::Num(f), Value::Int(i as i64), &mut m).is_ok());
            // (Past it only until the next insert, which reseeds first.)
            assert!(check_chains(&t) <= CHAIN_FREE + 1 + usize::from(t.reseed_due));
        }
        assert_ne!(t.seed, seed);
        for (i, &f) in aimed.iter().enumerate() {
            assert!(t.get(Value::Num(f)).raw_eq(Value::Int(i as i64)));
        }
        assert!(t.resized > 0);
    }
}
