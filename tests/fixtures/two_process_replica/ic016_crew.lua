-- Provider-free mailbox fixture for the IC-016 rolling key-rotation gate.

local crew = Crew.new({
    goal = "verify staged human-input key rotation",
    provider = "openai",
    model = "offline-test",
    api_key = "unused",
})

local answer = crew:ask_human({
    prompt = "Approve the IC-016 key rotation?",
    choices = { "approve", "reject" },
    timeout_s = 600,
})

if answer ~= "rotation-approved" then
    error("the IC-016 encrypted answer was not delivered intact")
end

print("IC-016 encrypted answer consumed")
