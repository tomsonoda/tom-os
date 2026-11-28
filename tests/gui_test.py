#!/usr/bin/env python3
"""Graphical-mode test: boots tom-os with a display, drives it through the
serial port and QEMU's monitor (key presses, mouse, screenshots) and checks
pixels on the screen."""
import os
import socket
import sys
import tempfile
import time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, os.path.join(ROOT, "tools"))
sys.path.insert(0, os.path.join(ROOT, "tests"))
import mock_llm  # noqa: E402
import boot_test  # noqa: E402

PROMPT = boot_test.PROMPT
DESKTOP, TITLE_ON, TITLE_OFF = 0x1F4E5F, 0x2F6DB5, 0x5A6268
RED, GREEN, NAVY = 0xFF0000, 0x00FF00, 0x000080

# A graphics app on one line (it is typed into /lua): a square on a navy
# background that jumps right while "right" is held and turns green on "g".
BOX_APP = ('gfx.mode(320, 200, "boxtest") local x, c = 10, 0xFF0000 '
           'while not gfx.key("escape") do '
           'if gfx.key("right") then x = 200 end if gfx.key("g") then c = 0x00FF00 end '
           'gfx.clear(0x000080) gfx.rect(x, 50, 40, 40, c) gfx.text(4, 4, "hello", 0xFFFFFF) '
           'gfx.flip() end')
ASK_APP = 'io.write("name? ") local n = io.read() tomos.write_file("out.txt", "got:" .. n)'


class Monitor:
    def __init__(self, path):
        self.sock = socket.socket(socket.AF_UNIX)
        for _ in range(100):
            try:
                self.sock.connect(path)
                break
            except OSError:
                time.sleep(0.05)
        self._read()

    def _read(self):
        data = b""
        while not data.endswith(b"(qemu) "):
            chunk = self.sock.recv(4096)
            if not chunk:
                break
            data += chunk
        return data

    def cmd(self, line):
        self.sock.sendall(line.encode() + b"\n")
        return self._read()

    def keys(self, *names):
        # An explicit hold time: with QEMU's default the release follows the
        # press almost immediately, too fast for a game loop to notice.
        for n in names:
            self.cmd("sendkey %s 150" % n)
            time.sleep(0.3)


