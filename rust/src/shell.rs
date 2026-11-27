//! The top-level prompt: slash commands are handled locally, everything
//! else goes to the agent.


use crate::apps::{self, Started};
use crate::rt::{self, console, lossy, ui};
use crate::{agent, cpu, fs, home, kprint, mem, net, run, sound, wm};

const HELP: &str = "Type what you want in plain language; the agent answers or builds an app for it.
Commands:
  /apps          list files (apps are saved as apps/<name>.lua)
  /run <name>    start an app (Ctrl-C stops it)
  /ps, /kill <id>  list and stop running apps (graphical mode)
  /display <w>x<h> [scale]  change the screen size (graphical mode)
  /layout full|windows      one app at a time, or overlapping windows
Editing: arrows or Ctrl-B/F move, Home/End or Ctrl-A/E jump, Ctrl-K/U/W cut,
  up/down recall earlier lines.
Japanese input: Option/Alt+Space switches romaji -> kana (あ) on and off.
  Space: convert to kanji (again: another candidate), Enter: accept,
  Ctrl+K: katakana, Backspace/Esc: undo. Lines starting with / stay in letters.
  /cat <path>    show a file
  /rm <path>     delete a file
  /lua <code>    run one line of Lua
  /clear         forget the conversation
  /info          memory, network and LLM settings
  /set llm <ip:port> | model <name> | path <url path>
                 change which LLM server and model are used
  /reboot, /halt
";

fn graphical() -> bool {
    wm::active()
}

fn full_screen() -> bool {
    wm::layout() == wm::LAYOUT_FULL
}

/// The number a text starts with (0 if none), like C's atoi.
fn number(s: &str) -> i32 {
    let digits = s.trim_start().bytes().take_while(u8::is_ascii_digit).count();
    s.trim_start()[..digits].parse().unwrap_or(0)
}

fn error(text: &str) {
    kprint!("{}[error] {text}\n{}", ui::ERROR, ui::END);
}

fn cmd_info() {
    kprint!("tom-os {}\n", rt::VERSION);
    let (used, total) = (mem::used(), mem::total());
    kprint!("memory : {} KiB used of {} MiB\n", used / 1024, total >> 20);
    if let Some((mac, ip)) = net::address() {
        let [a, b, c, d, e, f] = mac;
        kprint!(
            "network: virtio-net {a:02x}:{b:02x}:{c:02x}:{d:02x}:{e:02x}:{f:02x}, ip {}.{}.{}.{}\n",
            ip >> 24,
            (ip >> 16) & 255,
            (ip >> 8) & 255,
            ip & 255
        );
    } else {
        kprint!("network: no device\n");
    }
    if graphical() {
        let (w, h, scale) = wm::display();
        let layout = if full_screen() { "full screen" } else { "windows" };
        let pointer = if crate::arch::input::pointer_absolute() { "absolute" } else { "relative" };
        kprint!("display: graphical ({layout}), {w}x{h}, scale {scale}, {pointer} pointer\n");
    } else {
        kprint!("display: text\n");
    }
    let sound = if sound::available() {
        "AC'97, 48 kHz stereo"
    } else {
        "no sound card (start QEMU with -device AC97)"
    };
    kprint!("sound  : {sound}\n");
    let get = |key| lossy(&agent::setting(key));
    kprint!("llm    : http://{}{}  model={}\n", get("llm"), get("path"), get("model"));
}

fn cmd_display(arg: &str) {
    if !graphical() {
        console(b"/display only works in graphical mode\n");
        return;
    }
    let (mut w, mut h, mut scale) = (0, 0, 0);
    if let Some((left, right)) = arg.split_once('x') {
        w = number(left);
        h = number(right);
        if let Some((_, s)) = right.split_once(' ') {
            scale = number(s);
        }
    }
    if w == 0 || h == 0 {
        let (w, h, scale) = wm::display();
        kprint!(
            "display: {w}x{h}, scale {scale}\n\
             usage: /display <width>x<height> [scale]\n  \
             e.g. /display 1920x1080      more room\n       \
             /display 2048x1536 2    the same layout, twice as big\n       \
             /display 1024x768       the default\n"
        );
        return;
    }
    if let Err(e) = wm::set_display(w, h, scale) {
        kprint!("[error] {e}\n");
    } else {
        let (w, h, scale) = wm::display();
        kprint!("display: {w}x{h}, scale {scale}\n");
    }
}

