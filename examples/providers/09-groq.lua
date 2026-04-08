-- Groq via OpenAI-compat endpoint
-- Model: llama-3.3-70b-versatile (fast inference)
-- Requires: GROQ_API_KEY

-- API key is auto-resolved from GROQ_API_KEY env var based on base_url.
local crew = Crew.new({
    goal = "Verify Groq via OpenAI compat",
    provider = "openai",
    model = "llama-3.3-70b-versatile",
    base_url = "https://api.groq.com/openai/v1",
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
