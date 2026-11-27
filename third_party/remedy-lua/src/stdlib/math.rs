//! The math library (with Lua 5.4's xoshiro256** generator).

#[allow(unused_imports)]
use crate::nostd::prelude::*;
use super::R;
use crate::number;
use crate::value::Value;
use crate::vm::{Args, Lua, LuaError};

pub(super) fn open(lua: &mut Lua) {
    let t = lua.lib_table("math");
    let fns: [(&str, &'static str, crate::vm::NativeFn); 23] = [
        ("abs", "math.abs", abs),
        ("ceil", "math.ceil", |l, a| round(l, a, f64::ceil)),
        ("floor", "math.floor", |l, a| round(l, a, f64::floor)),
        ("sqrt", "math.sqrt", |l, a| unary_f(l, a, f64::sqrt)),
        ("sin", "math.sin", |l, a| unary_f(l, a, f64::sin)),
        ("cos", "math.cos", |l, a| unary_f(l, a, f64::cos)),
        ("tan", "math.tan", |l, a| unary_f(l, a, f64::tan)),
        ("asin", "math.asin", |l, a| unary_f(l, a, f64::asin)),
        ("acos", "math.acos", |l, a| unary_f(l, a, f64::acos)),
        ("exp", "math.exp", |l, a| unary_f(l, a, f64::exp)),
        ("atan", "math.atan", atan),
        ("log", "math.log", log),
        ("fmod", "math.fmod", fmod),
        ("modf", "math.modf", modf),
        ("tointeger", "math.tointeger", tointeger),
        ("type", "math.type", type_),
        ("ult", "math.ult", ult),
        ("max", "math.max", |l, a| extreme(l, a, false)),
        ("min", "math.min", |l, a| extreme(l, a, true)),
        ("random", "math.random", random),
        ("randomseed", "math.randomseed", randomseed),
        ("deg", "math.deg", |l, a| unary_f(l, a, |x| x * (180.0 / crate::nostd::f64::consts::PI))),
        ("rad", "math.rad", |l, a| unary_f(l, a, |x| x * (crate::nostd::f64::consts::PI / 180.0))),
    ];
    for (n, q, f) in fns {
        lua.set_fn(t, n, q, f);
    }
    // `LUA_COMPAT_MATHLIB`, on in stock 5.4 builds (`LUA_COMPAT_5_3`).
    let compat: [(&str, &'static str, crate::vm::NativeFn); 8] = [
        ("atan2", "math.atan2", atan),
        ("cosh", "math.cosh", |l, a| unary_f(l, a, f64::cosh)),
        ("sinh", "math.sinh", |l, a| unary_f(l, a, f64::sinh)),
        ("tanh", "math.tanh", |l, a| unary_f(l, a, f64::tanh)),
        ("pow", "math.pow", |l, a| {
            let (x, y) = (l.check_num(a, 0)?, l.check_num(a, 1)?);
            l.ret1(Value::Num(x.powf(y)))
        }),
        ("frexp", "math.frexp", frexp),
        ("ldexp", "math.ldexp", |l, a| {
            let x = l.check_num(a, 0)?;
            // C: `(int)luaL_checkinteger`, so the exponent wraps to 32 bits.
            #[allow(clippy::cast_possible_truncation, reason = "C's cast to int")]
            let e = l.check_int(a, 1)? as i32;
            l.ret1(Value::Num(number::ldexp(x, i64::from(e))))
        }),
        ("log10", "math.log10", |l, a| unary_f(l, a, f64::log10)),
    ];
    if !lua.limits.strict_5_4 {
        for (n, q, f) in compat {
            lua.set_fn(t, n, q, f);
        }
    }
    // The functions the fast loop runs in place, by identity (see `fast::math_call`).
    for (i, n) in crate::fast::MATH_FAST.iter().enumerate() {
        let f = lua.new_str(n.as_bytes()).map_or(Value::Nil, |k| lua.raw_get(t, k));
        if let Some(slot) = lua.names.math_fast.get_mut(i) {
            *slot = f.as_func();
        }
    }
    lua.names.math_mask = crate::fast::math_mask(&lua.names.math_fast);
    lua.ctx_epoch += 1;
    lua.set_field(t, "pi", Value::Num(crate::nostd::f64::consts::PI));
    lua.set_field(t, "huge", Value::Num(f64::INFINITY));
    lua.set_field(t, "maxinteger", Value::Int(i64::MAX));
    lua.set_field(t, "mininteger", Value::Int(i64::MIN));
}

/// C's `frexp`: `x = m * 2^e` with `0.5 <= |m| < 1` (zero, infinities and NaN as they are,
/// with `e = 0`).
fn frexp(lua: &mut Lua, a: Args) -> R {
    let x = lua.check_num(a, 0)?;
    let (m, e) = if x == 0.0 || !x.is_finite() {
        (x, 0)
    } else {
        // Subnormals: scale into the normal range first.
        let (y, adj) = if x.abs() < f64::MIN_POSITIVE { (x * 2f64.powi(54), -54) } else { (x, 0) };
        let bits = y.to_bits();
        let exp = ((bits >> 52) & 0x7ff) as i64;
        let m = f64::from_bits((bits & !(0x7ff << 52)) | (1022 << 52));
        (m, exp - 1022 + adj)
    };
    lua.ret(&[Value::Num(m), Value::Int(e)])
}

fn unary_f(lua: &mut Lua, a: Args, f: fn(f64) -> f64) -> R {
    let x = lua.check_num(a, 0)?;
    lua.ret1(Value::Num(f(x)))
}

fn int_or_float(f: f64) -> Value {
    match number::float_to_int(f) {
        Some(i) => Value::Int(i),
        None => Value::Num(f),
    }
}

fn abs(lua: &mut Lua, a: Args) -> R {
    // Only integers stay integers: numeric strings convert to floats (as `lua_isinteger`).
    match int_arg(lua, a)? {
        Value::Int(i) => lua.ret1(Value::Int(i.wrapping_abs())),
        v => lua.ret1(Value::Num(v.as_num().unwrap_or(0.0).abs())),
    }
}

/// `math.floor` or `math.ceil`, by `f`.
fn round(lua: &mut Lua, a: Args, f: fn(f64) -> f64) -> R {
    match int_arg(lua, a)? {
        Value::Int(i) => lua.ret1(Value::Int(i)),
        v => lua.ret1(int_or_float(f(v.as_num().unwrap_or(0.0)))),
    }
}

fn int_arg(lua: &mut Lua, a: Args) -> Result<Value, LuaError> {
    match lua.arg(a, 0) {
        Value::Int(i) => Ok(Value::Int(i)),
        _ => Ok(Value::Num(lua.check_num(a, 0)?)),
    }
}

fn atan(lua: &mut Lua, a: Args) -> R {
    let y = lua.check_num(a, 0)?;
    let x = if lua.arg(a, 1).is_nil() { 1.0 } else { lua.check_num(a, 1)? };
    lua.ret1(Value::Num(y.atan2(x)))
}

fn log(lua: &mut Lua, a: Args) -> R {
    let x = lua.check_num(a, 0)?;
    let r = if lua.arg(a, 1).is_nil() {
        x.ln()
    } else {
        let b = lua.check_num(a, 1)?;
        if b == 2.0 {
            x.log2()
        } else if b == 10.0 {
            x.log10()
        } else {
            x.ln() / b.ln()
        }
    };
    lua.ret1(Value::Num(r))
}

fn fmod(lua: &mut Lua, a: Args) -> R {
    if let (Value::Int(x), Value::Int(d)) = (lua.arg(a, 0), lua.arg(a, 1)) {
        if d == 0 {
            return Err(LuaError::Arg(2, "zero".into()));
        }
        if d == -1 {
            return lua.ret1(Value::Int(0));
        }
        return lua.ret1(Value::Int(x % d));
    }
    // C checks the second argument first (gcc evaluates `fmod(luaL_checknumber(L, 1),
    // luaL_checknumber(L, 2))` right to left), so `math.fmod()` names argument #2.
    let y = lua.check_num(a, 1)?;
    let x = lua.check_num(a, 0)?;
    lua.ret1(Value::Num(x % y))
}

fn modf(lua: &mut Lua, a: Args) -> R {
    if let Value::Int(i) = lua.arg(a, 0) {
        return lua.ret(&[Value::Int(i), Value::Num(0.0)]);
    }
    let n = lua.check_num(a, 0)?;
    let ip = if n < 0.0 { n.ceil() } else { n.floor() };
    let frac = if n == ip { 0.0 } else { n - ip };
    lua.ret(&[int_or_float(ip), Value::Num(frac)])
}

fn tointeger(lua: &mut Lua, a: Args) -> R {
    let v = lua.check_any(a, 0)?;
    let r = match v {
        Value::Int(i) => Value::Int(i),
        Value::Num(f) => number::float_to_int(f).map_or(Value::Nil, Value::Int),
        Value::Str(s) => {
            // Converting text reads all of it (and may parse it as a float): by length.
            lua.charge(lua.bytes(s).len() as u64 / 8 + 1)?;
            lua.tointeger(v).map_or(Value::Nil, Value::Int)
        }
        _ => Value::Nil,
    };
    lua.ret1(r)
}

fn type_(lua: &mut Lua, a: Args) -> R {
    let v = lua.check_any(a, 0)?;
    match v {
        Value::Int(_) => lua.ret_str(b"integer"),
        Value::Num(_) => lua.ret_str(b"float"),
        _ => lua.ret1(Value::Nil),
    }
}

fn ult(lua: &mut Lua, a: Args) -> R {
    let (x, y) = (lua.check_int(a, 0)?, lua.check_int(a, 1)?);
    lua.ret1(Value::Bool((x as u64) < (y as u64)))
}

fn extreme(lua: &mut Lua, a: Args, want_min: bool) -> R {
    if a.n == 0 {
        return Err(LuaError::Arg(1, "value expected".into()));
    }
    let mut best = lua.arg(a, 0);
    for i in 1..a.n {
        let v = lua.arg(a, i);
        let better = if want_min { lua.less_than(v, best)? } else { lua.less_than(best, v)? };
        if better {
            best = v;
        }
    }
    lua.ret1(best)
}

impl Lua {
    /// The next xoshiro256** value.
    pub(crate) fn next_random(&mut self) -> u64 {
        let s = &mut self.rng;
        let res = s[1].wrapping_mul(5).rotate_left(7).wrapping_mul(9);
        let t = s[1] << 17;
        s[2] ^= s[0];
        s[3] ^= s[1];
        s[1] ^= s[2];
        s[0] ^= s[3];
        s[2] ^= t;
        s[3] = s[3].rotate_left(45);
        res
    }

    pub(crate) fn seed_random(&mut self, n1: u64, n2: u64) {
        self.rng = [n1, 0xff, n2, 0];
        for _ in 0..16 {
            self.next_random();
        }
    }
}

fn project(lua: &mut Lua, ran: u64, n: u64) -> u64 {
    if n & n.wrapping_add(1) == 0 {
        return ran & n;
    }
    let mut lim = n;
    lim |= lim >> 1;
    lim |= lim >> 2;
    lim |= lim >> 4;
    lim |= lim >> 8;
    lim |= lim >> 16;
    lim |= lim >> 32;
    let mut ran = ran & lim;
    while ran > n {
        ran = lua.next_random() & lim;
    }
    ran
}

fn random(lua: &mut Lua, a: Args) -> R {
    let rv = lua.next_random();
    let (low, up) = match a.n {
        0 => {
            let f = (rv >> 11) as f64 * (0.5 / (1u64 << 52) as f64);
            return lua.ret1(Value::Num(f));
        }
        1 => {
            let up = lua.check_int(a, 0)?;
            if up == 0 {
                return lua.ret1(Value::Int(rv as i64));
            }
            (1, up)
        }
        2 => (lua.check_int(a, 0)?, lua.check_int(a, 1)?),
        _ => return Err(LuaError::msg("wrong number of arguments")),
    };
    if low > up {
        return Err(LuaError::Arg(1, "interval is empty".into()));
    }
    let p = project(lua, rv, (up as u64).wrapping_sub(low as u64));
    lua.ret1(Value::Int(p.wrapping_add(low as u64) as i64))
}

fn randomseed(lua: &mut Lua, a: Args) -> R {
    let (n1, n2) = if a.n == 0 {
        // A fresh seed; under a fixed `hash_seed`, the next one of the generator (so replays
        // repeat).
        let s = if lua.limits.hash_seed.is_some() { lua.next_random() } else { crate::heap::random_seed() };
        (s, s.rotate_left(32))
    } else {
        // 5.4.7: an integer (a float needs an exact integer value).
        let n1 = lua.check_int(a, 0)? as u64;
        (n1, lua.opt_int(a, 1, 0)? as u64)
    };
    lua.seed_random(n1, n2);
    lua.ret(&[Value::Int(n1 as i64), Value::Int(n2 as i64)])
}
