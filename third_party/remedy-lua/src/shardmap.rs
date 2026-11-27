//! A hash map that grows in pieces, for the compiler's maps that a hostile source makes
//! huge (the scope map, a function's constants): see `ShardMap`.

#[allow(unused_imports)]
use crate::nostd::prelude::*;
/// A hash map that grows in pieces, for the compiler's maps that a hostile source makes
/// huge (the scope map, a function's constants). Rehashing a `HashMap` of a million keys
/// took 30 to 40 ms between two looks at the host's interrupt. Up to `SPLIT` keys this is
/// one map; past it, the keys are spread over `SHARDS` maps by a hash of their own, so a
/// rehash moves a few thousand keys at a time. The shards fill evenly, so left alone they
/// would all rehash within a few hundred inserts of each other (as long, in all, as the
/// one big rehash): each insert also looks at one shard in turn and grows it early, shard
/// `j` once it is `1/2 + j/(2 * SHARDS)` full, which spreads the rehashes evenly.
/// (Small until it splits: a `HashMap` and a pointer. A compile holds one per function
/// being compiled, and thousands of nested functions' added up.)
pub(crate) struct ShardMap<K, V> {
    one: crate::nostd::collections::HashMap<K, V>,
    split: Option<Box<Shards<K, V>>>,
}

struct Shards<K, V> {
    maps: Vec<crate::nostd::collections::HashMap<K, V>>,
    pick: crate::nostd::collections::hash_map::RandomState,
    /// The shard the next insert looks at.
    turn: usize,
}

const SPLIT: usize = 1 << 14;

/// The slots of a `HashMap` that `n` inserts grew (hashbrown: a power of two of at least 4,
/// at most 7/8 full past 8 slots).
pub(crate) fn nominal_slots(n: usize) -> usize {
    match n {
        0 => 0,
        1..=3 => 4,
        4..=7 => 8,
        _ => n.saturating_mul(8).div_ceil(7).next_power_of_two(),
    }
}

/// The capacity of a `HashMap` that `n` inserts grew (see `nominal_slots`).
pub(crate) fn nominal_capacity(n: usize) -> usize {
    match nominal_slots(n) {
        s @ (0 | 4 | 8) => s.saturating_sub(1),
        s => s / 8 * 7,
    }
}
const SHARDS: usize = 256;

impl<K, V> Default for ShardMap<K, V> {
    fn default() -> Self {
        ShardMap { one: crate::nostd::collections::HashMap::<_, _, crate::nostd::collections::hash_map::RandomState>::default(), split: None }
    }
}

impl<K: crate::nostd::hash::Hash + Eq, V> ShardMap<K, V> {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    #[inline]
    fn shard_of<Q: crate::nostd::hash::Hash + ?Sized>(pick: &crate::nostd::collections::hash_map::RandomState, k: &Q) -> usize {
        use crate::nostd::hash::BuildHasher;
        (pick.hash_one(k) as usize) % SHARDS
    }

    #[inline]
    fn map_of<Q: crate::nostd::hash::Hash + ?Sized>(&self, k: &Q) -> &crate::nostd::collections::HashMap<K, V> {
        match &self.split {
            None => &self.one,
            Some(s) => s.maps.get(Self::shard_of(&s.pick, k)).unwrap_or(&self.one),
        }
    }

    #[inline]
    fn map_of_mut<Q: crate::nostd::hash::Hash + ?Sized>(&mut self, k: &Q) -> &mut crate::nostd::collections::HashMap<K, V> {
        match &mut self.split {
            None => &mut self.one,
            Some(s) => {
                let i = Self::shard_of(&s.pick, k);
                match s.maps.get_mut(i) {
                    Some(m) => m,
                    None => &mut self.one,
                }
            }
        }
    }

    /// Spreads the keys over the shards once there are `SPLIT` of them.
    fn split(&mut self) {
        let pick = crate::nostd::collections::hash_map::RandomState::new();
        let mut maps: Vec<_> = (0..SHARDS).map(|_| crate::nostd::collections::HashMap::<_, _, crate::nostd::collections::hash_map::RandomState>::default()).collect();
        for (k, v) in crate::nostd::mem::take(&mut self.one) {
            if let Some(m) = maps.get_mut(Self::shard_of(&pick, &k)) {
                m.insert(k, v);
            }
        }
        self.one = crate::nostd::collections::HashMap::<_, _, crate::nostd::collections::hash_map::RandomState>::default();
        self.split = Some(Box::new(Shards { maps, pick, turn: 0 }));
    }

    pub(crate) fn len(&self) -> usize {
        self.one.len() + self.split.as_ref().map_or(0, |s| s.maps.iter().map(crate::nostd::collections::HashMap::len).sum())
    }

    pub(crate) fn get<Q>(&self, k: &Q) -> Option<&V>
    where
        K: crate::nostd::borrow::Borrow<Q>,
        Q: crate::nostd::hash::Hash + Eq + ?Sized,
    {
        self.map_of(k).get(k)
    }

    pub(crate) fn get_mut<Q>(&mut self, k: &Q) -> Option<&mut V>
    where
        K: crate::nostd::borrow::Borrow<Q>,
        Q: crate::nostd::hash::Hash + Eq + ?Sized,
    {
        self.map_of_mut(k).get_mut(k)
    }

    pub(crate) fn contains_key<Q>(&self, k: &Q) -> bool
    where
        K: crate::nostd::borrow::Borrow<Q>,
        Q: crate::nostd::hash::Hash + Eq + ?Sized,
    {
        self.map_of(k).contains_key(k)
    }

