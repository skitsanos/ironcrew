-- Anthropic native — extended thinking with streaming
-- Model: claude-sonnet-4-5 (latest Sonnet)
-- Thinking is streamed dim to stderr and persisted to the run record.
-- Requires: ANTHROPIC_API_KEY

local crew = Crew.new({
    goal = "Verify Anthropic extended thinking + streaming",
    provider = "anthropic",
    model = "claude-sonnet-4-5-20250929",
    api_key = env("ANTHROPIC_API_KEY"),
    thinking_budget = 3000,
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
