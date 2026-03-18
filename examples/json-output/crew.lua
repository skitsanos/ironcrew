--[[
    JSON Output Example

    Demonstrates:
    - response_format with json_schema (structured output)
    - Chaining JSON output between tasks
    - Using file_write tool to save results
    - Lua globals: json_parse, uuid4, now_rfc3339
]]

local crew = Crew.new({
    goal = "Extract structured company data and save as JSON",
    provider = "openai",
    model = env("OPENAI_MODEL") or "gpt-4o-mini",
    base_url = env("OPENAI_BASE_URL"),
})

-- Task 1: Extract structured data using JSON Schema response format
-- The extractor agent has response_format = json_schema configured,
-- so the LLM is forced to return valid JSON matching the schema.
crew:add_task({
    name = "extract",
    description = [[
        Analyze these companies and extract structured data:

        1. Apple - Makes consumer electronics, software, and services.
           Known for iPhone, Mac, and ecosystem lock-in. Dominant in premium segment.

        2. Tesla - Electric vehicles and energy storage.
           Pioneered mass-market EVs. Strong brand but increasing competition.

        3. Shopify - E-commerce platform for businesses of all sizes.
           Growing rapidly in the SMB segment. Competing with BigCommerce and WooCommerce.
    ]],
    expected_output = "JSON object with companies array and summary",
    timeout_secs = 60,
})

-- Task 2: Take the JSON output and save it to a file
crew:add_task({
    name = "save_report",
    description = [[
        You received structured company analysis data from a previous task.
        Use the file_write tool to save it as a nicely formatted JSON file
        at "output/companies.json". Just save the JSON data as-is, properly formatted.
    ]],
    agent = "reporter",
    depends_on = { "extract" },
    timeout_secs = 60,
})

-- Run the crew
local results = crew:run()

-- Display results
for _, result in ipairs(results) do
    if result.success then
        print("=== " .. result.task .. " (by " .. result.agent .. ", " .. result.duration_ms .. "ms) ===")

        -- Try to pretty-print if it's JSON
        local ok, parsed = pcall(json_parse, result.output)
        if ok and parsed then
            print(json_stringify(parsed))
        else
            print(result.output)
        end
    else
        print("FAILED: " .. result.task .. " - " .. result.output)
    end
    print()
end

-- Print metadata
print("Run completed at: " .. now_rfc3339())
