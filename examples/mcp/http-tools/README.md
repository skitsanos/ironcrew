# MCP HTTP Tools Example

Demonstrates using a Streamable HTTP MCP `2026-07-28` server.

## Prerequisites

- A running MCP server reachable over HTTP that implements `server/discover`
  for protocol revision `2026-07-28`.
- Set `MCP_SERVER_URL` to your server's endpoint (e.g. `http://localhost:8000/mcp`).
- Optionally set `MCP_API_TOKEN` if your server requires bearer auth.

## Run

```bash
# Start your MCP server first, then:
MCP_SERVER_URL=http://localhost:8000/mcp \
MCP_API_TOKEN=yourtoken \
OPENAI_API_KEY=sk-... \
IRONCREW_MCP_ALLOW_LOCALHOST=1 \
  ironcrew run examples/mcp/http-tools/
```

`IRONCREW_MCP_ALLOW_LOCALHOST=1` is required when the server runs on localhost
(the SSRF filter blocks loopback by default for production safety).

Use the Streamable HTTP POST endpoint, normally `/mcp`, rather than a legacy
`/sse` endpoint. IronCrew does not send `initialize` or fall back to an older
lifecycle. Requests are sessionless and carry the current protocol/client
metadata independently.

If a tool schema uses `x-mcp-header`, IronCrew supports annotations reached only
through nested `properties` and excludes a tool whose annotation is placed
behind arrays, references, composition, or conditionals. Missing/`null` values
are omitted; unsafe text uses the protocol Base64 sentinel; integer values must
be JavaScript-safe. An exact `-32020` header mismatch permits one bounded
`tools/list` refresh and one retry inside the original call deadline. The
refresh may change only header annotations, not the tool's remaining definition.

## Tool naming

Tools discovered on the `myapi` server are registered as `mcp__myapi__<tool_name>`.
Update the `tools` list in `crew.lua` to match the tools your server exposes.

## Environment variables

| Variable | Description |
|---|---|
| `MCP_SERVER_URL` | Full URL to the MCP server (e.g. `http://localhost:8000/mcp`) |
| `MCP_API_TOKEN` | Optional bearer token for auth |
| `IRONCREW_MCP_ALLOW_LOCALHOST` | Set to `1` to allow localhost URLs |
| `IRONCREW_MCP_DISCOVERY_TIMEOUT_SECS` | Seconds to wait for `server/discover` and setup (default: 10) |
| `IRONCREW_MCP_MAX_MRTR_ROUNDS` | Maximum total attempts for state-only MRTR (default: 10) |
| `IRONCREW_MCP_MAX_REQUEST_STATE_BYTES` | Maximum opaque `requestState` bytes (default: 65536) |
| `IRONCREW_MCP_MAX_INBOUND_MESSAGE_BYTES` | Maximum pre-decode HTTP JSON/SSE event bytes (default: 1048576) |
| `IRONCREW_MCP_TOOL_RESULT_MAX_BYTES` | Max bytes per tool result (default: 262144) |
