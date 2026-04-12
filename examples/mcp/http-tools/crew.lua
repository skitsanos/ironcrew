-- examples/mcp/http-tools/crew.lua
--
-- Demonstrates MCP HTTP Streamable transport.
-- Replace the URL with your actual MCP server endpoint.
-- Run: ironcrew run examples/mcp/http-tools/

local crew = Crew.new({
    goal     = "Use an HTTP MCP server to perform a task",
    provider = "openai",
    model    = "gpt-4.1-mini",

    mcp_servers = {
        myapi = {
            transport = "http",
            url       = env("MCP_SERVER_URL") or "https://mcp.example.com/mcp",
            -- Optional: extra HTTP headers (authorization values are redacted in logs)
            headers   = {
                authorization = env("MCP_API_TOKEN") and ("Bearer " .. env("MCP_API_TOKEN")) or nil,
            },
        },
    },
})

crew:add_agent({
    name      = "api_agent",
    role      = "API tool user",
    goal      = "Invoke MCP server tools and report results",
    backstory = "Expert at using external APIs via MCP.",
    -- Use mcp__<label>__<tool> naming.
    -- Replace 'increment' with an actual tool name from your MCP server.
    tools     = { "mcp__myapi__increment" },
    max_iter  = 3,
})

crew:add_task({
    description     = "Call the increment tool on the MCP server and report what it returned.",
    agent           = "api_agent",
    expected_output = "The result of calling the increment tool.",
})

local results = crew:run()
for _, r in ipairs(results) do
    print(string.format("[%s] %s", r.agent, r.output))
end
