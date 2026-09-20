use super::super::*;

impl PostgresStore {
    pub(super) async fn bootstrap_run_events(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    ) -> Result<()> {
        let t = &self.table_name;
        // 9. Durable bounded run-event journal. The event table owns payloads,
        // the state table keeps exact per-run replay bounds, and one trigger-
        // maintained singleton accounts global rows/bytes even during
        // cascading run deletion.
        let events = &self.run_events_table;
        let event_state = &self.run_event_state_table;
        let event_usage = &self.run_event_usage_table;
        let run_events_sql = format!(
            "CREATE TABLE IF NOT EXISTS {events} (
                run_id       TEXT NOT NULL,
                sequence     BIGINT NOT NULL,
                event_type   TEXT NOT NULL,
                payload      JSONB NOT NULL,
                payload_bytes BIGINT NOT NULL,
                accounted_bytes BIGINT GENERATED ALWAYS AS (
                    GREATEST(payload_bytes, octet_length(payload::text)::BIGINT,
                             {MIN_ACCOUNTED_RUN_EVENT_BYTES})
                ) STORED,
                created_at   TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
                expires_at   TIMESTAMPTZ NOT NULL,
                PRIMARY KEY (run_id, sequence),
                CONSTRAINT {events}_run_fk FOREIGN KEY (run_id)
                    REFERENCES {t} (run_id) ON DELETE CASCADE,
                CONSTRAINT {events}_payload_ck CHECK (
                    sequence > 0 AND
                    length(event_type) BETWEEN 1 AND 64 AND
                    event_type !~ '[^a-z0-9_]' AND
                    jsonb_typeof(payload) = 'object' AND
                    payload ? 'event' AND payload->>'event' = event_type AND
                    payload_bytes > 0 AND
                    accounted_bytes > 0 AND
                    accounted_bytes <= {HARD_MAX_EVENT_BYTES}
                ),
                CONSTRAINT {events}_expiry_ck CHECK (expires_at > created_at)
            );
            CREATE TABLE IF NOT EXISTS {event_state} (
                run_id                  TEXT PRIMARY KEY,
                flow                    TEXT NOT NULL,
                owner_instance_id       TEXT NOT NULL,
                latest_sequence         BIGINT NOT NULL DEFAULT 0,
                dropped_through         BIGINT NOT NULL DEFAULT 0,
                retained_events         BIGINT NOT NULL DEFAULT 0,
                retained_bytes          BIGINT NOT NULL DEFAULT 0,
                journal_complete        BOOLEAN NOT NULL DEFAULT TRUE,
                eviction_reason         TEXT,
                terminal_event_sequence BIGINT,
                updated_at              TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
                CONSTRAINT {event_state}_run_fk FOREIGN KEY (run_id)
                    REFERENCES {t} (run_id) ON DELETE CASCADE,
                CONSTRAINT {event_state}_bounds_ck CHECK (
                    latest_sequence >= 0 AND dropped_through >= 0 AND
                    dropped_through <= latest_sequence AND
                    retained_events >= 0 AND retained_bytes >= 0 AND
                    ((retained_events = 0 AND retained_bytes = 0) OR
                     (retained_events > 0 AND retained_bytes > 0)) AND
                    (terminal_event_sequence IS NULL OR
                     (terminal_event_sequence > 0 AND
                      terminal_event_sequence <= latest_sequence))
                ),
                CONSTRAINT {event_state}_reason_ck CHECK (
                    (dropped_through = 0 AND eviction_reason IS NULL) OR
                    (dropped_through > 0 AND eviction_reason IN
                        ('writer_backpressure', 'retention', 'global_capacity', 'owner_lost'))
                )
            );
            CREATE TABLE IF NOT EXISTS {event_usage} (
                singleton       BOOLEAN PRIMARY KEY DEFAULT TRUE CHECK (singleton),
                schema_version  INTEGER NOT NULL DEFAULT 0,
                retained_events BIGINT NOT NULL DEFAULT 0,
                retained_bytes  BIGINT NOT NULL DEFAULT 0,
                updated_at      TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
                CONSTRAINT {event_usage}_usage_ck CHECK (
                    schema_version >= 0 AND
                    retained_events >= 0 AND retained_bytes >= 0 AND
                    ((retained_events = 0 AND retained_bytes = 0) OR
                     (retained_events > 0 AND retained_bytes > 0))
                )
            );
            INSERT INTO {event_usage} (singleton) VALUES (TRUE)
            ON CONFLICT (singleton) DO NOTHING"
        );
        sqlx::raw_sql(sqlx::AssertSqlSafe(run_events_sql))
            .execute(&mut **tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "Failed to create PostgreSQL run-event journal tables: {error}"
                ))
            })?;
        let run_event_usage_migration = format!(
            "ALTER TABLE {event_usage} ADD COLUMN IF NOT EXISTS \
                 schema_version INTEGER NOT NULL DEFAULT 0"
        );
        sqlx::query(sqlx::AssertSqlSafe(run_event_usage_migration))
            .execute(&mut **tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "Failed to migrate PostgreSQL run-event schema version: {error}"
                ))
            })?;

        // `payload_bytes` is the storage-neutral compact JSON wire size used
        // for duplicate and page semantics. PostgreSQL's JSONB rendering can
        // be larger, so a generated conservative accounting size prevents a
        // direct/buggy insert from understating memory/storage consumption.
        // Refresh the constraint for databases created by an earlier binary.
        let run_event_payload_accounting_sql = format!(
            "ALTER TABLE {events} ADD COLUMN IF NOT EXISTS accounted_bytes BIGINT \
                 GENERATED ALWAYS AS (\
                     GREATEST(payload_bytes, octet_length(payload::text)::BIGINT, \
                              {MIN_ACCOUNTED_RUN_EVENT_BYTES})\
                 ) STORED; \
             ALTER TABLE {events} DROP CONSTRAINT IF EXISTS {events}_payload_ck; \
             ALTER TABLE {events} ADD CONSTRAINT {events}_payload_ck CHECK (\
                 sequence > 0 AND \
                 length(event_type) BETWEEN 1 AND 64 AND \
                 event_type !~ '[^a-z0-9_]' AND \
                 jsonb_typeof(payload) = 'object' AND \
                 payload ? 'event' AND payload->>'event' = event_type AND \
                 payload_bytes > 0 AND accounted_bytes > 0 AND \
                 accounted_bytes <= {HARD_MAX_EVENT_BYTES}\
             )"
        );
        sqlx::raw_sql(sqlx::AssertSqlSafe(run_event_payload_accounting_sql))
            .execute(&mut **tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "Failed to harden PostgreSQL run-event payload accounting: {error}"
                ))
            })?;

        for sql in [
            format!(
                "CREATE INDEX IF NOT EXISTS {events}_exp_idx ON {events} \
                 (expires_at, run_id, sequence)"
            ),
            format!(
                "CREATE INDEX IF NOT EXISTS {events}_old_idx ON {events} \
                 (created_at, run_id, sequence)"
            ),
        ] {
            sqlx::query(sqlx::AssertSqlSafe(sql))
                .execute(&mut **tx)
                .await
                .map_err(|error| {
                    IronCrewError::Validation(format!(
                        "Failed to create PostgreSQL run-event journal index: {error}"
                    ))
                })?;
        }

        let event_usage_function = format!("{events}_acct_fn");
        let event_usage_trigger = format!("{events}_acct_trg");
        let event_usage_function_sql = format!(
            r#"CREATE OR REPLACE FUNCTION {event_usage_function}() RETURNS TRIGGER
               LANGUAGE plpgsql AS $ironcrew$
               BEGIN
                   IF TG_OP = 'INSERT' THEN
                       UPDATE {event_usage}
                       SET retained_events = retained_events + 1,
                           retained_bytes = retained_bytes + NEW.accounted_bytes,
                           updated_at = clock_timestamp()
                       WHERE singleton = TRUE;
                       IF NOT FOUND THEN
                           RAISE EXCEPTION 'global run-event accounting row is missing';
                       END IF;
                       RETURN NEW;
                   END IF;

                   UPDATE {event_usage}
                   SET retained_events = retained_events - 1,
                       retained_bytes = retained_bytes - OLD.accounted_bytes,
                       updated_at = clock_timestamp()
                   WHERE singleton = TRUE;
                   IF NOT FOUND THEN
                       RAISE EXCEPTION 'global run-event accounting row is missing';
                   END IF;
                   RETURN OLD;
               END;
               $ironcrew$"#
        );
        sqlx::query(sqlx::AssertSqlSafe(event_usage_function_sql))
            .execute(&mut **tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "Failed to create PostgreSQL run-event accounting function: {error}"
                ))
            })?;
        let event_usage_trigger_sql = format!(
            "DROP TRIGGER IF EXISTS {event_usage_trigger} ON {events}; \
             CREATE TRIGGER {event_usage_trigger} \
             AFTER INSERT OR DELETE ON {events} FOR EACH ROW \
             EXECUTE FUNCTION {event_usage_function}()"
        );
        sqlx::raw_sql(sqlx::AssertSqlSafe(event_usage_trigger_sql))
            .execute(&mut **tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "Failed to create PostgreSQL run-event accounting trigger: {error}"
                ))
            })?;

        // Reconcile any journal created by a previous binary before releasing
        // the bootstrap DDL locks. Existing dropped boundaries remain intact;
        // retained counts and global usage are rebuilt from source rows.
        let reconcile_run_events = format!(
            "INSERT INTO {event_state} AS state (
                 run_id, flow, owner_instance_id, latest_sequence,
                 retained_events, retained_bytes, journal_complete,
                 terminal_event_sequence, updated_at
             )
             SELECT event.run_id, run.flow, run.owner_instance_id,
                    MAX(event.sequence), COUNT(*)::BIGINT,
                    SUM(event.accounted_bytes)::BIGINT,
                    MIN(event.sequence) = 1 AND COUNT(*)::BIGINT = MAX(event.sequence),
                    MAX(event.sequence) FILTER (WHERE event.event_type = 'run_complete'),
                    clock_timestamp()
             FROM {events} AS event
             JOIN {t} AS run ON run.run_id = event.run_id
             GROUP BY event.run_id, run.flow, run.owner_instance_id
             ON CONFLICT (run_id) DO UPDATE SET
                 flow = EXCLUDED.flow,
                 owner_instance_id = EXCLUDED.owner_instance_id,
                 latest_sequence = GREATEST(state.latest_sequence, EXCLUDED.latest_sequence),
                 retained_events = EXCLUDED.retained_events,
                 retained_bytes = EXCLUDED.retained_bytes,
                 journal_complete = state.journal_complete AND
                     EXCLUDED.retained_events =
                         EXCLUDED.latest_sequence - state.dropped_through AND
                     (SELECT MIN(retained.sequence) = state.dropped_through + 1
                      FROM {events} AS retained
                      WHERE retained.run_id = state.run_id),
                 terminal_event_sequence = COALESCE(
                     state.terminal_event_sequence, EXCLUDED.terminal_event_sequence),
                 updated_at = clock_timestamp();
             UPDATE {event_usage}
             SET retained_events = (SELECT COUNT(*)::BIGINT FROM {events}),
                 retained_bytes = (SELECT COALESCE(SUM(accounted_bytes), 0)::BIGINT FROM {events}),
                 updated_at = clock_timestamp()
             WHERE singleton = TRUE"
        );
        let stored_run_event_schema_version: i32 =
            sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                "SELECT schema_version FROM {event_usage} \
                 WHERE singleton = TRUE FOR UPDATE"
            )))
            .fetch_one(&mut **tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "Failed to read PostgreSQL run-event schema version: {error}"
                ))
            })?;
        if stored_run_event_schema_version > RUN_EVENT_SCHEMA_VERSION {
            return Err(IronCrewError::Validation(format!(
                "PostgreSQL run-event schema version {stored_run_event_schema_version} is newer than supported version {RUN_EVENT_SCHEMA_VERSION}"
            )));
        }
        if stored_run_event_schema_version < RUN_EVENT_SCHEMA_VERSION {
            sqlx::raw_sql(sqlx::AssertSqlSafe(reconcile_run_events))
                .execute(&mut **tx)
                .await
                .map_err(|error| {
                    IronCrewError::Validation(format!(
                        "Failed to reconcile PostgreSQL run-event journal accounting: {error}"
                    ))
                })?;
            let mark_version_sql = format!(
                "UPDATE {event_usage} SET schema_version = $1, \
                     updated_at = clock_timestamp() WHERE singleton = TRUE"
            );
            sqlx::query(sqlx::AssertSqlSafe(mark_version_sql))
                .bind(RUN_EVENT_SCHEMA_VERSION)
                .execute(&mut **tx)
                .await
                .map_err(|error| {
                    IronCrewError::Validation(format!(
                        "Failed to mark PostgreSQL run-event schema version: {error}"
                    ))
                })?;
        }
        Ok(())
    }
}
