-- IC-018 bounded large-result profile. This is a 64 KiB loopback mock result,
-- not live-provider or production traffic evidence.

local provider_base_url = assert(
    env("IC018_PROFILE_PROVIDER_BASE_URL"),
    "IC018_PROFILE_PROVIDER_BASE_URL must be explicitly allowlisted"
)

local crew = Crew.new({
    goal = "Retain one bounded 64 KiB provider result",
    provider = "openai",
    model = "ic018-large-result",
    api_key = "ic018-loopback-not-a-secret",
    base_url = provider_base_url,
    max_concurrent = 1,
})

crew:add_agent(Agent.new({
    name = "large-result-probe",
    goal = "Return exactly the bounded mock payload",
    capabilities = { "testing" },
    temperature = 0.0,
}))

crew:add_task({
    name = "large-result",
    agent = "large-result-probe",
    description = "Return the fixture's exact 64 KiB response.",
    expected_output = "Exactly 65,536 ASCII L bytes",
    max_retries = 0,
    timeout_secs = 30,
})

local results = crew:run()
assert(#results == 1, "large-result profile expected one result")
assert(results[1].success, "large-result profile failed")
assert(#results[1].output == 65536, "large-result profile returned the wrong byte count")
