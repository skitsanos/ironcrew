--[[
    Groq Provider + JSON Output Example

    Demonstrates:
    - Using Groq (or any OpenAI-compatible API) via base_url override
    - response_format = json_object for simple JSON output
    - Using json_parse() to work with structured responses in Lua
]]

local crew = Crew.new({
    goal = "Generate structured data using Groq",
    provider = "openai",
    model = env("GROQ_MODEL") or "llama-3.3-70b-versatile",
    base_url = env("GROQ_API_URL") or "https://api.groq.com/openai/v1",
    api_key = env("GROQ_API_KEY"),
})

crew:add_agent(Agent.new({
    name = "analyst",
    goal = "Analyze topics and return structured JSON data",
    system_prompt = "You are a data analyst. Always respond with valid JSON objects.",
    capabilities = { "analysis", "json" },
    temperature = 0.2,
    response_format = {
        type = "json_object",
    },
}))

crew:add_task({
    name = "analyze",
    description = [[
        Return a JSON object with the following structure:
        {
            "topic": "Rust programming language",
            "pros": ["list of 3 advantages"],
            "cons": ["list of 2 disadvantages"],
            "rating": <number from 1-10>,
            "recommendation": "<one sentence>"
        }
    ]],
    expected_output = "A JSON object with topic analysis",
    timeout_secs = 30,
})

local results = crew:run()

for _, result in ipairs(results) do
    if result.success then
        print("=== " .. result.task .. " ===")

        -- Parse the JSON response
        local data = json_parse(result.output)
        if data then
            print("Topic: " .. (data.topic or "unknown"))
            print("Rating: " .. tostring(data.rating or "N/A") .. "/10")
            print("Recommendation: " .. (data.recommendation or "N/A"))

            if data.pros then
                print("\nPros:")
                for i, pro in ipairs(data.pros) do
                    print("  " .. i .. ". " .. pro)
                end
            end

            if data.cons then
                print("\nCons:")
                for i, con in ipairs(data.cons) do
                    print("  " .. i .. ". " .. con)
                end
            end

            -- Save to file
            print("\nRaw JSON:")
            print(json_stringify(data))
        else
            print(result.output)
        end
    else
        print("FAILED: " .. result.task .. " - " .. result.output)
    end
end
