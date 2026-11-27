//! `io` (`Libs::IO`), as C Lua 5.4.7's `liolib.c`, over streams the host gives: the
//! standard streams ([`Lua::set_stdin`] and the others; by default stdin is empty and stdout
//! and stderr write where `print` writes) and the files its opener hands out
//! ([`Lua::set_file_opener`]). No file system and no process: no `popen`, no `tmpfile`.
//!
//! [`Lua::set_stdin`]: crate::Lua::set_stdin
//! [`Lua::set_file_opener`]: crate::Lua::set_file_opener

#[allow(unused_imports)]
use crate::nostd::prelude::*;
use crate::nostd::io::SeekFrom;

use super::R;
use crate::value::Value;
use crate::vm::{Args, Lua, LuaError};

/// An error from a [`Stream`] or the host's opener, as C's `errno`: `strerror`'s message
/// and the number. Scripts see `nil, message, code`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IoError {
    /// The message ("No such file or directory").
    pub message: String,
    /// The number (2), or 0.
    pub code: i64,
}

impl IoError {
    /// An error with this message and number.
    pub fn new(message: impl Into<String>, code: i64) -> IoError {
        IoError { message: message.into(), code }
    }
}

fn not_found() -> IoError {
    IoError::new("No such file or directory", 2)
}

fn bad_descriptor() -> IoError {
    IoError::new("Bad file descriptor", 9)
}

fn illegal_seek() -> IoError {
    IoError::new("Illegal seek", 29)
}

/// A stream a script's `io` reads and writes. Each method defaults to C's failure on a
/// stream that cannot do it. Calls run inside the script's call: a stream that blocks holds
/// the script (fuel and the deadline are not read meanwhile).
pub trait Stream: Send {
    /// Reads into `buf`; 0 is the end.
    fn read(&mut self, _buf: &mut [u8]) -> Result<usize, IoError> {
        Err(bad_descriptor())
    }
    /// Writes all of `data`.
    fn write(&mut self, _data: &[u8]) -> Result<(), IoError> {
        Err(bad_descriptor())
    }
    /// Flushes what was written.
    fn flush(&mut self) -> Result<(), IoError> {
        Ok(())
    }
    /// Moves to `pos`; the new position from the start.
    fn seek(&mut self, _pos: SeekFrom) -> Result<u64, IoError> {
        Err(illegal_seek())
    }
    /// Closes the stream (`file:close()`); it is dropped after.
    fn close(&mut self) -> Result<(), IoError> {
        Ok(())
    }
}

/// Read-ahead per file (stdio's buffer), counted in the file's memory.
const BUF: usize = 4096;
/// C's `L_MAXLENNUM`.
const MAXLENNUM: usize = 200;
/// C's `MAXARGLINE`.
const MAXARGLINE: usize = 250;

pub(crate) enum Backing {
    /// `print`'s output: write only.
    Output,
    Stream(Box<dyn Stream>),
    /// Reads see the end, writes fail (the default stdin).
    Empty,
}

/// A file's stream and what it read ahead.
pub(crate) struct FileState {
    backing: Backing,
    buf: Vec<u8>,
    pos: usize,
    /// C's `ferror`.
    error: Option<IoError>,
}

impl FileState {
    pub(crate) fn new(backing: Backing) -> FileState {
        FileState { backing, buf: Vec::new(), pos: 0, error: None }
    }

    /// Reads ahead when the buffer is used up: false at the end or on an error. Charged 1
    /// per 32 bytes read.
    fn fill(&mut self, lua: &mut Lua) -> Result<bool, LuaError> {
        if self.pos < self.buf.len() {
            return Ok(true);
        }
        self.pos = 0;
        self.buf.resize(BUF, 0);
        let r = match &mut self.backing {
            Backing::Stream(s) => s.read(&mut self.buf),
            Backing::Empty => Ok(0),
            Backing::Output => Err(bad_descriptor()),
        };
        let n = r.unwrap_or_else(|e| {
            self.error = Some(e);
            0
        });
        self.buf.truncate(n.min(BUF));
        lua.charge(1 + n as u64 / 32)?;
        Ok(n > 0)
    }

