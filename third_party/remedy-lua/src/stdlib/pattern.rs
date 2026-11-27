//! Lua patterns: the matcher of Lua 5.4's string library, ported to Rust, counting its
//! steps. A match that takes too many steps fails with "pattern too complex" (which a
//! script may catch), and the steps it took are charged as fuel.

#[allow(unused_imports)]
use crate::nostd::prelude::*;
/// Most captures in a pattern.
pub(crate) const MAX_CAPTURES: usize = 32;
/// Captures a match's results keep inline (`Captures`).
const CAPS_INLINE: usize = 4;
/// Deepest nesting of the matcher's alternatives (C Lua's `MAXCCALLS`).
const MAX_DEPTH: usize = 200;
const CAP_UNFINISHED: isize = -1;
const CAP_POSITION: isize = -2;

/// Matcher steps (and plain-search bytes / 16) between looks at the host's interrupt and
/// deadline: a match is charged only when it ends, so it watches for them itself.
pub(crate) const WATCH_EVERY: u64 = 1 << 16;

/// What stops a long match before its fuel is charged: the host's interrupt and the
/// call's deadline (the checks `Lua::refuel` makes between slices of fuel).
#[derive(Clone, Default)]
pub(crate) struct Watch {
    flag: Option<crate::nostd::sync::Arc<crate::nostd::sync::atomic::AtomicBool>>,
    deadline: Option<(crate::Clock, crate::nostd::time::Duration)>,
}

impl Watch {
    /// No interrupt, no deadline (for a search too short to look at them).
    pub const NONE: Watch = Watch { flag: None, deadline: None };

    pub fn of(lua: &crate::Lua) -> Watch {
        let clock = lua.time_limit.as_ref().map(|(c, _)| crate::nostd::sync::Arc::clone(c));
        Watch { flag: Some(crate::nostd::sync::Arc::clone(&lua.interrupt)), deadline: clock.zip(lua.deadline) }
    }

    /// The same watch, the interrupt only.
    pub fn without_deadline(self) -> Watch {
        Watch { deadline: None, ..self }
    }

    /// The limit that stops the call now, if any.
    pub fn stop(&self) -> Option<crate::LimitKind> {
        if self.flag.as_ref().is_some_and(|f| f.load(crate::nostd::sync::atomic::Ordering::Relaxed)) {
            return Some(crate::LimitKind::Interrupted);
        }
        match &self.deadline {
            Some((clock, at)) if clock() >= *at => Some(crate::LimitKind::Time),
            _ => None,
        }
    }
}

/// A pattern error (raised as a script error).
#[derive(Debug)]
pub(crate) struct PatError(pub String);

/// A matcher: a subject and a pattern, and the state a match works in (borrowed: `gsub`
/// binds the same state to the subject again after each replacement function, which
/// moved the whole state, some 700 bytes, twice a match).
pub(crate) struct Matcher<'a> {
    pub src: &'a [u8],
    pub pat: &'a [u8],
    st: &'a mut MatchState,
}

impl crate::nostd::ops::Deref for Matcher<'_> {
    type Target = MatchState;
    #[inline]
    fn deref(&self) -> &MatchState {
        self.st
    }
}

impl crate::nostd::ops::DerefMut for Matcher<'_> {
    #[inline]
    fn deref_mut(&mut self) -> &mut MatchState {
        self.st
    }
}

/// What a match works in: captures, alternatives and counters (see `Matcher`).
pub(crate) struct MatchState {
    pub level: usize,
    pub caps: [(usize, isize); MAX_CAPTURES],
    /// What to do when the nested matches fail, innermost last (see `do_match`).
    back: BackStack,
    /// The end of the first item when it is one character class with no `*`, `?` or `-`
    /// after it (see `first_fails`); worked out on first use.
    first: Option<Option<usize>>,
    pub steps: u64,
    max_steps: u64,
    watch: Watch,
    /// Steps since the last look at `watch`.
    tick: u64,
    /// The host's interrupt or deadline stopped the match (its error then has no text).
    pub stop: Option<crate::LimitKind>,
    /// Each set (`[...]`) of the pattern tried so far, by the offset of its `[`, as the 256
    /// bytes it matches (`class_set`): LR-11, a set was scanned whole for every character
    /// tried against it, charged a step per 16 bytes, and a set of 65,536 bytes under `gsub`
    /// ran at 11x plain code per unit of fuel. Made once per call (a call keeps one
    /// pattern: `MatchState::new`, `reuse`) in one pass over the set; the steps charged
    /// are as before, so fuel does not change.
    sets: Vec<(usize, [u64; 4])>,
    /// The end of each set (`[...]`) by the offset of its `[` (`item_end`), for the same
    /// reason: finding it walked the set at every attempt.
    ends: Vec<(usize, usize)>,
}

