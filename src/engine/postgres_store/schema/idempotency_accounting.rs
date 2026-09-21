use super::super::*;

impl PostgresStore {
    pub(super) async fn bootstrap_idempotency_accounting(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    ) -> Result<()> {
        let it = &self.idempotency_table;
        // A compact accounting table avoids COUNT/SUM scans on every claim
        // and completion. The global row is always updated first, then the
        // opaque principal row, matching the application advisory-lock order.
        let accounting = &self.idempotency_accounting_table;
        let accounting_sql = format!(
            "CREATE TABLE IF NOT EXISTS {accounting} (
                principal_id    TEXT PRIMARY KEY,
                is_global       BOOLEAN NOT NULL,
                record_count    BIGINT NOT NULL DEFAULT 0 CHECK (record_count >= 0),
                in_flight_count BIGINT NOT NULL DEFAULT 0 CHECK (in_flight_count >= 0),
                response_bytes  BIGINT NOT NULL DEFAULT 0 CHECK (response_bytes >= 0),
                updated_at      TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
                CHECK ((is_global AND principal_id = 'global') OR
                       (NOT is_global AND length(principal_id) = 64 AND
                        principal_id !~ '[^0-9a-f]'))
            );
            INSERT INTO {accounting} (principal_id, is_global)
            VALUES ('global', TRUE)
            ON CONFLICT (principal_id) DO NOTHING"
        );
        sqlx::raw_sql(sqlx::AssertSqlSafe(accounting_sql))
            .execute(&mut **tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "Failed to create PostgreSQL idempotency accounting table: {error}"
                ))
            })?;

        let accounting_function = format!("{it}_acct_fn");
        let accounting_trigger = format!("{it}_acct_trg");
        let function_sql = format!(
            r#"CREATE OR REPLACE FUNCTION {accounting_function}() RETURNS TRIGGER
               LANGUAGE plpgsql AS $ironcrew$
               DECLARE
                   changed_principal TEXT;
                   record_delta BIGINT := 0;
                   in_flight_delta BIGINT := 0;
                   response_delta BIGINT := 0;
               BEGIN
                   IF TG_OP = 'INSERT' THEN
                       changed_principal := NEW.principal_id;
                       record_delta := 1;
                       in_flight_delta := CASE WHEN NEW.state IN ('claimed', 'running') THEN 1 ELSE 0 END;
                       response_delta := COALESCE(octet_length(NEW.response_body), 0);
                   ELSIF TG_OP = 'DELETE' THEN
                       changed_principal := OLD.principal_id;
                       record_delta := -1;
                       in_flight_delta := -(CASE WHEN OLD.state IN ('claimed', 'running') THEN 1 ELSE 0 END);
                       response_delta := -COALESCE(octet_length(OLD.response_body), 0);
                   ELSE
                       IF OLD.principal_id <> NEW.principal_id THEN
                           RAISE EXCEPTION 'idempotency principal_id is immutable';
                       END IF;
                       changed_principal := NEW.principal_id;
                       in_flight_delta :=
                           (CASE WHEN NEW.state IN ('claimed', 'running') THEN 1 ELSE 0 END) -
                           (CASE WHEN OLD.state IN ('claimed', 'running') THEN 1 ELSE 0 END);
                       response_delta := COALESCE(octet_length(NEW.response_body), 0) -
                                         COALESCE(octet_length(OLD.response_body), 0);
                       IF in_flight_delta = 0 AND response_delta = 0 THEN
                           RETURN NEW;
                       END IF;
                   END IF;

                   UPDATE {accounting}
                   SET record_count = record_count + record_delta,
                       in_flight_count = in_flight_count + in_flight_delta,
                       response_bytes = response_bytes + response_delta,
                       updated_at = clock_timestamp()
                   WHERE principal_id = 'global' AND is_global = TRUE;
                   IF NOT FOUND THEN
                       RAISE EXCEPTION 'global idempotency accounting row is missing';
                   END IF;

                   IF TG_OP = 'INSERT' THEN
                       INSERT INTO {accounting} AS usage
                           (principal_id, is_global, record_count, in_flight_count,
                            response_bytes, updated_at)
                       VALUES (changed_principal, FALSE, record_delta, in_flight_delta,
                               response_delta, clock_timestamp())
                       ON CONFLICT (principal_id) DO UPDATE SET
                           record_count = usage.record_count + EXCLUDED.record_count,
                           in_flight_count = usage.in_flight_count + EXCLUDED.in_flight_count,
                           response_bytes = usage.response_bytes + EXCLUDED.response_bytes,
                           updated_at = clock_timestamp();
                   ELSE
                       UPDATE {accounting}
                       SET record_count = record_count + record_delta,
                           in_flight_count = in_flight_count + in_flight_delta,
                           response_bytes = response_bytes + response_delta,
                           updated_at = clock_timestamp()
                       WHERE principal_id = changed_principal AND is_global = FALSE;
                       IF NOT FOUND THEN
                           RAISE EXCEPTION 'principal idempotency accounting row is missing';
                       END IF;
                   END IF;

                   DELETE FROM {accounting}
                   WHERE principal_id = changed_principal AND is_global = FALSE
                     AND record_count = 0 AND in_flight_count = 0 AND response_bytes = 0;
                   IF TG_OP = 'DELETE' THEN
                       RETURN OLD;
                   END IF;
                   RETURN NEW;
               END;
               $ironcrew$"#
        );
        sqlx::query(sqlx::AssertSqlSafe(function_sql))
            .execute(&mut **tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "Failed to create PostgreSQL idempotency accounting function: {error}"
                ))
            })?;
        let trigger_sql = format!(
            "DROP TRIGGER IF EXISTS {accounting_trigger} ON {it}; \
             CREATE TRIGGER {accounting_trigger} \
             AFTER INSERT OR DELETE OR UPDATE OF principal_id, state, response_body ON {it} \
             FOR EACH ROW EXECUTE FUNCTION {accounting_function}()"
        );
        sqlx::raw_sql(sqlx::AssertSqlSafe(trigger_sql))
            .execute(&mut **tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "Failed to create PostgreSQL idempotency accounting trigger: {error}"
                ))
            })?;

        // Reconcile once during migration. DDL locks on the ledger remain
        // held until commit, so a concurrent write either precedes this scan
        // or runs through the newly installed trigger afterwards.
        let reconcile_accounting = format!(
            "DELETE FROM {accounting} WHERE is_global = FALSE; \
             INSERT INTO {accounting} \
                 (principal_id, is_global, record_count, in_flight_count, response_bytes) \
             SELECT principal_id, FALSE, COUNT(*)::BIGINT, \
                    COUNT(*) FILTER (WHERE state IN ('claimed', 'running'))::BIGINT, \
                    COALESCE(SUM(octet_length(response_body)), 0)::BIGINT \
             FROM {it} GROUP BY principal_id; \
             UPDATE {accounting} SET \
                 record_count = (SELECT COUNT(*)::BIGINT FROM {it}), \
                 in_flight_count = (SELECT COUNT(*)::BIGINT FROM {it} \
                                    WHERE state IN ('claimed', 'running')), \
                 response_bytes = (SELECT COALESCE(SUM(octet_length(response_body)), 0)::BIGINT \
                                   FROM {it}), \
                 updated_at = clock_timestamp() \
             WHERE principal_id = 'global' AND is_global = TRUE"
        );
        sqlx::raw_sql(sqlx::AssertSqlSafe(reconcile_accounting))
            .execute(&mut **tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "Failed to reconcile PostgreSQL idempotency accounting: {error}"
                ))
            })?;
        Ok(())
    }
}
