//! Values: what a register, a table slot or an upvalue holds.

#[allow(unused_imports)]
use crate::nostd::prelude::*;
use crate::number;

macro_rules! gc_ref {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        ///
        /// A reference into the collector's heap: valid while what it names is reachable from
        /// the script, from a running call's arguments, or from a [`Root`](crate::Root).
        #[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
        pub struct $name {
            pub(crate) idx: u32,
            pub(crate) generation: u32,
        }
    };
}

gc_ref!(
    /// A string.
    StrRef
);
gc_ref!(
    /// A table.
    TableRef
);
gc_ref!(
    /// A function: a Lua closure or a native (Rust) function.
    FuncRef
);
gc_ref!(
    /// A full userdata: a host value with an optional metatable.
    UdRef
);
gc_ref!(
    /// A thread (coroutine).
    ThreadRef
);

/// A RemedyLua value. Strings, tables, functions, userdata and threads are references
/// into the collector's heap ([`StrRef`] and friends); the rest are plain data. Values are
/// `Copy`: holding one does not keep what it names alive (see [`Root`](crate::Root)).
// Every payload at offset 8 (the tag, then one 8-byte slot): a value copies as two
// words, never byte by byte through the stack. The tag is a whole word: with a one-byte
// tag the seven bytes after it were part of the enum's payload as the code generator sees
// it, so a value read for a test (`is_nil`) and then stored was copied as 1 + 8 + 4 + 2 + 1
// bytes, through the stack frame, and the stores stalled the loads after them (fannkuch,
// whose every step reads or writes an array slot, ran 28% slower).
#[repr(C, u64)]
#[derive(Clone, Copy, Debug, Default)]
pub enum Value {
    /// `nil`.
    #[default]
    Nil,
    /// `true` or `false`.
    Bool(bool),
    /// An integer (64-bit, wrapping).
    Int(i64),
    /// A float (64-bit).
    Num(f64),
    /// A string (bytes, not necessarily UTF-8).
    Str(StrRef),
    /// A table.
    Table(TableRef),
    /// A function.
    Func(FuncRef),
    /// A full userdata.
    UserData(UdRef),
    /// A coroutine.
    Thread(ThreadRef),
}

// Two words, with or without `Option` (its `None` is a tag no value has): table slots
// and stacks are charged `SLOT_SIZE` (16) bytes a value.
const _: () = assert!(crate::nostd::mem::size_of::<Value>() == 16 && crate::nostd::mem::size_of::<Option<Value>>() == 16);

impl Value {
    /// The value, copied as its two words (tag, then payload) rather than one 16-byte
    /// move. A register just written by an instruction is two 8-byte stores (tag and
    /// payload), and a 16-byte load over two stores cannot be forwarded from them: the
    /// copy waits for both to reach the cache, about a dozen cycles per Lua `MOVE` after
    /// an arithmetic result. Use it where a value is copied right after being made.
    #[inline(always)]
    pub(crate) fn copied(&self) -> Value {
        match *self {
            Value::Nil => Value::Nil,
            Value::Bool(x) => Value::Bool(x),
            Value::Int(x) => Value::Int(x),
            Value::Num(x) => Value::Num(x),
            Value::Str(x) => Value::Str(x),
            Value::Table(x) => Value::Table(x),
            Value::Func(x) => Value::Func(x),
            Value::UserData(x) => Value::UserData(x),
            Value::Thread(x) => Value::Thread(x),
        }
    }

    /// Whether it counts as true in a condition (everything but `nil` and `false`).
    #[inline]
    pub fn truthy(self) -> bool {
        !matches!(self, Value::Nil | Value::Bool(false))
    }

    /// Whether it is `nil`.
    #[inline]
    pub fn is_nil(self) -> bool {
        matches!(self, Value::Nil)
    }

