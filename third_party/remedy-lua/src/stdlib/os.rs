//! `os.time`, `os.clock`, `os.date`, `os.difftime` and `os.setlocale` (the `"C"` locale
//! only): the parts of `os` that reach nothing outside the script. Local time is UTC plus
//! the host's offset ([`Lua::set_utc_offset`](crate::Lua::set_utc_offset)). With
//! `Libs::ENV`, `os.getenv` (only the host's map, [`Lua::set_env`](crate::Lua::set_env)).

#[allow(unused_imports)]
use crate::nostd::prelude::*;
use super::R;
use crate::value::{TableRef, Value};
use crate::vm::{Args, Lua, LuaError};

pub(super) fn open(lua: &mut Lua) {
    let t = lua.lib_table("os");
    lua.set_fn(t, "time", "os.time", time);
    lua.set_fn(t, "clock", "os.clock", clock);
    lua.set_fn(t, "date", "os.date", date);
    lua.set_fn(t, "difftime", "os.difftime", difftime);
    lua.set_fn(t, "setlocale", "os.setlocale", setlocale);
}

/// `os.getenv` (`Libs::ENV`).
pub(super) fn open_env(lua: &mut Lua) {
    let t = lua.lib_table("os");
    lua.set_fn(t, "getenv", "os.getenv", getenv);
}

/// C's `os_getenv`: `getenv(luaL_checkstring(L, 1))`, here from the host's map. The name
/// ends at its first zero byte, as C's `getenv` reads it. Charged for the lookup and per 32
/// bytes of name and value.
fn getenv(lua: &mut Lua, a: Args) -> R {
    let name = lua.check_str(a, 0)?;
    let len = lua.bytes(name).split(|&c| c == 0).next().unwrap_or_default().len();
    lua.charge(4 + len as u64 / 32)?;
    // A name longer than any the host set is not hashed (a 16 MiB one would be, for a
    // few units of fuel).
    let found = if len > lua.env_name_max {
        None
    } else {
        lua.env.get(lua.bytes(name).get(..len).unwrap_or_default()).map(crate::nostd::sync::Arc::clone)
    };
    let Some(v) = found else {
        return lua.ret1(Value::Nil);
    };
    lua.charge(v.len() as u64 / 32)?;
    lua.ret_str(&v)
}

/// C's `os_setlocale` in a process whose locale is `"C"` and stays so: a query (no locale,
/// or `""`, the environment's, which is `"C"` here) and `"C"` or `"POSIX"` give `"C"`;
/// any other locale is one this installation lacks: nil, as C's `setlocale` gives. The
/// process locale is never changed (it is shared by every state in the process).
fn setlocale(lua: &mut Lua, a: Args) -> R {
    lua.charge(4)?;
    let l = lua.opt_str(a, 0)?;
    lua.check_option(a, 1, Some("all"), &["all", "collate", "ctype", "monetary", "numeric", "time"])?;
    let known = match l {
        None => true,
        // C reads the name up to its first zero byte.
        Some(l) => matches!(lua.bytes(l).split(|&c| c == 0).next().unwrap_or_default(), b"" | b"C" | b"POSIX"),
    };
    if known { lua.ret_str(b"C") } else { lua.ret1(Value::Nil) }
}

/// Days since 1970-01-01 of a civil date (proleptic Gregorian).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn now(lua: &Lua) -> i64 {
    lua.os_clocks.0.as_ref().map_or(0, |c| i64::try_from(c().as_secs()).unwrap_or(i64::MAX))
}

fn field(lua: &mut Lua, t: TableRef, k: &str, d: Option<i64>, delta: i64) -> Result<i64, LuaError> {
    // C's `lua_getfield`: through `__index`.
    let key = lua.new_str(k.as_bytes())?;
    let v = lua.index_nested(Value::Table(t), key)?;
    match v {
        Value::Nil => d.ok_or_else(|| LuaError::msg(format!("field '{k}' missing in date table"))),
        _ => {
            // C's `getfield`: any value but nil that is not an integer.
            let Some(n) = lua.tointeger(v) else {
                return Err(LuaError::msg(format!("field '{k}' is not an integer")));
            };
            if !(i64::from(i32::MIN) + delta..=i64::from(i32::MAX) + delta).contains(&n) {
                return Err(LuaError::msg(format!("field '{k}' is out-of-bound")));
            }
            Ok(n - delta)
        }
    }
}

