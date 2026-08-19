-- ironcrew:op
-- params: execution_id text
SELECT stage, payload FROM checkpoints WHERE execution_id = $1 ORDER BY stage LIMIT 100;
