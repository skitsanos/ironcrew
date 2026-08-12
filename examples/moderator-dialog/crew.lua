-- Moderator-Driven Dialog
--
-- Demonstrates two approaches to moderator-driven agent selection:
--
-- 1. `turn_selector` callback — a Lua function that runs automatically during
--    dialog:run(). Can call async methods (like moderator:send()) to let an
--    LLM pick the next speaker based on the discussion so far.
--
-- 2. `dialog:next_turn_from(agent_name)` — explicit control in a manual loop,
--    where your Lua code decides who speaks next each turn.
--
-- Run the automatic selector (default):
--   ironcrew run examples/moderator-dialog
-- Run the explicit manual sequence:
--   ironcrew run examples/moderator-dialog --input '{"mode":"manual"}'
--
-- Topic: a product launch go/no-go decision with three stakeholders.
--
-- Requires: OPENAI_API_KEY (for gpt-5.4-mini)

local crew = Crew.new({
    goal = "Moderator-driven product launch debate",
    provider = "openai",
    model = "gpt-5.4-mini",
})

-- The three stakeholders
crew:add_agent(Agent.new({
    name = "product",
    goal = "Advocate for launching to capture market momentum",
    system_prompt = [[
You are the Product Manager. You focus on market timing, user demand, and
competitive positioning. You want to launch because you see a window of
opportunity. Cite specific reasoning. Keep turns to 2-3 sentences.
]],
}))

crew:add_agent(Agent.new({
    name = "engineering",
    goal = "Raise technical risks and quality concerns",
    system_prompt = [[
You are the Engineering Lead. You focus on tech debt, stability, test
coverage, and the risk of post-launch hotfixes. You want to ship when it's
ready, not before. Cite specific reasoning. Keep turns to 2-3 sentences.
]],
}))

crew:add_agent(Agent.new({
    name = "customer_success",
    goal = "Represent the voice of existing customers and support burden",
    system_prompt = [[
You are the Customer Success Lead. You focus on support readiness, migration
risk for existing users, and whether training materials are ready. You want
to protect the customer experience. Cite specific reasoning. Keep turns to
2-3 sentences.
]],
}))

-- The moderator agent (separate from the dialog participants)
crew:add_agent(Agent.new({
    name = "moderator",
    goal = "Steer a multi-stakeholder discussion toward actionable decisions",
    system_prompt = [[
You are a neutral discussion facilitator for a product team. Your job is to
pick which stakeholder should speak NEXT based on the conversation so far.

Rules:
  - Avoid letting one person dominate (no one speaks twice in a row)
  - When someone raises a concern, give the most relevant responder a chance
  - When the discussion loops, redirect to someone who hasn't spoken recently

You MUST reply with ONLY the agent name (one of the names provided). Nothing else.
No explanation, no punctuation, just the name.
]],
}))

-- Create a moderator conversation (used inside the turn_selector callback)
local mod_conv = crew:conversation({ agent = "moderator" })

-- Format a transcript into text for the moderator
local function format_transcript(transcript, agents)
    local parts = {}
    for _, t in ipairs(transcript) do
        table.insert(parts, "[" .. t.agent .. "]: " .. t.content)
    end
    return "Discussion so far:\n" ..
        (table.concat(parts, "\n\n") or "(just started)") ..
        "\n\nParticipants: " .. table.concat(agents, ", ") ..
        "\n\nWho should speak next? Reply with ONLY the agent name."
end

-- ╔══════════════════════════════════════════════════════════════════════╗
-- ║  Approach 1: turn_selector callback (automatic, used by run())    ║
-- ╚══════════════════════════════════════════════════════════════════════╝

local TOPIC = [[
Our SaaS product v2.0 has been in beta for 3 weeks. We have a launch window
next Tuesday (marketing is ready, press embargo lifts). However, there are
2 known P1 bugs in the migration path, test coverage is at 68%, and the
support team hasn't finished the new documentation.

Should we launch next Tuesday, delay by 2 weeks, or do a limited launch
to a subset of users? Discuss.
]]

local mode = (input and input.mode) or "selector"
if mode ~= "selector" and mode ~= "manual" then
    error("input.mode must be 'selector' or 'manual'")
end

local dialog_options = {
    agents = { "product", "engineering", "customer_success" },
    starter = TOPIC,
    max_turns = 6,
}

if mode == "selector" then
    -- The moderator picks who speaks next via LLM reasoning.
    dialog_options.turn_selector = function(transcript, agents)
        if #transcript == 0 then
            -- First speaker: let the person with the strongest opinion go first
            return "product"
        end
        -- Ask the moderator agent who should speak next
        local prompt = format_transcript(transcript, agents)
        local next_name = mod_conv:send(prompt)
        -- Trim whitespace and return
        return next_name:match("^%s*(.-)%s*$")
    end
end

local dialog = crew:dialog(dialog_options)

print("=== Moderator-Driven Dialog ===")
print(mode == "selector"
    and "(moderator agent picks who speaks next each turn)"
    or "(Lua explicitly picks each speaker with next_turn_from)")
print("")

local transcript
if mode == "manual" then
    -- Approach 2: the flow owns speaker selection explicitly. `next_turn_from`
    -- executes exactly one turn and returns it (or nil when the dialog ended).
    local speaker_sequence = {
        "product",
        "engineering",
        "customer_success",
        "product",
        "engineering",
        "customer_success",
    }
    for _, speaker in ipairs(speaker_sequence) do
        if dialog:next_turn_from(speaker) == nil then
            break
        end
    end
    transcript = dialog:transcript()
else
    -- Approach 1: `dialog:run()` invokes turn_selector before every turn.
    transcript = dialog:run()
end

for _, turn in ipairs(transcript) do
    print(string.format("--- Turn %d: %s ---", turn.index + 1, string.upper(turn.agent)))
    print(turn.content)
    print("")
end

-- Show the moderator's internal reasoning (what it decided and why)
if mode == "selector" then
    print("=== Moderator decisions (conversation history) ===")
    local mod_history = mod_conv:history()
    for i, msg in ipairs(mod_history) do
        if msg.role == "assistant" then
            print(string.format("  Turn %d → chose: %s", math.ceil((i - 1) / 2), msg.content))
        end
    end
    print("")
end

print(string.format(
    "Dialog complete in %s mode: %d turns, %d participants",
    mode,
    #transcript,
    #dialog:agents()
))
