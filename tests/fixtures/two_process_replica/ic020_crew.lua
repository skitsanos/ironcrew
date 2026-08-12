-- Provider-free held run for the IC-020 drain and replacement acceptance gate.

local crew = Crew.new({
    goal = "verify explicit replica drain and rolling replacement",
    provider = "openai",
    model = "offline-test",
    api_key = "unused",
})

local answer = crew:ask_human({
    prompt = "Hold the IC-020 owner until drain completes",
    choices = { "release" },
    timeout_s = 600,
})

if answer ~= "release" then
    error("the IC-020 human answer was not delivered intact")
end

print("IC-020 held run released")
