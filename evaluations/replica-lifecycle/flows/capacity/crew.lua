-- IC-020 capacity fixture. The evaluator supplies a bounded loopback-only
-- OpenAI-compatible endpoint and never uses a live provider credential.

local provider_base_url = assert(
    env("IC020_PROVIDER_BASE_URL"),
    "IC020_PROVIDER_BASE_URL must be explicitly allowlisted"
)

local crew = Crew.new({
    goal = "Measure bounded provider and replica resource concurrency",
    provider = "openai",
    model = "ic020-loopback",
    api_key = "ic020-loopback-not-a-secret",
    base_url = provider_base_url,
    max_concurrent = 1,
})

crew:add_agent(Agent.new({
    name = "capacity-probe",
    goal = "Return the deterministic capacity receipt",
    capabilities = { "testing" },
    temperature = 0.0,
}))

crew:add_task({
    name = "provider-call",
    description = "Return the deterministic IC-020 capacity receipt.",
    expected_output = "The exact bounded loopback-provider receipt",
    max_retries = 0,
    timeout_secs = 30,
})

local results = crew:run()
assert(#results == 1, "capacity fixture expected one task result")
assert(results[1].output == "ic020-capacity-ok", "unexpected mock response")
