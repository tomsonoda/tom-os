//! `string.format`: C's printf conversions as Lua 5.4 accepts them.

#[allow(unused_imports)]
use crate::nostd::prelude::*;
use super::R;
use crate::number;
use crate::value::Value;
use crate::vm::{Args, Lua, LuaError};

struct Spec {
    left: bool,
    plus: bool,
    space: bool,
    alt: bool,
    zero: bool,
    width: usize,
    prec: Option<usize>,
}

fn pad(out: &mut Vec<u8>, sign: &str, body: &(impl AsRef<[u8]> + ?Sized), spec: &Spec, zero_ok: bool) {
    let body = body.as_ref();
    let len = sign.len() + body.len();
    let fill = spec.width.saturating_sub(len);
    if spec.left {
        out.extend_from_slice(sign.as_bytes());
        out.extend_from_slice(body);
        out.extend(crate::nostd::iter::repeat_n(b' ', fill));
    } else if spec.zero && zero_ok {
        out.extend_from_slice(sign.as_bytes());
        out.extend(crate::nostd::iter::repeat_n(b'0', fill));
        out.extend_from_slice(body);
    } else {
        out.extend(crate::nostd::iter::repeat_n(b' ', fill));
        out.extend_from_slice(sign.as_bytes());
        out.extend_from_slice(body);
    }
}

/// `%.Nd`-style precision: at least `prec` digits (none for a zero at precision 0).
fn apply_prec(digits: &mut String, prec: Option<usize>, zero: bool) {
    if let Some(p) = prec {
        if p == 0 && zero {
            digits.clear();
        }
        while digits.len() < p {
            digits.insert(0, '0');
        }
    }
}

fn parse_spec(body: &[u8]) -> Spec {
    let mut spec = Spec { left: false, plus: false, space: false, alt: false, zero: false, width: 0, prec: None };
    let mut j = 0;
    while let Some(&c) = body.get(j) {
        match c {
            b'-' => spec.left = true,
            b'+' => spec.plus = true,
            b' ' => spec.space = true,
            b'#' => spec.alt = true,
            b'0' => spec.zero = true,
            _ => break,
        }
        j += 1;
    }
    while let Some(&c) = body.get(j).filter(|c| c.is_ascii_digit()) {
        spec.width = spec.width * 10 + usize::from(c - b'0');
        j += 1;
    }
    if body.get(j) == Some(&b'.') {
        j += 1;
        let mut p = 0;
        while let Some(&c) = body.get(j).filter(|c| c.is_ascii_digit()) {
            p = p * 10 + usize::from(c - b'0');
            j += 1;
        }
        spec.prec = Some(p);
    }
    spec
}

fn sign_of(neg: bool, spec: &Spec) -> &'static str {
    if neg {
        "-"
    } else if spec.plus {
        "+"
    } else if spec.space {
        " "
    } else {
        ""
    }
}

pub(super) fn format(lua: &mut Lua, a: Args) -> R {
    let fs = lua.check_str(a, 0)?;
    // The format, copied (the arguments' conversions call back into Lua): on the stack
    // when short.
    let mut short = [0u8; 64];
    let long: Vec<u8>;
    let mut hold = super::MemHold::default();
    let f: &[u8] = match short.get_mut(..lua.bytes(fs).len()) {
        Some(dest) => {
            dest.copy_from_slice(lua.bytes(fs));
            dest
        }
        None => {
            // Counted while held (SEC-M14).
            hold.cover(lua, lua.bytes(fs).len())?;
            long = lua.bytes(fs).to_vec();
            &long
        }
    };
    // The output is paid for on every exit (`OutBuf`), and on an error the format copied.
    let mut out = super::OutBuf::new(32);
    // Room for the usual result at once (growing from empty reallocated it 3 or 4 times).
    out.reserve(lua, f.len() + 32)?;
    let r = format_into(lua, a, f, &mut out);
    if r.is_err() {
        super::owe(f.len() as u64 / 32);
    }
    lua.charge(1)?;
    r?;
    let b = out.into_vec(lua);
    lua.ret_owned(b)
}

