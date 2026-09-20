use super::*;

impl PostgresStore {
    /// Create a new PostgreSQL store.
    /// `table_prefix` allows sharing a database across projects:
    ///   prefix = "myapp_" → table = "myapp_runs"
    ///   prefix = "" → table = "runs" (default)
    pub async fn new(database_url: &str, table_prefix: &str) -> Result<Self> {
        Self::new_with_lease_config(database_url, table_prefix, RunLeaseConfig::from_env()?).await
    }

    pub async fn new_with_lease_config(
        database_url: &str,
        table_prefix: &str,
        lease: RunLeaseConfig,
    ) -> Result<Self> {
        let keyring = HumanInputKeyring::from_env()?;
        Self::new_with_lease_config_and_human_input_keyring(
            database_url,
            table_prefix,
            lease,
            keyring,
        )
        .await
    }

    /// Construct a store with an explicit durable human-input keyring.
    ///
    /// Production callers normally use [`Self::new_with_lease_config`], which
    /// loads the keyring once from the environment. This constructor keeps
    /// live-database tests deterministic and avoids process-global env races.
    pub async fn new_with_lease_config_and_human_input_keyring(
        database_url: &str,
        table_prefix: &str,
        lease: RunLeaseConfig,
        human_input_keyring: Option<HumanInputKeyring>,
    ) -> Result<Self> {
        let run_event_journal_config = RunEventJournalConfig::from_env()?;
        Self::new_with_runtime_config(
            database_url,
            table_prefix,
            lease,
            human_input_keyring,
            run_event_journal_config,
        )
        .await
    }

    /// Construct a store with deterministic process-wide runtime features.
    /// Production constructors load these immutable values once from env;
    /// live PostgreSQL tests use this entrypoint to avoid environment races.
    pub async fn new_with_runtime_config(
        database_url: &str,
        table_prefix: &str,
        lease: RunLeaseConfig,
        human_input_keyring: Option<HumanInputKeyring>,
        run_event_journal_config: RunEventJournalConfig,
    ) -> Result<Self> {
        // Validate table prefix to prevent SQL injection via env var
        validate_table_prefix(table_prefix)?;
        run_event_journal_config.validate()?;
        let human_input_max_pending_rows = max_pending().min(MAX_DURABLE_HUMAN_INPUT_ROWS);
        let human_input_max_pending_ciphertext_bytes = max_pending_bytes()
            .checked_add(
                human_input_max_pending_rows.saturating_mul(HUMAN_INPUT_AEAD_OVERHEAD_BYTES),
            )
            .ok_or_else(|| {
                IronCrewError::Validation("PostgreSQL human-input ciphertext limit overflow".into())
            })?;
        let human_input_read_concurrency: usize = parse_env(
            HUMAN_INPUT_READ_CONCURRENCY_ENV,
            DEFAULT_HUMAN_INPUT_READ_CONCURRENCY,
        )?;
        if !(1..=MAX_HUMAN_INPUT_READ_CONCURRENCY).contains(&human_input_read_concurrency) {
            return Err(IronCrewError::Validation(format!(
                "{HUMAN_INPUT_READ_CONCURRENCY_ENV} must be between 1 and {MAX_HUMAN_INPUT_READ_CONCURRENCY}"
            )));
        }

        let max_conn: u32 = parse_env("IRONCREW_DB_POOL_SIZE", 10)?;
        if max_conn == 0 || max_conn > MAX_DB_POOL_SIZE {
            return Err(IronCrewError::Validation(format!(
                "IRONCREW_DB_POOL_SIZE must be between 1 and {MAX_DB_POOL_SIZE}"
            )));
        }

        // Retries *after* the initial attempt. With backoff this rides out a
        // transient database outage (e.g. a platform restart) so a brief blip
        // doesn't crash the process and burn a container restart per attempt.
        let retries: u32 = parse_env("IRONCREW_DB_CONNECT_RETRIES", 10)?;
        if retries > MAX_CONNECT_RETRIES {
            return Err(IronCrewError::Validation(format!(
                "IRONCREW_DB_CONNECT_RETRIES must be at most {MAX_CONNECT_RETRIES}"
            )));
        }
        let backoff_base_ms: u64 = parse_env("IRONCREW_DB_CONNECT_BACKOFF_MS", 1_000)?;
        if backoff_base_ms == 0 || backoff_base_ms > CONNECT_BACKOFF_CAP_MS {
            return Err(IronCrewError::Validation(format!(
                "IRONCREW_DB_CONNECT_BACKOFF_MS must be between 1 and {CONNECT_BACKOFF_CAP_MS}"
            )));
        }
        let connect_timeout_secs: u64 = parse_env("IRONCREW_DB_CONNECT_TIMEOUT_SECS", 30)?;
        if connect_timeout_secs == 0 || connect_timeout_secs > MAX_CONNECT_TIMEOUT_SECS {
            return Err(IronCrewError::Validation(format!(
                "IRONCREW_DB_CONNECT_TIMEOUT_SECS must be between 1 and {MAX_CONNECT_TIMEOUT_SECS}"
            )));
        }

        let pool = crate::engine::pg_runtime::connect_pool(
            database_url,
            &crate::engine::pg_runtime::PgConnectSettings {
                max_connections: max_conn,
                acquire_timeout: Duration::from_secs(connect_timeout_secs),
                retries,
                backoff_base_ms,
                backoff_cap_ms: CONNECT_BACKOFF_CAP_MS,
            },
            "state store",
        )
        .await?;

        crate::engine::pg_runtime::ensure_supported_postgres_version(&pool).await?;

        let table_name = format!("{}runs", table_prefix);
        let conversations_table = format!("{}conversations", table_prefix);
        let dialogs_table = format!("{}dialogs", table_prefix);
        let audit_events_table = format!("{}audit_events", table_prefix);
        let idempotency_table = format!("{}idempotency", table_prefix);
        let idempotency_accounting_table = format!("{}idempotency_accounting", table_prefix);
        let human_inputs_table = format!("{}human_inputs", table_prefix);
        let run_events_table = format!("{}run_events", table_prefix);
        let run_event_state_table = format!("{}run_event_state", table_prefix);
        let run_event_usage_table = format!("{}run_event_usage", table_prefix);

        let store = Self {
            pool,
            table_name: table_name.clone(),
            conversations_table,
            dialogs_table,
            audit_events_table,
            idempotency_table,
            idempotency_accounting_table,
            human_inputs_table,
            run_events_table,
            run_event_state_table,
            run_event_usage_table,
            human_input_keyring,
            human_input_max_pending_rows,
            human_input_max_pending_ciphertext_bytes,
            human_input_read_slots: Semaphore::new(human_input_read_concurrency),
            run_event_journal_config,
            lease,
        };
        store.bootstrap().await?;
        store
            .verify_human_input_key_coverage(Duration::from_secs(connect_timeout_secs))
            .await?;

        tracing::info!("PostgreSQL store ready (table: {})", table_name);
        Ok(store)
    }
}
