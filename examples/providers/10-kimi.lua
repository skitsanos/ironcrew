-- Kimi K2.5 via Moonshot AI (OpenAI-compat)
-- Model: kimi-k2.5
-- Requires: MOONSHOT_API_KEY

-- API key is auto-resolved from MOONSHOT_API_KEY env var based on base_url.
local crew = Crew.new({
    goal = "Verify Kimi K2.5 via Moonshot OpenAI compat",
    provider = "openai",
    model = "kimi-k2.5",
    base_url = "https://api.moonshot.ai/v1",
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