fn format_into(lua: &mut Lua, a: Args, f: &[u8], out: &mut super::OutBuf) -> Result<(), LuaError> {
    use crate::nostd::io::Write as _;
    let mut arg = 0usize;
    let mut i = 0usize;
    while i < f.len() {
        let c = f.get(i).copied().unwrap_or(0);
        i += 1;
        if c != b'%' {
            // A run of text, its room paid for first (SEC-M14).
            let run = f.get(i..).and_then(|r| r.iter().position(|&b| b == b'%')).map_or(f.len(), |k| i + k);
            out.reserve(lua, run + 1 - i)?;
            out.extend_from_slice(f.get(i - 1..run).unwrap_or_default());
            i = run;
            continue;
        }
        if f.get(i) == Some(&b'%') {
            out.push(b'%');
            i += 1;
            continue;
        }
        // What the conversions so far wrote (the plain forms below `continue`).
        out.pay(lua)?;
        // Lua's `getformat`: flags, digits and points, then the conversion character.
        arg += 1;
        if arg >= a.n {
            return Err(LuaError::Arg(arg + 1, "no value".into()));
        }
        let start = i;
        while f.get(i).is_some_and(|c| matches!(c, b'-' | b'+' | b' ' | b'#' | b'.' | b'0'..=b'9')) {
            i += 1;
        }
        let body: &[u8] = f.get(start..i).unwrap_or_default();
        if body.len() + 1 >= 22 {
            return Err(LuaError::msg("invalid format (too long)"));
        }
        let conv = f.get(i).copied().unwrap_or(0);
        i += 1;
        // The specification as written, for error messages only.
        let form = || {
            let mut form = String::from("%");
            form.push_str(&String::from_utf8_lossy(body));
            if conv != 0 && conv.is_ascii() {
                form.push(char::from(conv));
            }
            form
        };
        // The common plain forms, written straight into the output.
        if body.is_empty() {
            match conv {
                b'd' | b'i' => {
                    let n = lua.check_int(a, arg)?;
                    let _ = write!(out, "{n}");
                    continue;
                }
                b's' => {
                    let v = lua.arg(a, arg);
                    // A piece at a time between looks (`extend_watched`).
                    let r = match v {
                        Value::Str(r) => Some(r),
                        _ => match lua.tostring(v)? {
                            Value::Str(r) => Some(r),
                            _ => None,
                        },
                    };
                    if let Some(r) = r {
                        let n = lua.bytes(r).len();
                        lua.check_len(out.len().saturating_add(n))?;
                        out.reserve(lua, n)?;
                        lua.extend_watched(&mut **out, r, 0, usize::MAX)?;
                    }
                    lua.check_len(out.len())?;
                    continue;
                }
                _ => {}
            }
        }
        let spec = parse_spec(body);
        // Lua's `checkformat`: only the conversion's flags, two-digit width and precision.
        let check = |flags: &[u8], precision: bool| -> Result<(), LuaError> {
            let mut j = 0;
            while body.get(j).is_some_and(|c| flags.contains(c)) {
                j += 1;
            }
            if body.get(j) != Some(&b'0') {
                let mut d = 0;
                while d < 2 && body.get(j).is_some_and(u8::is_ascii_digit) {
                    j += 1;
                    d += 1;
                }
                if body.get(j) == Some(&b'.') && precision {
                    j += 1;
                    let mut d = 0;
                    while d < 2 && body.get(j).is_some_and(u8::is_ascii_digit) {
                        j += 1;
                        d += 1;
                    }
                }
            }
            if j != body.len() {
                return Err(LuaError::msg(format!("invalid conversion specification: '{}'", form())));
            }
            Ok(())
        };
        match conv {
            b'c' => check(b"-", false)?,
            b'd' | b'i' => {
                lua.check_int(a, arg)?;
                check(b"-+0 ", true)?;
            }
            b'u' => {
                lua.check_int(a, arg)?;
                check(b"-0", true)?;
            }
            b'o' | b'x' | b'X' => {
                lua.check_int(a, arg)?;
                check(b"-#0", true)?;
            }
            b'a' | b'A' => check(b"-+ #0", true)?,
            b'f' | b'e' | b'E' | b'g' | b'G' => {
                lua.check_num(a, arg)?;
                check(b"-+ #0", true)?;
            }
            b'p' => check(b"-", false)?,
            b'q' => {
                if !body.is_empty() {
                    return Err(LuaError::msg("specifier '%q' cannot have modifiers"));
                }
            }
            b's' => {}
            // The conversion byte as it is (S5COR-3), not as the Latin-1 character.
            _ if !conv.is_ascii() => {
                return Err(lua.msg_bytes(&[b"invalid conversion '", form().as_bytes(), &[conv], b"' to 'format'"]));
            }
            _ => return Err(LuaError::msg(format!("invalid conversion '{}' to 'format'", form()))),
        }
        match conv {
            b'c' => {
                // The byte as it is, even past 0x7f.
                let n = lua.check_int(a, arg)?;
                pad(out, "", &[n as u8], &spec, false);
            }
            b'd' | b'i' | b'u' => {
                let n = lua.check_int(a, arg)?;
                // `%u` is the integer's bits as unsigned.
                let (neg, mut digits) =
                    if conv == b'u' { (false, (n as u64).to_string()) } else { (n < 0, n.unsigned_abs().to_string()) };
                apply_prec(&mut digits, spec.prec, n == 0);
                pad(out, sign_of(neg, &spec), &digits, &spec, spec.prec.is_none());
            }
            b'x' | b'X' | b'o' => {
                let n = lua.check_int(a, arg)? as u64;
                let mut digits = match conv {
                    b'x' => format!("{n:x}"),
                    b'X' => format!("{n:X}"),
                    _ => format!("{n:o}"),
                };
                apply_prec(&mut digits, spec.prec, n == 0);
                if spec.alt && conv == b'o' && !digits.starts_with('0') {
                    // `#` makes octal start with 0 (zero included).
                    digits.insert(0, '0');
                }
                let prefix = match conv {
                    b'x' if spec.alt && n != 0 => "0x",
                    b'X' if spec.alt && n != 0 => "0X",
                    _ => "",
                };
                pad(out, prefix, &digits, &spec, spec.prec.is_none());
            }
            b'a' | b'A' | b'e' | b'E' | b'f' | b'g' | b'G' => {
                let x = lua.check_num(a, arg)?;
                let upper = conv.is_ascii_uppercase();
                let body = match conv.to_ascii_lowercase() {
                    b'a' => {
                        let s = number::fmt_a(x, spec.prec, upper);
                        // '#' keeps the point even with no digits after it ("0x1.p+0").
                        if spec.alt && x.is_finite() && !s.contains('.') {
                            let (p, dp) = if upper { ("P", ".P") } else { ("p", ".p") };
                            s.replacen(p, dp, 1)
                        } else {
                            s
                        }
                    }
                    b'e' => number::fmt_e(x, spec.prec.unwrap_or(6), upper, spec.alt),
                    b'f' => number::fmt_f(x, spec.prec.unwrap_or(6), upper, spec.alt),
                    _ => number::fmt_g(x, spec.prec.unwrap_or(6), upper, spec.alt),
                };
                let (neg, body) = match body.strip_prefix('-') {
                    Some(rest) => (true, rest),
                    None => (false, body.as_str()),
                };
                let finite = x.is_finite();
                if conv.to_ascii_lowercase() == b'a' && spec.zero && finite {
                    // Zeros go after the "0x".
                    let (pre, digits) = body.split_at(2.min(body.len()));
                    let sign = format!("{}{pre}", sign_of(neg, &spec));
                    pad(out, &sign, digits, &spec, true);
                } else {
                    pad(out, sign_of(neg, &spec), body, &spec, finite);
                }
                // Long expansions (`%.99f` of 1e300) cost per digit made.
                lua.charge(crate::vm::FLOAT_TEXT_FUEL + body.len() as u64 / 2)?;
            }
            b'p' => {
                let v = lua.arg(a, arg);
                let s = match v {
                    Value::Nil | Value::Bool(_) | Value::Int(_) | Value::Num(_) => "(null)".to_owned(),
                    other => format!("0x{:08x}", crate::vm::address_of(other)),
                };
                pad(out, "", &s, &spec, false);
            }
            b'q' => {
                let v = lua.arg(a, arg);
                quote(lua, v, arg + 1, out)?;
            }
            b's' => {
                let v = lua.arg(a, arg);
                let s = lua.tostring(v)?;
                let r = match s {
                    Value::Str(r) => Some(r),
                    _ => None,
                };
                let len = r.map_or(0, |r| lua.bytes(r).len());
                let whole = body.is_empty() || (spec.prec.is_none() && len >= 100);
                // Only the bytes written are copied, checked and made room for: a precision
                // cuts the string (SEC7-N4: checked whole, `%.1s` of 9 MiB beside 9 MiB of
                // output raised a memory error C Lua does not). Counted while held, the room
                // paid before it is written (SEC-M14).
                let w = if whole { len } else { spec.prec.map_or(len, |p| p.min(len)) };
                let mut hold = super::MemHold::default();
                hold.cover(lua, w)?;
                let b = r.map_or_else(Vec::new, |r| lua.bytes(r).get(..w).unwrap_or_default().to_vec());
                lua.check_len(out.len().saturating_add(w))?;
                out.reserve(lua, w + 100)?;
                // Read whole (scanned for zeros) however little of it is written.
                lua.charge(len as u64 / 32)?;
                if !body.is_empty() {
                    // Checked before the long-string shortcut, as C Lua does (S5COR-10:
                    // `%028s` of a string of 100 bytes or more was accepted).
                    if r.is_some_and(|r| lua.bytes(r).contains(&0)) {
                        return Err(LuaError::Arg(arg + 1, "string contains zeros".into()));
                    }
                    check(b"-", true)?;
                }
                if whole {
                    out.extend_from_slice(&b);
                } else {
                    pad(out, "", &b, &spec, false);
                }
            }
            _ => {}
        }
        out.pay(lua)?;
        lua.check_len(out.len())?;
    }
    Ok(())
}

