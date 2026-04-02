--[[
    Google Gemini Provider Example

    Demonstrates using Google Gemini via its OpenAI-compatible endpoint.
    Requires GEMENI_API_KEY in .env

    Run: ironcrew run examples/gemini
]]

local crew = Crew.new({
    goal = "Demonstrate Google Gemini as an LLM provider",
    provider = "openai",
    model = env("GEMINI_MODEL") or "gemini-2.5-flash",
    base_url = "https://generativelanguage.googleapis.com/v1beta/openai",
    api_key = env("GEMENI_API_KEY"),
})

crew:add_agent(Agent.new({
    name = "gemini_assistant",
    goal = "Answer questions using Google Gemini",
    capabilities = {"general", "analysis"},
    temperature = 0.7,
}))

-- Simple text task
crew:add_task({
    name = "explain",
    description = "In 2-3 sentences, explain what makes Google Gemini different from other LLMs.",
    expected_output = "A concise comparison",
})

-- JSON output task
crew:add_task({
    name = "compare",
    description = [[
        Compare Gemini and GPT-4 across three dimensions.
        Return a JSON object with this structure:
        {"comparisons": [{"dimension": "...", "gemini": "...", "gpt4": "..."}]}
    ]],
    response_format = { type = "json_object" },
    depends_on = {"explain"},
})

local results = crew:run()

for _, result in ipairs(results) do
    if result.success then
        print("=== " .. result.task .. " (" .. result.duration_ms .. "ms) ===")

        local ok, parsed = pcall(json_parse, result.output)
        if ok and type(parsed) == "table" then
            print(json_stringify(parsed))
        else
            print(result.output)
        end

        if result.token_usage then
            print(string.format(
                "  [tokens: %d prompt, %d completion, %d total]",
                result.token_usage.prompt_tokens,
                result.token_usage.completion_tokens,
                result.token_usage.total_tokens
            ))
        end
    else
        print("FAILED: " .. result.task .. " - " .. result.output)
    end
    print()
end