    pub(crate) fn remove<Q>(&mut self, k: &Q) -> Option<V>
    where
        K: crate::nostd::borrow::Borrow<Q>,
        Q: crate::nostd::hash::Hash + Eq + ?Sized,
    {
        self.map_of_mut(k).remove(k)
    }

    /// Before an insert: splits the map at `SPLIT` keys, then grows one shard early.
    fn before_insert(&mut self) {
        let Some(s) = &mut self.split else {
            if self.one.len() >= SPLIT {
                self.split();
            }
            return;
        };
        let j = s.turn;
        s.turn = (j + 1) % SHARDS;
        if let Some(m) = s.maps.get_mut(j) {
            let cap = m.capacity();
            if cap > 0 && m.len() >= cap / 2 + cap / 2 * j / SHARDS {
                m.reserve(cap);
            }
        }
    }

    pub(crate) fn insert(&mut self, k: K, v: V) -> Option<V> {
        self.before_insert();
        self.map_of_mut(&k).insert(k, v)
    }

    /// The value at `k`, inserted as `v` if missing.
    pub(crate) fn or_insert(&mut self, k: K, v: V) -> &mut V {
        self.before_insert();
        self.map_of_mut(&k).entry(k).or_insert(v)
    }

    /// Room for keys in all, for the compiler's memory account: what an insert-only map of
    /// this many keys has (`nominal_capacity`), not the maps' own `capacity()`, which
    /// follows the random hasher (where removals left tombstones, how the keys spread over
    /// the shards) where the memory charged must not.
    pub(crate) fn capacity(&self) -> usize {
        nominal_capacity(self.len())
    }

    /// Its entries, to free a piece at a time (`diag::Piecewise`), with what it holds
    /// measured: the shards' bucket arrays as std allocates them and the keys' own bytes.
    pub(crate) fn into_pieces(self) -> ShardPieces<K, V>
    where
        K: crate::diag::KeyHeap,
    {
        let mut maps = self.split.map(|s| s.maps).unwrap_or_default();
        maps.push(self.one);
        // Sized from the key count, as an insert-only map of them would be: what the maps
        // hold follows the random hasher (shards, tombstones), and the account must not.
        let len = maps.iter().map(crate::nostd::collections::HashMap::len).sum();
        let tables = crate::diag::table_bytes::<K, V>(nominal_capacity(len));
        let keys = maps.iter().flat_map(|m| m.keys()).map(crate::diag::KeyHeap::heap).sum();
        ShardPieces { maps, cur: None, tables, keys }
    }
}

/// A `ShardMap`'s entries being freed a piece at a time.
pub(crate) struct ShardPieces<K, V> {
    maps: Vec<crate::nostd::collections::HashMap<K, V>>,
    cur: Option<crate::nostd::collections::hash_map::IntoIter<K, V>>,
    /// The bucket arrays, as `nominal_capacity` sizes them (given back when all is freed).
    tables: usize,
    /// The keys' own bytes not freed yet.
    keys: usize,
}

impl<K: crate::diag::KeyHeap, V> ShardPieces<K, V> {
    /// The bytes it still holds (exact, as measured by `into_pieces` and given back).
    pub(crate) fn held(&self) -> usize {
        self.tables + self.keys + self.maps.capacity() * crate::nostd::mem::size_of::<crate::nostd::collections::HashMap<K, V>>()
    }

    /// Frees up to `n` entries: whether all are freed.
    pub(crate) fn free_some(&mut self, mut n: usize) -> bool {
        while n > 0 {
            if let Some(it) = &mut self.cur {
                match it.next() {
                    Some((k, _)) => {
                        self.keys = self.keys.saturating_sub(k.heap());
                        n -= 1;
                        continue;
                    }
                    None => self.cur = None,
                }
            }
            match self.maps.pop() {
                Some(m) => self.cur = Some(m.into_iter()),
                None => {
                    self.tables = 0;
                    self.keys = 0;
                    return true;
                }
            }
        }
        self.cur.is_none() && self.maps.is_empty()
    }
}


#[cfg(test)]
mod tests {
    #[test]
    fn nominal_capacity_is_an_insert_only_maps() {
        let mut m = crate::nostd::collections::HashMap::<_, _, crate::nostd::collections::hash_map::RandomState>::default();
        for i in 0..200_000usize {
            assert_eq!(super::nominal_capacity(m.len()), m.capacity(), "{} keys", m.len());
            m.insert(i, i);
        }
    }

    #[test]
    fn shard_map_behaves_as_a_map() {
        let mut m: super::ShardMap<String, usize> = super::ShardMap::new();
        let mut h = crate::nostd::collections::HashMap::<_, _, crate::nostd::collections::hash_map::RandomState>::default();
        for i in 0..100_000usize {
            let k = format!("k{}", i % 70_000);
            *m.or_insert(k.clone(), 0) += i;
            *h.entry(k).or_insert(0) += i;
            if i % 7 == 0 {
                let r = format!("k{}", i / 3);
                assert_eq!(m.remove(r.as_str()), h.remove(&r));
            }
        }
        assert_eq!(m.len(), h.len());
        for (k, v) in &h {
            assert_eq!(m.get(k.as_str()), Some(v));
            assert!(m.contains_key(k.as_str()));
        }
        assert!(!m.contains_key("nope"));
        assert!(m.capacity() >= h.len());
        let mut p = m.into_pieces();
        let mut rounds = 0;
        while !p.free_some(1000) {
            rounds += 1;
        }
        assert!(rounds >= h.len() / 1000 - 1);
    }
}
