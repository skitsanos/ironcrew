-- Declarative IC-008 platform fixture. Conversation turns use the bounded
-- OpenAI-compatible mock; bootstrap itself performs no provider or tool work.

local provider_base_url = assert(
    env("PLATFORM_CANARY_PROVIDER_BASE_URL"),
    "PLATFORM_CANARY_PROVIDER_BASE_URL must be explicitly allowlisted"
)

local crew = Crew.new({
    goal = "Verify shared PostgreSQL conversation rehydration",
    provider = "openai",
    model = "ic008-platform-canary",
    api_key = "ic008-platform-mock-key",
    base_url = provider_base_url,
    max_concurrent = 1,
})

crew:add_agent(Agent.new({
    name = "coordinator",
    goal = "Return the deterministic mock reply after one counted effect",
    system_prompt = "Use only the bounded IC-008 platform mock.",
    capabilities = { "testing" },
    tools = { "http_request" },
    temperature = 0.0,
}))
