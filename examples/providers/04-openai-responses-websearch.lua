-- OpenAI Responses API — built-in web_search
-- Model: gpt-5.4-mini
-- Requires: OPENAI_API_KEY

local crew = Crew.new({
    goal = "Verify OpenAI Responses web_search tool",
    provider = "openai-responses",
    model = "gpt-5.4-mini",
    api_key = env("OPENAI_API_KEY"),
    server_tools = { "web_search" },
    web_search_context_size = "medium",
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
