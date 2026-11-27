//! Cooperative threads. A thread runs until it calls `yield_now` or
//! `idle`; the Lua runtime yields every few thousand steps, so running
//! apps share the CPU without being able to hog it.

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::arch::thread;
use crate::rt::KCell;
use crate::{console, cpu, net, sound, wm};

const MAX_THREADS: usize = 16;
const STACK_SIZE: usize = 1024 * 1024;

/// Names a thread; never 0, never reused.
pub type ThreadId = usize;

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Free,
    Running,
    Dead,
}

struct Thread {
    id: ThreadId,
    /// the saved stack pointer while the thread is not running
    rsp: u64,
    /// owned here so that it is freed with the thread
    #[allow(dead_code)]
    stack: Vec<u8>,
    state: State,
    /// last gave up the CPU because it was waiting
    idle: bool,
    /// asked to stop
    kill: bool,
    /// the console the thread writes to (console::TtyId)
    tty: usize,
    /// the window of the thread's app (wm::Id), or 0
    window: usize,
    /// a slot for the Lua runtime
    data: usize,
    /// what a new thread runs
    entry: Option<Box<dyn FnOnce()>>,
}

const FREE: Thread =
    Thread { id: 0, rsp: 0, stack: Vec::new(), state: State::Free, idle: false, kill: false, tty: 0, window: 0, data: 0, entry: None };

struct Sched {
    threads: [Thread; MAX_THREADS],
    current: usize,
    next_id: ThreadId,
}

/// Slot 0 is the boot thread. The state is never borrowed across a switch.
static SCHED: KCell<Sched> = KCell::new(Sched {
    threads: {
        let mut t = [FREE; MAX_THREADS];
        t[0].state = State::Running;
        t[0].id = 1;
        t
    },
    current: 0,
    next_id: 2,
});

fn me<R>(f: impl FnOnce(&mut Thread) -> R) -> R {
    SCHED.with(|s| f(&mut s.threads[s.current]))
}

pub fn tty() -> usize {
    me(|t| t.tty)
}
pub fn set_tty(tty: usize) {
    me(|t| t.tty = tty)
}
pub fn window() -> usize {
    me(|t| t.window)
}
pub fn set_window(window: usize) {
    me(|t| t.window = window)
}
pub fn data() -> usize {
    me(|t| t.data)
}
pub fn set_data(data: usize) {
    me(|t| t.data = data)
}

/// Has the calling thread been asked to stop?
pub fn killed() -> bool {
    me(|t| t.kill)
}

/// Asks a thread to stop. It notices when it next waits or polls.
pub fn kill(id: ThreadId) {
    SCHED.with(|s| {
        if let Some(t) = s.threads.iter_mut().find(|t| t.id == id && t.state == State::Running) {
            t.kill = true;
        }
    })
}

static LAST_TICK: AtomicU64 = AtomicU64::new(u64::MAX);
static IN_TICK: AtomicBool = AtomicBool::new(false);

/// Device polling shared by every thread, at most once per millisecond.
fn tick() {
    let now = cpu::time_ms();
    if LAST_TICK.load(Ordering::Relaxed) == now || IN_TICK.swap(true, Ordering::Relaxed) {
        return;
    }
    LAST_TICK.store(now, Ordering::Relaxed);
    console::poll_input();
    net::poll();
    sound::poll();
    wm::tick();
    IN_TICK.store(false, Ordering::Relaxed);
}

fn switch_next(idle: bool) {
    me(|t| t.idle = idle);
    tick();
    cpu::relax();

    let all_idle = SCHED.with(|s| {
        let current = s.current;
        for (i, t) in s.threads.iter_mut().enumerate() {
            if t.state == State::Dead && i != current {
                *t = FREE; // reap: never free the stack we are on
            }
        }
        s.threads.iter().all(|t| t.state != State::Running || t.idle)
    });
    if idle && all_idle {
        cpu::halt();
    }

    let switch = SCHED.with(|s| {
        let from = s.current;
        let to = (1..=MAX_THREADS).map(|i| (from + i) % MAX_THREADS).find(|&i| s.threads[i].state == State::Running)?;
        if to == from {
            return None;
        }
        s.current = to;
        Some((&raw mut s.threads[from].rsp, s.threads[to].rsp))
    });
    if let Some((save, new)) = switch {
        // SAFETY: `save` points into the static thread table; `new` was
        // saved by an earlier switch or built by `spawn`. The scheduler
        // state is not borrowed here.
        unsafe { thread::ctx_switch(save, new) };
    }
}

/// Waits for `ms` milliseconds, letting other threads run.
pub fn sleep_ms(ms: u64) {
    let end = cpu::time_ms() + ms;
    while cpu::time_ms() < end {
        net::poll();
        idle();
    }
}

/// Lets other threads run; the caller stays busy.
pub fn yield_now() {
    switch_next(false)
}

/// The caller is waiting: runs the others, and sleeps the CPU if everyone
/// waits.
pub fn idle() {
    switch_next(true)
}

fn exit() -> ! {
    me(|t| t.state = State::Dead);
    loop {
        switch_next(true);
    }
}

extern "C" fn thread_entry() -> ! {
    if let Some(f) = me(|t| t.entry.take()) {
        f();
    }
    exit()
}

/// Starts a thread that runs `f` and writes to the console `tty`. None if
/// there are too many threads or memory is short.
pub fn spawn(f: Box<dyn FnOnce()>, tty: usize) -> Option<ThreadId> {
    let mut stack = Vec::new();
    stack.try_reserve_exact(STACK_SIZE).ok()?;
    stack.resize(STACK_SIZE, 0u8);
    let rsp = thread::first_frame(&mut stack, thread_entry);
    SCHED.with(|s| {
        let id = s.next_id;
        let slot = s.threads.iter_mut().skip(1).find(|t| t.state == State::Free)?;
        *slot = Thread { id, rsp, stack, state: State::Running, tty, entry: Some(f), ..FREE };
        s.next_id += 1;
        Some(id)
    })
}
