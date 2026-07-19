-- Provider-free fixture for the genuine two-process PostgreSQL acceptance test.
-- Both checkpoints execute in replica A's Lua process while their encrypted
-- questions and answers cross replica B through the shared store.

local crew = Crew.new({
    goal = "verify cross-replica process boundaries",
    provider = "openai",
    model = "offline-test",
    api_key = "unused",
})

local approval = crew:ask_human({
    prompt = "Approve the genuine two-process handoff?",
    choices = { "approve", "reject" },
    timeout_s = 30,
})

if approval ~= "approved-by-replica-b" then
    error("the first cross-replica answer was not delivered intact")
end

print("first cross-replica checkpoint completed")

local finish = crew:ask_human({
    prompt = "Finish the genuine two-process acceptance run?",
    choices = { "finish", "hold" },
    timeout_s = 30,
})

if finish ~= "finished-by-replica-b" then
    error("the second cross-replica answer was not delivered intact")
end

print("two-process replica acceptance completed")
