-- Replica-soak flow: exercises durable HITL, run history, and SSE without
-- calling a model provider. The unreachable base URL is a second fail-closed
-- guard; this flow intentionally never calls crew:run().
local crew = Crew.new({
    goal = "Exercise cross-replica control paths without model spend",
    provider = "openai",
    model = "replica-soak-no-llm",
    api_key = "replica-soak-no-network",
    base_url = "http://127.0.0.1:9/v1",
})

local answer = crew:ask_human({
    prompt = "Continue the bounded replica soak run?",
    choices = { "continue", "stop" },
    timeout_s = 30,
})

if answer ~= "continue" then
    error("replica soak received an unexpected answer")
end

print("replica soak checkpoint completed")