fn time(lua: &mut Lua, a: Args) -> R {
    // Reading the clock, or seven fields read, nine written and the calendar arithmetic (calibrated with the
    // fuel-against-time property in `tests/sweep3_sec.rs`).
    lua.charge(if lua.arg(a, 0).is_nil() { 4 } else { 28 })?;
    let Value::Table(t) = lua.arg(a, 0) else {
        if !lua.arg(a, 0).is_nil() {
            return Err(lua.type_error(a, 0, "table"));
        }
        return lua.ret1(Value::Int(now(lua)));
    };
    // The bounds are C's `struct tm` ones: `tm_year` counts from 1900, `tm_mon` from 0.
    let year = field(lua, t, "year", None, 1900)? + 1900;
    let month = field(lua, t, "month", None, 1)? + 1;
    let day = field(lua, t, "day", None, 0)?;
    let hour = field(lua, t, "hour", Some(12), 0)?;
    let min = field(lua, t, "min", Some(0), 0)?;
    let sec = field(lua, t, "sec", Some(0), 0)?;
    // C's `getboolfield`: -1 for nil (mktime decides), else the value's truth.
    let isdst = {
        let key = lua.new_str(b"isdst")?;
        let v = lua.index_nested(Value::Table(t), key)?;
        if v.is_nil() { -1 } else { i32::from(v.truthy()) }
    };
    // Normalize months, then let days, hours… carry over.
    let m0 = month - 1;
    let y = year + m0.div_euclid(12);
    let m = m0.rem_euclid(12) + 1;
    let days = days_from_civil(y, m, 1) + day - 1;
    let secs = days
        .checked_mul(86_400)
        .and_then(|s| s.checked_add(hour * 3600 + min * 60 + sec))
        .and_then(|s| s.checked_sub(lua.utc_offset))
        // Local time here has no daylight saving time. Given `isdst = true` anyway,
        // glibc's `mktime` (C Lua's on Linux) takes the table as an hour ahead of
        // standard time: an hour less.
        .and_then(|s| if isdst > 0 { s.checked_sub(3600) } else { Some(s) })
        .filter(|t| t.unsigned_abs() < 1 << 55)
        .ok_or_else(|| LuaError::msg("time result cannot be represented in this installation"))?;
    // Update the table's fields to their normalized values, as mktime does.
    let (ny, nm, nd, nh, nmin, nsec, wday, yday) = broken_down(secs + lua.utc_offset);
    // C's `setallfields`: in its order, through `__newindex` (`lua_setfield`), and
    // `isdst` (false: the normalized time is standard time).
    for (k, v) in [("year", ny), ("month", nm), ("day", nd), ("hour", nh), ("min", nmin), ("sec", nsec), ("yday", yday), ("wday", wday)] {
        let key = lua.new_str(k.as_bytes())?;
        lua.set_index_nested(Value::Table(t), key, Value::Int(v))?;
    }
    let key = lua.new_str(b"isdst")?;
    lua.set_index_nested(Value::Table(t), key, Value::Bool(false))?;
    lua.charge_probes()?;
    lua.ret1(Value::Int(secs))
}

#[allow(clippy::type_complexity, reason = "the fields of a broken-down time")]
fn broken_down(t: i64) -> (i64, i64, i64, i64, i64, i64, i64, i64) {
    let days = t.div_euclid(86_400);
    let rem = t.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    let wday = (days + 4).rem_euclid(7) + 1;
    let yday = days - days_from_civil(y, 1, 1) + 1;
    (y, m, d, rem / 3600, rem % 3600 / 60, rem % 60, wday, yday)
}

fn clock(lua: &mut Lua, _a: Args) -> R {
    let s = lua.os_clocks.1.as_ref().map_or(0.0, |c| c().as_secs_f64());
    lua.ret1(Value::Num(s))
}

