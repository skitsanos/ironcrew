# MCP Git Tools Example

Demonstrates using the `mcp-server-git` MCP server via stdio transport.

## Prerequisites

- `uvx` installed (`pip install uv`)
- `mcp-server-git` available via uvx: `uvx mcp-server-git --help`

## Run

```bash
# From the ironcrew project root
OPENAI_API_KEY=sk-... ironcrew run examples/mcp/git-tools/
```

## What it does

1. At `crew:run()`, IronCrew spawns `uvx mcp-server-git` as a child process.
2. Performs the MCP handshake over stdio.
3. Discovers all tools (`git_status`, `git_log`, `git_diff`, etc.) and registers them
   in the tool registry as `mcp__git__<tool_name>`.
4. The `git_analyst` agent uses those tools to inspect the repository and return a summary.

## Security

- The child process only inherits `PATH`, `HOME`, `USER`, `LANG`, and `LC_*` variables.
  Your `OPENAI_API_KEY` and other secrets are not forwarded.
- Set `IRONCREW_MCP_ALLOWED_COMMANDS=uvx` to enforce the allowlist in production.

## Environment variables

| Variable | Description |
|---|---|
| `IRONCREW_MCP_HANDSHAKE_TIMEOUT_SECS` | Seconds to wait for handshake (default: 10) |
| `IRONCREW_MCP_ALLOWED_COMMANDS` | Comma-separated allowed binary names |
| `IRONCREW_MCP_TOOL_RESULT_MAX_BYTES` | Max bytes per tool result (default: 262144) |
