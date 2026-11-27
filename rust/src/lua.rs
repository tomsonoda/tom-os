//! The Lua runtime for apps.
//!
//! The interpreter is RemedyLua (third_party/remedy-lua), which has no unsafe
//! code and enforces memory limits itself. This module gives apps their
//! libraries (`io`, `os`, `tomos`, `gfx`, `sound`, `require`) on top of the
//! kernel (lua/lhost.c and friends) and exports `rl_run`.

use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::ffi::{c_char, c_void};
use core::time::Duration;

use remedy_lua::{Error, Interrupt, IoError, Libs, LimitKind, Limits, Lua, ModuleSource, Stream, Value};

use crate::fs;
use crate::rt::{console, cstr};
use crate::sys;

// -------------------------------------------------------------- arguments

type R = Result<Vec<Value>, Error>;

fn bad(i: usize, what: &str, got: Option<&Value>) -> Error {
    Error::runtime(format!("bad argument #{} ({} expected, got {})", i + 1, what, got.map_or("no value", |v| v.type_name())))
}

fn num(lua: &Lua, a: &[Value], i: usize) -> Result<f64, Error> {
    match a.get(i) {
        Some(Value::Int(n)) => Ok(*n as f64),
        Some(Value::Num(f)) => Ok(*f),
        Some(v @ Value::Str(_)) => {
            // numeric strings count, as in C Lua
            let s = lua.to_string_lossy(*v).unwrap_or_default();
            s.trim().parse::<f64>().map_err(|_| bad(i, "number", a.get(i)))
        }
        other => Err(bad(i, "number", other)),
    }
}

fn opt_num(lua: &Lua, a: &[Value], i: usize, default: f64) -> Result<f64, Error> {
    match a.get(i) {
        None | Some(Value::Nil) => Ok(default),
        _ => num(lua, a, i),
    }
}

/// An integer argument; floats are truncated and huge values clamped.
fn int(lua: &Lua, a: &[Value], i: usize) -> Result<i32, Error> {
    let f = num(lua, a, i)?;
    Ok(if f.is_nan() { 0 } else { f.clamp(-1e9, 1e9) as i32 })
}

fn opt_int(lua: &Lua, a: &[Value], i: usize, default: i32) -> Result<i32, Error> {
    match a.get(i) {
        None | Some(Value::Nil) => Ok(default),
        _ => int(lua, a, i),
    }
}

fn color(lua: &Lua, a: &[Value], i: usize, default: u32) -> Result<u32, Error> {
    match a.get(i) {
        None | Some(Value::Nil) => Ok(default),
        _ => Ok((num(lua, a, i)? as i64 as u32) & 0xFF_FFFF),
    }
}

fn bytes(lua: &Lua, a: &[Value], i: usize) -> Result<Vec<u8>, Error> {
    match a.get(i) {
        Some(v @ Value::Str(_)) => Ok(lua.str_bytes(*v).unwrap_or_default().to_vec()),
        Some(v @ (Value::Int(_) | Value::Num(_))) => Ok(lua.to_string_lossy(*v).unwrap_or_default().into_bytes()),
        other => Err(bad(i, "string", other)),
    }
}

/// Any value as text, as `tostring` shows plain values.
fn display(lua: &Lua, v: Option<&Value>) -> Vec<u8> {
    match v {
        None | Some(Value::Nil) => b"nil".to_vec(),
        Some(Value::Bool(b)) => if *b { b"true".to_vec() } else { b"false".to_vec() },
        Some(v @ Value::Str(_)) => lua.str_bytes(*v).unwrap_or_default().to_vec(),
        Some(v) => lua.to_string_lossy(*v).map(String::into_bytes).unwrap_or_else(|| v.type_name().as_bytes().to_vec()),
    }
}

fn is_given(a: &[Value], i: usize) -> bool {
    !matches!(a.get(i), None | Some(Value::Nil))
}

/// Stops the script: the interpreter raises an error no `pcall` can catch
/// as soon as the host function returns.
fn stop(intr: &Interrupt) -> R {
    intr.interrupt();
    Ok(vec![])
}

fn gfx_result(code: i32) -> R {
    match code {
        0 => Ok(vec![]),
        -1 => Err(Error::runtime(
            "graphics are not available here (needs the GUI and an app started with /run or launch_app)",
        )),
        -2 => Err(Error::runtime("call gfx.mode(width, height) first")),
        -3 => Err(Error::runtime("out of memory")),
        _ => Err(Error::runtime("graphics error")),
    }
}

