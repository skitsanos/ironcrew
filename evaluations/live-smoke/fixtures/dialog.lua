local origin = assert(env("IC048_BASE_URL"), "IC048_BASE_URL is required")
local crew = Crew.new({
    goal = "IC048 bounded dialog", provider = "openai-responses",
    model = "gpt-5.6-luna", base_url = origin, api_key = env("IC048_FIXTURE_KEY"),
})
for _, name in ipairs({"alice", "bob"}) do
    crew:add_agent(Agent.new({name = name, goal = "Discuss concise test coverage",
        reasoning_effort = "low", max_tokens = 1024,
        system_prompt = "Reply in one short sentence. Do not leave the answer blank."}))
end
local dialog = crew:dialog({agents = {"alice", "bob"}, max_turns = 2,
    stream = true, starter = "Name a useful kind of software test.",
    should_stop = function(_, transcript)
        if #transcript == 2 then return "smoke_turn_cap" end
        return false
    end})
if IRONCREW_MODE == "validate" then return end
local turns = dialog:run()
print("IC048_RESULT " .. json_stringify({turns = turns, reason = dialog:stop_reason(),
    stopped = dialog:stopped(), usage = crew:flow_usage()}))
