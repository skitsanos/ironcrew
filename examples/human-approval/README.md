# Human Approval

This example puts both human-in-the-loop mechanisms in one real agent turn:

- The `release_manager` opts into `tools = { "ask_human", "file_write" }`
  and decides when it needs the release channel.
- `require_approval = { "file_write" }` independently gates each attempted
  write, even though the agent is allowed to use the tool.

The distinction is deliberate: `ask_human` gathers missing information;
`require_approval` authorizes a consequential action.

## Run it

```bash
cp examples/human-approval/.env.example examples/human-approval/.env
# Replace OPENAI_API_KEY in the copied file.
ironcrew run examples/human-approval
```

The expected interaction is:

1. Answer the agent's release-channel question with `canary`, `stable`, or
   `hold`.
2. If a write is attempted, answer its `kind: "approval"` question:
   - `allow` permits only that call.
   - `always` permits that call and later `file_write` calls for this flow
     execution.
   - `deny`, a free-form response, or a timeout fails closed.
3. Approved artifacts appear under `examples/human-approval/output/`.

The agent is instructed not to retry a denied call and not to claim that a
denied artifact exists. Model behavior is still probabilistic, so consumers
must rely on tool results and run events—not prose alone—to confirm effects.

The task's `timeout_secs = 120` is an active-execution budget. IronCrew pauses
that clock while the run is suspended on `ask_human` or an approval gate, so
the 300-second question timeout and approval timeout remain independent.

## HTTP mode

Use the authenticated server and SSE setup in
[`../ask-human/README.md`](../ask-human/README.md#authenticated-http-sse-and-answers),
but start `human-approval` with an empty input object. The questions endpoint
distinguishes:

- `kind: "question"` for the agent's release-channel request.
- `kind: "approval"` for each gated `file_write` call.

Unlike the fixed-checkpoint example, this run asks more than once. Poll and
answer the agent's question, then poll again for its first approval. Using
`always` at that point permits this and later `file_write` calls in the same
run:

```bash
start=$(curl -fsS -X POST http://127.0.0.1:3000/flows/human-approval/run \
  -H "Authorization: Bearer $IRONCREW_API_TOKEN" \
  -H 'Content-Type: application/json' \
  -H "Idempotency-Key: human-approval-$(uuidgen)" \
  -d '{}')
run_id=$(printf '%s' "$start" | jq -r '.run_id')

curl -N \
  -H "Authorization: Bearer $IRONCREW_API_TOKEN" \
  "http://127.0.0.1:3000/flows/human-approval/events/$run_id" &
sse_pid=$!

wait_for_question() {
  local deadline=$((SECONDS + 360))
  while (( SECONDS < deadline )); do
    if pending=$(curl -fsS \
      -H "Authorization: Bearer $IRONCREW_API_TOKEN" \
      "http://127.0.0.1:3000/flows/human-approval/questions/$run_id" 2>/dev/null); then
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
  curl -fsS -X POST \
    -H "Authorization: Bearer $IRONCREW_API_TOKEN" \
    -H 'Content-Type: application/json' \
    "http://127.0.0.1:3000/flows/human-approval/answer/$run_id" \
    -d "$(jq -nc --arg id "$question_id" --arg answer "$1" \
      '{question_id: $id, answer: $answer}')"
}

wait_for_question  # kind: question
answer_current canary
wait_for_question  # kind: approval
answer_current always
wait "$sse_pid"
```

The SSE stream emits `human_input_requested` and `human_input_received` for
both kinds, followed by `run_complete`. Only a standalone `allow`, `yes`,
`always`, or `allow-always` token permits a gated call; comparisons are
case-insensitive and surrounding whitespace is ignored.
