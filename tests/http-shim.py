#!/usr/bin/env python3
"""Expose a stdio systemd-mcpd over HTTP, for the MCP conformance suite.

The suite's server mode speaks HTTP only (`--url`), and this server is
stdio only. This shim bridges the two for testing and is not part of the
product: nothing here is shipped, and the binary grows no HTTP transport.

It is deliberately dumb. The request body is written to the server's
stdin unchanged and the reply line is returned as the response body
unchanged, so the suite judges the bytes systemd-mcpd actually produces.
A bridge built on an MCP SDK would re-serialize both directions and hide
exactly the mistakes this is meant to find.

What the shim owns, and what therefore cannot be credited to the server
under test: the HTTP status codes, the session and streaming behavior,
and the header validation the transport scenarios check. Those scenarios
belong in an expected-failures baseline, not in a pass count.

While this runs, anything that can reach the port drives the server: it
listens on the loopback address and asks for no credential. A page open
in a browser on the same machine can post to a loopback port, so the
Origin and Host checks below are what keeps this from being a way for a
web page to operate systemd. Run it with read scopes, stop it when the
suite finishes, and do not leave it running with units:write.

    python3 tests/http-shim.py --port 3000 -- ./systemd-mcpd --grant units:read
"""

import argparse
import json
import subprocess
import sys
from http.server import BaseHTTPRequestHandler, HTTPServer

parser = argparse.ArgumentParser()
parser.add_argument("--port", type=int, default=3000)
parser.add_argument("command", nargs=argparse.REMAINDER)
args = parser.parse_args()
command = args.command[1:] if args.command[:1] == ["--"] else args.command
if not command:
    sys.exit("usage: http-shim.py --port N -- <server> [args...]")

server = subprocess.Popen(
    command, stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True, bufsize=1
)


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *_):
        pass  # The suite reports; this would only interleave with it.

    def _send(self, status, body=b"", content_type="application/json"):
        self.send_response(status)
        if body:
            self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        if body:
            self.wfile.write(body)

    def _is_local_client(self):
        # A browser can post to a loopback port from any page, and it
        # sends an Origin when it does. Refuse anything that carries
        # one, and refuse a Host that is not the loopback address, which
        # is what a DNS-rebinding attack has to send.
        if self.headers.get("Origin") is not None:
            return False
        host = (self.headers.get("Host") or "").rsplit(":", 1)[0].strip("[]")
        return host in ("127.0.0.1", "localhost", "::1", "")

    def do_POST(self):
        if not self._is_local_client():
            self._send(403, b'{"error":"cross-origin request refused"}')
            return
        body = self.rfile.read(int(self.headers.get("Content-Length", 0)))
        try:
            message = json.loads(body)
        except ValueError:
            self._send(400, b'{"error":"invalid json"}')
            return
        # One line in, one line out. A body carrying a newline would
        # write two frames and read one reply, leaving every later
        # request paired with the wrong response.
        text = body.decode("utf-8", "replace").strip()
        if "\n" in text or "\r" in text:
            self._send(400, b'{"error":"embedded newline in request"}')
            return

        server.stdin.write(text + "\n")
        server.stdin.flush()

        # A notification draws no reply, so reading one would deadlock.
        if not isinstance(message, dict) or "id" not in message:
            self._send(202)
            return

        reply = server.stdout.readline()
        if not reply:
            self._send(500, b'{"error":"server closed stdout"}')
            return
        self._send(200, reply.strip().encode())

    def do_GET(self):
        # No SSE stream. The transport permits refusing the GET, and this
        # server has no way to originate a message anyway.
        self._send(405)

    def do_DELETE(self):
        self._send(405)


try:
    HTTPServer(("127.0.0.1", args.port), Handler).serve_forever()
except KeyboardInterrupt:
    pass
finally:
    server.terminate()
