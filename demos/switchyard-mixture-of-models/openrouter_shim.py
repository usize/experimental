#!/usr/bin/env python3
"""Local adapter that puts OpenRouter behind a plain-HTTP loopback cluster.

Praxis's `load_balancer` cluster endpoints are `host:port` only — no scheme,
no path rewrite (see docs/switchyard-route.md). OpenRouter needs HTTPS, a
Bearer token, and a `/api/v1/chat/completions` path that doesn't match the
`/v1/chat/completions` path the demo's gateway and Ollama both use. This
shim absorbs all three: it listens on plain HTTP on loopback, injects the
Authorization header from OPENROUTER_API_KEY, and forwards to the real
OpenRouter endpoint.

Usage: OPENROUTER_API_KEY=... python3 openrouter_shim.py [port]  (default 18097)
"""

import json
import os
import ssl
import sys
import urllib.error
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

OPENROUTER_URL = "https://openrouter.ai/api/v1/chat/completions"


def tls_context() -> ssl.SSLContext:
    """A verified TLS context, preferring certifi's CA bundle.

    Some python.org macOS builds ship a cert.pem that resolves to nothing,
    so the interpreter's own default context fails every HTTPS request with
    CERTIFICATE_VERIFY_FAILED. certifi's bundle is a reliable fallback.
    """
    try:
        import certifi

        return ssl.create_default_context(cafile=certifi.where())
    except ImportError:
        return ssl.create_default_context()


TLS_CONTEXT = tls_context()


def api_key() -> str:
    key = os.environ.get("OPENROUTER_API_KEY", "")
    if not key:
        print("OPENROUTER_API_KEY is unset or empty", file=sys.stderr)
        sys.exit(1)
    return key


class Shim(BaseHTTPRequestHandler):
    def do_POST(self):  # noqa: N802 (http.server API)
        length = int(self.headers.get("Content-Length", "0"))
        body = self.rfile.read(length)
        request = urllib.request.Request(
            OPENROUTER_URL,
            data=body,
            method="POST",
            headers={
                "Content-Type": "application/json",
                "Authorization": f"Bearer {api_key()}",
            },
        )
        try:
            with urllib.request.urlopen(request, timeout=120, context=TLS_CONTEXT) as response:
                payload = response.read()
                status = response.status
        except urllib.error.HTTPError as error:
            payload = error.read()
            status = error.code
        except urllib.error.URLError as error:
            payload = json.dumps({"error": {"message": str(error.reason)}}).encode()
            status = 502

        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def log_message(self, format, *args):  # noqa: A002 (matches base signature)
        print(f"[openrouter-shim] {format % args}", flush=True)


if __name__ == "__main__":
    api_key()  # fail fast on a missing key
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 18097
    server = ThreadingHTTPServer(("127.0.0.1", port), Shim)
    print(f"openrouter-shim listening on 127.0.0.1:{port} -> {OPENROUTER_URL}", flush=True)
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        server.shutdown()
