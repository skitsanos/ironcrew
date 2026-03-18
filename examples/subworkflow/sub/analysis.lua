--[[
    Sub-crew: performs a focused analysis on a given topic.
    Receives `input.topic` from the parent crew.
]]

local topic = (input and input.topic) or "general programming"

local crew = Crew.new({
    goal = "Analyze " .. topic,
    provider = "openai",
    model = env("OPENAI_MODEL") or "gpt-4o-mini",
    base_url = env("OPENAI_BASE_URL"),
})

crew:add_agent(Agent.new({
    name = "expert",
    goal = "Provide expert analysis on technical topics",
    capabilities = {"analysis", "expertise"},
    temperature = 0.3,
}))

crew:add_task({
    name = "analyze",
    description = "Provide a brief 2-sentence expert analysis of: " .. topic,
    expected_output = "A concise expert analysis",
})

local results = crew:run()

-- Return the analysis output
if results and #results > 0 and results[1].success then
    return results[1].output
else
    return "Analysis failed"
end
