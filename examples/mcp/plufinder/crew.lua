-- examples/mcp/plufinder/crew.lua
--
-- End-to-end MCP demo against a real public MCP server:
-- the PLU Finder at https://mcp.plufinder.com/sse.
--
-- Exercises HTTP Streamable transport, paginated tool discovery,
-- and agent-driven tool invocation.
--
-- Run:
--   export OPENAI_API_KEY=sk-...
--   ironcrew run examples/mcp/plufinder/

local crew = Crew.new({
    goal     = "Look up PLU codes for fresh produce via the PLU Finder MCP server.",
    provider = "openai",
    model    = env("OPENAI_MODEL") or "gpt-4.1-mini",

    mcp_servers = {
        plu = {
            transport = "http",
            url       = "https://mcp.plufinder.com/sse",
        },
    },
})

crew:add_agent({
    name      = "grocer",
    role      = "Produce expert",
    goal      = "Answer questions about PLU codes for fresh fruit and vegetables.",
    backstory = "Knows the PLU system cold and uses the MCP tools to look up codes.",
    tools     = {
        "mcp__plu__search_plu_codes",
        "mcp__plu__get_plu_code",
        "mcp__plu__get_plu_categories",
    },
    max_iter  = 5,
})

crew:add_task({
    name            = "lookup_plu_4300",
    agent           = "grocer",
    description     = [[
Tell me about PLU 4300. Include the commodity, any variety, size, and
storage or handling guidance. Present the answer as a short, human-readable
summary — not raw JSON.
    ]],
    expected_output = "A human-readable summary of PLU 4300 including commodity, variety, size, and storage notes.",
})

local results = crew:run()
print("")
print("=== Result ===")
for _, r in ipairs(results) do
    print(r.output)
end
