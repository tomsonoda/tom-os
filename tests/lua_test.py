#!/usr/bin/env python3
"""Lua compatibility test: runs the scripts in tests/lua inside tom-os and
compares their output with what C Lua 5.4 printed (the .expected files, made
with `lua5.4 name.lua` in that directory). Also checks the limits that keep
an app from taking the system down."""
import glob
import os
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, os.path.join(ROOT, "tools"))
sys.path.insert(0, os.path.join(ROOT, "tests"))
import mock_llm  # noqa: E402
import boot_test  # noqa: E402

PROMPT = boot_test.PROMPT


def main():
    _, port = mock_llm.start()
    vm = boot_test.VM(port, gui=False)
    steps = 0

    def step(name, fn):
        nonlocal steps
        fn()
        steps += 1
        print("ok %2d - %s" % (steps, name))

    try:
        step("boots", lambda: (vm.expect("network up", 90), vm.expect(PROMPT)))

        for path in sorted(glob.glob(os.path.join(ROOT, "tests", "lua", "*.lua"))):
            name = os.path.basename(path)[:-4]

            def compat(name=name):
                vm.send("compat %s\n" % name)
                vm.expect("COMPAT %s " % name, 120)
                vm.expect(PROMPT)
                out = vm.out.rsplit("COMPAT %s " % name, 1)[1]
                assert out.startswith("OK"), "differs from C Lua: " + out[:700]
            step("%s.lua prints what C Lua 5.4 prints" % name, compat)

        step("an app that eats memory is stopped and the system carries on", lambda: (
            vm.command("/lua local t = {} while true do t[#t + 1] = ('x'):rep(1000000) end",
                       "[error]", "out of memory", PROMPT, timeout=120),
            vm.command("/lua print(collectgarbage('count') < 4096, 6 * 7)", "true\t42", PROMPT)))

        step("pcall cannot swallow Ctrl-C", lambda: (
            vm.send("/lua while true do pcall(function() while true do end end) end\n"),
            __import__("time").sleep(1.0), vm.send("\x03"),
            vm.expect("interrupted", 20), vm.expect(PROMPT)))

        step("errors name the file and line, with a traceback", lambda: (
            vm.command('/lua tomos.write_file("apps/bad.lua", "local x = 1\\nlocal function f() error(\\"deep\\") end\\nf()\\n")', PROMPT),
            vm.command("/run bad", "apps/bad.lua:2: deep", "stack traceback", PROMPT)))

        step("require, loadfile and dofile read the in-memory filesystem", lambda: (
            vm.command('/lua tomos.write_file("lib/m.lua", "return {v = 41}") tomos.write_file("d.lua", "return 1")', PROMPT),
            vm.command('/lua print(require("m").v + dofile("d.lua"), loadfile("nope.lua"))', "42\tnil\tcannot open nope.lua", PROMPT)))

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
