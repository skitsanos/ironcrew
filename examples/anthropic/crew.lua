-- Anthropic Claude via native Messages API
--
-- Uses Anthropic's native provider with server-side tools and extended thinking.
-- Requires ANTHROPIC_API_KEY in .env or environment.

local crew = Crew.new({
    goal = "Demonstrate Anthropic Claude working through the native Messages API",
    provider = "anthropic",
    model = "claude-haiku-4-5-20251001",
    api_key = env("ANTHROPIC_API_KEY"),
})

crew:add_agent(Agent.new({
    name = "claude",
    goal = "Answer questions clearly and concisely",
    capabilities = { "reasoning", "analysis", "writing" },
    system_prompt = "You are a helpful assistant. Be concise.",
}))

crew:add_task({
    name = "identify",
    description = "What model are you? Reply with your exact model name and a one-sentence description of your capabilities.",
    agent = "claude",
})

crew:add_task({
    name = "reason",
    description = "A farmer has 17 sheep. All but 9 run away. How many sheep does the farmer have left? Explain your reasoning step by step.",
    agent = "claude",
})

crew:add_task({
    name = "summarize",
    description = "Based on the previous results, write a one-paragraph summary confirming that Anthropic Claude works correctly through IronCrew's native Anthropic provider.",
    agent = "claude",
    depends_on = { "identify", "reason" },
})

crew:run()
