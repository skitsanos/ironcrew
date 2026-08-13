-- IC-018 conversation profile. Turns use a bounded loopback mock. The runner
-- distinguishes warm-owner work, committed-boundary cold-peer rehydration,
-- and the unsupported shared-store conversation-SSE boundary.

local provider_base_url = assert(
    env("IC018_PROFILE_PROVIDER_BASE_URL"),
    "IC018_PROFILE_PROVIDER_BASE_URL must be explicitly allowlisted"
)

local crew = Crew.new({
    goal = "Exercise bounded conversation coordination boundaries",
    provider = "openai",
    model = "ic018-conversation",
    api_key = "ic018-loopback-not-a-secret",
    base_url = provider_base_url,
    max_concurrent = 1,
})

crew:add_agent(Agent.new({
    name = "coordinator",
    goal = "Return the deterministic conversation profile reply",
    system_prompt = "Use only the bounded IC-018 loopback fixture.",
    capabilities = { "testing" },
    temperature = 0.0,
}))
