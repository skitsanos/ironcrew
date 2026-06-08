--[[
    report.lua — a sub-flow invoked by crew.lua via run_flow().

    A sub-flow resolves `require` from ITS OWN directory's `_lib`. Because this
    file sits next to crew.lua, both share the same `_lib/textutil.lua` — the
    shared helper lives in exactly one place.

    This sub-flow needs no LLM: it reads `input`, uses the shared module, and
    returns a plain table that run_flow() bridges back to the caller.
]]

local text = require("textutil")

local title = (input and input.title) or "untitled"

return {
    slug = text.slug(title),
    words = text.wordcount(title),
}
