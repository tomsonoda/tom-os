//! Numbers: text to number and back the way Lua 5.4 does it, and the integer/float rules.

#[allow(unused_imports)]
use crate::nostd::prelude::*;
/// 2^63 as a float: the first float past the integers.
const TWO63: f64 = 9_223_372_036_854_775_808.0;

/// `f` as an integer when it has an exact integer value in range.
pub(crate) fn float_to_int(f: f64) -> Option<i64> {
    if f.floor() == f && (-TWO63..TWO63).contains(&f) {
        #[allow(clippy::cast_possible_truncation, reason = "checked integral and in range just above")]
        Some(f as i64)
    } else {
        None
    }
}

/// `floor(f)` as an integer when in range.
pub(crate) fn float_floor_to_int(f: f64) -> Option<i64> {
    float_to_int(f.floor())
}

/// `ceil(f)` as an integer when in range.
pub(crate) fn float_ceil_to_int(f: f64) -> Option<i64> {
    float_to_int(f.ceil())
}

/// `i == f`, exactly.
pub(crate) fn int_eq_float(i: i64, f: f64) -> bool {
    float_to_int(f) == Some(i)
}

/// `i < f`, exactly.
pub(crate) fn int_lt_float(i: i64, f: f64) -> bool {
    if f.is_nan() {
        return false;
    }
    if f >= TWO63 {
        return true;
    }
    if f <= -TWO63 {
        return false;
    }
    match float_ceil_to_int(f) {
        Some(c) => i < c,
        None => false,
    }
}

/// `i <= f`, exactly.
pub(crate) fn int_le_float(i: i64, f: f64) -> bool {
    if f.is_nan() {
        return false;
    }
    if f >= TWO63 {
        return true;
    }
    if f < -TWO63 {
        return false;
    }
    match float_floor_to_int(f) {
        Some(c) => i <= c,
        None => false,
    }
}

/// `f < i`, exactly.
pub(crate) fn float_lt_int(f: f64, i: i64) -> bool {
    if f.is_nan() {
        return false;
    }
    if f >= TWO63 {
        return false;
    }
    if f < -TWO63 {
        return true;
    }
    match float_floor_to_int(f) {
        Some(c) => c < i,
        None => false,
    }
}

/// `f <= i`, exactly.
pub(crate) fn float_le_int(f: f64, i: i64) -> bool {
    if f.is_nan() {
        return false;
    }
    if f >= TWO63 {
        return false;
    }
    if f <= -TWO63 {
        return true;
    }
    match float_ceil_to_int(f) {
        Some(c) => c <= i,
        None => false,
    }
}

/// Floor division of integers (`b != 0`).
pub(crate) fn int_floor_div(a: i64, b: i64) -> i64 {
    if b == -1 {
        return a.wrapping_neg();
    }
    let q = a / b;
    if (a % b != 0) && ((a < 0) != (b < 0)) { q - 1 } else { q }
}

/// Lua's integer modulo (`b != 0`): the result has the sign of `b`.
pub(crate) fn int_mod(a: i64, b: i64) -> i64 {
    if b == -1 {
        return 0;
    }
    let m = a % b;
    if m != 0 && ((m ^ b) < 0) { m + b } else { m }
}

/// Lua's float modulo.
pub(crate) fn float_mod(a: f64, b: f64) -> f64 {
    let m = a % b;
    if if m > 0.0 { b < 0.0 } else { m < 0.0 && b > 0.0 } { m + b } else { m }
}

/// Lua's shift left (negative shifts go right; 64 or more gives 0).
pub(crate) fn shift_left(x: i64, n: i64) -> i64 {
    if n <= -64 || n >= 64 {
        0
    } else if n >= 0 {
        ((x as u64) << n) as i64
    } else {
        ((x as u64) >> (-n)) as i64
    }
}

/// A float as `%.14g` prints it, with `.0` added when it looks like an integer: how
/// `tostring` writes floats.
pub(crate) fn fmt_float(f: f64) -> String {
    let mut s = fmt_g(f, 14, false, false);
    if s.bytes().all(|b| b == b'-' || b.is_ascii_digit()) {
        s.push_str(".0");
    }
    s
}

