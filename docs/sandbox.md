# Lua Sandbox

IronCrew runs every `crew.lua`, tool definition, agent script, and
hook callback inside an mlua sandbox with a curated set of globals.
This page documents the security-relevant defaults and the env vars
operators use to tune them.

## `env(name)` — read process environment

```lua
local key = env("OPENAI_API_KEY")
```

Returns the env var's value, or `nil` if the var is unset **or
blocked by the sandbox**.

### Default blocklist

The following are blocked by default to prevent a crew script from
exfiltrating server-side secrets:

| Blocked | Why |
|---|---|
| Any name ending in `_API_KEY`, `_SECRET`, `_TOKEN`, `_PASSWORD` | Generic credential names |
| `DATABASE_URL` | Postgres connection string with embedded credentials |
| `IRONCREW_API_TOKEN` | Bearer token used by the REST API |
| `IRONCREW_PG_TABLE_PREFIX` | Internal storage layout, not crew-relevant |

A blocked `env()` call returns `nil` and emits a `tracing::warn!` so
operators can detect and audit attempts.

### Granting explicit access — `IRONCREW_ENV_ALLOWLIST`

The default blocklist is conservative. To expose specific vars to
your own crews (because you own the secret, your script needs it,
and you trust the deployment), list them in `IRONCREW_ENV_ALLOWLIST`:

```bash
export IRONCREW_ENV_ALLOWLIST=AZURE_OPENAI_API_KEY,MY_DB_PASSWORD
```

```lua
-- in crew.lua
local azure_key = env("AZURE_OPENAI_API_KEY")  -- now returns the value
```

Semantics:

- Comma-separated, exact names (case-insensitive).
- The allowlist is checked **first** and wins over every block rule —
  including the hardcoded defaults and the `*_API_KEY` suffix
  patterns. This lets you grant precise per-var access without
  disabling the generic patterns for the rest of the codebase.
- Empty entries (`""` or `,,`) match nothing.
- Defaults to empty when unset.

### Extending the blocklist — `IRONCREW_ENV_BLOCKLIST`

For project-specific secrets that don't match the generic suffixes,
add them to the deny set:

```bash
export IRONCREW_ENV_BLOCKLIST=COMPANY_LICENSE,INTERNAL_WEBHOOK
```

Comma-separated, case-insensitive. **Additive** to the hardcoded
defaults. Note that `IRONCREW_ENV_ALLOWLIST` overrides this — if a
name appears in both, the allowlist wins.

### Resolution order

```
env("X") →
  1. Is X in IRONCREW_ENV_ALLOWLIST?            → return std::env::var(X)
  2. Is X in DEFAULT_BLOCKED or matches a       → log warn, return nil
     BLOCKED_SUFFIX or in IRONCREW_ENV_BLOCKLIST?
  3. Otherwise                                   → return std::env::var(X)
```

## What else the sandbox hardens

- **No `require`, `dofile`, `loadfile`, `io.*`, `os.execute`** — Lua's
  built-in I/O is removed. File access goes through built-in tools
  (`file_read`, `file_write`) that enforce the project directory
  boundary and per-file size caps.
- **SSRF protection on `http_request`** — `IRONCREW_HTTP_ALLOWLIST` /
  blocked private-IP ranges. See `docs/http-scaling.md`.
- **Tool-arg validation** — every built-in tool validates its
  arguments before execution.

## See also

- `docs/best-practices.md` — security checklist for production
- `docs/providers.md` — provider-specific env-var setup
- `docs/cloud-deployment.md` — all env knobs at a glance
