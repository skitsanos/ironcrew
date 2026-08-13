-- examples/mcp/stdio-tools/crew.lua
-- Dependency-free MCP 2026-07-28 stdio example.

local crew = Crew.new({
    goal     = "Use a local MCP 2026-07-28 tool",
    provider = "openai",
    model    = "gpt-5.6-luna",

    mcp_servers = {
        local_tools = {
            transport = "stdio",
            execution_identity = "ironcrew-mcp-2026-fixture-v1",
            command   = env("PYTHON") or "python3",
            args      = { "examples/mcp/stdio-tools/server.py" },
        },
    },
})

crew:add_agent({
    name      = "echo_agent",
    role      = "MCP tool user",
    goal      = "Call the echo tool and report its response",
    backstory = "Careful protocol integration tester.",
    tools     = { "mcp__local_tools__echo" },
    max_iter  = 3,
})

crew:add_task({
    name            = "echo_message",
    description     = "Use the echo tool with the text 'MCP 2026 is ready'.",
    agent           = "echo_agent",
    expected_output = "The exact text returned by the echo tool.",
})

local results = crew:run()
for _, result in ipairs(results) do
    print(string.format("[%s] %s", result.agent, result.output))
end
