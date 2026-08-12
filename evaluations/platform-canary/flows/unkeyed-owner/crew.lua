-- Deterministic unkeyed wrong-owner fixture for IC-007 platform diagnosis.

local provider_base_url = assert(
    env("PLATFORM_CANARY_PROVIDER_BASE_URL"),
    "PLATFORM_CANARY_PROVIDER_BASE_URL must be explicitly allowlisted"
)

local crew = Crew.new({
    goal = "Verify truthful unkeyed owner diagnostics",
    provider = "openai",
    model = "ic007-platform-canary",
    api_key = "ic007-mock-not-a-secret",
    base_url = provider_base_url,
    max_concurrent = 1,
})

crew:add_agent(Agent.new({
    name = "owner-probe",
    goal = "Complete one deterministic task before the owner-local checkpoint",
    capabilities = { "testing" },
    tools = { "http_request" },
}))

crew:add_task({
    name = "materialize-owner",
    agent = "owner-probe",
    description = "Follow the mock provider's one bounded http_request tool call.",
    expected_output = "The exact deterministic platform effect receipt",
    max_retries = 0,
    timeout_secs = 30,
})

local results = crew:run()
assert(#results == 1, "owner diagnostic expected one result")
assert(results[1].success, "owner diagnostic task did not succeed")
assert(
    results[1].output == "ic007-platform-effect-recorded",
    "unexpected owner diagnostic receipt"
)

crew:ask_human({
    prompt = "Hold the IC-007 unkeyed owner?",
    choices = { "continue", "stop" },
    timeout_s = 300,
})
