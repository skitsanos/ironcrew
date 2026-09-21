-- OpenAI Responses API — basic
-- Model: gpt-5.6-luna
-- Omitted effort defaults to low, including when function tools are present.
-- Requires: OPENAI_API_KEY
-- Optional shared run ceiling:
-- IRONCREW_MAX_RUN_TOKENS=100000 ironcrew run examples/providers/02-openai-responses.lua
-- With a ceiling enabled, omitted max_tokens uses a 4096-token output bound.

local crew = Crew.new({
    goal = "Verify OpenAI Responses API works",
    provider = "openai-responses",
    model = "gpt-5.6-luna",
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

-- Construction validation needs no credential and stops before paid execution.
if IRONCREW_MODE == "validate" then return end
crew:run()
print("Run token budget:", json_stringify(crew:flow_usage().budget))
