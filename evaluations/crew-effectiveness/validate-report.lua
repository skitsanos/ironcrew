-- Validate a generated evaluator report with IronCrew's built-in JSON Schema
-- implementation. The evaluator copies base64 document chunks into the
-- validator's scoped _lib directory so process arguments stay small without
-- granting the top-level Lua VM filesystem capabilities.

assert(input and input.schema_chunks, "input.schema_chunks is required")
assert(input.report_chunks, "input.report_chunks is required")

local function load_document(label, count)
    assert(type(count) == "number" and count % 1 == 0, label .. " chunk count must be an integer")
    assert(count >= 1 and count <= 128, label .. " chunk count is outside the validator limit")

    local chunks = {}
    for index = 1, count do
        local module_name = string.format("validator_%s_%04d", label, index)
        local chunk = require(module_name)
        assert(type(chunk) == "string", module_name .. " must return a string")
        chunks[index] = chunk
    end
    return base64_decode(table.concat(chunks))
end

local schema = json_parse(load_document("schema", input.schema_chunks))
local report = load_document("report", input.report_chunks)
local validation = validate_json(report, schema)

if not validation.valid then
    for _, item in ipairs(validation.errors) do
        log("error", (item.path or "$") .. ": " .. item.message)
    end
    error("crew-effectiveness report does not match the supplied report schema")
end

print("Crew-effectiveness report schema validation passed.")
