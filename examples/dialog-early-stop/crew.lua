-- Dialog early termination via `should_stop`
--
-- Demonstrates the `should_stop` callback on `crew:dialog({...})`. Two
-- negotiators try to reach an agreement on a simple trade. The dialog has
-- a generous `max_turns = 20` safety cap, but the custom termination
-- callback ends the dialog as soon as the most recent turn contains an
-- explicit agreement marker — which usually happens in 4-8 turns.
--
-- The callback can return:
--   - nil / false  → continue
--   - true         → stop (reason = "custom_stop")
--   - "reason"     → stop with the given reason string (recommended — it
--                    shows up in dialog:stop_reason() and the SSE event)
--
-- Requires: OPENAI_API_KEY (for gpt-5.4-mini)

local crew = Crew.new({
    goal = "Negotiate a trade until consensus is reached",
    provider = "openai",
    model = "gpt-5.4-mini",
})

crew:add_agent(Agent.new({
    name = "alice",
    goal = "Sell an antique clock for a fair price",
    system_prompt = [[
You are Alice, selling an antique mantel clock (estimated value $400-$600).
You want to get at least $450 but would prefer more. Be friendly but firm.
Keep each turn to 2-3 sentences.

When (and only when) you fully accept the buyer's current offer, say the
exact phrase "AGREED" in your reply. Do not use that word for any other
purpose — the dialog will end the moment it appears.
]],
}))

crew:add_agent(Agent.new({
    name = "bob",
    goal = "Buy the clock at a good price",
    system_prompt = [[
You are Bob, a collector interested in the antique clock. You are willing
to pay up to $520 but would love to pay less. Negotiate respectfully. Keep
each turn to 2-3 sentences.

When (and only when) you and Alice have settled on a specific price, say
the exact phrase "AGREED" in your reply. Do not use that word for any
other purpose — the dialog will end the moment it appears.
]],
}))

-- ---------------------------------------------------------------------------
-- The dialog — note the `should_stop` callback
-- ---------------------------------------------------------------------------

local dialog = crew:dialog({
    agents = { "alice", "bob" },
    starter = "Alice, please open the negotiation. Name your asking price for the clock and briefly justify it.",
    max_turns = 20, -- generous safety cap; early stop is the normal path
    should_stop = function(last_turn, transcript)
        -- Continue if the most recent speaker hasn't agreed
        if not last_turn.content:find("AGREED") then
            return false
        end
        -- Need at least 2 turns so we don't stop on the opening line
        if #transcript < 2 then
            return false
        end
        -- Optional: require the *other* party to also signal agreement
        -- somewhere in the transcript, not just the current speaker
        local other_agreed = false
        for _, t in ipairs(transcript) do
            if t.agent ~= last_turn.agent and t.content:find("AGREED") then
                other_agreed = true
                break
            end
        end
        if other_agreed then
            return "consensus reached"
        end
        return false
    end,
})

print("=== Negotiation ===")
print("(dialog will stop automatically once both parties say AGREED)")
print("")

local transcript = dialog:run()

for _, turn in ipairs(transcript) do
    print(string.format("--- Turn %d: %s ---", turn.index + 1, string.upper(turn.agent)))
    print(turn.content)
    print("")
end

-- Show why the dialog ended
local reason = dialog:stop_reason()
if reason then
    print(string.format("Dialog ended early after %d turns. Reason: %s", #transcript, reason))
else
    print(string.format("Dialog ran to max_turns (%d turns).", #transcript))
end
