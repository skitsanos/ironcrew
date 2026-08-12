# chat-http — HTTP conversation example

Demonstrates IronCrew's idiomatic multi-agent-from-chat pattern:

  * one user-facing agent (`coordinator`) drives `crew:conversation()`,
  * a custom tool (`tools/brief_team.lua`) calls the sandbox-level
    `run_flow(...)` primitive,
  * `subs/project-team/crew.lua` runs a three-agent pipeline
    (researcher → analyst → writer) and returns a finished brief,
  * the coordinator presents it to the chat user.

No HTTP self-calls, no SSRF bypass, no `?wait=1` — everything stays
in-process.

## Layout

```
examples/chat-http/
├── crew.lua                      # coordinator agent (single user-facing)
├── tools/
│   └── brief_team.lua            # custom tool wrapping run_flow
└── subs/
    └── project-team/
        └── crew.lua              # 3-agent sub-crew, returns the brief
```

## Boot the server

```sh
export OPENAI_API_KEY=sk-...
export IRONCREW_API_TOKEN=ironcrew-local-development-token-0001 # 32+ bytes

ironcrew serve --flows-dir examples --host 127.0.0.1 --port 3000
```

## Start a session

```sh
curl -sX POST http://127.0.0.1:3000/flows/chat-http/conversations/demo/start \
     -H "Authorization: Bearer $IRONCREW_API_TOKEN" \
     -H 'Content-Type: application/json' \
     -d '{ "agent": "coordinator", "max_history": 50 }' | jq
```

## Send a message

```sh
curl -sX POST http://127.0.0.1:3000/flows/chat-http/conversations/demo/messages \
     -H "Authorization: Bearer $IRONCREW_API_TOKEN" \
     -H 'Idempotency-Key: demo-message-1' \
     -H 'Content-Type: application/json' \
     -d '{ "content": "Hi! What can you help me with?" }' | jq
```

## Tail live events

This command applies to the default JSON store and SQLite. Conversation SSE is
process-local and does not support `Last-Event-ID` replay. With PostgreSQL,
`/events` returns `409` for an existing conversation; use durable `/history`
for recovery instead.

```sh
curl -sN http://127.0.0.1:3000/flows/chat-http/conversations/demo/events \
     -H "Authorization: Bearer $IRONCREW_API_TOKEN"
```

## Read stored history

```sh
curl -s http://127.0.0.1:3000/flows/chat-http/conversations/demo/history \
     -H "Authorization: Bearer $IRONCREW_API_TOKEN" | jq
```

## List conversations for this flow

```sh
curl -s 'http://127.0.0.1:3000/flows/chat-http/conversations?limit=10' \
     -H "Authorization: Bearer $IRONCREW_API_TOKEN" | jq
```

## Delete a session

```sh
curl -sX DELETE http://127.0.0.1:3000/flows/chat-http/conversations/demo \
     -H "Authorization: Bearer $IRONCREW_API_TOKEN" | jq
```

## Environment knobs

| Variable                                 | Default | Purpose                                         |
| ---------------------------------------- | ------- | ----------------------------------------------- |
| `OPENAI_API_KEY`                         | —       | OpenAI credential used by the coordinator and sub-crew |
| `OPENAI_MODEL`                           | `gpt-5.6-luna` | Model used by both flows when allowlisted       |
| `IRONCREW_ENV_ALLOWLIST`                 | —       | Include `OPENAI_MODEL` to expose that override to sandboxed Lua |
| `IRONCREW_API_TOKEN`                     | —       | Bearer token required for the protected routes  |
| `IRONCREW_MAX_ACTIVE_CONVERSATIONS`      | 8       | Simultaneous in-memory session cap              |
| `IRONCREW_CHAT_SESSION_IDLE_SECS`        | 1800    | Idle eviction threshold                         |
| `IRONCREW_CONVERSATIONS_DEFAULT_LIMIT`   | 20      | Default page size for list                      |
| `IRONCREW_CONVERSATIONS_MAX_LIMIT`       | 100     | Hard cap on `?limit=` parameter                 |
| `IRONCREW_CONVERSATION_MAX_HISTORY`      | 50      | Default per-session message cap                 |
| `IRONCREW_REQUIRE_IDEMPOTENCY_KEY`        | false   | Require retry-safe keys for runs and JSON/SQLite messages; PostgreSQL conversation messages always require a key; use `true` in production |

With `IRONCREW_STORE=postgres`, the keyed message may be sent through any
replica and will cold-rehydrate the durable conversation when that process has
no live handle. Keep the same key only for the same logical message. Each HTTP
build executes one immutable bounded snapshot of the flow's Lua sources,
including `_lib` modules and nested `run_flow`, and binds the effective
non-secret reachable-tool policy. Deploy identical flow, configuration, and
artifact identities on every replica; drift returns `409` instead of mixing
definitions.
