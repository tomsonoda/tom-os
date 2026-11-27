//! The kernel's C interface, for the parts that are still C.
#![allow(dead_code)]

use core::ffi::{c_char, c_void};
unsafe extern "C" {
    pub fn malloc(n: usize) -> *mut u8;
    pub fn free(p: *mut u8);
    pub fn time_ms() -> u64;



    pub fn sound_available() -> i32;
    pub fn sound_tone(freq: f64, ms: i32, vol: f64, wave: i32, delay_ms: i32, owner: *mut c_void) -> i32;
    pub fn sound_pcm(samples: *const f32, n: usize, rate: i32, vol: f64, delay_ms: i32, owner: *mut c_void) -> i32;
    pub fn sound_melody(notes: *const c_char, default_ms: i32, vol: f64, wave: i32, owner: *mut c_void) -> i32;
    pub fn sound_stop(owner: *mut c_void);
    pub fn sound_busy(owner: *mut c_void) -> i32;

    // console and system
    pub fn cpu_idle();
    pub fn cmdline_get(key: *const c_char, out: *mut u8, n: usize) -> i32;

    // apps, threads, windows
    pub fn thread_spawn(f: extern "C" fn(*mut c_void), arg: *mut c_void, tty: usize) -> *mut c_void;
    pub fn thread_tty() -> usize;
    pub fn thread_set_tty(tty: usize);
    pub fn thread_data() -> *mut c_void;
    pub fn thread_set_data(p: *mut c_void);
    pub fn sched_yield();
    pub fn sleep_ms(ms: u64);
    pub fn mem_init(multiboot_info: u32);
    pub fn cpu_init();
    pub fn sound_init();
    pub fn thread_kill(t: *mut c_void);
    pub fn thread_killed() -> i32;
    pub fn thread_set_window(w: usize);
    pub fn thread_window() -> usize;
    pub fn rtc_unix_time() -> u64;
    pub fn mem_used() -> usize;
    pub fn mem_total() -> usize;
    pub fn cpu_poweroff();
    pub fn cpu_reboot();
}
