-- OpenAI Responses API — reasoning models
--
-- Uses reasoning_effort to enable deep thinking on gpt-5.6-luna; the crew sets the
-- default and the solver agent raises it for its own requests.
-- The reasoning summary is captured in the run record under `reasoning`.
-- With stream = true, reasoning summary deltas appear dim on stderr.

local crew = Crew.new({
    goal = "Solve a logic puzzle using reasoning",
    provider = "openai-responses",
    model = "gpt-5.6-luna",
    api_key = env("OPENAI_API_KEY"),
    reasoning_effort = "medium",
    reasoning_summary = "auto",
    stream = true,
})

crew:add_agent(Agent.new({
    name = "solver",
    goal = "Solve problems methodically",
    capabilities = { "reasoning", "logic" },
    -- Per-agent override: this agent thinks harder than the crew default.
    reasoning_effort = "high",
}))

crew:add_task({
    name = "puzzle",
    description = [[
A farmer needs to cross a river with a wolf, a goat, and a cabbage.
The boat holds only the farmer and one item at a time.
The wolf will eat the goat if left alone; the goat will eat the cabbage.
How does the farmer get everything across safely?
]],
    agent = "solver",
})

crew:run()
