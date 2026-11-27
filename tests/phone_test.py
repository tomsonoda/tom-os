#!/usr/bin/env python3
"""Full-screen ("phone") layout test: one app fills the screen, the home bar
switches apps by dragging, and the overview shows all apps as cards. Drives
QEMU with key presses (monitor) and an absolute pointer (QMP) and checks
pixels in screenshots."""
import os
import sys
import tempfile
import time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, os.path.join(ROOT, "tools"))
sys.path.insert(0, os.path.join(ROOT, "tests"))
import mock_llm  # noqa: E402
import boot_test  # noqa: E402
from gui_test import Monitor, Pointer, Screen  # noqa: E402

PROMPT = boot_test.PROMPT
W, H = 1024, 768
STATUS_H, BAR_H = 24, 28
BAR, OVERVIEW, BLACK = 0x141A1F, 0x0E2A36, 0x000000
APP_BG = 0x205020  # the test app's background

# A full-screen canvas app: green background, records every click (in its own
# coordinates) to a file, and turns a box red while the left button is down.
CLICK_APP = ('local w, h = gfx.mode("clicker") local was = false '
             'tomos.write_file("size", w .. "x" .. h) '
             'while not gfx.key("escape") do local x, y, l = gfx.mouse() '
             'if l and not was then tomos.write_file("click", x .. "," .. y) end was = l '
             'gfx.clear(0x205020) gfx.rect(0, 0, 40, 40, l and 0xFF0000 or 0xFFFFFF) gfx.flip() end')
# A small canvas, shown enlarged.
SMALL_APP = ('gfx.mode(320, 200, "small") while true do local x, y, l = gfx.mouse() '
             'if l then tomos.write_file("small", x .. "," .. y) end '
             'gfx.clear(0x000080) gfx.rect(0, 0, 10, 10, 0xFFFF00) gfx.flip() end')


