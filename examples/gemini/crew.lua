--[[
    Google Gemini Provider Example

    Demonstrates using Google Gemini via its OpenAI-compatible endpoint,
    including JSON Schema structured output.

    Requires GEMINI_API_KEY in .env

    Run: ironcrew run examples/gemini
]]

local crew = Crew.new({
    goal = "Demonstrate Google Gemini as an LLM provider with structured output",
    provider = "openai",
    model = env("GEMINI_MODEL") or "gemini-3-flash-preview",
    base_url = "https://generativelanguage.googleapis.com/v1beta/openai",
    api_key = env("GEMINI_API_KEY"),
})

-- Agent with plain text output
crew:add_agent(Agent.new({
    name = "analyst",
    goal = "Provide clear, concise analysis",
    capabilities = {"analysis", "comparison"},
    temperature = 0.5,
}))

-- Agent with JSON Schema response_format
crew:add_agent(Agent.new({
    name = "data_extractor",
    goal = "Extract structured data into JSON",
    capabilities = {"extraction", "json"},
    temperature = 0.1,
    response_format = {
        type = "json_schema",
        name = "language_comparison",
        schema = {
            type = "object",
            properties = {
                languages = {
                    type = "array",
                    items = {
                        type = "object",
                        properties = {
                            name = { type = "string" },
                            category = { type = "string" },
                            main_strength = { type = "string" },
                            year_created = { type = "integer" },
                        },
                        required = { "name", "category", "main_strength", "year_created" },
                    },
                },
                summary = { type = "string" },
            },
            required = { "languages", "summary" },
        },
    },
}))

-- Task 1: Plain text analysis
crew:add_task({
    name = "overview",
    description = "In 2 sentences, explain what makes Google Gemini different from other LLMs.",
    agent = "analyst",
})

-- Task 2: JSON Schema structured output
crew:add_task({
    name = "extract_languages",
    description = "Extract structured data about these 3 programming languages: Rust, Python, and TypeScript. Include name, category (systems/scripting/web), main strength, and year created.",
    agent = "data_extractor",
    depends_on = { "overview" },
})

-- Task 3: Validate the JSON Schema output
crew:add_task({
    name = "use_data",
    description = "Based on the structured data, which language is newest? Answer in one sentence.",
    agent = "analyst",
    depends_on = { "extract_languages" },
})

local results = crew:run()

print("=== Gemini Results ===")
print()

for _, result in ipairs(results) do
    if result.success then
        print("[OK] " .. result.task .. " (by " .. result.agent .. ", " .. result.duration_ms .. "ms)")

        -- Try to pretty-print JSON
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
        print("[FAIL] " .. result.task .. " - " .. result.output)
    end
    print()
end

-- Validate the JSON Schema output
for _, result in ipairs(results) do
    if result.task == "extract_languages" and result.success then
        local check = validate_json(result.output, {
            type = "object",
            required = { "languages", "summary" },
            properties = {
                languages = {
                    type = "array",
                    items = {
                        type = "object",
                        required = { "name", "category", "main_strength", "year_created" },
                    },
                },
                summary = { type = "string" },
            },
        })
        if check.valid then
            print("JSON Schema validation: PASSED")
        else
            print("JSON Schema validation: FAILED")
            for _, err in ipairs(check.errors) do
                print("  " .. err.message)
            end
        end
    end
end
