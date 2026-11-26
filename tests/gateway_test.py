#!/usr/bin/env python3
"""Tests tools/claude_gateway.py end to end: tom-os in QEMU talks to the real
gateway, and the gateway talks to a fake Claude client that streams a slow
reply. No API key or network access is needed. Skipped if the anthropic
package is not installed."""
import os
import sys
import threading
import time
from http.server import ThreadingHTTPServer
from types import SimpleNamespace as NS

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, os.path.join(ROOT, "tools"))
sys.path.insert(0, os.path.join(ROOT, "tests"))

try:
    import anthropic  # noqa: F401
except ImportError:
    print("skipped: the anthropic package is not installed (pip install anthropic)")
    sys.exit(0)

import boot_test  # noqa: E402
import claude_gateway as gw  # noqa: E402

PROMPT = boot_test.PROMPT
APP = 'print("calc ready", 6 * 7)'


class FakeStream:
    """Streams `pieces` (event type, text) with a delay, like the SDK stream."""

    def __init__(self, pieces, delay, final):
        self.pieces, self.delay, self.final = pieces, delay, final

    def __enter__(self):
        return self

    def __exit__(self, *exc):
        return False

    def __iter__(self):
        for kind, text in self.pieces:
            time.sleep(self.delay)
            if kind == "text":
                yield NS(type="text", text=text)
            else:
                yield NS(type="input_json", partial_json=text)

    def get_final_message(self):
        return self.final


class FakeMessages:
    def __init__(self):
        self.calls = []
        self.mode = "ok"

    def stream(self, **params):
        self.calls.append(params)
        if self.mode == "unreachable":
            raise anthropic.APIConnectionError(request=None)
        tool_results = [b for m in params["messages"] if m["role"] == "user"
                        for b in m["content"] if b.get("type") == "tool_result"]
        if self.mode == "slow" and not tool_results:
            # about 14 s with nothing but thinking, then a whole app as a tool call
            content = [NS(type="text", text="Writing it."),
                       NS(type="tool_use", id="toolu_1", name="write_file",
                          input={"path": "apps/calc.lua", "content": APP})]
            pieces = [("text", "")] * 14 + [("text", "Writing it."), ("json", '{"path": "apps/calc.lua"'),
                                             ("json", ', "content": "..."}')]
            return FakeStream(pieces, 1.0, NS(id="m1", model="fake", stop_reason="tool_use", content=content))
        if self.mode == "slow":
            return FakeStream([("text", "Done.")], 0.2,
                              NS(id="m2", model="fake", stop_reason="end_turn",
                                 content=[NS(type="text", text="Done. Run it with /run calc")]))
        if self.mode == "endless":
            return FakeStream([("text", ".")] * 600, 0.1,
                              NS(id="m3", model="fake", stop_reason="end_turn", content=[]))
        return FakeStream([("text", "hi")], 0.1,
                          NS(id="m4", model="fake", stop_reason="end_turn", content=[NS(type="text", text="hi there")]))


class LogCapture:
    def __init__(self):
        self.lines = []

    def write(self, s):
        self.lines.append(s)

    def flush(self):
        pass

    def text(self):
        return "".join(self.lines)


def main():
    fake = FakeMessages()
    gw.client = NS(beta=NS(messages=fake))
    gw.args = NS(model="fake-model", effort="medium")
    log = LogCapture()
    sys.stderr = log
    server = ThreadingHTTPServer(("127.0.0.1", 0), gw.Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()

    vm = boot_test.VM(server.server_address[1], gui=False)
    steps = 0

    def step(name, fn):
        nonlocal steps
        fn()
        steps += 1
        sys.stdout.write("ok %2d - %s\n" % (steps, name))
        sys.stdout.flush()

    def wait_log(text, timeout=10):
        deadline = time.time() + timeout
        while text not in log.text():
            if time.time() > deadline:
                raise AssertionError("gateway log lacks %r:\n%s" % (text, log.text()[-800:]))
            time.sleep(0.1)

    try:
        step("boots", lambda: (vm.expect("network up", 60), vm.expect(PROMPT)))

        def quick():
            vm.command("hello", "hi there", PROMPT)
            wait_log("request from")
            wait_log("reply after")
        step("a short reply goes through the gateway", quick)

        def slow():
            fake.mode = "slow"
            vm.send("make a calculator\n")
            vm.expect("thinking... 8 s", 20)
            wait_log("still waiting for Claude")
            vm.expect("[write_file] apps/calc.lua", 30)
            vm.expect("Done. Run it with /run calc", 30)
            vm.expect(PROMPT)
            vm.command("/run calc", "calc ready\t42", PROMPT)
            params = fake.calls[-1]
            assert params["tools"][0]["eager_input_streaming"] is True
            assert params["fallbacks"] == "default"
        step("a slow streamed reply with a tool call arrives (keep-alive, progress log)", slow)

        def cancel():
            fake.mode = "endless"
            vm.send("write a novel\n")
            vm.expect("thinking... 2 s", 20)
            vm.send("\x03")
            vm.expect("interrupted")
            vm.expect(PROMPT)
            wait_log("tom-os stopped waiting", 20)
            fake.mode = "ok"
            vm.command("hello", "hi there", PROMPT)
        step("Ctrl-C stops the request; the gateway notices; the next one works", cancel)

        def unreachable():
            fake.mode = "unreachable"
            vm.command("hello", "LLM error: the gateway could not reach the Claude API", PROMPT)
            fake.mode = "ok"
        step("Claude API errors are shown in tom-os", unreachable)

        def halt():
            vm.send("/halt\n")
            vm.proc.wait(timeout=15)
        step("powers off", halt)
    except Exception as e:
        sys.stdout.write("FAILED at step %d: %s\n" % (steps + 1, e))
        vm.proc.kill()
        return 1
    sys.stdout.write("all %d steps passed\n" % steps)
    return 0


if __name__ == "__main__":
    sys.exit(main())
