-- Hermetic fixture for subflow integration tests. Does not actually call
-- crew:run() because we don't bind a real LLM provider. The tests drive the
-- tool registry directly to validate that custom tools can invoke
-- `run_flow` from inside their sandbox VM.
local crew = Crew.new({
    goal = "subflow fixture",
    provider = "openai",
    model = "test-model",
    api_key = "test-key",
})

return crew
