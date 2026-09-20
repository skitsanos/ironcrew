-- Per-agent reasoning_effort — the same task at three effort levels, side by side.
--
-- The crew sets a default effort; each agent overrides it for its own requests.
-- Independent tasks run in parallel, so one run gives a direct comparison of
-- how much longer the model thinks (duration, completion tokens — which include
-- reasoning tokens on the Responses API) and how the answer changes.
--
-- Effort is a Responses API feature. On the plain `openai` Chat Completions
-- provider an explicit agent effort is forwarded as `reasoning_effort`, except
-- that a tool-using agent on gpt-5.6-luna may only set "none". The `anthropic`
-- provider rejects the option (use the crew-level `thinking_budget` instead).

local crew = Crew.new({
    goal = "Compare reasoning effort levels on one problem",
    provider = "openai-responses",
    model = env("OPENAI_MODEL") or "gpt-5.6-luna",
    reasoning_effort = "low",     -- crew default: any agent without its own value
    reasoning_summary = "auto",   -- captured in the run record under `reasoning`
})

-- A verifiable problem (the correct answer is 36), so effort levels can be
-- compared on correctness as well as on cost and latency.
local PUZZLE = [[
How many positive integers less than 1000 have digits that sum to exactly 20?
Work it out carefully and finish with a single line of the form "Answer: N".
]]

for _, effort in ipairs({ "low", "medium", "high" }) do
    crew:add_agent(Agent.new({
        name = "solver_" .. effort,
        goal = "Solve logic puzzles and explain the reasoning",
        reasoning_effort = effort,   -- per-agent override of the crew default
    }))
    crew:add_task({
        name = "solve_" .. effort,
        description = PUZZLE,
        agent = "solver_" .. effort,
    })
end

local results = crew:run()

-- crew:run() returns an array of result entries; index them by task name.
local by_task = {}
for _, entry in ipairs(results) do
    by_task[entry.task] = entry
end

local function final_answer(text)
    return text:match("Answer:%s*([%d,]+)") or "?"
end

print("")
print(string.format("%-8s %10s %8s %12s %8s  %s", "effort", "duration", "prompt", "completion", "answer", "(correct: 36)"))
for _, effort in ipairs({ "low", "medium", "high" }) do
    local r = by_task["solve_" .. effort]
    local usage = r.token_usage or {}
    print(string.format(
        "%-8s %8d ms %8d %12d %8s",
        effort,
        r.duration_ms,
        usage.prompt_tokens or 0,
        usage.completion_tokens or 0,
        final_answer(r.output)
    ))
end
print("")
print("Completion tokens include the model's reasoning tokens; the reasoning")
print("summaries themselves are in the run record (`ironcrew run ... --json`).")
