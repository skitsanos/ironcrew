-- Custom Lua tool exposed to the chat-http coordinator.
--
-- Delegates the heavy lifting to a three-agent sub-crew
-- (researcher → analyst → writer) via the sandbox-level `run_flow`
-- primitive — no HTTP, no self-calls, same process.
--
-- Returns the sub-flow's final deliverable (a plain-text brief) that
-- the coordinator then presents to the user verbatim.

return {
    name = "brief_team",
    description =
        "Produce a short research brief on the user's topic by delegating " ..
        "to the project team (researcher → analyst → writer). Returns the " ..
        "finished brief as plain prose. Use this when the user asks for a " ..
        "write-up, research summary, pros/cons, or short report. Do NOT " ..
        "use it for small-talk or simple clarifying questions.",
    parameters = {
        topic = {
            type = "string",
            description = "Concise topic the team should produce a brief on.",
            required = true,
        },
    },
    execute = function(args)
        local topic = (args and args.topic) or ""
        if topic == "" then
            return "brief_team error: topic is required"
        end

        local output = run_flow("subs/project-team/crew.lua", { topic = topic })
        if type(output) == "string" and #output > 0 then
            return output
        end
        return "brief_team returned no output"
    end,
}
