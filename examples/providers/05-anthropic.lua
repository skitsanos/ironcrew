-- Anthropic native Messages API — baseline
-- Model: claude-haiku-4-5 (cheapest current Haiku)
-- Requires: ANTHROPIC_API_KEY

local crew = Crew.new({
    goal = "Verify Anthropic native provider",
    provider = "anthropic",
    model = "claude-haiku-4-5-20251001",
    api_key = env("ANTHROPIC_API_KEY"),
})

crew:add_agent(Agent.new({
    name = "assistant",
    goal = "Answer concisely",
    system_prompt = "You are a helpful assistant. Be concise.",
}))

crew:add_task({
    name = "test",
    description = "In one sentence, what is the primary benefit of Rust's ownership model?",
    agent = "assistant",
})

crew:run()
