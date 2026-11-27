//! `string.pack`, `string.unpack` and `string.packsize`, as in Lua 5.4.

#[allow(unused_imports)]
use crate::nostd::prelude::*;
use super::R;
use crate::value::Value;
use crate::vm::{Args, Lua, LuaError};

const MAX_INT_SIZE: usize = 16;
const NATIVE_ALIGN: usize = 8;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Int,
    Uint,
    Float,
    Double,
    Number,
    Char,
    Str,
    Zstr,
    Padding,
    PadAlign,
    Nop,
}

struct Fmt<'a> {
    f: &'a [u8],
    i: usize,
    little: bool,
    max_align: usize,
    /// The option byte that was not one (S5COR-3: quoted as it is in the message).
    bad: Option<u8>,
}

impl<'a> Fmt<'a> {
    /// `e`, or the bad option's message with its byte as it is (C Lua's `%c`).
    fn raw_error(&self, lua: &mut Lua, e: LuaError) -> LuaError {
        match self.bad {
            Some(b) if !b.is_ascii() => lua.msg_bytes(&[b"invalid format option '", &[b], b"'"]),
            _ => e,
        }
    }

    fn num(&mut self, df: usize) -> usize {
        if !self.f.get(self.i).copied().is_some_and(|c| c.is_ascii_digit()) {
            return df;
        }
        let mut a: usize = 0;
        while let Some(&c) = self.f.get(self.i) {
            // C's `getnum` stops reading digits past `(INT_MAX - 9) / 10` (`MAXSIZE` is INT_MAX).
            if !c.is_ascii_digit() || a > (i32::MAX as usize - 9) / 10 {
                break;
            }
            a = a * 10 + usize::from(c - b'0');
            self.i += 1;
        }
        a
    }

    fn int_size(&mut self, df: usize) -> Result<usize, LuaError> {
        let sz = self.num(df);
        if sz > MAX_INT_SIZE || sz == 0 {
            return Err(LuaError::msg(format!("integral size ({sz}) out of limits [1,{MAX_INT_SIZE}]")));
        }
        Ok(sz)
    }

    /// The next option: kind and size.
    fn option(&mut self) -> Result<(Kind, usize), LuaError> {
        let c = self.f.get(self.i).copied().unwrap_or(0);
        self.i += 1;
        Ok(match c {
            b'b' => (Kind::Int, 1),
            b'B' => (Kind::Uint, 1),
            b'h' => (Kind::Int, 2),
            b'H' => (Kind::Uint, 2),
            b'l' | b'j' => (Kind::Int, 8),
            b'L' | b'J' | b'T' => (Kind::Uint, 8),
            b'f' => (Kind::Float, 4),
            b'n' | b'd' => (if c == b'n' { Kind::Number } else { Kind::Double }, 8),
            b'i' => (Kind::Int, self.int_size(4)?),
            b'I' => (Kind::Uint, self.int_size(4)?),
            b's' => (Kind::Str, self.int_size(8)?),
            b'c' => {
                let had = self.f.get(self.i).copied().is_some_and(|c| c.is_ascii_digit());
                let n = self.num(usize::MAX);
                if !had {
                    return Err(LuaError::msg("missing size for format option 'c'"));
                }
                (Kind::Char, n)
            }
            b'z' => (Kind::Zstr, 0),
            b'x' => (Kind::Padding, 1),
            b'X' => (Kind::PadAlign, 0),
            b' ' => (Kind::Nop, 0),
            b'<' | b'=' => {
                self.little = true;
                (Kind::Nop, 0)
            }
            b'>' => {
                self.little = false;
                (Kind::Nop, 0)
            }
            b'!' => {
                self.max_align = self.int_size(NATIVE_ALIGN)?;
                (Kind::Nop, 0)
            }
            other => {
                self.bad = Some(other);
                return Err(LuaError::msg(format!("invalid format option '{}'", char::from(other))));
            }
        })
    }

