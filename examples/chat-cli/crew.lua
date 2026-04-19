-- chat-cli — minimal conversational flow for `ironcrew chat`.
--
-- The same crew.lua works for `ironcrew run` (one-shot) and
-- `ironcrew chat` (REPL). The canonical pattern is to guard any
-- top-level `crew:run()` with `if IRONCREW_MODE ~= "chat" then ... end`
-- so chat mode only builds the crew and exits, letting the REPL drive
-- the conversation.

local crew = Crew.new({
    goal = "Teach Rust one step at a time",
    provider = "openai",
    model = "gpt-4.1-mini",
})

crew:add_agent(Agent.new({
    name = "tutor",
    goal = "Teach Rust concepts patiently",
    capabilities = { "explanation", "teaching" },
    system_prompt =
        "You are a patient Rust tutor. Keep answers under 3 sentences. " ..
        "When asked for an example, include a compilable snippet.",
}))

-- In chat mode, the REPL drives the conversation — nothing to do here.
-- In run mode, you could kick off a one-shot task instead:
-- if IRONCREW_MODE ~= "chat" then
--     crew:add_task({ name = "demo", agent = "tutor", description = "Say hi" })
--     crew:run()
-- end
