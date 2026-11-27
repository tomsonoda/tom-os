//! The type checker for RemedyLua's annotations (`.rlua`): diagnostics only, never a runtime
//! effect. `docs/reference.md` ("The type checker") is the design; in short:
//!
//! - **Gradual.** Unannotated code is dynamic (`any`): a global, an unannotated parameter,
//!   the result of a call to an unannotated function, a local whose initializer is not
//!   typed. Types come from annotations, and from literals and operators where they meet
//!   annotated code. An operation is checked only when one of its operands has a type that
//!   came from an annotation, so a plain Lua program never gets a type diagnostic.
//! - **Sound where it speaks.** A value assigned, passed, returned or stored where an
//!   annotation stands must have a type the annotation accepts (subtyping with invariant
//!   table contents, contravariant parameters, covariant results; a table constructor is
//!   fresh, so it is checked covariantly). An operation is reported when some value of its
//!   operands' types makes it fail at run time (indexing a possibly-nil value, calling a
//!   number). `any` is compatible with everything, both ways, and nothing is checked at run
//!   time: an `any` value that breaks an annotation is not caught.
//! - **Bounded.** No recursion that follows the program's nesting: the walker, the type
//!   conversion, subtyping, flattening and printing use explicit stacks. Work is counted
//!   (`max_steps`, linear in the source) and memory is counted (`memory`); past either the
//!   check stops with one warning, keeping what it found.
//!
//! The walk runs twice over the same tree in the same order. The first pass finds which
//! locals are ever assigned after their declaration and which loops contain a `break`; the
//! second checks. A local that is never reassigned keeps the type of its initializer and
//! can be narrowed by conditions (`if x then`, `x ~= nil`, `x and x.y`, an early `return`).

#[allow(unused_imports)]
use crate::nostd::prelude::*;
use crate::nostd::collections::{HashMap, HashSet};
use crate::nostd::rc::Rc;

use crate::ast::{BinOp, Block, Expr, ExprKind, Field, ForNum, FuncBody, InterpExpr, Stat, StatKind, Type, UnOp};
use crate::diag::{Diagnostic, Pos};

/// A type: an index into the checker's arena.
type T = u32;

/// A signature's parameters, varargs' type and results (see `Sig`).
type SigParts = (Box<[T]>, Option<T>, Option<Rc<[T]>>);

const ANY: T = 0;
const NIL: T = 1;
const BOOL: T = 2;
const NUM: T = 3;
const INT: T = 4;
const STR: T = 5;
const FUNC: T = 6;
const TABLE: T = 7;
const UDATA: T = 8;
const THREAD: T = 9;
/// `true` and `false` (no annotation names them; `boolean` is both).
const TRUE: T = 10;
const FALSE: T = 11;

/// Most diagnostics one check reports.
const MAX_DIAGS: usize = 500;
/// Steps between looks at the memory held.
const MEMORY_EVERY: u64 = 1024;
/// Nodes of a condition looked at for narrowing (more are ignored: less narrowing, never
/// a wrong one).
const FACT_NODES: usize = 64;
/// Characters of a type in a message.
const SHOW_MAX: usize = 120;

/// Built-in type names.
const BUILTIN: &[(&str, T)] = &[
    ("any", ANY),
    ("nil", NIL),
    ("boolean", BOOL),
    ("number", NUM),
    ("integer", INT),
    ("string", STR),
    ("function", FUNC),
    ("table", TABLE),
    ("userdata", UDATA),
    ("thread", THREAD),
];

enum Ty {
    Any,
    Nil,
    Bool,
    Num,
    Int,
    Str,
    Func,
    Table,
    Udata,
    Thread,
    True,
    False,
    /// A string literal type.
    Lit(Box<[u8]>),
    Sig(Box<Sig>),
    Array(T),
    Map(T, T),
    /// Fields sorted by name, no duplicates.
    Shape(Box<[(Box<str>, T)]>),
    Union(Box<[T]>),
    /// A `type` alias (its `AliasDef`).
    Alias(u32),
    /// A table constructor's type: fresh, so it is checked covariantly.
    Ctor(Box<Ctor>),
}

struct Sig {
    params: Box<[T]>,
    /// The varargs' type, when the function is variadic.
    va: Option<T>,
    /// The results; `None` when not annotated (any number of `any`).
    ret: Option<Rc<[T]>>,
}

struct Ctor {
    /// Fields by name (sorted, the last of duplicates kept).
    named: Box<[(Box<str>, T)]>,
    items: Box<[T]>,
    /// The values of a last positional call or `...`.
    tail: Multi,
    /// Computed keys and their values.
    keyed: Box<[(T, T)]>,
}

struct AliasDef {
    name: String,
    /// What it stands for (never another alias once its block is set up).
    target: T,
    /// Its `Ty::Alias`.
    t: T,
}

/// What follows an expression's first value when it can give several.
#[derive(Clone)]
enum Multi {
    One,
    /// Exactly these values (the first included).
    List(Rc<[T]>),
    /// Any number of values of this type.
    Rep(T),
}

/// The value of an expression: its type, whether that type came from an annotation
/// (`st`, "static"), and the values after the first.
#[derive(Clone)]
struct V {
    ty: T,
    st: bool,
    multi: Multi,
}

impl V {
    fn one(ty: T, st: bool) -> V {
        V { ty, st, multi: Multi::One }
    }
    fn dynamic() -> V {
        V { ty: ANY, st: false, multi: Multi::Rep(ANY) }
    }
}

/// An expression list spread into values.
struct Spread {
    fixed: Vec<(T, bool)>,
    rep: Option<(T, bool)>,
}

struct Local<'a> {
    name: &'a str,
    /// The annotation, when it has one: every assignment must fit it.
    decl: Option<T>,
    /// Its type here (narrowed by conditions).
    ty: T,
    st: bool,
    /// Its declaration's number (the same in both passes).
    id: u32,
    /// How many functions deep it was declared.
    depth: u32,
}

struct Scope {
    locals: usize,
    narrow: usize,
    aliases: usize,
}

struct FnCtx {
    ret: Option<Rc<[T]>>,
    /// The varargs' value (when the function is variadic).
    va: Option<V>,
}

/// Where a `function a.b:c()` statement stores its function.
enum Target<'a> {
    Local(usize),
    Global,
    Field(V, &'a str),
}

