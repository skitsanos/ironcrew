--[[
    Memory Example

    Demonstrates the crew memory system:
    - memory_set/memory_get for shared state
    - Persistent memory across conceptual "sessions"
    - Memory context auto-injected into agent prompts
]]

local crew = Crew.new({
    goal = "Demonstrate memory system",
    provider = "openai",
    model = env("OPENAI_MODEL") or "gpt-4o-mini",
    base_url = env("OPENAI_BASE_URL"),
    memory = "ephemeral",  -- or "persistent" to save to .ironcrew/memory.json
})

crew:add_agent(Agent.new({
    name = "researcher",
    goal = "Research topics and remember findings",
    capabilities = {"research", "analysis"},
}))

-- Store some context in memory before tasks run
crew:memory_set("project", "IronCrew - AI Agent Framework")
crew:memory_set("preferences", {
    style = "concise",
    format = "bullet points",
    max_length = "3 items",
})

-- Task 1: Research (memory context will be auto-injected)
crew:add_task({
    name = "research",
    description = "List 3 key features of Rust programming language. Keep it concise with bullet points.",
})

-- Task 2: Use memory to build on previous work
crew:add_task({
    name = "analyze",
    description = "Based on the research, which Rust feature is most important for building AI frameworks? One sentence.",
    depends_on = {"research"},
})

local results = crew:run()

-- Display results
for _, result in ipairs(results) do
    if result.success then
        print("=== " .. result.task .. " (" .. result.duration_ms .. "ms) ===")
        print(result.output)
    else
        print("FAILED: " .. result.task .. " - " .. result.output)
    end
    print()
end

-- Show what's in memory after the run
print("--- Memory contents ---")
local keys = crew:memory_keys()
for _, key in ipairs(keys) do
    local value = crew:memory_get(key)
    print(key .. " = " .. json_stringify(value))
end
