-- OpenAI Responses API — with reasoning and streaming
-- Model: gpt-5.4-nano (cheapest reasoning-capable model)
-- Reasoning summary is captured in the run record under `reasoning`.
-- With stream=true, reasoning summary deltas appear dim on stderr.
-- Requires: OPENAI_API_KEY

local crew = Crew.new({
    goal = "Verify OpenAI Responses reasoning + streaming",
    provider = "openai-responses",
    model = "gpt-5.4-nano",
    api_key = env("OPENAI_API_KEY"),
    reasoning_effort = "medium",
    reasoning_summary = "auto",
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
