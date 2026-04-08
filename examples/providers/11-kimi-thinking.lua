-- Kimi K2 Thinking via Moonshot AI (OpenAI-compat reasoning_content test)
-- Model: kimi-k2-thinking
-- Tests the reasoning_content field parsing in the OpenAI provider.
-- Reasoning should stream dim to stderr and be persisted to the run record.
-- Requires: MOONSHOT_API_KEY

-- API key is auto-resolved from MOONSHOT_API_KEY env var based on base_url.
local crew = Crew.new({
    goal = "Verify Kimi reasoning_content parsing",
    provider = "openai",
    model = "kimi-k2-thinking",
    base_url = "https://api.moonshot.ai/v1",
    stream = true,
})

crew:add_agent(Agent.new({
    name = "solver",
    goal = "Solve problems step by step",
}))

crew:add_task({
    name = "puzzle",
    description = "A bat and ball cost $1.10 total. The bat costs $1.00 more than the ball. How much does the ball cost? Think carefully.",
    agent = "solver",
})

crew:run()
