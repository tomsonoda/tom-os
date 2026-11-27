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
use core::time::Duration;

use remedy_lua::{Error, Interrupt, IoError, Libs, LimitKind, Limits, Lua, ModuleSource, Stream, Value};

use crate::sound::{self, Wave};
use crate::{cpu, fs};
use crate::run::{self, Key, Line};
use crate::gfx::Surface;
use crate::wm::{self, GfxError};
use crate::rt::console;

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

fn gfx_error(e: GfxError) -> Error {
    Error::runtime(match e {
        GfxError::Unavailable => {
            "graphics are not available here (needs the GUI and an app started with /run or launch_app)"
        }
        GfxError::NoCanvas => "call gfx.mode(width, height) first",
        GfxError::OutOfMemory => "out of memory",
    })
}

/// Draws on the app's canvas.
fn draw(f: impl FnOnce(&mut Surface)) -> R {
    wm::app_draw(f).map_err(gfx_error)?;
    Ok(vec![])
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
            match run::readline() {
                Line::Text(text) => self.line = text,
                Line::EndOfInput => return Ok(0),
                Line::Stopped => {
                    self.intr.interrupt();
                    return Ok(0);
                }
            }
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
        if run::sleep(ms) {
            return stop(&i);
        }
        Ok(vec![])
    })?;
    let i = intr.clone();
    lua.register("tomos.getkey", move |lua, a| {
        let timeout = if is_given(a, 0) { int(lua, a, 0)?.max(0) } else { -1 };
        match run::getkey(timeout) {
            Key::Stopped => stop(&i),
            Key::None => Ok(vec![Value::Nil]),
            Key::Pressed(key) => Ok(vec![lua.create_string(key)?]),
        }
    })?;
    let i = intr.clone();
    lua.register("tomos.readline", move |lua, a| {
        if is_given(a, 0) {
            console(&bytes(lua, a, 0)?);
        }
        match run::readline() {
            Line::Stopped => stop(&i),
            Line::EndOfInput => Ok(vec![Value::Nil]),
            Line::Text(text) => Ok(vec![lua.create_string(text)?]),
        }
    })?;
    lua.register("tomos.time_ms", |_, _| Ok(vec![Value::Int(cpu::time_ms() as i64)]))?;
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
        run::exit();
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
        let size = if whole { None } else { Some((int(lua, a, 0)?, int(lua, a, 1)?)) };
        let title_at = if whole { 0 } else { 2 };
        let title = if matches!(a.get(title_at), Some(Value::Str(_))) { Some(bytes(lua, a, title_at)?) } else { None };
        let (w, h) = wm::app_mode(size, title.as_deref()).map_err(gfx_error)?;
        Ok(vec![Value::Int(w as i64), Value::Int(h as i64)])
    })?;
    lua.register("gfx.clear", |lua, a| {
        let c = color(lua, a, 0, 0)?;
        draw(|s| s.fill(0, 0, s.w, s.h, c))
    })?;
    lua.register("gfx.rect", |lua, a| {
        let (x, y, w, h) = (int(lua, a, 0)?, int(lua, a, 1)?, int(lua, a, 2)?, int(lua, a, 3)?);
        let c = color(lua, a, 4, 0xFF_FFFF)?;
        draw(|s| s.fill(x, y, w, h, c))
    })?;
    lua.register("gfx.box", |lua, a| {
        let (x, y, w, h) = (int(lua, a, 0)?, int(lua, a, 1)?, int(lua, a, 2)?, int(lua, a, 3)?);
        let c = color(lua, a, 4, 0xFF_FFFF)?;
        draw(|s| {
            s.fill(x, y, w, 1, c);
            s.fill(x, y.saturating_add(h).saturating_sub(1), w, 1, c);
            s.fill(x, y, 1, h, c);
            s.fill(x.saturating_add(w).saturating_sub(1), y, 1, h, c);
        })
    })?;
    lua.register("gfx.pixel", |lua, a| {
        let (x, y, c) = (int(lua, a, 0)?, int(lua, a, 1)?, color(lua, a, 2, 0xFF_FFFF)?);
        draw(|s| s.pixel(x, y, c))
    })?;
    lua.register("gfx.line", |lua, a| {
        let (x0, y0, x1, y1) = (int(lua, a, 0)?, int(lua, a, 1)?, int(lua, a, 2)?, int(lua, a, 3)?);
        let c = color(lua, a, 4, 0xFF_FFFF)?;
        draw(|s| s.line(x0, y0, x1, y1, c))
    })?;
    lua.register("gfx.circle", |lua, a| {
        let fill = a.get(4).is_some_and(|v| v.truthy());
        let (x, y, r, c) = (int(lua, a, 0)?, int(lua, a, 1)?, int(lua, a, 2)?, color(lua, a, 3, 0xFF_FFFF)?);
        draw(|s| s.circle(x, y, r, c, fill))
    })?;
    lua.register("gfx.text", |lua, a| {
        let text = display(lua, a.get(2));
        let (x, y) = (int(lua, a, 0)?, int(lua, a, 1)?);
        let (c, scale) = (color(lua, a, 3, 0xFF_FFFF)?, opt_int(lua, a, 4, 1)?);
        draw(|s| s.text(x, y, &text, c, None, scale))
    })?;
    let i = intr.clone();
    lua.register("gfx.flip", move |lua, a| {
        if wm::app_flip(opt_num(lua, a, 0, 60.0)?).map_err(gfx_error)? {
            return stop(&i);
        }
        Ok(vec![])
    })?;
    lua.register("gfx.key", |lua, a| {
        let name = bytes(lua, a, 0)?;
        match wm::app_key(&name).map_err(gfx_error)? {
            Some(held) => Ok(vec![Value::Bool(held)]),
            None => Err(Error::runtime(format!("unknown key name '{}'", String::from_utf8_lossy(&name)))),
        }
    })?;
    lua.register("gfx.mouse", |_, _| {
        let (x, y, left, right) = wm::app_mouse().map_err(gfx_error)?;
        Ok(vec![Value::Int(x as i64), Value::Int(y as i64), Value::Bool(left), Value::Bool(right)])
    })?;
    lua.register("gfx.size", |_, _| {
        let (w, h) = wm::app_size().map_err(gfx_error)?;
        Ok(vec![Value::Int(w as i64), Value::Int(h as i64)])
    })?;
    lua.register("gfx.rgb", |lua, a| {
        let c = |i| -> Result<i64, Error> { Ok(int(lua, a, i)?.clamp(0, 255) as i64) };
        Ok(vec![Value::Int((c(0)? << 16) | (c(1)? << 8) | c(2)?)])
    })?;
    Ok(())
}

