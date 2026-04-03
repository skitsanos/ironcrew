# Feature Request: Parse task output JSON in condition evaluator

## Problem

The `condition` field on tasks evaluates Lua expressions against `results`, but `results.{task}.output` is a raw JSON **string**, not a parsed Lua table. This means conditions like:

```lua
condition = "results.parse.hasUnknowns"
```

don't work — `hasUnknowns` is a field inside the JSON string, not on the Lua table. It evaluates to `nil` (false).

## Current behavior (`src/engine/condition.rs`)

```rust
let _ = entry.set("output", result.output.clone()); // string, not parsed
```

The condition evaluator sets `output` as a string. Accessing nested fields fails silently.

## Requested behavior

Parse the `output` string as JSON and set it as a Lua table, so conditions can access nested fields:

```lua
-- This should work:
condition = "results.parse.hasUnknowns"

-- Where results.parse.output was: '{"hasUnknowns": true, "speakers": [...]}'
-- After parsing, results.parse.hasUnknowns == true
```

## Suggested implementation

In `evaluate_condition()`, after setting `output`, also parse it as JSON and merge fields into the entry table:

```rust
// Parse output as JSON and merge top-level fields into the entry
if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&result.output) {
    if let serde_json::Value::Object(map) = parsed {
        for (key, value) in map {
            let _ = entry.set(key, lua_value_from_json(&lua, &value));
        }
    }
}
```

This way both `results.parse.output` (raw string) and `results.parse.hasUnknowns` (parsed field) work.

## Use case

Speaker analysis flow with conditional resolution phase — Phase 2 should only run when Phase 1 detects unknown speakers (`hasUnknowns: true`). Without this, the condition always evaluates to false and the phase is skipped.

## Workaround

Remove `condition` and always run Phase 2. The resolver returns empty `resolutions: []` when no unknowns exist. Works but wastes an LLM call.