    /// The next option with its alignment padding for `total` bytes so far.
    fn details(&mut self, total: usize) -> Result<(Kind, usize, usize), LuaError> {
        let (kind, size) = self.option()?;
        let mut align = size;
        if kind == Kind::PadAlign {
            if self.i >= self.f.len() {
                return Err(LuaError::Arg(1, "invalid next option for option 'X'".into()));
            }
            let (k2, s2) = self.option()?;
            align = s2;
            if k2 == Kind::Char || align == 0 {
                return Err(LuaError::Arg(1, "invalid next option for option 'X'".into()));
            }
        }
        let mut ntoalign = 0;
        if align > 1 && kind != Kind::Char {
            let align = align.min(self.max_align);
            if align & (align - 1) != 0 {
                return Err(LuaError::Arg(1, "format asks for alignment not power of 2".into()));
            }
            ntoalign = (align - (total & (align - 1))) & (align - 1);
        }
        Ok((kind, size, ntoalign))
    }
}

fn put_int(out: &mut Vec<u8>, v: u64, little: bool, size: usize, neg: bool) {
    let mut bytes = vec![0u8; size];
    for (i, b) in bytes.iter_mut().enumerate() {
        *b = if i < 8 { (v >> (8 * i)) as u8 } else if neg { 0xff } else { 0 };
    }
    if !little {
        bytes.reverse();
    }
    out.extend_from_slice(&bytes);
}

/// Fuel per format byte read (`pack`, `packsize`, `unpack`). It was one unit per 8 bytes:
/// a format of 16,384 items read at 5x plain code per unit of fuel (`packsize`, `pack`)
/// and 7.5x (`unpack`), up to the bound of 10 on some code layouts. At 2 a byte: under 2x
/// against the relative cost tests' control, with global writes in the fast loop.
const FORMAT_FUEL_PER_BYTE: u64 = 2;

/// Format bytes read between looks at the host's interrupt and deadline (SEC-M12: a 16 MiB
/// format ran 14 to 22 ms past a 5 ms time limit in `pack` and `packsize`).
const FORMAT_LOOK_EVERY: usize = 1 << 16;

/// Looks at the interrupt and deadline when the format's reader `i` has crossed the next
/// `FORMAT_LOOK_EVERY` bytes since `*next`.
#[inline]
fn format_look(lua: &mut Lua, i: usize, next: &mut usize) -> Result<(), LuaError> {
    if i >= *next {
        *next = i + FORMAT_LOOK_EVERY;
        lua.check_time()?;
    }
    Ok(())
}

/// The format as C Lua reads it, a C string ending at the first NUL byte, of string `s`:
/// read and copied a megabyte at a time between looks at the
/// host's interrupt and deadline (SEC-M12: a 16 MiB format's scan and copy came before the
/// first look).
fn c_format(lua: &mut Lua, s: crate::value::StrRef, hold: &mut super::MemHold) -> Result<Vec<u8>, LuaError> {
    // Counted while the caller holds it (SEC-M14).
    hold.cover(lua, lua.bytes(s).len())?;
    let mut f = Vec::with_capacity(lua.bytes(s).len());
    let mut at = 0;
    loop {
        let done = {
            let b = lua.bytes(s);
            let piece = b.get(at..(at + super::COPY_PIECE).min(b.len())).unwrap_or_default();
            match piece.iter().position(|&c| c == 0) {
                Some(k) => {
                    f.extend_from_slice(piece.get(..k).unwrap_or_default());
                    true
                }
                None => {
                    f.extend_from_slice(piece);
                    at += piece.len();
                    at >= b.len()
                }
            }
        };
        if done {
            return Ok(f);
        }
        lua.check_time()?;
    }
}

pub(super) fn pack(lua: &mut Lua, a: Args) -> R {
    let fs = lua.check_str(a, 0)?;
    let mut hold = super::MemHold::default();
    let f = c_format(lua, fs, &mut hold)?;
    // The format is read item by item, before any error: paid for first.
    lua.charge((f.len() as u64).saturating_mul(FORMAT_FUEL_PER_BYTE))?;
    // The bytes copied are paid for on every exit (`OutBuf`); strings read and refused
    // owe their length.
    let mut out = super::OutBuf::new(32);
    let mut read = 0usize;
    let r = pack_into(lua, a, &f, &mut out, &mut read);
    super::owe(read as u64 / 32);
    lua.charge(1)?;
    r?;
    // The buffer becomes the string as it is (not copied again: 16 MiB was 4 ms).
    let out = out.into_vec(lua);
    lua.ret_owned(out)
}

