//! The register machine's instructions and function prototypes.

#[allow(unused_imports)]
use crate::nostd::prelude::*;
use crate::nostd::sync::Arc;

use crate::value::Value;

/// A register or a constant: constants have the top bit set.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Rk(pub u16);

impl Rk {
    pub const CONST: u16 = 0x8000;
    pub fn reg(r: Reg) -> Rk {
        Rk(r)
    }
    pub fn konst(k: u16) -> Rk {
        Rk(k | Self::CONST)
    }
    #[inline]
    pub fn is_const(self) -> bool {
        self.0 & Self::CONST != 0
    }
    #[inline]
    pub fn index(self) -> usize {
        usize::from(self.0 & !Self::CONST)
    }
}

/// Arithmetic and bitwise operators, in Lua's metamethod order.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum ArithOp {
    Add,
    Sub,
    Mul,
    Mod,
    Pow,
    Div,
    IDiv,
    BAnd,
    BOr,
    BXor,
    Shl,
    Shr,
}

/// Count fields that mean "up to the top" (variable).
pub(crate) const MULTI: u16 = 0xffff;
/// `SetList`'s count meaning "up to the top".
pub(crate) const SETLIST_MULTI: u8 = 0xff;
/// A register number. Functions whose registers all fit in 8 bits run in the narrow
/// interpreter loop; the others in the wide one (see `Proto::wide`).
pub(crate) type Reg = u16;
/// Registers a function may use: `Rk` keeps its top bit for constants.
pub(crate) const MAX_REGS: usize = 0x7fff;

const _: () = assert!(crate::nostd::mem::size_of::<Instr>() == 8);

/// `Instr::NewTable`'s flag in `hash`: the last positional item is a call or `...`, so
/// Lua's array size is one less than `arr` (the room stays).
pub(crate) const NEWTABLE_MULTRET: u16 = 0x8000;
/// The most fields `Instr::NewTable` records.
pub(crate) const NEWTABLE_HASH_MAX: u16 = 0x7fff;

/// `Instr::NewTable`'s (room, Lua's array size, fields).
#[inline]
pub(crate) fn newtable_sizes(arr: u16, hash: u16) -> (usize, usize, usize) {
    let multret = usize::from(hash & NEWTABLE_MULTRET != 0);
    (usize::from(arr), usize::from(arr).saturating_sub(multret), usize::from(hash & NEWTABLE_HASH_MAX))
}

/// One instruction (8 bytes). Registers are relative to the frame's base. Comparisons and
/// tests skip the next instruction (a jump) when the condition does not hold as `k` says.
pub(crate) type Instr = Op<Reg, Rk>;