def main():
    _, port = mock_llm.start()
    tmpdir = tempfile.mkdtemp(prefix="tomos-phone-")
    mon_path, qmp_path = os.path.join(tmpdir, "mon"), os.path.join(tmpdir, "qmp")
    vm = boot_test.VM(port, gui=True, monitor=mon_path, qmp=qmp_path)
    mon = Monitor(mon_path)
    scr = Screen(mon, tmpdir)
    ptr = Pointer(qmp_path, (W, H))
    steps = 0

    def step(name, fn):
        nonlocal steps
        fn()
        steps += 1
        print("ok %2d - %s" % (steps, name))

    def cat(path):
        vm.command("/cat " + path, PROMPT)
        return vm.out.rsplit("/cat " + path, 1)[1].split()[0]

    def wait_file(path, expected, timeout=5):
        deadline = time.time() + timeout
        while True:
            got = cat(path)
            if got == expected:
                return
            assert time.time() < deadline, "%s is %r, expected %r" % (path, got, expected)
            time.sleep(0.3)

    try:
        def boot():
            vm.expect("network up", 90)
            vm.expect(PROMPT)
            if vm.iso:
                vm.command("/set llm 10.0.2.2:%d" % port, PROMPT)
            vm.command("/info", "graphical (full screen)", "absolute pointer", PROMPT)
            scr.wait_pixel(300, 10, BAR, "status bar")
            scr.wait_pixel(300, H - 5, BAR, "home bar")
            scr.wait_pixel(5, 400, BLACK, "the shell fills the screen")
        step("boots full screen with a status bar and a home bar", boot)

        def run_app():
            vm.command('/lua tomos.write_file("apps/clicker.lua", [[%s]])' % CLICK_APP, PROMPT)
            vm.command("/run clicker", "running", PROMPT)
            scr.wait_pixel(500, 400, APP_BG, "the app fills the screen")
            scr.wait_pixel(5, STATUS_H + 5, 0xFFFFFF, "canvas starts right below the status bar")
            assert cat("size") == "%dx%d" % (W, H - STATUS_H - BAR_H)
        step("a graphics app takes the whole screen (gfx.mode() without a size)", run_app)

        def click():
            ptr.click(300, 200)
            wait_file("click", "300,%d" % (200 - STATUS_H))
            ptr.move(20, STATUS_H + 20)
            ptr.button(True)
            scr.wait_pixel(10, STATUS_H + 10, 0xFF0000, "button held down")
            ptr.button(False)
            scr.wait_pixel(10, STATUS_H + 10, 0xFFFFFF, "button released")
        step("mouse clicks reach the app at the right place, short clicks too", click)

        def swipe():
            ptr.drag(200, H - 12, 800, H - 12)   # drag right: the app to the left (the shell)
            scr.wait_pixel(500, 400, BLACK, "shell after swiping right")
            ptr.drag(800, H - 12, 200, H - 12)   # drag left: back to the app
            scr.wait_pixel(500, 400, APP_BG, "app after swiping left")
            ptr.drag(500, H - 12, 560, H - 12)   # too short: stays
            time.sleep(0.6)
            scr.wait_pixel(500, 400, APP_BG, "a short drag springs back")
        step("dragging the home bar sideways switches apps", swipe)

        def keys():
            mon.keys("alt-left")
            scr.wait_pixel(500, 400, BLACK, "shell after Alt+Left")
            mon.keys("alt-right")
            scr.wait_pixel(500, 400, APP_BG, "app after Alt+Right")
            ptr.click(20, 10)                    # "<" in the status bar
            scr.wait_pixel(500, 400, BLACK, "shell after clicking <")
            ptr.click(W - 20, 10)                # ">"
            scr.wait_pixel(500, 400, APP_BG, "app after clicking >")
        step("Alt+arrows and the < > buttons switch apps", keys)

        def small():
            vm.command('/lua tomos.write_file("apps/small.lua", [[%s]])' % SMALL_APP, PROMPT)
            vm.command("/run small", "running", PROMPT)
            # 320x200 is enlarged 3x to 960x600, centred in the 1024x716 area
            ox, oy = (W - 960) // 2, STATUS_H + (H - STATUS_H - BAR_H - 600) // 2
            scr.wait_pixel(ox + 15, oy + 15, 0xFFFF00, "enlarged small canvas")
            scr.wait_pixel(ox + 500, oy + 300, 0x000080, "small canvas background")
            ptr.click(ox + 3 * 100 + 1, oy + 3 * 50 + 1)
            wait_file("small", "100,50")
        step("a small canvas is shown enlarged and clicks are scaled back", small)

        def overview():
            ptr.click(500, H - 12)               # click the home bar
            scr.wait_pixel(100, 150, OVERVIEW, "overview background")
            ptr.click(30, 150)                   # outside the cards: back
            scr.wait_pixel(30, 400, BLACK, "back to the app (small canvas margin)")
            mon.keys("alt-up")
            scr.wait_pixel(100, 150, OVERVIEW, "overview with Alt+Up")
            mon.keys("esc")
            scr.wait_pixel(30, 400, BLACK, "Esc leaves the overview")
        step("clicking the home bar opens the overview of all apps", overview)

        def open_card():
            ptr.click(500, H - 12)
            scr.wait_pixel(100, 150, OVERVIEW, "overview")
            # the current app (3rd) is in the middle; the shell card is two to the left
            ptr.drag(100, 400, 1000, 400, steps=10)  # two cards' width to the right
            time.sleep(0.3)
            ptr.click(500, 400)
            scr.wait_pixel(600, 400, BLACK, "shell opened from its card")
            scr.wait_pixel(5, 400, BLACK, "full screen again")
        step("dragging the cards and clicking one opens that app", open_card)

        def close_card():
            vm.command("/ps", "apps/clicker.lua", "apps/small.lua", PROMPT)
            ptr.click(500, H - 12)
            scr.wait_pixel(100, 150, OVERVIEW, "overview")
            ptr.drag(500 + (W * 5 // 12 + 28), 400, 500 + (W * 5 // 12 + 28), 120, steps=10)  # flick the clicker up
            time.sleep(0.8)
            mon.keys("esc")
            vm.command("/ps", "apps/small.lua", PROMPT)
            assert "clicker" not in vm.out.rsplit("/ps", 1)[1], "clicker should be closed"
            mon.keys("alt-q")                    # the shell is not closable; nothing happens
            time.sleep(0.3)
            vm.command("/ps", "apps/small.lua", PROMPT)
        step("flicking a card up closes that app", close_card)

        def layout():
            vm.command("/layout windows", PROMPT)
            scr.wait_pixel(1000, 600, 0x1F4E5F, "windows desktop")
            vm.command("/layout full", PROMPT)
            scr.wait_pixel(300, H - 5, BAR, "home bar again")
            vm.command("/layout", "layout: full", PROMPT)
        step("/layout switches between full screen and windows", layout)

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
