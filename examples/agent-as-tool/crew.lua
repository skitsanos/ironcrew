--[[
    agent-as-tool — orchestrator delegates per turn to specialists.

    Demonstrates the v2.14.0 `agent__<name>` primitive. The coordinator
    lists two specialists in its `tools` array; its LLM invokes them
    like any other tool. Each invocation runs the specialist's own
    tool-call loop and returns a single string.

    Layout:
      crew.lua   (this file — coordinator + researcher + writer)

    Run:
      ironcrew run examples/agent-as-tool
      ironcrew run examples/agent-as-tool --input '{"question":"why does Rust have both Arc and Rc?"}'

    Compare with examples/chat-http/, which uses run_flow to delegate
    to a whole sub-crew pipeline. Pick agent-as-tool when you want
    per-turn delegation of individual questions; pick run_flow when
    you need a multi-step flow with task dependencies.
]]

local crew = Crew.new({
    goal     = "Answer research questions by delegating to specialists",
    provider = "openai",
    model    = env("GEMINI_MODEL") or "gemini-2.5-flash",
    base_url = env("GEMINI_BASE_URL")
               or "https://generativelanguage.googleapis.com/v1beta/openai",
})

crew:add_agent(Agent.new({
    name  = "coordinator",
    goal  = "Route research asks to the right specialist",
    tools = { "agent__researcher", "agent__writer" },
    system_prompt = [[
You coordinate two specialists, each callable as a tool:
  * agent__researcher — give it a focused question, get concise facts back.
  * agent__writer     — give it bullets or raw facts, get a paragraph back.

Strategy for any user question:
  1. Call agent__researcher with a single-sentence prompt naming the topic.
  2. Take the returned facts, pass them to agent__writer with an
     instruction like "turn these into a short paragraph".
  3. Return the writer's paragraph verbatim.

Each specialist tool takes one argument:
  { "prompt": "<a focused instruction>" }

Do not attempt to answer research questions yourself.
]],
}))

crew:add_agent(Agent.new({
    name        = "researcher",
    goal        = "Gather concrete, verifiable facts on the requested topic",
    temperature = 0.3,
    system_prompt = [[
You receive a specific research question. Reply with three or four
bulleted facts, under 80 words total. No introductions, no wrap-up.
]],
}))

crew:add_agent(Agent.new({
    name        = "writer",
    goal        = "Turn research bullets into one readable paragraph",
    temperature = 0.5,
    system_prompt = [[
You receive a set of bullets or raw facts. Turn them into a single
paragraph (3–5 sentences), plain prose, no headings, no bullets.
Preserve the facts exactly as stated.
]],
}))

local question = (input and input.question)
                 or "Why does Rust have both Arc and Rc?"

crew:add_task({
    name        = "answer",
    agent       = "coordinator",
    description = question,
})

local results = crew:run()

for _, r in ipairs(results) do
    print("--- " .. r.task .. " (by " .. r.agent .. ") ---")
    print(r.output)
    print()
end