/// The instruction set over a register type `R` and a register-or-constant type `K`: the
/// machine's `Instr` (16-bit), and the compiler's own form with registers numbered from the
/// frame's base without bound, which it maps to `Instr` (see `Window`).
#[derive(Clone, Copy, Debug)]
pub(crate) enum Op<R, K> {
    Move { a: R, b: R },
    LoadK { a: R, k: u32 },
    LoadInt { a: R, i: i32 },
    LoadBool { a: R, v: bool },
    /// R[a] = false, and skip the next instruction.
    LoadFalseSkip { a: R },
    /// R[a..=a+n] = nil.
    LoadNil { a: R, n: u16 },
    GetUpval { a: R, u: u16 },
    /// R[a] = the running closure, which its upvalue `u` holds (`settle_upvals`); `u`
    /// names it in messages.
    GetSelf { a: R, u: u16 },
    SetUpval { a: R, u: u16 },
    /// R[a] = Up[u][K[k]] (a string key).
    GetTabUp { a: R, u: u16, k: u16 },
    /// Up[u][K[k]] = RK(v).
    SetTabUp { u: u16, k: u16, v: K },
    /// R[a] = R[b][R[c]].
    GetTable { a: R, b: R, c: R },
    /// R[a] = R[b][K[k]].
    GetTableK { a: R, b: R, k: u16 },
    /// R[a][R[b]] = RK(c).
    SetTable { a: R, b: R, c: K },
    /// R[a][K[k]] = RK(c).
    SetTableK { a: R, k: u16, c: K },
    NewTable { a: R, arr: u16, hash: u16 },
    /// R[a+1] = R[b]; R[a] = R[b][K[k]].
    SelfK { a: R, b: R, k: u16 },
    Arith { op: ArithOp, a: R, b: K, c: K },
    // The common arithmetic on a register and a register or a constant, one opcode each
    // (the compiler turns `Arith` into these where it can).
    Add { a: R, b: R, c: R },
    AddK { a: R, b: R, k: u16 },
    Sub { a: R, b: R, c: R },
    SubK { a: R, b: R, k: u16 },
    Mul { a: R, b: R, c: R },
    MulK { a: R, b: R, k: u16 },
    Div { a: R, b: R, c: R },
    DivK { a: R, b: R, k: u16 },
    Mod { a: R, b: R, c: R },
    ModK { a: R, b: R, k: u16 },
    IDiv { a: R, b: R, c: R },
    IDivK { a: R, b: R, k: u16 },
    /// R[a] = R[b] + i; `sub`: written `R[b] - -i` (metamethods and messages see `-`).
    AddI { a: R, b: R, i: i16, sub: bool },
    Pow { a: R, b: R, c: R },
    PowK { a: R, b: R, k: u16 },
    BAnd { a: R, b: R, c: R },
    BAndK { a: R, b: R, k: u16 },
    BOr { a: R, b: R, c: R },
    BOrK { a: R, b: R, k: u16 },
    BXor { a: R, b: R, c: R },
    BXorK { a: R, b: R, k: u16 },
    Shl { a: R, b: R, c: R },
    ShlK { a: R, b: R, k: u16 },
    Shr { a: R, b: R, c: R },
    ShrK { a: R, b: R, k: u16 },
    /// if (R[a] == K[k]) != eq then skip (a constant other than a small integer).
    EqK { a: R, k: u16, eq: bool },
    /// if (R[a] == i) != eq then skip.
    EqI { a: R, i: i16, eq: bool },
    /// if (R[a] < i) != k then skip.
    LtI { a: R, i: i16, k: bool },
    /// if (R[a] <= i) != k then skip.
    LeI { a: R, i: i16, k: bool },
    /// if (R[a] > i) != k then skip (`i < R[a]`: metamethods and messages see that order).
    GtI { a: R, i: i16, k: bool },
    /// if (R[a] >= i) != k then skip (`i <= R[a]`).
    GeI { a: R, i: i16, k: bool },
    Unm { a: R, b: R },
    BNot { a: R, b: R },
    Not { a: R, b: R },
    Len { a: R, b: R },
    /// R[a] = R[a] .. … .. R[a+n-1].
    Concat { a: R, n: u16 },
    /// R[a] = tostring(R[b]) (string interpolation).
    ToStr { a: R, b: R },
    /// Jump; with `close` > 0, first close upvalues and to-be-closed variables from
    /// register `close - 1` up (from the frame's base, never a window).
    Jmp { off: i32, close: u16 },
    Eq { b: K, c: K, k: bool },
    Lt { b: K, c: K, k: bool },
    Le { b: K, c: K, k: bool },
    /// if truthy(R[a]) != k then skip.
    Test { a: R, k: bool },
    /// if truthy(R[b]) == k then R[a] = R[b] else skip.
    TestSet { a: R, b: R, k: bool },
    /// Call R[a] with `b - 1` arguments (MULTI: up to the top), wanting `c - 1` results.
    Call { a: R, b: u16, c: u16 },
    TailCall { a: R, b: u16 },
    /// Return R[a..a+b-1] (`b` MULTI: up to the top).
    Return { a: R, b: u16 },
    /// `Return` of no value / of R[a] alone, in a function that is not vararg and has no
    /// to-be-closed variable (C Lua's `RETURN0` and `RETURN1`: `Proto::specialize_returns`).
    Return0,
    Return1 { a: R },
    ForPrep { a: R, off: i32 },
    ForLoop { a: R, off: i32 },
    TForPrep { a: R, off: i32 },
    /// The generic `for` iterator call: results to a+4.., `c` of them (MULTI: all, setting
    /// the top, for more than a 16-bit count; `PadNil` follows).
    TForCall { a: R, c: u16 },
    TForLoop { a: R, off: i32 },
    /// R[a][off+i] = R[a+i], 1 <= i <= n (n 0: up to the top).
    SetList { a: R, n: u8, off: u32 },
    /// R[a] = a closure of prototype `p`; its upvalues from registers count from the
    /// frame's base (never a window).
    Closure { a: R, p: u32 },
    /// R[a..a+n-2] = varargs (n 0: all of them, setting the top).
    VarArg { a: R, n: u16 },
    Close { a: R },
    Tbc { a: R },
    /// The next instruction's registers count from `base + off` (functions with more
    /// registers than 16 bits address: only they have this). A jump never lands on the
    /// instruction after it, and nothing skips over it alone.
    Window { off: u32 },
    /// R[a] = R[b | b_hi << 16]: `a` from the frame's base, `b` from the window's (the
    /// frame's base unless a `Window` comes first, which reaches sources past 2^24): a move
    /// between registers too far apart for one window.
    MoveAbs { a: u32, b: u16, b_hi: u8 },
    /// top = frame base + n: the next instruction (a call, tail call or return with its
    /// count MULTI) takes its values up to there. For counts past a 16-bit field.
    SetTop { n: u32 },
    /// The registers from the top (set by the instruction before, a call, `...` or `for`
    /// iterator call with MULTI results) up to frame base + n become nil: that instruction's
    /// results adjusted to a count past a 16-bit field.
    PadNil { n: u32 },
}