fn difftime(lua: &mut Lua, a: Args) -> R {
    let t1 = lua.check_int(a, 0)?;
    let t2 = lua.check_int(a, 1)?; // 5.4: both required
    lua.ret1(Value::Num(t1 as f64 - t2 as f64))
}

const DAYS: [&str; 7] = ["Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday"];
const MONTHS: [&str; 12] =
    ["January", "February", "March", "April", "May", "June", "July", "August", "September", "October", "November", "December"];

fn date(lua: &mut Lua, a: Args) -> R {
    // Calendar arithmetic and formatting: a few dozen instructions' worth.
    lua.charge(16)?;
    // The format (copied once) and the output, counted while held, the output's room
    // before it is written (SEC-M14).
    let (mut hold, mut held_out) = (super::MemHold::default(), super::MemHold::default());
    let fmt = match lua.opt_str(a, 0)? {
        Some(s) => {
            hold.cover(lua, lua.bytes(s).len())?;
            lua.bytes(s).to_vec()
        }
        None => b"%c".to_vec(),
    };
    let t = if lua.arg(a, 1).is_nil() { now(lua) } else { lua.check_int(a, 1)? };
    let (utc, f) = match fmt.strip_prefix(b"!") {
        Some(rest) => (true, rest),
        None => (false, fmt.as_slice()),
    };
    // Each format byte costs about 2 instructions (a conversion formats a piece), charged
    // a block at a time as it goes so a long format stops on fuel or the interrupt.
    const BLOCK: usize = 1024;
    lua.charge(2 * (f.len() % BLOCK) as u64)?;
    let limit = lua.limits.string_len;
    let local = if utc { Some(t) } else { t.checked_add(lua.utc_offset) };
    // Beyond what a calendar date can show.
    let Some(local) = local.filter(|t| t.unsigned_abs() < 1 << 55) else {
        return Err(LuaError::msg("date result cannot be represented in this installation"));
    };
    let (y, m, d, h, mi, s, wday, yday) = broken_down(local);
    if f.starts_with(b"*t") {
        let tab = lua.new_table(0, 9)?;
        for (k, v) in [("year", y), ("month", m), ("day", d), ("hour", h), ("min", mi), ("sec", s), ("wday", wday), ("yday", yday)] {
            let key = lua.new_str(k.as_bytes())?;
            lua.raw_set(tab, key, Value::Int(v))?;
        }
        let key = lua.new_str(b"isdst")?;
        lua.raw_set(tab, key, Value::Bool(false))?;
        return lua.ret1(Value::Table(tab));
    }
    let dayname = DAYS.get((wday - 1) as usize).copied().unwrap_or("");
    let monname = MONTHS.get((m - 1) as usize).copied().unwrap_or("");
    let h12 = if h % 12 == 0 { 12 } else { h % 12 };
    let mut out = Vec::new();
    let mut i = 0;
    // The whole blocks are charged as they are reached, the rest was charged above.
    let mut next_block = 0;
    let whole = f.len() - f.len() % BLOCK;
    while i < f.len() {
        if out.len() > limit {
            return Err(LuaError::Limit(crate::LimitKind::Memory));
        }
        if i >= next_block && next_block < whole {
            next_block += BLOCK;
            lua.charge(2 * BLOCK as u64)?;
        }
        let c = f.get(i).copied().unwrap_or(0);
        i += 1;
        if c != b'%' {
            // Bytes, as `strftime` copies them (not each made a UTF-8 character).
            held_out.reserve(lua, &mut out, 1)?;
            out.push(c);
            continue;
        }
        let mut conv = f.get(i).copied().unwrap_or(0);
        i += 1;
        if conv == b'E' || conv == b'O' {
            let next = f.get(i).copied().unwrap_or(0);
            let ok = if conv == b'E' { b"cCxXyY".contains(&next) } else { b"deHImMSuUVwWy".contains(&next) };
            if !ok {
                return Err(bad_conversion(lua, f.get(i - 1..).unwrap_or_default()));
            }
            conv = next;
            i += 1;
        }
        let piece = match conv {
            b'a' => dayname.get(..3).unwrap_or("").to_owned(),
            b'A' => dayname.to_owned(),
            b'b' | b'h' => monname.get(..3).unwrap_or("").to_owned(),
            b'B' => monname.to_owned(),
            b'c' => format!(
                "{} {} {:2} {:02}:{:02}:{:02} {}",
                dayname.get(..3).unwrap_or(""),
                monname.get(..3).unwrap_or(""),
                d,
                h,
                mi,
                s,
                y
            ),
            b'C' => format!("{:02}", y / 100),
            b'd' => format!("{d:02}"),
            b'D' | b'x' => format!("{m:02}/{d:02}/{:02}", y % 100),
            b'e' => format!("{d:2}"),
            b'F' => format!("{y}-{m:02}-{d:02}"),
            b'H' => format!("{h:02}"),
            b'I' => format!("{h12:02}"),
            b'j' => format!("{yday:03}"),
            b'm' => format!("{m:02}"),
            b'M' => format!("{mi:02}"),
            b'n' => "\n".into(),
            b'p' => if h < 12 { "AM" } else { "PM" }.into(),
            b'r' => format!("{h12:02}:{mi:02}:{s:02} {}", if h < 12 { "AM" } else { "PM" }),
            b'R' => format!("{h:02}:{mi:02}"),
            b'S' => format!("{s:02}"),
            b't' => "\t".into(),
            b'T' | b'X' => format!("{h:02}:{mi:02}:{s:02}"),
            b'u' => format!("{}", if wday == 1 { 7 } else { wday - 1 }),
            b'w' => format!("{}", wday - 1),
            b'U' => format!("{:02}", (yday - 1 + 7 - (wday - 1)) / 7),
            b'W' => format!("{:02}", (yday - 1 + 7 - (wday + 5) % 7) / 7),
            b'G' | b'g' | b'V' => {
                let (iy, iw) = iso_week(y, yday, wday);
                match conv {
                    b'G' => format!("{iy}"),
                    b'g' => format!("{:02}", iy.rem_euclid(100)),
                    _ => format!("{iw:02}"),
                }
            }
            b'y' => format!("{:02}", y % 100),
            b'Y' => format!("{y}"),
            b'z' => {
                let o = if utc { 0 } else { lua.utc_offset };
                format!("{}{:02}{:02}", if o < 0 { '-' } else { '+' }, o.abs() / 3600, o.abs() % 3600 / 60)
            }
            b'Z' => if utc || lua.utc_offset == 0 { "UTC" } else { "" }.into(),
            b'%' => "%".into(),
            _ => {
                return Err(bad_conversion(lua, f.get(i - 1..).unwrap_or_default()));
            }
        };
        held_out.reserve(lua, &mut out, piece.len())?;
        out.extend_from_slice(piece.as_bytes());
    }
    lua.ret_str(&out)
}