    fn getc(&mut self, lua: &mut Lua) -> Result<Option<u8>, LuaError> {
        if !self.fill(lua)? {
            return Ok(None);
        }
        self.pos += 1;
        Ok(self.buf.get(self.pos - 1).copied())
    }

    /// Drops the read-ahead (before a write or a seek), moving the stream back over what
    /// the script has not read, as stdio does. The bytes given back.
    fn unread(&mut self) -> i64 {
        let n = self.buf.len().saturating_sub(self.pos) as i64;
        self.buf.clear();
        self.pos = 0;
        n
    }

    fn write(&mut self, lua: &mut Lua, b: &[u8]) -> Result<Result<(), IoError>, LuaError> {
        lua.charge(1 + b.len() as u64 / 32)?;
        let n = self.unread();
        Ok(match &mut self.backing {
            Backing::Output => super::base::emit_output(lua, b).map(|()| Ok(()))?,
            Backing::Stream(s) => {
                if n > 0 {
                    let _ = s.seek(SeekFrom::Current(-n));
                }
                s.write(b)
            }
            Backing::Empty => Err(bad_descriptor()),
        })
    }
}

/// A file handle's data (`LStream`). A standard stream's state is the state's (`io_std`).
pub(crate) struct LFile {
    std: Option<usize>,
    closed: bool,
    state: Option<FileState>,
}

