-- IC-007 counted external-effect fixture. The canary injects a bounded
-- OpenAI-compatible mock and never uses a live provider credential.

local provider_base_url = assert(
    env("PLATFORM_CANARY_PROVIDER_BASE_URL"),
    "PLATFORM_CANARY_PROVIDER_BASE_URL must be explicitly allowlisted"
)

local crew = Crew.new({
    goal = "Prove one externally counted provider/tool effect",
    provider = "openai",
    model = "ic007-platform-canary",
    api_key = "ic007-mock-not-a-secret",
    base_url = provider_base_url,
    max_concurrent = 1,
})

crew:add_agent(Agent.new({
    name = "effect-probe",
    goal = "Execute only the deterministic mock-requested effect",
    capabilities = { "testing" },
    tools = { "http_request" },
    temperature = 0.0,
}))

crew:add_task({
    name = "counted-effect",
    agent = "effect-probe",
    description = "Follow the mock provider's one http_request tool call.",
    expected_output = "The exact deterministic platform effect receipt",
    max_retries = 0,
    timeout_secs = 30,
})

local results = crew:run()
assert(#results == 1, "platform effect fixture expected one result")
assert(results[1].success, "platform effect fixture did not succeed")
assert(
    results[1].output == "ic007-platform-effect-recorded",
    "unexpected platform effect receipt"
)
