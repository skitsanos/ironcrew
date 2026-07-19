--[[
    human-approval — agent-initiated questions and tool approval gates

    Unlike examples/ask-human, the flow does not choose when to ask. The
    release_manager agent opts into the `ask_human` tool and decides during
    its turn that it needs an operator's release channel. Both file writes
    are independently protected by `require_approval`.

    Run: ironcrew run examples/human-approval
]]

local crew = Crew.new({
    goal = "Prepare a human-approved release plan",
    provider = "openai",
    model = env("OPENAI_MODEL") or "gpt-4o-mini",
    base_url = env("OPENAI_BASE_URL"),
    require_approval = { "file_write" },
})

crew:add_agent(Agent.new({
    name = "release_manager",
    goal = "Prepare release artifacts while keeping the operator in control",
    tools = { "ask_human", "file_write" },
    temperature = 0.2,
    system_prompt = [[
You prepare a release plan, but the human operator owns the decision.

Follow this sequence exactly:
1. Call ask_human once. Ask which release channel to target, with choices
   "canary", "stable", and "hold" and a 300-second timeout.
2. If the answer is "hold", explain that no files were written and stop.
3. Otherwise, call file_write to create output/release-plan.md with a short
   checklist naming the selected channel.
4. Call file_write again to create output/release-metadata.json containing
   valid JSON with the selected channel and a "pending" status.
5. Summarize what happened. If either write is denied, do not retry it and do
   not claim that its file exists.

Do not invent the operator's answer and do not skip either tool type.
]],
}))

crew:add_task({
    name = "prepare_release",
    agent = "release_manager",
    description = [[
Ask the operator which channel this release should use, then prepare the two
release artifacts described in your instructions. Keep the final response
under 100 words and accurately report denied operations.
    ]],
    expected_output = "A concise, truthful summary of the human-controlled release preparation",
    -- This budget counts active model/tool work. Time suspended on either
    -- ask_human or an approval gate is excluded by the task runner.
    timeout_secs = 120,
})

local results = crew:run()

for _, result in ipairs(results) do
    local status = result.success and "OK" or "FAILED"
    print(string.format("[%s] %s (by %s)", status, result.task, result.agent))
    print(result.output)
end
