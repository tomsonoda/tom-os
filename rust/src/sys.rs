//! The kernel's C interface, for the parts that are still C.
#![allow(dead_code)]

use core::ffi::{c_char, c_void};
unsafe extern "C" {
    pub fn malloc(n: usize) -> *mut u8;
    pub fn free(p: *mut u8);
    pub fn console_write(s: *const u8, n: usize);
    pub fn time_ms() -> u64;
    pub fn sb_put(b: *mut c_void, s: *const u8, n: usize);

    pub fn host_poll() -> i32;
    pub fn host_exit();
    pub fn host_owner() -> *mut c_void;
    pub fn host_panic(msg: *const u8, len: usize) -> !;
    pub fn host_sleep(ms: f64) -> i32;
    pub fn host_getkey(timeout_ms: i32, out: *mut u8) -> i32;
    pub fn host_readline(buf: *mut u8, max: usize) -> i32;


    pub fn sound_available() -> i32;
    pub fn sound_tone(freq: f64, ms: i32, vol: f64, wave: i32, delay_ms: i32, owner: *mut c_void) -> i32;
    pub fn sound_pcm(samples: *const f32, n: usize, rate: i32, vol: f64, delay_ms: i32, owner: *mut c_void) -> i32;
    pub fn sound_melody(notes: *const c_char, default_ms: i32, vol: f64, wave: i32, owner: *mut c_void) -> i32;
    pub fn sound_stop(owner: *mut c_void);
    pub fn sound_busy(owner: *mut c_void) -> i32;
    pub fn host_sound_wait() -> i32;

    // console and system
    pub fn console_clear_break();
    pub fn console_break_pending() -> i32;
    pub fn cpu_idle();
    pub fn cmdline_get(key: *const c_char, out: *mut u8, n: usize) -> i32;

    // apps, threads, windows
    pub fn lua_run(name: *const c_char, code: *const u8, len: usize, interactive: i32, timeout_ms: i32,
                   out: *mut c_void, err: *mut c_void) -> i32;
    pub fn thread_spawn(f: extern "C" fn(*mut c_void), arg: *mut c_void, tty: *mut c_void) -> *mut c_void;
    pub fn thread_kill(t: *mut c_void);
    pub fn thread_killed() -> i32;
    pub fn thread_set_window(w: usize);
    pub fn thread_window() -> usize;
    pub fn tty_term(tty: *mut c_void) -> *mut c_void;
    pub fn tty_home(tty: *mut c_void) -> *mut c_void;
    pub fn term_resize(term: *mut c_void, cols: i32, rows: i32);
    pub fn rtc_unix_time() -> u64;
    pub fn mouse_init();
    pub fn host_abort() -> i32;
    pub fn tty_create(cols: i32, rows: i32) -> *mut c_void;
    pub fn tty_destroy(t: *mut c_void);
    pub fn console_getc(timeout_ms: i32) -> i32;
    pub fn console_readline(buf: *mut u8, max: usize, history: i32) -> i32;
    pub fn thread_home() -> *mut c_void;
    pub fn tty_input(tty: *mut c_void, bytes: *const c_char);
    pub fn ime_enabled() -> i32;
    pub fn mouse_absolute() -> i32;
    pub fn mem_used() -> usize;
    pub fn mem_total() -> usize;
    pub fn cpu_poweroff();
    pub fn cpu_reboot();
}
