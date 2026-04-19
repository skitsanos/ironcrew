-- sub-flow: project-team — three-agent research pipeline.
--
-- Invoked by the chat-http coordinator via the `brief_team` tool, which
-- calls `run_flow("subs/project-team/crew.lua", { topic = "..." })`.
-- Reads `input.topic`, runs researcher → analyst → writer, then returns
-- the finished brief as a plain string for the coordinator to surface.

local topic = (input and input.topic) or "small multi-agent teams"

local crew = Crew.new({
    goal     = "Produce short research briefs on demand",
    provider = "openai",
    model    = env("GEMINI_MODEL") or "gemini-2.5-flash",
    base_url = "https://generativelanguage.googleapis.com/v1beta/openai",
})

crew:add_agent(Agent.new({
    name        = "researcher",
    goal        = "Gather concise, accurate facts on the topic",
    temperature = 0.4,
}))

crew:add_agent(Agent.new({
    name        = "analyst",
    goal        = "Extract load-bearing insights and one real risk",
    temperature = 0.3,
}))

crew:add_agent(Agent.new({
    name        = "writer",
    goal        = "Package research and analysis into a short readable brief",
    temperature = 0.5,
}))

crew:add_task({
    name = "research",
    agent = "researcher",
    description = string.format(
        "List three concrete, verifiable facts about: %s\n" ..
        "Short bullets, under 120 words total.",
        topic
    ),
})

crew:add_task({
    name = "analyze",
    agent = "analyst",
    depends_on = { "research" },
    description = string.format(
        "Given these facts about \"%s\":\n\n${results.research.output}\n\n" ..
        "Identify two non-obvious insights and one real risk. Three bullets.",
        topic
    ),
})

crew:add_task({
    name = "write",
    agent = "writer",
    depends_on = { "research", "analyze" },
    description = string.format(
        "Write a six-sentence brief titled \"%s\" weaving together the " ..
        "researcher's facts and the analyst's insights + risk. Plain " ..
        "prose, no headings, no bullets.\n\n" ..
        "Facts:\n${results.research.output}\n\n" ..
        "Insights:\n${results.analyze.output}",
        topic
    ),
})

local results = crew:run()

-- Return the writer's output. The tool unwraps this and surfaces it
-- verbatim to the chat user.
for _, r in ipairs(results) do
    if r.task == "write" and r.success then
        return r.output
    end
end
return "project-team finished without producing a brief"
