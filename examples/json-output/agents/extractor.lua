return {
    name = "extractor",
    goal = "Extract structured data from text into JSON format",
    system_prompt = "You are a data extraction specialist. You always return valid JSON matching the requested schema.",
    capabilities = { "extraction", "analysis", "json" },
    tools = { "file_write" },
    temperature = 0.1,
    response_format = {
        type = "json_schema",
        name = "company_analysis",
        schema = {
            type = "object",
            properties = {
                companies = {
                    type = "array",
                    items = {
                        type = "object",
                        properties = {
                            name = { type = "string" },
                            industry = { type = "string" },
                            strengths = {
                                type = "array",
                                items = { type = "string" },
                            },
                            market_position = {
                                type = "string",
                                enum = { "leader", "challenger", "follower", "niche" },
                            },
                        },
                        required = { "name", "industry", "strengths", "market_position" },
                        additionalProperties = false,
                    },
                },
                summary = { type = "string" },
            },
            required = { "companies", "summary" },
            additionalProperties = false,
        },
    },
}