impl<R: Copy, K: Copy> Op<R, K> {
    /// The same instruction with its registers mapped by `r` and its register-or-constant
    /// operands by `k`.
    pub fn map<R2, K2>(self, r: impl Fn(R) -> R2, k: impl Fn(K) -> K2) -> Op<R2, K2> {
        match self {
            Op::Move { a, b } => Op::Move { a: r(a), b: r(b) },
            Op::LoadK { a, k } => Op::LoadK { a: r(a), k },
            Op::LoadInt { a, i } => Op::LoadInt { a: r(a), i },
            Op::LoadBool { a, v } => Op::LoadBool { a: r(a), v },
            Op::LoadFalseSkip { a } => Op::LoadFalseSkip { a: r(a) },
            Op::LoadNil { a, n } => Op::LoadNil { a: r(a), n },
            Op::GetUpval { a, u } => Op::GetUpval { a: r(a), u },
            Op::GetSelf { a, u } => Op::GetSelf { a: r(a), u },
            Op::SetUpval { a, u } => Op::SetUpval { a: r(a), u },
            Op::GetTabUp { a, u, k } => Op::GetTabUp { a: r(a), u, k },
            Op::SetTabUp { u, k: kk, v } => Op::SetTabUp { u, k: kk, v: k(v) },
            Op::GetTable { a, b, c } => Op::GetTable { a: r(a), b: r(b), c: r(c) },
            Op::GetTableK { a, b, k: kk } => Op::GetTableK { a: r(a), b: r(b), k: kk },
            Op::SetTable { a, b, c } => Op::SetTable { a: r(a), b: r(b), c: k(c) },
            Op::SetTableK { a, k: kk, c } => Op::SetTableK { a: r(a), k: kk, c: k(c) },
            Op::NewTable { a, arr, hash } => Op::NewTable { a: r(a), arr, hash },
            Op::SelfK { a, b, k: kk } => Op::SelfK { a: r(a), b: r(b), k: kk },
            Op::Arith { op, a, b, c } => Op::Arith { op, a: r(a), b: k(b), c: k(c) },
            Op::Add { a, b, c } => Op::Add { a: r(a), b: r(b), c: r(c) },
            Op::Sub { a, b, c } => Op::Sub { a: r(a), b: r(b), c: r(c) },
            Op::Mul { a, b, c } => Op::Mul { a: r(a), b: r(b), c: r(c) },
            Op::Div { a, b, c } => Op::Div { a: r(a), b: r(b), c: r(c) },
            Op::Mod { a, b, c } => Op::Mod { a: r(a), b: r(b), c: r(c) },
            Op::IDiv { a, b, c } => Op::IDiv { a: r(a), b: r(b), c: r(c) },
            Op::AddK { a, b, k: kk } => Op::AddK { a: r(a), b: r(b), k: kk },
            Op::SubK { a, b, k: kk } => Op::SubK { a: r(a), b: r(b), k: kk },
            Op::MulK { a, b, k: kk } => Op::MulK { a: r(a), b: r(b), k: kk },
            Op::DivK { a, b, k: kk } => Op::DivK { a: r(a), b: r(b), k: kk },
            Op::ModK { a, b, k: kk } => Op::ModK { a: r(a), b: r(b), k: kk },
            Op::IDivK { a, b, k: kk } => Op::IDivK { a: r(a), b: r(b), k: kk },
            Op::AddI { a, b, i, sub } => Op::AddI { a: r(a), b: r(b), i, sub },
            Op::Pow { a, b, c } => Op::Pow { a: r(a), b: r(b), c: r(c) },
            Op::PowK { a, b, k: kk } => Op::PowK { a: r(a), b: r(b), k: kk },
            Op::BAnd { a, b, c } => Op::BAnd { a: r(a), b: r(b), c: r(c) },
            Op::BAndK { a, b, k: kk } => Op::BAndK { a: r(a), b: r(b), k: kk },
            Op::BOr { a, b, c } => Op::BOr { a: r(a), b: r(b), c: r(c) },
            Op::BOrK { a, b, k: kk } => Op::BOrK { a: r(a), b: r(b), k: kk },
            Op::BXor { a, b, c } => Op::BXor { a: r(a), b: r(b), c: r(c) },
            Op::BXorK { a, b, k: kk } => Op::BXorK { a: r(a), b: r(b), k: kk },
            Op::Shl { a, b, c } => Op::Shl { a: r(a), b: r(b), c: r(c) },
            Op::ShlK { a, b, k: kk } => Op::ShlK { a: r(a), b: r(b), k: kk },
            Op::Shr { a, b, c } => Op::Shr { a: r(a), b: r(b), c: r(c) },
            Op::ShrK { a, b, k: kk } => Op::ShrK { a: r(a), b: r(b), k: kk },
            Op::EqK { a, k: kk, eq } => Op::EqK { a: r(a), k: kk, eq },
            Op::EqI { a, i, eq } => Op::EqI { a: r(a), i, eq },
            Op::LtI { a, i, k: kk } => Op::LtI { a: r(a), i, k: kk },
            Op::LeI { a, i, k: kk } => Op::LeI { a: r(a), i, k: kk },
            Op::GtI { a, i, k: kk } => Op::GtI { a: r(a), i, k: kk },
            Op::GeI { a, i, k: kk } => Op::GeI { a: r(a), i, k: kk },
            Op::Unm { a, b } => Op::Unm { a: r(a), b: r(b) },
            Op::BNot { a, b } => Op::BNot { a: r(a), b: r(b) },
            Op::Not { a, b } => Op::Not { a: r(a), b: r(b) },
            Op::Len { a, b } => Op::Len { a: r(a), b: r(b) },
            Op::ToStr { a, b } => Op::ToStr { a: r(a), b: r(b) },
            Op::Concat { a, n } => Op::Concat { a: r(a), n },
            Op::Jmp { off, close } => Op::Jmp { off, close },
            Op::Eq { b, c, k: kk } => Op::Eq { b: k(b), c: k(c), k: kk },
            Op::Lt { b, c, k: kk } => Op::Lt { b: k(b), c: k(c), k: kk },
            Op::Le { b, c, k: kk } => Op::Le { b: k(b), c: k(c), k: kk },
            Op::Test { a, k: kk } => Op::Test { a: r(a), k: kk },
            Op::TestSet { a, b, k: kk } => Op::TestSet { a: r(a), b: r(b), k: kk },
            Op::Call { a, b, c } => Op::Call { a: r(a), b, c },
            Op::TailCall { a, b } => Op::TailCall { a: r(a), b },
            Op::Return { a, b } => Op::Return { a: r(a), b },
            Op::Return0 => Op::Return0,
            Op::Return1 { a } => Op::Return1 { a: r(a) },
            Op::ForPrep { a, off } => Op::ForPrep { a: r(a), off },
            Op::ForLoop { a, off } => Op::ForLoop { a: r(a), off },
            Op::TForPrep { a, off } => Op::TForPrep { a: r(a), off },
            Op::TForLoop { a, off } => Op::TForLoop { a: r(a), off },
            Op::TForCall { a, c } => Op::TForCall { a: r(a), c },
            Op::SetList { a, n, off } => Op::SetList { a: r(a), n, off },
            Op::Closure { a, p } => Op::Closure { a: r(a), p },
            Op::VarArg { a, n } => Op::VarArg { a: r(a), n },
            Op::Close { a } => Op::Close { a: r(a) },
            Op::Tbc { a } => Op::Tbc { a: r(a) },
            Op::Window { off } => Op::Window { off },
            Op::MoveAbs { a, b, b_hi } => Op::MoveAbs { a, b, b_hi },
            Op::SetTop { n } => Op::SetTop { n },
            Op::PadNil { n } => Op::PadNil { n },
        }
    }
}

