#!/usr/bin/env python3
"""Full-screen ("phone") layout test: the shell is the home screen, apps open
on top of it, the bottom bar goes home or switches apps, and the card view
shows the running apps. Drives
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

    def nonblack_in(x0, y0, x1, y1):
        scr.grab()
        return any(scr.pixel(x, y) not in (BLACK, -1) for x in range(x0, x1) for y in range(y0, y1))

    try:
        def boot():
            vm.expect("network up", 90)
            vm.expect(PROMPT)
            if vm.iso:
                vm.command("/set llm 10.0.2.2:%d" % port, PROMPT)
            vm.command("/info", "graphical (full screen)", "absolute pointer", PROMPT)
            scr.wait_pixel(300, 10, BAR, "status bar")
            scr.wait_pixel(300, H - 5, BAR, "home bar")
            scr.wait_pixel(5, 400, BLACK, "the home screen fills the screen")
        step("boots to the home screen (the prompt) with a status bar and a home bar", boot)

        def wide_home():
            vm.command('/lua tomos.cls() print(string.rep("#", 120))', PROMPT)
            time.sleep(0.5)
            assert nonblack_in(119 * 8, STATUS_H, 120 * 8, STATUS_H + 16), "column 120 of the first line"
        step("the home screen's terminal is as wide as the screen", wide_home)

        def run_app():
            vm.command('/lua tomos.write_file("apps/clicker.lua", [[%s]])' % CLICK_APP, PROMPT)
            vm.command("/run clicker", "running", PROMPT)
            scr.wait_pixel(500, 400, APP_BG, "the app opens over the home screen")
            scr.wait_pixel(5, STATUS_H + 5, 0xFFFFFF, "canvas starts right below the status bar")
            assert cat("size") == "%dx%d" % (W, H - STATUS_H - BAR_H)
        step("an app opens full screen on top of the home screen", run_app)

        def click():
            ptr.click(300, 200)
            wait_file("click", "300,%d" % (200 - STATUS_H))
            ptr.move(20, STATUS_H + 20)
            ptr.button(True)
            scr.wait_pixel(10, STATUS_H + 10, 0xFF0000, "button held down")
            ptr.button(False)
            scr.wait_pixel(10, STATUS_H + 10, 0xFFFFFF, "button released")
        step("mouse clicks reach the app at the right place, short clicks too", click)

        def home_and_back():
            ptr.click(500, H - 12)               # click the home bar: home
            scr.wait_pixel(600, 400, BLACK, "home screen")
            vm.command("/ps", "apps/clicker.lua", PROMPT)   # still running
            ptr.click(500, H - 12)               # at home: the running apps
            scr.wait_pixel(100, 150, OVERVIEW, "running apps")
            ptr.click(500, 400)                  # the only card
            scr.wait_pixel(600, 400, APP_BG, "back in the app")
            ptr.drag(500, H - 12, 500, H - 200)  # drag the bar up: home
            scr.wait_pixel(600, 400, BLACK, "home after dragging up")
            mon.keys("alt-tab")                  # from home: the first app
            scr.wait_pixel(600, 400, APP_BG, "app after Alt+Tab")
            mon.keys("alt-h")
            scr.wait_pixel(600, 400, BLACK, "home after Alt+H")
        step("the home bar goes home; apps keep running and can be reopened", home_and_back)

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

        def swipe():
            ptr.drag(200, H - 12, 800, H - 12)   # drag right: the app to the left (clicker)
            scr.wait_pixel(600, 400, APP_BG, "clicker after swiping right")
            ptr.drag(800, H - 12, 200, H - 12)   # drag left: small again
            scr.wait_pixel(600, 400, 0x000080, "small after swiping left")
            ptr.drag(500, H - 12, 560, H - 12)   # too short: stays
            time.sleep(0.6)
            scr.wait_pixel(600, 400, 0x000080, "a short drag springs back")
            mon.keys("alt-left")
            scr.wait_pixel(600, 400, APP_BG, "clicker after Alt+Left")
            ptr.click(W - 20, 10)                # ">" in the status bar
            scr.wait_pixel(600, 400, 0x000080, "small after clicking >")
        step("dragging the bottom bar sideways, Alt+arrows and < > switch apps", swipe)

        def overview():
            mon.keys("alt-up")
            scr.wait_pixel(100, 150, OVERVIEW, "running apps with Alt+Up")
            mon.keys("esc")
            scr.wait_pixel(5, 400, BLACK, "Esc goes back to the app (its left margin)")
            scr.wait_pixel(600, 400, 0x000080, "the app again")
            ptr.click(W // 2, 10)                # the title in the status bar
            scr.wait_pixel(100, 150, OVERVIEW, "running apps from the status bar")
            ptr.drag(100, 400, 1000, 400, steps=10)   # one card to the right: clicker in the middle
            time.sleep(0.3)
            ptr.click(500, 400)
            scr.wait_pixel(600, 400, APP_BG, "clicker opened from its card")
        step("the card view shows the running apps and opens one", overview)

        def close():
            mon.keys("alt-q")                    # close the current app: back home
            scr.wait_pixel(600, 400, BLACK, "home after closing the app")
            vm.command("/ps", "apps/small.lua", PROMPT)
            assert "clicker" not in vm.out.rsplit("/ps", 1)[1], "clicker should be closed"
            ptr.click(500, H - 12)
            scr.wait_pixel(100, 150, OVERVIEW, "running apps")
            ptr.drag(500, 400, 500, 120, steps=10)   # flick the only card up
            time.sleep(0.8)
            vm.command("/ps", "(no apps are running)", PROMPT)
            scr.wait_pixel(600, 400, BLACK, "home once no app is left")
            mon.keys("alt-q")                    # home cannot be closed
            time.sleep(0.3)
            vm.command("/ps", "(no apps are running)", PROMPT)
        step("closing an app returns home; flicking a card up closes it", close)

        def layout():
            vm.command("/layout windows", PROMPT)
            scr.wait_pixel(1000, 600, 0x1F4E5F, "windows desktop")
            vm.command("/layout full", PROMPT)
            scr.wait_pixel(300, H - 5, BAR, "home bar again")
            scr.wait_pixel(5, 400, BLACK, "home fills the screen again")
            vm.command("/layout", "layout: full", PROMPT)
        step("/layout switches between full screen and windows", layout)

        def halt():
            vm.send("/halt\n")
            vm.proc.wait(timeout=15)
        step("powers off", halt)
    except Exception as e:
        print("FAILED at step %d: %s" % (steps + 1, e))
        if os.environ.get("TOMOS_DEBUG"):
            print(vm.raw.decode("utf-8", "replace")[-3000:])
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