/// Infinity and NaN as C's printf writes them.
fn non_finite(f: f64, upper: bool) -> Option<String> {
    let s = if f.is_nan() {
        if f.is_sign_negative() { "-nan" } else { "nan" }
    } else if f.is_infinite() {
        if f < 0.0 { "-inf" } else { "inf" }
    } else {
        return None;
    };
    Some(if upper { s.to_ascii_uppercase() } else { s.to_owned() })
}

/// `%.{prec}g` (`alt`: the `#` flag, keeping trailing zeros).
pub(crate) fn fmt_g(f: f64, prec: usize, upper: bool, alt: bool) -> String {
    if let Some(s) = non_finite(f, upper) {
        return s;
    }
    let p = if prec == 0 { 1 } else { prec.min(400) };
    if f == 0.0 {
        let mut s = String::from(if f.is_sign_negative() { "-0" } else { "0" });
        if alt {
            s.push('.');
            s.extend(crate::nostd::iter::repeat_n('0', p - 1));
        }
        return s;
    }
    // The exponent after rounding to p significant digits.
    let mut out = with_fmt(format_args!("{:.*e}", p - 1, f), |sci| {
        let (mant, exp) = sci.split_once('e').unwrap_or((sci, "0"));
        let exp: i32 = exp.parse().unwrap_or(0);
        if exp < -4 || exp >= p as i32 {
            let mant = if alt { mant } else { strip_zeros(mant) };
            with_exponent(mant, exp, upper)
        } else {
            // Fixed notation from the same p rounded digits (C's %g: p - 1 - exp decimals),
            // without formatting the number a second time.
            let mut s = String::with_capacity(p + 8);
            if f < 0.0 {
                s.push('-');
            }
            let digits = mant.bytes().filter(u8::is_ascii_digit).map(char::from);
            if exp < 0 {
                s.push_str("0.");
                s.extend(crate::nostd::iter::repeat_n('0', exp.unsigned_abs() as usize - 1));
                s.extend(digits);
            } else {
                let int_len = exp as usize + 1;
                for (i, d) in digits.enumerate() {
                    if i == int_len {
                        s.push('.');
                    }
                    s.push(d);
                }
            }
            if !alt && s.contains('.') {
                s.truncate(s.trim_end_matches('0').trim_end_matches('.').len());
            }
            s
        }
    });
    if alt && !out.contains('.') {
        // `#` keeps the decimal point.
        match out.find(['e', 'E']) {
            Some(i) => out.insert(i, '.'),
            None => out.push('.'),
        }
    }
    out
}

fn strip_zeros(s: &str) -> &str {
    if s.contains('.') { s.trim_end_matches('0').trim_end_matches('.') } else { s }
}

/// `mant` then C's exponent: `e`, a sign and at least two digits.
fn with_exponent(mant: &str, exp: i32, upper: bool) -> String {
    use crate::nostd::fmt::Write as _;
    let mut s = String::with_capacity(mant.len() + 6);
    s.push_str(mant);
    s.push(if upper { 'E' } else { 'e' });
    s.push(if exp < 0 { '-' } else { '+' });
    let _ = write!(s, "{:02}", exp.unsigned_abs());
    s
}

/// Formats into a stack buffer and hands the text to `f` (a `String` only when it is
/// longer): `format!` grows its `String` piece by piece, several allocations a number.
fn with_fmt<R>(args: crate::nostd::fmt::Arguments<'_>, f: impl FnOnce(&str) -> R) -> R {
    use crate::nostd::io::Write as _;
    let mut buf = [0u8; 512];
    let n = {
        let mut w: &mut [u8] = &mut buf;
        w.write_fmt(args).ok().map(|()| 512 - w.len())
    };
    match n.and_then(|n| buf.get(..n)).and_then(|b| crate::nostd::str::from_utf8(b).ok()) {
        Some(s) => f(s),
        None => f(&crate::nostd::fmt::format(args)),
    }
}

/// `%.{prec}e`.
pub(crate) fn fmt_e(f: f64, prec: usize, upper: bool, alt: bool) -> String {
    if let Some(s) = non_finite(f, upper) {
        return s;
    }
    with_fmt(format_args!("{:.*e}", prec.min(400), f), |sci| {
        let (mant, exp) = sci.split_once('e').unwrap_or((sci, "0"));
        let exp: i32 = exp.parse().unwrap_or(0);
        if alt && !mant.contains('.') {
            with_exponent(&format!("{mant}."), exp, upper)
        } else {
            with_exponent(mant, exp, upper)
        }
    })
}

