-- Provider-free run/chat fixture for IC-007 per-replica admission checks.

local crew = Crew.new({
    goal = "Verify process-local admission and shared durable quota",
    provider = "openai",
    model = "offline-test",
    api_key = "unused",
    max_concurrent = 1,
})

crew:add_agent(Agent.new({
    name = "holder",
    goal = "Hold one provider-free process-local slot",
    capabilities = { "testing" },
}))

if IRONCREW_MODE ~= "chat" then
    crew:ask_human({
        prompt = "Hold IC-007 admission capacity?",
        choices = { "release" },
        timeout_s = 300,
    })
end