fn pack_into(lua: &mut Lua, a: Args, f: &[u8], out: &mut super::OutBuf, read: &mut usize) -> Result<(), LuaError> {
    let mut h = Fmt { f, i: 0, little: true, max_align: 1, bad: None };
    let mut arg = 0usize;
    let mut look = FORMAT_LOOK_EVERY;
    while h.i < f.len() {
        format_look(lua, h.i, &mut look)?;
        out.pay(lua)?;
        let (kind, size, ntoalign) = h.details(out.len()).map_err(|e| h.raw_error(lua, e))?;
        if ntoalign + size.min(1 << 20) > lua.limits.string_len.saturating_sub(out.len()) && kind != Kind::Char {
            return Err(LuaError::msg("resulting string too large"));
        }
        out.extend(crate::nostd::iter::repeat_n(0u8, ntoalign));
        if !matches!(kind, Kind::Padding | Kind::PadAlign | Kind::Nop) {
            arg += 1;
            // Lua's pack pushes a nil after its arguments: the first missing one is "nil".
            if arg == a.n {
                return Err(match kind {
                    Kind::Int | Kind::Uint | Kind::Float | Kind::Double | Kind::Number => {
                        LuaError::Arg(arg + 1, "number expected, got nil".into())
                    }
                    _ => LuaError::Arg(arg + 1, "string expected, got nil".into()),
                });
            }
        }
        match kind {
            Kind::Int | Kind::Uint => {
                let n = lua.check_int(a, arg)?;
                if size < 8 {
                    if kind == Kind::Int {
                        let lim = 1i64 << (size * 8 - 1);
                        if !(-lim <= n && n < lim) {
                            return Err(LuaError::Arg(arg + 1, "integer overflow".into()));
                        }
                    } else if (n as u64) >= (1u64 << (size * 8)) {
                        return Err(LuaError::Arg(arg + 1, "unsigned overflow".into()));
                    }
                }
                put_int(out, n as u64, h.little, size, kind == Kind::Int && n < 0);
            }
            Kind::Float => {
                let x = lua.check_num(a, arg)? as f32;
                let b = if h.little { x.to_le_bytes() } else { x.to_be_bytes() };
                out.extend_from_slice(&b);
            }
            Kind::Double | Kind::Number => {
                let x = lua.check_num(a, arg)?;
                let b = if h.little { x.to_le_bytes() } else { x.to_be_bytes() };
                out.extend_from_slice(&b);
            }
            Kind::Char => {
                let s = lua.check_str(a, arg)?;
                let b = lua.bytes(s);
                if size > lua.limits.string_len.saturating_sub(out.len()) {
                    return Err(LuaError::msg("resulting string too large"));
                }
                if b.len() > size {
                    return Err(LuaError::Arg(arg + 1, "string longer than given size".into()));
                }
                let (len, pad) = (b.len(), size - b.len());
                // The room paid for before it is written (SEC7-N2: copied first, a field of
                // 12 MiB was outside the account until the next payment).
                out.reserve(lua, size)?;
                lua.extend_watched(&mut **out, s, 0, len)?;
                out.extend(crate::nostd::iter::repeat_n(0u8, pad));
            }
            Kind::Str => {
                let s = lua.check_str(a, arg)?;
                let b = lua.bytes(s);
                if size < 8 && b.len() as u64 >= 1u64 << (size * 8) {
                    return Err(LuaError::Arg(arg + 1, "string length does not fit in given size".into()));
                }
                let len = b.len();
                // (Checked and paid for before it is written: SEC7-N2.)
                lua.check_len(out.len().saturating_add(size).saturating_add(len))?;
                out.reserve(lua, size + len)?;
                put_int(out, len as u64, h.little, size, false);
                lua.extend_watched(&mut **out, s, 0, len)?;
            }
            Kind::Zstr => {
                let s = lua.check_str(a, arg)?;
                let b = lua.bytes(s);
                if b.contains(&0) {
                    *read += b.len();
                    return Err(LuaError::Arg(arg + 1, "string contains zeros".into()));
                }
                let len = b.len();
                // (Checked and paid for before it is written: SEC7-N2.)
                lua.check_len(out.len().saturating_add(len + 1))?;
                out.reserve(lua, len + 1)?;
                lua.extend_watched(&mut **out, s, 0, len)?;
                out.push(0);
            }
            Kind::Padding => out.push(0),
            Kind::PadAlign | Kind::Nop => {}
        }
        lua.check_len(out.len())?;
    }
    Ok(())
}

