--[[
    Collaborative Task + MessageBus Example

    Demonstrates:
    - Multi-agent collaborative discussion
    - MessageBus for agent-to-agent communication
    - Agents building on each other's work
]]

local crew = Crew.new({
    goal = "Demonstrate agent collaboration and messaging",
    provider = "openai",
    model = env("OPENAI_MODEL") or "gpt-5.6-luna",
    base_url = env("OPENAI_BASE_URL"),
})

crew:add_agent(Agent.new({
    name = "optimist",
    goal = "See the positive side of things and advocate for opportunities",
    capabilities = {"analysis", "advocacy"},
}))

crew:add_agent(Agent.new({
    name = "critic",
    goal = "Identify risks, weaknesses, and potential problems",
    capabilities = {"analysis", "risk-assessment"},
}))

crew:add_agent(Agent.new({
    name = "pragmatist",
    goal = "Find practical, balanced solutions",
    capabilities = {"synthesis", "planning"},
}))

-- Phase 1: Individual research (runs in parallel)
crew:add_task({
    name = "research_benefits",
    description = "List 3 key benefits of using AI agents in software development. Brief bullet points.",
    agent = "optimist",
})

crew:add_task({
    name = "research_risks",
    description = "List 3 key risks of using AI agents in software development. Brief bullet points.",
    agent = "critic",
})

-- Phase 2: Collaborative discussion (agents debate the topic together)
crew:add_collaborative_task({
    name = "debate",
    description = "Should software teams adopt AI agents for code generation? Discuss the benefits and risks identified by your colleagues. Each agent should argue from their perspective.",
    agents = {"optimist", "critic", "pragmatist"},
    max_turns = 2,
    depends_on = {"research_benefits", "research_risks"},
})

-- Seed the MessageBus before execution. Targeted messages and broadcasts are
-- delivered to each agent with its next task prompt, then consumed.
crew:message_send(
    "facilitator",
    "*",
    "Ground every claim in a concrete software-team practice.",
    "broadcast"
)
crew:message_send(
    "critic",
    "pragmatist",
    "In the synthesis, pair every recommendation with a risk control.",
    "request"
)

local results = crew:run()

for _, result in ipairs(results) do
    if result.success then
        print("=== " .. result.task .. " (by " .. result.agent .. ", " .. result.duration_ms .. "ms) ===")
        print(result.output)
    else
        print("FAILED: " .. result.task .. " - " .. result.output)
    end
    print()
end

-- MessageBus is also available directly to Lua for explicit coordination.
-- This targeted follow-up is consumed from the recipient's queue, while the
-- history remains available for observability.
crew:message_send(
    "pragmatist",
    "optimist",
    "Please carry the agreed risk controls into the next planning cycle.",
    "request"
)

print("=== MessageBus follow-up inbox ===")
for _, message in ipairs(crew:message_read("optimist")) do
    print(string.format("%s -> %s [%s]: %s",
        message.from, message.to, message.type, message.content))
end

local history = crew:message_history()
print(string.format("MessageBus history retained %d message(s).", #history))
