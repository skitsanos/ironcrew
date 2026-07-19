use std::path::Path;
use std::sync::Arc;

use crate::api;
use crate::utils::error::{IronCrewError, Result};

const MAX_REQUEST_BODY_HARD_LIMIT: usize = 64 * 1024 * 1024;
const MAX_SHUTDOWN_TIMEOUT_SECS: u64 = 300;
const MAX_SHUTDOWN_DRAIN_MS: u64 = 30_000;

fn bounded_env_u64(name: &str, default: u64, min: u64, max: u64) -> Result<u64> {
    let value = match std::env::var(name) {
        Ok(value) => value.parse::<u64>().map_err(|_| {
            IronCrewError::Validation(format!("{name} must be an integer between {min} and {max}"))
        })?,
        Err(std::env::VarError::NotPresent) => default,
        Err(std::env::VarError::NotUnicode(_)) => {
            return Err(IronCrewError::Validation(format!(
                "{name} must contain valid UTF-8"
            )));
        }
    };
    if !(min..=max).contains(&value) {
        return Err(IronCrewError::Validation(format!(
            "{name} must be between {min} and {max}"
        )));
    }
    Ok(value)
}

fn public_bind_requires_auth(host: &str) -> bool {
    host.parse::<std::net::IpAddr>()
        .map(|address| !address.is_loopback())
        .unwrap_or_else(|_| !host.eq_ignore_ascii_case("localhost"))
}

fn unauthenticated_public_bind_allowed() -> Result<bool> {
    match std::env::var("IRONCREW_ALLOW_UNAUTHENTICATED") {
        Err(std::env::VarError::NotPresent) => Ok(false),
        Ok(value) if value == "1" || value.eq_ignore_ascii_case("true") => Ok(true),
        Ok(value) if value == "0" || value.eq_ignore_ascii_case("false") => Ok(false),
        Ok(_) | Err(std::env::VarError::NotUnicode(_)) => Err(IronCrewError::Validation(
            "IRONCREW_ALLOW_UNAUTHENTICATED must be one of: 1, true, 0, false".into(),
        )),
    }
}

fn prepare_file_write_root(public_bind: bool, flows_dir: &Path) -> Result<()> {
    let configured = std::env::var_os("IRONCREW_FILE_WRITE_ROOT");
    let Some(configured) = configured.filter(|value| !value.is_empty()) else {
        if public_bind {
            return Err(IronCrewError::Validation(
                "Public server binds require IRONCREW_FILE_WRITE_ROOT to be an explicit writable directory separate from the flow source tree".into(),
            ));
        }
        return Ok(());
    };
    let root = std::path::PathBuf::from(configured);
    if public_bind && !root.is_absolute() {
        return Err(IronCrewError::Validation(
            "IRONCREW_FILE_WRITE_ROOT must be absolute for public server binds".into(),
        ));
    }
    std::fs::create_dir_all(&root).map_err(|error| {
        IronCrewError::Validation(format!(
            "Failed to create IRONCREW_FILE_WRITE_ROOT '{}': {error}",
            root.display()
        ))
    })?;
    let root = std::fs::canonicalize(&root).map_err(|error| {
        IronCrewError::Validation(format!(
            "Failed to resolve IRONCREW_FILE_WRITE_ROOT '{}': {error}",
            root.display()
        ))
    })?;
    if root == flows_dir || root.starts_with(flows_dir) || flows_dir.starts_with(&root) {
        return Err(IronCrewError::Validation(format!(
            "IRONCREW_FILE_WRITE_ROOT '{}' must be disjoint from flows directory '{}'",
            root.display(),
            flows_dir.display()
        )));
    }
    Ok(())
}

fn require_public_mcp_policy(public_bind: bool) -> Result<()> {
    if !public_bind {
        return Ok(());
    }
    for (name, transport) in [
        ("IRONCREW_MCP_ALLOWED_COMMANDS", "stdio"),
        ("IRONCREW_MCP_ALLOWED_HTTP_HOSTS", "HTTP"),
    ] {
        if !matches!(std::env::var(name), Ok(value) if !value.trim().is_empty()) {
            return Err(IronCrewError::Validation(format!(
                "Public server binds require {name}; set an exact allowlist or __disabled__ to disable {transport} MCP"
            )));
        }
    }
    Ok(())
}