pub(super) fn packsize(lua: &mut Lua, a: Args) -> R {
    let fs = lua.check_str(a, 0)?;
    let mut hold = super::MemHold::default();
    let f = c_format(lua, fs, &mut hold)?;
    // The format is read item by item, before any error: paid for first.
    lua.charge((f.len() as u64).saturating_mul(FORMAT_FUEL_PER_BYTE))?;
    let mut h = Fmt { f: &f, i: 0, little: true, max_align: 1, bad: None };
    let mut total: usize = 0;
    let mut look = FORMAT_LOOK_EVERY;
    while h.i < f.len() {
        format_look(lua, h.i, &mut look)?;
        let (kind, size, ntoalign) = h.details(total).map_err(|e| h.raw_error(lua, e))?;
        if matches!(kind, Kind::Str | Kind::Zstr) {
            return Err(LuaError::Arg(1, "variable-length format".into()));
        }
        let size = size + ntoalign;
        if size > 0x7fff_ffff - total {
            return Err(LuaError::Arg(1, "format result too large".into()));
        }
        total += size;
    }
    lua.ret1(Value::Int(total as i64))
}

pub(super) fn unpack(lua: &mut Lua, a: Args) -> R {
    let fs = lua.check_str(a, 0)?;
    let ds = lua.check_str(a, 1)?;
    let mut hold = super::MemHold::default();
    let f = c_format(lua, fs, &mut hold)?;
    // The format is read item by item, before any error: paid for first.
    lua.charge((f.len() as u64).saturating_mul(FORMAT_FUEL_PER_BYTE))?;
    // The values go straight into the results, counted as the list grows (SEC7-N1: built
    // uncounted and then copied, a 16 MiB format of 'c0' items made 8M values twice, 288
    // MiB under a 48 MiB limit), and then are counted where they are placed (SEC-M15).
    let mut out = crate::nostd::mem::take(&mut lua.rets);
    out.clear();
    let mut read = 0usize;
    // The data is read in place (never copied whole); the values made, the bytes the
    // strings take and what a 'z' scans are owed, paid on every exit.
    let r = unpack_into(lua, a, &f, ds, &mut out, &mut read, &mut hold);
    super::owe(out.len() as u64 / 4 + read as u64 / 64);
    lua.rets = out;
    lua.charge(1)?;
    r?;
    Ok(crate::vm::Flow::Return)
}

/// C's `unpackint`: `field`'s integer, sign-extended when `signed`; past 8 bytes, the extra
/// bytes must be the sign extension.
fn unpack_int(field: &[u8], little: bool, signed: bool) -> Result<u64, LuaError> {
    let size = field.len();
    let mut bytes = field.to_vec();
    if !little {
        bytes.reverse();
    }
    let mut v: u64 = 0;
    for (i, b) in bytes.iter().enumerate().take(8) {
        v |= u64::from(*b) << (8 * i);
    }
    if size < 8 {
        if signed {
            let shift = 64 - size * 8;
            v = (((v << shift) as i64) >> shift) as u64;
        }
    } else if size > 8 {
        let mask = if signed && (v as i64) < 0 { 0xff } else { 0 };
        if bytes.iter().skip(8).any(|&b| b != mask) {
            return Err(LuaError::msg(format!("{size}-byte integer does not fit into Lua Integer")));
        }
    }
    Ok(v)
}

