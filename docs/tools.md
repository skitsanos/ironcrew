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

**Limit:** files larger than `IRONCREW_FILE_READ_MAX_BYTES` (default 10 MB)
are rejected with a clear error. Checked via filesystem metadata before
any content is read into memory.

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
- `IRONCREW_GLOB_MAX_FILES` (default 500) — max number of files to return.
- `IRONCREW_GLOB_MAX_BYTES` (default 50 MB) — max aggregated byte total across
  all returned files.

When either limit is hit, the glob iteration stops and the result is returned
with `truncated: true`. Set either env var to `0` to disable the cap.

### file_write

Write content to a file. Creates parent directories automatically. Only
whitelisted extensions are allowed by default: `txt`, `md`, `json`, `csv`,
`yaml`, `yml`, `toml`, `xml`, `html`, `css`, `js`, `ts`, `py`, `rs`, `lua`,
`sh`.

- **Parameters:** `path` (string, required), `content` (string, required)

```lua
{ "path": "output/summary.md", "content": "# Summary\n..." }
```

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

**Output limits:** stdout and stderr are each capped at
`IRONCREW_SHELL_MAX_OUTPUT_BYTES` bytes (default 1 MB per stream). The child
process is spawned with piped stdio and each stream is read with a bounded
reader. When the cap is hit, further output is drained and discarded (so the
child can still exit cleanly) and a truncation marker is appended to the
captured output.

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
link-local, CGNAT) are blocked by default to prevent SSRF attacks. Override with
`IRONCREW_ALLOW_PRIVATE_IPS=1`.

**Response size limit:** `IRONCREW_MAX_RESPONSE_SIZE` (default 50 MB). Enforced
both via the `Content-Length` header (cheap pre-check) and during streaming
read (handles chunked responses with no header) — the request aborts as soon
as the byte budget is exceeded, so oversized responses never fully materialize
in memory.

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
| `crew:subworkflow(child_crew)` | Rust/Lua code structures nested crews at construction time | compile-time composition |

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
`IRONCREW_MCP_TOOL_RESULT_MAX_BYTES` (default `262144` / 256 KB). Oversized
results are truncated with a marker appended.

---

## Lua Globals

IronCrew exposes Lua globals in two distinct sandboxes:

| Sandbox | Where it runs | What's available |
|---------|---------------|------------------|
| **Crew sandbox** | `crew.lua`, `config.lua`, agent definitions in `agents/` | All globals below **plus the `http` namespace** and `run_flow` |
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
| `base64_decode(str)` | string   | Decode a base64 string |
| `log(level, msg)`    | nil      | Emit a log message (levels: trace, debug, info, warn, error) |
| `validate_json(json_str, schema_table)` | table | Validate JSON against a schema; returns `{valid, errors}` |
| `template(tpl_str, data_table)` | string | Render a Tera template with data (variables, loops, conditionals) |

**`env()` security:** Sensitive environment variables are blocked by default to
prevent Lua scripts from exfiltrating secrets. Blocked variables:
- `DATABASE_URL`, `IRONCREW_API_TOKEN`, `IRONCREW_PG_TABLE_PREFIX`
- Any variable ending with `_API_KEY`, `_SECRET`, `_TOKEN`, or `_PASSWORD`
- Custom names listed in `IRONCREW_ENV_BLOCKLIST` (comma-separated)

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
private/loopback IPs are blocked by default (override: `IRONCREW_ALLOW_PRIVATE_IPS=1`).
Response bodies exceeding `IRONCREW_MAX_RESPONSE_SIZE` (default 50MB) are rejected.

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
The default timeout is **60 seconds**. Override it with the
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
