# PLU Finder MCP Example

End-to-end demo that connects IronCrew to a real public MCP server
(the PLU Finder at `https://mcp.plufinder.com/sse`) and lets an agent
look up produce PLU codes through its tools.

## What this proves

- **HTTP Streamable transport** works against a live remote MCP server.
- **Paginated tool discovery** — the server exposes 10 tools.
- **Tool invocation** via the `mcp__<server>__<tool>` naming scheme.
- **Graceful shutdown** of the MCP client after the run.

## Prerequisites

```
export OPENAI_API_KEY=sk-...
```

## Run

```
ironcrew run examples/mcp/plufinder/
```

Expected behaviour: the agent calls `mcp__plu__search_plu_codes` to find
matching entries for organic Hass avocados, then calls
`mcp__plu__get_plu_code` to retrieve full details, and reports the
result.

## Tools exposed by the server

- `search_plu_codes` — search by name, category, or variety
- `get_plu_code` — detailed lookup by PLU number
- `get_plu_categories` — list all categories
- `get_commodities_by_category` — list produce within a category
- `report_plu_inconsistency` — community-driven retail intelligence
- `search_ggn_producers`, `get_ggn_producer_details`, `producers_around_me`,
  `get_ggn_producers_by_crop`, `contribute_ggn_producer` — GLOBALG.A.P.
  producer lookups (not used in this example)

## How to verify without an LLM call

For a pure protocol-level smoke test (no OpenAI key needed):

```
cargo test --features mcp --test mcp_http_live_test -- --ignored --nocapture
```

This runs two integration tests that connect to the PLU Finder server
directly, list its tools, and invoke `get_plu_categories`.
