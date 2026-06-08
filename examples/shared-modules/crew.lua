--[[
    Shared Modules (require) Example

    Demonstrates loading shared Lua code with `require`, resolved only from this
    flow's `_lib/` directory. The same module is used by:

      * this top-level flow (crew.lua), and
      * a sub-flow (report.lua) invoked via run_flow().

    No LLM is needed — the example focuses on the module system and runs offline.

    Run:      ironcrew run examples/shared-modules
    Validate: ironcrew validate examples/shared-modules
]]

-- Resolved from examples/shared-modules/_lib/textutil.lua
local text = require("textutil")

local title = (input and input.title) or "Hello, IronCrew World!"

print("Top-level flow using require('textutil'):")
print("  title (titlecase): " .. text.titlecase(title))
print("  slug:              " .. text.slug(title))
print("  word count:        " .. tostring(text.wordcount(title)))
print()

-- The sub-flow requires the SAME shared module from this flow's _lib.
local report = run_flow("report.lua", { title = title })

print("Sub-flow report.lua (also via require('textutil')):")
print("  slug:       " .. report.slug)
print("  word count: " .. tostring(report.words))
print()

-- `require` returns a cached value: a second require runs textutil.lua zero
-- extra times and yields the same table.
local text_again = require("textutil")
print("require is cached: same table = " .. tostring(text_again == text))