/// `%.{prec}f`.
pub(crate) fn fmt_f(f: f64, prec: usize, upper: bool, alt: bool) -> String {
    if let Some(s) = non_finite(f, upper) {
        return s;
    }
    with_fmt(format_args!("{:.*}", prec.min(400), f), |t| {
        let mut s = String::with_capacity(t.len() + 1);
        s.push_str(t);
        if alt && !s.contains('.') {
            s.push('.');
        }
        s
    })
}

/// `%a`: a float in hexadecimal, as C writes it (`0x1.8p+1`).
pub(crate) fn fmt_a(f: f64, prec: Option<usize>, upper: bool) -> String {
    if let Some(s) = non_finite(f, upper) {
        return s;
    }
    let bits = f.to_bits();
    let neg = bits >> 63 != 0;
    let mut exp = ((bits >> 52) & 0x7ff) as i64;
    let mut mant = bits & ((1u64 << 52) - 1);
    let lead: u8;
    if exp == 0 {
        lead = 0;
        exp = if mant == 0 { 0 } else { -1022 };
    } else {
        lead = 1;
        exp -= 1023;
    }
    // 13 hex digits of mantissa.
    let mut digits: Vec<u8> = (0..13).rev().map(|i| ((mant >> (i * 4)) & 0xf) as u8).collect();
    let mut lead = lead;
    match prec {
        Some(p) if p < 13 => {
            // Round to nearest, ties to even, at digit p.
            let shift = (13 - p) * 4;
            let full = (u64::from(lead) << 52) | mant;
            let rem = full & ((1u64 << shift) - 1);
            let half = 1u64 << (shift - 1);
            let mut kept = full >> shift;
            if rem > half || (rem == half && kept & 1 == 1) {
                kept += 1;
            }
            let back = kept << shift;
            lead = ((back >> 52) & 0xf) as u8;
            mant = back & ((1u64 << 52) - 1);
            digits = (0..13).rev().map(|i| ((mant >> (i * 4)) & 0xf) as u8).collect();
            digits.truncate(p);
        }
        Some(p) => digits.resize(p, 0),
        None => {
            while digits.last() == Some(&0) {
                digits.pop();
            }
        }
    }
    let hex = if upper { b"0123456789ABCDEF" } else { b"0123456789abcdef" };
    let mut s = String::new();
    if neg {
        s.push('-');
    }
    s.push_str(if upper { "0X" } else { "0x" });
    s.push(char::from(b'0' + lead));
    if !digits.is_empty() {
        s.push('.');
        for d in digits {
            s.push(char::from(hex.get(usize::from(d)).copied().unwrap_or(b'0')));
        }
    }
    s.push(if upper { 'P' } else { 'p' });
    s.push(if exp < 0 { '-' } else { '+' });
    s.push_str(&exp.abs().to_string());
    s
}

fn is_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c)
}

fn trim(s: &[u8]) -> &[u8] {
    let start = s.iter().position(|&b| !is_space(b)).unwrap_or(s.len());
    let end = s.iter().rposition(|&b| !is_space(b)).map_or(start, |e| e + 1);
    s.get(start..end).unwrap_or_default()
}

fn hex_digit(b: u8) -> Option<u32> {
    char::from(b).to_digit(16)
}

/// The number text means, as Lua reads numerals in strings (`tonumber`, arithmetic on
/// strings): decimal or hexadecimal integers (hexadecimal ones wrap around), decimal or
/// hexadecimal floats, surrounding spaces allowed. An integer numeral too big for an
/// integer is a float.
pub(crate) fn str_to_number(s: &[u8]) -> Option<crate::Value> {
    let t = trim(s);
    if t.is_empty() {
        return None;
    }
    if let Some(i) = str_to_int(t) {
        return Some(crate::Value::Int(i));
    }
    str_to_float(t).map(crate::Value::Num)
}

fn str_to_int(t: &[u8]) -> Option<i64> {
    let (neg, rest) = match t.first() {
        Some(b'-') => (true, t.get(1..)?),
        Some(b'+') => (false, t.get(1..)?),
        _ => (false, t),
    };
    let mut a: u64 = 0;
    if rest.len() > 2 && (rest.starts_with(b"0x") || rest.starts_with(b"0X")) {
        for &b in rest.get(2..)? {
            a = a.wrapping_mul(16).wrapping_add(u64::from(hex_digit(b)?));
        }
    } else {
        if rest.is_empty() {
            return None;
        }
        for &b in rest {
            if !b.is_ascii_digit() {
                return None;
            }
            let d = u64::from(b - b'0');
            // Overflow: not an integer (it reads as a float).
            if a >= 922_337_203_685_477_580 && (a > 922_337_203_685_477_580 || d > 7 + u64::from(neg)) {
                return None;
            }
            a = a * 10 + d;
        }
    }
    Some(if neg { (a as i64).wrapping_neg() } else { a as i64 })
}

