-- Cross-run persistence demo
--
-- Run this project twice in a row:
--
--   ironcrew run examples/cross-run-persistence
--   ironcrew run examples/cross-run-persistence
--
-- The first run creates a support conversation and a two-agent negotiation.
-- Both are keyed by a stable `id`, so the second run *resumes* them — the
-- conversation sees its prior messages and the dialog picks up from the next
-- turn in the transcript instead of starting fresh.
--
-- See README.md in this folder for the full walkthrough.

local crew = Crew.new({
    goal = "Cross-run persistence demo",
    provider = "openai",
    model = "gpt-5.4-mini",
})

-- Single agent for the conversation side of the demo
crew:add_agent(Agent.new({
    name = "support_bot",
    goal = "Help the user debug intermittent API timeouts",
    system_prompt = [[
You are a patient support engineer. Remember everything the user has told
you across turns — the test is that you recall prior context from earlier
in the conversation.
]],
}))

-- Two agents for the dialog side
crew:add_agent(Agent.new({
    name = "optimist",
    goal = "Argue that the feature should ship this sprint",
    system_prompt = "Be concise. 2-3 sentences max. Cite one concrete reason per turn.",
}))
crew:add_agent(Agent.new({
    name = "pessimist",
    goal = "Argue that shipping this sprint is risky",
    system_prompt = "Be concise. 2-3 sentences max. Cite one concrete risk per turn.",
}))

-- ---------------------------------------------------------------------------
-- A) Resumable conversation
-- ---------------------------------------------------------------------------

local chat = crew:conversation({
    id       = "support-ticket-4821",   -- stable id = persistence key
    agent    = "support_bot",
    -- autosave defaults to true for persistent sessions
})

print("=== Conversation (id: " .. chat:id() .. ") ===")
print("persistent: " .. tostring(chat:is_persistent()))
print("")

-- On the first run this is empty; on subsequent runs it's the full history
-- that was loaded from disk.
local prior = chat:history()
if #prior > 1 then
    print("Resumed with " .. (#prior - 1) .. " prior message(s):")
    for i, msg in ipairs(prior) do
        if msg.role ~= "system" then
            print(string.format("  [%s] %s", msg.role, msg.content:sub(1, 80)))
        end
    end
    print("")
    -- Send a follow-up that references something only established on the
    -- first run — the bot will only recall it if the history was loaded.
    print("Follow-up turn:")
    print(chat:send("Remind me what error code we discussed and what I should try next."))
else
    print("No prior state — starting a fresh conversation.")
    print(chat:send(
        "I'm seeing intermittent 504 timeouts from our billing API, " ..
        "roughly every 3rd request. It started last Tuesday."
    ))
end
print("")

-- ---------------------------------------------------------------------------
-- B) Resumable dialog
-- ---------------------------------------------------------------------------

local debate = crew:dialog({
    id       = "ship-decision-q2",      -- stable id = persistence key
    agents   = { "optimist", "pessimist" },
    starter  = "Should we ship the billing rewrite this sprint?",
    max_turns = 6,
    -- autosave defaults to true
})

print("=== Dialog (id: " .. debate:id() .. ") ===")
print("persistent: " .. tostring(debate:is_persistent()))
local before = debate:turn_count()
print("turns already recorded: " .. before)
print("")

-- Run the dialog — if it was already completed in a prior run, this is a
-- no-op because next_index is past max_turns.
local transcript = debate:run()

print("Full transcript after this run (" .. #transcript .. " turns):")
for _, t in ipairs(transcript) do
    print(string.format("  [%s] %s",
        t.agent,
        t.content:sub(1, 100) .. (t.content:len() > 100 and "..." or "")))
end
print("")

if before == 0 then
    print("This was the first run — the dialog was played from turn 0.")
else
    print(string.format(
        "This run resumed from turn %d (the earlier run had already produced %d turns).",
        before, before
    ))
end
