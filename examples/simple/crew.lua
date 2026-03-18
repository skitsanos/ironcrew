local crew = Crew.new({
    goal = "Answer a simple question about Rust programming",
    provider = "openai",
    model = env("OPENAI_MODEL") or "gpt-4o-mini",
    base_url = env("OPENAI_BASE_URL"),
})

crew:add_agent(Agent.new({
    name = "assistant",
    goal = "Answer programming questions clearly and concisely",
    capabilities = {"programming", "explanation"},
    temperature = 0.5,
}))

-- Add stream = true to see LLM responses in real-time:
crew:add_task({
    name = "answer",
    description = "Explain what ownership means in Rust in 2-3 sentences.",
    expected_output = "A clear, concise explanation of Rust ownership",
    max_retries = 2,
    retry_backoff_secs = 1.0,
    timeout_secs = 120,
    -- stream = true,  -- uncomment to enable streaming output
})

local results = crew:run()

for _, result in ipairs(results) do
    print("--- " .. result.task .. " (by " .. result.agent .. ") ---")
    print(result.output)
    print()
end
