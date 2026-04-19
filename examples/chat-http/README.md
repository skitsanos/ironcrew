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
export IRONCREW_API_TOKEN=dev-token   # required to exercise the API

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
     -H 'Content-Type: application/json' \
     -d '{ "content": "Hi! What can you help me with?" }' | jq
```

## Tail live events

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
| `IRONCREW_API_TOKEN`                     | —       | Bearer token required for the protected routes  |
| `IRONCREW_MAX_ACTIVE_CONVERSATIONS`      | 100     | Simultaneous in-memory session cap              |
| `IRONCREW_CHAT_SESSION_IDLE_SECS`        | 1800    | Idle eviction threshold                         |
| `IRONCREW_CONVERSATIONS_DEFAULT_LIMIT`   | 20      | Default page size for list                      |
| `IRONCREW_CONVERSATIONS_MAX_LIMIT`       | 100     | Hard cap on `?limit=` parameter                 |
| `IRONCREW_CONVERSATION_MAX_HISTORY`      | 50      | Default per-session message cap                 |
