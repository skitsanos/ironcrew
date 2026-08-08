-- Provider-free fixture for the unkeyed wrong-owner process gate.
-- The skipped task drives crew:run() through its durable lifecycle without an
-- LLM call. The later question then parks execution in replica A's process,
-- after the PostgreSQL run row and skipped result already exist.

local crew = Crew.new({
    goal = "verify unkeyed cross-replica ownership diagnostics",
    provider = "openai",
    model = "offline-test",
    api_key = "unused",
})

crew:add_agent(Agent.new({
    name = "offline",
    goal = "Exercise lifecycle without a provider call",
    capabilities = { "testing" },
}))

crew:add_task_if("false", {
    name = "skipped",
    agent = "offline",
    description = "This task is intentionally skipped",
    expected_output = "No provider output",
})

local results = crew:run()
if not results[1] or not results[1].success then
    error("expected the conditionally skipped task result")
end

crew:ask_human({
    prompt = "Keep the unkeyed owner alive?",
    choices = { "continue", "stop" },
    timeout_s = 600,
})
