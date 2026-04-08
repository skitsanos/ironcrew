-- Anthropic native — server-side web_search tool
-- Model: claude-haiku-4-5
-- Requires: ANTHROPIC_API_KEY

local crew = Crew.new({
    goal = "Verify Anthropic server-side web_search",
    provider = "anthropic",
    model = "claude-haiku-4-5-20251001",
    api_key = env("ANTHROPIC_API_KEY"),
    server_tools = { "web_search" },
    web_search_max_uses = 3,
})

crew:add_agent(Agent.new({
    name = "researcher",
    goal = "Find current information",
    system_prompt = "You are a research assistant. Use web search when needed. Be concise.",
}))

crew:add_task({
    name = "search",
    description = "What is the current latest stable version of Rust? Reply with just the version number and release date.",
    agent = "researcher",
})

crew:run()
