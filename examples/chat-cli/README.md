# chat-cli — interactive REPL example

Minimal conversational flow for `ironcrew chat`. The crew declares a single
agent (`tutor`) and no tasks. The REPL drives the conversation — the flow
only exists to construct the crew and declare the agents.

## Prerequisites

- `OPENAI_API_KEY` exported in your environment (or in a `.env` file next
  to the project).

## Run

```sh
# Ephemeral session — history dies when you exit.
ironcrew chat examples/chat-cli --agent tutor

# Persistent session — pass --id to enable cross-run resume. The first
# invocation creates the record; any later `ironcrew chat examples/chat-cli
# --agent tutor --id my-session` re-opens it with the same history.
ironcrew chat examples/chat-cli --agent tutor --id my-session
```

## Slash commands

| Command           | Effect                                   |
| ----------------- | ---------------------------------------- |
| `/help`           | Show available commands                  |
| `/exit` / `/quit` | End the session                          |
| `/reset`          | Clear history (keep the system prompt)   |
| `/id`             | Print the session id                     |
| `/save`           | Persist the session now                  |
| `/history`        | Dump the full transcript                 |

## Notes

- Storage is governed by `IRONCREW_STORE` — JSON by default, SQLite or
  PostgreSQL if configured. Persisted records live in `.ironcrew/` next
  to `crew.lua`.
- The `IRONCREW_MODE` global is set to `"chat"` inside the REPL, so the
  common guard `if IRONCREW_MODE ~= "chat" then crew:run() end` lets you
  share a single `crew.lua` between `ironcrew run` and `ironcrew chat`.
