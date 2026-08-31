#!/usr/bin/env python3
"""Web UI backend for the switchyard_route mixture-of-models demo.

Serves the static chat page and proxies chat turns to the Praxis gateway
(127.0.0.1:18080), which runs the real switchyard_route filter: a judge call
(OpenRouter, deepseek/deepseek-chat) classifies each turn, and the
filter routes the answer to Ollama (weak) or OpenRouter via the local shim
(strong) — rewriting the model field and clamping to the session's floor so
a session that reaches strong never drops back to weak.

This process does no routing itself. It only labels each answer's tier for
the UI, by comparing the `model` field OpenRouter/Ollama actually reported
back against these two names (must match praxis.yaml's targets).

Usage: python3 server.py [port]  (default 8787)
"""

import json
import re
import sys
import time
import urllib.error
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

ANSI_ESCAPE = re.compile(r"\x1b\[[0-9;]*m")

GATEWAY_URL = "http://127.0.0.1:18080/v1/chat/completions"
WEAK_MODEL = "llama3.2:3b"
STRONG_MODEL = "anthropic/claude-sonnet-4.5"
JUDGE_MODEL = "deepseek/deepseek-chat"

WEB_ROOT = Path(__file__).resolve().parent
GATEWAY_LOG = WEB_ROOT.parent / "server.log"
LOG_BACKFILL_LINES = 60
LOG_POLL_SECS = 0.25

STATIC_FILES = {
    "/": ("index.html", "text/html; charset=utf-8"),
    "/index.html": ("index.html", "text/html; charset=utf-8"),
    "/app.js": ("app.js", "text/javascript; charset=utf-8"),
    "/styles.css": ("styles.css", "text/css; charset=utf-8"),
}


def tier_for(model: str) -> str:
    if model == STRONG_MODEL:
        return "strong"
    if model == WEAK_MODEL:
        return "weak"
    return "unknown"


class Handler(BaseHTTPRequestHandler):
    def _send_json(self, status: int, payload: dict):
        body = json.dumps(payload).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):  # noqa: N802 (http.server API)
        if self.path == "/api/config":
            self._send_json(
                200,
                {"weak_model": WEAK_MODEL, "strong_model": STRONG_MODEL, "judge_model": JUDGE_MODEL},
            )
            return
        if self.path == "/api/logs":
            self._stream_logs()
            return
        entry = STATIC_FILES.get(self.path)
        if entry is None:
            self.send_error(404)
            return
        filename, content_type = entry
        data = (WEB_ROOT / filename).read_bytes()
        self.send_response(200)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_POST(self):  # noqa: N802 (http.server API)
        if self.path != "/api/chat":
            self.send_error(404)
            return
        length = int(self.headers.get("Content-Length", "0"))
        try:
            request = json.loads(self.rfile.read(length) or b"{}")
            session_id = str(request["session_id"])
            message = str(request["message"])
        except (json.JSONDecodeError, KeyError, ValueError):
            self._send_json(400, {"error": "expected {session_id, message}"})
            return

        gateway_request = urllib.request.Request(
            GATEWAY_URL,
            data=json.dumps(
                {
                    "model": "agent-default",
                    "messages": [{"role": "user", "content": message}],
                }
            ).encode(),
            method="POST",
            headers={
                "Content-Type": "application/json",
                "x-switchyard-session-id": session_id,
            },
        )
        started = time.monotonic()
        try:
            with urllib.request.urlopen(gateway_request, timeout=120) as response:
                body = json.loads(response.read())
        except urllib.error.HTTPError as error:
            detail = error.read().decode(errors="replace")
            self._send_json(502, {"error": f"gateway {error.code}: {detail[:400]}"})
            return
        except urllib.error.URLError as error:
            self._send_json(502, {"error": f"gateway unreachable: {error.reason}"})
            return
        latency_ms = round((time.monotonic() - started) * 1000)

        try:
            reply = body["choices"][0]["message"]["content"]
            model = body["model"]
        except (KeyError, IndexError, TypeError):
            self._send_json(502, {"error": f"unexpected gateway response: {json.dumps(body)[:400]}"})
            return

        self._send_json(
            200,
            {
                "reply": reply,
                "model": model,
                "tier": tier_for(model),
                "latency_ms": latency_ms,
            },
        )

    def _stream_logs(self):
        """SSE tail of the gateway's own log — the switchyard_route routing
        decisions, judge callouts, and fail-open reasons, as they happen."""
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-cache")
        self.end_headers()
        try:
            with open(GATEWAY_LOG, "r", errors="replace") as handle:
                backfill = handle.readlines()[-LOG_BACKFILL_LINES:]
                for line in backfill:
                    self._emit_log_line(line)
                while True:
                    line = handle.readline()
                    if not line:
                        time.sleep(LOG_POLL_SECS)
                        continue
                    self._emit_log_line(line)
        except FileNotFoundError:
            self._emit_log_line(f"[web] waiting for {GATEWAY_LOG.name} to be created...\n")
        except (BrokenPipeError, ConnectionResetError):
            pass

    def _emit_log_line(self, line: str):
        clean = ANSI_ESCAPE.sub("", line.rstrip("\n"))
        payload = json.dumps({"line": clean})
        self.wfile.write(f"data: {payload}\n\n".encode())
        self.wfile.flush()

    def log_message(self, format, *args):  # noqa: A002 (matches base signature)
        print(f"[web] {format % args}", flush=True)


if __name__ == "__main__":
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 8787
    server = ThreadingHTTPServer(("127.0.0.1", port), Handler)
    print(f"web UI listening on http://127.0.0.1:{port}", flush=True)
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        server.shutdown()
