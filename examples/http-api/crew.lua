--[[
    HTTP API + Template Example

    Demonstrates:
    - http.get() / http.post() for direct API calls from Lua
    - template() for rendering results with Tera templates
    - Mixing Lua-level HTTP calls with LLM agent tasks
    - json_parse() for working with API responses
]]

local crew = Crew.new({
    goal = "Fetch data from an API and generate a summary",
    provider = "openai",
    model = env("OPENAI_MODEL") or "gpt-4o-mini",
    base_url = env("OPENAI_BASE_URL"),
})

crew:add_agent(Agent.new({
    name = "analyst",
    goal = "Analyze and summarize data",
    capabilities = {"analysis", "summarization"},
    temperature = 0.5,
}))

-- Step 1: Fetch data directly from a public API (no LLM needed)
-- Using JSONPlaceholder as a demo API
local api_response = http.get("https://jsonplaceholder.typicode.com/posts?_limit=5")

if not api_response.ok then
    log("error", "API request failed: " .. tostring(api_response.status))
    return
end

local posts = api_response.json
log("info", "Fetched " .. #posts .. " posts from API")

-- Store in memory for the agent to use
crew:memory_set("api_data", json_stringify(posts))

-- Step 2: Use template() to format the data (no LLM needed)
local formatted = template([[
## Fetched Posts

{% for post in posts %}
### {{ post.id }}. {{ post.title }}
{{ post.body | truncate(length=80) }}

{% endfor %}
]], { posts = posts })

log("info", "Formatted with template engine")

-- Step 3: LLM task — analyze the fetched data
crew:add_task({
    name = "analyze",
    description = "Analyze these blog posts and identify the main themes in 2-3 sentences:\n\n" .. formatted,
    expected_output = "A brief thematic analysis",
})

local results = crew:run()

for _, result in ipairs(results) do
    if result.success then
        print("=== " .. result.task .. " (" .. result.duration_ms .. "ms) ===")
        print(result.output)
    else
        print("FAILED: " .. result.task .. " - " .. result.output)
    end
end

-- Bonus: render the final output with a template
print()
print(template("Analysis completed at {{ time }} using {{ model }}", {
    time = now_rfc3339(),
    model = env("OPENAI_MODEL") or "gpt-4o-mini",
}))
