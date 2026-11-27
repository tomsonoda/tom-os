//! Running apps. In text mode an app takes over the console until it exits.
//! In graphics mode each app gets its own thread, terminal and window.

use alloc::boxed::Box;
use alloc::format;
use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec::Vec;
use core::cell::{Cell, RefCell};

use crate::console::{self, TtyId};
use crate::rt::{KCell, lossy};
use crate::sched::{self, ThreadId};
use crate::{cpu, fs, kprint, run, wm};

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
    path: Vec<u8>,
    code: Vec<u8>, // private copy: the app may rewrite its own file
    tty: TtyId,
    window: wm::Id,
    outcome: Rc<Outcome>,
}

struct Running {
    id: i32,
    path: String,
    thread: ThreadId,
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
fn run_app(path: &[u8], code: &[u8]) -> Result<(), String> {
    run::lua_run(&lossy(path), code, true, 0).0
}

/// The app's thread.
fn app_main(app: App) {
    sched::set_window(app.window); // the scheduler keeps the id for the Lua gfx library
    let result = run_app(&app.path, &app.code);
    app.outcome.failed.set(result.is_err());
    if let Err(e) = &result {
        *app.outcome.error.borrow_mut() = e.clone();
    }
    app.outcome.done.set(true);

    if !sched::killed() {
        let graphical = wm::is_canvas(app.window);
        if result.is_err() || !graphical {
            // keep the window so the output or the error can be read
            if graphical {
                wm::set_text(app.window);
            }
            if let Err(e) = &result {
                kprint!("\x1b[0m\n\x1b[1;31m[error]\x1b[0m {e}\n");
            }
            let how = if result.is_err() { "stopped" } else { "finished" };
            kprint!("\x1b[0m\n[{how} - press a key to close this window]");
            console::clear_break();
            console::getc(-1);
        }
    }
    wm::destroy(app.window);
    console::tty_destroy(app.tty);
    RUNNING.with(|r| r.retain(|a| a.id != app.id));
}

fn launch_windowed(path: Vec<u8>, code: Vec<u8>) -> Result<Started, String> {
    if RUNNING.with(|r| r.len()) >= MAX_APPS {
        return Err(String::from("too many apps are running; close one first"));
    }
    let failed = || Err(String::from("could not start the app (out of memory or too many windows)"));
    let Some(tty) = console::tty_create(80, 25) else { return failed() };
    let Some(window) = wm::create(&path, tty, 0) else {
        console::tty_destroy(tty);
        return failed();
    };
    let id = NEXT_ID.with(|n| {
        *n += 1;
        *n - 1
    });
    let outcome = Rc::new(Outcome::default());
    let shown = lossy(&path);
    let app = App { id, path, code, tty, window, outcome: outcome.clone() };
    let Some(thread) = sched::spawn(Box::new(move || app_main(app)), tty) else {
        wm::destroy(window);
        console::tty_destroy(tty);
        return failed();
    };
    wm::set_owner(window, thread); // closing the window stops the app
    RUNNING.with(|r| r.push(Running { id, path: shown, thread, outcome: outcome.clone() }));

    // Give it a moment so start-up errors can be reported to the caller.
    let until = cpu::time_ms() + 2500;
    while !outcome.done.get() && cpu::time_ms() < until {
        sched::idle();
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
    if wm::active() {
        return launch_windowed(path, code);
    }
    let result = run_app(&path, &code);
    console::write(b"\x1b[0m\n");
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
        console::write(b"  (no apps are running)\n");
    }
    for line in lines {
        console::write(line.as_bytes());
    }
}

/// Asks an app to stop; false if there is no app with this id.
pub fn kill(id: i32) -> bool {
    let thread = RUNNING.with(|r| r.iter().find(|a| a.id == id).map(|a| a.thread));
    if let Some(t) = thread {
        sched::kill(t);
    }
    thread.is_some()
}
