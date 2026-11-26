#!/usr/bin/env python3
"""Host-side gateway that lets tom-os talk to Claude.

tom-os has no TLS, so it sends plain-HTTP chat requests (the wire format it
uses for every backend) to this process on the host. The gateway translates
them into Claude Messages API calls through the official Anthropic SDK and
translates the answer back. The API credential stays on the host; it never
enters the VM.

    pip install anthropic
    export ANTHROPIC_API_KEY=...        # or: ant auth login
    python3 tools/claude_gateway.py     # listens on 127.0.0.1:8080
    make run                            # in another terminal
"""
import argparse
import json
import sys
import threading
import time
from collections import OrderedDict
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import anthropic

DEFAULT_MODEL = "claude-opus-5-5"
MAX_TOKENS = 16000

client = None
args = None

# The OS only keeps text and tool calls. Claude's replies also carry thinking
# blocks that must be sent back unchanged on later turns, so the full content
# of each reply is remembered here, keyed by what the OS will echo back.
_replies = OrderedDict()
_replies_lock = threading.Lock()


def _remember(key, content):
    with _replies_lock:
        _replies[key] = content
        while len(_replies) > 2000:
            _replies.popitem(last=False)


def _recall(key):
    with _replies_lock:
        return _replies.get(key)


def _reply_key(tool_ids, text):
    return ("tools",) + tuple(tool_ids) if tool_ids else ("text", text or "")


def to_claude(req):
    """Chat-completions style request -> (system, messages, tools)."""
    system_parts, messages = [], []

    def add(role, blocks):
        if messages and messages[-1]["role"] == role and isinstance(messages[-1]["content"], list) \
                and role == "user":
            messages[-1]["content"].extend(blocks)  # all tool results of a turn in one message
        else:
            messages.append({"role": role, "content": blocks})

    for m in req.get("messages", []):
        role, text = m.get("role"), m.get("content") or ""
        if role == "system":
            system_parts.append(text)
        elif role == "user":
            add("user", [{"type": "text", "text": text}])
        elif role == "tool":
            add("user", [{"type": "tool_result", "tool_use_id": m.get("tool_call_id", ""),
                          "content": text or "(no output)",
                          "is_error": text.startswith("error:")}])
        elif role == "assistant":
            calls = m.get("tool_calls") or []
            original = _recall(_reply_key([c.get("id") for c in calls], text))
            if original is not None:
                messages.append({"role": "assistant", "content": original})
                continue
            blocks = [{"type": "text", "text": text}] if text else []
            for c in calls:
                fn = c.get("function", {})
                try:
                    tool_input = json.loads(fn.get("arguments") or "{}")
                except ValueError:
                    tool_input = {}
                blocks.append({"type": "tool_use", "id": c.get("id", ""),
                               "name": fn.get("name", ""), "input": tool_input})
            if blocks:
                messages.append({"role": "assistant", "content": blocks})

    tools = [{"name": t["function"]["name"],
              "description": t["function"].get("description", ""),
              "input_schema": t["function"].get("parameters") or {"type": "object", "properties": {}}}
             for t in req.get("tools", []) if t.get("type") == "function"]
    return "\n\n".join(system_parts), messages, tools


def from_claude(response):
    """Claude Message -> chat-completions style response."""
    text = "".join(b.text for b in response.content if b.type == "text")
    calls = [{"id": b.id, "type": "function",
              "function": {"name": b.name, "arguments": json.dumps(b.input)}}
             for b in response.content if b.type == "tool_use"]

    if response.stop_reason == "refusal":
        text, calls = "(The model declined this request.)", []
    elif response.stop_reason == "max_tokens":
        # A cut-off tool call must not be executed.
        text, calls = (text + "\n[The reply was cut off at the output limit.]").strip(), []
    else:
        _remember(_reply_key([c["id"] for c in calls], text), response.content)

    message = {"role": "assistant", "content": text or None}
    if calls:
        message["tool_calls"] = calls
    return {"id": response.id, "object": "chat.completion", "model": response.model,
            "choices": [{"index": 0, "message": message,
                         "finish_reason": "tool_calls" if calls else "stop"}]}


def complete(req):
    system, messages, tools = to_claude(req)
    params = dict(
        model=args.model,
        max_tokens=MAX_TOKENS,
        thinking={"type": "adaptive"},
        output_config={"effort": args.effort},
        system=system,
        messages=messages,
        # If a safety classifier declines, retry on the fallback model the API picks.
        betas=["server-side-fallback-2026-07-01"],
        fallbacks="default",
    )
    if tools:
        params["tools"] = tools
    return from_claude(client.beta.messages.create(**params))


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, fmt, *a):
        pass  # replaced by the messages in do_POST

    def note(self, text):
        sys.stderr.write("[gateway %s] %s\n" % (time.strftime("%H:%M:%S"), text))
        sys.stderr.flush()

    def _send(self, status, payload):
        data = json.dumps(payload).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.send_header("Connection", "close")
        self.end_headers()
        self.wfile.write(data)
        self.close_connection = True

    def do_POST(self):
        try:
            req = json.loads(self.rfile.read(int(self.headers.get("Content-Length", "0"))))
        except ValueError:
            return self._send(400, {"error": {"message": "request body is not valid JSON"}})
        msgs = req.get("messages", [])
        last = next((m.get("content") or "" for m in reversed(msgs) if m.get("role") in ("user", "tool")), "")
        self.note("request from %s: %d messages, last: %r - waiting for %s"
                  % (self.client_address[0], len(msgs), last[:60], args.model))
        start = time.time()
        try:
            reply = complete(req)
            msg = reply["choices"][0]["message"]
            calls = [c["function"]["name"] for c in msg.get("tool_calls", [])]
            self.note("reply after %.0f s: %s" % (time.time() - start,
                      "tool calls " + ", ".join(calls) if calls else "%d characters of text" % len(msg["content"] or "")))
            self._send(200, reply)
        except (BrokenPipeError, ConnectionResetError):
            self.note("tom-os stopped waiting (Ctrl-C?) after %.0f s; reply dropped" % (time.time() - start))
        except anthropic.AuthenticationError:
            self.note("no valid Anthropic credential")
            self._send(401, {"error": {"message": "the gateway has no valid Anthropic credential "
                                                  "(set ANTHROPIC_API_KEY or run `ant auth login`)"}})
        except anthropic.RateLimitError:
            self._send(429, {"error": {"message": "rate limited by the Claude API; try again shortly"}})
        except anthropic.APIStatusError as e:
            self.note("Claude API error %d: %s" % (e.status_code, e.message))
            self._send(e.status_code, {"error": {"message": "Claude API error: %s" % e.message}})
        except anthropic.APIConnectionError as e:
            self.note("could not reach the Claude API: %s" % e)
            self._send(502, {"error": {"message": "gateway could not reach the Claude API: %s" % e}})


def main():
    global client, args
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--port", type=int, default=8080)
    ap.add_argument("--bind", default="127.0.0.1",
                    help="address to listen on (QEMU user networking reaches 127.0.0.1 as 10.0.2.2)")
    ap.add_argument("--model", default=DEFAULT_MODEL)
    ap.add_argument("--effort", default="medium", choices=["low", "medium", "high", "xhigh", "max"])
    args = ap.parse_args()
    client = anthropic.Anthropic()
    server = ThreadingHTTPServer((args.bind, args.port), Handler)
    print("Claude gateway on http://%s:%d  model=%s effort=%s" % (args.bind, args.port, args.model, args.effort))
    print("Waiting for tom-os. Writing a whole app can take a minute or two per reply.", flush=True)
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass


if __name__ == "__main__":
    main()
