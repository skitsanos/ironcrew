--[[
    Subworkflow Example

    Demonstrates nested crew composition — the main crew
    delegates a sub-task to a separate crew defined in another file.
]]

local crew = Crew.new({
    goal = "Demonstrate subworkflow composition",
    provider = "openai",
    model = env("OPENAI_MODEL") or "gpt-4o-mini",
    base_url = env("OPENAI_BASE_URL"),
})

crew:add_agent(Agent.new({
    name = "coordinator",
    goal = "Coordinate work and integrate results",
    capabilities = {"coordination", "summarization"},
}))

-- Main task
crew:add_task({
    name = "gather",
    description = "List 3 popular programming languages and one strength of each, briefly",
})

-- This task depends on gather, then runs a subworkflow
crew:add_task({
    name = "deep_dive",
    description = "Based on the gathered info, pick the most interesting language and explain why in 2 sentences",
    depends_on = {"gather"},
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

-- Also demonstrate subworkflow execution
print("--- Running subworkflow ---")
local sub_result = crew:subworkflow("sub/analysis.lua", {
    input = { topic = "Rust programming" },
})
if sub_result then
    print("Subworkflow returned: " .. tostring(sub_result))
end
