-- OpenAI Responses API — basic usage
--
-- Uses the /v1/responses endpoint instead of /v1/chat/completions.
-- Stateless mode — every task sends full context (no previous_response_id).
-- Requires OPENAI_API_KEY in .env or environment.

local crew = Crew.new({
    goal = "Demonstrate basic OpenAI Responses API usage",
    provider = "openai-responses",
    model = "gpt-5.4",
    api_key = env("OPENAI_API_KEY"),
})

crew:add_agent(Agent.new({
    name = "assistant",
    goal = "Answer questions clearly and concisely",
    capabilities = { "reasoning", "writing" },
    system_prompt = "You are a helpful assistant. Be concise.",
}))

crew:add_task({
    name = "explain",
    description = "In 2-3 sentences, explain what ownership means in Rust and why it matters.",
    agent = "assistant",
})

crew:run()
