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
    model = env("OPENAI_MODEL") or "gpt-4o-mini",
    base_url = env("OPENAI_BASE_URL"),
})

crew:add_agent(Agent.new({
    name = "optimist",
    goal = "See the positive side of things and advocate for opportunities",
    capabilities = {"analysis", "advocacy"},
    temperature = 0.8,
}))

crew:add_agent(Agent.new({
    name = "critic",
    goal = "Identify risks, weaknesses, and potential problems",
    capabilities = {"analysis", "risk-assessment"},
    temperature = 0.3,
}))

crew:add_agent(Agent.new({
    name = "pragmatist",
    goal = "Find practical, balanced solutions",
    capabilities = {"synthesis", "planning"},
    temperature = 0.5,
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
