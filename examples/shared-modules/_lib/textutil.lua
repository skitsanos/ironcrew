--[[
    _lib/textutil.lua — a shared Lua module.

    Modules placed under `_lib/` can be loaded with `require("textutil")` from
    crew.lua, sub-flows, and agent definitions. They execute in the SAME sandbox
    as the flow, so they have access to the usual globals (env, json_*,
    base64_*, the crypto helpers, http, regex, …).

    A module is plain Lua that returns a value — here, a table of functions.
    Returned modules are cached: requiring the same name twice runs this file
    once.
]]

local M = {}

--- Lowercase, hyphenated slug: "Hello, World!" -> "hello-world".
function M.slug(s)
    return (s:lower():gsub("[^%w]+", "-"):gsub("^%-+", ""):gsub("%-+$", ""))
end

--- Title-case each word: "hello world" -> "Hello World".
function M.titlecase(s)
    return (s:gsub("(%a)([%w']*)", function(first, rest)
        return first:upper() .. rest:lower()
    end))
end

--- Count whitespace-separated words.
function M.wordcount(s)
    local n = 0
    for _ in s:gmatch("%S+") do
        n = n + 1
    end
    return n
end

return M
