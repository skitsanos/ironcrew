-- OpenAI Chat Completions API — baseline test
-- Model: gpt-5.4-mini (cost-effective)
-- Requires: OPENAI_API_KEY

local crew = Crew.new({
    goal = "Verify OpenAI Chat Completions API works",
    provider = "openai",
    model = "gpt-5.4-mini",
    api_key = env("OPENAI_API_KEY"),
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
