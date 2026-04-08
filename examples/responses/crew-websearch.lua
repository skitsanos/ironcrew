-- OpenAI Responses API — server-side web search
--
-- Uses the built-in web_search tool. No custom tool or HTTP calls needed.

local crew = Crew.new({
    goal = "Research current information using built-in web search",
    provider = "openai-responses",
    model = "gpt-5.4",
    api_key = env("OPENAI_API_KEY"),
    server_tools = { "web_search" },
    web_search_context_size = "medium",
})

crew:add_agent(Agent.new({
    name = "researcher",
    goal = "Find and summarize current information",
    capabilities = { "research", "summarization" },
    system_prompt = "You are a research assistant. Use web search to find current facts. Be concise.",
}))

crew:add_task({
    name = "search",
    description = "What is the latest stable version of Rust? Search the web and report the version number and one key feature.",
    agent = "researcher",
})

crew:run()