/// The matcher's alternatives: the first few inline (most patterns never need more, and a
/// matcher is made per call), the rest on the heap, `MAX_DEPTH` at most.
#[derive(Clone)]
struct BackStack {
    inline: [Back; 8],
    n: usize,
    more: Vec<Back>,
}

impl BackStack {
    fn new() -> Self {
        BackStack { inline: [Back::CapStart; 8], n: 0, more: Vec::new() }
    }

    fn len(&self) -> usize {
        self.n
    }

    fn clear(&mut self) {
        self.n = 0;
        self.more.clear();
    }

    #[inline]
    fn push(&mut self, b: Back) {
        match self.inline.get_mut(self.n) {
            Some(slot) => *slot = b,
            None => self.more.push(b),
        }
        self.n += 1;
    }

    #[inline]
    fn pop(&mut self) -> Option<Back> {
        if self.n == 0 {
            return None;
        }
        self.n -= 1;
        match self.inline.get(self.n) {
            Some(b) => Some(*b),
            None => self.more.pop(),
        }
    }
}

/// A pending alternative, or an undo, for when a nested match fails.
#[derive(Clone, Copy)]
enum Back {
    /// A capture opened: close it again.
    CapStart,
    /// Capture `l` closed: open it again.
    CapEnd(usize),
    /// `x?` matched x: try without it, from `s` at pattern `p`.
    Optional { s: usize, p: usize },
    /// `x*`/`x+` took `i` repetitions from `s`: try one fewer.
    Max { s: usize, p: usize, ep: usize, i: usize },
    /// `x-` took the repetitions up to `s`: try one more.
    Min { s: usize, p: usize, ep: usize },
}

/// A capture's value.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Cap {
    Str(usize, usize),
    Pos(usize),
}

type M = Result<Option<usize>, PatError>;

/// The classes (`%a`, `%d` ...) by bit: which ones each byte is in.
const CLASS_OF: [u16; 256] = class_table();
/// The bit of each class letter (either case; 0: not a class, the letter itself).
const CLASS_BIT: [u16; 256] = class_bits();
const CLASS_LETTERS: &[u8; 11] = b"acdglpsuwxz";

#[allow(clippy::indexing_slicing, reason = "bytes index 256-entry tables")]
const fn class_table() -> [u16; 256] {
    let mut t = [0u16; 256];
    let mut i = 0;
    while i < 256 {
        let c = i as u8;
        let is = [
            c.is_ascii_alphabetic(),
            c.is_ascii_control(),
            c.is_ascii_digit(),
            c.is_ascii_graphic(),
            c.is_ascii_lowercase(),
            c.is_ascii_punctuation(),
            matches!(c, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c),
            c.is_ascii_uppercase(),
            c.is_ascii_alphanumeric(),
            c.is_ascii_hexdigit(),
            c == 0,
        ];
        let mut k = 0;
        while k < is.len() {
            if is[k] {
                t[i] |= 1 << k;
            }
            k += 1;
        }
        i += 1;
    }
    t
}

#[allow(clippy::indexing_slicing, reason = "bytes index 256-entry tables")]
const fn class_bits() -> [u16; 256] {
    let mut t = [0u16; 256];
    let mut k = 0;
    while k < CLASS_LETTERS.len() {
        let l = CLASS_LETTERS[k];
        t[l as usize] = 1 << k;
        t[l.to_ascii_uppercase() as usize] = 1 << k;
        k += 1;
    }
    t
}

/// The bytes each `%x` item matches, by `x` (`class_matches` for every byte), for
/// `build_set`. Made at compile time.
const ITEM_SET: [[u64; 4]; 256] = item_sets();

