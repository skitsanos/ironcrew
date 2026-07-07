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

-- 1. Ask the human for the topic (free-form answer).
local topic = crew:ask_human({
    prompt    = "What should the announcement be about?",
    timeout_s = 300,
    default   = "our new CLI release",
})

crew:add_task({
    name = "draft",
    description = "Write a 3-sentence product announcement about: " .. topic,
    expected_output = "A short announcement draft",
})

local results = crew:run()
local draft = results[1] and results[1].output or "(no draft)"

print("=== Draft ===")
print(draft)

-- 2. Approval gate before "publishing" (constrained choices).
local decision = crew:ask_human({
    prompt    = "Publish this draft?",
    choices   = { "publish", "hold" },
    timeout_s = 300,
    default   = "hold",
})

if decision == "publish" then
    print("Published.")
else
    print("Held for review (answer was: " .. tostring(decision) .. ")")
end
