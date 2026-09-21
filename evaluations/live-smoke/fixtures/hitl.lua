-- Two named agents, each asking a question and requesting a separately gated write.
-- Only disposable text artifacts are permitted; the runner approves writer and denies reviewer.
local origin = assert(env("IC048_BASE_URL"), "IC048_BASE_URL is required")
local crew = Crew.new({
    goal = "IC048 human-controlled tools", provider = "openai-responses",
    model = "gpt-5.6-luna", base_url = origin, api_key = env("IC048_FIXTURE_KEY"),
    max_concurrent = 1, require_approval = {"file_write"},
})
for _, name in ipairs({"writer", "reviewer"}) do
    crew:add_agent(Agent.new({
        name = name, goal = "Follow human decisions truthfully", max_tokens = 1024,
        reasoning_effort = "low", tools = {"ask_human", "file_write"},
        response_format = {type = "json_schema", name = "smoke_decision", schema = {
            type = "object", additionalProperties = false,
            required = {"performed", "summary"}, properties = {
                performed = {type = "boolean"}, summary = {type = "string"},
            },
        }},
        system_prompt = "IC048_AGENT:" .. name .. [[
First call ask_human exactly once to ask whether the disposable fixture is ready.
Use a 15-second timeout. Wait for the answer. Then call file_write exactly once
with path output/]] .. name .. [[.txt and content 'smoke fixture'.
The runtime will independently ask the operator to approve or deny this write.
Do not retry a denied operation. Finally return JSON with performed true only
if the write tool succeeded, otherwise false, and a short truthful summary.
Do not call any other tools or invent a human answer.]],
    }))
    local task = {name = name, agent = name, max_retries = 0, timeout_secs = 60,
        description = "Ask the human, attempt your single gated fixture write, then report truthfully."}
    if name == "reviewer" then task.depends_on = {"writer"} end
    crew:add_task(task)
end
if IRONCREW_MODE == "validate" then return end
local results = crew:run()
print("IC048_RESULT " .. json_stringify({results = results, usage = crew:flow_usage()}))
