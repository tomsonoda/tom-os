//! Running apps. In text mode an app takes over the console until it exits.
//! In graphics mode each app gets its own thread, terminal and window.

use alloc::boxed::Box;
use alloc::format;
use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec::Vec;
use core::cell::{Cell, RefCell};
use core::ffi::{c_char, c_void};

use crate::rt::{KCell, StrBuf, console, cstr, lossy};
use crate::{fs, kprint, sys};

const MAX_APPS: usize = 10;

/// How a run ended, shared between the app's thread and whoever started it.
#[derive(Default)]
struct Outcome {
    done: Cell<bool>,
    failed: Cell<bool>,
    error: RefCell<String>,
}

/// What the app's thread owns.
struct App {
    id: i32,
    path: Vec<u8>, // NUL-terminated
    code: Vec<u8>, // private copy: the app may rewrite its own file
    tty: *mut c_void,
    window: *mut c_void,
    outcome: Rc<Outcome>,
}

struct Running {
    id: i32,
    path: String,
    thread: *mut c_void,
    outcome: Rc<Outcome>,
}

static RUNNING: KCell<Vec<Running>> = KCell::new(Vec::new());
static NEXT_ID: KCell<i32> = KCell::new(1);

pub enum Started {
    /// The app ran and has exited.
    Finished,
    /// The app is running in its own window.
    Running,
}

/// Finds the source for "name", "apps/name.lua" or a full path.
fn resolve(name: &[u8]) -> Option<(Vec<u8>, Vec<u8>)> {
    let name = lossy(name);
    [format!("{name}"), format!("apps/{name}.lua"), format!("apps/{name}")]
        .into_iter()
        .find_map(|path| fs::read(path.as_bytes()).map(|code| (path.into_bytes(), code)))
}

/// Runs a program on the calling thread, with the keyboard.
fn run(path: &[u8], code: &[u8]) -> Result<(), String> {
    let mut err = StrBuf::new();
    let r = unsafe {
        sys::lua_run(path.as_ptr() as *const c_char, code.as_ptr(), code.len(), 1, 0, core::ptr::null_mut(), err.as_ptr())
    };
    if r == 0 { Ok(()) } else { Err(lossy(err.as_bytes())) }
}

extern "C" fn app_main(arg: *mut c_void) {
    // SAFETY: `arg` is the Box that launch_windowed handed to this thread
    let app = unsafe { Box::from_raw(arg as *mut App) };
    unsafe { sys::thread_set_window(app.window) };
    let result = run(&app.path, &app.code);
    app.outcome.failed.set(result.is_err());
    if let Err(e) = &result {
        *app.outcome.error.borrow_mut() = e.clone();
    }
    app.outcome.done.set(true);

    if unsafe { sys::thread_killed() } == 0 {
        let graphical = unsafe { sys::wm_is_canvas(app.window) } != 0;
        if result.is_err() || !graphical {
            // keep the window so the output or the error can be read
            if graphical {
                unsafe { sys::wm_set_text(app.window) };
            }
            if let Err(e) = &result {
                kprint!("\x1b[0m\n\x1b[1;31m[error]\x1b[0m {e}\n");
            }
            let how = if result.is_err() { "stopped" } else { "finished" };
            kprint!("\x1b[0m\n[{how} - press a key to close this window]");
            unsafe {
                sys::console_clear_break();
                sys::console_getc(-1);
            }
        }
    }
    unsafe {
        sys::wm_destroy(app.window);
        sys::tty_destroy(app.tty);
    }
    RUNNING.with(|r| r.retain(|a| a.id != app.id));
}

fn launch_windowed(path: Vec<u8>, code: Vec<u8>) -> Result<Started, String> {
    if RUNNING.with(|r| r.len()) >= MAX_APPS {
        return Err(String::from("too many apps are running; close one first"));
    }
    let failed = || Err(String::from("could not start the app (out of memory or too many windows)"));
    let title = cstr(&path);
    let tty = unsafe { sys::tty_create(80, 25) };
    if tty.is_null() {
        return failed();
    }
    let window = unsafe { sys::wm_create(title.as_ptr() as *const c_char, tty, core::ptr::null_mut()) };
    if window.is_null() {
        unsafe { sys::tty_destroy(tty) };
        return failed();
    }
    let id = NEXT_ID.with(|n| {
        *n += 1;
        *n - 1
    });
    let outcome = Rc::new(Outcome::default());
    let shown = lossy(&path);
    let app = Box::into_raw(Box::new(App { id, path: title, code, tty, window, outcome: outcome.clone() }));
    let thread = unsafe { sys::thread_spawn(app_main, app as *mut c_void, tty) };
    if thread.is_null() {
        // SAFETY: the thread was not created, so the Box is still ours
        drop(unsafe { Box::from_raw(app) });
        unsafe {
            sys::wm_destroy(window);
            sys::tty_destroy(tty);
        }
        return failed();
    }
    unsafe { sys::wm_set_owner(window, thread) }; // closing the window stops the app
    RUNNING.with(|r| r.push(Running { id, path: shown, thread, outcome: outcome.clone() }));

    // Give it a moment so start-up errors can be reported to the caller.
    let until = unsafe { sys::time_ms() } + 2500;
    while !outcome.done.get() && unsafe { sys::time_ms() } < until {
        unsafe { sys::cpu_idle() };
    }
    if !outcome.done.get() {
        Ok(Started::Running)
    } else if outcome.failed.get() {
        Err(outcome.error.borrow().clone())
    } else {
        Ok(Started::Finished)
    }
}

/// Starts an app by name or path.
pub fn launch(name: &[u8]) -> Result<Started, String> {
    let Some((path, code)) = resolve(name) else {
        return Err(format!("no such app: {}", lossy(name)));
    };
    if unsafe { sys::wm_active() } != 0 {
        return launch_windowed(path, code);
    }
    let result = run(&cstr(&path), &code);
    console(b"\x1b[0m\n");
    result.map(|()| Started::Finished)
}

/// Prints the running apps.
pub fn list() {
    let lines: Vec<String> = RUNNING.with(|r| {
        r.iter()
            .map(|a| format!("  {:2}  {}{}\n", a.id, a.path, if a.outcome.done.get() { "  (finished)" } else { "" }))
            .collect()
    });
    if lines.is_empty() {
        console(b"  (no apps are running)\n");
    }
    for line in lines {
        console(line.as_bytes());
    }
}

/// Asks an app to stop; false if there is no app with this id.
pub fn kill(id: i32) -> bool {
    let thread = RUNNING.with(|r| r.iter().find(|a| a.id == id).map(|a| a.thread));
    if let Some(t) = thread {
        unsafe { sys::thread_kill(t) };
    }
    thread.is_some()
}
