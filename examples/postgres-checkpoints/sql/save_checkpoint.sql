-- ironcrew:op
-- params: execution_id text, stage text, payload json
INSERT INTO checkpoints (idempotency_key, execution_id, stage, payload)
VALUES ($1 || ':' || $2, $1, $2, $3)
ON CONFLICT (idempotency_key) DO UPDATE SET payload = EXCLUDED.payload;