/// Where a closure's upvalue comes from when it is made.
#[derive(Clone, Debug)]
pub(crate) struct UpvalDesc {
    /// From the enclosing function's register `idx` (else its upvalue `idx`).
    pub in_stack: bool,
    pub idx: u32,
    pub name: Arc<str>,
}

/// A local's name and where it is live, for error messages.
#[derive(Clone, Debug)]
pub(crate) struct LocVar {
    pub name: Arc<str>,
    pub reg: Reg,
    pub start: u32,
    pub end: u32,
}

/// Settles what `p`'s upvalues are, once the functions nested in it are finished:
/// `p.upval_writes`, a bit per upvalue (past 64: all) that `p` or a function nested in it
/// may assign; and in each function nested in it that a `local function` makes, the reads of
/// that local turned into `GetSelf` when nothing assigns the local while it lives (the
/// local then holds the closure made into it: the one running when it is read).
pub(crate) fn settle_upvals(p: &mut Proto, protos: &mut crate::heap::Slab<Proto>) {
    p.specialize_returns(protos);
    let written = |c: &Proto, j: usize| j >= 64 || c.upval_writes & 1 << j != 0;
    // The registers nested functions may assign (past 255: not looked at below).
    let mut regs = [0u64; 4];
    let mut writes = 0u64;
    for i in &p.code {
        if let Op::SetUpval { u, .. } = *i {
            writes |= 1u64.checked_shl(u32::from(u)).unwrap_or(0);
        }
    }
    for c in p.protos.iter().filter_map(|&c| protos.get(c)) {
        for (_, d) in c.upvals.iter().enumerate().filter(|&(j, _)| written(c, j)) {
            let i = d.idx as usize;
            match (d.in_stack, regs.get_mut(i / 64)) {
                (true, Some(w)) => *w |= 1 << (i % 64),
                (false, _) => writes |= 1u64.checked_shl(d.idx).unwrap_or(0),
                (true, None) => {}
            }
        }
    }
    p.upval_writes = writes;
    if p.wide() || p.frame > 0 {
        return;
    }
    // Each local's life scanned once at most, and all of them within twice the code: past
    // that the rest are left as they are (a bound on the work, not a different result).
    let mut budget = 2 * p.code.len();
    for v in &p.locvars {
        let (r, start, end) = (v.reg, v.start as usize, (v.end as usize).min(p.code.len()));
        let Some(&Op::Closure { a, p: ci }) = p.code.get(start) else { continue };
        let life = p.code.get(start + 1..end).unwrap_or_default();
        if a != r || regs.get(usize::from(r) / 64).is_some_and(|w| w & 1 << (r % 64) != 0) || life.len() > budget {
            continue;
        }
        budget -= life.len();
        if life.iter().any(|i| crate::debug::sets_reg(i, r)) {
            continue;
        }
        let Some(c) = p.protos.get(ci as usize).and_then(|&c| protos.get_mut(c)) else { continue };
        let mine = |u: u16| c.upvals.get(usize::from(u)).is_some_and(|d| d.in_stack && d.idx == u32::from(r));
        for i in 0..c.code.len() {
            if let Some(&Op::GetUpval { a, u }) = c.code.get(i)
                && mine(u)
                && let Some(slot) = c.code.get_mut(i)
            {
                *slot = Op::GetSelf { a, u };
            }
        }
    }
}

