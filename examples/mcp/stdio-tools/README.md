# MCP 2026 stdio tools example

This example uses a dependency-free Python server that implements IronCrew's
required MCP protocol revision, `2026-07-28`. IronCrew starts it over stdio,
discovers it with `server/discover`, and exposes its `echo` tool as
`mcp__local_tools__echo`.

## Run

From the repository root:

```bash
OPENAI_API_KEY=sk-... ironcrew run examples/mcp/stdio-tools/
```

Set `PYTHON` when the interpreter is not available as `python3`.

The fixture server is also exercised by the deterministic MCP protocol tests,
so the example does not depend on an external package or legacy initialize
lifecycle.

## Relevant limits

| Variable | Default |
|---|---:|
| `IRONCREW_MCP_DISCOVERY_TIMEOUT_SECS` | `10` seconds |
| `IRONCREW_MCP_CALL_TIMEOUT_SECS` | `60` seconds |
| `IRONCREW_MCP_MAX_MRTR_ROUNDS` | `10` total attempts |
| `IRONCREW_MCP_MAX_REQUEST_STATE_BYTES` | `65536` bytes |
