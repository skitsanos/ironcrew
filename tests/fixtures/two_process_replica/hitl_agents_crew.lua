-- Two named agents independently ask the human during a real crew run.
-- The provider is a bounded local OpenAI-compatible fixture injected only
-- into the CLI and disposable PostgreSQL acceptance processes.
local crew = Crew.new({
    goal = "verify agent-directed human input across product channels",
    provider = "openai",
    model = "hitl-agent-fixture",
    base_url = env("HITL_PROVIDER_BASE_URL"),
    api_key = "hitl-agent-local-mock-key",
    max_concurrent = 1,
})

crew:add_agent(Agent.new({
    name = "analyst",
    goal = "Ask for the dataset before analysis",
    tools = { "ask_human" },
}))
crew:add_agent(Agent.new({
    name = "reviewer",
    goal = "Ask for approval before accepting the analysis",
    tools = { "ask_human" },
}))

crew:add_task({
    name = "analyze",
    agent = "analyst",
    description = "ANALYST_HITL_CHECKPOINT",
    expected_output = "FINAL:dataset-alpha",
})
crew:add_task({
    name = "review",
    agent = "reviewer",
    description = "REVIEWER_HITL_CHECKPOINT",
    expected_output = "FINAL:approved",
    depends_on = { "analyze" },
})

local results = crew:run()
local by_task = {}
for _, result in ipairs(results) do
    by_task[result.task] = result.output
end
if by_task.analyze ~= "FINAL:dataset-alpha" then
    error("analyst did not receive the human answer: " .. tostring(by_task.analyze))
end
if by_task.review ~= "FINAL:approved" then
    error("reviewer did not receive the human answer: " .. tostring(by_task.review))
end
print("HITL_AGENT_RESULTS=" .. by_task.analyze .. ":" .. by_task.review)