#[allow(clippy::indexing_slicing, reason = "bytes index 256-entry tables")]
const fn item_sets() -> [[u64; 4]; 256] {
    let mut t = [[0u64; 4]; 256];
    let mut cl = 0;
    while cl < 256 {
        let bit = CLASS_BIT[cl];
        let mut c = 0;
        while c < 256 {
            let m = if bit == 0 {
                c == cl
            } else {
                (CLASS_OF[c] & bit != 0) != (cl as u8).is_ascii_uppercase()
            };
            if m {
                t[cl][c >> 6] |= 1 << (c & 63);
            }
            c += 1;
        }
        cl += 1;
    }
    t
}

/// Sets shorter than this (in pattern bytes) are walked, not made into `ITEM_SET`-style
/// sets: a walk that short costs about what a lookup does, and making a set per call
/// (`gmatch` makes one per match) would cost more.
const SET_WALK_MAX: usize = 32;

/// Whether byte `c` matches class letter `cl` (`%cl`; an upper-case letter negates; any
/// other character stands for itself): two table lookups.
#[inline]
fn class_matches(c: u8, cl: u8) -> bool {
    let bit = CLASS_BIT.get(usize::from(cl)).copied().unwrap_or(0);
    if bit == 0 {
        return cl == c;
    }
    let res = CLASS_OF.get(usize::from(c)).copied().unwrap_or(0) & bit != 0;
    res != cl.is_ascii_uppercase()
}

impl MatchState {
    pub fn new(max_steps: u64, watch: Watch) -> Self {
        let caps = [(0, 0); MAX_CAPTURES];
        MatchState { level: 0, caps, back: BackStack::new(), first: None, steps: 0, max_steps, watch, tick: 0, stop: None, sets: Vec::new(), ends: Vec::new() }
    }
}

impl<'a> Matcher<'a> {
    /// `st` over `src` and `pat` (its captures, counters and what it worked out about the
    /// pattern kept: a state serves one pattern from `MatchState::new` or `reuse` on).
    /// LR-11: `gsub` binds its state again at every fuel charge, and forgetting the first
    /// item here walked a set of 65,536 bytes again for every character tried.
    pub fn new(src: &'a [u8], pat: &'a [u8], st: &'a mut MatchState) -> Self {
        Matcher { src, pat, st }
    }

    /// The end of the first item when an attempt that fails on it is exactly one step (a
    /// single character class, no `*`, `?` or `-` after it, nothing special before).
    fn simple_first(&self) -> Option<usize> {
        match (self.p(0), self.p(1)) {
            (b'(' | b')', _) | (b'%', b'b' | b'f' | b'0'..=b'9') => return None,
            (b'$', _) if self.pat.len() == 1 => return None,
            _ if self.pat.is_empty() => return None,
            _ => {}
        }
        let ep = self.class_end(0).ok()?;
        if ep < self.pat.len() && matches!(self.p(ep), b'*' | b'?' | b'-') { None } else { Some(ep) }
    }

    /// Whether an attempt at `s` fails on the first item at once (see `attempt`).
    #[inline(always)]
    fn first_fails(&mut self, s: usize) -> bool {
        let first = match self.first {
            Some(f) => f,
            None => {
                let f = self.simple_first();
                *self.first.insert(f)
            }
        };
        let Some(ep) = first else { return false };
        self.bulk(self.class_cost(0, ep));
        !self.single(s, 0, ep)
    }

    /// One attempt at a match from `s`; `quick`: one that fails on its first item counts
    /// as one step (`first_fails`) instead of running `do_match`, which would do the same.
    // (Always inlined, and `first_fails` with it: with the in-place `find` and `match`
    // calling `search_in` too, LLVM kept one or the other out of line, a call per attempt.)
    #[inline(always)]
    pub fn attempt(&mut self, s: usize, quick: bool) -> M {
        self.level = 0;
        if quick && self.first_fails(s) { self.step().map(|()| None) } else { self.do_match(s, 0) }
    }

    fn p(&self, i: usize) -> u8 {
        self.pat.get(i).copied().unwrap_or(0)
    }

    fn s(&self, i: usize) -> u8 {
        self.src.get(i).copied().unwrap_or(0)
    }

