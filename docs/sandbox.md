# Lua Sandbox

IronCrew runs every `crew.lua`, tool definition, agent script, and
hook callback inside an mlua sandbox with a curated set of globals.
This page documents the security-relevant defaults and the env vars
operators use to tune them.

## `env(name)` — read process environment

```lua
local key = env("OPENAI_API_KEY")
```

Returns the env var's value, or `nil` if the var is unset **or not
allowed by the sandbox**.

### Fail-closed allowlist — `IRONCREW_ENV_ALLOWLIST`

`env()` is **deny-by-default**. A crew script can read *only* the
environment variables whose exact names appear in
`IRONCREW_ENV_ALLOWLIST`; every other name returns `nil` and emits a
`tracing::warn!` so operators can detect and audit attempts.

```bash
export IRONCREW_ENV_ALLOWLIST=APP_REGION,FEATURE_FLAGS,AZURE_OPENAI_API_KEY
```

```lua
-- in crew.lua
local region = env("APP_REGION")           -- returns the value (allowlisted)
local db = env("DATABASE_URL")             -- returns nil (not allowlisted)
```

Semantics:

- Comma-separated, exact names (case-insensitive).
- Empty entries (`""` or `,,`) match nothing.
- Defaults to empty when unset — meaning `env()` returns `nil` for
  **everything** until you opt names in.

This posture replaces the earlier suffix denylist, which was
fail-open: it blocked `*_API_KEY`/`*_SECRET`/`*_TOKEN`/`*_PASSWORD`
but silently leaked credentials it didn't anticipate — e.g.
`AWS_SECRET_ACCESS_KEY` (ends `_ACCESS_KEY`), `AWS_ACCESS_KEY_ID`,
and `GOOGLE_APPLICATION_CREDENTIALS`. An allowlist is the only
posture that can't be defeated by an unanticipated variable name.

> **Migration note (breaking):** the previous `IRONCREW_ENV_BLOCKLIST`
> variable and the built-in `*_API_KEY` suffix denylist are gone.
> Crews that relied on reading non-secret vars by default must now
> list those names in `IRONCREW_ENV_ALLOWLIST`.

### Resolution order

```
env("X") →
  1. Is X in IRONCREW_ENV_ALLOWLIST?  → return std::env::var(X)
  2. Otherwise                         → log warn, return nil
```

## What else the sandbox hardens

- **No `dofile`, `loadfile`, `io.*`, `os.execute`** — Lua's built-in I/O is
  removed. File access goes through built-in tools (`file_read`, `file_write`)
  that enforce the project directory boundary and per-file size caps.
- **Scoped `require`** — flows and sub-flows can `require("name")` shared Lua
  modules, but resolution is restricted to the flow's own `_lib/` directory
  (`_lib/name.lua`). Absolute paths, `..` traversal, and path separators in the
  name are rejected with a clean Lua error — no filesystem escape. The Lua
  `package` stdlib is never enabled, so `package.loadlib` and C-module loading
  are unavailable (Lua-source modules only). Modules run in the same sandbox as
  the flow and gain no extra capabilities. See [`docs/tools.md`](tools.md)
  (Shared Modules).
- **SSRF protection on `http_request`** — `IRONCREW_HTTP_ALLOWLIST` /
  blocked private-IP ranges. See `docs/http-scaling.md`.
- **Tool-arg validation** — every built-in tool validates its
  arguments before execution.

## See also

- `docs/best-practices.md` — security checklist for production
- `docs/providers.md` — provider-specific env-var setup
- `docs/cloud-deployment.md` — all env knobs at a glance