/// The ISO 8601 week-numbering year and week of a date (`yday` from 1, `wday` from 1 =
/// Sunday).
fn iso_week(y: i64, yday: i64, wday: i64) -> (i64, i64) {
    let wd = (wday + 5) % 7 + 1; // Monday 1 … Sunday 7
    let w = (yday - wd + 10) / 7;
    let leap = |y: i64| (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
    let weeks_in = |y: i64| {
        // A year has 53 weeks when it starts on a Thursday (or Wednesday in a leap year).
        let jan1 = (days_from_civil(y, 1, 1) + 4).rem_euclid(7); // 0 = Sunday
        if jan1 == 4 || (jan1 == 3 && leap(y)) { 53 } else { 52 }
    };
    if w < 1 {
        (y - 1, weeks_in(y - 1))
    } else if w > weeks_in(y) {
        (y + 1, 1)
    } else {
        (y, w)
    }
}

/// C Lua's message for a bad `os.date` conversion: the format from it to the end (a C
/// string, so up to a NUL byte).
fn bad_conversion(lua: &mut Lua, rest: &[u8]) -> LuaError {
    let rest = rest.split(|&c| c == 0).next().unwrap_or_default();
    // The bytes as they are (S5COR-3), not converted to UTF-8.
    let msg = [b"invalid conversion specifier '%".as_slice(), rest, b"'"].concat();
    match lua.arg_error_bytes(1, &msg) {
        Ok(v) => LuaError::Value(v),
        Err(e) => e,
    }
}
