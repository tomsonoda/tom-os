# tom-os

A very small bare-metal operating system built for one thing: running an LLM agent.

There are no pre-installed applications. The system boots straight into an agent prompt. Ask for something ("make me a calculator") and the LLM writes the app in Lua on the spot, checks it, and hands it to you to use.

```
tom-os> make a number guessing game
  [write_file] apps/guess.lua (612 bytes)
  [run_lua] 58 bytes ... ok
  [launch_app] apps/guess.lua
Guess a number from 1 to 100: _
```

tom-os does not use Linux or any other existing kernel. Boot code, console, memory management, networking (virtio-net / ARP / IPv4 / TCP / HTTP), JSON, the window system and the agent loop are all written in this repository, in C and Rust with a NASM boot stub. Apps run on a Lua 5.4 interpreter written in safe Rust (RemedyLua, in `third_party/remedy-lua`, ported to run without an operating system). The kernel is about 1.9 MiB and boots instantly.

The kernel is being moved from C to Rust piece by piece. Done so far (`rust/`): the Lua runtime and its bindings, the agent, the shell, app management, the file system, JSON, the home screen's chat view, 2D drawing and fonts, and the whole network path (HTTP client, TCP/IP, virtio-net driver, PCI access). Still C: the window manager and terminals, the other drivers, memory management and boot.

