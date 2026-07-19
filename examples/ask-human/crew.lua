--[[
    ask-human — mid-run Human-in-the-Loop

    The flow suspends on crew:ask_human() until a human answers:
      - `ironcrew run .`  → prompt on the terminal (stderr), answer on stdin
      - `ironcrew serve`  → SSE emits human_input_requested; answer with
            POST /flows/ask-human/answer/{run_id}
            {"question_id": "...", "answer": "publish"}
        (pending questions: GET /flows/ask-human/questions/{run_id})

    Unattended runs don't hang: with no TTY (piped stdin, CI) or when the
    timeout expires, ask_human returns `default` — so this same flow works
    attended and unattended.
]]

local crew = Crew.new({
    goal = "Draft a short product announcement",
    provider = "openai",
    model = env("OPENAI_MODEL") or "gpt-4o-mini",
    base_url = env("OPENAI_BASE_URL"),
})

crew:add_agent(Agent.new({
    name = "announcer",
    goal = "Write concise, accurate product announcements",
    capabilities = { "writing", "editing" },
    temperature = 0.4,
}))

-- Define the task before asking so `ironcrew graph` can capture the complete
-- workflow without invoking the live input bridge. The human's answer is put
-- in shared memory before the task runs.
crew:add_task({
    name = "draft",
    agent = "announcer",
    description = [[
Write a three-sentence product announcement about the topic stored in shared
memory as `announcement_topic`. Be specific and avoid unsupported claims.
]],
    expected_output = "A short announcement draft",
})

-- 1. Ask for the topic unless the caller supplied `input.topic`. Keyed HTTP
-- runs persist their acceptance before Lua setup, so this pre-run question is
-- answerable through the normal questions/answer endpoints too. The
-- setup-only branch is hermetic for CI.
local topic
if input and input.setup_only == true then
    topic = "the offline CI setup probe"
elseif input and type(input.topic) == "string" and input.topic ~= "" then
    topic = input.topic
else
    topic = crew:ask_human({
        prompt    = "What should the announcement be about?",
        timeout_s = 300,
        default   = "our new CLI release",
    })
end

crew:memory_set("announcement_topic", topic)

-- CI uses this branch to exercise the real Lua setup without a prompt or LLM
-- call. Normal runs never set this input.
if input and input.setup_only == true then
    print("ask-human setup probe passed")
    return
end

local results = crew:run()
local draft = results[1] and results[1].output or "(no draft)"

print("=== Draft ===")
print(draft)

-- 2. Human approval checkpoint before "publishing" (constrained choices).
local decision = crew:ask_human({
    prompt    = "Publish this draft?",
    choices   = { "publish", "hold" },
    timeout_s = 300,
    default   = "hold",
})

if decision == "publish" then
    print("Published.")
else
    -- Do not echo free-form human input: HTTP answers may contain secrets,
    -- and print output is replayed as an SSE log event in server mode.
    print("Held for review.")
end
