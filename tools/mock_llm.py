#!/usr/bin/env python3
"""Scripted stand-in for an OpenAI-compatible chat completions server.

Used by tests/boot_test.py so the whole agent loop (HTTP, JSON, tool calls,
app generation) can be exercised without a real model. It can also be run by
hand:  python3 tools/mock_llm.py 8080
"""
import json
import sys
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

GREET_APP = r'''-- greeter: asks for names until the user types q
while true do
  io.write("Your name? ")
  local name = io.read("l")
  if name == nil or name == "q" then break end
  print(string.format("Hello, %s! (%d letters)", name, #name))
end
print("bye")
'''

CHECK_CODE = 'local f = assert(loadfile("apps/greet.lua"))\nprint("syntax ok", 6 * 7)\n'

BIG_TEXT = "".join("line %05d of the big reply\n" % i for i in range(4000)) + "END-OF-BIG"


def tool_call(call_id, name, args, content=None):
    return {
        "role": "assistant",
        "content": content,
        "tool_calls": [
            {
                "id": call_id,
                "type": "function",
                "function": {"name": name, "arguments": json.dumps(args)},
            }
        ],
    }


def reply_for(req):
    """Returns (message, use_chunked_encoding)."""
    msgs = req["messages"]
    last_user = max(i for i, m in enumerate(msgs) if m["role"] == "user")
    user_text = msgs[last_user]["content"]
    tool_results = [m for m in msgs[last_user + 1:] if m["role"] == "tool"]

    if user_text == "big":
        return {"role": "assistant", "content": BIG_TEXT}, False

    if user_text == "launch":
        if not tool_results:
            return tool_call("call_l1", "launch_app", {"path": "apps/greet.lua"}), False
        return {"role": "assistant", "content": "launch result: " + tool_results[-1]["content"]}, True

    if "app" in user_text:
        if len(tool_results) == 0:
            return tool_call("call_1", "write_file",
                             {"path": "apps/greet.lua", "content": GREET_APP},
                             "Writing the app. あ"), False
        if len(tool_results) == 1:
            return tool_call("call_2", "run_lua", {"code": CHECK_CODE}), True
        if "syntax ok\t42" in tool_results[1]["content"]:
            return {"role": "assistant", "content": "Done. Run it with /run greet"}, True
        return {"role": "assistant",
                "content": "UNEXPECTED tool result: " + tool_results[1]["content"]}, False

    return {"role": "assistant", "content": "echo: " + user_text}, False


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def do_POST(self):
        body = self.rfile.read(int(self.headers.get("Content-Length", "0")))
        try:
            req = json.loads(body)
            assert req["messages"][0]["role"] == "system", "first message must be system"
            assert req.get("model"), "model missing"
            assert len(req["tools"]) == 6, "expected 6 tools"
            message, chunked = reply_for(req)
            status, payload = 200, {"id": "mock", "object": "chat.completion",
                                    "choices": [{"index": 0, "message": message,
                                                 "finish_reason": "stop"}]}
        except Exception as e:  # report request problems back to the client
            status, chunked = 400, False
            payload = {"error": {"message": "mock: %s: %s" % (type(e).__name__, e)}}

        data = json.dumps(payload).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Connection", "close")
        if chunked:
            self.send_header("Transfer-Encoding", "chunked")
            self.end_headers()
            for i in range(0, len(data), 700):
                part = data[i:i + 700]
                self.wfile.write(b"%x\r\n%s\r\n" % (len(part), part))
            self.wfile.write(b"0\r\n\r\n")
        else:
            self.send_header("Content-Length", str(len(data)))
            self.end_headers()
            self.wfile.write(data)
        self.close_connection = True


def start(port=0):
    """Starts the server in a background thread; returns (server, port)."""
    server = ThreadingHTTPServer(("127.0.0.1", port), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    return server, server.server_address[1]


if __name__ == "__main__":
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 8080
    server, port = start(port)
    print("mock LLM listening on http://127.0.0.1:%d/v1/chat/completions" % port)
    try:
        threading.Event().wait()
    except KeyboardInterrupt:
        pass
