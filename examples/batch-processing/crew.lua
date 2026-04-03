--[[
    Batch Processing Example

    Demonstrates:
    - file_read_glob tool for reading multiple files at once
    - validate_json() for schema validation of each item
    - foreach pattern for processing items individually
    - template() for generating a formatted report
]]

local crew = Crew.new({
    goal = "Process and analyze a batch of product data files",
    provider = "openai",
    model = env("OPENAI_MODEL") or "gpt-4o-mini",
    base_url = env("OPENAI_BASE_URL"),
})

crew:add_agent(Agent.new({
    name = "processor",
    goal = "Read and validate batch data files",
    capabilities = {"processing", "validation"},
    tools = {"file_read_glob"},
    temperature = 0.1,
}))

crew:add_agent(Agent.new({
    name = "analyst",
    goal = "Analyze product data and provide insights",
    capabilities = {"analysis", "summarization"},
    temperature = 0.5,
}))

-- Step 1: Read all JSON files from input/
crew:add_task({
    name = "load_files",
    description = "Read all JSON files from the input/ directory using file_read_glob with pattern 'input/*.json'",
    agent = "processor",
})

-- Step 2: Analyze the batch
crew:add_task({
    name = "analyze_batch",
    description = "Analyze the product data. How many items are in stock? What's the average price? Which category has the most items? Answer in 3-4 sentences.",
    agent = "analyst",
    depends_on = {"load_files"},
})

local results = crew:run()

-- Validate each loaded item against a schema
local item_schema = {
    type = "object",
    required = {"id", "name", "category", "price", "in_stock"},
    properties = {
        id = { type = "string" },
        name = { type = "string" },
        category = { type = "string" },
        price = { type = "number" },
        in_stock = { type = "boolean" },
    },
}

-- Parse the loaded files from the first task's output
for _, result in ipairs(results) do
    if result.task == "load_files" and result.success then
        local ok, items = pcall(json_parse, result.output)
        if ok and type(items) == "table" then
            local valid_count = 0
            local invalid_count = 0
            for _, item in ipairs(items) do
                if item.content then
                    local check = validate_json(item.content, item_schema)
                    if check.valid then
                        valid_count = valid_count + 1
                    else
                        invalid_count = invalid_count + 1
                        log("warn", "Invalid item in " .. (item.path or "?") .. ": " .. #check.errors .. " errors")
                    end
                end
            end
            print(template("Schema validation: {{ valid }} valid, {{ invalid }} invalid out of {{ total }} files", {
                valid = valid_count,
                invalid = invalid_count,
                total = valid_count + invalid_count,
            }))
        end
    end
end

print()

-- Print analysis
for _, result in ipairs(results) do
    if result.success then
        print("=== " .. result.task .. " (" .. result.duration_ms .. "ms) ===")
        print(result.output)
        print()
    else
        print("FAILED: " .. result.task .. " - " .. result.output)
    end
end