fn str_to_float(t: &[u8]) -> Option<f64> {
    // `inf` and `nan` are not Lua numerals.
    if t.iter().any(|&b| b == b'n' || b == b'N') {
        return None;
    }
    let (neg, rest) = match t.first() {
        Some(b'-') => (true, t.get(1..)?),
        Some(b'+') => (false, t.get(1..)?),
        _ => (false, t),
    };
    let v = if rest.len() > 1 && (rest.starts_with(b"0x") || rest.starts_with(b"0X")) {
        hex_float(rest.get(2..)?)?
    } else {
        dec_float(rest)?
    };
    Some(if neg { -v } else { v })
}

fn dec_float(s: &[u8]) -> Option<f64> {
    // Digits, an optional point and digits, an optional exponent: what strtod takes in the
    // C locale, without its `inf`/`nan`/hex forms (handled elsewhere).
    let mut i = 0;
    let mut digits = 0;
    while s.get(i).is_some_and(u8::is_ascii_digit) {
        i += 1;
        digits += 1;
    }
    if s.get(i) == Some(&b'.') {
        i += 1;
        while s.get(i).is_some_and(u8::is_ascii_digit) {
            i += 1;
            digits += 1;
        }
    }
    if digits == 0 {
        return None;
    }
    if matches!(s.get(i), Some(b'e' | b'E')) {
        i += 1;
        if matches!(s.get(i), Some(b'+' | b'-')) {
            i += 1;
        }
        let start = i;
        while s.get(i).is_some_and(u8::is_ascii_digit) {
            i += 1;
        }
        if i == start {
            return None;
        }
    }
    if i != s.len() {
        return None;
    }
    let text = crate::nostd::str::from_utf8(s).ok()?;
    // Rust does not take a bare trailing point ("5.") or a leading one (".5")? It takes both.
    text.parse::<f64>().ok()
}

/// A hexadecimal float after its `0x`: digits, an optional point and digits, an optional
/// binary exponent `p±n`.
pub(crate) fn hex_float(s: &[u8]) -> Option<f64> {
    let mut mant: u64 = 0;
    let mut exp: i64 = 0;
    let mut any = false;
    let mut i = 0;
    let mut seen_point = false;
    // Significant digits past what fits shift the exponent instead.
    while let Some(&b) = s.get(i) {
        if b == b'.' {
            if seen_point {
                return None;
            }
            seen_point = true;
        } else if let Some(d) = hex_digit(b) {
            any = true;
            if mant >> 56 == 0 {
                mant = mant * 16 + u64::from(d);
                if seen_point {
                    exp -= 4;
                }
            } else {
                // A dropped digit still decides the rounding: keep it as a sticky bit
                // (bit 0 lies at least 4 bits below the 53 kept).
                mant |= u64::from(d != 0);
                if !seen_point {
                    exp += 4;
                }
            }
        } else {
            break;
        }
        i += 1;
    }
    if !any {
        return None;
    }
    if matches!(s.get(i), Some(b'p' | b'P')) {
        i += 1;
        let neg = match s.get(i) {
            Some(b'-') => {
                i += 1;
                true
            }
            Some(b'+') => {
                i += 1;
                false
            }
            _ => false,
        };
        let start = i;
        let mut e: i64 = 0;
        while let Some(&b) = s.get(i) {
            if !b.is_ascii_digit() {
                break;
            }
            e = (e * 10 + i64::from(b - b'0')).min(1 << 20);
            i += 1;
        }
        if i == start {
            return None;
        }
        exp += if neg { -e } else { e };
    }
    if i != s.len() {
        return None;
    }
    Some(round_to_f64(mant, exp))
}

