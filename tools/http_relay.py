#!/usr/bin/env python3
"""Plain-HTTP to HTTPS relay for chat-completions style endpoints.

tom-os has no TLS. Servers that already speak plain HTTP on the host
(llama.cpp's llama-server, Ollama, ...) need no relay at all: point tom-os at
them directly, e.g. `make run LLM=10.0.2.2:11434 MODEL=qwen3`.

For a hosted endpoint that requires HTTPS and a key, run this relay on the
host. It forwards each request unchanged and adds the Authorization header
from the RELAY_API_KEY environment variable, so the key never enters the VM.

    export RELAY_API_KEY=...
    python3 tools/http_relay.py --upstream https://example.com/v1/chat/completions
    make run MODEL=<model name the upstream expects>

For Claude use tools/claude_gateway.py instead.
"""
import argparse
import json
import os
import sys
import urllib.error
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

args = None


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, fmt, *a):
        sys.stderr.write("[relay] " + fmt % a + "\n")

    def _send(self, status, data):
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.send_header("Connection", "close")
        self.end_headers()
        self.wfile.write(data)
        self.close_connection = True

    def do_POST(self):
        body = self.rfile.read(int(self.headers.get("Content-Length", "0")))
        headers = {"Content-Type": "application/json", "Accept": "application/json"}
        key = os.environ.get("RELAY_API_KEY")
        if key:
            headers["Authorization"] = "Bearer " + key
        req = urllib.request.Request(args.upstream, data=body, headers=headers, method="POST")
        try:
            with urllib.request.urlopen(req, timeout=args.timeout) as resp:
                self._send(resp.status, resp.read())
        except urllib.error.HTTPError as e:
            self._send(e.code, e.read())
        except (urllib.error.URLError, TimeoutError) as e:
            msg = {"error": {"message": "relay could not reach the upstream: %s" % e}}
            self._send(502, json.dumps(msg).encode())


def main():
    global args
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--upstream", required=True, help="full URL of the chat completions endpoint")
    ap.add_argument("--port", type=int, default=8080)
    ap.add_argument("--bind", default="127.0.0.1")
    ap.add_argument("--timeout", type=float, default=600)
    args = ap.parse_args()
    server = ThreadingHTTPServer((args.bind, args.port), Handler)
    print("relay on http://%s:%d -> %s" % (args.bind, args.port, args.upstream))
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass


if __name__ == "__main__":
    main()
