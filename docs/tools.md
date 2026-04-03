# Tools

Tools are functions that agents can invoke during task execution. IronCrew ships
with 9 built-in tools and supports custom tools written in Lua.

## Built-in Tools

### file_read

Read the contents of a file. Paths are relative to the project directory;
absolute paths and directory traversal (`..`) are rejected.

- **Parameters:** `path` (string, required)

```lua
-- Agent tool call (handled automatically by the LLM)
{ "path": "input/report.md" }
```

### file_read_glob

Read multiple files matching a glob pattern. Returns a JSON array of
`{path, content}` objects (or `{path, error}` on failure), sorted by path.

- **Parameters:** `pattern` (string, required)

```lua
{ "pattern": "data/**/*.json" }
```

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

### shell

Execute a shell command via `sh -c` and return stdout/stderr. **Disabled by
default** — enable with `IRONCREW_ALLOW_SHELL=1` environment variable.
See [Shell Tool Safety](#shell-tool-safety) below.

- **Parameters:** `command` (string, required)

```lua
{ "command": "wc -l data/*.csv" }
```

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

---

## Lua Globals

The following functions and namespaces are available in all Lua contexts
(crew.lua, tool scripts, agent definitions):

### Utility Functions

| Function             | Returns  | Description |
|----------------------|----------|-------------|
| `env(name)`          | string or nil | Read an environment variable |
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

Rust's regex engine exposed to Lua.

| Function | Returns | Description |
|----------|---------|-------------|
| `regex.match(pattern, text)` | bool | Test if the pattern matches |
| `regex.find(pattern, text)` | string or nil | First match |
| `regex.find_all(pattern, text)` | table | All matches |
| `regex.captures(pattern, text)` | table or nil | Capture groups (numeric and named) |
| `regex.replace(pattern, text, replacement)` | string | Replace first match |
| `regex.replace_all(pattern, text, replacement)` | string | Replace all matches |
| `regex.split(pattern, text)` | table | Split text by pattern |

### HTTP Namespace

Async HTTP client available in crew.lua scripts. All methods return a response
table.

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
