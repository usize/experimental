#!/usr/bin/env bash
set -euo pipefail

# Runs the switchyard_route mixture-of-models demo against REAL models:
#
#   weak   -> Ollama, local        (llama3.2:3b)
#   strong -> OpenRouter, remote   (anthropic/claude-sonnet-4.5)
#   judge  -> OpenRouter, remote   (deepseek/deepseek-v4-flash)
#
# Requires: OPENROUTER_API_KEY in the environment, Ollama running locally
# with llama3.2:3b pulled (`ollama pull llama3.2:3b`), and python3.
#
# Starts, in order: the OpenRouter shim (adapts OpenRouter's real path/auth
# onto a plain-HTTP loopback cluster), the switchyard-server gateway, and the
# web UI — then opens the UI in a browser.

cd "$(dirname "$0")"

: "${OPENROUTER_API_KEY:?set OPENROUTER_API_KEY to your OpenRouter key}"

if ! curl -s -m 2 http://127.0.0.1:11434/api/tags > /dev/null; then
  echo "Ollama is not reachable on 127.0.0.1:11434 — run 'ollama serve' first." >&2
  exit 1
fi
OLLAMA_MODELS="$(ollama list 2>/dev/null)"
if ! grep -q '^llama3.2:3b' <<< "$OLLAMA_MODELS"; then
  echo "llama3.2:3b is not pulled — run 'ollama pull llama3.2:3b' first." >&2
  exit 1
fi

SERVER_BIN=../../target/debug/switchyard-server
if [[ ! -x "$SERVER_BIN" ]]; then
  echo "building switchyard-server..." >&2
  (cd ../.. && cargo build -p switchyard-server)
fi

python3 openrouter_shim.py &
SHIM_PID=$!

RUST_LOG="${RUST_LOG:-info,switchyard_filters=debug}" "$SERVER_BIN" > server.log 2>&1 &
SERVER_PID=$!

python3 web/server.py &
WEB_PID=$!

cleanup() {
  kill "$SHIM_PID" "$SERVER_PID" "$WEB_PID" 2>/dev/null || true
}
trap cleanup EXIT

sleep 2
if ! kill -0 "$SERVER_PID" 2>/dev/null; then
  echo "switchyard-server exited immediately — check server.log:" >&2
  tail -n 40 server.log >&2 || true
  exit 1
fi

echo
echo "gateway:  http://127.0.0.1:18080/v1/chat/completions"
echo "web UI:   http://127.0.0.1:8787"
echo "filter log: tail -f $(pwd)/server.log"
echo

if command -v open > /dev/null; then
  open http://127.0.0.1:8787
fi

echo "Ctrl-C to stop."
wait "$WEB_PID"
