-- Anthropic Claude with extended thinking
--
-- Demonstrates Claude's chain-of-thought reasoning via thinking budget.
-- The thinking process is internal — only the final answer appears in output.
-- Requires ANTHROPIC_API_KEY in .env or environment.

local crew = Crew.new({
    goal = "Solve a complex reasoning problem using extended thinking",
    provider = "anthropic",
    model = "claude-sonnet-4-20250514",
    api_key = env("ANTHROPIC_API_KEY"),
    thinking_budget = 5000,
    stream = true,  -- watch the reasoning unfold in real-time on stderr
})

crew:add_agent(Agent.new({
    name = "thinker",
    goal = "Solve complex problems with careful step-by-step reasoning",
    capabilities = { "reasoning", "math", "logic" },
    system_prompt = "You are a careful problem solver. Think through each step methodically.",
}))

crew:add_task({
    name = "puzzle",
    description = [[
Three friends — Alice, Bob, and Carol — each have a different pet (cat, dog, fish)
and a different favorite color (red, blue, green).

Clues:
1. Alice does not have the cat.
2. The person with the dog likes blue.
3. Carol likes green.
4. Bob does not have the fish.

Who has which pet and what is their favorite color? Present the answer as a table.
]],
    agent = "thinker",
})

crew:run()
