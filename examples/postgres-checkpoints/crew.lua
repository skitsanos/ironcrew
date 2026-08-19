-- Checkpoint intermediate results to the app database (postgres.* namespace).
-- Requires IRONCREW_APP_DATABASE_URL and a `checkpoints` table:
--   CREATE TABLE checkpoints (
--     idempotency_key text PRIMARY KEY,
--     execution_id text NOT NULL, stage text NOT NULL, payload jsonb NOT NULL);

local crew = Crew.new({
    goal = "Analyze a topic and checkpoint each stage",
    provider = "openai",
    model = env("OPENAI_MODEL") or "gpt-5.6-luna",
})

crew:add_agent({ name = "analyst", goal = "Produce a short structured analysis" })

crew:add_task({
    name = "analyze",
    description = "List 3 key benefits of Rust for systems programming as JSON array.",
    agent = "analyst",
})

local results = crew:run()
local execution_id = uuid4()

-- At-least-once contract: the operation is an upsert keyed on
-- execution_id .. ':' .. stage, so a retried task cannot duplicate rows.
postgres.execute("save_checkpoint", {
    execution_id = execution_id,
    stage = "analyze",
    payload = { output = results[1].output },
})

local rows = postgres.query("load_checkpoints", { execution_id = execution_id })
print(string.format("stored %d checkpoint(s) for %s", #rows, execution_id))
