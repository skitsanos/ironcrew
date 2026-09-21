use std::path::Path;
use std::sync::Arc;

use crate::api;
use crate::utils::error::{IronCrewError, Result};

mod startup_policy;
use startup_policy::*;

pub async fn cmd_serve(host: &str, port: u16, flows_dir: &Path) -> Result<()> {
    crate::usage::budget::TokenBudget::from_environment()?;
    // `.env` is loaded once in `main` before the runtime starts; the server
    // never mutates the environment per-request (that was a data race and a
    // cross-flow secret-bleed source). Flows use the process environment.

    let flows_dir = std::fs::canonicalize(flows_dir).map_err(|error| {
        IronCrewError::Validation(format!(
            "Failed to resolve flows directory '{}': {error}",
            flows_dir.display()
        ))
    })?;
    if !flows_dir.is_dir() {
        return Err(IronCrewError::Validation(format!(
            "Flows path '{}' is not a directory",
            flows_dir.display()
        )));
    }

    let public_bind = public_bind_requires_auth(host);
    prepare_file_write_root(public_bind, &flows_dir)?;
    require_public_mcp_policy(public_bind)?;
    let runtime_identity = api::deployment::RuntimeIdentity::from_env()?;
    let auth = Arc::new(api::auth::AuthConfig::from_env()?);
    let admission = Arc::new(api::admission::AdmissionController::from_env()?);
    if public_bind
        && matches!(
            std::env::var("IRONCREW_STORE"),
            Err(std::env::VarError::NotPresent)
        )
    {
        return Err(IronCrewError::Validation(
            "Public server binds require an explicit IRONCREW_STORE=json, sqlite, or postgres; refusing the implicit local JSON default".into(),
        ));
    }

    if public_bind && !auth.is_configured() {
        if !unauthenticated_public_bind_allowed()? {
            return Err(IronCrewError::Validation(format!(
                "Refusing unauthenticated public bind on {host}; set IRONCREW_API_TOKEN or IRONCREW_API_TOKENS (recommended), or explicitly set IRONCREW_ALLOW_UNAUTHENTICATED=true"
            )));
        }
        tracing::warn!(
            bind_host = host,
            "Starting a public HTTP listener without API authentication because IRONCREW_ALLOW_UNAUTHENTICATED is enabled"
        );
    }

    // Bootstrap one persistence store and reuse its connection pool.
    let store = crate::engine::store::create_store(flows_dir.join(".ironcrew"))
        .await
        .map_err(|e| IronCrewError::Validation(format!("Failed to init store: {}", e)))?;

    // Reconcile only legacy or expired run leases. Healthy work owned by
    // another Railway/OpenShift replica remains untouched.
    let mut initial_maintenance_healthy = match crate::engine::reconciler::reconcile_stuck_runs(
        &store,
    )
    .await
    {
        Ok(_) => true,
        Err(error) => {
            tracing::error!(%error, "Initial run reconciliation failed; readiness will stay down until maintenance recovers");
            false
        }
    };

    let max_active_conversations = api::conversations::max_active_conversations();
    let max_active_conversation_lifecycles =
        api::conversation_lifecycle::max_active_conversation_lifecycles();
    let max_active_runs = api::handlers::max_active_runs();
    let max_active_inspections = api::handlers::max_active_inspections();
    let max_sse_connections = api::handlers::max_sse_connections();
    let max_run_lifetime = api::handlers::max_run_lifetime();
    let idempotency = api::idempotency::IdempotencyConfig::from_env(max_run_lifetime)?;
    let prune_now = chrono::Utc::now().to_rfc3339();
    let maintenance_watchdog = store.run_maintenance_watchdog();
    let pruned_idempotency_records = match startup_prune_with_policy(
        maintenance_watchdog,
        store.prune_idempotency(&prune_now, idempotency.prune_batch),
    )
    .await?
    {
        StartupPruneOutcome::Pruned(count) => count,
        StartupPruneOutcome::MaintenanceUnhealthy(error) => {
            initial_maintenance_healthy = false;
            tracing::error!(%error, "Startup idempotency pruning failed; readiness will stay down until maintenance recovers");
            0
        }
    };
    if pruned_idempotency_records > 0 {
        tracing::info!(
            count = pruned_idempotency_records,
            "Pruned expired idempotency records"
        );
    }
    let state = Arc::new(api::AppState {
        flows_dir: flows_dir.clone(),
        runtime_identity,
        auth,
        admission,
        lifecycle: api::lifecycle::LifecycleController::new(),
        active_runs: Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new())),
        active_conversations: Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new())),
        conversation_lifecycles: Arc::new(
            api::conversation_lifecycle::ConversationLifecycleRegistry::new(
                max_active_conversation_lifecycles,
            ),
        ),
        max_active_conversations,
        conversation_permits: Arc::new(tokio::sync::Semaphore::new(max_active_conversations)),
        max_active_runs,
        run_permits: Arc::new(tokio::sync::Semaphore::new(max_active_runs)),
        max_active_inspections,
        inspection_permits: Arc::new(tokio::sync::Semaphore::new(max_active_inspections)),
        max_sse_connections,
        sse_permits: Arc::new(tokio::sync::Semaphore::new(max_sse_connections)),
        max_run_lifetime,
        terminal_persistence_failures: std::sync::atomic::AtomicUsize::new(0),
        store_maintenance_healthy: std::sync::atomic::AtomicBool::new(initial_maintenance_healthy),
        readiness_cache: tokio::sync::Mutex::new(None),
        idempotency,
        store,
    });

    // Renew ownership leases well inside their TTL. A second pod may only
    // reconcile runs after these heartbeats expire, so rolling deployments no
    // longer abandon work owned by a healthy replica.
    let heartbeat_store = state.store.clone();
    let heartbeat_state = state.clone();
    let heartbeat_interval =
        crate::engine::store::run_lease_heartbeat_interval(heartbeat_store.run_lease_ttl());
    tracing::info!(
        instance_id = heartbeat_store.instance_id(),
        interval_seconds = heartbeat_interval.as_secs(),
        "Starting run lease heartbeat"
    );
    let heartbeat_handle = tokio::spawn(async move {
        let mut interval = tokio::time::interval(heartbeat_interval);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            let cycle = crate::engine::reconciler::maintain_run_leases(
                &heartbeat_store,
                &heartbeat_state.store_maintenance_healthy,
            )
            .await;
            match cycle.heartbeat {
                Ok(count) => {
                    tracing::trace!(count, "Refreshed owned run leases");
                }
                Err(error) => {
                    tracing::error!(%error, "Failed to refresh owned run leases");
                }
            }
            match cycle.reconciliation {
                Ok(0) => {}
                Ok(count) => {
                    tracing::warn!(count, "Reconciled expired run leases");
                }
                Err(error) => {
                    tracing::error!(%error, "Failed to reconcile expired run leases");
                }
            }
        }
    });

    // Background task: evict idle chat session handles.
    let idle_eviction_handle = tokio::spawn(api::conversations::idle_eviction_loop(state.clone()));

    // CORS: use IRONCREW_CORS_ORIGINS (comma-separated) or deny all.
    let cors = super::server_cors::from_env()?;

    let http_limits = super::http_limits::HttpLimits::from_env()?;
    let app = http_limits
        .apply(api::create_router(state.clone()))
        .layer(cors);

    let addr = format!("{}:{}", host, port);
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .map_err(|e| IronCrewError::Validation(format!("Failed to bind to {}: {}", addr, e)))?;

    println!("IronCrew API server v{}", env!("CARGO_PKG_VERSION"));
    println!("Listening on http://{}", addr);
    println!("Flows directory: {}", flows_dir.display());
    println!();
    println!("Endpoints:");
    println!("  GET    /health                       - Health check");
    println!("  GET    /health/live                  - Liveness check");
    println!("  GET    /health/ready                 - Storage-aware readiness check");
    println!("  GET    /metrics                      - Protected Prometheus metrics");
    println!("  POST   /flows/{{flow}}/run             - Run a crew (async, returns run_id)");
    println!("  POST   /flows/{{flow}}/abort/{{run_id}}  - Abort a running crew");
    println!("  GET    /flows/{{flow}}/events/{{run_id}} - SSE event stream for a run");
    println!("  GET    /flows/{{flow}}/questions/{{run_id}} - Pending ask_human questions");
    println!("  POST   /flows/{{flow}}/answer/{{run_id}} - Answer an ask_human question");
    println!("  GET    /flows/{{flow}}/runs            - List runs for a flow");
    println!("  GET    /flows/{{flow}}/runs/{{id}}       - Get run details");
    println!("  DELETE /flows/{{flow}}/runs/{{id}}       - Delete a run");
    println!("  GET    /flows/{{flow}}/validate         - Validate a flow");
    println!("  GET    /flows/{{flow}}/agents           - List agents in a flow");
    println!("  GET    /flows/{{flow}}/conversations    - List conversations for a flow");
    println!("  POST   /flows/{{flow}}/conversations/{{id}}/start    - Start a chat session");
    println!("  POST   /flows/{{flow}}/conversations/{{id}}/messages - Send a message");
    println!("  GET    /flows/{{flow}}/conversations/{{id}}/history  - Read history");
    println!("  GET    /flows/{{flow}}/conversations/{{id}}/events   - SSE event stream");
    println!("  DELETE /flows/{{flow}}/conversations/{{id}}          - Delete a conversation");
    println!("  GET    /nodes                         - List built-in tools");

    // After SIGTERM/Ctrl-C, fail readiness and reject mutations for a bounded
    // routing interval before stopping the listener. The teardown deadline
    // begins only once the lifecycle advances to stopping.
    let routing_grace_secs = bounded_env_u64(
        "IRONCREW_SHUTDOWN_ROUTING_GRACE_SECS",
        5,
        0,
        MAX_SHUTDOWN_ROUTING_GRACE_SECS,
    )?;
    let shutdown_timeout_secs = bounded_env_u64(
        "IRONCREW_SHUTDOWN_TIMEOUT_SECS",
        10,
        1,
        MAX_SHUTDOWN_TIMEOUT_SECS,
    )?;
    let drain_ms = bounded_env_u64("IRONCREW_SHUTDOWN_DRAIN_MS", 1000, 0, MAX_SHUTDOWN_DRAIN_MS)?;
    super::server_shutdown::serve_with_lifecycle(
        listener,
        app,
        state,
        heartbeat_handle,
        idle_eviction_handle,
        http_limits,
        super::server_shutdown::ShutdownConfig {
            routing_grace: std::time::Duration::from_secs(routing_grace_secs),
            teardown_timeout: std::time::Duration::from_secs(shutdown_timeout_secs),
            background_drain: std::time::Duration::from_millis(drain_ms),
        },
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::{StartupPruneOutcome, public_bind_requires_auth, startup_prune_with_policy};

    fn idempotency_store_failures() -> u64 {
        let mut body = String::new();
        crate::metrics::append_prometheus(&mut body);
        let prefix = "ironcrew_store_failures_total{operation=\"idempotency\"} ";
        body.lines()
            .find_map(|line| line.strip_prefix(prefix)?.parse().ok())
            .expect("idempotency store failure series")
    }
    use crate::utils::error::IronCrewError;

    #[test]
    fn loopback_binds_do_not_require_server_auth() {
        assert!(!public_bind_requires_auth("127.0.0.1"));
        assert!(!public_bind_requires_auth("::1"));
        assert!(!public_bind_requires_auth("localhost"));
    }

    #[test]
    fn wildcard_and_named_binds_require_server_auth() {
        assert!(public_bind_requires_auth("0.0.0.0"));
        assert!(public_bind_requires_auth("::"));
        assert!(public_bind_requires_auth("ironcrew.internal"));
    }

    #[tokio::test]
    async fn startup_prune_outer_timeout_is_a_watchdog_backend_soft_failure() {
        let failures_before = idempotency_store_failures();
        let outcome = startup_prune_with_policy(
            Some(std::time::Duration::from_millis(1)),
            std::future::pending(),
        )
        .await
        .expect("watchdog-backed startup pruning must degrade readiness");
        let StartupPruneOutcome::MaintenanceUnhealthy(error) = outcome else {
            panic!("outer timeout unexpectedly reported successful pruning");
        };
        assert!(error.to_string().contains("maintenance timeout"));
        assert!(idempotency_store_failures() > failures_before);
    }

    #[tokio::test]
    async fn startup_prune_local_backend_error_remains_fatal() {
        let result = startup_prune_with_policy(None, async {
            Err(IronCrewError::Validation("local prune failed".into()))
        })
        .await;
        let Err(error) = result else {
            panic!("local startup pruning unexpectedly became nonfatal");
        };
        assert!(
            error
                .to_string()
                .contains("Failed to prune the idempotency ledger at startup")
        );
    }
}
