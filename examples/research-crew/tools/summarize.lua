return {
    name = "summarize",
    description = "Summarize text to a short version",
    parameters = {
        text = { type = "string", description = "Text to summarize", required = true },
    },
    execute = function(args)
        local text = args.text or ""
        if #text > 200 then
            return text:sub(1, 200) .. "..."
        end
        return text
    end,
}
