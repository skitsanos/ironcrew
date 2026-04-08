-- Roundtable — multi-party (3+ agent) dialog
--
-- Demonstrates crew:dialog({}) with the new `agents` array form. Three
-- agents take turns in round-robin order, each seeing the others' previous
-- turns from their own first-person perspective (their turns become
-- "assistant", everyone else becomes "user" with [name]: prefixes).
--
-- Topic: a hypothetical engineering decision. Three perspectives:
--   * optimist  — focuses on upside, momentum, learning value
--   * pessimist — focuses on risks, costs, sunk costs, opportunity cost
--   * realist   — focuses on data, falsifiable claims, what can be tested
--
-- Cheap defaults: gpt-5.4-mini.

local crew = Crew.new({
    goal = "3-way engineering decision roundtable",
    provider = "openai",
    model = "gpt-5.4-mini",
})

crew:add_agent(Agent.new({
    name = "optimist",
    goal = "Argue for taking action; emphasize upside, momentum, and learning value",
    capabilities = { "argumentation", "vision" },
    system_prompt = [[
You are an optimistic engineering lead. You make the case to ACT — to ship,
to invest, to try. You focus on upside, momentum, optionality, what we'll
learn, and the cost of inaction. Cite specific reasoning, not just enthusiasm.

REQUIRED FORMAT for every turn:
  1. Make your point in 2-3 sentences.
  2. Address one specific thing another participant said when applicable.
  3. End with a single line: "MY ACTIONABLE: <one concrete thing we should do this week>"

Keep responses tight. No hedging.
]],
}))

crew:add_agent(Agent.new({
    name = "pessimist",
    goal = "Argue against; surface risks, hidden costs, and reasons to wait",
    capabilities = { "argumentation", "risk_analysis" },
    system_prompt = [[
You are a skeptical engineering lead. You make the case to WAIT or NOT
proceed. You focus on risks, hidden costs, opportunity cost, sunk-cost
fallacies, and what could go wrong. Cite specific concerns, not vague worry.

REQUIRED FORMAT for every turn:
  1. Make your point in 2-3 sentences.
  2. Address one specific thing another participant said when applicable.
  3. End with a single line: "MY KILL CRITERION: <one specific signal that would make me say no>"

Keep responses tight. No hedging.
]],
}))

crew:add_agent(Agent.new({
    name = "realist",
    goal = "Pull the discussion toward testable claims and data",
    capabilities = { "analysis", "skepticism" },
    system_prompt = [[
You are a pragmatic engineering lead. Your job is to PULL THE DISCUSSION
TOWARD TESTABLE CLAIMS. When others make sweeping assertions, ask what
specific signal would confirm or refute them. Don't take sides — surface
what we can actually measure.

REQUIRED FORMAT for every turn:
  1. Identify one specific claim made by either the optimist or pessimist.
  2. Reframe it as a falsifiable question we could answer in under a week.
  3. End with a single line: "MY EXPERIMENT: <one concrete thing we could measure this week>"

Keep responses tight. Don't editorialize.
]],
}))

-- The decision under discussion
local TOPIC = [[
We have a 6-engineer team. We've been building feature X for 5 weeks. It's
~70% done but the latest user research shows lukewarm interest from our
target segment. We have two competing items in the backlog (a paying-customer
churn fix that needs ~2 weeks, and a brand new prototype for a different
segment that would take ~4 weeks).

Should we finish feature X, pivot to the churn fix, or pivot to the new
prototype? Discuss with each turn building on the previous speakers.
]]

local roundtable = crew:dialog({
    agents = { "optimist", "pessimist", "realist" },
    starter = TOPIC,
    max_turns = 6, -- 2 turns per agent
    starting_speaker = "realist", -- start by framing what we know
})

print("=== Roundtable: 3-agent decision dialog ===")
print("")

local transcript = roundtable:run()

for _, turn in ipairs(transcript) do
    print(string.format("--- Turn %d: %s ---", turn.index + 1, string.upper(turn.agent)))
    print(turn.content)
    print("")
end

print(string.format("Total turns: %d", #transcript))
print(string.format("Participants: %s", table.concat(roundtable:agents(), ", ")))