// ------------------------------------------------------------- io streams

/// The console as `io.stdout` / `io.stderr`.
struct ConsoleOut;

impl Stream for ConsoleOut {
    fn write(&mut self, data: &[u8]) -> Result<(), IoError> {
        console(data);
        Ok(())
    }
}

/// The keyboard as `io.stdin`: one edited line at a time.
struct ConsoleIn {
    line: Vec<u8>,
    pos: usize,
    intr: Interrupt,
}

impl Stream for ConsoleIn {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, IoError> {
        if self.pos >= self.line.len() {
            let mut tmp = [0u8; 1024];
            let n = unsafe { sys::host_readline(tmp.as_mut_ptr(), tmp.len()) };
            if n == -2 {
                self.intr.interrupt();
                return Ok(0);
            }
            if n < 0 {
                return Ok(0); // no keyboard: end of input
            }
            self.line.clear();
            self.line.extend_from_slice(&tmp[..n as usize]);
            self.line.push(b'\n');
            self.pos = 0;
        }
        let n = buf.len().min(self.line.len() - self.pos);
        buf[..n].copy_from_slice(&self.line[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

/// A file of the in-memory filesystem; written back when flushed or closed.
struct RamFile {
    path: Vec<u8>,
    data: Vec<u8>,
    pos: usize,
    readable: bool,
    writable: bool,
    append: bool,
    dirty: bool,
}

impl RamFile {
    fn save(&mut self) {
        if self.dirty {
            fs::write(&self.path, &self.data);
            self.dirty = false;
        }
    }
}

impl Stream for RamFile {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, IoError> {
        if !self.readable {
            return Err(IoError::new("Bad file descriptor", 9));
        }
        let n = buf.len().min(self.data.len().saturating_sub(self.pos));
        buf[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }

    fn write(&mut self, data: &[u8]) -> Result<(), IoError> {
        if !self.writable {
            return Err(IoError::new("Bad file descriptor", 9));
        }
        if self.append {
            self.pos = self.data.len();
        }
        if self.pos > self.data.len() {
            self.data.resize(self.pos, 0);
        }
        let overlap = data.len().min(self.data.len() - self.pos);
        self.data[self.pos..self.pos + overlap].copy_from_slice(&data[..overlap]);
        self.data.extend_from_slice(&data[overlap..]);
        self.pos += data.len();
        self.dirty = true;
        Ok(())
    }

    fn flush(&mut self) -> Result<(), IoError> {
        self.save();
        Ok(())
    }

    fn seek(&mut self, pos: remedy_lua::nostd::io::SeekFrom) -> Result<u64, IoError> {
        use remedy_lua::nostd::io::SeekFrom;
        let target = match pos {
            SeekFrom::Start(n) => n as i64,
            SeekFrom::Current(d) => self.pos as i64 + d,
            SeekFrom::End(d) => self.data.len() as i64 + d,
        };
        if target < 0 {
            return Err(IoError::new("Invalid argument", 22));
        }
        self.pos = target as usize;
        Ok(target as u64)
    }

    fn close(&mut self) -> Result<(), IoError> {
        self.save();
        Ok(())
    }
}

impl Drop for RamFile {
    fn drop(&mut self) {
        self.save();
    }
}

fn open_file(path: &str, mode: &str) -> Result<Box<dyn Stream>, IoError> {
    let kind = mode.as_bytes().first().copied().unwrap_or(b'r');
    let plus = mode.contains('+');
    let existing = fs::read(path.as_bytes());
    if kind == b'r' && existing.is_none() {
        return Err(IoError::new("No such file or directory", 2));
    }
    let mut f = RamFile {
        path: path.as_bytes().to_vec(),
        data: if kind == b'w' { Vec::new() } else { existing.unwrap_or_default() },
        pos: 0,
        readable: kind == b'r' || plus,
        writable: kind != b'r' || plus,
        append: kind == b'a',
        dirty: kind == b'w', // create or truncate even if nothing is written
    };
    if kind == b'w' {
        f.save();
    }
    Ok(Box::new(f))
}

/// `require`: name.lua, apps/name.lua, lib/name.lua or the name itself.
fn find_module(name: &str) -> Option<ModuleSource> {
    let rel = name.replace('.', "/");
    for path in [format!("{rel}.lua"), format!("apps/{rel}.lua"), format!("lib/{rel}.lua"), rel.clone()] {
        if let Some(code) = fs::read(path.as_bytes()) {
            return Some(ModuleSource::new(code, format!("@{path}")));
        }
    }
    None
}

// -------------------------------------------------------------- libraries

/// Functions and values that are simplest to define in Lua itself.
const PRELUDE: &str = r#"
function loadfile(path, mode, env)
  local f = io.open(path, "r")
  if not f then return nil, "cannot open " .. tostring(path) end
  local src = f:read("a")
  f:close()
  if env ~= nil then return load(src, "@" .. path, "t", env) end
  return load(src, "@" .. path)
end
function dofile(path)
  local f, err = loadfile(path)
  if not f then error(err, 2) end
  return f()
end
os.getenv = os.getenv or function() return nil end
tomos.version = "0.1"
tomos.width, tomos.height = 80, 25
"#;

fn register_tomos(lua: &mut Lua, intr: &Interrupt) -> Result<(), Error> {
    lua.register("tomos.cls", |_, _| {
        console(b"\x1b[2J\x1b[H");
        Ok(vec![])
    })?;
    let i = intr.clone();
    lua.register("tomos.sleep", move |lua, a| {
        let ms = num(lua, a, 0)?;
        if unsafe { sys::host_sleep(ms) } != 0 {
            return stop(&i);
        }
        Ok(vec![])
    })?;
    let i = intr.clone();
    lua.register("tomos.getkey", move |lua, a| {
        let timeout = if is_given(a, 0) { int(lua, a, 0)?.max(0) } else { -1 };
        let mut out = [0u8; 16];
        let n = unsafe { sys::host_getkey(timeout, out.as_mut_ptr()) };
        match n {
            -1 => stop(&i),
            0 => Ok(vec![Value::Nil]),
            n => Ok(vec![lua.create_string(&out[..n as usize])?]),
        }
    })?;
    let i = intr.clone();
    lua.register("tomos.readline", move |lua, a| {
        if is_given(a, 0) {
            console(&bytes(lua, a, 0)?);
        }
        let mut tmp = [0u8; 1024];
        let n = unsafe { sys::host_readline(tmp.as_mut_ptr(), tmp.len()) };
        match n {
            -2 => stop(&i),
            n if n < 0 => Ok(vec![Value::Nil]),
            n => Ok(vec![lua.create_string(&tmp[..n as usize])?]),
        }
    })?;
    lua.register("tomos.time_ms", |_, _| Ok(vec![Value::Int(unsafe { sys::time_ms() } as i64)]))?;
    lua.register("tomos.files", |lua, _| {
        let t = lua.create_table()?;
        for (n, (name, _)) in fs::list().into_iter().enumerate() {
            let v = lua.create_string(name)?;
            lua.table_set(t, Value::Int(n as i64 + 1), v)?;
        }
        Ok(vec![Value::Table(t)])
    })?;
    lua.register("tomos.read_file", |lua, a| match fs::read(&bytes(lua, a, 0)?) {
        Some(d) => Ok(vec![lua.create_string(d)?]),
        None => Ok(vec![Value::Nil]),
    })?;
    lua.register("tomos.write_file", |lua, a| {
        Ok(vec![Value::Bool(fs::write(&bytes(lua, a, 0)?, &bytes(lua, a, 1)?))])
    })?;
    Ok(())
}

fn register_os(lua: &mut Lua, intr: &Interrupt) -> Result<(), Error> {
    let i = intr.clone();
    lua.register("os.exit", move |_, _| {
        unsafe { sys::host_exit() };
        stop(&i)
    })?;
    lua.register("os.remove", |lua, a| {
        let path = bytes(lua, a, 0)?;
        if fs::delete(&path) {
            return Ok(vec![Value::Bool(true)]);
        }
        let msg = lua.create_string(format!("{}: No such file or directory", String::from_utf8_lossy(&path)))?;
        Ok(vec![Value::Nil, msg, Value::Int(2)])
    })?;
    lua.register("os.rename", |lua, a| {
        let (from, to) = (bytes(lua, a, 0)?, bytes(lua, a, 1)?);
        if fs::rename(&from, &to) {
            return Ok(vec![Value::Bool(true)]);
        }
        let msg = lua.create_string(format!("{}: No such file or directory", String::from_utf8_lossy(&from)))?;
        Ok(vec![Value::Nil, msg, Value::Int(2)])
    })?;
    Ok(())
}

fn register_gfx(lua: &mut Lua, intr: &Interrupt) -> Result<(), Error> {
    lua.register("gfx.mode", |lua, a| {
        // gfx.mode() or gfx.mode(title): the whole screen; gfx.mode(w, h [, title])
        let whole = !is_given(a, 0) || matches!(a.first(), Some(Value::Str(_)));
        let (w, h) = if whole { (0, 0) } else { (int(lua, a, 0)?, int(lua, a, 1)?) };
        let title_at = if whole { 0 } else { 2 };
        let title = if matches!(a.get(title_at), Some(Value::Str(_))) { Some(cstr(&bytes(lua, a, title_at)?)) } else { None };
        let (mut ow, mut oh) = (0, 0);
        let tp = title.as_ref().map_or(core::ptr::null(), |t| t.as_ptr() as *const c_char);
        gfx_result(unsafe { sys::host_gfx_mode(whole as i32, w, h, tp, &mut ow, &mut oh) })?;
        Ok(vec![Value::Int(ow as i64), Value::Int(oh as i64)])
    })?;
    lua.register("gfx.clear", |lua, a| gfx_result(unsafe { sys::host_gfx_clear(color(lua, a, 0, 0)?) }))?;
    lua.register("gfx.rect", |lua, a| {
        let c = color(lua, a, 4, 0xFF_FFFF)?;
        gfx_result(unsafe { sys::host_gfx_rect(int(lua, a, 0)?, int(lua, a, 1)?, int(lua, a, 2)?, int(lua, a, 3)?, c, 0) })
    })?;
    lua.register("gfx.box", |lua, a| {
        let c = color(lua, a, 4, 0xFF_FFFF)?;
        gfx_result(unsafe { sys::host_gfx_rect(int(lua, a, 0)?, int(lua, a, 1)?, int(lua, a, 2)?, int(lua, a, 3)?, c, 1) })
    })?;
    lua.register("gfx.pixel", |lua, a| {
        gfx_result(unsafe { sys::host_gfx_pixel(int(lua, a, 0)?, int(lua, a, 1)?, color(lua, a, 2, 0xFF_FFFF)?) })
    })?;
    lua.register("gfx.line", |lua, a| {
        let c = color(lua, a, 4, 0xFF_FFFF)?;
        gfx_result(unsafe { sys::host_gfx_line(int(lua, a, 0)?, int(lua, a, 1)?, int(lua, a, 2)?, int(lua, a, 3)?, c) })
    })?;
    lua.register("gfx.circle", |lua, a| {
        let fill = a.get(4).is_some_and(|v| v.truthy());
        let c = color(lua, a, 3, 0xFF_FFFF)?;
        gfx_result(unsafe { sys::host_gfx_circle(int(lua, a, 0)?, int(lua, a, 1)?, int(lua, a, 2)?, c, fill as i32) })
    })?;
    lua.register("gfx.text", |lua, a| {
        let text = display(lua, a.get(2));
        let (c, scale) = (color(lua, a, 3, 0xFF_FFFF)?, opt_int(lua, a, 4, 1)?);
        gfx_result(unsafe { sys::host_gfx_text(int(lua, a, 0)?, int(lua, a, 1)?, text.as_ptr(), text.len(), c, scale) })
    })?;
    let i = intr.clone();
    lua.register("gfx.flip", move |lua, a| {
        match unsafe { sys::host_gfx_flip(opt_num(lua, a, 0, 60.0)?) } {
            1 => stop(&i),
            code => gfx_result(code),
        }
    })?;
    lua.register("gfx.key", |lua, a| {
        let name = bytes(lua, a, 0)?;
        let c = cstr(&name);
        match unsafe { sys::host_gfx_key(c.as_ptr() as *const c_char) } {
            -4 => Err(Error::runtime(format!("unknown key name '{}'", String::from_utf8_lossy(&name)))),
            code if code < 0 => gfx_result(code),
            held => Ok(vec![Value::Bool(held != 0)]),
        }
    })?;
    lua.register("gfx.mouse", |_, _| {
        let mut m = [0i32; 4];
        gfx_result(unsafe { sys::host_gfx_mouse(m.as_mut_ptr()) })?;
        Ok(vec![Value::Int(m[0] as i64), Value::Int(m[1] as i64), Value::Bool(m[2] != 0), Value::Bool(m[3] != 0)])
    })?;
    lua.register("gfx.size", |_, _| {
        let mut s = [0i32; 2];
        gfx_result(unsafe { sys::host_gfx_size(s.as_mut_ptr()) })?;
        Ok(vec![Value::Int(s[0] as i64), Value::Int(s[1] as i64)])
    })?;
    lua.register("gfx.rgb", |lua, a| {
        let c = |i| -> Result<i64, Error> { Ok(int(lua, a, i)?.clamp(0, 255) as i64) };
        Ok(vec![Value::Int((c(0)? << 16) | (c(1)? << 8) | c(2)?)])
    })?;
    Ok(())
}

const WAVES: [&str; 5] = ["square", "sine", "triangle", "saw", "noise"];

fn wave_named(lua: &Lua, v: Value, i: usize) -> Result<i32, Error> {
    let name = lua.to_string_lossy(v).unwrap_or_default();
    WAVES.iter().position(|w| *w == name).map(|p| p as i32).ok_or_else(|| {
        Error::runtime(format!("bad argument #{} (invalid wave '{}': square, sine, triangle, saw or noise)", i + 1, name))
    })
}

/// Optional sound arguments from index `i`: (wave, volume) or a table
/// {wave=, volume=, delay=}. Returns (wave, volume, delay_ms).
fn sound_opts(lua: &mut Lua, a: &[Value], i: usize) -> Result<(i32, f64, i32), Error> {
    let (mut wave, mut vol, mut delay) = (0, 0.5, 0);
    if let Some(t @ Value::Table(_)) = a.get(i) {
        let key = |lua: &mut Lua, k: &str| -> Result<Value, Error> {
            let k = lua.create_string(k)?;
            lua.get(*t, k)
        };
        let w = key(lua, "wave")?;
        if !w.is_nil() {
            wave = wave_named(lua, w, i)?;
        }
        if let Some(v) = key(lua, "volume")?.as_num() {
            vol = v;
        }
        if let Some(d) = key(lua, "delay")?.as_num() {
            delay = d as i32;
        }
    } else {
        if is_given(a, i) {
            wave = wave_named(lua, a[i], i)?;
        }
        vol = opt_num(lua, a, i + 1, 0.5)?;
    }
    Ok((wave, vol, delay))
}

fn register_sound(lua: &mut Lua, intr: &Interrupt) -> Result<(), Error> {
    lua.register("sound.available", |_, _| Ok(vec![Value::Bool(unsafe { sys::sound_available() } != 0)]))?;
    lua.register("sound.tone", |lua, a| {
        let (freq, ms) = (num(lua, a, 0)?, int(lua, a, 1)?);
        let (wave, vol, delay) = sound_opts(lua, a, 2)?;
        let ok = unsafe { sys::sound_tone(freq, ms, vol, wave, delay, sys::host_owner()) } == 0;
        Ok(vec![Value::Bool(ok)])
    })?;
    lua.register("sound.note", |lua, a| {
        let name = bytes(lua, a, 0)?;
        let ms = opt_int(lua, a, 1, 200)?;
        let (wave, vol, _) = sound_opts(lua, a, 2)?;
        let mut spec = name;
        spec.extend_from_slice(format!(":{ms}").as_bytes());
        let c = cstr(&spec);
        unsafe { sys::sound_melody(c.as_ptr() as *const c_char, ms, vol, wave, sys::host_owner()) };
        Ok(vec![])
    })?;
    lua.register("sound.melody", |lua, a| {
        let notes = cstr(&bytes(lua, a, 0)?);
        let mut len = 200;
        let (wave, vol, _) = if let Some(t @ Value::Table(_)) = a.get(1) {
            let k = lua.create_string("length")?;
            if let Some(l) = lua.get(*t, k)?.as_num() {
                len = l as i32;
            }
            sound_opts(lua, a, 1)?
        } else {
            len = opt_int(lua, a, 1, 200)?;
            sound_opts(lua, a, 2)?
        };
        let total = unsafe { sys::sound_melody(notes.as_ptr() as *const c_char, len, vol, wave, sys::host_owner()) };
        Ok(vec![Value::Int(total as i64)])
    })?;
    lua.register("sound.play", |lua, a| {
        let Some(Value::Table(t)) = a.first() else { return Err(bad(0, "table", a.first())) };
        let rate = opt_int(lua, a, 1, 22050)?;
        let vol = opt_num(lua, a, 2, 0.5)?;
        let n = lua.table_len(*t).max(0) as usize;
        if n > 48000 * 30 {
            return Err(Error::runtime("too many samples (at most 30 seconds)"));
        }
        let mut samples = Vec::with_capacity(n);
        for i in 1..=n {
            samples.push(lua.table_get(*t, Value::Int(i as i64)).as_num().unwrap_or(0.0) as f32);
        }
        let ok = unsafe { sys::sound_pcm(samples.as_ptr(), n, rate, vol, 0, sys::host_owner()) } == 0;
        Ok(vec![Value::Bool(ok)])
    })?;
    lua.register("sound.beep", |_, _| {
        unsafe { sys::sound_tone(880.0, 90, 0.4, 0, 0, sys::host_owner()) };
        Ok(vec![])
    })?;
    lua.register("sound.stop", |_, _| {
        unsafe { sys::sound_stop(sys::host_owner()) };
        Ok(vec![])
    })?;
    lua.register("sound.playing", |_, _| Ok(vec![Value::Bool(unsafe { sys::sound_busy(sys::host_owner()) } != 0)]))?;
    let i = intr.clone();
    lua.register("sound.wait", move |_, _| {
        if unsafe { sys::host_sound_wait() } != 0 {
            return stop(&i);
        }
        Ok(vec![])
    })?;
    Ok(())
}

fn setup(lua: &mut Lua) -> Result<(), Error> {
    let intr = lua.interrupt_handle();

    // Every few thousand steps the interpreter reads this clock: the moment
    // to let other threads run and to notice Ctrl-C, a closed window or the
    // time limit.
    let i = intr.clone();
    let clock: remedy_lua::Clock = Arc::new(move || {
        if unsafe { sys::host_poll() } != 0 {
            i.interrupt();
        }
        Duration::from_millis(unsafe { sys::time_ms() })
    });
    lua.set_time_limit(Some((clock, Duration::from_secs(1 << 40))));

    lua.set_print(|s| console(s.as_bytes()));
    lua.set_stdout(Some(Box::new(ConsoleOut)));
    lua.set_stderr(Some(Box::new(ConsoleOut)));
    lua.set_stdin(Some(Box::new(ConsoleIn { line: Vec::new(), pos: 0, intr: intr.clone() })));
    lua.set_file_opener(Some(open_file));
    lua.set_module_loader(Some(find_module));

    register_tomos(lua, &intr)?;
    register_os(lua, &intr)?;
    register_gfx(lua, &intr)?;
    register_sound(lua, &intr)?;
    lua.exec(PRELUDE, "=prelude")?;
    let gui = Value::Bool(unsafe { sys::wm_active() } != 0);
    let tomos = lua.get_global("tomos")?;
    let key = lua.create_string("gui")?;
    lua.set(tomos, key, gui)?;
    Ok(())
}

fn describe(e: &Error) -> String {
    match e {
        Error::Runtime { message, traceback } if !traceback.is_empty() => format!("{message}\n{traceback}"),
        Error::Limit(LimitKind::Memory) => String::from("out of memory (the app used more than its share)"),
        Error::Limit(LimitKind::Interrupted | LimitKind::Time) => String::from("interrupted"),
        other => format!("{other}"),
    }
}

/// Runs a Lua chunk in a fresh state. Returns 0, or -1 with the message
/// appended to `err` (a kernel `strbuf`).
///
/// # Safety
/// `name` and `code` must point to `name_len` and `len` readable bytes, and
/// `err` to a valid strbuf.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rl_run(name: *const u8, name_len: usize, code: *const u8, len: usize, mem_limit: usize, err: *mut c_void) -> i32 {
    let name = String::from_utf8_lossy(unsafe { core::slice::from_raw_parts(name, name_len) });
    let code = unsafe { core::slice::from_raw_parts(code, len) };
    let limits = Limits::unmetered().memory(mem_limit).native_depth(200).stack_bytes(400 << 10);
    let mut lua = Lua::new(limits, Libs::ALL | Libs::IO | Libs::REQUIRE | Libs::DEBUG_INFO);
    let result = setup(&mut lua).and_then(|()| lua.exec(code, &format!("@{name}")));
    match result {
        Ok(_) => 0,
        Err(e) => {
            let msg = describe(&e);
            unsafe { sys::sb_put(err, msg.as_ptr(), msg.len()) };
            -1
        }
    }
}
