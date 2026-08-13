--[[
    Model Router Example

    Demonstrates routing different tasks to different models.
    Efficient tasks use Luna; complex synthesis and explicit overrides use Terra.
]]

local crew = Crew.new({
    goal = "Demonstrate model routing for cost optimization",
    provider = "openai",
    model = env("OPENAI_MODEL") or "gpt-5.6-luna",  -- default
    base_url = env("OPENAI_BASE_URL"),
    models = {
        task_execution = env("OPENAI_MODEL") or "gpt-5.6-luna",
        collaboration = env("OPENAI_MODEL") or "gpt-5.6-luna",
        collaboration_synthesis = "gpt-5.6-terra",
    },
})

crew:add_agent(Agent.new({
    name = "fast_agent",
    goal = "Handle simple tasks quickly",
    capabilities = {"general"},
}))

crew:add_agent(Agent.new({
    name = "smart_agent",
    goal = "Handle complex analysis tasks",
    capabilities = {"analysis", "reasoning"},
    model = "gpt-5.6-terra",  -- agent-level override
}))

-- Simple task: uses crew default model (via router)
crew:add_task({
    name = "quick_task",
    description = "What is 2+2? Reply with just the number.",
    agent = "fast_agent",
})

-- Complex task: uses agent's model override
crew:add_task({
    name = "complex_task",
    description = "Explain the P vs NP problem in 2 sentences.",
    agent = "smart_agent",
    depends_on = {"quick_task"},
})

-- Task with per-task model override
crew:add_task({
    name = "specific_model_task",
    description = "Name one advantage of Rust in one sentence.",
    model = "gpt-5.6-terra",
    depends_on = {"quick_task"},
})

local results = crew:run()

for _, result in ipairs(results) do
    if result.success then
        print("=== " .. result.task .. " (" .. result.duration_ms .. "ms) ===")
        print(result.output)
    else
        print("FAILED: " .. result.task .. " - " .. result.output)
    end
    print()
end
