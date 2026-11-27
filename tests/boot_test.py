#!/usr/bin/env python3
"""End-to-end test: boots tom-os in QEMU and drives it over the serial port,
with tools/mock_llm.py playing the model."""
import os
import subprocess
import sys
import threading
import time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, os.path.join(ROOT, "tools"))
import mock_llm  # noqa: E402

PROMPT = "tom-os>"


class VM:
    def __init__(self, port, gui=False, monitor=None, qmp=None, args="", qemu_args=()):
        # TOMOS_ISO=<path> boots the CD image through GRUB instead of loading
        # the kernel directly; the LLM address is then set with /set.
        self.iso = os.environ.get("TOMOS_ISO")
        # TOMOS_ARCH=aarch64 tests the ARM version on QEMU's "virt" machine.
        arch = os.environ.get("TOMOS_ARCH", "x86_64")
        append = "llm=10.0.2.2:%d model=mock%s %s" % (port, "" if gui else " gui=0", args)
        if arch == "aarch64":
            machine = ["-M", "virt", "-cpu", "cortex-a72", "-global", "virtio-mmio.force-legacy=false",
                       "-netdev", "user,id=n0", "-device", "virtio-net-device,netdev=n0"]
            if gui:
                machine += ["-device", "ramfb", "-device", "virtio-keyboard-device",
                            "-device", "virtio-tablet-device"]
            boot = ["-kernel", os.path.join(ROOT, "build", "aarch64", "Image"), "-append", append]
        else:
            machine = ["-vga", "none", "-device", "VGA,vgamem_mb=64",
                       "-netdev", "user,id=n0", "-device", "virtio-net-pci,netdev=n0"]
            if self.iso:
                boot = ["-cdrom", os.path.join(ROOT, self.iso), "-boot", "d"]
            else:
                boot = ["-kernel", os.path.join(ROOT, "build", "tomos.elf"), "-append", append]
        mon = ["-monitor", "unix:%s,server,nowait" % monitor] if monitor else ["-monitor", "none"]
        if qmp:
            mon += ["-qmp", "unix:%s,server,nowait" % qmp]
        self.proc = subprocess.Popen(
            [os.environ.get("QEMU", "qemu-system-" + arch), "-m", "256M", "-no-reboot",
             "-display", "none", "-serial", "stdio"] + machine + mon + boot + list(qemu_args),
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        self.gui = gui
        self.raw = b""  # decoded as a whole: multi-byte characters arrive split
        self.pos = 0
        self.lock = threading.Lock()
        threading.Thread(target=self._reader, daemon=True).start()

    def _reader(self):
        while True:
            data = self.proc.stdout.read1(4096)
            if not data:
                return
            with self.lock:
                self.raw += data

    def send(self, text):
        self.proc.stdin.write(text.encode())
        self.proc.stdin.flush()

    def expect(self, needle, timeout=30):
        """Waits for `needle` in the output after the last match."""
        deadline = time.time() + timeout
        while time.time() < deadline:
            with self.lock:
                self.out = self.raw.decode("utf-8", "replace")
                idx = self.out.find(needle, self.pos)
                if idx >= 0:
                    self.pos = idx + len(needle)
                    return
            if self.proc.poll() is not None:
                break
            time.sleep(0.02)
        with self.lock:
            tail = self.out[self.pos:][-1500:]
        raise AssertionError("did not see %r; output since last match:\n%s" % (needle, tail))

    def command(self, line, *needles, timeout=30):
        self.send(line + "\n")
        for n in needles:
            self.expect(n, timeout)


def main():
    _, port = mock_llm.start()
    vm = VM(port)
    steps = 0

    def step(name, fn):
        nonlocal steps
        fn()
        steps += 1
        print("ok %2d - %s" % (steps, name))

    try:
        def boot():
            if vm.iso:  # second GRUB entry is text mode (Ctrl-N moves down)
                vm.expect("GNU GRUB", 60)
                vm.send("\x0e\r")
            vm.expect("network up", 60)
            vm.expect(PROMPT)
            vm.command("/info", "display: text", PROMPT)
        step("boots to a text-mode prompt" + (" from the ISO" if vm.iso else ""), boot)

        step("/set changes the LLM server and model", lambda: (
            vm.command("/set llm 10.0.2.2:%d" % port, "llm = 10.0.2.2:%d" % port, PROMPT),
            vm.command("/set model mock", "model = mock", PROMPT),
            vm.command("/info", "http://10.0.2.2:%d/v1/chat/completions  model=mock" % port, PROMPT)))

        step("Lua arithmetic and number formatting", lambda: vm.command(
            "/lua print(6*7, 0.1+0.2, 2^10, 10//3, string.format('%.3f %5d %g', math.pi, 42, 1e21))",
            "42\t0.3\t1024.0\t3\t3.142    42 1e+21", PROMPT))

        def history():
            vm.command("/lua print('his' .. 'tory', 1)", "history\t1", PROMPT)
            vm.command("/lua print('second')", "second", PROMPT)
            vm.command("\x1b[A\x1b[A", "history\t1", PROMPT)       # up twice: first command
            vm.command("\x1b[A\x1b[A\x1b[B", "second", PROMPT)     # up, up, down
        step("up/down arrows recall earlier commands", history)

        def editing():
            left = "\x1b[D"
            vm.command("/lua print(1+1)" + left * 4 + "4", "42", PROMPT)            # insert: 41+1
            vm.command("lua print(7*7)\x01/", "49", PROMPT)                         # Ctrl-A
            vm.command("/lua print(5*11)+nil" + left * 4 + "\x0b", "55", PROMPT)    # Ctrl-K
            vm.command("junk\x15/lua print(6*11)", "66", PROMPT)                    # Ctrl-U
            vm.command("/lua print(7*11) +\x17", "77", PROMPT)                      # Ctrl-W
            vm.command("/lua print(8*111)" + left * 2 + "\x1b[3~\x05", "88", PROMPT)  # Delete, Ctrl-E
            vm.command("/lua print(9*1)" + "\x02" * 2 + "\x061", "99", PROMPT)       # Ctrl-B, Ctrl-F
            vm.command("あう" + left + "い", "echo: あいう", PROMPT)                   # wide characters
        step("the line can be edited: arrows, Ctrl-A/E/B/F/K/U/W, Delete", editing)

        step("Lua errors are reported with a traceback", lambda: vm.command(
            "/lua error('boom')", "boom", "stack traceback", PROMPT))

        step("plain chat round trip over HTTP", lambda: vm.command(
            "hello there", "echo: hello there", PROMPT))

        step("agent writes and checks an app via tool calls", lambda: vm.command(
            "make me a greeter app",
            "Writing the app. \u3042", "[write_file] apps/greet.lua", "[run_lua]", "ok",
            "Done. Run it with /run greet", PROMPT))

        step("the generated app is listed", lambda: vm.command(
            "/apps", "apps/greet.lua", PROMPT))

        def run_app():
            vm.command("/run greet", "Your name? ")
            vm.command("Tom", "Hello, Tom! (3 letters)", "Your name? ")
            vm.command("q", "bye", PROMPT)
        step("the user runs the generated app interactively", run_app)

        def launch():
            vm.command("launch", "[launch_app] apps/greet.lua", "Your name? ")
            vm.command("Ada", "Hello, Ada! (3 letters)", "Your name? ")
            vm.command("q", "bye", "launch result: the app ran and exited normally", PROMPT)
        step("the agent launches the app for the user", launch)

        def slow():
            vm.command("slow", "thinking... 2 s", "slow reply", PROMPT, timeout=20)
            vm.send("slow\n")
            vm.expect("thinking... 1 s")
            vm.send("\x03")
            vm.expect("interrupted")
            vm.expect(PROMPT)
            vm.command("hello again", "echo: hello again", PROMPT)
        step("slow replies show elapsed time; Ctrl-C cancels and the next request works", slow)

        step("large response (many TCP segments)", lambda: vm.command(
            "big", "line 03999 of the big reply", "END-OF-BIG", PROMPT, timeout=60))

        def interrupt():
            vm.send("/lua while true do end\n")
            time.sleep(1.0)
            vm.send("\x03")
            vm.expect("interrupted", 15)
            vm.expect(PROMPT)
        step("Ctrl-C stops a runaway program", interrupt)

        step("files written from Lua persist across runs", lambda: (
            vm.command("/lua local f = io.open('notes.txt', 'w') f:write('alpha\\nbeta\\n') f:close()",
                       PROMPT),
            vm.command("/lua for l in io.lines('notes.txt') do io.write('<', l, '>') end print()",
                       "<alpha><beta>", PROMPT)))

        def halt():
            vm.send("/halt\n")
            vm.proc.wait(timeout=15)
        step("powers off", halt)
    except Exception as e:
        print("FAILED at step %d: %s" % (steps + 1, e))
        vm.proc.kill()
        return 1
    print("all %d steps passed" % steps)
    return 0


if __name__ == "__main__":
    sys.exit(main())
