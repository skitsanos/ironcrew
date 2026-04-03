# Feature Request: Skipped tasks should not cause "failed" run status

## Problem

When a task is conditionally skipped via `condition` (e.g., `condition = "results.parse.hasUnknowns"`), the run status is reported as `"failed"` in the `run_complete` SSE event, even though all non-skipped tasks completed successfully.

## Current behavior

```
task_completed: parse (success)
task_skipped: resolve (condition false)
task_completed: analyze_roles (success)
task_completed: profile_speakers (success)
task_completed: verify (success)
run_complete: status = "failed"   ← wrong
```

The run is saved with `status: "Success"` in the runs endpoint, but the SSE event says `"failed"`.

## Expected behavior

```
run_complete: status = "success"
```

A run where all executed tasks succeeded and skipped tasks were intentionally skipped should be `"success"`, not `"failed"`.

## Workaround

The TranscriptIntel app ignores the status string and always tries to fetch results (unless status is `"aborted"`). This works but is fragile.

## Suggested fix

In the run completion logic, only set status to `"failed"` if a task actually failed (not skipped). Skipped tasks should be treated as neutral — they don't affect the overall run status.
