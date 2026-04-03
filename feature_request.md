# Feature Request: on_error tasks should only run when parent task fails

## Problem

Tasks with `on_error = "fallback_task"` run the fallback even when the parent task succeeds. Both the main task and the fallback execute, producing two results.

## Current behavior

```lua
crew:add_task({
    name = "summarize",
    on_error = "fallback_summary",
    description = "...",
})

crew:add_task({
    name = "fallback_summary",
    description = "...",
})
```

Both `summarize` and `fallback_summary` execute and both appear in `task_results` as successful.

## Expected behavior

`fallback_summary` should only execute if `summarize` fails. If `summarize` succeeds, `fallback_summary` should be skipped entirely (not appear in task_results, or marked as skipped).

## Workaround

The TranscriptIntel app filters out tasks whose names start with `fallback_` when processing results. This works but is fragile.