    /// `class_end`, a set's kept once found (see `MatchState::ends`).
    fn item_end(&mut self, p: usize) -> Result<usize, PatError> {
        if self.p(p) != b'[' {
            return self.class_end(p);
        }
        if let Some(&(_, e)) = self.ends.iter().find(|(at, _)| *at == p) {
            return Ok(e);
        }
        let e = self.class_end(p)?;
        self.ends.push((p, e));
        Ok(e)
    }

    fn class_end(&self, p: usize) -> Result<usize, PatError> {
        let mut p = p;
        let c = self.p(p);
        p += 1;
        if c == b'%' {
            if p >= self.pat.len() {
                return Err(PatError("malformed pattern (ends with '%')".into()));
            }
            return Ok(p + 1);
        }
        if c == b'[' {
            if self.p(p) == b'^' {
                p += 1;
            }
            loop {
                if p >= self.pat.len() {
                    return Err(PatError("malformed pattern (missing ']')".into()));
                }
                let cc = self.p(p);
                p += 1;
                if cc == b'%' && p < self.pat.len() {
                    p += 1;
                }
                if self.p(p) == b']' {
                    break;
                }
            }
            return Ok(p + 1);
        }
        Ok(p)
    }

    /// Whether byte `c` matches the set at `p` (its `]` at `ec`), by walking it: what
    /// `build_set` computes for every byte at once (used for short sets, `SET_WALK_MAX`).
    fn match_bracket(&self, c: u8, p: usize, ec: usize) -> bool {
        let mut p = p;
        let mut sig = true;
        if self.p(p + 1) == b'^' {
            sig = false;
            p += 1;
        }
        loop {
            p += 1;
            if p >= ec {
                break;
            }
            if self.p(p) == b'%' {
                p += 1;
                if class_matches(c, self.p(p)) {
                    return sig;
                }
            } else if self.p(p + 1) == b'-' && p + 2 < ec {
                p += 2;
                if self.p(p - 2) <= c && c <= self.p(p) {
                    return sig;
                }
            } else if self.p(p) == c {
                return sig;
            }
        }
        !sig
    }

    fn single(&mut self, s: usize, p: usize, ep: usize) -> bool {
        if s >= self.src.len() {
            return false;
        }
        let c = self.s(s);
        match self.p(p) {
            b'.' => true,
            b'%' => class_matches(c, self.p(p + 1)),
            b'[' if ep - p < SET_WALK_MAX => self.match_bracket(c, p, ep - 1),
            b'[' => {
                let set = self.class_set(p, ep - 1);
                set.get(usize::from(c >> 6)).is_some_and(|w| w >> (c & 63) & 1 != 0)
            }
            pc => pc == c,
        }
    }

    /// The bytes the set at `p` (its `]` at `ec`) matches, made on first use (see
    /// `MatchState::sets`).
    fn class_set(&mut self, p: usize, ec: usize) -> [u64; 4] {
        if let Some(&(_, set)) = self.sets.iter().find(|(at, _)| *at == p) {
            return set;
        }
        let set = self.build_set(p, ec);
        self.sets.push((p, set));
        set
    }

    /// `match_bracket` for every byte at once: the same walk over the set, each item adding
    /// the bytes it matches.
    fn build_set(&self, p: usize, ec: usize) -> [u64; 4] {
        let mut bits = [0u64; 4];
        let add = |bits: &mut [u64; 4], c: u8| {
            if let Some(w) = bits.get_mut(usize::from(c >> 6)) {
                *w |= 1 << (c & 63);
            }
        };
        let mut p = p;
        let mut sig = true;
        if self.p(p + 1) == b'^' {
            sig = false;
            p += 1;
        }
        loop {
            p += 1;
            if p >= ec {
                break;
            }
            if self.p(p) == b'%' {
                p += 1;
                let class = ITEM_SET.get(usize::from(self.p(p))).copied().unwrap_or_default();
                for (w, cw) in bits.iter_mut().zip(class) {
                    *w |= cw;
                }
            } else if self.p(p + 1) == b'-' && p + 2 < ec {
                p += 2;
                let (lo, hi) = (self.p(p - 2), self.p(p));
                if lo <= hi {
                    for c in lo..=hi {
                        add(&mut bits, c);
                    }
                }
            } else {
                add(&mut bits, self.p(p));
            }
        }
        if !sig {
            for w in &mut bits {
                *w = !*w;
            }
        }
        bits
    }

