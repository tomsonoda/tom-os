#!/usr/bin/env python3
"""Japanese display and input test (graphical mode): text drawn with the
Japanese font, and the input method driven from the QEMU keyboard: romaji to
kana, conversion through the (mock) LLM, katakana, editing."""
import os
import sys
import tempfile
import time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, os.path.join(ROOT, "tools"))
sys.path.insert(0, os.path.join(ROOT, "tests"))
import mock_llm  # noqa: E402
import boot_test  # noqa: E402
from gui_test import Monitor, Screen  # noqa: E402

PROMPT = boot_test.PROMPT
STATUS_H = 24
BLACK = 0x000000
IME_ON = 0x2F6DB5   # indicator background when on
W = 1024


def main():
    _, port = mock_llm.start()
    tmpdir = tempfile.mkdtemp(prefix="tomos-ime-")
    mon_path = os.path.join(tmpdir, "mon")
    vm = boot_test.VM(port, gui=True, monitor=mon_path)
    mon = Monitor(mon_path)
    scr = Screen(mon, tmpdir)
    steps = 0

    def step(name, fn):
        nonlocal steps
        fn()
        steps += 1
        print("ok %2d - %s" % (steps, name))

    def typ(text):
        """Types ASCII text on the QEMU keyboard."""
        names = {" ": "spc", "-": "minus", ",": "comma", ".": "dot", "\n": "ret", "/": "slash"}
        for ch in text:
            mon.cmd("sendkey %s 40" % names.get(ch, ch))
            time.sleep(0.08)

    def nonblack(x0, y0, x1, y1):
        scr.grab()
        return sum(1 for x in range(x0, x1) for y in range(y0, y1) if scr.pixel(x, y) not in (BLACK, -1))

    try:
        step("boots", lambda: (vm.expect("network up", 90), vm.expect(PROMPT),
                               vm.iso and vm.command("/set llm 10.0.2.2:%d" % port, PROMPT)))

        def display():
            vm.command('/lua tomos.cls() print("日本語X")', PROMPT)
            time.sleep(0.5)
            for i in range(3):  # three full-width characters, 16 pixels each
                assert nonblack(16 * i, STATUS_H, 16 * i + 16, STATUS_H + 16) > 20, "glyph %d" % i
            assert nonblack(48, STATUS_H, 56, STATUS_H + 16) > 5, "X right after them (column 7)"
            assert nonblack(56, STATUS_H, 80, STATUS_H + 16) == 0, "nothing after X"
        step("Japanese text is drawn, two columns per character", display)

        def toggle():
            scr.grab()
            off = scr.pixel(W - 40 - 84 + 2, 5)
            mon.cmd("sendkey alt-spc")
            scr.wait_pixel(W - 40 - 84 + 2, 5, IME_ON, "indicator shows あ")
            mon.cmd("sendkey alt-spc")
            time.sleep(0.4)
            scr.wait_pixel(W - 40 - 84 + 2, 5, off, "indicator back to A")
        step("Alt+Space turns the input method on and off (indicator)", toggle)

        def kana():
            mon.cmd("sendkey alt-spc")
            time.sleep(0.2)
            typ("nihongo\n")             # Enter accepts the kana ...
            time.sleep(0.3)
            typ("\n")                    # ... and Enter again sends the line
            vm.expect("echo: にほんご")
            vm.expect(PROMPT)
        step("romaji becomes hiragana; Enter accepts, Enter sends", kana)

        def convert():
            typ("nihongo ")              # Space: the LLM converts
            time.sleep(1.0)
            typ("\n\n")
            vm.expect("echo: 日本語")
            vm.expect(PROMPT)
            typ("nihongo ")
            time.sleep(1.0)
            typ(" ")                     # Space again: another candidate
            time.sleep(1.0)
            typ("\n\n")
            vm.expect("echo: 二本語")
            vm.expect(PROMPT)
        step("Space converts to kanji through the LLM; Space again gives another candidate", convert)

        def sentence():
            typ("burokkukuzusiwotukutte ")
            time.sleep(1.0)
            typ("\n\n")
            vm.expect("echo: ブロック崩しを作って")
            vm.expect(PROMPT)
        step("double consonants and a whole sentence", sentence)

        def katakana_and_edit():
            typ("terebi")
            mon.cmd("sendkey ctrl-k")
            time.sleep(0.3)
            typ("\n\n")
            vm.expect("echo: テレビ")
            vm.expect(PROMPT)
            typ("kakix")                 # x is pending romaji
            mon.cmd("sendkey backspace")  # drops the x
            time.sleep(0.1)
            mon.cmd("sendkey backspace")  # drops き
            time.sleep(0.1)
            typ("nn\n\n")
            vm.expect("echo: かん")
            vm.expect(PROMPT)
            typ("kon")                   # a trailing n becomes ん on Enter
            typ("\n\n")
            vm.expect("echo: こん")
            vm.expect(PROMPT)
        step("Ctrl+K makes katakana; Backspace edits; a final n is ん", katakana_and_edit)

        def mixed():
            mon.cmd("sendkey alt-spc")   # off
            time.sleep(0.2)
            typ("abc ")
            mon.cmd("sendkey alt-spc")   # on
            time.sleep(0.2)
            typ("desu")
            mon.cmd("sendkey alt-spc")   # off commits the kana
            time.sleep(0.2)
            typ(".\n")
            vm.expect("echo: abc です.")
            vm.expect(PROMPT)
        step("switching off keeps what was typed; ASCII and kana mix", mixed)

        def commands():
            mon.cmd("sendkey alt-spc")   # on
            time.sleep(0.2)
            typ("/info\n")               # a / at the start stays a command
            vm.expect("display: graphical")
            vm.expect(PROMPT)
            mon.cmd("sendkey alt-spc")   # off
        step("a leading / is still typed as a command", commands)

        def serial_utf8():
            vm.command("シリアルからの入力", "echo: シリアルからの入力", PROMPT)
        step("UTF-8 typed on the serial console still works", serial_utf8)

        def app_text():
            vm.command('/lua tomos.write_file("apps/jp.lua", [[gfx.mode(320, 100, "日本語") '
                       'while true do gfx.clear(0) gfx.text(0, 0, "日本", 0xFFFFFF) gfx.flip() end]])', PROMPT)
            vm.command("/run jp", "running", PROMPT)
            time.sleep(1.0)
            # 320x100 enlarged 3x: the two characters span 96x48 pixels
            ox, oy = (W - 960) // 2, STATUS_H + (768 - STATUS_H - 28 - 300) // 2
            assert nonblack(ox, oy, ox + 96, oy + 48) > 100, "gfx.text draws Japanese"
        step("apps can draw Japanese with gfx.text", app_text)

        def halt():
            vm.send("/halt\n")
            vm.proc.wait(timeout=15)
        step("powers off", halt)
    except Exception as e:
        print("FAILED at step %d: %s" % (steps + 1, e))
        if os.environ.get("TOMOS_DEBUG"):
            print(vm.raw.decode("utf-8", "replace")[-2000:])
        vm.proc.kill()
        return 1
    print("all %d steps passed" % steps)
    return 0


if __name__ == "__main__":
    sys.exit(main())
