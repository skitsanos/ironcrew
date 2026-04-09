# Cross-Run Persistence

Demonstrates how `crew:conversation({...})` and `crew:dialog({...})` can
**resume across separate `ironcrew run` invocations** (or API requests)
by keying the session on a stable `id`.

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

The **second run** is the interesting one: the output explicitly reports
the number of prior messages loaded, and the follow-up turn references
something that was only said on the first run.

## How persistence is wired

### The `id` field is the persistence key

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
2. **Queries the store** for a prior record with that id.
3. On **hit**, loads the prior messages / transcript / turn counter /
   stop flag into the new session so it resumes exactly where it left off.
4. On **miss**, starts fresh. The record is written by the first autosave.

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
via `IRONCREW_STORE`:

| Backend     | Conversations                                   | Dialogs                                    |
|-------------|--------------------------------------------------|--------------------------------------------|
| `json` (default) | `.ironcrew/conversations/<id>.json`             | `.ironcrew/dialogs/<id>.json`              |
| `sqlite`    | `conversations` table in `.ironcrew/ironcrew.db` | `dialogs` table in `.ironcrew/ironcrew.db` |
| `postgres`  | `conversations` table (with prefix)              | `dialogs` table (with prefix)              |

Want to wipe a session for a fresh test run?

```bash
# JSON backend
rm .ironcrew/conversations/support-ticket-4821.json
rm .ironcrew/dialogs/ship-decision-q2.json

# SQLite backend
sqlite3 .ironcrew/ironcrew.db "DELETE FROM conversations WHERE id = 'support-ticket-4821'"
sqlite3 .ironcrew/ironcrew.db "DELETE FROM dialogs WHERE id = 'ship-decision-q2'"
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
- **Last-write-wins concurrency.** Two processes using the same session id
  at the same time will race. For single-user CLI flows this is almost never
  an issue, but don't use the same id for concurrent requests in a server
  hosted via `ironcrew serve` without your own external locking.
- **IDs are restricted.** Alphanumerics + `-`, `_`, `.`, 1-128 chars. Spaces,
  slashes, and SQL metacharacters are rejected at the Lua layer before they
  reach the store — use a UUID, a slug, or a deterministic hash of whatever
  business key you care about.