/// The bytes `%q` writes for byte `j` of `b` (as `quote` writes them).
fn escaped_len(b: &[u8], j: usize) -> usize {
    match b.get(j).copied().unwrap_or(0) {
        b'"' | b'\\' | b'\n' => 2,
        c if c.is_ascii_control() => {
            1 + if c >= 100 || b.get(j + 1).is_some_and(u8::is_ascii_digit) {
                3
            } else if c >= 10 {
                2
            } else {
                1
            }
        }
        _ => 1,
    }
}

/// `%q` of `v`, argument `pos` of `string.format` (for the error message).
fn quote(lua: &mut Lua, v: Value, pos: usize, out: &mut super::OutBuf) -> Result<(), LuaError> {
    match v {
        Value::Str(s) => {
            // Counted while held (SEC-M14).
            let mut hold = super::MemHold::default();
            hold.cover(lua, lua.bytes(s).len())?;
            let b = lua.bytes(s).to_vec();
            // Escaped byte by byte: a unit per 8 (calibrated: `examples/fuelcal.rs`).
            lua.charge(b.len() as u64 / 8)?;
            out.push(b'"');
            for (i, &c) in b.iter().enumerate() {
                // SEC-M4: the host's interrupt and deadline, a chunk at a time (16 MiB
                // ran 29 ms past the time limit).
                if i % (1 << 20) == 0 {
                    if i > 0 {
                        lua.check_time()?;
                    }
                    // The piece's escaped bytes paid for before they are written (SEC-M14).
                    let n: usize = (i..b.len().min(i + (1 << 20))).map(|j| escaped_len(&b, j)).sum();
                    lua.check_len(out.len().saturating_add(n))?;
                    out.reserve(lua, n + 2)?;
                }
                match c {
                    b'"' | b'\\' | b'\n' => {
                        out.push(b'\\');
                        out.push(c);
                    }
                    c if c.is_ascii_control() => {
                        // `\ddd`, as short as the next character allows, without an
                        // allocation per character (all control characters is the worst
                        // case).
                        let digits = [b'0' + c / 100, b'0' + c / 10 % 10, b'0' + c % 10];
                        let keep = if c >= 100 || b.get(i + 1).is_some_and(u8::is_ascii_digit) {
                            3
                        } else if c >= 10 {
                            2
                        } else {
                            1
                        };
                        out.push(b'\\');
                        out.extend_from_slice(digits.get(3 - keep..).unwrap_or_default());
                    }
                    c => out.push(c),
                }
            }
            out.push(b'"');
        }
        Value::Int(n) => {
            if n == i64::MIN {
                out.extend_from_slice(b"0x8000000000000000");
            } else {
                out.extend_from_slice(n.to_string().as_bytes());
            }
        }
        Value::Num(f) => {
            if f == f64::INFINITY {
                out.extend_from_slice(b"1e9999");
            } else if f == f64::NEG_INFINITY {
                out.extend_from_slice(b"-1e9999");
            } else if f.is_nan() {
                out.extend_from_slice(b"(0/0)");
            } else {
                // Hexadecimal, as Lua writes it: exact, and an integral float keeps its
                // float-ness ("%d.0"-style is not exact for big values).
                out.extend_from_slice(number::fmt_a(f, None, false).as_bytes());
            }
        }
        Value::Nil | Value::Bool(_) => {
            let s = lua.tostring(v)?;
            if let Value::Str(r) = s {
                out.extend_from_slice(lua.bytes(r));
            }
        }
        _ => return Err(LuaError::Arg(pos, "value has no literal form".into())),
    }
    Ok(())
}
