-- Custom Lua tool that delegates its work to a sub-flow via `run_flow`.
-- Used by tests/subflow_test.rs to verify that tools hosted in the
-- LuaScriptTool sandbox can invoke sub-flows cleanly.
return {
    name = "delegator",
    description = "Delegate arithmetic to subs/math/math.lua",
    parameters = {
        x = { type = "integer", description = "input value", required = true },
    },
    execute = function(args)
        local result = run_flow("subs/math/math.lua", { x = args.x })
        if result and result.got then
            return "got=" .. tostring(result.got)
        end
        return "no-result"
    end,
}
