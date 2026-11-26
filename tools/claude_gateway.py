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


def complete(req, progress):
    """Asks Claude, streaming so long replies neither time out nor go silent.
    progress(chars) is called as text and tool arguments arrive."""
    system, messages, tools = to_claude(req)
    for t in tools:
        t["eager_input_streaming"] = True  # stream large tool arguments (whole apps) as written
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
    chars = 0
    with client.beta.messages.stream(**params) as stream:
        for event in stream:
            if event.type == "text":
                chars += len(event.text)
            elif event.type == "input_json":
                chars += len(event.partial_json)
            else:
                continue
            progress(chars)
        return from_claude(stream.get_final_message())


def error(message):
    return {"error": {"message": message}}


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, fmt, *a):
        pass  # replaced by the messages in do_POST

    def note(self, text):
        sys.stderr.write("[gateway %s] %s\n" % (time.strftime("%H:%M:%S"), text))
        sys.stderr.flush()

    def _chunk(self, data):
        self.wfile.write(b"%x\r\n%s\r\n" % (len(data), data))
        self.wfile.flush()

    def do_POST(self):
        try:
            req = json.loads(self.rfile.read(int(self.headers.get("Content-Length", "0"))))
        except ValueError:
            req = None
        msgs = (req or {}).get("messages", [])
        last = next((m.get("content") or "" for m in reversed(msgs) if m.get("role") in ("user", "tool")), "")
        self.note("request from %s: %d messages, last: %r - asking %s"
                  % (self.client_address[0], len(msgs), last[:60], args.model))

        # Answer at once and send a space every few seconds until Claude is
        # done (JSON allows leading whitespace), so tom-os knows the gateway is
        # alive and the connection never sits idle for minutes.
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Transfer-Encoding", "chunked")
        self.send_header("Connection", "close")
        self.end_headers()
        self.close_connection = True

        start = time.time()
        lock = threading.Lock()
        stop = threading.Event()
        state = {"chars": 0, "gone": False, "last_note": start}

        def keepalive():
            while not stop.wait(5):
                with lock:
                    if state["gone"]:
                        return
                    try:
                        self._chunk(b" ")
                    except OSError:
                        state["gone"] = True
                        return
                now = time.time()
                if now - state["last_note"] >= 15:
                    state["last_note"] = now
                    self.note("still waiting for Claude: %.0f s, %d characters so far"
                              % (now - start, state["chars"]))

        def progress(chars):
            state["chars"] = chars
            if state["gone"]:
                raise ConnectionAbortedError

        ka = threading.Thread(target=keepalive, daemon=True)
        ka.start()
        try:
            if req is None:
                payload = error("request body is not valid JSON")
            else:
                payload = complete(req, progress)
                msg = payload["choices"][0]["message"]
                calls = [c["function"]["name"] for c in msg.get("tool_calls", [])]
                self.note("reply after %.0f s: %s" % (time.time() - start,
                          "tool calls " + ", ".join(calls) if calls
                          else "%d characters of text" % len(msg["content"] or "")))
        except ConnectionAbortedError:
            payload = None
        except anthropic.AuthenticationError:
            payload = error("the gateway has no valid Anthropic credential "
                            "(set ANTHROPIC_API_KEY or run `ant auth login`)")
        except anthropic.RateLimitError:
            payload = error("rate limited by the Claude API; try again shortly")
        except anthropic.APIStatusError as e:
            payload = error("Claude API error %d: %s" % (e.status_code, e.message))
        except anthropic.APIConnectionError as e:
            payload = error("the gateway could not reach the Claude API: %s" % e)
        except ValueError:  # unparseable streamed tool arguments
            payload = error("Claude's reply could not be read; please try again")
        finally:
            stop.set()
            ka.join()
        if payload and "error" in payload:
            self.note(payload["error"]["message"])
        if state["gone"] or payload is None:
            self.note("tom-os stopped waiting (Ctrl-C?) after %.0f s; reply dropped" % (time.time() - start))
            return
        try:
            self._chunk(json.dumps(payload).encode())
            self._chunk(b"")
        except OSError:
            self.note("tom-os closed the connection before the reply was sent")


def main():
    global client, args
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--port", type=int, default=8080)
    ap.add_argument("--bind", default="127.0.0.1",
                    help="address to listen on (QEMU user networking reaches 127.0.0.1 as 10.0.2.2)")
    ap.add_argument("--model", default=DEFAULT_MODEL)
    ap.add_argument("--effort", default="medium", choices=["low", "medium", "high", "xhigh", "max"])
    args = ap.parse_args()
    # Streaming replies: the timeout applies between pieces of the reply, not
    # to the whole reply, so a stalled request fails instead of hanging.
    client = anthropic.Anthropic(timeout=120.0, max_retries=2)
    server = ThreadingHTTPServer((args.bind, args.port), Handler)
    print("Claude gateway on http://%s:%d  model=%s effort=%s" % (args.bind, args.port, args.model, args.effort))
    print("Waiting for tom-os. Writing a whole app can take a minute or two per reply.", flush=True)
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass


if __name__ == "__main__":
    main()
