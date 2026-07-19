# Ask Human

This flow demonstrates fixed, flow-authored checkpoints with
`crew:ask_human()`: an attended CLI run asks for a topic before the task, and
all normal runs ask for a publish or hold decision after the draft. The flow
has a real `announcer` agent and task, so it completes after those decisions.

For questions an agent decides to ask during its own turn, and approval gates
around tool calls, see [`../human-approval`](../human-approval/).

## CLI

```bash
cp examples/ask-human/.env.example examples/ask-human/.env
# Replace OPENAI_API_KEY in the copied file.
ironcrew run examples/ask-human
```

Prompts are written to the terminal and answers are read from standard input.
A bare choice number selects that numbered choice. In a non-interactive run,
each question immediately uses its `default` instead of hanging.

## Authenticated HTTP, SSE, and answers

Server mode reads provider credentials from the server process environment,
not from a flow-local `.env`. From the repository root, start the server with
a development token of at least 32 visible ASCII characters:

```bash
export OPENAI_API_KEY='replace-me'
export IRONCREW_API_TOKEN='local-development-token-change-me-123456'
ironcrew serve --flows-dir examples --host 127.0.0.1 --port 3000
```

In another shell, start a run with an empty input object. The server persists
the keyed run intent before Lua setup and returns the `run_id` immediately, so
the pre-run topic question is available through the normal question endpoint.
The idempotency header also makes retries safe when
`IRONCREW_REQUIRE_IDEMPOTENCY_KEY=true`:

```bash
export IRONCREW_API_TOKEN='local-development-token-change-me-123456'

start=$(curl -fsS -X POST http://127.0.0.1:3000/flows/ask-human/run \
  -H "Authorization: Bearer $IRONCREW_API_TOKEN" \
  -H 'Content-Type: application/json' \
  -H "Idempotency-Key: ask-human-$(uuidgen)" \
  -d '{}')
run_id=$(printf '%s' "$start" | jq -r '.run_id')
printf 'run_id=%s\n' "$run_id"
```

Subscribe to the run's replayable SSE stream in a third shell, or background
it while answering questions. JSON/SQLite replay is process-local; PostgreSQL
mode below provides durable cross-replica run replay:

```bash
curl -N \
  -H "Authorization: Bearer $IRONCREW_API_TOKEN" \
  "http://127.0.0.1:3000/flows/ask-human/events/$run_id" &
sse_pid=$!
```

Use a bounded poll that also notices if the SSE stream has already closed. It
avoids hanging forever when a provider or flow fails:

```bash
wait_for_question() {
  local deadline=$((SECONDS + 360))
  while (( SECONDS < deadline )); do
    if pending=$(curl -fsS \
      -H "Authorization: Bearer $IRONCREW_API_TOKEN" \
      "http://127.0.0.1:3000/flows/ask-human/questions/$run_id" 2>/dev/null); then
      question_id=$(printf '%s' "$pending" | jq -r \
        '.questions[0].question_id // empty')
      [ -n "$question_id" ] && {
        printf '%s' "$pending" | jq
        return 0
      }
    fi
    if ! kill -0 "$sse_pid" 2>/dev/null; then
      echo "run ended before another question appeared" >&2
      return 1
    fi
    sleep 1
  done
  echo "timed out waiting for a question" >&2
  return 1
}

answer_current() {
  jq -nc --arg id "$question_id" --arg answer "$1" \
    '{question_id: $id, answer: $answer}' |
    curl -fsS -X POST \
      -H "Authorization: Bearer $IRONCREW_API_TOKEN" \
      -H 'Content-Type: application/json' \
      "http://127.0.0.1:3000/flows/ask-human/answer/$run_id" \
      --data-binary @-
}
```

Answer the topic question, then wait while the agent drafts and answer the
publish checkpoint. The SSE stream emits `human_input_requested` and
`human_input_received` for both checkpoints, followed by `run_complete`:

```bash
wait_for_question
answer_current 'IronCrew human-in-the-loop support'

wait_for_question
answer_current publish

wait "$sse_pid"
```

The bridge-generated audit and `human_input_*` events never contain the human
answer. In PostgreSQL's durable event journal,
`human_input_requested` also omits prompt and choices and points clients to the
authenticated questions endpoint. Arbitrary flow logs, model output, and tool
output are not sanitized, so flows should not print sensitive answers.

## Cross-replica PostgreSQL mode

The example above works with the owner-local bridge on any backend. To let a
different replica list and answer its questions, run every server against the
same PostgreSQL 15+ database and give every replica the same HITL keyring:

```bash
export IRONCREW_STORE=postgres
export DATABASE_URL='postgres://user:password@host/ironcrew'

# Generate once, then store these two values in your platform secret manager.
hitl_key=$(openssl rand -base64 32)
export IRONCREW_HITL_ENCRYPTION_KEYS=$(jq -nc \
  --arg key "$hitl_key" '{"2026-07": $key}')
unset hitl_key
export IRONCREW_HITL_ACTIVE_KEY_ID='2026-07'
```

The run request must keep its `Idempotency-Key`. A question GET through either
replica then reports `control_scope: "shared_store"`; an accepted answer
returns HTTP `202` with `status: "queued"`. PostgreSQL stores encrypted
question metadata and answer ciphertext, and the owning replica polls and
resumes the Lua coroutine. A second answer returns `404` because the first
writer wins.

PostgreSQL also journals bounded run events in plaintext JSONB. Any replica can
serve the run SSE endpoint and resume retained events with an id such as
`<run_id>:<sequence>` in `Last-Event-ID`:

```bash
last_event_id="$run_id:17" # Replace 17 with the last id you fully processed.
curl -N -H "Authorization: Bearer $IRONCREW_API_TOKEN" \
  -H "Last-Event-ID: $last_event_id" \
  "http://127.0.0.1:3000/flows/ask-human/events/$run_id"
```

The journal is best-effort and bounded: it can report explicit gaps, and a
terminal run record can synthesize an incomplete `run_complete`. Other event
payloads may contain sensitive task/model/tool/log data. Because API tokens do
not provide per-flow read authorization, treat every token as
administrator-equivalent and configure journal retention/capacity accordingly.

Neither the encrypted HITL mailbox nor the event journal moves execution
between pods. Conversation SSE and JSON/SQLite run SSE remain process-local.
Follow the staged key-rotation procedure in the
[cloud deployment guide](../../docs/cloud-deployment.md#hitl-key-rotation-on-railway-and-openshift).
