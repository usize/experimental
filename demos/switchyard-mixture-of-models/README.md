# switchyard_route: mixture-of-models, live

A graphical demo of `switchyard_route` (see `docs/switchyard-route.md`)
routing between a real local model and a real remote model, with a browser
UI that shows the routing decision on every turn.

- **weak** (efficient tier): [Ollama](https://ollama.com), local, `llama3.2:3b`.
- **strong** (capable tier): [OpenRouter](https://openrouter.ai), remote, `anthropic/claude-sonnet-4.5`.
- **judge** (classifier): OpenRouter, `deepseek/deepseek-chat` — cheap and
  fast, so the judge tax stays small relative to the weak/strong gap.

The point of the demo: an easy turn routes to the small local model for
free. A hard turn escalates to the frontier model. Once a session escalates,
`session_floor` (a host-owned ratchet in the filter, not a Switchyard
feature — see "The no-downgrade guarantee" in the docs) pegs it to `strong`
permanently — a later easy question in the same session does **not** drop
back to weak. `escalation_ratchet: true` in `praxis.yaml` even skips the
judge call once pegged, since its verdict is by then a foregone conclusion.
A fresh session starts back at weak, independently.

## Prerequisites

- `OPENROUTER_API_KEY` in your environment.
- Ollama running locally (`ollama serve`) with `llama3.2:3b` pulled
  (`ollama pull llama3.2:3b`).
- `cargo build -p switchyard-server` able to run (this demo builds it for
  you if the binary isn't already there).

## Run

```console
$ export OPENROUTER_API_KEY=sk-or-...
$ ./run-demo.sh
```

This starts three local processes — `openrouter_shim.py`, the
`switchyard-server` gateway, and the web UI — and opens
`http://127.0.0.1:8787` in your browser. Ctrl-C stops everything.

Ask an easy question first ("What is 2+2?"), then a hard one (there are
preset buttons for both). Watch the tier badge flip from **weak** to
**strong**, the "turns on the small model" counter freeze, and the peg
banner appear. Ask another easy question in the same session — it stays on
**strong**. Click "New session" and ask the easy question again — it's back
on **weak**, because the floor is per-session.

## Why there's a shim

Praxis's `load_balancer` cluster endpoints are `host:port` only — no scheme,
no path rewrite (see `docs/switchyard-route.md` and `docs/gateway-quickstart.md`).
Ollama already serves `/v1/chat/completions`, so the weak leg routes to it
directly. OpenRouter needs HTTPS, a Bearer token, and
`/api/v1/chat/completions`, none of which a bare `host:port` cluster entry
can express — so `openrouter_shim.py` is a tiny local process that adds all
three and forwards to the real OpenRouter endpoint. The **judge** callout
doesn't need this: it's a direct HTTPS sub-request inside the filter and
talks to OpenRouter's real endpoint directly (see `judge.endpoint` in
`praxis.yaml`).

## Troubleshooting

- **Everything is stuck on weak / errors on the hard question:** check
  `server.log` (`tail -f server.log`) for `switchyard_route: routing
  unavailable` — usually a judge timeout or an OpenRouter auth failure.
- **500 "no cluster set in context":** the filter failed open (see
  "Failure topology" in `docs/switchyard-route.md`) — the request passed
  through unmodified and `load_balancer` had nothing to route to.
- **Ollama 404s:** confirm `ollama list` shows `llama3.2:3b` and that
  `curl http://127.0.0.1:11434/v1/chat/completions` responds.
