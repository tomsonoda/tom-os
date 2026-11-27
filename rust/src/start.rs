//! Kernel entry point.

use core::ffi::CStr;

use crate::rt::{self, ui};
use crate::{console, home, kprint, net, shell, sys, wm};

/// A kernel argument (`key=value` on the command line), if given.
fn arg(key: &CStr) -> Option<alloc::string::String> {
    let mut v = [0u8; 32];
    if unsafe { sys::cmdline_get(key.as_ptr(), v.as_mut_ptr(), v.len()) } == 0 {
        return None;
    }
    let end = v.iter().position(|&b| b == 0).unwrap_or(v.len());
    Some(rt::lossy(&v[..end]))
}

/// Graphical mode unless gui=0 was passed or no suitable display exists.
fn start_graphics() {
    if arg(c"gui").is_some_and(|v| v.starts_with('0')) {
        return;
    }
    let number = |s: &str| s.trim().parse::<i32>().unwrap_or(0);
    let (w, h) = arg(c"res").and_then(|r| r.split_once('x').map(|(w, h)| (number(w), number(h)))).unwrap_or((1024, 768));
    let scale = arg(c"scale").map_or(0, |s| number(&s));
    let mut gui = wm::init(w, h, scale);
    if !gui && (w, h, scale) != (1024, 768, 0) {
        kprint!("[display] {w}x{h} is not possible (at least 800x600 per scale step), using 1024x768\n");
        gui = wm::init(1024, 768, 1);
    }
    if !gui {
        return;
    }
    if arg(c"layout").is_some_and(|v| v.starts_with('w')) {
        wm::set_layout(wm::LAYOUT_WINDOWS);
    }
    // the shell moves from the VGA text screen into the home screen
    // (its terminal is resized to fit the screen by wm::set_home)
    if console::shell_to_window(80, 30) {
        home::create();
        if let Some(window) = wm::create(b"tom-os", console::SHELL_TTY, core::ptr::null_mut()) {
            wm::set_home(window);
        }
    }
}

/// Called by the boot code (arch/x86_64/boot.asm) once the CPU is in
/// 64-bit mode, with what the boot loader passed.
#[unsafe(no_mangle)]
pub extern "C" fn kmain(multiboot_info: u32, magic: u32) {
    if magic != 0x2BAD_B002 {
        console::write(b"tom-os: not started by a Multiboot loader\n");
        return;
    }
    // SAFETY: start-up, in this order: the heap first, then everything else
    unsafe { sys::mem_init(multiboot_info) };
    console::init();
    unsafe { sys::cpu_init() };
    net::net_init();
    unsafe { sys::sound_init() };
    start_graphics();

    kprint!("{}\x1b[1mtom-os {}\x1b[0m - an operating system whose apps are written by an LLM\n", ui::HIDE, rt::VERSION);
    let memory = unsafe { sys::mem_total() } >> 20;
    let network = if net::up() { "up" } else { "not found" };
    kprint!("{memory} MiB memory, network {network}. Type /help for commands.\n\n{}", ui::END);
    shell::run();
}
