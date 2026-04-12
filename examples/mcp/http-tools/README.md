# MCP HTTP Tools Example

Demonstrates using an HTTP Streamable MCP server.

## Prerequisites

- A running MCP server reachable over HTTP with the Streamable HTTP transport.
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

## Tool naming

Tools discovered on the `myapi` server are registered as `mcp__myapi__<tool_name>`.
Update the `tools` list in `crew.lua` to match the tools your server exposes.

## Environment variables

| Variable | Description |
|---|---|
| `MCP_SERVER_URL` | Full URL to the MCP server (e.g. `http://localhost:8000/mcp`) |
| `MCP_API_TOKEN` | Optional bearer token for auth |
| `IRONCREW_MCP_ALLOW_LOCALHOST` | Set to `1` to allow localhost URLs |
| `IRONCREW_MCP_HANDSHAKE_TIMEOUT_SECS` | Seconds to wait for handshake (default: 10) |
| `IRONCREW_MCP_TOOL_RESULT_MAX_BYTES` | Max bytes per tool result (default: 262144) |
