--[[
    Conditional Crew Example

    Demonstrates:
    - Conditional task execution (add_task_if)
    - on_error routing to handler tasks
    - Error recovery
]]

local crew = Crew.new({
    goal = "Demonstrate conditional tasks and error handling",
    provider = "openai",
    model = env("OPENAI_MODEL") or "gpt-4o-mini",
    base_url = env("OPENAI_BASE_URL"),
})

crew:add_agent(Agent.new({
    name = "analyst",
    goal = "Analyze data and make decisions",
    capabilities = {"analysis", "decision"},
}))

crew:add_agent(Agent.new({
    name = "handler",
    goal = "Handle errors gracefully",
    capabilities = {"error-handling"},
}))

-- Step 1: Analyze something
crew:add_task({
    name = "analyze",
    description = "Rate the programming language Python on a scale of 1-10. Return ONLY a number.",
    expected_output = "A single number between 1 and 10",
})

-- Step 2: Only runs if analysis was successful (condition)
crew:add_task_if("results.analyze and results.analyze.success", {
    name = "summarize",
    description = "Write a one-sentence summary based on the analysis rating",
    depends_on = {"analyze"},
})

-- Step 3: A task with error recovery
crew:add_task({
    name = "risky_task",
    description = "Briefly explain what makes Python popular in 1-2 sentences",
    on_error = "fallback",
    depends_on = {"analyze"},
})

-- Error handler (only runs if risky_task fails)
crew:add_task({
    name = "fallback",
    description = "Provide a default response: Python is popular due to its simplicity",
    agent = "handler",
})

local results = crew:run()

for _, result in ipairs(results) do
    local status = result.success and "OK" or "FAIL"
    print("[" .. status .. "] " .. result.task .. " (by " .. result.agent .. ")")
    print("  " .. result.output)
    print()
end
