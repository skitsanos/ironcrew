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
it while answering questions:

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

The bridge-generated audit and `human_input_*` events contain question metadata
but not the human answer. Arbitrary flow logs, model output, and tool output
are not sanitized, so flows should not print sensitive answers.
