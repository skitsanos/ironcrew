-- IC-018 bounded mock-provider/tool profile. The evaluator removes live
-- provider credentials and exposes only its loopback fixture to this flow.

local provider_base_url = assert(
    env("IC018_PROFILE_PROVIDER_BASE_URL"),
    "IC018_PROFILE_PROVIDER_BASE_URL must be explicitly allowlisted"
)

local crew = Crew.new({
    goal = "Exercise one counted mock-provider tool effect",
    provider = "openai",
    model = "ic018-provider-tool",
    api_key = "ic018-loopback-not-a-secret",
    base_url = provider_base_url,
    max_concurrent = 1,
})

crew:add_agent(Agent.new({
    name = "profile-probe",
    goal = "Follow the bounded fixture's single http_request call",
    capabilities = { "testing" },
    tools = { "http_request" },
    temperature = 0.0,
}))

crew:add_task({
    name = "counted-effect",
    agent = "profile-probe",
    description = "Execute only the mock-requested counted HTTP effect.",
    expected_output = "The exact deterministic IC-018 provider/tool receipt",
    max_retries = 0,
    timeout_secs = 30,
})

local results = crew:run()
assert(#results == 1, "provider/tool profile expected one result")
assert(results[1].success, "provider/tool profile failed")
assert(results[1].output == "ic018-provider-tool-ok", "unexpected profile receipt")