/// A compiled function.
#[derive(Default)]
pub(crate) struct Proto {
    pub code: Vec<Instr>,
    pub lines: Vec<u32>,
    pub consts: Vec<Value>,
    /// Nested functions: prototypes in the heap.
    pub protos: Vec<u32>,
    pub upvals: Vec<UpvalDesc>,
    pub locvars: Vec<LocVar>,
    pub num_params: u32,
    pub is_vararg: bool,
    pub max_stack: u16,
    /// Registers it uses in all, when more than one window holds (else 0): past
    /// `max_stack`, reached through `Window` and `MoveAbs`.
    pub frame: u32,
    pub source: Arc<str>,
    pub line_defined: u32,
    /// The line of its `end` (0 for a main chunk): `debug.getinfo`'s `lastlinedefined`.
    pub last_line_defined: u32,
    /// The chunk's name as given (`=t`, `@file`, or the source text itself), a string the
    /// collector keeps through the prototype: `debug.getinfo`'s `source`. Nil until set
    /// (`Lua::set_chunk_name`).
    pub chunk_name: Value,
    /// Inline cache per instruction: the hash slot a string-keyed field was last found
    /// in (a hint, checked before use).
    pub hints: Vec<crate::nostd::cell::Cell<u32>>,
    /// The upvalues it or a function nested in it may assign (see `settle_upvals`).
    pub upval_writes: u64,
}