fn cmd_layout(arg: &str) {
    if !graphical() {
        console(b"/layout only works in graphical mode\n");
        return;
    }
    match arg {
        "full" => wm::set_layout(wm::LAYOUT_FULL),
        "windows" => wm::set_layout(wm::LAYOUT_WINDOWS),
        _ => kprint!(
            "layout: {}\n\
             usage: /layout full      one app fills the screen (switch by dragging the bottom bar)\n       \
             /layout windows   overlapping windows\n",
            if full_screen() { "full" } else { "windows" }
        ),
    }
}

fn command(line: &[u8]) {
    let line = lossy(line);
    let (name, arg) = match line.split_once(' ') {
        Some((name, arg)) => (name, arg.trim_start_matches(' ')),
        None => (line.as_str(), ""),
    };
    match name {
        "/help" | "/?" => console(HELP.as_bytes()),
        "/apps" | "/ls" => {
            let files = fs::list();
            for (name, len) in &files {
                kprint!("  {:<40} {len} bytes\n", lossy(name));
            }
            if files.is_empty() {
                console(b"  (no files yet - ask the agent to make an app)\n");
            }
        }
        "/run" if arg.is_empty() => console(b"usage: /run <name>\n"),
        "/run" => match apps::launch(arg.as_bytes()) {
            Err(e) => error(&e),
            Ok(Started::Running) if full_screen() => {
                kprint!("{arg} is running full screen (drag the bottom bar to switch apps)\n")
            }
            Ok(Started::Running) => kprint!("{arg} is running in its own window\n"),
            Ok(Started::Finished) => {}
        },
        "/display" => cmd_display(arg),
        "/layout" => cmd_layout(arg),
        "/ps" => apps::list(),
        "/kill" => {
            if !apps::kill(number(arg)) {
                console(b"usage: /kill <id>   (ids are shown by /ps)\n");
            }
        }
        "/cat" => match fs::read(arg.as_bytes()) {
            None => kprint!("no such file: {arg}\n"),
            Some(data) => {
                console(&data);
                if data.last().is_some_and(|&b| b != b'\n') {
                    console(b"\n");
                }
            }
        },
        "/rm" => {
            if fs::delete(arg.as_bytes()) {
                kprint!("deleted {arg}\n");
            } else {
                kprint!("no such file: {arg}\n");
            }
        }
        "/lua" => {
            if let Err(e) = run::lua_run("lua", arg.as_bytes(), true, 0).0 {
                error(&e);
            }
        }
        "/clear" => {
            agent::reset();
            console(b"conversation cleared\n");
        }
        "/set" => match arg.split_once(' ').map(|(key, value)| (key, value.trim_start_matches(' '))) {
            Some((key, value)) if agent::set(key, value.as_bytes()) => {
                kprint!("{key} = {}\n", lossy(&agent::setting(key)));
            }
            _ => console(b"usage: /set llm <ip:port> | /set model <name> | /set path <url path>\n"),
        },
        "/info" => cmd_info(),
        "/halt" => {
            console(b"powering off\n");
            cpu::poweroff();
        }
        "/reboot" => cpu::reboot(),
        _ => kprint!("unknown command {name} (try /help)\n"),
    }
}

/// The shell's main loop; never returns.
pub fn run() -> ! {
    loop {
        kprint!("{}\x1b[1;32mtom-os>\x1b[0m ", ui::HIDE);
        let line = crate::console::readline(4095, true);
        console(ui::END.as_bytes());
        let Some(line) = line.filter(|l| !l.is_empty()) else { continue };
        let line = line.as_slice();
        home::add_user_line(line);
        if line[0] == b'/' {
            command(line);
        } else {
            agent::turn(line);
        }
    }
}

