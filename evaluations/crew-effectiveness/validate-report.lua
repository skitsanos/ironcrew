-- Validate a generated evaluator report with IronCrew's built-in JSON Schema
-- implementation. The Python evaluator injects both documents as strings so
-- the top-level Lua VM does not need filesystem capabilities.

assert(input and input.schema, "input.schema is required")
assert(input.report, "input.report is required")

local schema = json_parse(input.schema)
local report = input.report
local validation = validate_json(report, schema)

if not validation.valid then
    for _, item in ipairs(validation.errors) do
        log("error", (item.path or "$") .. ": " .. item.message)
    end
    error("crew-effectiveness report does not match report-v1.schema.json")
end

print("Crew-effectiveness report schema validation passed.")
