//! The kernel's C interface, for the parts that are still C.
#![allow(dead_code)]

use core::ffi::{c_char, c_void};
unsafe extern "C" {
    pub fn malloc(n: usize) -> *mut u8;
    pub fn free(p: *mut u8);
    pub fn console_write(s: *const u8, n: usize);
    pub fn time_ms() -> u64;
    pub fn wm_active() -> i32;
    pub fn fs_read(path: *const c_char, len: *mut usize) -> *const u8;
    pub fn fs_write(path: *const c_char, data: *const u8, len: usize) -> i32;
    pub fn fs_delete(path: *const c_char) -> i32;
    pub fn fs_rename(from: *const c_char, to: *const c_char) -> i32;
    pub fn fs_next(it: *mut *mut c_void, len: *mut usize) -> *const c_char;
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
    pub fn cmdline_get(key: *const c_char, out: *mut u8, n: usize) -> i32;

    // network (net/net.c, net/tcp.c)
    pub fn net_up() -> i32;
    pub fn net_parse_ip(s: *const c_char, ip: *mut u32) -> i32;
    pub fn net_strerror(e: i32) -> *const c_char;
    pub fn tcp_connect(ip: u32, port: u16, timeout_ms: i32) -> i32;
    pub fn tcp_send(data: *const u8, len: usize) -> i32;
    pub fn tcp_recv(buf: *mut u8, max: usize, timeout_ms: i32) -> i32;
    pub fn tcp_close();
    pub static mut net_wait_hook: Option<extern "C" fn()>;

    // apps
    pub fn lua_run(name: *const c_char, code: *const u8, len: usize, interactive: i32, timeout_ms: i32,
                   out: *mut c_void, err: *mut c_void) -> i32;
    pub fn app_launch(name: *const c_char, err: *mut c_void) -> i32;
}

pub const NET_ETIMEOUT: i32 = -2;
pub const NET_EPROTO: i32 = -6;
pub const NET_EBUSY: i32 = -7;
