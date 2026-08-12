-- Provider-free keyed HITL/SSE fixture for IC-007 platform routing.

local crew = Crew.new({
    goal = "Verify shared keyed control and retained event routing",
    provider = "openai",
    model = "offline-test",
    api_key = "unused",
})

local first = crew:ask_human({
    prompt = "Approve IC-007 platform checkpoint one?",
    choices = { "approve", "reject" },
    timeout_s = 300,
})
if first ~= "checkpoint-one-approved" then
    error("the first platform answer was not delivered intact")
end
print("IC-007 platform checkpoint one completed")

local second = crew:ask_human({
    prompt = "Approve IC-007 platform checkpoint two?",
    choices = { "approve", "reject" },
    timeout_s = 300,
})
if second ~= "checkpoint-two-approved" then
    error("the second platform answer was not delivered intact")
end
print("IC-007 platform shared-control fixture completed")
