#!/usr/bin/env python3
"""A Fluid function: a long-lived HTTP server that handles many concurrent
requests per instance (the whole point of Fluid compute).

Listens on $PORT. ThreadingHTTPServer => one instance serves concurrent
requests. `/api/slow` sleeps to make concurrency visible.
"""
import json
import os
import socket
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

PID = os.getpid()
HOST = socket.gethostname()
STARTED = time.time()


class Handler(BaseHTTPRequestHandler):
    # HTTP/1.1 keeps connections alive (with content-length) so the gateway can
    # reuse them as persistent tunnels.
    protocol_version = "HTTP/1.1"

    def _send(self, code, obj):
        body = json.dumps(obj).encode()
        self.send_response(code)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        if self.path.startswith("/api/verylong"):
            time.sleep(5.0)  # exceeds a small max_duration -> gateway 504
            return self._send(200, {"verylong": True, "pid": PID})

        if self.path.startswith("/api/boom"):
            # Simulate a handler error; must NOT take down other requests.
            raise RuntimeError("intentional boom")

        if self.path.startswith("/api/slow"):
            time.sleep(1.0)  # simulate an I/O-bound wait (e.g. an LLM call)
            return self._send(200, {"slow": True, "pid": PID, "host": HOST, "path": self.path})

        if self.path.startswith("/api/stream"):
            # Stream a response in pieces (LLM-style token streaming). We close
            # the connection at the end so the gateway streams to EOF (no reuse).
            self.close_connection = True
            lines = [f"data: chunk {i}\n\n".encode() for i in range(6)]
            body = b"".join(lines)
            self.send_response(200)
            self.send_header("content-type", "text/event-stream")
            self.send_header("content-length", str(len(body)))
            self.send_header("connection", "close")
            self.end_headers()
            for line in lines:
                self.wfile.write(line)
                self.wfile.flush()
                time.sleep(0.2)
            return

        if self.path.startswith("/api/cached"):
            # CDN-cacheable response (edge caches it for 60s).
            body = json.dumps({"cached": True, "pid": PID, "ts": time.time()}).encode()
            self.send_response(200)
            self.send_header("content-type", "application/json")
            self.send_header("content-length", str(len(body)))
            self.send_header("cache-control", "public, max-age=60")
            self.end_headers()
            self.wfile.write(body)
            return

        if self.path.startswith("/api/bg"):
            # waitUntil: respond immediately and declare background work via a
            # header. The connection stays reusable; the gateway keeps the
            # instance accounted as active for the window. Client isn't blocked.
            body = json.dumps({"ok": True, "note": "responded now; bg via waitUntil", "pid": PID}).encode()
            self.send_response(200)
            self.send_header("content-type", "application/json")
            self.send_header("content-length", str(len(body)))
            self.send_header("x-fluid-wait-until-ms", "600")
            self.end_headers()
            self.wfile.write(body)
            return

        return self._send(200, {
            "msg": "hello from a Fluid function",
            "pid": PID,
            "host": HOST,
            "uptime_s": round(time.time() - STARTED, 1),
            "path": self.path,
        })

    def log_message(self, *args):
        pass  # quiet


if __name__ == "__main__":
    port = int(os.environ.get("PORT", "8000"))
    ThreadingHTTPServer(("127.0.0.1", port), Handler).serve_forever()