/// `mant * 2^exp` rounded once, to nearest with ties to even, as `strtod` does (subnormal
/// results included: converting `mant` first and scaling after would round twice).
fn round_to_f64(mant: u64, exp: i64) -> f64 {
    if mant == 0 {
        return 0.0;
    }
    let len = i64::from(64 - mant.leading_zeros());
    let top = len - 1 + exp;
    if top > 1023 {
        return f64::INFINITY;
    }
    // Bits the result can hold: 53, fewer below the normal range.
    let prec = if top >= -1022 { 53 } else { 53 - (-1022 - top) };
    let drop = len - prec;
    if drop <= 0 {
        return ldexp(mant as f64, exp);
    }
    if drop > 65 {
        return 0.0;
    }
    let m = u128::from(mant);
    let mut kept = m >> drop;
    let rem = m & ((1u128 << drop) - 1);
    let half = 1u128 << (drop - 1);
    if rem > half || (rem == half && kept & 1 == 1) {
        kept += 1;
    }
    ldexp(kept as f64, exp + drop)
}

/// `x * 2^e` without overflowing in between.
pub(crate) fn ldexp(x: f64, e: i64) -> f64 {
    let mut x = x;
    let mut e = e.clamp(-4000, 4000);
    while e > 1000 {
        x *= 2f64.powi(1000);
        e -= 1000;
    }
    while e < -1000 {
        x *= 2f64.powi(-1000);
        e += 1000;
    }
    #[allow(clippy::cast_possible_truncation, reason = "clamped to ±1000 above")]
    let e = e as i32;
    x * 2f64.powi(e)
}

/// An integer in `base` (2 to 36), as `tonumber(s, base)` reads it.
pub(crate) fn str_to_int_base(s: &[u8], base: u32) -> Option<i64> {
    let t = trim(s);
    let (neg, rest) = match t.first() {
        Some(b'-') => (true, t.get(1..)?),
        Some(b'+') => (false, t.get(1..)?),
        _ => (false, t),
    };
    if rest.is_empty() {
        return None;
    }
    let mut n: i64 = 0;
    for &b in rest {
        let d = char::from(b).to_digit(36)?;
        if d >= base {
            return None;
        }
        n = n.wrapping_mul(i64::from(base)).wrapping_add(i64::from(d));
    }
    Some(if neg { n.wrapping_neg() } else { n })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn floats_print_like_lua() {
        assert_eq!(fmt_float(1.0), "1.0");
        assert_eq!(fmt_float(0.1), "0.1");
        assert_eq!(fmt_float(1e100), "1e+100");
        assert_eq!(fmt_float(-0.0), "-0.0");
        assert_eq!(fmt_float(123456789012345.0), "1.2345678901234e+14");
        assert_eq!(fmt_float(3.14159265358979), "3.1415926535898");
        assert_eq!(fmt_float(f64::INFINITY), "inf");
        assert_eq!(fmt_float(2.0f64.powi(63)), "9.2233720368548e+18");
        assert_eq!(fmt_g(0.0001, 6, false, false), "0.0001");
        assert_eq!(fmt_g(0.00001, 6, false, false), "1e-05");
        assert_eq!(fmt_a(1.0, None, false), "0x1p+0");
        assert_eq!(fmt_a(3.0, None, false), "0x1.8p+1");
        assert_eq!(fmt_a(0.0, None, false), "0x0p+0");
    }

    #[test]
    fn numerals_read_like_lua() {
        use crate::Value;
        let n = |s: &str| str_to_number(s.as_bytes());
        assert!(matches!(n(" 10 "), Some(Value::Int(10))));
        assert!(matches!(n("0x10"), Some(Value::Int(16))));
        assert!(matches!(n("0xffffffffffffffff"), Some(Value::Int(-1))));
        assert!(matches!(n("9223372036854775807"), Some(Value::Int(i64::MAX))));
        assert!(matches!(n("-9223372036854775808"), Some(Value::Int(i64::MIN))));
        assert!(matches!(n("9223372036854775808"), Some(Value::Num(_))));
        assert!(matches!(n("1e2"), Some(Value::Num(f)) if f == 100.0));
        assert!(matches!(n("0x1p4"), Some(Value::Num(f)) if f == 16.0));
        assert!(matches!(n(".5"), Some(Value::Num(f)) if f == 0.5));
        assert!(matches!(n("5."), Some(Value::Num(f)) if f == 5.0));
        assert!(n("inf").is_none() && n("nan").is_none() && n("1e").is_none() && n("").is_none() && n("0x").is_none());
    }
}
