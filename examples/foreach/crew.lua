--[[
    Foreach Task Example

    Demonstrates iterating over a list and processing each item.
]]

local crew = Crew.new({
    goal = "Demonstrate foreach pattern",
    provider = "openai",
    model = env("OPENAI_MODEL") or "gpt-4o-mini",
    base_url = env("OPENAI_BASE_URL"),
})

crew:add_agent(Agent.new({
    name = "analyst",
    goal = "Analyze topics concisely",
    capabilities = {"analysis"},
}))

-- Store a list in memory for the foreach task to iterate over
crew:memory_set("topics", json_stringify({"Rust", "Python", "Go"}))

-- Foreach task: process each topic
crew:add_foreach_task({
    name = "analyze_topics",
    description = "In one sentence, describe the main strength of ${item} as a programming language.",
    foreach = "topics",
    foreach_as = "item",
    agent = "analyst",
})

local results = crew:run()

for _, result in ipairs(results) do
    if result.success then
        print("=== " .. result.task .. " (" .. result.duration_ms .. "ms) ===")
        local ok, parsed = pcall(json_parse, result.output)
        if ok and type(parsed) == "table" then
            for i, v in ipairs(parsed) do
                print("  " .. i .. ". " .. v)
            end
        else
            print(result.output)
        end
    else
        print("FAILED: " .. result.task .. " - " .. result.output)
    end
    print()
end
