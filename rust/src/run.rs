//! Running a Lua program on the calling thread: the state of the run (how
//! it can be stopped) and what the Lua libraries need from the kernel.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;
use core::cell::Cell;
use core::ffi::c_void;

use crate::console::{self, KEY_BREAK, KEY_TIMEOUT};
use crate::{lua, sys};

/// Why a run has to stop.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Stop {
    /// Ctrl-C, or the app's window was closed
    Break,
    Timeout,
    /// os.exit()
    Exit,
}

/// The state of one run. Several apps can run at once, each on its own
/// thread, so the scheduler keeps a pointer to it for the thread. Its
/// address also names the app's sounds.
struct Run {
    /// sticky
    stop: Cell<Option<Stop>>,
    /// console input allowed
    interactive: bool,
    /// 0 = none
    deadline: u64,
}

/// The run on the calling thread.
fn current<'a>() -> Option<&'a Run> {
    // SAFETY: set by `lua_run`, which keeps the Run alive until it has
    // cleared the pointer again, on this same thread
    unsafe { (sys::thread_data() as *const Run).as_ref() }
}

/// Checks whether the run must stop; the answer is sticky.
pub fn stopped() -> Option<Stop> {
    let Some(run) = current() else { return Some(Stop::Break) };
    if run.stop.get().is_none() {
        if unsafe { sys::thread_killed() } != 0 || console::break_pending() {
            run.stop.set(Some(Stop::Break));
        } else if run.deadline != 0 && unsafe { sys::time_ms() } > run.deadline {
            run.stop.set(Some(Stop::Timeout));
        }
    }
    run.stop.get()
}

/// Called by the interpreter every few thousand steps; true if the run
/// must stop.
pub fn poll() -> bool {
    if stopped().is_some() {
        return true;
    }
    unsafe { sys::sched_yield() }; // share the CPU with other apps and the shell
    false
}

/// os.exit(): the run stops without an error.
pub fn exit() {
    if let Some(run) = current() {
        run.stop.set(Some(Stop::Exit));
    }
}

fn set_break() {
    if let Some(run) = current() {
        run.stop.set(Some(Stop::Break));
    }
}

fn interactive() -> bool {
    current().is_some_and(|r| r.interactive)
}

/// What the sound driver knows this run's sounds by.
pub fn owner() -> *mut c_void {
    unsafe { sys::thread_data() }
}

/// Waits; true if the run was stopped meanwhile.
pub fn sleep(ms: f64) -> bool {
    let end = unsafe { sys::time_ms() } + ms.max(0.0) as u64;
    while unsafe { sys::time_ms() } < end {
        if stopped().is_some() {
            return true;
        }
        unsafe { sys::cpu_idle() };
    }
    false
}

pub enum Key {
    /// a key name ("up", "enter", ...) or the character typed
    Pressed(Vec<u8>),
    /// nothing in time, or there is no keyboard
    None,
    Stopped,
}

/// One key; `timeout_ms` < 0 waits.
pub fn getkey(timeout_ms: i32) -> Key {
    if !interactive() {
        return Key::None;
    }
    let now = || unsafe { sys::time_ms() };
    let end = if timeout_ms < 0 { u64::MAX } else { now() + timeout_ms as u64 };
    let c = loop {
        // in slices, so a time limit or a closed window still applies
        let c = console::getc(end.saturating_sub(now()).min(50) as i32);
        if c != KEY_TIMEOUT {
            break c;
        }
        if stopped().is_some() {
            return Key::Stopped;
        }
        if now() >= end {
            return Key::None;
        }
    };
    let name: &[u8] = match c {
        KEY_BREAK => {
            set_break();
            return Key::Stopped;
        }
        0x1B => match console::getc(30) {
            0x5B | 0x4F => match console::getc(30) {
                0x41 => b"up",
                0x42 => b"down",
                0x43 => b"right",
                0x44 => b"left",
                _ => b"esc",
            },
            _ => b"esc",
        },
        0x0A => b"enter",
        0x08 => b"backspace",
        0x09 => b"tab",
        _ => {
            // a character: UTF-8 arrives byte by byte
            let mut ch = alloc::vec![c as u8];
            let extra = if c >= 0xF0 { 3 } else if c >= 0xE0 { 2 } else if c >= 0xC0 { 1 } else { 0 };
            for _ in 0..extra {
                match console::getc(30) {
                    cc if cc < 0 => break,
                    cc => ch.push(cc as u8),
                }
            }
            return Key::Pressed(ch);
        }
    };
    Key::Pressed(name.to_vec())
}

pub enum Line {
    Text(Vec<u8>),
    /// no keyboard
    EndOfInput,
    Stopped,
}

/// A line of input, edited by the user.
pub fn readline() -> Line {
    if !interactive() {
        return Line::EndOfInput;
    }
    match console::readline(1023, false) {
        Some(text) => Line::Text(text),
        None => {
            set_break();
            Line::Stopped
        }
    }
}

/// Waits until the run's sounds have played; true if the run was stopped.
pub fn sound_wait() -> bool {
    while unsafe { sys::sound_busy(owner()) } != 0 {
        if stopped().is_some() {
            return true;
        }
        unsafe { sys::cpu_idle() };
    }
    false
}

/// Runs a Lua program on the calling thread.
///
/// Interactive runs use the console; the others have no keyboard and their
/// output (up to 16 KiB) is returned. `timeout_ms` 0 means no time limit.
pub fn lua_run(name: &str, code: &[u8], interactive: bool, timeout_ms: u64) -> (Result<(), String>, Vec<u8>) {
    if current().is_some() {
        return (Err(String::from("a Lua program is already running on this thread")), Vec::new());
    }
    let now = unsafe { sys::time_ms() };
    let deadline = if timeout_ms > 0 { now + timeout_ms } else { 0 };
    let run = Box::new(Run { stop: Cell::new(None), interactive, deadline });
    let key = &*run as *const Run as *mut c_void;
    unsafe { sys::thread_set_data(key) };
    console::clear_break();
    if !interactive {
        console::capture_start(16384);
    }

    let free = unsafe { sys::mem_total() - sys::mem_used() };
    let result = match (lua::exec(name, code, free / 2), run.stop.get()) {
        (Err(_), Some(Stop::Exit)) => Ok(()),
        (Err(_), Some(Stop::Timeout)) => Err(String::from("time limit exceeded")),
        (Err(_), Some(Stop::Break)) => Err(String::from("interrupted (Ctrl-C)")),
        (result, _) => result,
    };
    // Sounds may outlive a program that ends normally (a short jingle);
    // a stopped one is silenced.
    if matches!(run.stop.get(), Some(Stop::Break | Stop::Timeout)) || unsafe { sys::thread_killed() } != 0 {
        unsafe { sys::sound_stop(key) };
    }
    let output = if interactive { Vec::new() } else { console::capture_end() };
    unsafe { sys::thread_set_data(core::ptr::null_mut()) };
    (result, output)
}