class Pointer:
    """Moves the absolute pointer and clicks, through QEMU's QMP interface.
    Coordinates are screen pixels."""

    def __init__(self, path, screen):
        import json
        self.json = json
        self.screen = screen
        self.sock = socket.socket(socket.AF_UNIX)
        for _ in range(100):
            try:
                self.sock.connect(path)
                break
            except OSError:
                time.sleep(0.05)
        self.f = self.sock.makefile("rw")
        self.f.readline()  # greeting
        self._call({"execute": "qmp_capabilities"})
        self.x = self.y = 0

    def _call(self, msg):
        self.f.write(self.json.dumps(msg) + "\n")
        self.f.flush()
        while True:
            reply = self.json.loads(self.f.readline())
            if "return" in reply or "error" in reply:
                assert "error" not in reply, reply
                return reply

    def _send(self, events):
        self._call({"execute": "input-send-event", "arguments": {"events": events}})

    def move(self, x, y):
        self.x, self.y = x, y
        w, h = self.screen
        self._send([{"type": "abs", "data": {"axis": "x", "value": int(x * 0x7FFF / w) + 1}},
                    {"type": "abs", "data": {"axis": "y", "value": int(y * 0x7FFF / h) + 1}}])
        time.sleep(0.05)

    def button(self, down):
        self._send([{"type": "btn", "data": {"down": down, "button": "left"}}])
        time.sleep(0.05)

    def click(self, x, y):
        self.move(x, y)
        self.button(True)
        self.button(False)

    def drag(self, x0, y0, x1, y1, steps=8):
        self.move(x0, y0)
        self.button(True)
        for i in range(1, steps + 1):
            self.move(x0 + (x1 - x0) * i // steps, y0 + (y1 - y0) * i // steps)
            time.sleep(0.03)
        self.button(False)


class Screen:
    def __init__(self, mon, tmpdir):
        self.mon, self.path = mon, os.path.join(tmpdir, "screen.ppm")

    def grab(self):
        self.mon.cmd("screendump " + self.path)
        with open(self.path, "rb") as f:
            assert f.readline().strip() == b"P6"
            self.w, self.h = map(int, f.readline().split())
            f.readline()
            self.data = f.read()

    def pixel(self, x, y):
        if x >= self.w or y >= self.h:
            return -1  # e.g. still in text mode, or the mode is about to change
        i = (y * self.w + x) * 3
        return (self.data[i] << 16) | (self.data[i + 1] << 8) | self.data[i + 2]

    def wait_pixel(self, x, y, color, what, timeout=10):
        deadline = time.time() + timeout
        while True:
            self.grab()
            got = self.pixel(x, y)
            if got == color:
                return
            if time.time() > deadline:
                raise AssertionError("%s: pixel (%d,%d) is %06x, expected %06x" % (what, x, y, got, color))
            time.sleep(0.2)


def main():
    _, port = mock_llm.start()
    tmpdir = tempfile.mkdtemp(prefix="tomos-gui-")
    mon_path, qmp_path = os.path.join(tmpdir, "mon"), os.path.join(tmpdir, "qmp")
    vm = boot_test.VM(port, gui=True, monitor=mon_path, qmp=qmp_path, args="layout=windows")
    mon = Monitor(mon_path)
    scr = Screen(mon, tmpdir)
    ptr = Pointer(qmp_path, (1024, 768))
    steps = 0

    def step(name, fn):
        nonlocal steps
        fn()
        steps += 1
        print("ok %2d - %s" % (steps, name))

    # The first app window opens at (60, 62); see the cascade in gui/wm.c.
    AX, AY = 60, 62

    try:
        def boot():
            vm.expect("network up", 90)   # from the ISO this includes the GRUB countdown
            vm.expect(PROMPT)
            if vm.iso:  # the ISO boots in the default layout
                vm.command("/set llm 10.0.2.2:%d" % port, PROMPT)
                vm.command("/layout windows", PROMPT)
            scr.wait_pixel(1000, 600, DESKTOP, "desktop background")
            scr.wait_pixel(500, 20, TITLE_ON, "shell window title bar")
            assert (scr.w, scr.h) == (1024, 768)
        step("boots to a desktop with the shell window", boot)

        step("keys typed in the QEMU window reach the shell", lambda: (
            mon.keys("slash", "i", "n", "f", "o", "ret"),
            vm.expect("display: graphical (windows)"), vm.expect(PROMPT)))

        def run_box():
            vm.command('/lua tomos.write_file("apps/box.lua", [[%s]])' % BOX_APP, PROMPT)
            vm.command("/run box", "box is running in its own window", PROMPT)
            scr.wait_pixel(AX + 300, AY + 150, NAVY, "app canvas background")
            scr.wait_pixel(AX + 30, AY + 70, RED, "square drawn by the app")
            scr.wait_pixel(500, 20, TITLE_OFF, "shell lost focus to the app")
        step("a graphics app runs in its own window", run_box)

        step("the shell stays usable while the app runs", lambda: (
            vm.command("/lua print(6 * 7)", "42", PROMPT),
            vm.command("/ps", "apps/box.lua", PROMPT)))

        def keys_to_app():
            mon.keys("right")
            scr.wait_pixel(AX + 220, AY + 70, RED, "square moved by the right arrow")
            mon.keys("g")
            scr.wait_pixel(AX + 220, AY + 70, GREEN, "square recoloured by the g key")
        step("held keys reach the focused app", keys_to_app)

        def switch():
            mon.keys("alt-tab")
            scr.wait_pixel(500, 20, TITLE_ON, "shell focused after Alt+Tab")
            mon.keys("alt-tab")
            scr.wait_pixel(500, 20, TITLE_OFF, "app focused after second Alt+Tab")
        step("Alt+Tab switches windows", switch)

        def drag():
            # grab the app's title bar and drag it 100 right, 80 down
            ptr.drag(AX + 150, AY - 10, AX + 250, AY + 70)
            scr.wait_pixel(AX + 100 + 220, AY + 80 + 70, GREEN, "square after dragging the window")
        step("the mouse drags a window by its title bar", drag)

        def close():
            mon.keys("alt-q")
            deadline = time.time() + 10   # the canvas must disappear from the screen
            while True:
                scr.grab()
                if scr.pixel(AX + 100 + 300, AY + 80 + 150) != NAVY:
                    break
                assert time.time() < deadline, "the app window is still on screen"
                time.sleep(0.2)
            scr.wait_pixel(500, 20, TITLE_ON, "shell focused again")
            vm.command("/ps", "(no apps are running)", PROMPT)
        step("Alt+Q closes the app", close)

        def text_app():
            vm.command('/lua tomos.write_file("apps/ask.lua", [[%s]])' % ASK_APP, PROMPT)
            vm.command("/run ask", "ask is running in its own window", PROMPT)
            mon.keys("t", "o", "m", "ret")
            time.sleep(0.5)
            vm.command("/cat out.txt", "got:tom", PROMPT)
            mon.keys("spc")  # dismiss "press a key to close"
            time.sleep(0.5)
            vm.command("/ps", "(no apps are running)", PROMPT)
        step("a text app gets its own terminal window and keyboard input", text_app)

        step("start-up errors are reported to the shell", lambda: (
            vm.command('/lua tomos.write_file("apps/bad.lua", "error(\'boom\')")', PROMPT),
            vm.command("/run bad", "[error]", "boom", PROMPT),
            mon.keys("spc"), time.sleep(0.5)))

        def agent_launch():
            vm.command("make me a greeter app", "Done. Run it with /run greet", PROMPT)
            vm.command("launch", "[launch_app] apps/greet.lua",
                       "launch result: the app started without errors and is running in its own window",
                       PROMPT)
            vm.command("/kill 99", "usage: /kill", PROMPT)
            vm.command("/ps", "apps/greet.lua", PROMPT)
            mon.keys("q", "ret")       # the greeter quits on "q"
            time.sleep(0.5)
            mon.keys("spc")
            time.sleep(0.5)
            vm.command("/ps", "(no apps are running)", PROMPT)
        step("the agent builds an app and launches it in a window", agent_launch)

        def display():
            vm.command("/display 1920x1080", "display: 1920x1080, scale 1", PROMPT)
            scr.wait_pixel(1900, 1000, DESKTOP, "desktop at 1920x1080")
            assert (scr.w, scr.h) == (1920, 1080), (scr.w, scr.h)
            scr.wait_pixel(500, 20, TITLE_ON, "shell title bar at 1920x1080")
            vm.command("/display 2048x1536", "display: 2048x1536, scale 2", PROMPT)
            scr.wait_pixel(1000, 40, TITLE_ON, "shell title bar drawn twice as big")
            assert (scr.w, scr.h) == (2048, 1536), (scr.w, scr.h)
            scr.wait_pixel(1001, 41, TITLE_ON, "2x2 block")
            vm.command("/display 1000x300", "[error]", "at least", PROMPT)  # too low on every machine
            vm.command("/display 8000x8000", "[error]", "video memory", PROMPT)
            scr.grab()
            assert (scr.w, scr.h) == (2048, 1536), "a failed change must keep the old mode"
            vm.command("/display 1024x768", "display: 1024x768, scale 1", PROMPT)
            scr.wait_pixel(500, 20, TITLE_ON, "back to 1024x768")
            vm.command("/info", "graphical (windows), 1024x768, scale 1", PROMPT)
        step("/display changes the screen size and scale", display)

        def halt():
            vm.send("/halt\n")
            vm.proc.wait(timeout=15)
        step("powers off", halt)
    except Exception as e:
        print("FAILED at step %d: %s" % (steps + 1, e))
        try:
            scr.grab()
            print("last screenshot: " + scr.path)
        except Exception:
            pass
        vm.proc.kill()
        return 1
    print("all %d steps passed" % steps)
    return 0


if __name__ == "__main__":
    sys.exit(main())
