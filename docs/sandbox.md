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

`env()` and `${env.NAME}` interpolation are **deny-by-default**. A crew script
can read *only* the environment variables whose exact names appear in
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
- Defaults to empty when unset — meaning `env()` returns `nil` and
  `${env.NAME}` resolves to an empty string for **everything** until opted in.

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

### VM and execution budgets

Every Lua VM receives a lifetime allocator cap, and every top-level Lua call
activates a fresh instruction and wall-clock budget. Persistent conversation
VMs do not consume instruction/time budget while idle; the budget resets for
the next message.

| Variable | Default | Accepted range |
|---|---:|---:|
| `IRONCREW_LUA_MAX_MEMORY_BYTES` | `33554432` (32 MiB) | 1 MiB–512 MiB |
| `IRONCREW_LUA_MAX_INSTRUCTIONS` | `50000000` | 100000–10000000000 |
| `IRONCREW_LUA_MAX_EXECUTION_SECONDS` | `1800` | 1–86400 seconds |
| `IRONCREW_LUA_MAX_SOURCE_BYTES` | `1048576` (1 MiB) | 1 byte–16 MiB |

Invalid or out-of-range values fail VM construction rather than silently
removing a guard.

Lua/JSON conversion is also bounded: depth 64 (ceiling 256), 100000 visited
nodes (ceiling 1000000), 8 MiB of aggregate strings, and 16 MiB of serialized
output (both byte ceilings 256 MiB). The corresponding variables are
`IRONCREW_LUA_JSON_MAX_DEPTH`, `IRONCREW_LUA_JSON_MAX_NODES`,
`IRONCREW_LUA_JSON_MAX_STRING_BYTES`, and
`IRONCREW_LUA_JSON_MAX_OUTPUT_BYTES`. Cyclic tables fail validation rather than
recursing indefinitely.

Custom-tool `fs.read` and `fs.write` calls default to 1 MiB per operation and
can be tuned with `IRONCREW_LUA_FS_MAX_READ_BYTES` and
`IRONCREW_LUA_FS_MAX_WRITE_BYTES` within 1 byte–16 MiB. Both use
descriptor-relative project access: absolute paths and traversal are rejected,
symlink resolution cannot escape the capability root, non-regular read targets
and final write symlinks are refused, and writes use atomic replacement.

Agent/task/tool definitions also have fixed parser ceilings: names 128 bytes,
provider names 64 bytes, free-text fields 256 KiB, string lists 256 items of
256 bytes each, models 256 bytes, and serialized response/parameter schemas
1 MiB. Agent temperature must be finite in `0..=2`, and `max_tokens` must be
in `1..=1000000`. Sparse/mixed Lua arrays, non-finite numbers, unsupported
Lua values, and cyclic tables fail validation instead of being silently
dropped or coerced.

### Capability restrictions

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
- **SSRF protection on HTTP clients** — `http_request` and Lua `http.*` block
  private and internal IP ranges by default. DNS answers, actual connection
  addresses, and redirect targets are all checked; protected clients ignore
  environment proxy settings to prevent a proxy bypass. Set
  `IRONCREW_ALLOW_PRIVATE_IPS=1` only for trusted workloads that intentionally
  need private-network access. See [Best Practices](best-practices.md#security).
- **Tool-arg validation** — every built-in tool validates its
  arguments before execution.

## See also

- [Best Practices](best-practices.md) — security checklist for production
- [Providers](providers.md) — provider-specific env-var setup
- [Cloud Deployment](cloud-deployment.md) — deployment-oriented env knobs
