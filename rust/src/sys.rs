//! The kernel's C interface, for the parts that are still C.
#![allow(dead_code)]

use core::ffi::{c_char, c_void};
unsafe extern "C" {
    pub fn malloc(n: usize) -> *mut u8;
    pub fn free(p: *mut u8);
    pub fn console_write(s: *const u8, n: usize);
    pub fn time_ms() -> u64;
    pub fn wm_active() -> i32;
    pub fn sb_put(b: *mut c_void, s: *const u8, n: usize);

    pub fn host_poll() -> i32;
    pub fn host_exit();
    pub fn host_owner() -> *mut c_void;
    pub fn host_panic(msg: *const u8, len: usize) -> !;
    pub fn host_sleep(ms: f64) -> i32;
    pub fn host_getkey(timeout_ms: i32, out: *mut u8) -> i32;
    pub fn host_readline(buf: *mut u8, max: usize) -> i32;

    pub fn host_gfx_mode(whole: i32, w: i32, h: i32, title: *const c_char, ow: *mut i32, oh: *mut i32) -> i32;
    pub fn host_gfx_clear(color: u32) -> i32;
    pub fn host_gfx_rect(x: i32, y: i32, w: i32, h: i32, color: u32, outline: i32) -> i32;
    pub fn host_gfx_pixel(x: i32, y: i32, color: u32) -> i32;
    pub fn host_gfx_line(x0: i32, y0: i32, x1: i32, y1: i32, color: u32) -> i32;
    pub fn host_gfx_circle(x: i32, y: i32, r: i32, color: u32, fill: i32) -> i32;
    pub fn host_gfx_text(x: i32, y: i32, s: *const u8, len: usize, color: u32, scale: i32) -> i32;
    pub fn host_gfx_flip(fps: f64) -> i32;
    pub fn host_gfx_key(name: *const c_char) -> i32;
    pub fn host_gfx_mouse(out: *mut i32) -> i32;
    pub fn host_gfx_size(out: *mut i32) -> i32;

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
    pub fn thread_set_window(w: *mut c_void);
    pub fn tty_create(cols: i32, rows: i32) -> *mut c_void;
    pub fn tty_destroy(t: *mut c_void);
    pub fn console_getc(timeout_ms: i32) -> i32;
    pub fn console_readline(buf: *mut u8, max: usize, history: i32) -> i32;
    pub fn wm_create(title: *const c_char, tty: *mut c_void, owner: *mut c_void) -> *mut c_void;
    pub fn wm_destroy(w: *mut c_void);
    pub fn wm_set_text(w: *mut c_void);
    pub fn wm_is_canvas(w: *mut c_void) -> i32;
    pub fn wm_set_owner(w: *mut c_void, thread: *mut c_void);
    pub fn wm_layout() -> i32;
    pub fn wm_set_layout(layout: i32);
    pub fn wm_display_w() -> i32;
    pub fn wm_display_h() -> i32;
    pub fn wm_scale() -> i32;
    pub fn wm_set_display(w: i32, h: i32, scale: i32, err: *mut c_void) -> i32;
    pub fn home_add_user_line(line: *const c_char);
    pub fn mouse_absolute() -> i32;
    pub fn mem_used() -> usize;
    pub fn mem_total() -> usize;
    pub fn cpu_poweroff();
    pub fn cpu_reboot();
}