/// Register window of a function that fits in 8-bit registers, and of a wide one: the
/// stack always holds a full window past the running frame's base, so registers are
/// indexed without checks (narrow: `& 0xff`; wide: `& 0x7fff`, `MAX_REGS` being below).
pub(crate) const WINDOW: usize = 256;
pub(crate) const WIDE_WINDOW: usize = 1 << 15;

impl Proto {
    /// C Lua's `luaK_finish`: in a function that is not vararg and needs nothing closed (no
    /// to-be-closed variable, no local a nested function captures: `needclose`), a return
    /// of no value is `Return0` and of one value `Return1`.
    pub(crate) fn specialize_returns(&mut self, protos: &crate::heap::Slab<Proto>) {
        let captured = self.protos.iter().any(|&c| protos.get(c).is_some_and(|c| c.upvals.iter().any(|u| u.in_stack)));
        // (A generic `for`'s fourth value is a to-be-closed variable too: `TForPrep`.)
        if self.is_vararg || captured || self.code.iter().any(|i| matches!(i, Op::Tbc { .. } | Op::TForPrep { .. })) {
            return;
        }
        for i in &mut self.code {
            *i = match *i {
                Op::Return { b: 0, .. } => Op::Return0,
                Op::Return { a, b: 1 } => Op::Return1 { a },
                x => x,
            };
        }
    }

    /// Its register window (see `WINDOW`).
    pub fn window(&self) -> usize {
        if self.wide() { WIDE_WINDOW } else { WINDOW }
    }

    /// Stack slots its registers take from the frame's base.
    pub fn frame_size(&self) -> usize {
        usize::from(self.max_stack).max(self.frame as usize)
    }

    /// Whether it needs the wide interpreter loop (registers past 255).
    pub fn wide(&self) -> bool {
        self.max_stack > 256
    }

    /// The source line of instruction `pc`.
    pub fn line(&self, pc: usize) -> u32 {
        self.lines.get(pc).copied().unwrap_or(self.line_defined)
    }

    /// Bytes it holds (for the memory limit; nested functions are prototypes of their
    /// own).
    pub fn mem_size(&self) -> usize {
        160 + self.code.len() * 12
            + self.lines.len() * 4
            + self.consts.len() * 16
            + self.protos.len() * 4
            + self.upvals.iter().map(|u| 32 + u.name.len()).sum::<usize>()
            + self.locvars.iter().map(|l| 40 + l.name.len()).sum::<usize>()
    }
}
