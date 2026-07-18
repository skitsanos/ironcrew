# Tools

Tools are functions that agents can invoke during task execution. IronCrew ships
with 8 built-in tools by default (9 when the opt-in `shell` tool is enabled via
`IRONCREW_ALLOW_SHELL=1`) and supports custom tools written in Lua. Additional
tools can be contributed by MCP servers configured on the crew (see
[MCP Tools](#mcp-tools) below).

## Built-in Tools

### file_read

Read the contents of a file. Paths are relative to the project directory;
absolute paths and directory traversal (`..`) are rejected.

- **Parameters:** `path` (string, required)

```lua
-- Agent tool call (handled automatically by the LLM)
{ "path": "input/report.md" }
```

**Limit:** files larger than `IRONCREW_FILE_READ_MAX_BYTES` (default 10 MiB,
hard ceiling 256 MiB)
are rejected. Reads are streamed through a capability directory and stop if a
file grows past the limit. Absolute paths, traversal, symlink escapes, and
non-regular files are rejected by descriptor-relative capability access,
avoiding a path-check/path-open race.

### file_read_glob

Read multiple files matching a glob pattern. Returns a JSON **object** with
the files array plus observability metadata.

- **Parameters:** `pattern` (string, required)

```lua
{ "pattern": "data/**/*.json" }
```

**Output shape** (v2.6.0+):

```json
{
  "files": [
    { "path": "data/a.json", "content": "..." },
    { "path": "data/b.json", "content": "..." }
  ],
  "file_count": 2,
  "total_bytes": 4096,
  "truncated": false
}
```

Individual files that fail to read yield `{path, error}` entries in the
`files` array instead of `{path, content}`.

**Limits:**
- The glob pattern itself is capped at 8192 bytes.
- `IRONCREW_GLOB_MAX_FILES` (default 500, hard 10 000) — max number of files to return.
- `IRONCREW_GLOB_MAX_BYTES` (default 50 MiB, hard 256 MiB) — max aggregated byte total across
  all returned files.
- `IRONCREW_GLOB_MAX_ENTRIES` (default 10 000, hard 100 000) — max regular filesystem
  entries scanned before matching.
- `IRONCREW_FILE_READ_MAX_BYTES` (default 10 MiB, hard 256 MiB) — max bytes per file.
- `IRONCREW_GLOB_MAX_OUTPUT_BYTES` (default 64 MiB, hard 256 MiB) — max final
  serialized JSON bytes, including escaping and metadata. Exceeding this
  final cap fails the tool call rather than returning partial JSON.

When either limit is hit, the glob iteration stops and the result is returned
with `truncated: true`. Zero or invalid resource-limit values use the bounded
defaults; they do not disable the caps. Directory traversal, symlink entries,
and non-regular files are not included in a glob scan.

### file_write

Write content to a file. Creates parent directories automatically. Only
whitelisted extensions are allowed by default: `txt`, `md`, `json`, `csv`,
`yaml`, `yml`, `toml`, `xml`, `html`, `css`, `js`, `ts`, `py`, `rs`, `lua`,
`sh`.

- **Parameters:** `path` (string, required), `content` (string, required)

```lua
{ "path": "output/summary.md", "content": "# Summary\n..." }
```

Writes are capped by `IRONCREW_FILE_WRITE_MAX_BYTES` (default 10 MiB, hard
ceiling 256 MiB), opened
relative to the project capability directory, and committed by an atomic
temporary-file rename. Absolute paths, traversal, and symlink targets are
rejected; intermediate path resolution remains confined to the project
capability directory.

### web_scrape

Fetch a URL and extract its visible text content. HTML is parsed and only body
text is returned. Output is truncated to 10 000 characters.

- **Parameters:** `url` (string, required)

```lua
{ "url": "https://example.com/article" }
```

**Limit:** raw HTML is streamed with a byte cap of
`IRONCREW_WEB_SCRAPE_MAX_BYTES` (default 2 MB) **before** DOM parsing, to
avoid the quadratic worst case of feeding very large HTML to the parser.
Responses exceeding the cap are rejected with an error.

### shell

Execute a shell command via `sh -c` and return stdout/stderr. **Disabled by
default** — enable with `IRONCREW_ALLOW_SHELL=1` environment variable.
See [Shell Tool Safety](#shell-tool-safety) below.

- **Parameters:** `command` (string, required)

```lua
{ "command": "wc -l data/*.csv" }
```

**Execution limits:** the deadline defaults to
`IRONCREW_SHELL_TIMEOUT_SECS=60`; a call-level `timeout_secs` can override it.
Both values must be in `1..=3600`. stdout and stderr are each capped at
`IRONCREW_SHELL_MAX_OUTPUT_BYTES` bytes (default 1 MiB, hard ceiling 16 MiB per stream). The child
process is spawned with piped stdio and each stream is read with a bounded
reader. When the cap is hit, further output is drained and discarded (so the
child can still exit cleanly) and a truncation marker is appended to the
captured output. Timeout, cancellation, and normal completion terminate the
whole child process group, including background descendants.

### http_request

Make an HTTP request with full control over method, headers, body, and
authentication. Supports bearer, basic, and API-key auth.

- **Parameters:**
  - `url` (string, required)
  - `method` (string, required) -- `GET`, `POST`, `PUT`, `DELETE`, `PATCH`
  - `headers` (object) -- key-value pairs
  - `body` (string) -- request body; auto-detects JSON
  - `timeout_secs` (number) -- default 30
  - `auth_type` (string) -- `bearer`, `basic`, or `api_key`
  - `auth_token` (string) -- token, password, or key value
  - `auth_username` (string) -- for basic auth
  - `auth_header` (string) -- header name for api_key auth (default `X-API-Key`)

```lua
{ "url": "https://api.example.com/data", "method": "POST", "body": "{\"q\": \"test\"}", "auth_type": "bearer", "auth_token": "sk-..." }
```

**Security:** Requests to private/internal IP addresses (loopback, RFC1918,
link-local, CGNAT, metadata and reserved ranges) are blocked by default.
Validation covers initial DNS answers, the addresses used for the actual
connection, and redirect targets. Protected clients ignore environment proxy
variables because a proxy could otherwise bypass address validation. Override
with `IRONCREW_ALLOW_PRIVATE_IPS=1` only for trusted workloads.

**Request budgets:** The URL is limited to 8192 bytes. Explicit headers must
be string-valued and, together with generated authentication headers, fit
`IRONCREW_HTTP_MAX_REQUEST_HEADER_BYTES` (64 KiB by default, 1 MiB hard
ceiling); no request may exceed 128 headers. String bodies are capped by
`IRONCREW_HTTP_MAX_REQUEST_BODY_BYTES` (8 MiB by default, 64 MiB hard
ceiling). Invalid header names or values fail before any request is sent.

**Response budgets:** `IRONCREW_HTTP_MAX_RESPONSE_BYTES` is the primary body
cap (default 8 MiB). `IRONCREW_MAX_RESPONSE_SIZE` remains a deprecated fallback
only when the primary variable is absent. `IRONCREW_HTTP_MAX_HEADER_BYTES`
defaults to 64 KiB, `IRONCREW_HTTP_MAX_JSON_BYTES` limits the additional parsed
JSON tree to 2 MiB, and `IRONCREW_HTTP_MAX_OUTPUT_BYTES` limits the final
serialized result to 16 MiB. Content length and chunked streaming are both
enforced. Each network-body setting is also constrained by a 256 MiB process
hard cap; invalid values use the safe default. A request-specific timeout must
be finite and in `(0, 300]` seconds.

### hash

Compute a hash of the input text. Supported algorithms: `md5`, `sha256`,
`sha512`.

- **Parameters:** `text` (string, required), `algorithm` (string, required)

```lua
{ "text": "hello world", "algorithm": "sha256" }
```

### template_render

Render a Tera template string with JSON data. Uses the
[Tera](https://keats.github.io/tera/) template engine (Jinja2-like syntax).

- **Parameters:** `template` (string, required), `data` (object, required)

```lua
{ "template": "Hello {{ name }}! You have {{ count }} items.", "data": { "name": "Alice", "count": 5 } }
```

### validate_schema

Validate a JSON string against a JSON Schema (Draft 7). Returns
`{valid, errors}` where `errors` is an array of `{path, message}` objects.

- **Parameters:** `data` (string, required), `schema` (object, required)

```lua
{ "data": "{\"name\": \"Alice\"}", "schema": { "type": "object", "properties": { "name": { "type": "string" } } } }
```

Schemas are capped by `IRONCREW_JSON_SCHEMA_MAX_BYTES` (default 256 KiB;
range 1 KiB–4 MiB). External document retrieval is disabled: `$ref` may use a
local `#` fragment, but HTTP, file, and other non-fragment references are
rejected before compilation.

### ask_human

Let an **agent** pause the run and ask the human operator a question
mid-reasoning — the agent-facing counterpart to the flow-level
[`crew:ask_human()`](crews.md#human-in-the-loop-ask_human) primitive. The
agent's turn suspends on the same per-run transport: SSE emits
`human_input_requested`, the answer arrives via the
[`questions`/`answer` endpoints](rest-api.md#mid-run-questions-crewask_human)
(server) or a terminal prompt (CLI).

- **Parameters:** `question` (string, required), `choices` (array of
  strings, optional), `timeout_s` (integer, optional, default 600)

```lua
{ "question": "Two conflicting revenue figures found — which source should I trust?", "choices": ["ERP export", "Finance spreadsheet"] }
```

The prompt shown to the human is prefixed with the asking agent's name
(`[analyst] …`). On timeout the agent receives a soft `[no answer]` result
telling it to proceed on its best judgment — not an error, so the model
isn't tempted into a retry loop that parks the run again.

**Timeouts:** the tool extends its own dispatch deadline past
`IRONCREW_TOOL_TIMEOUT`, and **human-wait time does not count against the
task's `timeout_secs`** — while a question is pending, the task clock is
paused (the run-lifetime cap still bounds the whole run).

Like every built-in, agents opt in explicitly:

```lua
crew:add_agent({
    name = "analyst",
    goal = "analyze quarterly data",
    tools = { "ask_human" },
})
```

---

## Custom Lua Tools

Place a `.lua` file in the `tools/` directory of your project. Each file must
return a table with `name`, `description`, `parameters`, and an `execute`
function.

```lua
-- tools/word_count.lua
return {
    name = "word_count",
    description = "Count words in a text string",
    parameters = {
        text = { type = "string", description = "Text to count", required = true },
    },
    execute = function(args)
        local count = 0
        for _ in args.text:gmatch("%S+") do
            count = count + 1
        end
        return tostring(count)
    end,
}
```

The `parameters` table uses a simplified format: each key is a parameter name
with `type`, `description`, and optional `required = true`. IronCrew converts
this to JSON Schema before sending to the LLM.

Custom tools run in a restricted sandbox (no `os`, `io`, `require`, `loadfile`,
`dofile`). A `fs` namespace scoped to the project directory is available
(`fs.read(path)`, `fs.write(path, content)`).

**Tools cannot call `http.*` directly.** The `http` global is not registered in
the tool sandbox. You have three options when a custom tool needs remote data:

1. **Delegate to a sub-flow via `run_flow`** (recommended for composing logic) —
   custom tools can call [`run_flow(path, input)`](#run_flow-sub-crew-delegation)
   to invoke a sub-crew Lua script that *does* have `http` access. The sub-flow
   runs in its own sandboxed VM and its result is JSON-bridged back to the tool.
2. **Fetch the data in `crew.lua`** (where `http` is available) and pass it via
   context, memory, or task results.
3. **Let the agent call the built-in `http_request` tool** directly — no custom
   Lua tool wrapper required.

---

## Delegation primitives

IronCrew gives you three primitives for running specialist work from
a top-level agent or crew:

| Primitive | When | Flavor |
|---|---|---|
| `agent__<name>` (tool entry) | one agent delegates a single question to another agent defined on the same crew | chat-driven, ephemeral |
| `run_flow("<path>")` (Lua global) | top-level script or tool calls a sub-crew's full pipeline | programmatic, depth-bounded |
| `crew:subworkflow("<path>", options?)` | top-level crew invokes a sub-flow and can wrap its result with `output_key` | programmatic, depth-bounded |

All three share the `IRONCREW_MAX_FLOW_DEPTH` cap (default `5`) so deeply-nested
delegation doesn't run away.

See [docs/agents.md](agents.md#agent-as-tool) for agent-as-tool usage and examples.

---

## `run_flow` (sub-crew delegation)

`run_flow(path, input)` is a sandbox-level primitive that invokes another
IronCrew Lua script (a "sub-flow") and returns its result into the caller's VM.
It lets `crew.lua` and custom tools compose crews without spawning a new
process.

### Signature

```
run_flow(path[, input]) -> value
```

| Arg     | Type                          | Description |
|---------|-------------------------------|-------------|
| `path`  | string                        | Path to the sub-flow Lua script, relative to the caller's project directory. Must stay inside the project root. |
| `input` | Lua table / primitive (optional) | Passed to the sub-flow as the global variable `input`. |

The return value is whatever the sub-flow's final Lua expression yields
(typically a `return { ... }` at the end of the script), marshalled across the
VM boundary via JSON.

### Semantics

- **Synchronous from Lua's perspective, async under the hood.** Callers just
  receive the return value; IronCrew awaits the sub-flow on the Tokio runtime.
- **Fresh Lua VM per sub-flow.** Each invocation builds a new sandboxed VM
  (same sandbox rules as the parent crew: no `os`, `io`, `require`, `loadfile`,
  `dofile`; `http`, `fs`, `template`, `regex`, `json_parse`, etc. are
  available). Sub-flows do not inherit memory, tasks, or agents from the caller.
- **Agents auto-load.** The sub-flow's directory is scanned for `agents/*.lua`
  just like a top-level crew.
- **Available in both sandboxes.** `run_flow` is registered on the top-level
  crew Lua VM *and* on the per-tool Lua VM used by custom `tools/*.lua` files
  (including tools invoked during a `crew:conversation()` tool-call loop). This
  is the key feature: **custom tools can delegate to sub-crews in-process**,
  bypassing the tool-sandbox restrictions on `http` and friends.

### Path validation

- `path` must be relative.
- Absolute paths, paths containing `..`, and symlink traversal that escapes the
  caller's project root are rejected with a validation error.
- The resolved path must exist and must be a file.

### Recursion cap

Nested `run_flow` calls are counted against `IRONCREW_MAX_FLOW_DEPTH` (default
`5`). Each nested invocation increments the depth; exceeding the limit raises a
validation error (`run_flow depth exceeded: already at N (limit N)`).

### Relationship to `crew:subworkflow(...)`

`run_flow` and `crew:subworkflow` share the same underlying implementation.
Differences:

| Feature                    | `run_flow(path, input)`                           | `crew:subworkflow(...)`                          |
|----------------------------|---------------------------------------------------|--------------------------------------------------|
| Where it's callable        | Any sandbox — crew VM **and** custom tool VMs     | Top-level crew VM only                           |
| `output_key` wrapping      | Not supported; returns the raw sub-flow value     | Optional — wraps result as `{ [key] = <value> }` |
| Target                     | A Lua script file                                 | A Lua script file                                |

Use `run_flow` from custom tools or when you want the unwrapped value;
`crew:subworkflow` remains useful when you want the result pre-wrapped into a
named key for merging into memory/results.

### Example: delegating a custom tool to a sub-crew

```lua
-- tools/delegator.lua
return {
    name = "delegator",
    description = "Delegates work to sub-crew",
    parameters = {
        x = { type = "integer", description = "Value to forward", required = true },
    },
    execute = function(args)
        return run_flow("subs/math/math.lua", { x = args.x })
    end,
}
```

The sub-flow `subs/math/math.lua` runs in its own sandbox with its own
`agents/` folder and can use `http`, `fs`, and crew/agent constructors
normally.

All agent-facing filesystem reads deny flow-local secrets and state, including
`.env`/`.env.*`, `.ironcrew`, common credential directories/files, and
private-key extensions. Custom tools read ordinary project data from the flow
root but write only beneath `IRONCREW_FILE_WRITE_ROOT`; source and executable
extensions are never writable.

---

## MCP Tools

When a crew configures `mcp_servers`, each tool exported by a connected MCP
server is registered in IronCrew's tool registry under the canonical name
`mcp__<server>__<tool>` (see `src/mcp/config.rs`). Agents list them in their
`tools = { ... }` field like any other tool:

```lua
crew:add_agent(Agent.new({
    name = "dev",
    goal = "Inspect repo state",
    tools = { "mcp__git__git_status", "mcp__git__git_log" },
}))
```

See [Crews](crews.md) for `mcp_servers` configuration.

**Result size cap.** MCP tool results are size-capped at
`IRONCREW_MCP_TOOL_RESULT_MAX_BYTES` (default `262144` / 256 KiB; hard ceiling
16 MiB). Oversized text is UTF-8-safely truncated with a marker. MCP also caps
discovery at 128 tools/32 pages, definitions at 128 KiB, arguments at 256 KiB,
and result content at 256 blocks by default. Separate handshake, discovery,
call, and shutdown deadlines prevent an unresponsive server from retaining a
run indefinitely. HTTP MCP transport does not follow redirects; loopback is
available only through the narrow `IRONCREW_MCP_ALLOW_LOCALHOST` opt-in.
rmcp currently materializes stdio/HTTP transport frames before IronCrew can
apply its post-decode result caps, so production must permit only trusted stdio
commands and exact hosts (`IRONCREW_MCP_ALLOWED_HTTP_HOSTS`), or keep both MCP
transports disabled. See
the complete environment table in
[CLI](cli.md#environment-variables).

---

## Lua Globals

IronCrew exposes Lua globals in two distinct sandboxes:

| Sandbox | Where it runs | What's available |
|---------|---------------|------------------|
| **Crew sandbox** | `crew.lua`, `config.lua`, agent definitions in `agents/` | All globals below **plus the `http` namespace**, `run_flow`, and `require` (shared modules from `_lib/`) |
| **Tool sandbox** | The `execute` function inside files in `tools/` | All globals below **plus the `fs` namespace** for sandboxed filesystem access and `run_flow` — but **no `http`** |

> **Important constraint:** Custom Lua tools cannot call `http.*` directly. The
> `http` global is only registered in the crew sandbox. If a tool needs remote
> data, either delegate to a sub-flow via
> [`run_flow`](#run_flow-sub-crew-delegation) (which *does* get `http` in its
> own sandbox), fetch the data in `crew.lua` and pass it through
> memory/context, or let the agent invoke the built-in `http_request` tool.

The `run_flow(path, input?)` primitive (see [`run_flow`](#run_flow-sub-crew-delegation))
is available in **both sandboxes** — it's the recommended way to compose crews
from inside custom tools.

### Utility Functions

| Function             | Returns  | Description |
|----------------------|----------|-------------|
| `env(name)`          | string or nil | Read an environment variable (see security note below) |
| `uuid4()`            | string   | Generate a random UUID v4 |
| `now_rfc3339()`      | string   | Current UTC time in RFC 3339 format |
| `now_unix_ms()`      | number   | Current UTC time as Unix milliseconds |
| `json_parse(str)`    | value    | Parse a JSON string into a Lua value |
| `json_stringify(val)` | string  | Serialize a Lua value to JSON |
| `base64_encode(str)` | string   | Base64-encode a string |
| `base64_decode(str)` | string   | Decode a base64 string (UTF-8 text) |
| `base64_decode_bytes(str)` | string | Decode a base64 string to raw bytes (no UTF-8 validation) |
| `pbkdf2_sha256(passphrase, salt, iterations, key_len)` | string | Derive a key via PBKDF2-HMAC-SHA256; returns raw key bytes |
| `aes_256_gcm_decrypt(key, iv, ciphertext_with_tag)` | string | AES-256-GCM decrypt (16-byte tag appended to ciphertext); raises on auth failure |
| `aes_gcm_decrypt_pbkdf2(blob_b64, passphrase, iterations?)` | string | Decrypt `base64(salt[16]‖iv[12]‖ct‖tag[16])`; PBKDF2-HMAC-SHA256 key derivation, `iterations` defaults to `600000` |
| `log(level, msg)`    | nil      | Emit a log message (levels: trace, debug, info, warn, error) |
| `validate_json(json_str, schema_table)` | table | Validate JSON against a schema; returns `{valid, errors}` |
| `template(tpl_str, data_table)` | string | Render a Tera template with data (variables, loops, conditionals) |

**`env()` security:** `env()` is fail-closed — Lua scripts can read **only** the
environment variables whose exact names are listed in `IRONCREW_ENV_ALLOWLIST`
(comma-separated, case-insensitive). Every other name returns `nil` and logs a
warning, so secrets are unreadable unless explicitly opted in. See
[docs/sandbox.md](sandbox.md).

### Template Rendering

The `template()` global renders [Tera](https://keats.github.io/tera/) templates directly in Lua — no LLM call needed:

```lua
-- Variables
local msg = template("Hello {{ name }}!", {name = "Alice"})

-- Loops
local list = template("{% for item in items %}- {{ item }}\n{% endfor %}", {
    items = {"Rust", "Python", "Go"}
})

-- Render structured LLM output into a document
local report = template([[
# {{ title }}
{% for f in findings %}
- {{ f.name }}: {{ f.description }}
{% endfor %}
]], json_parse(results.extract.output))
```

> **Tera 2.0 behavior (since IronCrew's tera upgrade):** template rendering is
> now **strict about undefined variables** — referencing a variable that is not
> in the data table (e.g. `{{ missing }}`) raises a render error instead of
> producing an empty string. Make sure every variable a template references is
> present in the data table, or guard it with `{% if missing %}…{% endif %}` /
> optional chaining (`{{ a?.b }}`). Other tera 2.0 changes to be aware of:
> array elements use bracket indexing (`items[0]`, not `items.0`), and a few
> filters were renamed (`escape` → `escape_html`, `as_str` → `str`,
> `divisibleby` → `divisible_by`, `linebreaksbr` → `newlines_to_br`). See the
> [tera migration guide](https://github.com/Keats/tera/blob/master/MIGRATION.md).

### Crypto (decrypt secrets at runtime)

Flows can decrypt secrets (e.g. API credentials) that are stored **encrypted at
rest**, so the caller never has to pass plaintext keys through the run input.
The supported on-disk format mirrors the Web Crypto API:

- Cipher: **AES-256-GCM**, 12-byte IV, 16-byte (128-bit) auth tag appended to
  the ciphertext.
- Key derivation: **PBKDF2-HMAC-SHA256**, 16-byte salt, 32-byte derived key,
  600 000 iterations by default.
- Serialized blob = `base64( salt[16] ‖ iv[12] ‖ ciphertext ‖ tag[16] )`.

All byte data (keys, IVs, salts, ciphertext, plaintext) is passed as Lua strings
holding raw bytes — exactly like `base64_decode_bytes` returns.

**Convenience helper** (recommended — matches the layout above):

```lua
-- Read an encrypted credential from your store (e.g. via http/Arango),
-- then decrypt it in-process using a passphrase from the allow-listed env.
local passphrase = env("CREDENTIAL_PASSPHRASE")  -- allow-list this var
local plaintext  = aes_gcm_decrypt_pbkdf2(record.blob_b64, passphrase)
-- iterations defaults to 600000; pass a 3rd arg to override.
local creds = json_parse(plaintext)
```

**Low-level primitives** (if you need to parse a different byte layout):

```lua
local raw  = base64_decode_bytes(blob_b64)
local salt = raw:sub(1, 16)
local iv   = raw:sub(17, 28)
local ct   = raw:sub(29)                          -- ciphertext + 16-byte tag
local key  = pbkdf2_sha256(passphrase, salt, 600000, 32)
local plaintext = aes_256_gcm_decrypt(key, iv, ct)
```

**Security:**

- A wrong passphrase or tampered ciphertext raises a clean Lua error (no
  partial plaintext, no panic). Tag verification is constant-time.
- Plaintext and passphrases are never logged.
- The passphrase is supplied by the flow via `env("…")`; you must add that
  variable to `IRONCREW_ENV_ALLOWLIST`, since `env()` returns `nil` for any
  name not on the allowlist — see the `env()` security note above.

> Encryption is intentionally **not** exposed — only decryption is required for
> the read-secrets-at-runtime use case.

### Shared Modules (`require`)

Flows and sub-flows can share Lua code with `require`, resolved **only** from a
`_lib/` directory next to the flow. Instead of copy-pasting the
credential-decrypt sequence above (or Arango helpers, env setup, …) into every
flow, put it in one module:

```lua
-- _lib/credentials.lua
local M = {}

-- Fetch an encrypted credential blob from a store, then decrypt it in-process.
function M.resolve(credential_id)
    local resp = http.get("https://store.internal/credentials/" .. credential_id, {
        headers = { Authorization = "Bearer " .. env("STORE_TOKEN") },
    })
    assert(resp.ok, "credential fetch failed: " .. tostring(resp.status))

    local record = json_parse(resp.body)              -- { blob_b64 = "..." }
    local plaintext = aes_gcm_decrypt_pbkdf2(record.blob_b64, env("ENCRYPTION_KEY"))
    return json_parse(plaintext).token
end

return M
```

```lua
-- any flow or sub-flow
local credentials = require("credentials")
local provider_api_key = credentials.resolve(input.credential_id)
```

**Resolution:**

- `require("credentials")` loads `_lib/credentials.lua`. Dotted names map to
  sub-paths: `require("auth.jwt")` → `_lib/auth/jwt.lua`.
- Each flow resolves `require` from **its own** directory's `_lib`. A top-level
  flow uses `<project_dir>/_lib`; a sub-flow invoked via `run_flow` uses the
  sub-flow directory's `_lib`.
- A module is plain Lua that returns a value (typically a table). It runs in the
  **same sandbox** as the flow — it gets the globals on this page (`env`,
  `json_*`, `base64_*`, the crypto helpers, `http`, `regex`, …) and no extra
  capabilities.
- Results are **cached**: requiring the same name twice runs the file once and
  returns the same value. Circular requires raise a clean error rather than
  hang.

**Security:**

- Resolution is restricted to `_lib/`. Absolute paths, `..` traversal, and path
  separators in the name raise a clean Lua error — no filesystem escape.
- The Lua `package` stdlib is never enabled, so `package.loadlib` and C-module
  loading are unavailable. Lua-source modules only.
- The tool sandbox (custom tools in `tools/`) does **not** get `require`.

> Runnable example: [`examples/shared-modules`](../examples/shared-modules) — a
> shared module used by both a top-level flow and a sub-flow, verifiable offline
> with `ironcrew run examples/shared-modules`.

### Regex Namespace

Rust's regex engine exposed to Lua. Compiled patterns are cached in a
thread-local cache (up to 256 entries), so repeated calls with the same pattern
avoid recompilation.

| Function | Returns | Description |
|----------|---------|-------------|
| `regex.match(pattern, text)` | bool | Test if the pattern matches |
| `regex.find(pattern, text)` | string or nil | First match |
| `regex.find_all(pattern, text)` | table | All matches |
| `regex.captures(pattern, text)` | table or nil | Capture groups (numeric and named) |
| `regex.replace(pattern, text, replacement)` | string | Replace first match |
| `regex.replace_all(pattern, text, replacement)` | string | Replace all matches |
| `regex.split(pattern, text)` | table | Split text by pattern |

### HTTP Namespace (crew sandbox only)

Async HTTP client available in `crew.lua`, `config.lua`, and agent definitions.
**Not available in custom tool execute functions.** All methods return a
response table. Uses a shared connection pool (singleton `reqwest::Client`).

**Security:** All `http.*` calls enforce SSRF protection — requests to
private/internal IPs are blocked at DNS resolution, connection, and redirect
time (override only for trusted workloads with `IRONCREW_ALLOW_PRIVATE_IPS=1`).
Environment proxy variables are ignored. Response bodies use
`IRONCREW_HTTP_MAX_RESPONSE_BYTES` (default 8 MiB), with the deprecated
`IRONCREW_MAX_RESPONSE_SIZE` consulted only as a fallback. Headers default to a
64 KiB cap and automatic JSON conversion is skipped above 2 MiB.

```lua
local resp = http.get("https://api.example.com/data", {
    headers = { Authorization = "Bearer " .. env("API_TOKEN") },
    timeout = 10,  -- seconds
})

if resp.ok then
    print(resp.status)          -- 200
    print(resp.body)            -- raw response body
    local data = resp.json      -- auto-parsed JSON (nil if not JSON)
    print(resp.headers["content-type"])
end
```

**Methods:**

| Method | Signature |
|--------|-----------|
| `http.get(url, options?)` | GET request |
| `http.post(url, options?)` | POST with optional body |
| `http.put(url, options?)` | PUT with optional body |
| `http.delete(url, options?)` | DELETE request |
| `http.request(method, url, options?)` | Any method (GET, POST, PUT, DELETE, PATCH, HEAD) |

**Options table:**

| Field     | Type   | Description |
|-----------|--------|-------------|
| `headers` | table  | Key-value request headers |
| `body`    | string | Raw request body (auto-detects JSON) |
| `json`    | table  | Lua table serialized as JSON body |
| `timeout` | number | Timeout in seconds (default 30) |

`timeout` must be finite, greater than zero, and no more than 300 seconds.

**Response table:**

| Field     | Type   | Description |
|-----------|--------|-------------|
| `status`  | number | HTTP status code |
| `headers` | table  | Response headers |
| `body`    | string | Raw response body |
| `json`    | value  | Auto-parsed JSON body (nil if not valid JSON) |
| `ok`      | bool   | `true` if status is 2xx |

---

## Tool Execution Timeout

Every tool invocation is wrapped in a timeout to prevent runaway executions.
The default timeout is **60 seconds**. Missing, invalid, and zero values use
that default; values above the one-hour hard ceiling clamp to 3600 seconds.
Override it with the
`IRONCREW_TOOL_TIMEOUT` environment variable (value in seconds):

```bash
# Allow tools up to 120 seconds
IRONCREW_TOOL_TIMEOUT=120 ironcrew run .

# Or in .env
IRONCREW_TOOL_TIMEOUT=120
```

If a tool exceeds the timeout, the tool call returns an error message
(`Tool timed out after Ns`) and the LLM continues with that error context.

---

## Shell Tool Safety

The `shell` tool is **not registered by default**. This is a deliberate safety
decision — unrestricted shell access allows an LLM to execute arbitrary commands.

Enable it by setting the `IRONCREW_ALLOW_SHELL` environment variable:

```bash
# Via env var
IRONCREW_ALLOW_SHELL=1 ironcrew run .

# Or in .env
IRONCREW_ALLOW_SHELL=true

# In Docker
docker run -e IRONCREW_ALLOW_SHELL=1 ...
```

When enabled, a warning is logged: `Shell tool enabled via IRONCREW_ALLOW_SHELL`.
When not set, agents listing `shell` in their tools get an unknown-tool validation warning.
