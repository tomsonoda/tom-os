//! Clocks, waiting and power, as the page provides them.

use core::sync::atomic::{AtomicU64, Ordering};

use super::{js, thread};

static STARTED: AtomicU64 = AtomicU64::new(0);
/// When the kernel last returned to the page (milliseconds).
static LAST_RETURN: AtomicU64 = AtomicU64::new(0);

pub fn init() {
    STARTED.store(js::js_now() as u64, Ordering::Relaxed);
}

/// Milliseconds since start-up.
pub fn time_ms() -> u64 {
    (js::js_now() as u64).saturating_sub(STARTED.load(Ordering::Relaxed))
}

/// Wall clock: seconds since 1970, in the time zone of the browser (the
/// status bar shows it as it is).
pub fn unix_time() -> u64 {
    js::js_unix_time() as u64
}

/// A number that is hard to predict.
pub fn entropy() -> u64 {
    (js::js_random() * 9_007_199_254_740_992.0) as u64 ^ (js::js_now() as u64) << 40
}

fn wait(ms: u32) {
    js::js_wait(thread::current(), ms);
    LAST_RETURN.store(time_ms(), Ordering::Relaxed);
}

/// Nothing to do: back to the page until something happens, 1 ms at the
/// latest (browsers may make that longer).
pub fn halt() {
    wait(1);
}

/// Called often by the scheduler. The page can only deliver events and
/// repaint while the kernel is not running, so a busy kernel returns to it
/// briefly every few milliseconds.
pub fn relax() {
    if time_ms().saturating_sub(LAST_RETURN.load(Ordering::Relaxed)) >= 12 {
        wait(0);
    }
}

/// Stops for good.
pub fn stop() -> ! {
    loop {
        wait(1000);
    }
}

pub fn poweroff() -> ! {
    js::js_power(0);
    stop()
}

pub fn reboot() -> ! {
    js::js_power(1);
    stop()
}