The project was inspired by [llm.asm](https://github.com/alesha-pro/llm.asm) but goes in a different direction. llm.asm is a chat REPL that runs model inference inside the kernel. tom-os starts with an agent that can use tools and a way to run the apps it creates.

## Status

First milestone. Runs on x86-64 under QEMU.

| Works | Not yet |
|---|---|
| Boot in QEMU and talk to the agent | Android devices / aarch64 |
| The LLM writes, checks and launches Lua apps | Local inference inside the OS (the LLM is reached over HTTP) |
| Serial and VGA text consoles; Japanese text on screen and Japanese input (romaji, kanji conversion by the LLM) in graphical mode | Other non-ASCII scripts on screen; Japanese on the VGA text screen (text mode) |
| Graphical desktop: a chat-style home screen for the prompt and full-screen apps on top (switched by dragging the bottom bar), or overlapping windows; mouse; apps that draw pixels | GPU acceleration, touch gestures from a trackpad |
| Sound: an AC'97 driver and a synthesizer (tones, melodies, samples) for apps | Recording, sound files (WAV/MP3) |
| Screen size and 2x/3x/4x scaling, at boot or with `/display` | Following the host window size automatically (QEMU scales the picture instead) |
| Several apps running at once | Process isolation between apps |
| Booting from an ISO (BIOS) | UEFI boot, installing to disk |
| In-memory filesystem | Storage that survives a reboot |
| LLM access over plain HTTP | TLS, DHCP, DNS |

## Requirements

- `gcc`, `nasm`, `binutils`, `make` (Linux), or on macOS a cross toolchain:
  `brew install nasm x86_64-elf-gcc x86_64-elf-binutils qemu`
- Rust through [rustup](https://rustup.rs) (on macOS: `brew install rustup`, then `rustup-init`). The build uses the nightly toolchain named in `rust/rust-toolchain.toml`; rustup downloads it on the first `make`.
- `qemu-system-x86_64`
- Python 3 (for the tests and the host-side gateways)

## Build and test

```sh
make          # builds build/tomos.elf
make test     # boots in QEMU and runs the whole flow against a mock LLM
make iso      # builds build/tomos.iso (CD image, BIOS boot via GRUB)
make test-iso # runs the same test booting from the ISO
```

Building the ISO also needs `grub-pc-bin`, `xorriso` and `mtools`. A prebuilt ISO can be downloaded from the GitHub Actions runs of this repository (artifact `tomos-iso`).

`make test` does not use a real LLM. `tests/lua_test.py` runs the scripts in `tests/lua` inside tom-os and compares their output with what C Lua 5.4 prints, and checks that an app cannot exhaust memory or ignore `Ctrl-C`. `tests/sound_test.py` records what QEMU plays (its WAV backend) and checks pitches and timing. `tests/gateway_test.py` runs the Claude gateway against a fake, slow Claude (it needs `pip install anthropic`, but no key). `tools/mock_llm.py` returns scripted replies, including tool calls. `tests/boot_test.py` covers text mode (boot, HTTP, app generation, running the app interactively, command history, power-off). `tests/gui_test.py` (windows layout) and `tests/phone_test.py` (full-screen layout) cover graphical mode: they send key presses and absolute mouse movement through QEMU and check pixels in screenshots (switching apps by dragging, the card view, clicks reaching apps, a graphics app reacting to held keys).

## Running

tom-os calls the LLM over plain HTTP. The server is chosen with the kernel arguments `llm=IP:port` and `model=name` (`LLM=` and `MODEL=` for `make run`). Under QEMU user networking the host is reachable as `10.0.2.2`.

### With Claude

tom-os has no TLS, so a small gateway runs on the host. The API key stays on the host and never enters the VM.

```sh
pip install anthropic
export ANTHROPIC_API_KEY=...          # or: ant auth login
python3 tools/claude_gateway.py       # listens on 127.0.0.1:8080
```

In another terminal:

```sh
make run-gui      # graphical desktop in a QEMU window
make run          # or: text mode in this terminal
```

The default model is `claude-opus-5-5`. Change it with `--model` and `--effort`.

The gateway prints a line when a request arrives, a progress line every 15 seconds while Claude is writing, and a line when the reply is sent. Writing a whole app can take a minute or two per reply; tom-os shows the seconds that have passed (`thinking... 42 s`). Pressing `Ctrl-C` cancels the request.

The gateway streams Claude's reply and sends tom-os a space every few seconds while it waits, so a long reply never looks like a dead connection. If something fails, tom-os says at which step (connecting, sending the request, waiting for the reply) and the gateway prints the error from the Claude API.

### With a local LLM on the host

Servers that speak the chat completions format (with tool calls) over plain HTTP, such as llama.cpp's `llama-server` or Ollama, can be used directly.

```sh
make run LLM=10.0.2.2:11434 MODEL=qwen3     # Ollama example
```

### Other endpoints that require HTTPS

```sh
export RELAY_API_KEY=...
python3 tools/http_relay.py --upstream https://example.com/v1/chat/completions
make run MODEL=<model name>
```

### Using it

There are two modes, chosen at boot:

| | Start with | What you get |
|---|---|---|
| Graphical | `make run-gui`, or the first entry of the ISO menu | A desktop in the QEMU window. The shell is a window; every app opens in its own window. |
| Text | `make run`, `make run-vga`, the second ISO entry, or the kernel argument `gui=0` | One console. An app takes over the console until it exits. |

Graphical mode is used whenever a suitable display is found and `gui=0` is not given.

In both modes the terminal that started QEMU is connected to the shell over the serial port. Quit QEMU with `Ctrl-A` then `X`. `Ctrl-C` goes to tom-os and interrupts a running app or a pending LLM request.

You can write to the agent in any language from that terminal. In graphical mode the screen also shows Japanese, and you can type Japanese in the QEMU window with the built-in input method (below). The VGA text screen of text mode shows ASCII only. The keyboard in the QEMU window uses the US layout.

### Japanese input

The input method of your computer does not reach the QEMU window, so tom-os has its own. It works wherever a line is typed: the prompt, and apps that read text.

| Key | |
|---|---|
| `Option+Space` (Mac) / `Alt+Space`, `Ctrl+Space` | Switch Japanese input on and off. The status bar shows `あ` or `A`. The 変換/無変換/ひらがな keys of a Japanese PC keyboard also work. |
| letters | Romaji become hiragana as you type (`nihongo` → にほんご, `kka` → っか, `n'` / `nn` → ん). `-` `,` `.` `[` `]` become ー 、 。 「 」. |
| `Space` | Convert to kanji. The LLM does the conversion (tom-os has no dictionary), so it takes a moment; press `Space` again for another candidate. |
| `Enter` | Accept. Press `Enter` again to send the line. |
| `Ctrl+K` | Katakana. |
| `Backspace`, `Esc` | Back to the kana / delete. |

Lines that start with `/` are commands and stay in plain letters. The LLM understands hiragana too, so converting is optional when you write to the agent. Typing in the terminal that started QEMU (with your computer's own input method) also works.

The line you type can be edited, at the prompt and in apps that read a line:

| Key | |
|---|---|
| `←` `→` or `Ctrl-B` `Ctrl-F` | Move the cursor |
| `Home` `End` or `Ctrl-A` `Ctrl-E` | Jump to the start or end |
| `Backspace`, `Delete` or `Ctrl-D` | Remove the character before or at the cursor |
| `Ctrl-K`, `Ctrl-U`, `Ctrl-W` | Cut to the end, cut to the start, remove the word before the cursor |
| `↑` `↓` or `Ctrl-P` `Ctrl-N` | Earlier and later lines (at the `tom-os>` prompt) |

In the terminal that started QEMU, `Ctrl-A` is QEMU's own command key: press `Ctrl-A` twice there, or use `Home`. Lines longer than the terminal is wide are not redrawn correctly there while editing; the graphical home screen wraps them properly.

### Sound

tom-os has its own driver for the AC'97 sound card that QEMU emulates; QEMU plays the sound through your computer. The `make` targets add the card:

- macOS: sound goes to the Mac's speakers (CoreAudio) automatically.
- Linux: choose your sound system, for example `make run-gui AUDIO=pa` (PulseAudio or PipeWire) or `AUDIO=alsa`. The default `none` keeps the card but plays nothing.
- By hand: add `-audiodev coreaudio,id=snd -device AC97,audiodev=snd` (or another QEMU audio backend instead of `coreaudio`).

`/info` shows whether a sound card was found. Apps play sound with the `sound` library below; ask the agent for a game "with sound effects" or "that plays a melody".

### Screen size

The graphical desktop starts at 1024x768. Larger screens are available at boot or at any time:

| | At boot | While running |
|---|---|---|
| More room | `make run-gui RES=1920x1080` | `/display 1920x1080` |
| Everything twice as big | `make run-gui RES=2048x1536` | `/display 2048x1536` |
| Choose the scale | `make run-gui RES=2560x1600 SCALE=2` | `/display 2560x1600 2` |

The scale (1 to 4) enlarges everything, so text stays sharp on large or high-resolution displays. Screens of 2048x1200 and larger use scale 2 unless another scale is given. After scaling, at least 800x600 must remain. The kernel arguments are `res=WIDTHxHEIGHT` and `scale=N`, and the ISO menu has entries for the large and wide sizes.

Sizes above 1920x1080 need more video memory than QEMU gives by default. The `make` targets start QEMU with `-vga none -device VGA,vgamem_mb=64`; add the same options when starting QEMU by hand.

On macOS, `make run-gui` also starts QEMU with `-display cocoa,zoom-to-fit=on`, so the picture is enlarged to fill the QEMU window when you resize or maximize it. Scaling by QEMU makes text a little blurry; for the sharpest picture, pick a size with `/display` that matches the window. Override the QEMU display options with `QEMU_DISPLAY=...`.

### The graphical desktop

Graphical mode has two layouts. Switch with `/layout full` and `/layout windows`, or boot with `layout=windows`.

**Full screen (default)**, like a phone. The home screen is the prompt, shown as a chat: your requests appear in bubbles on the right, the agent's answers on the left, and the steps it takes (writing a file, testing it, starting the app) as quiet notes in between. You type in the rounded box at the bottom; `Enter` or the round button sends. Before the first request, the home screen greets you with a few suggestions you can click. The mouse wheel or `PageUp` / `PageDown` scroll back through the conversation. Apps open on top of the home screen, one at a time, filling the screen.

- Click the bar at the bottom of the screen, or drag it up, to go back to the home screen (`Alt+H`). Apps keep running.
- In an app, drag the bottom bar sideways to switch to the app that was started before or after it. A short drag springs back. The `<` and `>` at the top and `Alt+Left` / `Alt+Right` do the same.
- On the home screen, click the bottom bar (or the status bar, or press `Alt+Up`) to see the running apps as cards. Drag the row sideways, click a card to open that app, drag a card up or click its `x` to close it. `Esc` goes back.
- `Alt+Q` closes the current app and returns to the home screen; so does an app that finishes. `Alt+Tab` on the home screen opens the first running app.
- Apps that ask for a small canvas are shown enlarged; `gfx.mode()` without a size gives an app the whole screen.

**Windows**: overlapping windows with title bars.

- Click a window to bring it to the front, drag its title bar to move it, click the red box to close an app.
- `Alt+Tab` switches to the next window, `Alt+Q` closes the focused app, `Alt+arrows` move the focused window.

In both layouts:

- On a Mac, `Alt` is the Option key.
- The pointer follows the host's mouse exactly (QEMU's absolute "vmmouse" pointer), so there is nothing to capture or release, and clicks land where you click, also when the QEMU window is zoomed. `/info` shows `absolute pointer` when this works. Without it, tom-os falls back to the PS/2 mouse, which QEMU captures when you click in its window.
- Apps keep running while you type in the shell or wait for the LLM.
- A text app's screen stays after it finishes, so its output can be read; press a key to close it. If an app fails, the error is shown there.

### Screen size

The graphical desktop starts at 1024x768. Larger screens are available at boot or at any time:

| | At boot | While running |
|---|---|---|
| More room | `make run-gui RES=1920x1080` | `/display 1920x1080` |
| Everything twice as big | `make run-gui RES=2048x1536` | `/display 2048x1536` |
| Choose the scale | `make run-gui RES=2560x1600 SCALE=2` | `/display 2560x1600 2` |

The scale (1 to 4) enlarges everything, so text stays sharp on large or high-resolution displays. Screens of 2048x1200 and larger use scale 2 unless another scale is given. After scaling, at least 800x600 must remain. The kernel arguments are `res=WIDTHxHEIGHT` and `scale=N`, and the ISO menu has entries for the large and wide sizes.

Sizes above 1920x1080 need more video memory than QEMU gives by default. The `make` targets start QEMU with `-vga none -device VGA,vgamem_mb=64`; add the same options when starting QEMU by hand.

On macOS, `make run-gui` also starts QEMU with `-display cocoa,zoom-to-fit=on`, so the picture is enlarged to fill the QEMU window when you resize or maximize it. Scaling by QEMU makes text a little blurry; for the sharpest picture, pick a size with `/display` that matches the window. Override the QEMU display options with `QEMU_DISPLAY=...`.

### The desktop (graphical mode)

- Click a window to bring it to the front. Drag its title bar to move it. Click the red box to close an app.
- `Alt+Tab` switches to the next window, `Alt+Q` closes the focused app, `Alt+arrows` move the focused window. On a Mac, `Alt` is the Option key.
- QEMU captures the mouse when you click in its window; the title bar of the QEMU window shows the key combination that releases it.
- Apps keep running while you type in the shell or wait for the LLM.
- A text app's window stays open after it finishes, so its output can be read; press a key to close it. If an app fails, the error is shown in its window.

| Command | What it does |
|---|---|
| (plain text) | Ask the agent |
| `/apps` | List files (apps are saved as `apps/<name>.lua`) |
| `/run <name>` | Start an app (graphical mode: on its own screen or window) |
| `/ps` `/kill <id>` | List and stop running apps (graphical mode) |
| `/display <w>x<h> [scale]` | Change the screen size (graphical mode) |
| `/layout full` `/layout windows` | One app at a time, or overlapping windows (graphical mode) |
| `/cat <path>` `/rm <path>` | Show or delete a file |
| `/lua <code>` | Run one line of Lua |
| `/clear` | Forget the conversation |
| `/info` | Memory, network and LLM settings |
| `/set llm <IP:port>` `/set model <name>` `/set path <path>` | Change the LLM server and model after boot |
| `/halt` `/reboot` | Power off, reboot |

## Trying it on a Mac (macOS)

tom-os is emulated as an x86-64 machine, so both Apple Silicon and Intel Macs work.

The steps in this section are based on testing with QEMU on Linux. They have not been verified on macOS.

### Build and run from source

Apple's compiler cannot produce the kernel, so install a cross toolchain. The Makefile uses it automatically on macOS.

```sh
brew install nasm x86_64-elf-gcc x86_64-elf-binutils qemu rustup
rustup-init    # once; accept the defaults, then open a new terminal
make
make test      # optional
make run-gui   # graphical desktop; or: make run for text mode in the terminal
```

`make run` loads the kernel directly, so no ISO is needed. `make iso` is not set up for macOS; download the ISO from GitHub Actions as described above if you want one (for UTM, for example).

### QEMU with the ISO

```sh
brew install qemu
qemu-system-x86_64 -m 256M -cdrom tomos.iso -boot d \
  -vga none -device VGA,vgamem_mb=64 -display cocoa,zoom-to-fit=on \
  -audiodev coreaudio,id=snd -device AC97,audiodev=snd \
  -netdev user,id=n0 -device virtio-net-pci,netdev=n0 -serial mon:stdio
```

The ISO menu offers graphical mode (default) and text mode. The terminal is connected to the shell in both.

### UTM

UTM is a virtual machine app for macOS that puts a settings UI on top of QEMU. Create a new machine with "Emulate" and "Other", then set:

- Architecture: x86_64
- Boot: disable UEFI boot (use BIOS)
- Memory: 256 MB or more
- Display: the default card works up to 1920x1080; for larger sizes, add `-device VGA,vgamem_mb=64` under QEMU arguments (and remove the default display device)
- Network: "Emulated VLAN" with a virtio-net-pci card (the default "Shared Network" will not connect)
- CD/DVD: `tomos.iso`

### Running the LLM on the Mac

From inside the VM the Mac is `10.0.2.2`. The default server is port 8080; `/set` changes the server and model after boot.

```sh
# Ollama (needs a model that supports tool calls)
ollama serve
#   tom-os> /set llm 10.0.2.2:11434
#   tom-os> /set model <a model you have pulled>

# LM Studio (start its local server; the model id is listed by
# curl http://localhost:1234/v1/models)
#   tom-os> /set llm 10.0.2.2:1234
#   tom-os> /set model <model id>

# Claude
pip install anthropic && export ANTHROPIC_API_KEY=...
python3 tools/claude_gateway.py
```

### What to check

1. The ISO boots to a prompt
2. Keyboard input works (US layout only; symbols are in different places on other layouts)
3. `/info` shows the network device
4. One chat round trip with the LLM works
5. "Make me a ... app" produces an app, and `/run <name>` starts it
6. In graphical mode: "make a breakout game" opens a playable game full screen; clicks work where you click; clicking the bottom bar goes back to the prompt, and the card view brings the game back
7. Japanese: `Option+Space` shows あ in the status bar; typing `nihongo` and `Space` turns into 日本語
8. Sound: `/info` shows the sound card; `/lua sound.melody("C4 E4 G4 C5")` plays from the Mac's speakers

## How it works

```
+------------------------------------------------------------------+
| shell (rust/src/shell.rs) prompt, / commands                      |
| agent (rust/src/agent.rs) conversation, LLM calls, tool execution |
|   tools: write_file read_file list_files delete_file              |
|          run_lua (captured, for checks) launch_app                |
| apps  (rust/src/apps.rs)  one thread + terminal + window per app  |
+-------------------------------+----------------------------------+
| Lua 5.4 in Rust (rust/,       | rust/src/http.rs  HTTP/1.1 client |
|  third_party/remedy-lua)      |          tcp.rs   TCP (one        |
|   io / os / tomos / gfx /     |     connection, retransmission)   |
|   sound, lua/lhost.c          |          net.rs   Ethernet, ARP,  |
| rust/src/fs.rs  in-memory FS  |                   IPv4            |
| gui/wm.c   windows, focus,    |          virtio_net.rs, pci.rs    |
|            compositor         |                   network card    |
| rust/src/gfx.rs  framebuffer, |                                   |
|            fonts, 2D drawing  |                                   |
+-------------------------------+----------------------------------+
| kernel/  console (terminals, serial, PS/2 keyboard and mouse)     |
|          sched (cooperative threads)  mem (heap)                  |
|          cpu (IDT, PIC, timer, RTC)                               |
|          ac97 (sound card, synthesizer)                           |
| lib/     minimal libc, printf with floating point, libm (x87)     |
| rust/src/json.rs  JSON                                            |
| arch/x86_64/boot.asm  Multiboot1 -> long mode, thread switch      |
+------------------------------------------------------------------+
```

- The home screen's chat view (`rust/src/home.rs`) reads the shell's ordinary output. Private escape sequences, which terminals ignore, mark what each piece of text is (an answer, a step, an error, the waiting indicator, or something to leave out), so the serial console and the windows layout still show the plain text.
- Single address space. Threads are cooperative: an app gives up the CPU whenever it waits for input, sleeps or shows a frame, and the Lua runtime also yields every few thousand instructions, so a busy app cannot freeze the others.
- All devices are polled; interrupts are only used to wake the CPU from `HLT`, so the system does not spin while idle.
- Graphics use the linear framebuffer of QEMU's standard display adapter (1024x768). The window manager keeps one buffer per window and redraws only the part of the screen that changed. The 8x16 font is read from the VGA adapter at boot.
- Every app run gets a fresh Lua state with a memory limit. `Ctrl-C`, closing the window and time limits can always stop it; a script cannot catch that stop with `pcall`. The interpreter contains no `unsafe` code, so a bug in it cannot corrupt kernel memory the way a bug in a C interpreter could.
- The LLM protocol is chat completions JSON with `tools`. Responses with `Content-Length` and with chunked encoding are both handled.

### API available to apps

In addition to Lua's `string`, `table`, `math`, `utf8` and `coroutine`:

- `io.write` `io.read` `io.lines` `io.open` (reading and writing the in-memory filesystem)
- `os.time` `os.date` `os.clock` `os.remove` `os.rename` `os.exit`
- `tomos.cls()` `tomos.getkey([timeout_ms])` `tomos.sleep(ms)` `tomos.readline([prompt])` `tomos.time_ms()` `tomos.files()` `tomos.read_file(path)` `tomos.write_file(path, data)`
- `require('name')` (looks for `name.lua`, `apps/name.lua`, `lib/name.lua`)
- Text screen control through ANSI escape sequences (colors, cursor movement, clearing)

Sound (when `sound.available()` is true). Every call except `wait` returns at once, and sounds mix:

- `sound.tone(hz, ms [, wave, volume])`: waves `"square"` (default), `"sine"`, `"triangle"`, `"saw"`, `"noise"`; volume 0..1
- `sound.note("C5", ms [, wave, volume])`, `sound.melody("C4:200 E4 G4:400 R:100 C5" [, default_ms [, wave, volume]])` (`R` is a rest, `#` and `b` raise and lower)
- `sound.play(samples, rate [, volume])` plays a table of numbers between -1 and 1
- `sound.beep()`, `sound.stop()`, `sound.playing()`, `sound.wait()`

Graphics (graphical mode only; `tomos.gui` tells an app whether it is available):

- `gfx.mode([title])` turns the app's screen into a pixel canvas as large as possible; `gfx.mode(width, height [, title])` asks for a size (small canvases are shown enlarged in full-screen layout)
- `gfx.clear([color])` `gfx.rect(x, y, w, h, color)` `gfx.box(x, y, w, h, color)` `gfx.line(x1, y1, x2, y2, color)` `gfx.circle(x, y, r, color [, filled])` `gfx.pixel(x, y, color)` `gfx.text(x, y, string, color [, scale])`
- `gfx.flip([fps])` shows the frame and waits for the next one (60 per second by default)
- `gfx.key(name)` is true while a key is held (`'left'`, `'space'`, `'a'`, ...); `gfx.mouse()` returns x, y and the two buttons in canvas coordinates (a short click counts as down for at least one call)
- `gfx.rgb(r, g, b)` `gfx.size()`; colors are integers `0xRRGGBB`

```lua
gfx.mode(640, 480, "Demo")
local x = 320
while not gfx.key("escape") do
  if gfx.key("left") then x = x - 4 end
  if gfx.key("right") then x = x + 4 end
  gfx.clear(0x101830)
  gfx.rect(x - 40, 440, 80, 10, 0xFFFFFF)
  gfx.text(10, 10, "arrows move, escape quits", 0xFFE040)
  gfx.flip()
end
```

## Limitations

- Generated apps run with the same privileges as the kernel. They are confined to the Lua runtime, but there is no process isolation or memory protection.
- Files live in memory and are lost on reboot.
- The IP address is static (default `10.0.2.15`, gateway `10.0.2.2`; change with the kernel arguments `ip=`, `gw=`, `mask=`).
- The conversation history is never trimmed. Use `/clear` when it gets long.
- Booting on real PC hardware has not been tested. virtio-net is the only network driver, and graphical mode needs the display interface of QEMU's standard VGA adapter.
- The mode is chosen at boot; switching between graphical and text mode needs a reboot.
- Windows cannot be resized by the user, and the screen size does not follow the host window by itself (QEMU scales the picture; `/display` changes the real size).

## Possible next steps

1. Persistent storage (virtio-blk) so apps survive a reboot
2. Streaming display of LLM responses
3. Local inference inside the kernel with a small model
4. aarch64 support, on the way to Android devices
5. Other scripts on screen (a larger Unicode font), keyboard layouts other than US, a local kana-kanji dictionary
6. Images, sound files, and a richer widget set for apps
7. App isolation (user mode, memory protection)

## License

`third_party/remedy-lua` is RemedyLua 0.1.2 by Ahmi Darrow, licensed MIT OR Apache-2.0, with the changes described in `third_party/remedy-lua/README.md`. It depends on the `hashbrown` (MIT OR Apache-2.0) and `libm` (MIT) crates, which cargo downloads.

`gui/font_ja.bin` is converted by `tools/make_font.py` from the Shinonome (東雲) 16-dot gothic font (`shnmk16`, Debian package `xfonts-shinonome`), which its authors placed in the public domain.
