-- chat-http — conversational flow that delegates real work to a sub-crew.
--
-- Demonstrates IronCrew's idiomatic multi-agent pattern from a chat
-- context: one user-facing agent (the coordinator) uses a custom tool
-- (`brief_team`) that invokes a three-agent sub-crew via the
-- sandbox-level `run_flow` primitive. No HTTP self-calls, no SSRF
-- bypass — all in-process.
--
-- Layout:
--   crew.lua                         (this file — single agent)
--   tools/brief_team.lua             (custom tool wrapping run_flow)
--   subs/project-team/crew.lua       (sub-crew: researcher → analyst → writer)
--
-- Endpoints:
--   POST   /flows/chat-http/conversations/{id}/start
--   POST   /flows/chat-http/conversations/{id}/messages
--   GET    /flows/chat-http/conversations/{id}/history
--   GET    /flows/chat-http/conversations/{id}/events   (SSE)
--   DELETE /flows/chat-http/conversations/{id}

local crew = Crew.new({
    goal     = "Run a small multi-agent research team on demand",
    provider = "openai",
    model    = env("GEMINI_MODEL") or "gemini-2.5-flash",
    base_url = "https://generativelanguage.googleapis.com/v1beta/openai",
})

crew:add_agent(Agent.new({
    name  = "coordinator",
    goal  = "Route user asks to the specialist team and present their work",
    tools = { "brief_team" },
    system_prompt = [[
You are the front desk for a small research team. The team has three
specialists (researcher, analyst, writer). You do NOT do their work
yourself — you delegate by calling the `brief_team` tool.

When to delegate:
  * the user asks for a research brief, comparison, or analysis
  * the user asks you to "look into" or "write up" a topic
  * the user asks for facts, pros/cons, or a short report on something

How to delegate:
  Call `brief_team` with a single argument:
      { "topic": "<the user's topic>" }
  The tool returns the finished brief as plain prose. Present it
  verbatim, prefaced by a single short line such as:
      "Here is the brief the team put together:"
  Do not rewrite, summarise, or add headings.

Do NOT call the tool for small-talk, greetings, or simple clarifying
questions. Answer those yourself in one or two sentences. Remember
earlier context silently — don't keep re-greeting the user.

If the tool errors, quote the error plainly. Don't invent a brief.
]],
}))
