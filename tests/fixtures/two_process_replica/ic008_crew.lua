-- Shared PostgreSQL conversation fixture for the IC-008 process gate.
-- The provider is a bounded in-test OpenAI-compatible server. The explicit
-- loopback override is injected only into the disposable acceptance processes.
local crew = Crew.new({
    goal = "verify shared conversation coordination",
    provider = "openai",
    model = "ic008-offline",
    base_url = env("IC008_PROVIDER_BASE_URL"),
    api_key = "ic008-local-mock-key",
    max_concurrent = 1,
})

crew:add_agent(Agent.new({
    name = "coordinator",
    goal = "Return deterministic mock-provider replies",
    system_prompt = "Use only the bounded IC-008 mock provider.",
    capabilities = { "testing" },
}))
