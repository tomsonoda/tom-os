//! Kernel entry point.

use crate::rt::{self, ui};
use crate::{console, cpu, home, kprint, mem, net, shell, sound, wm};

/// Graphical mode unless gui=0 was passed or no suitable display exists.
fn start_graphics() {
    if mem::arg("gui").is_some_and(|v| v.starts_with('0')) {
        return;
    }
    let number = |s: &str| s.trim().parse::<i32>().unwrap_or(0);
    let (w, h) = mem::arg("res").and_then(|r| r.split_once('x').map(|(w, h)| (number(w), number(h)))).unwrap_or((1024, 768));
    let scale = mem::arg("scale").map_or(0, |s| number(&s));
    let mut gui = wm::init(w, h, scale);
    if !gui && (w, h, scale) != (1024, 768, 0) {
        kprint!("[display] {w}x{h} is not possible (at least 800x600 per scale step), using 1024x768\n");
        gui = wm::init(1024, 768, 1);
    }
    if !gui {
        return;
    }
    if mem::arg("layout").is_some_and(|v| v.starts_with('w')) {
        wm::set_layout(wm::LAYOUT_WINDOWS);
    }
    // the shell moves from the VGA text screen into the home screen
    // (its terminal is resized to fit the screen by wm::set_home)
    if console::shell_to_window(80, 30) {
        home::create();
        if let Some(window) = wm::create(b"tom-os", console::SHELL_TTY, 0) {
            wm::set_home(window);
        }
    }
}

/// Starts the kernel; called by the machine's boot code with what the
/// boot loader passed.
pub fn main(boot_arg: usize) -> ! {
    // SAFETY: the boot loader's information, used once, before anything allocates
    unsafe { mem::init(boot_arg) };
    console::init();
    cpu::init();
    net::init();
    sound::init();
    start_graphics();

    kprint!("{}\x1b[1mtom-os {}\x1b[0m - an operating system whose apps are written by an LLM\n", ui::HIDE, rt::VERSION);
    let memory = mem::total() >> 20;
    let network = if net::up() { "up" } else { "not found" };
    kprint!("{memory} MiB memory, network {network}. Type /help for commands.\n\n{}", ui::END);
    shell::run()
}
