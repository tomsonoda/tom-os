//! The browser: tom-os compiled to WebAssembly, with a web page as the
//! machine (web/tomos.js). The page's canvas is the display, its events
//! are the keyboard and pointer, and it carries requests to the LLM.
//!
//! WebAssembly has one call stack and cannot switch stacks or block, so
//! the build is passed through Binaryen's "asyncify", which lets a call to
//! the page unwind the stack into a buffer and rewind it later. That is how
//! threads are switched and how the kernel waits.

pub mod boot;
pub mod cpu;
pub mod display;
pub mod input;
pub mod llm;
pub mod serial;
pub mod sound;
pub mod thread;
pub mod virtio;

/// What the page provides (the imports of the WebAssembly module).
mod js {
    #[link(wasm_import_module = "env")]
    unsafe extern "C" {
        pub safe fn js_now() -> f64;
        pub safe fn js_unix_time() -> f64;
        pub safe fn js_random() -> f64;
        /// Leaves thread `from` and continues thread `to` (addresses of
        /// their control blocks, see thread.rs).
        pub safe fn js_switch(from: u32, to: u32);
        /// Returns to the page for `ms` milliseconds or until an event.
        pub safe fn js_wait(current: u32, ms: u32);
        pub safe fn js_power(kind: u32);
        pub safe fn js_serial_put(c: u32);
        pub safe fn js_serial_get() -> i32;
        /// The kernel arguments; returns their length.
        pub fn js_cmdline(out: *mut u8, max: usize) -> usize;
        /// The next input event into `out`; returns its kind (0: none).
        pub fn js_event(out: *mut i32) -> i32;
        /// The text of the last text event.
        pub fn js_event_text(out: *mut u8, max: usize) -> usize;
        /// The picture: `w` x `h` pixels at `pixels`.
        pub fn js_mode(pixels: *const u32, w: u32, h: u32);
        pub safe fn js_present(x0: u32, y0: u32, x1: u32, y1: u32);
        /// Starts a chat request with this JSON body.
        pub fn js_llm_start(body: *const u8, len: usize);
        /// -1 while the request is under way, else the length of the answer.
        pub safe fn js_llm_poll() -> i32;
        /// The HTTP status of the answer; 0 if the request failed (the
        /// answer is then the reason).
        pub safe fn js_llm_status() -> i32;
        pub fn js_llm_read(out: *mut u8);
        pub safe fn js_llm_abort();
    }
}
