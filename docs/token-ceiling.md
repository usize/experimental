# token_ceiling — per-key token spend caps

An explicitly **interim** filter for the Standalone AI Gateway MVP
([ai#758], success criterion 6): name budget keys in the config, cap how
many tokens each may consume per fixed window, and a key that has spent
its ceiling receives `429` with `Retry-After` until its window resets.

[ai#758]: https://github.com/praxis-proxy/ai/issues/758

## Configuration

```yaml
- filter: token_ceiling
  window: 1h                # fixed window: 250ms | 30s | 5m | 1h forms
  key:                      # where the budget key comes from; omit the
    metadata: identity.user #   block for the identity.user metadata key
  # key:
  #   header: x-app-id      # or a request header — client-controlled!
  missing_key: allow        # allow (default) | reject keyless requests
  ceilings:                 # per-key ceilings, tokens per window
    alice: 200000
    bob: 50000
  default_ceiling: 10000    # optional: cap for keys not listed above;
                            #   omit to leave unlisted keys un-capped
```

| Field             | Default              | Meaning                                                                 |
| ----------------- | -------------------- | ----------------------------------------------------------------------- |
| `window`          | required             | Fixed window length. Each key's window starts at its first charge.      |
| `key.metadata`    | `identity.user`      | Read the budget key from this `filter_metadata` key.                    |
| `key.header`      | unset                | Read the budget key from this request header instead.                   |
| `missing_key`     | `allow`              | `allow` passes keyless requests untracked; `reject` returns `403`.      |
| `ceilings`        | `{}`                 | Per-key token budgets. At least one ceiling must be configured.         |
| `default_ceiling` | unset                | Budget for keys not in `ceilings`; omitted means un-capped, untracked.  |

Set exactly one of `key.metadata` / `key.header`. A key value is trimmed;
empty or longer than 256 bytes counts as missing (never truncated, so two
long keys cannot collide into one budget).

## Pipeline placement

Declare `token_ceiling` **before** `token_count` in the filter list.
Response hooks run in reverse declared order, so `token_count` must parse
the provider's usage (writing `token.total` to filter metadata) before
this filter's end-of-stream hook charges it — the same ordering contract
`token_usage_headers` relies on. See
[`examples/configs/token-ceiling.yaml`](../examples/configs/token-ceiling.yaml)
for a complete, verified config.

## Semantics

Accounting is **post-hoc**. Nothing is estimated or reserved at admission;
when a response completes, the `token.total` that `token_count` reported
is charged against the request's budget key. A request is denied only
once its key's *committed* usage has reached the ceiling:

- `429` carries `Retry-After` (seconds until the window resets, rounded
  up) plus `X-RateLimit-Limit-Tokens`, `X-RateLimit-Remaining-Tokens`,
  and `X-RateLimit-Reset-Tokens`, the [ai#124] header convention the
  upstream `token_rate_limit` filter also uses.
- A key can therefore **overshoot** its ceiling by whatever is in flight,
  plus the final admitted request's own usage.
- Responses with no parseable `token.total` (for example the
  `token_count` capture limit overflowed) charge nothing.
- Keys are tracked per gateway instance, in memory, bounded at 10,000
  distinct keys. When the bound is hit, requests for *new* keys fail
  closed with `503` rather than letting header-chosen keys bypass
  ceilings; known keys keep working.

[ai#124]: https://github.com/praxis-proxy/ai/issues/124

## Trust

The `header` key source is **client-controlled**: anyone who can reach
the gateway picks their own budget by picking their own header value.
That is acceptable for the single-user standalone image and for demos.
For anything stronger, use the `metadata` source fed by an
identity-producing filter earlier in the pipeline (the planned
`api_key_auth`, ai#758 Track B) — clients cannot write filter metadata.

## Why not the upstream token_rate_limit filter?

Production token rate limiting is owned by epic [ai#121]; its first
milestone landed in praxis-proxy/ai as the feature-gated
`token_rate_limit` filter ([ai#796]): reservation-based admission,
sliding-window and token-bucket algorithms, optional shared Valkey state.
Its budgets are **per header-matched rule** — one budget shared by every
request the rule matches — with identity-keyed, per-principal quota
explicitly deferred to follow-on integration work ([grid#101]).

`token_ceiling` is the deliberately small per-key answer until that
lands: named keys, one number per key, one fixed window, post-hoc
charging. It keeps a distinct name, config surface, and semantics from
the ai#121 design on purpose — nothing written against this filter should
ever look like a production `token_rate_limit` config, so retiring it
later is a config rewrite, not an untangling.

[ai#121]: https://github.com/praxis-proxy/ai/issues/121
[ai#796]: https://github.com/praxis-proxy/ai/pull/796
[grid#101]: https://github.com/praxis-proxy/grid/issues/101

## Limitations (all deliberate)

- In-memory, single-instance: replicas each enforce their own copy of
  every ceiling. No shared or durable state; a restart resets budgets.
- Fixed window, keyed to each key's first charge — not a sliding window,
  not calendar-aligned.
- Charges `token.total` only; no per-type (input/output/cached)
  weighting.
- No metrics yet; rejections and dropped charges are logged via
  `tracing`.
