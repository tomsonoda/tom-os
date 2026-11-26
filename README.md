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

tom-os does not use Linux or any other existing kernel. Boot code, console, memory management, networking (virtio-net / ARP / IPv4 / TCP / HTTP), JSON and the agent loop are all written in this repository, in C with a NASM boot stub. The only third-party code is Lua 5.4 (`third_party/lua`, unmodified), which is the app runtime. The whole kernel is about 270 KiB and boots instantly.

The project was inspired by [llm.asm](https://github.com/alesha-pro/llm.asm) but goes in a different direction. llm.asm is a chat REPL that runs model inference inside the kernel. tom-os starts with an agent that can use tools and a way to run the apps it creates.

## Status

First milestone. Runs on x86-64 under QEMU.

| Works | Not yet |
|---|---|
| Boot in QEMU and talk to the agent | Android devices / aarch64 |
| The LLM writes, checks and launches Lua apps | Local inference inside the OS (the LLM is reached over HTTP) |
| Serial and VGA text consoles | Graphical UI, non-ASCII text on the VGA screen |
| Booting from an ISO (BIOS) | UEFI boot, installing to disk |
| In-memory filesystem | Storage that survives a reboot |
| LLM access over plain HTTP | TLS, DHCP, DNS |

## Requirements

- `gcc`, `nasm`, `binutils`, `make` (Linux), or on macOS a cross toolchain:
  `brew install nasm x86_64-elf-gcc x86_64-elf-binutils qemu`
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

`make test` does not use a real LLM. `tools/mock_llm.py` returns scripted replies, including tool calls, and the test covers boot, HTTP, app generation, running the app interactively and power-off.

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
make run
```

The default model is `claude-opus-5-5`. Change it with `--model` and `--effort`.

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

With `make run` the terminal is the serial console. Quit QEMU with `Ctrl-A` then `X`. `Ctrl-C` goes to tom-os and interrupts a running app or a pending LLM request. Use `make run-vga` to also get the VGA window.

You can write to the agent in any language on the serial console. The VGA text screen can only display ASCII.

| Command | What it does |
|---|---|
| (plain text) | Ask the agent |
| `/apps` | List files (apps are saved as `apps/<name>.lua`) |
| `/run <name>` | Start an app |
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
brew install nasm x86_64-elf-gcc x86_64-elf-binutils qemu
make
make test      # optional
make run       # or: make run-vga for the VGA window
```

`make run` loads the kernel directly, so no ISO is needed. `make iso` is not set up for macOS; download the ISO from GitHub Actions as described above if you want one (for UTM, for example).

### QEMU with the ISO

```sh
brew install qemu
qemu-system-x86_64 -m 256M -cdrom tomos.iso -boot d \
  -netdev user,id=n0 -device virtio-net-pci,netdev=n0 -serial mon:stdio
```

Both the QEMU window (VGA screen) and this terminal (serial) are consoles. For non-ASCII input and output, use the terminal.

### UTM

UTM is a virtual machine app for macOS that puts a settings UI on top of QEMU. Create a new machine with "Emulate" and "Other", then set:

- Architecture: x86_64
- Boot: disable UEFI boot (use BIOS)
- Memory: 256 MB or more
- Network: "Emulated VLAN" with a virtio-net-pci card (the default "Shared Network" will not connect)
- CD/DVD: `tomos.iso`

### Running the LLM on the Mac

From inside the VM the Mac is `10.0.2.2`. The GRUB menu offers two servers (port 8080, or Ollama on 11434), and `/set` changes them after boot.

```sh
# Ollama (needs a model that supports tool calls)
ollama serve
#   tom-os> /set llm 10.0.2.2:11434
#   tom-os> /set model <a model you have pulled>

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

## How it works

```
+---------------------------------------------------------------+
| shell (agent/shell.c)   prompt and / commands                  |
| agent (agent/agent.c)   conversation, LLM calls, tool execution|
|   tools: write_file read_file list_files delete_file           |
|          run_lua (captured, for checks) launch_app (interactive)|
+----------------------------+----------------------------------+
| Lua 5.4 + lua/lbind.c      | net/http.c   HTTP/1.1 client      |
|   io / os / tomos / require| net/tcp.c    TCP (one connection, |
| fs/ramfs.c  in-memory FS   |              retransmission)      |
|                            | net/net.c    Ethernet, ARP, IPv4  |
+----------------------------+----------------------------------+
| kernel/  console (serial + VGA + PS/2)  mem (heap)  cpu (IDT,  |
|          PIC, timer, RTC)  virtio_net (PCI, legacy virtio)     |
| lib/     minimal libc, printf with floating point, libm (x87), |
|          JSON                                                  |
| arch/x86_64/boot.asm  Multiboot1 -> long mode                  |
+---------------------------------------------------------------+
```

- Single address space, single task. All devices are polled; interrupts are only used to wake the CPU from `HLT`, so the system does not spin while idle.
- Every app run gets a fresh Lua state with a memory limit. `Ctrl-C` and time limits can always stop it.
- The LLM protocol is chat completions JSON with `tools`. Responses with `Content-Length` and with chunked encoding are both handled.

### API available to apps

In addition to Lua's `string`, `table`, `math`, `utf8` and `coroutine`:

- `io.write` `io.read` `io.lines` `io.open` (reading and writing the in-memory filesystem)
- `os.time` `os.date` `os.clock` `os.remove` `os.rename` `os.exit`
- `tomos.cls()` `tomos.getkey([timeout_ms])` `tomos.sleep(ms)` `tomos.readline([prompt])` `tomos.time_ms()` `tomos.files()` `tomos.read_file(path)` `tomos.write_file(path, data)`
- `require('name')` (looks for `name.lua`, `apps/name.lua`, `lib/name.lua`)
- Screen control through ANSI escape sequences (colors, cursor movement, clearing)

## Limitations

- Generated apps run with the same privileges as the kernel. They are confined to the Lua runtime, but there is no process isolation or memory protection.
- Files live in memory and are lost on reboot.
- The IP address is static (default `10.0.2.15`, gateway `10.0.2.2`; change with the kernel arguments `ip=`, `gw=`, `mask=`).
- The conversation history is never trimmed. Use `/clear` when it gets long.
- Booting on real PC hardware has not been tested. virtio-net is the only network driver.

## Possible next steps

1. Persistent storage (virtio-blk) so apps survive a reboot
2. Streaming display of LLM responses
3. Local inference inside the kernel with a small model
4. aarch64 support, on the way to Android devices
5. Framebuffer graphics and non-ASCII text
6. App isolation (user mode, memory protection)

## License

`third_party/lua` is under the MIT license of Lua.org, PUC-Rio (see `third_party/lua/README.md`).
