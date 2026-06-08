# Shared Modules (`require` from `_lib`)

Flows and sub-flows can share Lua code with `require`, resolved **only** from a
sandboxed `_lib/` directory next to the flow. This keeps common logic — text
helpers, Arango query wrappers, or security-sensitive credential
fetch-and-decrypt — in one place instead of copy-pasted across every flow.

```
examples/shared-modules/
├── crew.lua          # top-level flow: require("textutil") + run_flow("report.lua")
├── report.lua        # sub-flow: also require("textutil") from the same _lib
├── _lib/
│   └── textutil.lua  # the shared module (returns a table of functions)
└── README.md
```

## Run it

No LLM or API key needed — this example exercises the module system offline.

```bash
ironcrew validate examples/shared-modules
ironcrew run examples/shared-modules
```

Expected output:

```
Top-level flow using require('textutil'):
  title (titlecase): Hello, Ironcrew World!
  slug:              hello-ironcrew-world
  word count:        3

Sub-flow report.lua (also via require('textutil')):
  slug:       hello-ironcrew-world
  word count: 3

require is cached: same table = true
```

## How resolution works

- `require("textutil")` loads `_lib/textutil.lua` (dotted names map to
  sub-paths: `require("auth.jwt")` → `_lib/auth/jwt.lua`).
- Each flow resolves `require` from **its own** directory's `_lib`. Here
  `crew.lua` and `report.lua` live in the same directory, so they share one
  `_lib`. A sub-flow loaded from another directory would use *that* directory's
  `_lib`.
- Modules run in the **same sandbox** as the flow — they get the usual globals
  (`env`, `json_*`, `base64_*`, the crypto helpers, `http`, `regex`, …). Being a
  module grants no extra capabilities.
- Results are **cached**: requiring the same name twice runs the file once and
  returns the same value. Circular requires raise a clean error.

## Security

`require` resolves Lua-source files **only** within `_lib/`:

- Absolute paths, `..` traversal, and path separators in the name are rejected
  with a clean Lua error — no filesystem escape.
- The Lua `package` stdlib is never enabled, so `package.loadlib` and C-module
  loading are unavailable. Lua-source modules only.

See [`docs/tools.md`](../../docs/tools.md) (Shared Modules) and
[`docs/sandbox.md`](../../docs/sandbox.md) for the full reference.