enum W<'a> {
    Block(&'a Block),
    EndScope,
    Stat(&'a Stat),
    LocalEnd(&'a Stat),
    AssignEnd(&'a Stat),
    OpAssignEnd(&'a Stat),
    CallStatEnd(&'a Stat),
    Discard,
    LoopEnd,
    IfStep(&'a Stat, usize),
    IfAfter(&'a Stat),
    NarrowEnter(&'a Expr, bool),
    NarrowExit,
    NumForHead(&'a Stat),
    GenForHead(&'a Stat),
    FuncStatEnd(Target<'a>, (Pos, Pos)),
    ReturnEnd(&'a Stat),
    Expr(&'a Expr),
    ExprEnd(&'a Expr),
    AndOrEnd(&'a Expr),
    FuncLit(&'a FuncBody, Option<V>, Option<(T, bool)>),
    FuncEnd(&'a FuncBody, T, bool),
}

// ---- kinds: what a value of a type can be at run time ----

const K_NIL: u16 = 1;
const K_BOOL: u16 = 2;
const K_INT: u16 = 4;
const K_FLOAT: u16 = 8;
const K_STR: u16 = 16;
/// A string literal that is not a number (arithmetic on it fails).
const K_BADSTR: u16 = 32;
const K_FUNC: u16 = 64;
const K_TABLE: u16 = 128;
const K_UDATA: u16 = 256;
const K_THREAD: u16 = 512;
const K_ANY: u16 = 1024;
const K_NUM: u16 = K_INT | K_FLOAT;
/// Kinds that may have a metatable of their own (so a metamethod may make an operation
/// work). Strings have the standard string metatable, which only has `__index`.
const K_META: u16 = K_TABLE | K_UDATA | K_ANY;

fn kind_name(k: u16) -> &'static str {
    for (bit, name) in [
        (K_NIL, "nil"),
        (K_BOOL, "boolean"),
        (K_INT, "number"),
        (K_FLOAT, "number"),
        (K_BADSTR, "string"),
        (K_STR, "string"),
        (K_FUNC, "function"),
        (K_THREAD, "thread"),
        (K_TABLE, "table"),
        (K_UDATA, "userdata"),
    ] {
        if k & bit != 0 {
            return name;
        }
    }
    "value"
}

/// The operations whose operand kinds are checked.
#[derive(Clone, Copy)]
enum Op {
    Index,
    Call,
    Arith,
    Bitwise,
    Concat,
    Len,
}

impl Op {
    /// The kinds this operation fails on (unless a metamethod helps).
    fn bad(self) -> u16 {
        match self {
            Op::Index => K_NIL | K_BOOL | K_NUM | K_FUNC | K_THREAD,
            Op::Call => K_NIL | K_BOOL | K_NUM | K_STR | K_THREAD,
            Op::Arith | Op::Bitwise => K_NIL | K_BOOL | K_BADSTR | K_FUNC | K_THREAD,
            Op::Concat => K_NIL | K_BOOL | K_FUNC | K_THREAD,
            Op::Len => K_NIL | K_BOOL | K_NUM | K_FUNC | K_THREAD,
        }
    }
    fn what(self) -> &'static str {
        match self {
            Op::Index => "index",
            Op::Call => "call",
            Op::Arith => "perform arithmetic on",
            Op::Bitwise => "perform bitwise operation on",
            Op::Concat => "concatenate",
            Op::Len => "get length of",
        }
    }
}

fn addr<X>(x: &X) -> usize {
    crate::nostd::ptr::from_ref(x) as usize
}

/// Checks `block` (parsed from a `.rlua` chunk of `src_len` bytes), using at most `memory`
/// bytes for its own tables: its diagnostics.
pub(crate) fn check(block: &Block, chunk: &str, src_len: usize, memory: usize) -> Vec<Diagnostic> {
    let max_steps = 4_000_000u64.saturating_add((src_len as u64).saturating_mul(128));
    let mut c = Checker::new(chunk, max_steps, memory);
    c.run(block);
    if c.stopped.is_none() {
        c = Checker::second(c);
        c.run(block);
    }
    let mut diags = crate::nostd::mem::take(&mut c.diags);
    if let Some(why) = c.stopped {
        let at = if c.pass == 2 { c.at } else { Pos { line: 1, col: 1 } };
        diags.push(Diagnostic::warning(chunk, at, format!("type checking stopped here: {why}; what follows was not checked")));
    }
    diags
}

/// One step of `sub`: the answer, or the goals that decide it (all of them, or any).
type Goals = Result<bool, (bool, Vec<(T, T)>)>;

#[derive(Default)]
struct Checker<'a> {
    chunk: String,
    /// The pass (1 finds assignments and breaks, 2 checks).
    pass: u8,
    types: Vec<Ty>,
    /// Bytes the types hold besides their slots.
    ty_bytes: usize,
    lits: HashMap<Box<[u8]>, T>,
    opts: HashMap<T, T>,
    kinds_of: HashMap<T, u16>,
    truthy_of: HashMap<T, T>,
    falsy_of: HashMap<T, T>,
    aliases: Vec<AliasDef>,
    alias_names: HashMap<&'a str, Vec<u32>>,
    alias_stack: Vec<&'a str>,
    locals: Vec<Local<'a>>,
    names: HashMap<&'a str, Vec<u32>>,
    scopes: Vec<Scope>,
    narrow: Vec<(usize, T)>,
    nmarks: Vec<usize>,
    fns: Vec<FnCtx>,
    /// Loops being walked (`u32::MAX` marks a function's start).
    loops: Vec<u32>,
    decls: u32,
    loop_count: u32,
    /// Per declaration: assigned after it (from the first pass).
    assigned: Vec<bool>,
    /// Per loop: has a `break` (from the first pass).
    loop_break: Vec<bool>,
    /// Per loop: the last loop inside it (loops are numbered in the order they start).
    loop_last: Vec<u32>,
    /// Per declaration: assigned from inside a function nested in its own.
    captured: Vec<bool>,
    /// Every assignment to a local: its declaration and the innermost loop around it
    /// (sorted for the second pass).
    assign_at: Vec<(u32, u32)>,
    /// Loops (by statement) with a `break`.
    breaks_at: HashMap<usize, bool>,
    /// Call statements that call the global `error`.
    error_calls: HashSet<usize>,
    /// `if` statements (with `else`) whose every branch leaves the block.
    if_exits: HashMap<usize, bool>,
    /// Goals being decided that are assumed true meanwhile, and their frames' depths.
    assume: HashMap<(T, T), u32>,
    /// Goals decided (see `sub`).
    memo: HashMap<(T, T), bool>,
    /// Where expressions start, found so far (by address).
    starts: HashMap<usize, Pos>,
    work: Vec<W<'a>>,
    vals: Vec<V>,
    diags: Vec<Diagnostic>,
    steps: u64,
    max_steps: u64,
    memory: usize,
    stopped: Option<&'static str>,
    /// The statement being checked (where a stopped check stopped).
    at: Pos,
}

impl<'a> Checker<'a> {
    fn new(chunk: &str, max_steps: u64, memory: usize) -> Self {
        let types = vec![
            Ty::Any,
            Ty::Nil,
            Ty::Bool,
            Ty::Num,
            Ty::Int,
            Ty::Str,
            Ty::Func,
            Ty::Table,
            Ty::Udata,
            Ty::Thread,
            Ty::True,
            Ty::False,
        ];
        Checker {
            chunk: chunk.to_owned(),
            pass: 1,
            types,
            max_steps,
            memory,
            at: Pos { line: 1, col: 1 },
            ..Default::default()
        }
    }

    /// The second pass, with what the first found (and its step count).
    fn second(first: Checker<'a>) -> Self {
        let mut c = Checker::new(&first.chunk, first.max_steps, first.memory);
        c.pass = 2;
        c.steps = first.steps;
        c.assigned = first.assigned;
        c.loop_break = first.loop_break;
        c.loop_last = first.loop_last;
        c.captured = first.captured;
        c.assign_at = first.assign_at;
        c.assign_at.sort_unstable();
        c
    }

    // ---- budgets ----

    /// Counts `n` steps of work: false once the check must stop.
    fn tick(&mut self, n: u64) -> bool {
        if self.stopped.is_some() {
            return false;
        }
        let before = self.steps;
        self.steps = self.steps.saturating_add(n);
        if self.steps > self.max_steps {
            self.stopped = Some("the program is too large or its types too complex for the work a check may do");
            return false;
        }
        if self.steps / MEMORY_EVERY != before / MEMORY_EVERY && self.held() > self.memory {
            self.stopped = Some("not enough memory");
            return false;
        }
        true
    }

    /// The memory the checker holds (counted from its tables' sizes).
    fn held(&self) -> usize {
        use crate::nostd::mem::size_of;
        let maps = (self.lits.len()
            + self.opts.len()
            + self.kinds_of.len()
            + self.truthy_of.len()
            + self.falsy_of.len()
            + self.names.len()
            + self.alias_names.len()
            + self.breaks_at.len()
            + self.error_calls.len()
            + self.if_exits.len()
            + self.assume.len()
            + self.memo.len()
            + self.starts.len())
            * 48;
        self.types.capacity() * size_of::<Ty>()
            + self.ty_bytes
            + maps
            + self.aliases.capacity() * (size_of::<AliasDef>() + 16)
            + self.locals.capacity() * (size_of::<Local<'_>>() + 8)
            + self.scopes.capacity() * size_of::<Scope>()
            + self.narrow.capacity() * size_of::<(usize, T)>()
            + self.fns.capacity() * size_of::<FnCtx>()
            + self.loops.capacity() * 4
            + self.assigned.capacity()
            + self.loop_break.capacity()
            + self.loop_last.capacity() * 4
            + self.captured.capacity()
            + self.assign_at.capacity() * 8
            + self.work.capacity() * size_of::<W<'_>>()
            + self.vals.capacity() * size_of::<V>()
            + self.diags.capacity() * (size_of::<Diagnostic>() + 64)
    }

    fn add(&mut self, t: Ty) -> T {
        let extra = match &t {
            Ty::Lit(b) => b.len(),
            Ty::Sig(s) => 64 + s.params.len() * 4 + s.ret.as_ref().map_or(0, |r| r.len() * 4),
            Ty::Shape(f) => f.iter().map(|(n, _)| n.len() + 24).sum(),
            Ty::Union(m) => m.len() * 4,
            Ty::Ctor(c) => 96 + c.named.iter().map(|(n, _)| n.len() + 24).sum::<usize>() + c.items.len() * 4 + c.keyed.len() * 8,
            _ => 0,
        };
        self.ty_bytes = self.ty_bytes.saturating_add(extra);
        let id = u32::try_from(self.types.len()).unwrap_or(u32::MAX);
        if id == u32::MAX {
            self.stopped = Some("not enough memory");
            return ANY;
        }
        self.types.push(t);
        id
    }

    fn ty(&self, t: T) -> &Ty {
        self.types.get(t as usize).unwrap_or(&Ty::Any)
    }

    // ---- diagnostics ----

    fn diag(&mut self, pos: Pos, end: Pos, msg: String) {
        if self.pass == 2 && self.diags.len() < MAX_DIAGS {
            self.diags.push(Diagnostic::error(&self.chunk, pos, msg).to(end));
        }
    }

    /// Where `e` starts (its leftmost token). Remembered for the expressions passed on
    /// the way, so a long left spine is walked once however often it is asked about.
    fn start(&mut self, e: &Expr) -> Pos {
        let mut path = Vec::new();
        let mut at = e;
        let pos = loop {
            if let Some(&p) = self.starts.get(&addr(at)) {
                break p;
            }
            if !self.tick(1) {
                break at.pos;
            }
            let next = match &at.kind {
                ExprKind::Bin(_, l, _) | ExprKind::Index(l, _) | ExprKind::Call(l, _) | ExprKind::Method(l, ..) => Some(&**l),
                ExprKind::Concat(v) => v.first(),
                _ => None,
            };
            match next {
                Some(n) => {
                    path.push(addr(at));
                    at = n;
                }
                None => break at.pos,
            }
        };
        for a in path {
            self.starts.insert(a, pos);
        }
        pos
    }

    fn diag_at(&mut self, e: &Expr, msg: String) {
        if self.pass == 2 && self.diags.len() < MAX_DIAGS {
            let s = self.start(e);
            self.diag(s, e.end, msg);
        }
    }

    /// How a message names `e`: ` (local 'x')` and the like.
    fn describe(&self, e: &Expr) -> String {
        match &e.kind {
            ExprKind::Name(n) => {
                if self.lookup(n).is_some() {
                    format!("local '{n}'")
                } else {
                    format!("global '{n}'")
                }
            }
            ExprKind::Index(_, k) => match &k.kind {
                ExprKind::Str(s) => format!("field '{}'", String::from_utf8_lossy(s)),
                _ => String::new(),
            },
            ExprKind::Method(_, m, ..) => format!("method '{m}'"),
            ExprKind::Str(s) => format!("constant '{}'", short(&String::from_utf8_lossy(s))),
            _ => String::new(),
        }
    }

    /// Reports `op` on a value of type `t` (named by `e`, the expression to mark) when
    /// some value of that type makes it fail.
    fn op_check(&mut self, op: Op, t: T, e: &Expr, mark: &Expr) {
        if self.pass != 2 {
            return;
        }
        let k = self.kinds(t);
        let bad = k & op.bad();
        if bad == 0 || k & K_ANY != 0 {
            return;
        }
        let desc = self.describe(e);
        let total = self.flatten(t).into_iter().all(|m| self.kinds(m) & op.bad() != 0);
        let msg = if total {
            let d = if desc.is_empty() { String::new() } else { format!(" ({desc})") };
            format!("attempt to {} a {} value{d}", op.what(), kind_name(bad))
        } else {
            let shown = self.show(t);
            let d = if desc.is_empty() { format!(" (its type is {shown})") } else { format!(" ({desc} is {shown})") };
            format!("attempt to {} a value that may be {}{d}", op.what(), kind_name(bad))
        };
        self.diag_at(mark, msg);
    }

    /// `place: expected T, got S` when `s` does not fit `t`.
    fn fit(&mut self, s: T, t: T, place: &dyn Fn() -> String, at: (Pos, Pos)) -> bool {
        if self.sub(s, t) {
            return true;
        }
        if self.pass == 2 {
            let (ts, ss) = (self.show(t), self.show(s));
            self.diag(at.0, at.1, format!("{}: expected {ts}, got {ss}", place()));
        }
        false
    }

    fn range(&mut self, e: &Expr) -> (Pos, Pos) {
        (self.start(e), e.end)
    }

    // ---- types: making them ----

    fn lit(&mut self, s: &[u8]) -> T {
        if let Some(&t) = self.lits.get(s) {
            return t;
        }
        let t = self.add(Ty::Lit(s.into()));
        self.lits.insert(s.into(), t);
        t
    }

    fn union2(&mut self, a: T, b: T) -> T {
        if a == b {
            return a;
        }
        if (a, b) == (TRUE, FALSE) || (a, b) == (FALSE, TRUE) {
            return BOOL;
        }
        if a == ANY || b == ANY {
            return ANY;
        }
        if self.absorbs(a, b) {
            return a;
        }
        if self.absorbs(b, a) {
            return b;
        }
        if b == NIL {
            return self.opt(a);
        }
        self.add(Ty::Union(Box::new([a, b])))
    }

    /// Whether `b`'s values are all `a`'s, by a quick look (no structure).
    fn absorbs(&self, a: T, b: T) -> bool {
        match (self.ty(a), self.ty(b)) {
            (Ty::Num, Ty::Int) | (Ty::Str, Ty::Lit(_)) | (Ty::Table, Ty::Ctor(_)) | (Ty::Bool, Ty::True | Ty::False) => true,
            (Ty::Func, Ty::Sig(_)) => true,
            (Ty::Union(m), _) => m.contains(&b),
            _ => false,
        }
    }

    /// `t | nil`.
    fn opt(&mut self, t: T) -> T {
        if t == ANY || t == NIL {
            return t;
        }
        if let Some(&o) = self.opts.get(&t) {
            return o;
        }
        if self.kinds(t) & K_NIL != 0 {
            return t;
        }
        let o = self.add(Ty::Union(Box::new([t, NIL])));
        self.opts.insert(t, o);
        o
    }

    fn union_of(&mut self, mut m: Vec<T>) -> T {
        if m.contains(&ANY) {
            return ANY;
        }
        match m.len() {
            0 => ANY,
            1 => m.pop().unwrap_or(ANY),
            _ => self.add(Ty::Union(m.into_boxed_slice())),
        }
    }

    /// A constructor's type made a table type, for a local that keeps it.
    fn widen(&mut self, t: T) -> T {
        match self.ty(t) {
            Ty::Ctor(_) => TABLE,
            Ty::Union(_) => {
                let m = self.flatten(t);
                if m.iter().any(|&x| matches!(self.ty(x), Ty::Ctor(_))) {
                    let m = m.into_iter().map(|x| if matches!(self.ty(x), Ty::Ctor(_)) { TABLE } else { x }).collect();
                    self.union_of(m)
                } else {
                    t
                }
            }
            _ => t,
        }
    }

    // ---- types: annotations ----

    /// The type an annotation stands for (diagnostics about it at `at`).
    fn conv(&mut self, root: &'a Type, at: Pos) -> T {
        enum C<'t> {
            Visit(&'t Type),
            Build(&'t Type),
        }
        let mut stack = vec![C::Visit(root)];
        let mut out: Vec<T> = Vec::new();
        while let Some(c) = stack.pop() {
            if !self.tick(1) {
                return ANY;
            }
            match c {
                C::Visit(t) => match t {
                    Type::Named(n, pos) => {
                        let r = self.named(n, *pos);
                        out.push(r);
                    }
                    Type::Nil => out.push(NIL),
                    Type::Literal(s) => {
                        let r = self.lit(s);
                        out.push(r);
                    }
                    Type::Optional(x) | Type::Array(x) => {
                        stack.push(C::Build(t));
                        stack.push(C::Visit(x));
                    }
                    Type::Map(k, v) => {
                        stack.push(C::Build(t));
                        stack.push(C::Visit(v));
                        stack.push(C::Visit(k));
                    }
                    Type::Union(v) => {
                        stack.push(C::Build(t));
                        stack.extend(v.iter().rev().map(C::Visit));
                    }
                    Type::Func { params, ret, .. } => {
                        stack.push(C::Build(t));
                        stack.extend(ret.iter().rev().map(C::Visit));
                        stack.extend(params.iter().rev().map(C::Visit));
                    }
                    Type::Shape(fs) => {
                        stack.push(C::Build(t));
                        stack.extend(fs.iter().rev().map(|(_, x)| C::Visit(x)));
                    }
                },
                C::Build(t) => {
                    let n = match t {
                        Type::Optional(_) | Type::Array(_) => 1,
                        Type::Map(..) => 2,
                        Type::Union(v) => v.len(),
                        Type::Func { params, ret, .. } => params.len() + ret.len(),
                        Type::Shape(fs) => fs.len(),
                        _ => 0,
                    };
                    let parts = out.split_off(out.len().saturating_sub(n));
                    let r = match t {
                        Type::Optional(_) => {
                            let x = parts.first().copied().unwrap_or(ANY);
                            self.opt(x)
                        }
                        Type::Array(_) => self.add(Ty::Array(parts.first().copied().unwrap_or(ANY))),
                        Type::Map(..) => {
                            let k = parts.first().copied().unwrap_or(ANY);
                            let v = parts.get(1).copied().unwrap_or(ANY);
                            self.add(Ty::Map(k, v))
                        }
                        Type::Union(_) => self.union_of(parts),
                        Type::Func { params, vararg, .. } => {
                            let (p, r) = parts.split_at(params.len().min(parts.len()));
                            let sig = Sig { params: p.into(), va: vararg.then_some(ANY), ret: Some(r.into()) };
                            self.add(Ty::Sig(Box::new(sig)))
                        }
                        Type::Shape(fs) => {
                            let mut fields: Vec<(Box<str>, T)> =
                                fs.iter().zip(parts).map(|((n, _), t)| (n.as_str().into(), t)).collect();
                            fields.sort_by(|a, b| a.0.cmp(&b.0));
                            let before = fields.len();
                            fields.dedup_by(|a, b| a.0 == b.0);
                            if fields.len() != before {
                                self.diag(at, at, "a field appears twice in a table type".into());
                            }
                            self.add(Ty::Shape(fields.into_boxed_slice()))
                        }
                        _ => ANY,
                    };
                    out.push(r);
                }
            }
        }
        out.pop().unwrap_or(ANY)
    }

    fn named(&mut self, n: &str, pos: Pos) -> T {
        if let Some(&(_, t)) = BUILTIN.iter().find(|(b, _)| *b == n) {
            return t;
        }
        if let Some(&i) = self.alias_names.get(n).and_then(|v| v.last()) {
            return self.aliases.get(i as usize).map_or(ANY, |a| a.t);
        }
        if n.contains('.') {
            // A host's or module's type: not known here.
            return ANY;
        }
        let end = Pos { line: pos.line, col: pos.col.saturating_add(u32::try_from(n.len()).unwrap_or(0)) };
        self.diag(pos, end, format!("unknown type '{n}'"));
        ANY
    }

    /// The aliases a block defines, visible in all of it (so they may refer to each other).
    fn hoist(&mut self, b: &'a Block) {
        let mut defs = Vec::new();
        let mut here: HashSet<&'a str> = HashSet::<_, crate::nostd::collections::hash_map::RandomState>::default();
        for s in &b.stats {
            if let StatKind::TypeDef(name, ty) = &s.kind {
                if !self.tick(1) {
                    return;
                }
                let end = Pos { line: s.pos.line, col: s.pos.col.saturating_add(4) };
                if BUILTIN.iter().any(|(b, _)| b == name) {
                    self.diag(s.pos, end, format!("cannot redefine the built-in type '{name}'"));
                    continue;
                }
                if !here.insert(name.as_str()) {
                    self.diag(s.pos, end, format!("type '{name}' is already defined in this block"));
                    continue;
                }
                let id = u32::try_from(self.aliases.len()).unwrap_or(u32::MAX);
                let t = self.add(Ty::Alias(id));
                self.aliases.push(AliasDef { name: name.clone(), target: ANY, t });
                self.alias_names.entry(name.as_str()).or_default().push(id);
                self.alias_stack.push(name.as_str());
                defs.push((id, ty, s.pos));
            }
        }
        for &(id, ty, pos) in &defs {
            let t = self.conv(ty, pos);
            if let Some(a) = self.aliases.get_mut(id as usize) {
                a.target = t;
            }
        }
        // Chains of aliases point straight at what they stand for; a cycle of plain
        // aliases stands for nothing.
        for &(id, _, pos) in &defs {
            let mut seen = vec![id];
            let mut t = self.aliases.get(id as usize).map_or(ANY, |a| a.target);
            loop {
                if !self.tick(1) {
                    return;
                }
                let Ty::Alias(j) = *self.ty(t) else { break };
                if seen.contains(&j) {
                    let name = self.aliases.get(id as usize).map(|a| a.name.clone()).unwrap_or_default();
                    self.diag(pos, pos, format!("type '{name}' is defined only in terms of itself"));
                    t = ANY;
                    break;
                }
                seen.push(j);
                t = self.aliases.get(j as usize).map_or(ANY, |a| a.target);
            }
            for j in seen {
                if let Some(a) = self.aliases.get_mut(j as usize) {
                    a.target = t;
                }
            }
        }
    }

    // ---- types: looking at them ----

    fn resolve(&self, t: T) -> T {
        match self.ty(t) {
            Ty::Alias(i) => self.aliases.get(*i as usize).map_or(ANY, |a| a.target),
            _ => t,
        }
    }

    /// The members of a type: unions and aliases taken apart.
    fn flatten(&mut self, t: T) -> Vec<T> {
        let mut out = Vec::new();
        let mut seen: HashSet<T> = HashSet::<_, crate::nostd::collections::hash_map::RandomState>::default();
        let mut stack = vec![t];
        while let Some(x) = stack.pop() {
            if !self.tick(1) {
                return vec![ANY];
            }
            match self.ty(x) {
                Ty::Alias(_) => {
                    if seen.insert(x) {
                        stack.push(self.resolve(x));
                    }
                }
                Ty::Union(m) => {
                    if seen.insert(x) {
                        stack.extend(m.iter().rev());
                    }
                }
                _ => {
                    if seen.insert(x) {
                        out.push(x);
                    }
                }
            }
        }
        out
    }

    fn kinds(&mut self, t: T) -> u16 {
        if let Some(&k) = self.kinds_of.get(&t) {
            return k;
        }
        let mut k = 0;
        for m in self.flatten(t) {
            k |= match self.ty(m) {
                Ty::Any | Ty::Alias(_) | Ty::Union(_) => K_ANY,
                Ty::Nil => K_NIL,
                Ty::Bool | Ty::True | Ty::False => K_BOOL,
                Ty::Num => K_NUM,
                Ty::Int => K_INT,
                Ty::Str => K_STR,
                Ty::Lit(b) => {
                    if crate::number::str_to_number(b).is_some() {
                        K_STR
                    } else {
                        K_STR | K_BADSTR
                    }
                }
                Ty::Func | Ty::Sig(_) => K_FUNC,
                Ty::Table | Ty::Array(_) | Ty::Map(..) | Ty::Shape(_) | Ty::Ctor(_) => K_TABLE,
                Ty::Udata => K_UDATA,
                Ty::Thread => K_THREAD,
            };
        }
        self.kinds_of.insert(t, k);
        k
    }

    /// The type without `nil` and `false`.
    fn truthy(&mut self, t: T) -> T {
        if let Some(&r) = self.truthy_of.get(&t) {
            return r;
        }
        let k = self.kinds(t);
        let r = if k & (K_NIL | K_BOOL) == 0 || k & K_ANY != 0 {
            t
        } else {
            let all = self.flatten(t);
            let m: Vec<T> = all.iter().filter(|&&x| x != NIL && x != FALSE).map(|&x| if x == BOOL { TRUE } else { x }).collect();
            if m.is_empty() || m == all { t } else { self.union_of(m) }
        };
        self.truthy_of.insert(t, r);
        r
    }

    /// The type's members that can be false or nil.
    fn falsy(&mut self, t: T) -> T {
        if let Some(&r) = self.falsy_of.get(&t) {
            return r;
        }
        let k = self.kinds(t);
        let r = if k & K_ANY != 0 {
            t
        } else {
            let all = self.flatten(t);
            let m: Vec<T> = all.iter().filter(|&&x| x == NIL || x == FALSE || x == BOOL).map(|&x| if x == BOOL { FALSE } else { x }).collect();
            if m.is_empty() || m == all { t } else { self.union_of(m) }
        };
        self.falsy_of.insert(t, r);
        r
    }

    fn field_of(&self, t: T, name: &str) -> Option<T> {
        match self.ty(t) {
            Ty::Shape(fs) => fs.binary_search_by(|(n, _)| (**n).cmp(name)).ok().and_then(|i| fs.get(i)).map(|(_, t)| *t),
            _ => None,
        }
    }

    /// The single function signature a callee's type has (after removing nil).
    fn sig_parts(&self, t: T) -> Option<SigParts> {
        match self.ty(t) {
            Ty::Sig(s) => Some((s.params.clone(), s.va, s.ret.clone())),
            _ => None,
        }
    }

    // ---- subtyping ----

    /// Whether every value of `a` is a value of `b` (with `any` fitting both ways).
    fn sub(&mut self, a: T, b: T) -> bool {
        // The first pass reports nothing, so it decides nothing.
        if a == b || a == ANY || b == ANY || self.pass == 1 {
            return true;
        }
        // Each frame is a goal being decided by all (or any) of its subgoals. A goal about
        // an alias is assumed true while it is being decided (types are recursive: the
        // relation is the greatest one). Answers are remembered: a failure always (an
        // assumption only makes more true), a success when it used no assumption made
        // further out (`low`, the outermost frame whose assumption it used).
        struct Frame {
            pair: (T, T),
            all: bool,
            goals: Vec<(T, T)>,
            i: usize,
            assumed: bool,
            low: usize,
        }
        let mut stack: Vec<Frame> = Vec::new();
        let mut next = Some((a, b));
        let mut pending: Option<bool> = None;
        loop {
            if let Some((x, y)) = next.take() {
                if !self.tick(1) {
                    for f in stack {
                        if f.assumed {
                            self.assume.remove(&f.pair);
                        }
                    }
                    return true;
                }
                if let Some(&r) = self.memo.get(&(x, y)) {
                    pending = Some(r);
                } else if let Some(&d) = self.assume.get(&(x, y)) {
                    if let Some(top) = stack.last_mut() {
                        top.low = top.low.min(d as usize);
                    }
                    pending = Some(true);
                } else {
                    match self.expand(x, y) {
                        Ok(r) => pending = Some(r),
                        Err((all, goals)) => {
                            let depth = stack.len();
                            let assumed = matches!(self.ty(x), Ty::Alias(_)) || matches!(self.ty(y), Ty::Alias(_));
                            if assumed {
                                self.assume.insert((x, y), u32::try_from(depth).unwrap_or(u32::MAX));
                            }
                            let _ = self.tick(goals.len() as u64 / 4);
                            stack.push(Frame { pair: (x, y), all, goals, i: 0, assumed, low: usize::MAX });
                            pending = None;
                        }
                    }
                }
            }
            let Some(top) = stack.last_mut() else { return pending.unwrap_or(true) };
            let decided = match pending.take() {
                Some(r) if r != top.all => Some(r),
                _ => {
                    if let Some(&g) = top.goals.get(top.i) {
                        top.i += 1;
                        next = Some(g);
                        None
                    } else {
                        Some(top.all)
                    }
                }
            };
            if let Some(r) = decided {
                let depth = stack.len() - 1;
                let Some(f) = stack.pop() else { continue };
                if f.assumed {
                    self.assume.remove(&f.pair);
                }
                if !r || f.low >= depth {
                    self.memo.insert(f.pair, r);
                }
                if f.low < depth
                    && let Some(parent) = stack.last_mut()
                {
                    parent.low = parent.low.min(f.low);
                }
                pending = Some(r);
            }
        }
    }

    /// One step of `sub`: the answer, or the goals that decide it (all of them, or any).
    fn expand(&mut self, a0: T, b0: T) -> Goals {
        let (a, b) = (self.resolve(a0), self.resolve(b0));
        if a == b || a == ANY || b == ANY {
            return Ok(true);
        }
        if let Ty::Union(m) = self.ty(a) {
            return Err((true, m.iter().map(|&x| (x, b)).collect()));
        }
        if let Ty::Union(m) = self.ty(b) {
            if a == BOOL {
                return Err((true, vec![(TRUE, b), (FALSE, b)]));
            }
            return Err((false, m.iter().map(|&y| (a, y)).collect()));
        }
        let tabular = |t: &Ty| matches!(t, Ty::Table | Ty::Array(_) | Ty::Map(..) | Ty::Shape(_) | Ty::Ctor(_));
        let r = match (self.ty(a), self.ty(b)) {
            (Ty::Int, Ty::Num) | (Ty::Lit(_), Ty::Str) | (Ty::True | Ty::False, Ty::Bool) => true,
            (Ty::Lit(x), Ty::Lit(y)) => x == y,
            (Ty::Func | Ty::Sig(_), Ty::Func) | (Ty::Func, Ty::Sig(_)) => true,
            (x, Ty::Table) if tabular(x) => true,
            (Ty::Table, y) if tabular(y) => true,
            (Ty::Array(x), Ty::Array(y)) => return Err((true, vec![(*x, *y), (*y, *x)])),
            (Ty::Map(k1, v1), Ty::Map(k2, v2)) => return Err((true, vec![(*k1, *k2), (*k2, *k1), (*v1, *v2), (*v2, *v1)])),
            (Ty::Array(x), Ty::Map(k, v)) | (Ty::Map(k, v), Ty::Array(x)) => {
                return Err((true, vec![(*k, NUM), (INT, *k), (*x, *v), (*v, *x)]));
            }
            (Ty::Shape(fs), Ty::Shape(gs)) => {
                let mut goals = Vec::with_capacity(gs.len() * 2);
                for (n, g) in gs.iter() {
                    match fs.binary_search_by(|(m, _)| m.cmp(n)).ok().and_then(|i| fs.get(i)) {
                        Some((_, f)) => {
                            goals.push((*f, *g));
                            goals.push((*g, *f));
                        }
                        None => return Ok(false),
                    }
                }
                let _ = self.tick(gs.len() as u64);
                return Err((true, goals));
            }
            (Ty::Sig(_), Ty::Sig(_)) => return self.sig_goals(a, b),
            (Ty::Ctor(_), _) => return self.ctor_goals(a, b),
            _ => false,
        };
        Ok(r)
    }

    /// The goals for signature `a` to fit signature `b`: `b`'s arguments fit `a`'s
    /// parameters, `a`'s results fit `b`'s.
    fn sig_goals(&mut self, a: T, b: T) -> Goals {
        let (Some((ap, ava, aret)), Some((bp, bva, bret))) = (self.sig_parts(a), self.sig_parts(b)) else {
            return Ok(false);
        };
        let _ = self.tick((ap.len() + bp.len()) as u64);
        let mut goals = Vec::new();
        for (i, &p) in ap.iter().enumerate() {
            let arg = match (bp.get(i), bva) {
                (Some(&x), _) => x,
                (None, Some(v)) => self.opt(v),
                (None, None) => NIL,
            };
            goals.push((arg, p));
        }
        if let (Some(v), Some(w)) = (bva, ava) {
            goals.push((v, w));
        }
        // (Without varargs, arguments the function does not take are dropped: fine.)
        if let Some(w) = ava {
            for &x in bp.iter().skip(ap.len()) {
                goals.push((x, w));
            }
        }
        if let (Some(ar), Some(br)) = (aret, bret) {
            for (i, &r) in br.iter().enumerate() {
                goals.push((ar.get(i).copied().unwrap_or(NIL), r));
            }
        }
        Err((true, goals))
    }

    /// The goals for a constructor's type `a` to fit `b`.
    fn ctor_goals(&mut self, a: T, b: T) -> Goals {
        let Ty::Ctor(c) = self.ty(a) else { return Ok(false) };
        let named: Vec<(Box<str>, T)> = c.named.to_vec();
        let items: Vec<T> = c.items.to_vec();
        let keyed: Vec<(T, T)> = c.keyed.to_vec();
        let tail = c.tail.clone();
        let _ = self.tick((named.len() + items.len() + keyed.len()) as u64);
        let tail_types: Vec<T> = match &tail {
            Multi::One => Vec::new(),
            Multi::List(l) => l.to_vec(),
            Multi::Rep(t) => vec![*t],
        };
        let mut goals = Vec::new();
        match self.ty(b) {
            Ty::Shape(gs) => {
                let gs: Vec<(Box<str>, T)> = gs.to_vec();
                if !items.is_empty() || !tail_types.is_empty() {
                    return Ok(false);
                }
                for (n, v) in &named {
                    match gs.binary_search_by(|(m, _)| m.cmp(n)).ok().and_then(|i| gs.get(i)) {
                        Some((_, g)) => goals.push((*v, *g)),
                        None => return Ok(false),
                    }
                }
                for (n, g) in &gs {
                    if named.binary_search_by(|(m, _)| m.cmp(n)).is_err() {
                        goals.push((NIL, *g));
                    }
                }
                for (k, v) in &keyed {
                    match self.ty(*k) {
                        Ty::Any => {}
                        Ty::Lit(s) => {
                            let s = String::from_utf8_lossy(s).into_owned();
                            match gs.binary_search_by(|(m, _)| (**m).cmp(s.as_str())).ok().and_then(|i| gs.get(i)) {
                                Some((_, g)) => goals.push((*v, *g)),
                                None => return Ok(false),
                            }
                        }
                        _ => return Ok(false),
                    }
                }
            }
            Ty::Array(t) => {
                let t = *t;
                if !named.is_empty() {
                    return Ok(false);
                }
                goals.extend(items.iter().chain(&tail_types).map(|&x| (x, t)));
                for (k, v) in &keyed {
                    goals.push((*k, NUM));
                    goals.push((*v, t));
                }
            }
            Ty::Map(k, v) => {
                let (k, v) = (*k, *v);
                for (n, x) in &named {
                    let l = self.lit(n.as_bytes());
                    goals.push((l, k));
                    goals.push((*x, v));
                }
                if !items.is_empty() || !tail_types.is_empty() {
                    goals.push((INT, k));
                }
                goals.extend(items.iter().chain(&tail_types).map(|&x| (x, v)));
                for (kk, vv) in &keyed {
                    goals.push((*kk, k));
                    goals.push((*vv, v));
                }
            }
            Ty::Table => return Ok(true),
            _ => return Ok(false),
        }
        Err((true, goals))
    }

    // ---- printing ----

    fn show(&mut self, t: T) -> String {
        enum P {
            T(T, bool),
            S(&'static str),
            O(String),
        }
        let mut out = String::new();
        let mut stack = vec![P::T(t, false)];
        while let Some(p) = stack.pop() {
            if !self.tick(1) || out.len() > SHOW_MAX {
                out.push_str("...");
                break;
            }
            let t = match p {
                P::S(s) => {
                    out.push_str(s);
                    continue;
                }
                P::O(s) => {
                    out.push_str(&s);
                    continue;
                }
                P::T(t, paren) => {
                    if paren && matches!(self.ty(t), Ty::Sig(_)) {
                        stack.push(P::S(")"));
                        stack.push(P::T(t, false));
                        stack.push(P::S("("));
                        continue;
                    }
                    t
                }
            };
            let mut parts: Vec<P> = Vec::new();
            match self.ty(t) {
                Ty::Any => out.push_str("any"),
                Ty::Nil => out.push_str("nil"),
                Ty::Bool => out.push_str("boolean"),
                Ty::True => out.push_str("true"),
                Ty::False => out.push_str("false"),
                Ty::Num => out.push_str("number"),
                Ty::Int => out.push_str("integer"),
                Ty::Str => out.push_str("string"),
                Ty::Func => out.push_str("function"),
                Ty::Table => out.push_str("table"),
                Ty::Udata => out.push_str("userdata"),
                Ty::Thread => out.push_str("thread"),
                Ty::Lit(b) => out.push_str(&format!("{:?}", short(&String::from_utf8_lossy(b)))),
                Ty::Alias(i) => out.push_str(self.aliases.get(*i as usize).map_or("?", |a| a.name.as_str())),
                Ty::Array(x) => parts = vec![P::S("{"), P::T(*x, false), P::S("}")],
                Ty::Map(k, v) => parts = vec![P::S("{["), P::T(*k, false), P::S("]: "), P::T(*v, false), P::S("}")],
                Ty::Shape(fs) => {
                    parts.push(P::S("{"));
                    for (i, (n, x)) in fs.iter().enumerate().take(16) {
                        if i > 0 {
                            parts.push(P::S(", "));
                        }
                        parts.push(P::O(format!("{n}: ")));
                        parts.push(P::T(*x, false));
                    }
                    if fs.len() > 16 {
                        parts.push(P::S(", ..."));
                    }
                    parts.push(P::S("}"));
                }
                Ty::Ctor(c) => {
                    parts.push(P::S("{"));
                    let mut first = true;
                    for (n, x) in c.named.iter().take(16) {
                        if !first {
                            parts.push(P::S(", "));
                        }
                        first = false;
                        parts.push(P::O(format!("{n}: ")));
                        parts.push(P::T(*x, false));
                    }
                    for x in c.items.iter().take(16) {
                        if !first {
                            parts.push(P::S(", "));
                        }
                        first = false;
                        parts.push(P::T(*x, false));
                    }
                    if !matches!(c.tail, Multi::One) || !c.keyed.is_empty() {
                        parts.push(P::S(if first { "..." } else { ", ..." }));
                    }
                    parts.push(P::S("}"));
                }
                Ty::Sig(s) => {
                    parts.push(P::S("("));
                    for (i, p) in s.params.iter().enumerate() {
                        if i > 0 {
                            parts.push(P::S(", "));
                        }
                        parts.push(P::T(*p, false));
                    }
                    if s.va.is_some() {
                        parts.push(P::S(if s.params.is_empty() { "..." } else { ", ..." }));
                    }
                    parts.push(P::S(") -> "));
                    match &s.ret {
                        None => parts.push(P::S("any...")),
                        Some(r) if r.len() == 1 => parts.extend(r.iter().map(|&x| P::T(x, true))),
                        Some(r) => {
                            parts.push(P::S("("));
                            for (i, x) in r.iter().enumerate() {
                                if i > 0 {
                                    parts.push(P::S(", "));
                                }
                                parts.push(P::T(*x, false));
                            }
                            parts.push(P::S(")"));
                        }
                    }
                }
                Ty::Union(m) => {
                    if let [x, n] = **m
                        && n == NIL
                        && !matches!(self.ty(x), Ty::Union(_))
                    {
                        parts = vec![P::T(x, true), P::S("?")];
                    } else {
                        for (i, x) in m.iter().enumerate() {
                            if i > 0 {
                                parts.push(P::S(" | "));
                            }
                            parts.push(P::T(*x, true));
                        }
                    }
                }
            }
            stack.extend(parts.into_iter().rev());
        }
        out
    }

    // ---- scopes and names ----

    fn lookup(&self, n: &str) -> Option<usize> {
        self.names.get(n).and_then(|v| v.last()).map(|&i| i as usize)
    }

    fn open_scope(&mut self) {
        self.scopes.push(Scope { locals: self.locals.len(), narrow: self.narrow.len(), aliases: self.alias_stack.len() });
    }

    fn close_scope(&mut self) {
        let Some(s) = self.scopes.pop() else { return };
        self.undo_narrow(s.narrow);
        // The innermost binding of `n` goes.
        fn unbind<'a>(m: &mut HashMap<&'a str, Vec<u32>>, n: &'a str) {
            if let Some(v) = m.get_mut(n) {
                v.pop();
                if v.is_empty() {
                    m.remove(n);
                }
            }
        }
        while self.locals.len() > s.locals {
            if let Some(l) = self.locals.pop() {
                unbind(&mut self.names, l.name);
            }
        }
        while self.alias_stack.len() > s.aliases {
            if let Some(n) = self.alias_stack.pop() {
                unbind(&mut self.alias_names, n);
            }
        }
    }

    fn undo_narrow(&mut self, to: usize) {
        while self.narrow.len() > to {
            if let Some((i, old)) = self.narrow.pop()
                && let Some(l) = self.locals.get_mut(i)
            {
                l.ty = old;
            }
        }
    }

    /// Declares a local: with its annotation (`decl`), or keeping `v`'s type when it is
    /// never reassigned and the type came from an annotation.
    fn declare(&mut self, name: &'a str, decl: Option<T>, v: &V) {
        let id = self.decls;
        self.decls = self.decls.saturating_add(1);
        if self.pass == 1 {
            self.assigned.push(false);
        }
        let reassigned = self.assigned.get(id as usize).copied().unwrap_or(true);
        let (ty, st) = match decl {
            Some(d) => (d, true),
            None if v.st && !reassigned && self.pass == 2 => (self.widen(v.ty), true),
            None => (ANY, false),
        };
        let idx = u32::try_from(self.locals.len()).unwrap_or(u32::MAX);
        let depth = u32::try_from(self.fns.len()).unwrap_or(u32::MAX);
        if self.pass == 1 {
            self.captured.push(false);
        }
        self.locals.push(Local { name, decl, ty, st, id, depth });
        self.names.entry(name).or_default().push(idx);
    }

    fn reassigned(&self, idx: usize) -> bool {
        self.locals.get(idx).and_then(|l| self.assigned.get(l.id as usize)).copied().unwrap_or(true)
    }

    fn name_value(&self, n: &str) -> V {
        match self.lookup(n).and_then(|i| self.locals.get(i)) {
            Some(l) => V::one(l.ty, l.st),
            None => V::one(ANY, false),
        }
    }

    // ---- the walk ----

    fn run(&mut self, block: &'a Block) {
        self.fns.push(FnCtx { ret: None, va: Some(V::dynamic()) });
        self.loops.push(u32::MAX);
        self.work.push(W::Block(block));
        while let Some(w) = self.work.pop() {
            if !self.tick(1) {
                break;
            }
            self.step(w);
        }
        debug_assert!(self.stopped.is_some() || self.vals.is_empty(), "the value stack is unbalanced");
    }

    fn open_block(&mut self, b: &'a Block) {
        self.open_scope();
        self.hoist(b);
        for s in b.stats.iter().rev() {
            self.work.push(W::Stat(s));
        }
    }

    fn pop_val(&mut self) -> V {
        self.vals.pop().unwrap_or_else(V::dynamic)
    }

    fn pop_vals(&mut self, n: usize) -> Vec<V> {
        let at = self.vals.len().saturating_sub(n);
        self.vals.split_off(at)
    }

    fn push_exprs(&mut self, es: &'a [Expr]) {
        for e in es.iter().rev() {
            self.work.push(W::Expr(e));
        }
    }

    fn step(&mut self, w: W<'a>) {
        match w {
            W::Block(b) => {
                self.work.push(W::EndScope);
                self.open_block(b);
            }
            W::EndScope => self.close_scope(),
            W::Stat(s) => self.stat(s),
            W::LocalEnd(s) => self.local_end(s),
            W::AssignEnd(s) => self.assign_end(s),
            W::OpAssignEnd(s) => self.op_assign_end(s),
            W::CallStatEnd(s) => self.call_stat_end(s),
            W::Discard => {
                self.pop_val();
            }
            W::LoopEnd => {
                if let Some(id) = self.loops.pop()
                    && self.pass == 1
                    && let Some(l) = self.loop_last.get_mut(id as usize)
                {
                    *l = self.loop_count.saturating_sub(1);
                }
            }
            W::IfStep(s, i) => self.if_step(s, i),
            W::IfAfter(s) => self.if_after(s),
            W::NarrowEnter(e, pol) => {
                self.nmarks.push(self.narrow.len());
                self.facts(e, pol);
            }
            W::NarrowExit => {
                let m = self.nmarks.pop().unwrap_or(0);
                self.undo_narrow(m);
            }
            W::NumForHead(s) => self.num_for_head(s),
            W::GenForHead(s) => self.gen_for_head(s),
            W::FuncStatEnd(t, at) => self.func_stat_end(t, at),
            W::ReturnEnd(s) => self.return_end(s),
            W::Expr(e) => self.expr(e),
            W::ExprEnd(e) => self.expr_end(e),
            W::AndOrEnd(e) => self.and_or_end(e),
            W::FuncLit(b, selfv, sig) => self.func_lit(b, selfv, sig),
            W::FuncEnd(b, sig, ann) => self.func_end(b, sig, ann),
        }
    }

    fn loop_start(&mut self, s: &'a Stat) {
        let id = self.loop_count;
        self.loop_count = self.loop_count.saturating_add(1);
        if self.pass == 1 {
            self.loop_break.push(false);
            self.loop_last.push(id);
        } else {
            let b = self.loop_break.get(id as usize).copied().unwrap_or(true);
            self.breaks_at.insert(addr(s), b);
            // What the loop body assigns may be what the next iteration sees.
            let last = self.loop_last.get(id as usize).copied().unwrap_or(u32::MAX);
            self.drop_narrowings(|c, i| c.assigned_in(i, id, last));
        }
        self.loops.push(id);
    }

    /// Whether local `i` is assigned anywhere inside loops `first..=last`.
    fn assigned_in(&self, i: usize, first: u32, last: u32) -> bool {
        let Some(l) = self.locals.get(i) else { return true };
        let at = self.assign_at.partition_point(|&p| p < (l.id, first));
        self.assign_at.get(at).is_some_and(|&(d, lp)| d == l.id && lp <= last)
    }

    /// Ends the narrowings of reassigned locals for which `which` holds: from here on
    /// (and in what encloses here) they have their declared type again.
    fn drop_narrowings(&mut self, which: impl Fn(&Self, usize) -> bool) {
        let _ = self.tick(self.narrow.len() as u64);
        let mut seen = HashSet::<_, crate::nostd::collections::hash_map::RandomState>::default();
        let ids: Vec<usize> = self.narrow.iter().map(|&(i, _)| i).filter(|&i| seen.insert(i)).collect();
        for i in ids {
            if self.reassigned(i) && which(self, i) {
                self.invalidate(i);
            }
        }
    }

    /// Local `i` (reassigned) has its declared type from here on, whatever narrowed it.
    fn invalidate(&mut self, i: usize) {
        let base = self.locals.get(i).and_then(|l| l.decl).unwrap_or(ANY);
        let _ = self.tick(self.narrow.len() as u64 / 8);
        for e in &mut self.narrow {
            if e.0 == i {
                e.1 = base;
            }
        }
        if let Some(l) = self.locals.get_mut(i) {
            l.ty = base;
        }
    }

    fn stat(&mut self, s: &'a Stat) {
        self.at = s.pos;
        match &s.kind {
            StatKind::Call(e) => {
                self.work.push(W::CallStatEnd(s));
                self.work.push(W::Expr(e));
            }
            StatKind::Local(_, es) => {
                self.work.push(W::LocalEnd(s));
                self.push_exprs(es);
            }
            StatKind::Assign(ts, es, _) => {
                self.work.push(W::AssignEnd(s));
                self.push_exprs(es);
                for t in ts.iter().rev() {
                    if let ExprKind::Index(o, k) = &t.kind {
                        self.work.push(W::Expr(k));
                        self.work.push(W::Expr(o));
                    }
                }
            }
            StatKind::OpAssign(b) => {
                let (t, _, e) = &**b;
                self.work.push(W::OpAssignEnd(s));
                self.work.push(W::Expr(e));
                if let ExprKind::Index(o, k) = &t.kind {
                    self.work.push(W::Expr(k));
                    self.work.push(W::Expr(o));
                }
            }
            StatKind::Do(b) => self.work.push(W::Block(b)),
            StatKind::While(c, b) => {
                // The condition is part of the loop: it runs again after the body.
                self.loop_start(s);
                self.work.push(W::LoopEnd);
                self.work.push(W::NarrowExit);
                self.work.push(W::Block(b));
                self.work.push(W::NarrowEnter(c, true));
                self.work.push(W::Discard);
                self.work.push(W::Expr(c));
            }
            StatKind::Repeat(b, c) => {
                self.loop_start(s);
                self.work.push(W::LoopEnd);
                self.work.push(W::EndScope);
                self.work.push(W::Discard);
                self.work.push(W::Expr(c));
                self.open_block(b);
            }
            StatKind::If(..) => {
                self.work.push(W::IfAfter(s));
                self.work.push(W::IfStep(s, 0));
            }
            StatKind::NumFor(f) => {
                let ForNum { start, limit, step, .. } = &**f;
                self.work.push(W::NumForHead(s));
                if let Some(st) = step {
                    self.work.push(W::Expr(st));
                }
                self.work.push(W::Expr(limit));
                self.work.push(W::Expr(start));
            }
            StatKind::GenFor { exprs, .. } => {
                self.work.push(W::GenForHead(s));
                self.push_exprs(exprs);
            }
            StatKind::Function { path, method, body } => self.func_stat(s, path, method.as_deref(), body),
            StatKind::LocalFunction(n, body) => {
                let (sig, ann) = self.sig_of(body, None);
                let v = V::one(sig, ann);
                self.declare(&n.name, ann.then_some(sig), &v);
                self.work.push(W::Discard);
                self.work.push(W::FuncLit(body, None, Some((sig, ann))));
            }
            StatKind::Return(es) => {
                self.work.push(W::ReturnEnd(s));
                self.push_exprs(es);
            }
            StatKind::Break => {
                if self.pass == 1
                    && let Some(&id) = self.loops.last()
                    && let Some(b) = self.loop_break.get_mut(id as usize)
                {
                    *b = true;
                }
            }
            // A `goto` may come here from where a reassigned local had another value.
            StatKind::Label(_) if self.pass == 2 => self.drop_narrowings(|_, _| true),
            StatKind::Continue | StatKind::Goto(..) | StatKind::Label(_) | StatKind::TypeDef(..) => {}
        }
    }

    fn spread(&mut self, vals: &[V]) -> Spread {
        let mut fixed = Vec::with_capacity(vals.len());
        let mut rep = None;
        let last = vals.len().saturating_sub(1);
        for (i, v) in vals.iter().enumerate() {
            match &v.multi {
                Multi::List(l) if i == last => fixed.extend(l.iter().map(|&t| (t, v.st))),
                Multi::Rep(t) if i == last => rep = Some((*t, v.st)),
                _ => fixed.push((v.ty, v.st)),
            }
        }
        let _ = self.tick(fixed.len() as u64);
        Spread { fixed, rep }
    }

    /// Value `i` of a spread list: its type and whether it came from an annotation.
    fn nth(&mut self, s: &Spread, i: usize) -> (T, bool) {
        if let Some(&v) = s.fixed.get(i) {
            return v;
        }
        match s.rep {
            Some((t, st)) => (self.opt(t), st),
            None => (NIL, false),
        }
    }

    fn local_end(&mut self, s: &'a Stat) {
        let StatKind::Local(names, es) = &s.kind else { return };
        let vals = self.pop_vals(es.len());
        let sp = self.spread(&vals);
        for (i, n) in names.iter().enumerate() {
            let (t, st) = self.nth(&sp, i);
            let decl = n.ty.as_ref().map(|ty| self.conv(ty, n.pos));
            if let Some(d) = decl {
                let at = match es.get(i).or(es.last()) {
                    Some(e) => self.range(e),
                    None => (n.pos, name_end(n.pos, &n.name)),
                };
                let name = n.name.as_str();
                self.fit(t, d, &|| format!("local '{name}'"), at);
            }
            self.declare(&n.name, decl, &V::one(t, st));
        }
    }

    /// Assigns a value of type `t` to `target` (a name or a field).
    fn assign(&mut self, target: &'a Expr, obj: Option<(V, V)>, t: T, at: (Pos, Pos)) {
        match (&target.kind, obj) {
            (ExprKind::Name(n), _) => {
                if let Some(i) = self.lookup(n) {
                    self.assign_local(i, t, at);
                }
            }
            (ExprKind::Index(o, k), Some((ov, kv))) => self.index_write(&ov, Key::Val(&kv, k), t, o, target, at),
            _ => {}
        }
    }

    fn assign_local(&mut self, i: usize, t: T, at: (Pos, Pos)) {
        let Some(l) = self.locals.get(i) else { return };
        let (id, decl, name, depth) = (l.id, l.decl, l.name, l.depth);
        if self.pass == 1 {
            if let Some(a) = self.assigned.get_mut(id as usize) {
                *a = true;
            }
            if self.fns.len() > depth as usize
                && let Some(c) = self.captured.get_mut(id as usize)
            {
                *c = true;
            }
            let lp = self.loops.last().copied().unwrap_or(u32::MAX);
            self.assign_at.push((id, lp));
            return;
        }
        let Some(d) = decl else { return };
        let fits = self.fit(t, d, &|| format!("local '{name}'"), at);
        if self.captured.get(id as usize).copied().unwrap_or(true) {
            return;
        }
        // From here on it holds this value: not nil if the value is not.
        self.invalidate(i);
        if fits && self.kinds(t) & (K_NIL | K_ANY) == 0 {
            let r = self.nonnil(d);
            self.set_narrow(i, r);
        }
    }

    /// The type without `nil`.
    fn nonnil(&mut self, t: T) -> T {
        if self.kinds(t) & K_NIL == 0 {
            return t;
        }
        let m: Vec<T> = self.flatten(t).into_iter().filter(|&x| x != NIL).collect();
        if m.is_empty() { t } else { self.union_of(m) }
    }

    fn assign_end(&mut self, s: &'a Stat) {
        let StatKind::Assign(ts, es, _) = &s.kind else { return };
        let vals = self.pop_vals(es.len());
        let nparts = ts.iter().filter(|t| matches!(t.kind, ExprKind::Index(..))).count() * 2;
        let mut parts = self.pop_vals(nparts).into_iter();
        let sp = self.spread(&vals);
        for (i, t) in ts.iter().enumerate() {
            let obj = if matches!(t.kind, ExprKind::Index(..)) {
                match (parts.next(), parts.next()) {
                    (Some(o), Some(k)) => Some((o, k)),
                    _ => None,
                }
            } else {
                None
            };
            let (ty, _) = self.nth(&sp, i);
            let at = match es.get(i).or(es.last()) {
                Some(e) => self.range(e),
                None => (s.pos, s.pos),
            };
            self.assign(t, obj, ty, at);
        }
    }

    fn op_assign_end(&mut self, s: &'a Stat) {
        let StatKind::OpAssign(b) = &s.kind else { return };
        let (t, op, e) = &**b;
        let ev = self.pop_val();
        let start = self.start(t);
        let at = (start, e.end);
        match &t.kind {
            ExprKind::Index(o, k) => {
                let kv = self.pop_val();
                let ov = self.pop_val();
                let cur = self.index_read(&ov, Key::Val(&kv, k), o, t);
                let r = self.binop(*op, &cur, &ev, t, e, t);
                self.index_write(&ov, Key::Val(&kv, k), r.ty, o, t, at);
            }
            ExprKind::Name(n) => {
                let cur = self.name_value(n);
                let r = self.binop(*op, &cur, &ev, t, e, t);
                if let Some(i) = self.lookup(n) {
                    self.assign_local(i, r.ty, at);
                }
            }
            _ => {}
        }
    }

    fn call_stat_end(&mut self, s: &'a Stat) {
        self.pop_val();
        let StatKind::Call(e) = &s.kind else { return };
        if let ExprKind::Call(f, args) = &e.kind
            && let ExprKind::Name(n) = &f.kind
            && self.lookup(n).is_none()
        {
            match n.as_str() {
                "error" => {
                    self.error_calls.insert(addr(s));
                }
                // What `assert` returns from is true from here to the end of the block.
                "assert" => {
                    if let Some(a) = args.first() {
                        self.facts(a, true);
                    }
                }
                _ => {}
            }
        }
    }

    fn if_step(&mut self, s: &'a Stat, i: usize) {
        let StatKind::If(conds, els) = &s.kind else { return };
        match conds.get(i) {
            Some((c, b)) => {
                self.work.push(W::NarrowExit);
                self.work.push(W::IfStep(s, i + 1));
                self.work.push(W::NarrowEnter(c, false));
                self.work.push(W::NarrowExit);
                self.work.push(W::Block(b));
                self.work.push(W::NarrowEnter(c, true));
                self.work.push(W::Discard);
                self.work.push(W::Expr(c));
            }
            None => {
                if let Some(b) = els {
                    self.work.push(W::Block(b));
                }
            }
        }
    }

    fn if_after(&mut self, s: &'a Stat) {
        let StatKind::If(conds, els) = &s.kind else { return };
        let mut all = els.is_some();
        let mut first_exits = false;
        for (i, (_, b)) in conds.iter().enumerate() {
            let x = self.exits(b);
            if i == 0 {
                first_exits = x;
            }
            all &= x;
        }
        let else_exits = els.as_ref().is_some_and(|b| self.exits(b));
        all &= else_exits;
        self.if_exits.insert(addr(s), all);
        // An early exit narrows the rest of the block.
        if let [(c, _)] = conds.as_slice() {
            if first_exits && !else_exits {
                self.facts(c, false);
            } else if else_exits && !first_exits {
                self.facts(c, true);
            }
        }
    }

    /// Whether control never reaches the end of `b` by falling through it (it returns,
    /// breaks, continues, calls the global `error`, or loops forever).
    fn exits(&mut self, b: &'a Block) -> bool {
        let mut todo = vec![b];
        while let Some(b) = todo.pop() {
            if !self.tick(1) {
                return true;
            }
            let Some(last) = b.stats.last() else { return false };
            match &last.kind {
                StatKind::Return(_) | StatKind::Break | StatKind::Continue => {}
                StatKind::Call(_) if self.error_calls.contains(&addr(last)) => {}
                StatKind::Do(inner) => todo.push(inner),
                StatKind::If(conds, Some(els)) => match self.if_exits.get(&addr(last)) {
                    Some(true) => {}
                    Some(false) => return false,
                    None => {
                        todo.extend(conds.iter().map(|(_, b)| b));
                        todo.push(els);
                    }
                },
                StatKind::While(c, _) if truthy_const(c) && !self.breaks_at.get(&addr(last)).copied().unwrap_or(true) => {}
                StatKind::Repeat(_, c) if falsy_const(c) && !self.breaks_at.get(&addr(last)).copied().unwrap_or(true) => {}
                _ => return false,
            }
        }
        true
    }

    /// Narrows the never-reassigned locals that `e` being true (`pol`) or false tells about.
    fn facts(&mut self, e: &'a Expr, pol: bool) {
        if self.pass != 2 {
            return;
        }
        // Breadth first: the cap keeps the parts near the top, however the chain leans.
        let mut queue = crate::nostd::collections::VecDeque::from([(e, pol)]);
        let mut n = 0;
        while let Some((e, pol)) = queue.pop_front() {
            n += 1;
            if n > FACT_NODES || !self.tick(1) {
                return;
            }
            match &e.kind {
                ExprKind::Paren(x) => queue.push_back((x, pol)),
                ExprKind::Un(UnOp::Not, x) => queue.push_back((x, !pol)),
                ExprKind::Bin(BinOp::And, a, b) if pol => {
                    queue.push_back((a, true));
                    queue.push_back((b, true));
                }
                ExprKind::Bin(BinOp::Or, a, b) if !pol => {
                    queue.push_back((a, false));
                    queue.push_back((b, false));
                }
                ExprKind::Name(_) => {
                    if let Some(i) = self.narrowable(e) {
                        let t = self.locals.get(i).map_or(ANY, |l| l.ty);
                        let r = if pol { self.truthy(t) } else { self.falsy(t) };
                        self.set_narrow(i, r);
                    }
                }
                ExprKind::Bin(op @ (BinOp::Eq | BinOp::Ne), a, b) => {
                    let x = match (&a.kind, &b.kind) {
                        (ExprKind::Nil, _) => b,
                        (_, ExprKind::Nil) => a,
                        _ => continue,
                    };
                    if let Some(i) = self.narrowable(x) {
                        let t = self.locals.get(i).map_or(ANY, |l| l.ty);
                        let is_nil = (*op == BinOp::Eq) == pol;
                        let r = if is_nil {
                            if self.kinds(t) & (K_NIL | K_ANY) != 0 { NIL } else { t }
                        } else {
                            self.truthy(t)
                        };
                        self.set_narrow(i, r);
                    }
                }
                _ => {}
            }
        }
    }

    /// The local `e` names, if it can be narrowed (it is never reassigned).
    fn narrowable(&self, e: &Expr) -> Option<usize> {
        let ExprKind::Name(n) = &e.kind else { return None };
        let i = self.lookup(n)?;
        let id = self.locals.get(i)?.id as usize;
        // A local assigned from a nested function can change at any call.
        (!self.reassigned(i) || !self.captured.get(id).copied().unwrap_or(true)).then_some(i)
    }

    fn set_narrow(&mut self, i: usize, t: T) {
        if let Some(l) = self.locals.get_mut(i)
            && l.ty != t
        {
            self.narrow.push((i, l.ty));
            l.ty = t;
        }
    }

    fn num_for_head(&mut self, s: &'a Stat) {
        let StatKind::NumFor(f) = &s.kind else { return };
        let ForNum { var, start, limit, step, body, .. } = &**f;
        let stepv = step.as_ref().map(|_| self.pop_val());
        let limitv = self.pop_val();
        let startv = self.pop_val();
        let mut ints = true;
        let parts = [(Some(&startv), Some(start), "initial"), (Some(&limitv), Some(limit), "limit"), (stepv.as_ref(), step.as_ref(), "step")];
        for (v, e, what) in parts {
            let (Some(v), Some(e)) = (v, e) else { continue };
            let k = self.kinds(v.ty);
            if what != "limit" && k & !K_INT != 0 {
                ints = false;
            }
            if v.st && self.pass == 2 && k & K_ANY == 0 && k & !K_NUM != 0 {
                let msg = if k & K_NUM == 0 {
                    format!("'for' {what} value must be a number")
                } else {
                    let shown = self.show(v.ty);
                    format!("'for' {what} value must be a number (it may be {}: its type is {shown})", kind_name(k & !K_NUM))
                };
                self.diag_at(e, msg);
            }
        }
        let vt = if ints { INT } else { NUM };
        let decl = var.ty.as_ref().map(|ty| self.conv(ty, var.pos));
        if let Some(d) = decl {
            let name = var.name.as_str();
            let at = (var.pos, name_end(var.pos, name));
            self.fit(vt, d, &|| format!("loop variable '{name}'"), at);
        }
        self.open_scope();
        self.declare(&var.name, decl, &V::one(vt, startv.st));
        self.loop_start(s);
        self.work.push(W::EndScope);
        self.work.push(W::LoopEnd);
        self.work.push(W::Block(body));
    }

    fn gen_for_head(&mut self, s: &'a Stat) {
        let StatKind::GenFor { names, exprs, body, .. } = &s.kind else { return };
        let vals = self.pop_vals(exprs.len());
        let f = vals.first().cloned().unwrap_or_else(V::dynamic);
        let iter = self.truthy(f.ty);
        let rets = self.sig_parts(self.resolve(iter)).and_then(|(_, _, r)| r);
        self.open_scope();
        for (i, n) in names.iter().enumerate() {
            let (t, st) = match &rets {
                Some(r) => {
                    let t = r.get(i).copied().unwrap_or(NIL);
                    (if i == 0 { self.truthy(t) } else { t }, f.st)
                }
                None => (ANY, false),
            };
            let decl = n.ty.as_ref().map(|ty| self.conv(ty, n.pos));
            if let Some(d) = decl {
                let name = n.name.as_str();
                let at = (n.pos, name_end(n.pos, name));
                self.fit(t, d, &|| format!("loop variable '{name}'"), at);
            }
            self.declare(&n.name, decl, &V::one(t, st));
        }
        self.loop_start(s);
        self.work.push(W::EndScope);
        self.work.push(W::LoopEnd);
        self.work.push(W::Block(body));
    }

    fn func_stat(&mut self, s: &'a Stat, path: &'a [String], method: Option<&'a str>, body: &'a FuncBody) {
        let Some(first) = path.first() else { return };
        // The name path, as written after `function`.
        let len: usize = path.iter().map(|p| p.len() + 1).sum::<usize>() + method.map_or(0, |m| m.len() + 1);
        let npos = Pos { line: s.pos.line, col: s.pos.col.saturating_add(9) };
        let at = (npos, Pos { line: npos.line, col: npos.col.saturating_add(u32::try_from(len.saturating_sub(1)).unwrap_or(0)) });
        let mut obj = self.name_value(first);
        let walk = if method.is_some() { path.len() } else { path.len().saturating_sub(1) };
        for seg in path.iter().take(walk).skip(1) {
            if obj.st && self.pass == 2 {
                let k = self.kinds(obj.ty);
                if k & K_ANY == 0 && k & Op::Index.bad() != 0 {
                    let shown = self.show(obj.ty);
                    self.diag(at.0, at.1, format!("attempt to index a value that may be {} (its type is {shown})", kind_name(k & Op::Index.bad())));
                }
            }
            obj = self.index_name(&obj, seg, Some(at));
        }
        let (target, selfv) = match (method, path.len()) {
            (Some(m), _) => (Target::Field(obj.clone(), m), Some(obj)),
            (None, 1) => match self.lookup(first) {
                Some(i) => (Target::Local(i), None),
                None => (Target::Global, None),
            },
            (None, _) => (Target::Field(obj, path.last().map_or("", String::as_str)), None),
        };
        let (sig, ann) = self.sig_of(body, selfv.as_ref().map(|v| v.ty));
        self.work.push(W::FuncStatEnd(target, at));
        self.work.push(W::FuncLit(body, selfv, Some((sig, ann))));
    }

    fn func_stat_end(&mut self, t: Target<'a>, at: (Pos, Pos)) {
        let f = self.pop_val();
        match t {
            Target::Local(i) => self.assign_local(i, f.ty, at),
            Target::Global => {}
            Target::Field(obj, name) => self.write_field(&obj, name, f.ty, at),
        }
    }

    fn return_end(&mut self, s: &'a Stat) {
        let StatKind::Return(es) = &s.kind else { return };
        let vals = self.pop_vals(es.len());
        let Some(rs) = self.fns.last().and_then(|f| f.ret.clone()) else { return };
        if self.pass != 2 {
            return;
        }
        let sp = self.spread(&vals);
        let whole = match (es.first(), es.last()) {
            (Some(a), Some(b)) => (self.start(a), b.end),
            _ => (s.pos, Pos { line: s.pos.line, col: s.pos.col.saturating_add(6) }),
        };
        if sp.fixed.len() > rs.len() {
            let at = match es.get(rs.len()) {
                Some(e) => self.range(e),
                None => whole,
            };
            self.diag(at.0, at.1, format!("too many return values: expected {}, got {}", rs.len(), sp.fixed.len()));
        }
        for (i, &r) in rs.iter().enumerate() {
            let (t, _) = self.nth(&sp, i);
            let at = match es.get(i) {
                Some(e) => self.range(e),
                None => whole,
            };
            if i >= sp.fixed.len() && sp.rep.is_none() {
                if !self.sub(NIL, r) {
                    let shown = self.show(r);
                    self.diag(at.0, at.1, format!("missing return value #{} (expected {shown})", i + 1));
                }
                continue;
            }
            let n = i + 1;
            self.fit(t, r, &|| format!("return value #{n}"), at);
        }
    }

    // ---- functions ----

    /// A function's type from its annotations (`self_ty` for a method's `self`), and
    /// whether it has any.
    fn sig_of(&mut self, b: &'a FuncBody, self_ty: Option<T>) -> (T, bool) {
        let ann = b.params.iter().any(|p| p.ty.is_some()) || b.vararg_ty.is_some() || b.ret.is_some();
        if !ann {
            return (FUNC, false);
        }
        let mut params = Vec::with_capacity(b.params.len() + 1);
        if b.is_method {
            params.push(self_ty.unwrap_or(ANY));
        }
        for p in &b.params {
            let t = match &p.ty {
                Some(ty) => self.conv(ty, p.pos),
                None => ANY,
            };
            params.push(t);
        }
        let va = if b.vararg {
            Some(match &b.vararg_ty {
                Some(ty) => self.conv(ty, b.pos),
                None => ANY,
            })
        } else {
            None
        };
        let ret = b.ret.as_ref().map(|rs| {
            let v: Vec<T> = rs.iter().map(|t| self.conv(t, b.pos)).collect();
            Rc::from(v)
        });
        (self.add(Ty::Sig(Box::new(Sig { params: params.into(), va, ret }))), true)
    }

    fn func_lit(&mut self, b: &'a FuncBody, selfv: Option<V>, sig: Option<(T, bool)>) {
        let (sig, ann) = match sig {
            Some(s) => s,
            None => self.sig_of(b, selfv.as_ref().map(|v| v.ty)),
        };
        let (params, va, ret) = self.sig_parts(sig).unwrap_or_else(|| (Box::new([]), b.vararg.then_some(ANY), None));
        let va = if b.vararg {
            let t = va.unwrap_or(ANY);
            Some(V { ty: self.opt(t), st: b.vararg_ty.is_some(), multi: Multi::Rep(t) })
        } else {
            None
        };
        self.fns.push(FnCtx { ret, va });
        self.loops.push(u32::MAX);
        self.open_scope();
        // The function may run after a reassigned local it sees has changed: none of
        // their narrowings hold inside it.
        if self.pass == 2 && !self.narrow.is_empty() {
            let _ = self.tick(self.narrow.len() as u64);
            let mut seen = HashSet::<_, crate::nostd::collections::hash_map::RandomState>::default();
            let ids: Vec<usize> = self.narrow.iter().map(|&(i, _)| i).filter(|&i| seen.insert(i)).collect();
            for i in ids {
                if self.reassigned(i) {
                    let base = self.locals.get(i).and_then(|l| l.decl).unwrap_or(ANY);
                    self.set_narrow(i, base);
                }
            }
        }
        let mut ps = params.iter().copied();
        if b.is_method {
            let sv = selfv.unwrap_or_else(|| V::one(ANY, false));
            let _ = ps.next();
            self.declare("self", None, &sv);
        }
        for p in &b.params {
            let t = ps.next().unwrap_or(ANY);
            let decl = p.ty.is_some().then_some(t);
            self.declare(&p.name, decl, &V::one(ANY, false));
        }
        self.work.push(W::FuncEnd(b, sig, ann));
        self.work.push(W::EndScope);
        self.work.push(W::Block(&b.body));
    }

    fn func_end(&mut self, b: &'a FuncBody, sig: T, ann: bool) {
        let ctx = self.fns.pop();
        self.loops.pop();
        if self.pass == 2
            && let Some(rs) = ctx.and_then(|c| c.ret)
        {
            if rs.iter().any(|&r| !self.sub(NIL, r)) && !self.exits(&b.body) {
                let shown = match rs.len() {
                    1 => rs.first().map(|&r| self.show(r)).unwrap_or_default(),
                    _ => {
                        let v: Vec<String> = rs.iter().map(|&r| self.show(r)).collect();
                        format!("({})", v.join(", "))
                    }
                };
                let end = b.body.end;
                self.diag(end, Pos { line: end.line, col: end.col.saturating_add(3) }, format!("function may end without returning {shown}"));
            }
        }
        self.vals.push(V::one(sig, ann));
    }

    // ---- expressions ----

    fn expr(&mut self, e: &'a Expr) {
        let v = match &e.kind {
            ExprKind::Nil => V::one(NIL, false),
            ExprKind::True => V::one(TRUE, false),
            ExprKind::False => V::one(FALSE, false),
            ExprKind::Int(_) => V::one(INT, false),
            ExprKind::Num(_) => V::one(NUM, false),
            ExprKind::Str(s) => V::one(self.lit(s), false),
            ExprKind::Dots => self.fns.last().and_then(|f| f.va.clone()).unwrap_or_else(V::dynamic),
            ExprKind::Name(n) => self.name_value(n),
            ExprKind::Func(b) => {
                self.work.push(W::FuncLit(b, None, None));
                return;
            }
            ExprKind::Interp(parts) => {
                self.work.push(W::ExprEnd(e));
                for p in parts.iter().rev() {
                    if let InterpExpr::Expr(x) = p {
                        self.work.push(W::Expr(x));
                    }
                }
                return;
            }
            ExprKind::Table(fields) => {
                self.work.push(W::ExprEnd(e));
                for f in fields.iter().rev() {
                    match f {
                        Field::Pos(v) | Field::Named(_, v, _) => self.work.push(W::Expr(v)),
                        Field::Keyed(k, v) => {
                            self.work.push(W::Expr(v));
                            self.work.push(W::Expr(k));
                        }
                    }
                }
                return;
            }
            ExprKind::Bin(op @ (BinOp::And | BinOp::Or), a, b) => {
                self.work.push(W::AndOrEnd(e));
                self.work.push(W::NarrowExit);
                self.work.push(W::Expr(b));
                self.work.push(W::NarrowEnter(a, *op == BinOp::And));
                self.work.push(W::Expr(a));
                return;
            }
            ExprKind::Bin(_, a, b) | ExprKind::Index(a, b) => {
                self.work.push(W::ExprEnd(e));
                self.work.push(W::Expr(b));
                self.work.push(W::Expr(a));
                return;
            }
            ExprKind::Concat(v) => {
                self.work.push(W::ExprEnd(e));
                self.push_exprs(v);
                return;
            }
            ExprKind::Un(_, x) | ExprKind::Paren(x) => {
                self.work.push(W::ExprEnd(e));
                self.work.push(W::Expr(x));
                return;
            }
            ExprKind::Call(f, args) | ExprKind::Method(f, _, _, args) => {
                self.work.push(W::ExprEnd(e));
                self.push_exprs(args);
                self.work.push(W::Expr(f));
                return;
            }
        };
        self.vals.push(v);
    }

    fn expr_end(&mut self, e: &'a Expr) {
        let v = match &e.kind {
            ExprKind::Interp(parts) => {
                let n = parts.iter().filter(|p| matches!(p, InterpExpr::Expr(_))).count();
                self.pop_vals(n);
                V::one(STR, false)
            }
            ExprKind::Table(fields) => self.table_end(fields),
            ExprKind::Bin(op, a, b) => {
                let bv = self.pop_val();
                let av = self.pop_val();
                self.binop(*op, &av, &bv, a, b, e)
            }
            ExprKind::Index(o, k) => {
                let kv = self.pop_val();
                let ov = self.pop_val();
                self.index_read(&ov, Key::Val(&kv, k), o, e)
            }
            ExprKind::Concat(parts) => {
                let vals = self.pop_vals(parts.len());
                self.concat(&vals, parts)
            }
            ExprKind::Un(op, x) => {
                let xv = self.pop_val();
                self.unop(*op, &xv, x, e)
            }
            ExprKind::Paren(_) => {
                let v = self.pop_val();
                V::one(v.ty, v.st)
            }
            ExprKind::Call(f, args) => {
                let mut vals = self.pop_vals(args.len() + 1);
                let fv = if vals.is_empty() { V::dynamic() } else { vals.remove(0) };
                self.call(&fv, None, &vals, f, args, e)
            }
            ExprKind::Method(o, name, npos, args) => {
                let mut vals = self.pop_vals(args.len() + 1);
                let ov = if vals.is_empty() { V::dynamic() } else { vals.remove(0) };
                let ov = V::one(ov.ty, ov.st);
                if ov.st {
                    self.op_check(Op::Index, ov.ty, o, o);
                }
                let m = self.index_name(&ov, name, Some((*npos, name_end(*npos, name))));
                self.call(&m, Some(&ov), &vals, e, args, e)
            }
            _ => V::dynamic(),
        };
        self.vals.push(v);
    }

    fn and_or_end(&mut self, e: &'a Expr) {
        let bv = self.pop_val();
        let av = self.pop_val();
        let ExprKind::Bin(op, ..) = &e.kind else { return };
        let left = if *op == BinOp::And { self.falsy(av.ty) } else { self.truthy(av.ty) };
        let k = self.kinds(av.ty);
        let ty = if *op == BinOp::And && k & (K_NIL | K_BOOL | K_ANY) == 0 {
            bv.ty
        } else if *op == BinOp::Or && k & (K_NIL | K_BOOL | K_ANY) == 0 {
            av.ty
        } else {
            self.union2(left, bv.ty)
        };
        self.vals.push(V::one(ty, av.st || bv.st));
    }

    fn table_end(&mut self, fields: &'a [Field]) -> V {
        let n: usize = fields.iter().map(|f| if matches!(f, Field::Keyed(..)) { 2 } else { 1 }).sum();
        let vals = self.pop_vals(n);
        let mut vi = vals.into_iter();
        let mut named: Vec<(Box<str>, T, usize)> = Vec::new();
        let mut items = Vec::new();
        let mut keyed = Vec::new();
        let mut tail = Multi::One;
        let last = fields.len().saturating_sub(1);
        for (i, f) in fields.iter().enumerate() {
            match f {
                Field::Pos(_) => {
                    let v = vi.next().unwrap_or_else(V::dynamic);
                    if i == last && !matches!(v.multi, Multi::One) {
                        tail = v.multi;
                    } else {
                        items.push(v.ty);
                    }
                }
                Field::Named(name, ..) => {
                    let v = vi.next().unwrap_or_else(V::dynamic);
                    named.push((name.as_str().into(), v.ty, i));
                }
                Field::Keyed(ke, _) => {
                    let k = vi.next().unwrap_or_else(V::dynamic);
                    let v = vi.next().unwrap_or_else(V::dynamic);
                    match (&ke.kind, crate::nostd::str::from_utf8(match &ke.kind {
                        ExprKind::Str(s) => s,
                        _ => &[],
                    })) {
                        (ExprKind::Str(_), Ok(s)) => named.push((s.into(), v.ty, i)),
                        _ => keyed.push((k.ty, v.ty)),
                    }
                }
            }
        }
        let _ = self.tick(fields.len() as u64);
        // The last of duplicate names wins, as in Lua.
        named.sort_by(|a, b| a.0.cmp(&b.0).then(b.2.cmp(&a.2)));
        named.dedup_by(|a, b| a.0 == b.0);
        let named: Box<[(Box<str>, T)]> = named.into_iter().map(|(n, t, _)| (n, t)).collect();
        let c = Ctor { named, items: items.into(), tail, keyed: keyed.into() };
        V::one(self.add(Ty::Ctor(Box::new(c))), false)
    }

    fn binop(&mut self, op: BinOp, a: &V, b: &V, ae: &Expr, be: &Expr, whole: &Expr) -> V {
        let st = a.st || b.st;
        let (ka, kb) = (self.kinds(a.ty), self.kinds(b.ty));
        let meta = (ka | kb) & K_META != 0;
        let ty = match op {
            BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
                if st && !matches!(op, BinOp::Eq | BinOp::Ne) {
                    self.compare_check(ka, kb, a.ty, b.ty, whole);
                }
                BOOL
            }
            BinOp::Concat => {
                let r = self.concat(&[a.clone(), b.clone()], &[]);
                if st && !meta {
                    self.op_check(Op::Concat, a.ty, ae, ae);
                    self.op_check(Op::Concat, b.ty, be, be);
                }
                return r;
            }
            BinOp::BAnd | BinOp::BOr | BinOp::BXor | BinOp::Shl | BinOp::Shr => {
                if st && !meta {
                    self.op_check(Op::Bitwise, a.ty, ae, ae);
                    self.op_check(Op::Bitwise, b.ty, be, be);
                }
                if meta { ANY } else { INT }
            }
            _ => {
                if st && !meta {
                    self.op_check(Op::Arith, a.ty, ae, ae);
                    self.op_check(Op::Arith, b.ty, be, be);
                }
                if meta {
                    ANY
                } else if matches!(op, BinOp::Div | BinOp::Pow) {
                    NUM
                } else if (ka | kb) & !K_INT == 0 {
                    INT
                } else {
                    NUM
                }
            }
        };
        V::one(ty, st && ty != ANY)
    }

    fn compare_check(&mut self, ka: u16, kb: u16, a: T, b: T, whole: &Expr) {
        if (ka | kb) & K_META != 0 || self.pass != 2 {
            return;
        }
        let (ka, kb) = (ka & !K_BADSTR, kb & !K_BADSTR);
        let bits = |k: u16| (0..11).map(|i| 1u16 << i).filter(move |b| k & b != 0);
        let ok = |x: u16, y: u16| (x & K_NUM != 0 && y & K_NUM != 0) || (x & K_STR != 0 && y & K_STR != 0);
        let mut fail = None;
        let mut any_ok = false;
        for x in bits(ka) {
            for y in bits(kb) {
                if ok(x, y) {
                    any_ok = true;
                } else if fail.is_none() {
                    fail = Some((x, y));
                }
            }
        }
        if let Some((x, y)) = fail {
            let msg = if any_ok {
                let (sa, sb) = (self.show(a), self.show(b));
                format!("comparison may fail: attempt to compare {} with {} (the operands are {sa} and {sb})", kind_name(x), kind_name(y))
            } else if x == y {
                format!("attempt to compare two {} values", kind_name(x))
            } else {
                format!("attempt to compare {} with {}", kind_name(x), kind_name(y))
            };
            self.diag_at(whole, msg);
        }
    }

    fn concat(&mut self, vals: &[V], parts: &'a [Expr]) -> V {
        let st = vals.iter().any(|v| v.st);
        let mut k = 0;
        for v in vals {
            k |= self.kinds(v.ty);
        }
        let meta = k & K_META != 0;
        if st && !meta {
            for (v, e) in vals.iter().zip(parts) {
                self.op_check(Op::Concat, v.ty, e, e);
            }
        }
        if meta { V::one(ANY, false) } else { V::one(STR, st) }
    }

    fn unop(&mut self, op: UnOp, x: &V, xe: &Expr, whole: &Expr) -> V {
        let k = self.kinds(x.ty);
        let meta = k & K_META != 0;
        let ty = match op {
            UnOp::Not => BOOL,
            UnOp::Neg => {
                if x.st && !meta {
                    self.op_check(Op::Arith, x.ty, xe, whole);
                }
                if meta {
                    ANY
                } else if k & !K_INT == 0 {
                    INT
                } else {
                    NUM
                }
            }
            UnOp::BNot => {
                if x.st && !meta {
                    self.op_check(Op::Bitwise, x.ty, xe, whole);
                }
                if meta { ANY } else { INT }
            }
            UnOp::Len => {
                if x.st {
                    self.op_check(Op::Len, x.ty, xe, whole);
                }
                if k & !K_STR == 0 && k != 0 { INT } else { ANY }
            }
        };
        V::one(ty, x.st && ty != ANY)
    }

    fn index_name(&mut self, obj: &V, name: &'a str, mark: Option<(Pos, Pos)>) -> V {
        self.index_by(obj, Key::Name(name), mark)
    }

    fn index_read(&mut self, obj: &V, key: Key<'_, 'a>, oe: &'a Expr, whole: &'a Expr) -> V {
        if obj.st {
            self.op_check(Op::Index, obj.ty, oe, whole);
        }
        let at = self.range(whole);
        self.index_by(obj, key, Some(at))
    }

    /// What reading `key` from `obj` gives (and diagnostics about the key, at `mark`).
    fn index_by(&mut self, obj: &V, key: Key<'_, 'a>, mark: Option<(Pos, Pos)>) -> V {
        let members = self.flatten(obj.ty);
        let whole = if members.len() == 1 { Some(obj.ty) } else { None };
        let mut acc: Vec<T> = Vec::new();
        let mut any = false;
        let mut reported = false;
        for m in members {
            let r = match self.ty(m) {
                Ty::Shape(_) => match key.name() {
                    Some(n) => match self.field_of(m, &n) {
                        Some(t) => Some(t),
                        None => {
                            if obj.st && !reported {
                                reported = true;
                                let shown = self.show(whole.unwrap_or(m));
                                self.key_diag(mark, format!("no field '{n}' in {shown}"));
                            }
                            None
                        }
                    },
                    None => {
                        any = true;
                        None
                    }
                },
                Ty::Array(t) => {
                    let t = *t;
                    if obj.st && !reported && !self.key_fits(&key, NUM) {
                        reported = true;
                        let shown = self.show(whole.unwrap_or(m));
                        self.key_diag(mark, format!("{} is not a valid index for {shown}", key.describe()));
                    }
                    Some(self.opt(t))
                }
                Ty::Map(k, v) => {
                    let (k, v) = (*k, *v);
                    if obj.st && !reported && !self.key_fits(&key, k) {
                        reported = true;
                        let shown = self.show(whole.unwrap_or(m));
                        self.key_diag(mark, format!("{} is not a valid key for {shown}", key.describe()));
                    }
                    Some(self.opt(v))
                }
                Ty::Nil | Ty::Bool | Ty::True | Ty::False | Ty::Num | Ty::Int | Ty::Func | Ty::Sig(_) | Ty::Thread => None,
                _ => {
                    any = true;
                    None
                }
            };
            if let Some(t) = r {
                acc.push(t);
            }
        }
        if any || acc.is_empty() {
            return V::one(ANY, false);
        }
        acc.dedup();
        let ty = self.union_of(acc);
        V::one(ty, obj.st)
    }

    fn key_fits(&mut self, key: &Key<'_, '_>, t: T) -> bool {
        match key {
            Key::Name(n) => {
                let l = self.lit(n.as_bytes());
                self.sub(l, t)
            }
            Key::Val(v, _) => self.sub(v.ty, t),
        }
    }

    fn key_diag(&mut self, mark: Option<(Pos, Pos)>, msg: String) {
        if let Some((a, b)) = mark {
            self.diag(a, b, msg);
        }
    }

    /// Stores a value of type `t` under `key` in `obj`.
    fn index_write(&mut self, obj: &V, key: Key<'_, 'a>, t: T, oe: &'a Expr, whole: &'a Expr, at: (Pos, Pos)) {
        if obj.st {
            self.op_check(Op::Index, obj.ty, oe, whole);
        }
        if !obj.st {
            return;
        }
        let r = self.range(whole);
        let members = self.flatten(obj.ty);
        let one = if members.len() == 1 { Some(obj.ty) } else { None };
        for m in members {
            match self.ty(m) {
                Ty::Shape(_) => {
                    let Some(n) = key.name() else { continue };
                    match self.field_of(m, &n) {
                        Some(ft) => {
                            self.fit(t, ft, &|| format!("field '{n}'"), at);
                        }
                        None => {
                            let shown = self.show(one.unwrap_or(m));
                            self.diag(r.0, r.1, format!("no field '{n}' in {shown}"));
                        }
                    }
                    return;
                }
                Ty::Array(e) => {
                    let e = *e;
                    if !self.key_fits(&key, NUM) {
                        let shown = self.show(one.unwrap_or(m));
                        self.diag(r.0, r.1, format!("{} is not a valid index for {shown}", key.describe()));
                    }
                    let o = self.opt(e);
                    self.fit(t, o, &|| "array element".to_owned(), at);
                    return;
                }
                Ty::Map(k, v) => {
                    let (k, v) = (*k, *v);
                    if !self.key_fits(&key, k) {
                        let shown = self.show(one.unwrap_or(m));
                        self.diag(r.0, r.1, format!("{} is not a valid key for {shown}", key.describe()));
                    }
                    let o = self.opt(v);
                    self.fit(t, o, &|| "map value".to_owned(), at);
                    return;
                }
                _ => {}
            }
        }
    }

    fn write_field(&mut self, obj: &V, name: &'a str, t: T, at: (Pos, Pos)) {
        if !obj.st || self.pass != 2 {
            return;
        }
        let members = self.flatten(obj.ty);
        let one = if members.len() == 1 { Some(obj.ty) } else { None };
        for m in members {
            if matches!(self.ty(m), Ty::Shape(_)) {
                match self.field_of(m, name) {
                    Some(ft) => {
                        self.fit(t, ft, &|| format!("field '{name}'"), at);
                    }
                    None => {
                        let shown = self.show(one.unwrap_or(m));
                        self.diag(at.0, at.1, format!("no field '{name}' in {shown}"));
                    }
                }
                return;
            }
        }
    }

    /// A call of `f` (with `selfv` first for a method call) on `args`.
    fn call(&mut self, f: &V, selfv: Option<&V>, args: &[V], fe: &'a Expr, arg_es: &'a [Expr], whole: &'a Expr) -> V {
        if f.st {
            self.op_check(Op::Call, f.ty, fe, fe);
        }
        let callee = self.truthy(f.ty);
        let callee = self.resolve(callee);
        let Some((params, va, ret)) = self.sig_parts(callee) else { return V::dynamic() };
        if self.pass == 2 {
            self.check_args(&params, va, selfv, args, fe, arg_es, whole);
        }
        match ret {
            Some(r) => V { ty: r.first().copied().unwrap_or(NIL), st: true, multi: Multi::List(r) },
            None => V::dynamic(),
        }
    }

    #[allow(clippy::too_many_arguments, reason = "the parts of a call")]
    fn check_args(&mut self, params: &[T], va: Option<T>, selfv: Option<&V>, args: &[V], fe: &Expr, arg_es: &[Expr], whole: &Expr) {
        let who = match &fe.kind {
            ExprKind::Name(n) => format!("'{n}'"),
            ExprKind::Index(_, k) => match &k.kind {
                ExprKind::Str(s) => format!("'{}'", String::from_utf8_lossy(s)),
                _ => "the function".to_owned(),
            },
            ExprKind::Method(_, m, ..) => format!("method '{m}'"),
            _ => "the function".to_owned(),
        };
        let off = usize::from(selfv.is_some());
        let mut all: Vec<V> = Vec::with_capacity(args.len() + off);
        if let Some(s) = selfv {
            all.push(s.clone());
        }
        all.extend(args.iter().cloned());
        let sp = self.spread(&all);
        let call_end = (whole.call_at(), whole.end);
        let at_arg = |c: &mut Self, j: usize| -> (Pos, Pos) {
            match arg_es.get(j).or(if j >= arg_es.len() { arg_es.last() } else { None }) {
                Some(e) => c.range(e),
                None => call_end,
            }
        };
        if va.is_none() && arg_es.len() + off > params.len() {
            let j = params.len().saturating_sub(off);
            let at = at_arg(self, j);
            self.diag(at.0, at.1, format!("too many arguments to {who}: expected {}, got {}", params.len() - off.min(params.len()), arg_es.len()));
        }
        for (i, &p) in params.iter().enumerate() {
            let exact = i < sp.fixed.len();
            if !exact && sp.rep.is_none() {
                if !self.sub(NIL, p) {
                    let shown = self.show(p);
                    let msg = if i < off {
                        format!("missing self for {who} (expected {shown})")
                    } else {
                        format!("missing argument #{} to {who} (expected {shown})", i + 1 - off)
                    };
                    self.diag(call_end.0, call_end.1, msg);
                }
                continue;
            }
            let (t, _) = self.nth(&sp, i);
            let at = if i < off { self.range(fe) } else { at_arg(self, i - off) };
            if i < off {
                self.fit(t, p, &|| format!("self of {who}"), at);
            } else {
                let n = i + 1 - off;
                self.fit(t, p, &|| format!("argument #{n} to {who}"), at);
            }
        }
        if let Some(w) = va {
            for i in params.len()..sp.fixed.len() {
                let (t, _) = self.nth(&sp, i);
                let at = at_arg(self, i.saturating_sub(off));
                let n = i + 1 - off;
                self.fit(t, w, &|| format!("argument #{n} to {who}"), at);
            }
            if let Some((r, _)) = sp.rep {
                let at = at_arg(self, arg_es.len());
                self.fit(r, w, &|| format!("varargs to {who}"), at);
            }
        }
    }
}

/// A key being read or written.
enum Key<'k, 'a> {
    Name(&'a str),
    Val(&'k V, &'a Expr),
}

impl Key<'_, '_> {
    fn name(&self) -> Option<String> {
        match self {
            Key::Name(n) => Some((*n).to_owned()),
            Key::Val(_, e) => match &e.kind {
                ExprKind::Str(s) => crate::nostd::str::from_utf8(s).ok().map(str::to_owned),
                _ => None,
            },
        }
    }

    fn describe(&self) -> String {
        match self {
            Key::Name(n) => format!("field '{n}'"),
            Key::Val(_, e) => match &e.kind {
                ExprKind::Str(s) => format!("field '{}'", short(&String::from_utf8_lossy(s))),
                _ => "this key".to_owned(),
            },
        }
    }
}

fn short(s: &str) -> String {
    if s.chars().count() > 40 { format!("{}...", s.chars().take(40).collect::<String>()) } else { s.to_owned() }
}

fn name_end(pos: Pos, name: &str) -> Pos {
    Pos { line: pos.line, col: pos.col.saturating_add(u32::try_from(name.len()).unwrap_or(0)) }
}

fn truthy_const(e: &Expr) -> bool {
    matches!(e.kind, ExprKind::True | ExprKind::Int(_) | ExprKind::Num(_) | ExprKind::Str(_))
}

fn falsy_const(e: &Expr) -> bool {
    matches!(e.kind, ExprKind::False | ExprKind::Nil)
}