pub async fn cmd_serve(host: &str, port: u16, flows_dir: &Path) -> Result<()> {
    use axum::extract::DefaultBodyLimit;
    use axum::http;
    use tower_http::cors::{AllowOrigin, CorsLayer};

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

    // Bootstrap the persistence store ONCE at server startup. Every
    // request handler below reuses `state.store` — this avoids per-call
    // Postgres migrations and keeps one connection pool across the
    // server's lifetime.
    let store = crate::engine::store::create_store(flows_dir.join(".ironcrew"))
        .await
        .map_err(|e| IronCrewError::Validation(format!("Failed to init store: {}", e)))?;

    // Reconcile only legacy or expired run leases. Healthy work owned by
    // another Railway/OpenShift replica remains untouched.
    let _ = crate::engine::reconciler::reconcile_stuck_runs(&store)
        .await
        .map_err(|e| {
            tracing::error!("Reconciler failed (non-fatal): {e}");
        });

    let max_active_conversations = api::conversations::max_active_conversations();
    let max_active_runs = api::handlers::max_active_runs();
    let max_sse_connections = api::handlers::max_sse_connections();
    let max_run_lifetime = api::handlers::max_run_lifetime();
    let idempotency = api::idempotency::IdempotencyConfig::from_env(max_run_lifetime)?;
    let pruned_idempotency_records = store
        .prune_idempotency(&chrono::Utc::now().to_rfc3339(), idempotency.prune_batch)
        .await
        .map_err(|error| {
            IronCrewError::Validation(format!(
                "Failed to prune the idempotency ledger at startup: {error}"
            ))
        })?;
    if pruned_idempotency_records > 0 {
        tracing::info!(
            count = pruned_idempotency_records,
            "Pruned expired idempotency records"
        );
    }
    let state = Arc::new(api::AppState {
        flows_dir: flows_dir.clone(),
        auth,
        admission,
        accepting_traffic: std::sync::atomic::AtomicBool::new(true),
        active_runs: Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new())),
        active_conversations: Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new())),
        max_active_conversations,
        conversation_permits: Arc::new(tokio::sync::Semaphore::new(max_active_conversations)),
        max_active_runs,
        run_permits: Arc::new(tokio::sync::Semaphore::new(max_active_runs)),
        max_sse_connections,
        sse_permits: Arc::new(tokio::sync::Semaphore::new(max_sse_connections)),
        max_run_lifetime,
        terminal_persistence_failures: std::sync::atomic::AtomicUsize::new(0),
        store_maintenance_healthy: std::sync::atomic::AtomicBool::new(true),
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
        (heartbeat_store.run_lease_ttl() / 3).max(std::time::Duration::from_secs(1));
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
            let heartbeat_ok = match heartbeat_store.heartbeat_owned_runs().await {
                Ok(count) => {
                    tracing::trace!(count, "Refreshed owned run leases");
                    true
                }
                Err(error) => {
                    tracing::error!(%error, "Failed to refresh owned run leases");
                    false
                }
            };
            let now = chrono::Utc::now().to_rfc3339();
            let reconcile_ok = match heartbeat_store.reconcile_abandoned_runs(&now).await {
                Ok(0) => true,
                Ok(count) => {
                    tracing::warn!(count, "Reconciled expired run leases");
                    true
                }
                Err(error) => {
                    tracing::error!(%error, "Failed to reconcile expired run leases");
                    false
                }
            };
            heartbeat_state.store_maintenance_healthy.store(
                heartbeat_ok && reconcile_ok,
                std::sync::atomic::Ordering::Release,
            );
        }
    });

    // Background task: evict idle chat session handles.
    tokio::spawn(api::conversations::idle_eviction_loop(state.clone()));

    // CORS: use IRONCREW_CORS_ORIGINS env var (comma-separated) or deny all
    let cors = match std::env::var("IRONCREW_CORS_ORIGINS") {
        Ok(origins) if origins == "*" => CorsLayer::permissive(),
        Ok(origins) => {
            let allowed: Vec<http::HeaderValue> = origins
                .split(',')
                .filter(|origin| !origin.trim().is_empty())
                .map(|origin| {
                    origin.trim().parse().map_err(|error| {
                        IronCrewError::Validation(format!(
                            "Invalid IRONCREW_CORS_ORIGINS entry {:?}: {error}",
                            origin.trim()
                        ))
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            CorsLayer::new()
                .allow_origin(AllowOrigin::list(allowed))
                .allow_methods([
                    http::Method::GET,
                    http::Method::POST,
                    http::Method::DELETE,
                    http::Method::OPTIONS,
                ])
                .allow_headers([
                    http::HeaderName::from_static("authorization"),
                    http::HeaderName::from_static("content-type"),
                    api::idempotency::IDEMPOTENCY_KEY_HEADER,
                    api::idempotency::IDEMPOTENCY_RECOVERY_KEY_HEADER,
                ])
                .expose_headers([
                    api::idempotency::IDEMPOTENCY_REPLAYED_HEADER,
                    http::header::RETRY_AFTER,
                ])
        }
        Err(_) => CorsLayer::new(), // no origins allowed by default
    };

    // Request body size limit (default 10MB, configurable via IRONCREW_MAX_BODY_SIZE)
    let max_body = bounded_env_u64(
        "IRONCREW_MAX_BODY_SIZE",
        10 * 1024 * 1024,
        1,
        MAX_REQUEST_BODY_HARD_LIMIT as u64,
    )? as usize;

    let app = api::create_router(state.clone())
        .layer(cors)
        .layer(DefaultBodyLimit::max(max_body));

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

    // Hard deadline applied *after* the shutdown signal fires — if
    // clients hold connections open past this budget we exit anyway
    // instead of hanging the process. Configurable via
    // `IRONCREW_SHUTDOWN_TIMEOUT_SECS` (default 10 s).
    let shutdown_timeout_secs = bounded_env_u64(
        "IRONCREW_SHUTDOWN_TIMEOUT_SECS",
        10,
        1,
        MAX_SHUTDOWN_TIMEOUT_SECS,
    )?;

    // Signal-flag channel: the graceful-shutdown future fires `tx` the
    // moment a signal arrives so the hard-deadline timer can start
    // counting from that point (not from server startup).
    let (tx_signaled, rx_signaled) = tokio::sync::oneshot::channel::<()>();
    let mut tx_signaled = Some(tx_signaled);

    // Graceful shutdown: listen for SIGTERM (Kubernetes) and Ctrl+C. On
    // signal, actively tear down the per-session state so long-lived SSE
    // streams terminate and axum's graceful-shutdown future can resolve.
    // Without this, axum waits for every in-flight EventSource
    // connection to complete, which never happens with keepalives.
    let shutdown_state = state.clone();
    let shutdown = async move {
        let ctrl_c = tokio::signal::ctrl_c();
        #[cfg(unix)]
        {
            let mut sigterm =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                    .expect("Failed to register SIGTERM handler");
            tokio::select! {
                _ = ctrl_c => tracing::info!("Received Ctrl+C, shutting down"),
                _ = sigterm.recv() => tracing::info!("Received SIGTERM, shutting down"),
            }
        }
        #[cfg(not(unix))]
        {
            ctrl_c.await.ok();
            tracing::info!("Received Ctrl+C, shutting down");
        }

        // Start the hard-deadline clock as early as possible so the
        // teardown below can't blow past the budget.
        if let Some(tx) = tx_signaled.take() {
            let _ = tx.send(());
        }

        // Fail readiness before cancelling or draining any work, allowing
        // Railway/OpenShift routing to stop sending new requests first.
        shutdown_state
            .accepting_traffic
            .store(false, std::sync::atomic::Ordering::Release);

        // Take ownership of all active runs, abort their work, then wait until
        // each monitor has persisted and emitted its terminal state before
        // dropping the handles/event buses.
        let mut active_runs: Vec<api::ActiveRun> = {
            let mut map = shutdown_state.active_runs.write().await;
            map.drain().map(|(_, run)| run).collect()
        };
        let run_count = active_runs.len();
        for run in &active_runs {
            run.abort_handle.abort();
        }
        for run in &mut active_runs {
            if !*run.terminal.borrow() {
                let _ = run.terminal.changed().await;
            }
        }
        if run_count > 0 {
            tracing::info!(run_count, "Terminalized active runs");
        }
        drop(active_runs);

        // Cancel active chat turns, wait for their rollback guards to release
        // the per-session lock, then make one final revision-guarded save.
        // Holding the drained Arcs keeps event buses and admission permits
        // alive until this drain is complete.
        let active_conversations = {
            let mut map = shutdown_state.active_conversations.write().await;
            map.drain().map(|(_, handle)| handle).collect::<Vec<_>>()
        };
        for handle in &active_conversations {
            handle.shutdown.send_replace(true);
        }
        for handle in &active_conversations {
            let _turn_guard = handle.turn_lock.lock().await;
            if let Err(error) = handle.conv.persist().await {
                tracing::warn!(
                    conversation_id = %handle.id,
                    %error,
                    "Failed to persist conversation while draining shutdown"
                );
            }
        }
        if !active_conversations.is_empty() {
            tracing::info!(
                count = active_conversations.len(),
                "Cancelled and closed active chat sessions"
            );
        }
        drop(active_conversations);
    };

    let serve_fut = axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown);

    // Race the server against a post-signal timeout. The timeout future
    // first waits for the signal, then sleeps `shutdown_timeout_secs`;
    // if axum hasn't finished by then we exit anyway.
    let hard_deadline = async move {
        let _ = rx_signaled.await;
        tokio::time::sleep(std::time::Duration::from_secs(shutdown_timeout_secs)).await;
    };

    tokio::select! {
        result = serve_fut => {
            result.map_err(|e| IronCrewError::Validation(format!("Server error: {}", e)))?;
        }
        _ = hard_deadline => {
            tracing::warn!(
                "Graceful shutdown exceeded {}s — exiting anyway",
                shutdown_timeout_secs
            );
        }
    }

    heartbeat_handle.abort();
    let _ = heartbeat_handle.await;

    // Post-serve drain window: background tasks spawned from `Drop` paths
    // (notably `McpConnectionManager::shutdown_blocking` for reaping stdio
    // MCP child processes) need a moment to complete before the tokio
    // runtime tears them down. Configurable for cloud deployments with
    // tight SIGTERM grace periods (Kubernetes `terminationGracePeriodSeconds`).
    let drain_ms = bounded_env_u64("IRONCREW_SHUTDOWN_DRAIN_MS", 1000, 0, MAX_SHUTDOWN_DRAIN_MS)?;
    if drain_ms > 0 {
        tracing::info!(drain_ms, "Draining background shutdown tasks");
        tokio::time::sleep(std::time::Duration::from_millis(drain_ms)).await;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::public_bind_requires_auth;

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
}
