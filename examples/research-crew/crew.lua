local crew = Crew.new({
    goal = "Produce a brief report on a topic",
    provider = "openai",
    model = env("OPENAI_MODEL") or "gpt-4o-mini",
    base_url = env("OPENAI_BASE_URL"),
})

crew:add_task({
    name = "research",
    description = "List 3 key benefits of using Rust for systems programming",
    expected_output = "A numbered list of 3 benefits with brief explanations",
})

crew:add_task({
    name = "write_summary",
    description = "Write a one-paragraph summary based on the research findings",
    agent = "writer",
    depends_on = {"research"},
})

local results = crew:run()

for _, result in ipairs(results) do
    if result.success then
        print("=== " .. result.task .. " (by " .. result.agent .. ", " .. result.duration_ms .. "ms) ===")
        print(result.output)
    else
        print("FAILED: " .. result.task .. " - " .. result.output)
    end
    print()
end
