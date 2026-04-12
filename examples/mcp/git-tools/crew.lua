-- examples/mcp/git-tools/crew.lua
--
-- Demonstrates MCP stdio transport using mcp-server-git (via uvx).
-- Prerequisites: `uvx` must be installed and `uvx mcp-server-git` must be reachable.
-- Run: ironcrew run examples/mcp/git-tools/

local crew = Crew.new({
    goal     = "Inspect the current Git repository and summarise recent activity",
    provider = "openai",
    model    = "gpt-4.1-mini",

    -- Declare MCP servers. Tools become available as mcp__<label>__<tool_name>.
    mcp_servers = {
        git = {
            transport = "stdio",
            command   = "uvx",
            args      = { "mcp-server-git" },
            -- Only PATH, HOME, USER, LANG, and LC_* are forwarded to the child by default.
            -- Add extra env vars here if the server needs them:
            -- env = { MY_VAR = "value" },
        },
    },
})

crew:add_agent({
    name        = "git_analyst",
    role        = "Git repository analyst",
    goal        = "Summarise recent commits and repository status",
    backstory   = "Expert in reading Git history and surfacing key insights.",
    -- Reference the MCP tool by its IronCrew name: mcp__<server>__<tool>
    tools       = { "mcp__git__git_log", "mcp__git__git_status", "mcp__git__git_diff" },
    max_iter    = 5,
})

crew:add_task({
    description = "Check the git status and list the last 5 commits of the repository at '.'.",
    agent       = "git_analyst",
    expected_output = "A brief summary: current branch, any uncommitted changes, last 5 commits with messages and authors.",
})

local results = crew:run()
for _, r in ipairs(results) do
    print(string.format("[%s] %s", r.agent, r.output))
end
