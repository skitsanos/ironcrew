--[[
    Parallel Task Execution Example

    Tasks a, b, c have no dependencies — they run in parallel.
    Task d depends on all three — it runs after they complete.
]]

local crew = Crew.new({
    goal = "Demonstrate parallel task execution",
    provider = "openai",
    model = env("OPENAI_MODEL") or "gpt-4o-mini",
    base_url = env("OPENAI_BASE_URL"),
    max_concurrent = 3,
})

crew:add_agent(Agent.new({
    name = "assistant",
    goal = "Answer questions concisely",
    capabilities = {"general"},
}))

-- These three tasks have no dependencies — they'll run in parallel
crew:add_task({
    name = "task_a",
    description = "In one sentence, what is Rust's main advantage?",
})

crew:add_task({
    name = "task_b",
    description = "In one sentence, what is Python's main advantage?",
})

crew:add_task({
    name = "task_c",
    description = "In one sentence, what is Go's main advantage?",
})

-- This task depends on all three — runs after they complete
crew:add_task({
    name = "summary",
    description = "Compare the three languages based on the findings above and pick the best one for systems programming in 2 sentences.",
    depends_on = {"task_a", "task_b", "task_c"},
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
