-- Anthropic Claude with server-side web search
--
-- Demonstrates Claude's built-in web_search tool — no custom tool needed.
-- Requires ANTHROPIC_API_KEY in .env or environment.

local crew = Crew.new({
    goal = "Research a topic using Claude's built-in web search",
    provider = "anthropic",
    model = "claude-haiku-4-5-20251001",
    api_key = env("ANTHROPIC_API_KEY"),
    server_tools = { "web_search" },
    web_search_max_uses = 3,
})

crew:add_agent(Agent.new({
    name = "researcher",
    goal = "Find and summarize current information from the web",
    capabilities = { "research", "analysis", "summarization" },
    system_prompt = "You are a research assistant. Use web search to find current information. Be concise and cite your sources.",
}))

crew:add_task({
    name = "search",
    description = "What is the latest stable version of the Rust programming language? Search the web and report the version number, release date, and one key feature.",
    agent = "researcher",
})

crew:run()