    /// Work beyond a step (a long class or back reference): counted, checked at the next step.
    fn bulk(&mut self, n: u64) {
        self.steps += n;
        self.tick += n;
    }

    /// The extra steps of matching one character against the class at `p..ep`: a set
    /// (`[...]`) is scanned, a unit per 16 bytes of it.
    fn class_cost(&self, p: usize, ep: usize) -> u64 {
        if self.p(p) == b'[' { (ep - p) as u64 / 16 } else { 0 }
    }

    fn step(&mut self) -> Result<(), PatError> {
        self.steps += 1;
        if self.steps > self.max_steps {
            return Err(PatError("pattern too complex".into()));
        }
        self.tick += 1;
        if self.tick >= WATCH_EVERY {
            self.tick = 0;
            self.stop = self.watch.stop();
            if self.stop.is_some() {
                return Err(PatError(String::new()));
            }
        }
        Ok(())
    }

    /// Matches the pattern from `p` against the subject from `s`: the end of the match.
    ///
    /// C Lua's matcher recurses (on items with alternatives: `?`, `*`, `+`, `-` and
    /// captures). This one keeps what it would do on failure in `back`, a stack on the
    /// heap of at most `MAX_DEPTH` entries (C Lua's `MAXCCALLS` limit, with the same "pattern
    /// too complex" at the same depth), so matching never grows the native stack; the steps
    /// it counts are the same.
    pub fn do_match(&mut self, s: usize, p: usize) -> M {
        self.back.clear();
        let (mut s, mut p) = (s, p);
        loop {
            match self.run(s, p)? {
                Some(e) => return Ok(Some(e)),
                None => match self.backtrack() {
                    Some((s2, p2)) => (s, p) = (s2, p2),
                    None => return Ok(None),
                },
            }
        }
    }

    /// Starts a nested match at (`s`, `p`) that falls back on `b` when it fails.
    fn push(&mut self, b: Back) -> Result<(), PatError> {
        if self.back.len() + 1 >= MAX_DEPTH {
            return Err(PatError("pattern too complex".into()));
        }
        self.back.push(b);
        Ok(())
    }

    /// After a failure: undoes what failed nested matches set and returns where the next
    /// alternative starts (None: no alternative left).
    fn backtrack(&mut self) -> Option<(usize, usize)> {
        while let Some(b) = self.back.pop() {
            match b {
                Back::CapStart => self.level -= 1,
                Back::CapEnd(l) => {
                    if let Some(c) = self.caps.get_mut(l) {
                        c.1 = CAP_UNFINISHED;
                    }
                }
                Back::Optional { s, p } => return Some((s, p)),
                Back::Max { s, p, ep, i } => {
                    if i > 0 {
                        // (Same depth as the attempt that failed: no new limit check.)
                        self.back.push(Back::Max { s, p, ep, i: i - 1 });
                        return Some((s + i - 1, ep + 1));
                    }
                }
                Back::Min { s, p, ep } => {
                    self.bulk(self.class_cost(p, ep));
                    if self.single(s, p, ep) {
                        self.back.push(Back::Min { s: s + 1, p, ep });
                        return Some((s + 1, ep + 1));
                    }
                }
            }
        }
        None
    }