fn unpack_into(
    lua: &mut Lua,
    a: Args,
    f: &[u8],
    ds: crate::value::StrRef,
    out: &mut Vec<Value>,
    read: &mut usize,
    hold: &mut super::MemHold,
) -> Result<(), LuaError> {
    let ld = lua.bytes(ds).len();
    let pos = super::string::posrelat_i(lua.opt_int(a, 2, 1)?, ld) - 1;
    if pos > ld {
        return Err(LuaError::Arg(3, "initial position out of string".into()));
    }
    let mut pos = pos;
    let mut h = Fmt { f, i: 0, little: true, max_align: 1, bad: None };
    let mut look = FORMAT_LOOK_EVERY;
    while h.i < f.len() {
        format_look(lua, h.i, &mut look)?;
        // (Room for this item's value and the position after, counted before it grows.)
        if out.len() + 2 > out.capacity() {
            let cap = (out.capacity() * 2).max(16);
            hold.cover(lua, cap * crate::nostd::mem::size_of::<Value>())?;
            out.reserve_exact(cap - out.len());
        }
        let (kind, size, ntoalign) = h.details(pos).map_err(|e| h.raw_error(lua, e))?;
        if ntoalign + size > ld - pos {
            return Err(LuaError::Arg(2, "data string too short".into()));
        }
        pos += ntoalign;
        // A number's bytes (at most 16), copied out of the data.
        let mut num = [0u8; MAX_INT_SIZE];
        if !matches!(kind, Kind::Char | Kind::Zstr) {
            let field = lua.bytes(ds).get(pos..pos + size.min(MAX_INT_SIZE)).unwrap_or_default();
            if let Some(d) = num.get_mut(..field.len()) {
                d.copy_from_slice(field);
            }
        }
        let field = num.get(..size.min(MAX_INT_SIZE)).unwrap_or_default();
        match kind {
            Kind::Int | Kind::Uint => out.push(Value::Int(unpack_int(field, h.little, kind == Kind::Int)? as i64)),
            Kind::Float => {
                let mut b = [0u8; 4];
                b.copy_from_slice(field.get(..4).unwrap_or(&[0; 4]));
                let x = if h.little { f32::from_le_bytes(b) } else { f32::from_be_bytes(b) };
                out.push(Value::Num(f64::from(x)));
            }
            Kind::Double | Kind::Number => {
                let mut b = [0u8; 8];
                b.copy_from_slice(field.get(..8).unwrap_or(&[0; 8]));
                let x = if h.little { f64::from_le_bytes(b) } else { f64::from_be_bytes(b) };
                out.push(Value::Num(x));
            }
            Kind::Char => {
                *read += size;
                let v = lua.new_derived(ds, pos, pos + size, |src, dest| dest.copy_from_slice(src))?;
                out.push(v);
            }
            Kind::Str => {
                let len = usize::try_from(unpack_int(field, h.little, false)?).unwrap_or(usize::MAX);
                if len > ld - pos - size {
                    return Err(LuaError::Arg(2, "data string too short".into()));
                }
                *read += len;
                let from = pos + size;
                let v = lua.new_copied(ds, from, from + len)?;
                out.push(v);
                pos += len;
            }
            Kind::Zstr => {
                // Its end found a megabyte at a time between looks at the interrupt and
                // deadline (SEC-M12: 16 MiB ran 9 ms past a 5 ms time limit).
                let mut at = pos;
                let z = loop {
                    let (found, piece, last) = {
                        let rest = lua.bytes(ds).get(at..).unwrap_or_default();
                        let piece = rest.get(..super::COPY_PIECE.min(rest.len())).unwrap_or_default();
                        (piece.iter().position(|&c| c == 0), piece.len(), piece.len() == rest.len())
                    };
                    if let Some(k) = found {
                        break Some(at + k - pos);
                    }
                    if last {
                        break None;
                    }
                    at += piece;
                    lua.check_time()?;
                };
                let Some(z) = z else {
                    *read += lua.bytes(ds).len().saturating_sub(pos);
                    return Err(LuaError::Arg(2, "unfinished string for format 'z'".into()));
                };
                *read += z + 1;
                let v = lua.new_copied(ds, pos, pos + z)?;
                out.push(v);
                pos += z + 1;
            }
            Kind::Padding | Kind::PadAlign | Kind::Nop => {}
        }
        pos += size;
    }
    out.push(Value::Int(pos as i64 + 1));
    Ok(())
}
