-- Conversation mode — multi-turn chat with an agent
--
-- Unlike tasks (single-shot), a Conversation maintains its message history
-- across multiple send()/ask() calls. Useful for stateful dialogues, agent
-- testing, or interactive workflows inside a Lua script.

local crew = Crew.new({
    goal = "Demonstrate multi-turn conversation",
    provider = "anthropic",
    model = "claude-haiku-4-5-20251001",
})

crew:add_agent(Agent.new({
    name = "tutor",
    goal = "Teach Rust concepts patiently",
    capabilities = { "explanation", "teaching" },
    system_prompt = "You are a patient Rust tutor. Keep answers under 3 sentences.",
}))

-- Bind a conversation to the tutor agent
local conv = crew:conversation({
    agent = "tutor",
    max_history = 20,
})

-- Turn 1
print("Q1: What is ownership in Rust?")
local r1 = conv:send("What is ownership in Rust?")
print("A1: " .. r1)
print()

-- Turn 2 — the model has the context of turn 1
print("Q2: Can you give me a simple example?")
local r2 = conv:send("Can you give me a simple example?")
print("A2: " .. r2)
print()

-- Turn 3 — full response with metadata
print("Q3: What happens if I try to use a moved value?")
local r3 = conv:ask("What happens if I try to use a moved value?")
print("A3: " .. r3.content)
print("(history length: " .. r3.length .. ")")
print()

-- Inspect history
print("Total messages in conversation: " .. conv:length())

-- Reset and start fresh
conv:reset()
print("After reset: " .. conv:length() .. " messages (system prompt only)")