const WAVES: [(&str, Wave); 5] =
    [("square", Wave::Square), ("sine", Wave::Sine), ("triangle", Wave::Triangle), ("saw", Wave::Saw), ("noise", Wave::Noise)];

fn wave_named(lua: &Lua, v: Value, i: usize) -> Result<Wave, Error> {
    let name = lua.to_string_lossy(v).unwrap_or_default();
    WAVES.iter().find(|w| w.0 == name).map(|w| w.1).ok_or_else(|| {
        Error::runtime(format!("bad argument #{} (invalid wave '{}': square, sine, triangle, saw or noise)", i + 1, name))
    })
}

/// Optional sound arguments from index `i`: (wave, volume) or a table
/// {wave=, volume=, delay=}. Returns (wave, volume, delay_ms).
fn sound_opts(lua: &mut Lua, a: &[Value], i: usize) -> Result<(Wave, f64, i32), Error> {
    let (mut wave, mut vol, mut delay) = (Wave::Square, 0.5, 0);
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
    lua.register("sound.available", |_, _| Ok(vec![Value::Bool(sound::available())]))?;
    lua.register("sound.tone", |lua, a| {
        let (freq, ms) = (num(lua, a, 0)?, int(lua, a, 1)?);
        let (wave, vol, delay) = sound_opts(lua, a, 2)?;
        let ok = sound::tone(freq, ms, vol, wave, delay, run::owner());
        Ok(vec![Value::Bool(ok)])
    })?;
    lua.register("sound.note", |lua, a| {
        let name = bytes(lua, a, 0)?;
        let ms = opt_int(lua, a, 1, 200)?;
        let (wave, vol, _) = sound_opts(lua, a, 2)?;
        let mut spec = name;
        spec.extend_from_slice(format!(":{ms}").as_bytes());
        sound::melody(&spec, ms, vol, wave, run::owner());
        Ok(vec![])
    })?;
    lua.register("sound.melody", |lua, a| {
        let notes = bytes(lua, a, 0)?;
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
        let total = sound::melody(&notes, len, vol, wave, run::owner());
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
        let ok = sound::pcm(&samples, rate, vol, 0, run::owner());
        Ok(vec![Value::Bool(ok)])
    })?;
    lua.register("sound.beep", |_, _| {
        sound::tone(880.0, 90, 0.4, Wave::Square, 0, run::owner());
        Ok(vec![])
    })?;
    lua.register("sound.stop", |_, _| {
        sound::stop(run::owner());
        Ok(vec![])
    })?;
    lua.register("sound.playing", |_, _| Ok(vec![Value::Bool(sound::busy(run::owner()))]))?;
    let i = intr.clone();
    lua.register("sound.wait", move |_, _| {
        if run::sound_wait() {
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
        if run::poll() {
            i.interrupt();
        }
        Duration::from_millis(cpu::time_ms())
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
    let gui = Value::Bool(wm::active());
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

/// Runs a program in a fresh Lua state that may use up to `mem_limit`
/// bytes; an error comes back as its message with a traceback.
pub fn exec(name: &str, code: &[u8], mem_limit: usize) -> Result<(), String> {
    let limits = Limits::unmetered().memory(mem_limit).native_depth(200).stack_bytes(400 << 10);
    let mut lua = Lua::new(limits, Libs::ALL | Libs::IO | Libs::REQUIRE | Libs::DEBUG_INFO);
    match setup(&mut lua).and_then(|()| lua.exec(code, &format!("@{name}"))) {
        Ok(_) => Ok(()),
        Err(e) => Err(describe(&e)),
    }
}