    /// Matches forward from (`s`, `p`) until the pattern ends (the match's end) or an item
    /// fails (None); items with alternatives push them on `back` and go on.
    fn run(&mut self, mut s: usize, mut p: usize) -> M {
        loop {
            self.step()?;
            if p >= self.pat.len() {
                return Ok(Some(s));
            }
            match self.p(p) {
                b'(' => {
                    let (np, what) = if self.p(p + 1) == b')' { (p + 2, CAP_POSITION) } else { (p + 1, CAP_UNFINISHED) };
                    if self.level >= MAX_CAPTURES {
                        return Err(PatError("too many captures".into()));
                    }
                    let level = self.level;
                    if let Some(c) = self.caps.get_mut(level) {
                        *c = (s, what);
                    }
                    self.level += 1;
                    self.push(Back::CapStart)?;
                    p = np;
                    continue;
                }
                b')' => {
                    let Some(l) =
                        (0..self.level).rev().find(|&i| self.caps.get(i).is_some_and(|c| c.1 == CAP_UNFINISHED))
                    else {
                        return Err(PatError("invalid pattern capture".into()));
                    };
                    let start = self.caps.get(l).map_or(0, |c| c.0);
                    if let Some(c) = self.caps.get_mut(l) {
                        c.1 = (s - start) as isize;
                    }
                    self.push(Back::CapEnd(l))?;
                    p += 1;
                    continue;
                }
                b'$' if p + 1 == self.pat.len() => {
                    return Ok(if s == self.src.len() { Some(s) } else { None });
                }
                b'%' if self.p(p + 1) == b'b' => match self.match_balance(s, p + 2)? {
                    Some(e) => {
                        s = e;
                        p += 4;
                        continue;
                    }
                    None => return Ok(None),
                },
                b'%' if self.p(p + 1) == b'f' => {
                    p += 2;
                    if self.p(p) != b'[' {
                        return Err(PatError("missing '[' after '%f' in pattern".into()));
                    }
                    let ep = self.item_end(p)?;
                    self.bulk(2 * self.class_cost(p, ep));
                    let prev = if s == 0 { 0 } else { self.s(s - 1) };
                    let cur = self.s(s);
                    let hit = if ep - p < SET_WALK_MAX {
                        !self.match_bracket(prev, p, ep - 1) && self.match_bracket(cur, p, ep - 1)
                    } else {
                        let set = self.class_set(p, ep - 1);
                        let has = |c: u8| set.get(usize::from(c >> 6)).is_some_and(|w| w >> (c & 63) & 1 != 0);
                        !has(prev) && has(cur)
                    };
                    if hit {
                        p = ep;
                        continue;
                    }
                    return Ok(None);
                }
                b'%' if self.p(p + 1).is_ascii_digit() => match self.match_capture(s, self.p(p + 1))? {
                    Some(e) => {
                        s = e;
                        p += 2;
                        continue;
                    }
                    None => return Ok(None),
                },
                _ => {}
            }
            let ep = self.item_end(p)?;
            let cost = self.class_cost(p, ep);
            self.bulk(cost);
            let epc = self.p(ep);
            if !self.single(s, p, ep) {
                if ep < self.pat.len() && matches!(epc, b'*' | b'?' | b'-') {
                    p = ep + 1;
                    continue;
                }
                return Ok(None);
            }
            match epc {
                b'?' if ep < self.pat.len() => {
                    self.push(Back::Optional { s, p: ep + 1 })?;
                    s += 1;
                    p = ep + 1;
                }
                b'+' | b'*' if ep < self.pat.len() => {
                    // The longest run first, then shorter ones on failure.
                    let s0 = if epc == b'+' { s + 1 } else { s };
                    let mut i = 0usize;
                    while self.single(s0 + i, p, ep) {
                        self.bulk(cost);
                        self.step()?;
                        i += 1;
                    }
                    self.push(Back::Max { s: s0, p, ep, i })?;
                    s = s0 + i;
                    p = ep + 1;
                }
                b'-' if ep < self.pat.len() => {
                    // The shortest run first, then longer ones on failure.
                    self.push(Back::Min { s, p, ep })?;
                    p = ep + 1;
                }
                _ => {
                    s += 1;
                    p = ep;
                }
            }
        }
    }

    fn match_balance(&mut self, s: usize, p: usize) -> M {
        if p + 1 >= self.pat.len() {
            return Err(PatError("malformed pattern (missing arguments to '%b')".into()));
        }
        if s >= self.src.len() || self.s(s) != self.p(p) {
            return Ok(None);
        }
        let (b, e) = (self.p(p), self.p(p + 1));
        let mut cont = 1;
        let mut s = s + 1;
        while s < self.src.len() {
            self.step()?;
            let c = self.s(s);
            if c == e {
                cont -= 1;
                if cont == 0 {
                    return Ok(Some(s + 1));
                }
            } else if c == b {
                cont += 1;
            }
            s += 1;
        }
        Ok(None)
    }

