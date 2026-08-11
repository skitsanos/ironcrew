-- Provider-free dual run/chat fixture for the IC-019 process admission gate.
-- HTTP conversation bootstrap sets the canonical Lua global to "chat", so it
-- constructs the same crew without entering the run-only human-input wait.

local crew = Crew.new({
    goal = "verify process-local admission scope",
    provider = "openai",
    model = "offline-test",
    api_key = "unused",
    max_concurrent = 1,
})

crew:add_agent(Agent.new({
    name = "holder",
    goal = "Hold one provider-free HTTP conversation slot",
    capabilities = { "testing" },
}))

if IRONCREW_MODE ~= "chat" then
    crew:ask_human({
        prompt = "Hold IC-019 admission capacity",
        choices = { "release" },
        timeout_s = 600,
    })
end
