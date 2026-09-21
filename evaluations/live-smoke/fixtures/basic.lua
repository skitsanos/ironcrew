local origin = assert(env("IC048_BASE_URL"), "IC048_BASE_URL is required")
local crew = Crew.new({
    goal = "IC048 basic compatibility", provider = "openai-responses",
    model = "gpt-5.6-luna", base_url = origin, api_key = env("IC048_FIXTURE_KEY"),
})
crew:add_agent(Agent.new({
    name = "assistant", goal = "Answer briefly", reasoning_effort = "none",
    max_tokens = 1024,
}))
crew:add_task({name = "answer", agent = "assistant", max_retries = 0,
    timeout_secs = 45, description = "Describe one benefit of automated tests in one sentence."})
if IRONCREW_MODE == "validate" then return end
local results = crew:run()
print("IC048_RESULT " .. json_stringify({results = results, usage = crew:flow_usage()}))