    fn match_capture(&mut self, s: usize, d: u8) -> M {
        let l = usize::from(d.wrapping_sub(b'1'));
        let Some(&(start, len)) = self.caps.get(l).filter(|_| l < self.level) else {
            return Err(PatError(format!("invalid capture index %{}", (d.wrapping_sub(b'0')) as i8)));
        };
        if len == CAP_UNFINISHED {
            return Err(PatError(format!("invalid capture index %{}", d.wrapping_sub(b'0'))));
        }
        // S5COR-2: a back-reference to a position capture never matches. C Lua's length is
        // `(size_t)CAP_POSITION`, longer than any rest of the subject; clamping it to 0
        // matched the empty string (`("aa"):find("()%1")` gave 1 instead of nil).
        if len == CAP_POSITION {
            return Ok(None);
        }
        let len = len.max(0) as usize;
        let cap = self.src.get(start..start + len).unwrap_or_default();
        let here = self.src.get(s..).unwrap_or_default();
        if here.len() >= len && here.get(..len) == Some(cap) {
            self.bulk(len as u64 / 16);
            Ok(Some(s + len))
        } else {
            Ok(None)
        }
    }
}

impl MatchState {
    /// Capture `i` of a match from `s` to `e`.
    pub fn capture(&self, i: usize, s: usize, e: usize) -> Result<Cap, PatError> {
        if i >= self.level {
            if i == 0 {
                return Ok(Cap::Str(s, e));
            }
            return Err(PatError(format!("invalid capture index %{}", i + 1)));
        }
        let (start, len) = self.caps.get(i).copied().unwrap_or((0, 0));
        if len == CAP_UNFINISHED {
            return Err(PatError("unfinished capture".into()));
        }
        if len == CAP_POSITION {
            return Ok(Cap::Pos(start + 1));
        }
        Ok(Cap::Str(start, start + len as usize))
    }

    /// All captures of a match (the whole match when the pattern has none, if `whole`),
    /// checked now (an unfinished one is an error) and read later.
    pub fn captures(&self, s: usize, e: usize, whole: bool) -> Result<Captures, PatError> {
        let n = if self.level == 0 && whole { 1 } else { self.level };
        for i in 0..n {
            self.capture(i, s, e)?;
        }
        let level = self.level.min(MAX_CAPTURES);
        let mut caps = [(0, 0); CAPS_INLINE];
        for (d, c) in caps.iter_mut().zip(self.caps.iter().take(level)) {
            *d = *c;
        }
        let more = self.caps.get(CAPS_INLINE..level).map(<[_]>::to_vec).unwrap_or_default();
        Ok(Captures { caps, more, level: self.level, n, s, e })
    }

    /// Ready for another call (`Lua::match_state` keeps one state for them all): no
    /// captures, no alternatives, nothing worked out, counters at zero. (The capture
    /// slots keep old values: only the first `level` are ever read.)
    pub fn reuse(&mut self, max_steps: u64, watch: Watch) {
        self.level = 0;
        self.back.clear();
        self.first = None;
        self.steps = 0;
        self.max_steps = max_steps;
        self.watch = watch;
        self.tick = 0;
        self.stop = None;
        self.sets.clear();
        self.ends.clear();
    }

    /// `reuse` with the watch it has (for a match too short to look at it).
    pub fn reuse_keeping_watch(&mut self, max_steps: u64) {
        let watch = crate::nostd::mem::take(&mut self.watch);
        self.reuse(max_steps, watch);
    }
}

/// A match's captures, kept without allocating (a copy of the matcher's).
pub(crate) struct Captures {
    /// The first `CAPS_INLINE` captures; the rest (rare) in `more`. (The whole array of
    /// 32 was copied three times a `find`.)
    caps: [(usize, isize); CAPS_INLINE],
    more: Vec<(usize, isize)>,
    level: usize,
    n: usize,
    s: usize,
    e: usize,
}

impl Captures {
    pub fn iter(&self) -> impl Iterator<Item = Cap> + '_ {
        (0..self.n).map(|i| {
            if i >= self.level {
                return Cap::Str(self.s, self.e);
            }
            let at = if i < CAPS_INLINE { self.caps.get(i) } else { self.more.get(i - CAPS_INLINE) };
            let (start, len) = at.copied().unwrap_or((0, 0));
            if len == CAP_POSITION { Cap::Pos(start + 1) } else { Cap::Str(start, start + len.max(0) as usize) }
        })
    }
}

