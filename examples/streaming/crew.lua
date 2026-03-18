--[[
    Streaming Example

    Demonstrates real-time streaming of LLM responses.
    Watch the output appear word-by-word on stderr while
    the final result is collected and printed normally.
]]

local crew = Crew.new({
    goal = "Demonstrate streaming output",
    provider = "openai",
    model = env("OPENAI_MODEL") or "gpt-4o-mini",
    base_url = env("OPENAI_BASE_URL"),
    stream = true,  -- enable streaming for all tasks
})

crew:add_agent(Agent.new({
    name = "storyteller",
    goal = "Tell short, engaging stories",
    capabilities = {"writing", "creativity"},
    temperature = 0.9,
}))

crew:add_task({
    name = "story",
    description = "Write a very short (3-4 sentences) story about a robot learning to paint.",
})

local results = crew:run()

print()
print("=== Final collected result ===")
for _, result in ipairs(results) do
    if result.success then
        print(result.output)
    end
end