    /// Lua's name for its type, as `type` returns it.
    pub fn type_name(self) -> &'static str {
        match self {
            Value::Nil => "nil",
            Value::Bool(_) => "boolean",
            Value::Int(_) | Value::Num(_) => "number",
            Value::Str(_) => "string",
            Value::Table(_) => "table",
            Value::Func(_) => "function",
            Value::UserData(_) => "userdata",
            Value::Thread(_) => "thread",
        }
    }

    /// Whether it lives in the collector's heap.
    #[inline]
    pub(crate) fn is_collectable(self) -> bool {
        matches!(self, Value::Str(_) | Value::Table(_) | Value::Func(_) | Value::UserData(_) | Value::Thread(_))
    }

    /// Equality without metamethods, by identity for strings: short strings (up to 40
    /// bytes) are interned, so this is `rawequal` for them, but two equal long strings may be
    /// different objects. Hosts compare values with [`Lua::raw_equal`](crate::Lua::raw_equal).
    #[inline]
    pub fn raw_eq(self, other: Value) -> bool {
        match (self, other) {
            (Value::Nil, Value::Nil) => true,
            (Value::Bool(a), Value::Bool(b)) => a == b,
            (Value::Int(a), Value::Int(b)) => a == b,
            (Value::Num(a), Value::Num(b)) => a == b,
            (Value::Int(i), Value::Num(f)) | (Value::Num(f), Value::Int(i)) => number::int_eq_float(i, f),
            (Value::Str(a), Value::Str(b)) => a == b,
            (Value::Table(a), Value::Table(b)) => a == b,
            (Value::Func(a), Value::Func(b)) => a == b,
            (Value::UserData(a), Value::UserData(b)) => a == b,
            (Value::Thread(a), Value::Thread(b)) => a == b,
            _ => false,
        }
    }

    /// The integer it is, or converts to exactly (a float with an integral value).
    pub fn as_int(self) -> Option<i64> {
        match self {
            Value::Int(i) => Some(i),
            Value::Num(f) => number::float_to_int(f),
            _ => None,
        }
    }

    /// The number it is, as a float.
    pub fn as_num(self) -> Option<f64> {
        match self {
            Value::Int(i) => Some(i as f64),
            Value::Num(f) => Some(f),
            _ => None,
        }
    }

    /// The boolean it is.
    pub fn as_bool(self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(b),
            _ => None,
        }
    }

    /// The table it is.
    pub fn as_table(self) -> Option<TableRef> {
        match self {
            Value::Table(t) => Some(t),
            _ => None,
        }
    }

    /// The function it is.
    pub fn as_func(self) -> Option<FuncRef> {
        match self {
            Value::Func(f) => Some(f),
            _ => None,
        }
    }
}

/// A table key: a value normalized so equal keys are equal Rust values (floats with
/// integral values become integers).
#[inline]
pub(crate) fn normalize_key(v: Value) -> Value {
    match v {
        Value::Num(f) => match number::float_to_int(f) {
            Some(i) => Value::Int(i),
            None => v,
        },
        other => other,
    }
}

/// A hash for a normalized key: seeded (each table carries its state's seed, so keys a
/// script chooses cannot be aimed at one slot), then mixed (murmur3's finalizer).
#[inline]
pub(crate) fn hash_key(v: Value, seed: u64) -> u64 {
    let raw = match v {
        Value::Nil => 0,
        Value::Bool(b) => u64::from(b) + 1,
        Value::Int(i) => i as u64,
        Value::Num(f) => f.to_bits() ^ 0x5555,
        Value::Str(r) => u64::from(r.idx) ^ (1 << 40),
        Value::Table(r) => u64::from(r.idx) ^ (2 << 40),
        Value::Func(r) => u64::from(r.idx) ^ (3 << 40),
        Value::UserData(r) => u64::from(r.idx) ^ (4 << 40),
        Value::Thread(r) => u64::from(r.idx) ^ (5 << 40),
    };
    let mut h = raw ^ seed;
    h ^= h >> 33;
    h = h.wrapping_mul(0xff51_afd7_ed55_8ccd);
    h ^= h >> 33;
    h = h.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
    h ^ (h >> 33)
}

impl PartialEq for Value {
    /// Raw equality without the heap: `1 == 1.0`, objects by identity, strings by identity
    /// (equal to by content for short strings only). Compare script values with
    /// [`Lua::raw_equal`](crate::Lua::raw_equal), which compares strings by content.
    fn eq(&self, other: &Value) -> bool {
        self.raw_eq(*other)
    }
}
