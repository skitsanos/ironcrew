-- crew.lua — only the workflow logic
--
-- provider, model, max_concurrent, memory, and the model router are all
-- inherited from config.lua in this directory. Notice how this file contains
-- only the parts that change between crews: the goal, agents, and tasks.

local crew = Crew.new({
    goal = "Demonstrate config.lua defaults inheritance",
})

crew:add_agent(Agent.new({
    name = "assistant",
    goal = "Answer questions concisely",
    capabilities = { "writing", "explanation" },
    system_prompt = "You are a helpful assistant. Be concise.",
}))

crew:add_task({
    name = "explain",
    description = "In one sentence, what does config.lua do in IronCrew?",
    agent = "assistant",
})

crew:run()
