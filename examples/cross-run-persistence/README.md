# Cross-Run Persistence

Demonstrates how `crew:conversation({...})` and `crew:dialog({...})` can
**resume across separate `ironcrew run` invocations** (or API requests)
by keying the session on a stable `id`.

The effective persistence key is `(flow_path, id)`: the project directory
name supplies `flow_path`, so two flows may safely use the same session id.

## What it shows

1. **A support conversation** (`support-ticket-4821`) that remembers a
   user's earlier report of 504 timeouts on the billing API. On the second
   run, the bot is asked about "the error code we discussed" and can only
   answer correctly because the prior history was loaded from disk.
2. **A two-agent debate** (`ship-decision-q2`) between an optimist and a
   pessimist arguing over a sprint ship decision. If the first run didn't
   play out all `max_turns`, the second run picks up from the next turn
   instead of starting over.

## Run it twice

```bash
# First run — both sessions are fresh
ironcrew run examples/cross-run-persistence

# Second run — both sessions resume from their saved state
ironcrew run examples/cross-run-persistence
```

Run this direct-Lua example with the default JSON store or SQLite. Persistent
PostgreSQL conversation turns intentionally require the keyed HTTP
`/messages` endpoint; a direct `conversation:send()` fails closed because it
does not own the shared durable turn claim. The dialog portion retains its
ordinary persisted-snapshot behavior.

The **second run** is the interesting one: the output explicitly reports
the number of prior messages loaded, and the follow-up turn references
something that was only said on the first run.

## How persistence is wired

### The `id` field is the persistence key within a flow

```lua
local chat = crew:conversation({
    id    = "support-ticket-4821",   -- stable id = persistence key
    agent = "support_bot",
})

local debate = crew:dialog({
    id      = "ship-decision-q2",
    agents  = { "optimist", "pessimist" },
    starter = "...",
    max_turns = 6,
})
```

When you supply `id`, IronCrew:

1. **Validates** the id (ASCII alphanumerics, `-`, `_`, `.`; 1-128 chars).
2. **Derives the flow scope** from the project directory name
   (`cross-run-persistence` here).
3. **Queries the store** for the `(flow_path, id)` pair.
4. On **hit**, loads the prior messages / transcript / turn counter /
   stop flag into the new session so it resumes exactly where it left off.
5. On **miss**, starts fresh. The record is written by the first autosave.

If you omit `id`, you get the pre-2.8 behavior: an auto-generated UUID,
no persistence, ephemeral session that disappears when the process exits.

### Autosave

Persistent sessions **autosave after every completed turn** by default.
Disable it with `autosave = false` and call `conversation:save()` /
`dialog:save()` manually when you want explicit control (e.g. to batch
many turns into one write).

```lua
local chat = crew:conversation({
    id       = "batched-chat",
    agent    = "support_bot",
    autosave = false,
})

chat:send("turn 1")
chat:send("turn 2")
chat:send("turn 3")
chat:save()   -- single write at the end
```

### Storage location

Sessions live in the same `StateStore` backend as run history, configured
via `IRONCREW_STORE`. Files and the default SQLite database are relative to
the flow project directory:

| Backend | Session storage |
|---|---|
| `json` (default) | `<project>/.ironcrew/conversations/<flow_path>/<id>.json` and `<project>/.ironcrew/dialogs/<flow_path>/<id>.json` |
| `sqlite` | `conversations` and `dialogs` in `<project>/.ironcrew/ironcrew.db`, uniquely keyed by `(flow_path, id)` |
| `postgres` | Prefixed `conversations` and `dialogs` tables, uniquely keyed by `(flow_path, id)`; persistent conversation turns use keyed HTTP rehydration rather than this direct-Lua example |

Want to wipe a session for a fresh test run?

```bash
cd examples/cross-run-persistence

# JSON backend
rm .ironcrew/conversations/cross-run-persistence/support-ticket-4821.json
rm .ironcrew/dialogs/cross-run-persistence/ship-decision-q2.json

# SQLite backend
sqlite3 .ironcrew/ironcrew.db \
  "DELETE FROM conversations WHERE flow_path = 'cross-run-persistence' AND id = 'support-ticket-4821'"
sqlite3 .ironcrew/ironcrew.db \
  "DELETE FROM dialogs WHERE flow_path = 'cross-run-persistence' AND id = 'ship-decision-q2'"
```

Or from Lua:

```lua
chat:delete()
debate:delete()
```

## Lua API for session state

### Conversation

| Method                | Returns | Description |
|-----------------------|---------|-------------|
| `conv:id()`           | string  | The stable session id (user-supplied or auto-UUID) |
| `conv:is_persistent()`| bool    | `true` if `id` was supplied and autosave/resume is active |
| `conv:save()`         | —       | Explicit save (use when `autosave = false`) |
| `conv:delete()`       | —       | Remove the persisted record |
| `conv:history()`      | table   | Current message history (unchanged from ephemeral mode) |

### Dialog

| Method                  | Returns | Description |
|-------------------------|---------|-------------|
| `dialog:id()`           | string  | The stable dialog id |
| `dialog:is_persistent()`| bool    | `true` if persisted |
| `dialog:save()`         | —       | Explicit save |
| `dialog:delete()`       | —       | Remove the persisted record |
| `dialog:turn_count()`   | int     | Number of completed turns (reflects prior runs) |
| `dialog:transcript()`   | table   | Full transcript including prior runs' turns |

## Prerequisites

- An OpenAI API key in `.env` (`OPENAI_API_KEY=sk-...`)
- Default provider `gpt-5.4-mini` — change the `model` field in `crew.lua`
  if you want to use a different one

## Gotchas

- **Agent list must match on resume.** If you save a dialog with
  `agents = { "alice", "bob" }` and try to resume it with
  `agents = { "alice", "carol" }`, the resume fails with a clear validation
  error. Dialogs are tied to their participant set.
- **Concurrent writers are rejected.** Session records carry an optimistic
  revision. Persistent conversations also carry a UUID incarnation and
  source/definition fingerprints. PostgreSQL HTTP messages claim that exact
  incarnation and revision before cold rehydration; stale writers and
  delete/recreate ABA reuse fail closed. HTTP message retries follow the
  idempotency contract in `docs/rest-api.md`.
- **Flow scope is part of identity.** Reusing `support-ticket-4821` in a
  different flow creates a distinct session; it does not resume this one.
- **IDs are restricted.** Alphanumerics + `-`, `_`, `.`, 1-128 chars. Spaces,
  slashes, and SQL metacharacters are rejected at the Lua layer before they
  reach the store — use a UUID, a slug, or a deterministic hash of whatever
  business key you care about.
