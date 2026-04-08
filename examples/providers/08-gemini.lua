-- Google Gemini via OpenAI-compat endpoint
-- Model: gemini-2.5-flash
-- Requires: GEMINI_API_KEY

-- API key is auto-resolved from GEMINI_API_KEY env var based on base_url.
local crew = Crew.new({
    goal = "Verify Gemini via OpenAI compat",
    provider = "openai",
    model = "gemini-2.5-flash",
    base_url = "https://generativelanguage.googleapis.com/v1beta/openai",
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