pub(super) fn open(lua: &mut Lua) {
    let t = lua.lib_table("io");
    let fns: [(&str, &'static str, crate::vm::NativeFn); 9] = [
        ("close", "io.close", io_close),
        ("flush", "io.flush", |l, _| {
            let f = default_file(l, "_IO_output", true)?;
            flush(l, f)
        }),
        ("input", "io.input", |l, a| g_iofile(l, a, "_IO_input", b"r")),
        ("lines", "io.lines", io_lines),
        ("open", "io.open", io_open),
        ("output", "io.output", |l, a| g_iofile(l, a, "_IO_output", b"w")),
        ("read", "io.read", |l, a| {
            let f = default_file(l, "_IO_input", true)?;
            g_read(l, f, &args_from(l, a, 0), 1)
        }),
        ("type", "io.type", io_type),
        ("write", "io.write", |l, a| {
            let f = default_file(l, "_IO_output", true)?;
            g_write(l, f, a, 0)
        }),
    ];
    for (n, q, f) in fns {
        lua.set_fn(t, n, q, f);
    }
    // `luaL_newmetatable(L, "FILE*")`. The methods' names end in `_aux`: C finds no global
    // name for them, so a message says '?' where the call site gives none.
    let (Ok(meta), Ok(methods)) = (lua.new_table(0, 8), lua.new_table(0, 8)) else { return };
    let ms: [(&str, &'static str, crate::vm::NativeFn); 7] = [
        ("read", "read_aux", |l, a| {
            let f = to_file(l, a)?;
            g_read(l, f, &args_from(l, a, 1), 2)
        }),
        ("write", "write_aux", |l, a| g_write(l, to_file(l, a)?, a, 1)),
        ("lines", "lines_aux", |l, a| {
            let f = aux_lines(l, to_file(l, a)?, a, false)?;
            l.ret1(f)
        }),
        ("flush", "flush_aux", |l, a| flush(l, to_file(l, a)?)),
        ("seek", "seek_aux", f_seek),
        ("close", "close_aux", |l, a| aux_close(l, to_file(l, a)?)),
        ("setvbuf", "setvbuf_aux", |l, a| {
            to_file(l, a)?;
            l.check_option(a, 1, None, &["no", "full", "line"])?;
            l.opt_int(a, 2, 0)?;
            l.ret1(Value::Bool(true))
        }),
    ];
    for (n, q, f) in ms {
        lua.set_fn(methods, n, q, f);
    }
    lua.set_field(meta, "__index", Value::Table(methods));
    lua.set_fn(meta, "__close", "__close_aux", f_gc);
    lua.set_fn(meta, "__tostring", "__tostring_aux", f_tostring);
    let name = lua.new_str(b"FILE*").unwrap_or_default();
    lua.set_field(meta, "__name", name);
    let reg = lua.registry;
    let _ = lua.raw_set(reg, name, Value::Table(meta));
    lua.io_meta = Some(meta);
    for (i, field, key) in [(0, "stdin", "_IO_input"), (1, "stdout", "_IO_output"), (2, "stderr", "")] {
        let Ok(f) = new_file(lua, LFile { std: Some(i), closed: false, state: None }) else { return };
        if !key.is_empty() {
            lua.set_field(reg, key, f);
        }
        lua.set_field(t, field, f);
    }
}

fn new_file(lua: &mut Lua, f: LFile) -> Result<Value, LuaError> {
    let size = crate::nostd::mem::size_of::<LFile>() + if f.std.is_some() { 0 } else { BUF };
    let v = Value::UserData(lua.alloc_ud(Box::new(f), size, lua.io_meta)?);
    lua.hold(v);
    Ok(v)
}

fn lfile(lua: &Lua, v: Value) -> Option<&LFile> {
    let Value::UserData(u) = v else { return None };
    lua.heap.uds.get(u.idx)?.data.as_ref()?.downcast_ref::<LFile>()
}

fn lfile_mut(lua: &mut Lua, v: Value) -> Option<&mut LFile> {
    let Value::UserData(u) = v else { return None };
    lua.heap.uds.get_mut(u.idx)?.data.as_mut()?.downcast_mut::<LFile>()
}

/// `luaL_checkudata(L, 1, "FILE*")`.
fn check_file(lua: &Lua, a: Args) -> Result<Value, LuaError> {
    let v = lua.arg(a, 0);
    if lfile(lua, v).is_some() { Ok(v) } else { Err(lua.type_error(a, 0, "FILE*")) }
}

/// An open file (C's `tofile`).
fn open_file(lua: &Lua, v: Value) -> Result<Value, LuaError> {
    if lfile(lua, v).is_some_and(|f| f.closed) { Err(LuaError::msg("attempt to use a closed file")) } else { Ok(v) }
}

fn to_file(lua: &Lua, a: Args) -> Result<Value, LuaError> {
    let v = check_file(lua, a)?;
    open_file(lua, v)
}

/// The default input or output (`_IO_input` / `_IO_output` in the registry); `strict` is
/// C's `getiofile` ("default ... file is closed"), else `tofile` applies.
fn default_file(lua: &mut Lua, key: &str, strict: bool) -> Result<Value, LuaError> {
    let k = lua.new_str(key.as_bytes())?;
    let v = lua.raw_get(lua.registry, k);
    if strict && lfile(lua, v).is_none_or(|f| f.closed) {
        return Err(LuaError::msg(format!("default {} file is closed", key.get(4..).unwrap_or_default())));
    }
    open_file(lua, v)
}

/// Runs `op` on file `v`'s state, taken out of it (or of `io_std`) and put back after.
fn with_state<T>(lua: &mut Lua, v: Value, op: impl FnOnce(&mut Lua, &mut FileState) -> Result<T, LuaError>) -> Result<T, LuaError> {
    let std = lfile(lua, v).and_then(|f| f.std);
    let taken = match std {
        Some(i) => lua.io_std.get_mut(i).and_then(Option::take),
        None => lfile_mut(lua, v).and_then(|f| f.state.take()),
    };
    let mut st = taken.unwrap_or_else(|| FileState::new(if std.is_some_and(|i| i > 0) { Backing::Output } else { Backing::Empty }));
    let r = op(lua, &mut st);
    match std {
        Some(i) => {
            if let Some(s) = lua.io_std.get_mut(i) {
                *s = Some(st);
            }
        }
        None => {
            if let Some(f) = lfile_mut(lua, v) {
                f.state = Some(st);
            }
        }
    }
    r
}

/// `luaL_fileresult`: true, or nil, the message (after `fname: `), the number.
fn file_result(lua: &mut Lua, r: Result<(), IoError>, fname: Option<&[u8]>) -> R {
    let Err(e) = r else { return lua.ret1(Value::Bool(true)) };
    let mut msg = fname.map(|n| [n, b": "].concat()).unwrap_or_default();
    msg.extend_from_slice(e.message.as_bytes());
    let m = lua.new_str(&msg)?;
    lua.ret(&[Value::Nil, m, Value::Int(e.code)])
}

/// C's `fopen` through the host's opener. The name ends at its first zero byte.
fn host_open(lua: &mut Lua, name: &[u8], mode: &[u8]) -> Result<Result<Value, IoError>, LuaError> {
    lua.charge(16 + name.len() as u64 / 32)?;
    let opened = match (lua.io_opener.clone(), crate::nostd::str::from_utf8(name), crate::nostd::str::from_utf8(mode)) {
        (Some(o), Ok(n), Ok(m)) => o(n, m),
        _ => Err(not_found()),
    };
    Ok(match opened {
        Ok(s) => Ok(new_file(lua, LFile { std: None, closed: false, state: Some(FileState::new(Backing::Stream(s))) })?),
        Err(e) => Err(e),
    })
}

/// The bytes of string argument `i` up to the first zero byte (as C reads a name).
fn c_str(lua: &mut Lua, a: Args, i: usize) -> Result<Vec<u8>, LuaError> {
    let s = lua.check_str(a, i)?;
    Ok(lua.bytes(s).split(|&c| c == 0).next().unwrap_or_default().to_vec())
}

/// C's `opencheck`.
fn open_check(lua: &mut Lua, a: Args, mode: &[u8]) -> Result<Value, LuaError> {
    let name = c_str(lua, a, 0)?;
    host_open(lua, &name, mode)?.map_err(|e| LuaError::msg(format!("cannot open file '{}' ({})", String::from_utf8_lossy(&name), e.message)))
}

fn io_open(lua: &mut Lua, a: Args) -> R {
    let name = c_str(lua, a, 0)?;
    let mode = if lua.arg(a, 1).is_nil() { b"r".to_vec() } else { c_str(lua, a, 1)? };
    // C's `l_checkmode`: `[rwa]%+?b*`.
    let rest = mode.get(1..).unwrap_or_default();
    let rest = rest.strip_prefix(b"+").unwrap_or(rest);
    if !matches!(mode.first(), Some(b'r' | b'w' | b'a')) || rest.iter().any(|&c| c != b'b') {
        return Err(LuaError::Arg(2, "invalid mode".into()));
    }
    match host_open(lua, &name, &mode)? {
        Ok(f) => lua.ret1(f),
        Err(e) => file_result(lua, Err(e), Some(&name)),
    }
}

fn io_type(lua: &mut Lua, a: Args) -> R {
    let v = lua.check_any(a, 0)?;
    match lfile(lua, v) {
        None => lua.ret1(Value::Nil),
        Some(f) => lua.ret_str(if f.closed { b"closed file" } else { b"file" }),
    }
}

fn f_tostring(lua: &mut Lua, a: Args) -> R {
    let v = check_file(lua, a)?;
    if lfile(lua, v).is_some_and(|f| f.closed) {
        return lua.ret_str(b"file (closed)");
    }
    lua.ret_str(format!("file (0x{:08x})", crate::vm::address_of(v)).as_bytes())
}

/// C's `aux_close`; a standard file stays open.
fn aux_close(lua: &mut Lua, v: Value) -> R {
    lua.charge(4)?;
    if lfile(lua, v).is_some_and(|f| f.std.is_some()) {
        let m = lua.new_str(b"cannot close standard file")?;
        return lua.ret(&[Value::Nil, m]);
    }
    let st = lfile_mut(lua, v).and_then(|f| {
        f.closed = true;
        f.state.take()
    });
    let r = match st {
        Some(FileState { backing: Backing::Stream(mut s), .. }) => s.flush().and(s.close()),
        _ => Ok(()),
    };
    file_result(lua, r, None)
}

fn io_close(lua: &mut Lua, a: Args) -> R {
    let v = if a.n == 0 { default_file(lua, "_IO_output", false)? } else { to_file(lua, a)? };
    aux_close(lua, v)
}

/// `__close` (C's `f_gc`).
fn f_gc(lua: &mut Lua, a: Args) -> R {
    let v = check_file(lua, a)?;
    if lfile(lua, v).is_some_and(|f| !f.closed) {
        aux_close(lua, v)?;
    }
    lua.ret(&[])
}

/// C's `g_iofile`.
fn g_iofile(lua: &mut Lua, a: Args, key: &str, mode: &[u8]) -> R {
    let k = lua.new_str(key.as_bytes())?;
    let f = match lua.arg(a, 0) {
        Value::Nil => lua.raw_get(lua.registry, k),
        Value::Str(_) | Value::Int(_) | Value::Num(_) => open_check(lua, a, mode)?,
        _ => to_file(lua, a)?,
    };
    let reg = lua.registry;
    lua.raw_set(reg, k, f)?;
    lua.ret1(f)
}

// ---- reading

/// Up to `n` bytes, stopping after a newline when `line`: the bytes and whether a newline
/// ended them (C's `read_line`, `read_chars` and `read_all`).
fn read_bytes(lua: &mut Lua, st: &mut FileState, n: u64, line: bool) -> Result<(Vec<u8>, bool), LuaError> {
    let mut out = Vec::new();
    let mut hold = super::MemHold::default();
    while (out.len() as u64) < n && st.fill(lua)? {
        let rest = st.buf.get(st.pos..).unwrap_or_default();
        let room = usize::try_from(n - out.len() as u64).unwrap_or(usize::MAX).min(rest.len());
        let nl = if line { rest.get(..room).and_then(|r| r.iter().position(|&c| c == b'\n')) } else { None };
        let take = nl.unwrap_or(room);
        if out.len() + take > lua.limits.string_len {
            return Err(LuaError::Limit(crate::LimitKind::Memory));
        }
        out.extend_from_slice(rest.get(..take).unwrap_or_default());
        hold.cover(lua, out.capacity())?;
        st.pos += take;
        if nl.is_some() {
            st.pos += 1;
            return Ok((out, true));
        }
    }
    Ok((out, false))
}

/// C's `read_number`: the longest numeral prefix (at most 200 bytes), as `tonumber` reads it.
fn read_number(lua: &mut Lua, st: &mut FileState) -> Result<Option<Value>, LuaError> {
    let mut buf = Vec::new();
    let mut c = st.getc(lua)?;
    while c.is_some_and(|c| matches!(c, b' ' | b'\t' | b'\n' | b'\x0b' | b'\x0c' | b'\r')) {
        c = st.getc(lua)?;
    }
    // C's `test2` and `readdigits`; past `L_MAXLENNUM`, `nextc` fails and the result is
    // invalid.
    let mut bad = false;
    let mut accept = |lua: &mut Lua, st: &mut FileState, c: &mut Option<u8>, ok: &dyn Fn(u8) -> bool| -> Result<bool, LuaError> {
        match *c {
            Some(x) if ok(x) && !bad => {
                bad = buf.len() >= MAXLENNUM;
                if !bad {
                    buf.push(x);
                    *c = st.getc(lua)?;
                }
                Ok(!bad)
            }
            _ => Ok(false),
        }
    };
    let mut count = 0;
    accept(lua, st, &mut c, &|x| x == b'-' || x == b'+')?;
    let hex = if accept(lua, st, &mut c, &|x| x == b'0')? {
        let hex = accept(lua, st, &mut c, &|x| x == b'x' || x == b'X')?;
        count = usize::from(!hex);
        hex
    } else {
        false
    };
    let digit = move |x: u8| if hex { x.is_ascii_hexdigit() } else { x.is_ascii_digit() };
    while accept(lua, st, &mut c, &digit)? {
        count += 1;
    }
    if accept(lua, st, &mut c, &|x| x == b'.')? {
        while accept(lua, st, &mut c, &digit)? {
            count += 1;
        }
    }
    if count > 0 && accept(lua, st, &mut c, &|x| if hex { x == b'p' || x == b'P' } else { x == b'e' || x == b'E' })? {
        accept(lua, st, &mut c, &|x| x == b'-' || x == b'+')?;
        while accept(lua, st, &mut c, &|x: u8| x.is_ascii_digit())? {}
    }
    if c.is_some() {
        st.pos -= 1; // `ungetc`
    }
    Ok(if bad { None } else { crate::number::str_to_number(&buf) })
}

/// C's `g_read` on file `v` with `formats`, the first being argument `argn0`.
fn g_read(lua: &mut Lua, v: Value, formats: &[Value], argn0: usize) -> R {
    lua.charge(4)?;
    let mut results = Vec::with_capacity(formats.len().max(1));
    let line = [Value::Nil];
    let fmts = if formats.is_empty() { &line[..] } else { formats };
    let err = with_state(lua, v, |lua, st| {
        st.error = None;
        for (k, &f) in fmts.iter().enumerate() {
            let argn = argn0 + k;
            let kind = match f {
                Value::Nil => b'l',
                Value::Int(_) | Value::Num(_) => b'#',
                Value::Str(s) => {
                    let b = lua.bytes(s);
                    b.strip_prefix(b"*").unwrap_or(b).first().copied().unwrap_or(0)
                }
                other => return Err(LuaError::Arg(argn, format!("string expected, got {}", lua.arg_type_name(other)))),
            };
            // A string to make, a number read, or nothing (the read failed).
            let read: Option<Result<Vec<u8>, Value>> = match kind {
                b'#' => match lua.tointeger(f) {
                    Some(0) => st.fill(lua)?.then(|| Ok(Vec::new())), // C's `test_eof`
                    Some(n) => Some(read_bytes(lua, st, n as u64, false)?.0).filter(|b| !b.is_empty()).map(Ok),
                    None => return Err(LuaError::Arg(argn, "number has no integer representation".into())),
                },
                b'n' => read_number(lua, st)?.map(Err),
                b'l' | b'L' => {
                    let (mut b, nl) = read_bytes(lua, st, u64::MAX, true)?;
                    if nl && kind == b'L' {
                        b.push(b'\n');
                    }
                    (nl || !b.is_empty()).then_some(Ok(b))
                }
                b'a' => Some(Ok(read_bytes(lua, st, u64::MAX, false)?.0)),
                _ => return Err(LuaError::Arg(argn, "invalid format".into())),
            };
            let val = match read {
                Some(Ok(b)) => lua.new_str(&b)?,
                Some(Err(n)) => n,
                None => {
                    results.push(Value::Nil);
                    break;
                }
            };
            lua.hold(val);
            results.push(val);
        }
        Ok(st.error.take())
    })?;
    if let Some(e) = err {
        return file_result(lua, Err(e), None);
    }
    lua.ret(&results)
}

fn args_from(lua: &Lua, a: Args, from: usize) -> Vec<Value> {
    (from..a.n).map(|i| lua.arg(a, i)).collect()
}

/// C's `aux_lines`: an iterator over `v` with the formats after argument 1.
fn aux_lines(lua: &mut Lua, v: Value, a: Args, close: bool) -> Result<Value, LuaError> {
    if a.n > MAXARGLINE + 1 {
        return Err(LuaError::Arg(MAXARGLINE + 2, "too many arguments".into()));
    }
    let mut up = vec![v, Value::Bool(close)];
    up.extend(args_from(lua, a, 1));
    lua.new_native(io_readline, "io_readline_aux", up)
}

fn io_lines(lua: &mut Lua, a: Args) -> R {
    if lua.arg(a, 0).is_nil() {
        let f = default_file(lua, "_IO_input", false)?;
        let it = aux_lines(lua, f, a, false)?;
        return lua.ret1(it);
    }
    let file = open_check(lua, a, b"r")?;
    let it = aux_lines(lua, file, a, true)?;
    lua.ret(&[it, Value::Nil, Value::Nil, file])
}

/// The `lines` iterator (C's `io_readline`): upvalues file, close-at-end, formats.
fn io_readline(lua: &mut Lua, a: Args) -> R {
    let v = lua.native_upval(a, 0);
    if lfile(lua, v).is_none_or(|f| f.closed) {
        return Err(LuaError::msg("file is already closed"));
    }
    let fmts: Vec<Value> = (2..lua.native_upvals_len(a)).map(|i| lua.native_upval(a, i)).collect();
    g_read(lua, v, &fmts, 2)?; // (C pushes the formats from stack slot 2.)
    let results = lua.rets.clone();
    match results.as_slice() {
        [first, ..] if first.truthy() => lua.ret(&results),
        [_, Value::Str(m), ..] => Err(LuaError::msg(String::from_utf8_lossy(lua.bytes(*m)).into_owned())),
        _ => {
            if lua.native_upval(a, 1).truthy() {
                aux_close(lua, v)?;
            }
            lua.ret(&[])
        }
    }
}

// ---- writing

/// C's `g_write`: numbers as `%d` / `%.14g`, strings as they are.
fn g_write(lua: &mut Lua, v: Value, a: Args, first: usize) -> R {
    lua.charge(4)?;
    let mut status = Ok(());
    for i in first..a.n {
        let bytes = match lua.arg(a, i) {
            Value::Int(n) => n.to_string().into_bytes(),
            Value::Num(f) => {
                lua.charge(crate::vm::FLOAT_TEXT_FUEL)?;
                crate::number::fmt_g(f, 14, false, false).into_bytes()
            }
            _ => {
                let s = lua.check_str(a, i)?;
                lua.bytes(s).to_vec()
            }
        };
        if status.is_ok() {
            status = with_state(lua, v, |lua, st| st.write(lua, &bytes))?;
        }
    }
    if status.is_ok() { lua.ret1(v) } else { file_result(lua, status, None) }
}

fn f_seek(lua: &mut Lua, a: Args) -> R {
    let v = to_file(lua, a)?;
    let op = lua.check_option(a, 1, Some("cur"), &["set", "cur", "end"])?;
    let off = lua.opt_int(a, 2, 0)?;
    lua.charge(4)?;
    let r = with_state(lua, v, |_, st| {
        // `cur` is where the script is: what was read ahead is given back.
        let unread = st.unread();
        let to = match op {
            0 => match u64::try_from(off) {
                Ok(o) => SeekFrom::Start(o),
                Err(_) => return Ok(Err(IoError::new("Invalid argument", 22))),
            },
            1 => SeekFrom::Current(off.saturating_sub(unread)),
            _ => SeekFrom::End(off),
        };
        Ok(if let Backing::Stream(s) = &mut st.backing { s.seek(to) } else { Err(illegal_seek()) })
    })?;
    match r {
        Ok(p) => lua.ret1(Value::Int(i64::try_from(p).unwrap_or(i64::MAX))),
        Err(e) => file_result(lua, Err(e), None),
    }
}

fn flush(lua: &mut Lua, v: Value) -> R {
    lua.charge(4)?;
    let r = with_state(lua, v, |_, st| Ok(if let Backing::Stream(s) = &mut st.backing { s.flush() } else { Ok(()) }))?;
    file_result(lua, r, None)
}