/// Whether a pattern has no special characters (so `find` can search plainly).
pub(crate) fn no_specials(p: &[u8]) -> bool {
    !p.iter().any(|c| b"^$*+?.([%-".contains(c))
}

/// Plain substring search from `from` (C Lua's `lmemfind`, quadratic at worst): the
/// position found and the bytes it looked at. It gives up (None) once it has looked at
/// more than `budget` bytes, so one call never runs far past the fuel left, or once
/// `watch` says to stop (the third value).
pub(crate) fn find_plain(
    hay: &[u8],
    needle: &[u8],
    from: usize,
    budget: u64,
    watch: &Watch,
) -> (Option<usize>, u64, Option<crate::LimitKind>) {
    if needle.is_empty() {
        return ((from <= hay.len()).then_some(from), 1, None);
    }
    let (Some(h), Some(&first)) = (hay.get(from..), needle.first()) else { return (None, 1, None) };
    let mut work = 0u64;
    let mut next_look = WATCH_EVERY * 16;
    let mut i = 0;
    while i + needle.len() <= h.len() && work <= budget {
        if work >= next_look {
            next_look = work + WATCH_EVERY * 16;
            if let Some(k) = watch.stop() {
                return (None, work, Some(k));
            }
        }
        // A window at a time: no occurrence of the first byte in 16 MiB was one scan with
        // no look at the watch (8 ms past a time limit).
        let window = h.len().min(i.saturating_add(super::COPY_PIECE));
        let Some(off) = h.get(i..window).and_then(|rest| rest.iter().position(|&c| c == first)) else {
            if window < h.len() {
                work += (window - i) as u64;
                i = window;
                next_look = work;
                continue;
            }
            return (None, work + (h.len() - i) as u64, None);
        };
        i += off;
        work += off as u64;
        let Some(cand) = h.get(i..i + needle.len()) else { break };
        // A short prefix first: a candidate that differs early costs little; one that
        // agrees on it is charged the whole compare.
        let k = needle.len().min(16);
        if cand.get(..k) == needle.get(..k) {
            work += needle.len() as u64;
            if cand == needle {
                return (Some(from + i), work, None);
            }
        } else {
            work += k as u64;
        }
        i += 1;
    }
    (None, work, None)
}

#[cfg(test)]
mod tests {
    use super::{MatchState, Matcher, Watch};

    /// LR-11: the sets made once per call match exactly the bytes the walk over the set
    /// matches, for every byte, over sets of every kind of item (and odd ones: ranges
    /// backwards, `-` first and last, `%` before a non-letter, `]` first, negation).
    #[test]
    fn sets_match_the_walk_for_every_byte() {
        let sets: &[&[u8]] = &[
            b"[abc]", b"[^abc]", b"[a-z]", b"[z-a]", b"[-a]", b"[a-]", b"[%a]", b"[%A%d_]", b"[^%s%p]",
            b"[%]]", b"[]]", b"[^]]", b"[%%-z]", b"[a-c-e]", b"[\x00-\x1f]", b"[%w%x%z%c%g%l%u]", b"[^%W]",
            b"[%.%-%[]", b"[\x80-\xff]", b"[^\0]", b"[%1]", b"[a-%]",
        ];
        let mut all: Vec<Vec<u8>> = sets.iter().map(|s| s.to_vec()).collect();
        // Raw bytes, including the high half and zero.
        all.push(vec![b'[', 0, 200, 255, b'-', 1, b']']);
        all.push(vec![b'[', b'^', 128, b'-', 255, b']']);
        for pat in &all {
            let mut st = MatchState::new(1 << 20, Watch::default());
            let mut m = Matcher::new(b"", pat, &mut st);
            let ec = pat.len() - 1;
            let set = m.build_set(0, ec);
            for c in 0..=255u8 {
                let walk = m.match_bracket(c, 0, ec);
                let bit = set[usize::from(c >> 6)] >> (c & 63) & 1 != 0;
                assert_eq!(bit, walk, "{:?} byte {c}", String::from_utf8_lossy(pat));
            }
            // And cached: the same set again.
            assert_eq!(m.class_set(0, ec), set);
            assert_eq!(m.class_set(0, ec), set);
        }
    }
}

