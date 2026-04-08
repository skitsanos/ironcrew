-- config.lua — project-wide defaults for Crew.new()
--
-- This file is loaded automatically when the project runs. Any field set here
-- becomes a default that crew.lua can override on a per-crew basis.
-- Fields explicitly set in Crew.new() always win over config.lua.
--
-- All Crew.new() options are supported:
--   provider, model, base_url, max_concurrent, memory, max_memory_items,
--   max_memory_tokens, stream, models (router), prompt_cache_key,
--   prompt_cache_retention, thinking_budget, server_tools,
--   web_search_max_uses, reasoning_effort, reasoning_summary,
--   web_search_context_size, file_search_vector_store_ids,
--   file_search_max_results.

return {
    provider = "anthropic",
    model = "claude-haiku-4-5-20251001",
    max_concurrent = 4,
    memory = "ephemeral",

    -- Default model router applied across all crews in this project.
    -- crew.lua can still override individual purposes.
    models = {
        task_execution = "claude-haiku-4-5-20251001",
        collaboration = "claude-haiku-4-5-20251001",
        collaboration_synthesis = "claude-sonnet-4-5-20250929",
    },
}
