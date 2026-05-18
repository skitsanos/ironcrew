use std::path::Path;
use std::sync::Arc;

use crate::api;
use crate::utils::error::{IronCrewError, Result};

pub async fn cmd_serve(host: &str, port: u16, flows_dir: &Path) -> Result<()> {
    use axum::extract::DefaultBodyLimit;
    use axum::http;
    use tower_http::cors::{AllowOrigin, CorsLayer};

    // Load .env from CWD
    dotenvy::dotenv().ok();

    let flows_dir = std::fs::canonicalize(flows_dir).unwrap_or_else(|_| flows_dir.to_path_buf());

    // Bootstrap the persistence store ONCE at server startup. Every
    // request handler below reuses `state.store` — this avoids per-call
    // Postgres migrations and keeps one connection pool across the
    // server's lifetime.
    let store = crate::engine::store::create_store(flows_dir.join(".ironcrew"))
        .await
        .map_err(|e| IronCrewError::Validation(format!("Failed to init store: {}", e)))?;

    // Flip any runs left in Running state by a prior crashed ironcrew
    // serve process. Single-instance assumption (see docs/superpowers/
    // specs/2026-04-23-stuck-run-reconciler-design.md §11).
    let _ = crate::engine::reconciler::reconcile_stuck_runs(&store)
        .await
        .map_err(|e| {
            tracing::error!("Reconciler failed (non-fatal): {e}");
        });

    let state = Arc::new(api::AppState {
        flows_dir: flows_dir.clone(),
        active_runs: Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new())),
        active_conversations: Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new())),
        max_active_conversations: api::conversations::max_active_conversations(),
        store,
    });

    // Background task: evict idle chat session handles.
    tokio::spawn(api::conversations::idle_eviction_loop(state.clone()));

    // CORS: use IRONCREW_CORS_ORIGINS env var (comma-separated) or deny all
    let cors = match std::env::var("IRONCREW_CORS_ORIGINS") {
        Ok(origins) if origins == "*" => CorsLayer::permissive(),
        Ok(origins) => {
            let allowed: Vec<_> = origins
                .split(',')
                .filter_map(|s| s.trim().parse().ok())
                .collect();
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
                ])
        }
        Err(_) => CorsLayer::new(), // no origins allowed by default
    };

    // Request body size limit (default 10MB, configurable via IRONCREW_MAX_BODY_SIZE)
    let max_body: usize = std::env::var("IRONCREW_MAX_BODY_SIZE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(10 * 1024 * 1024);

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
    println!("  POST   /flows/{{flow}}/run             - Run a crew (async, returns run_id)");
    println!("  POST   /flows/{{flow}}/abort/{{run_id}}  - Abort a running crew");
    println!("  GET    /flows/{{flow}}/events/{{run_id}} - SSE event stream for a run");
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
    let shutdown_timeout_secs: u64 = std::env::var("IRONCREW_SHUTDOWN_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(10);

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

        // Drop all active chat sessions — each handle owns the per-session
        // EventBus; dropping it closes the broadcast channel, so SSE
        // subscribers' `rx.recv()` returns `Closed` and the streams end.
        {
            let mut map = shutdown_state.active_conversations.write().await;
            let count = map.len();
            map.clear();
            if count > 0 {
                tracing::info!(count, "Closed active chat sessions");
            }
        }

        // Same for active crew runs — abort them and drop their event
        // buses so any SSE subscriber on `/events/{run_id}` unblocks.
        {
            let mut map = shutdown_state.active_runs.write().await;
            let count = map.len();
            for (_, run) in map.drain() {
                run.abort_handle.abort();
            }
            if count > 0 {
                tracing::info!(count, "Aborted active runs");
            }
        }
    };

    let serve_fut = axum::serve(listener, app).with_graceful_shutdown(shutdown);

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

    // Post-serve drain window: background tasks spawned from `Drop` paths
    // (notably `McpConnectionManager::shutdown_blocking` for reaping stdio
    // MCP child processes) need a moment to complete before the tokio
    // runtime tears them down. Configurable for cloud deployments with
    // tight SIGTERM grace periods (Kubernetes `terminationGracePeriodSeconds`).
    let drain_ms: u64 = std::env::var("IRONCREW_SHUTDOWN_DRAIN_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1000);
    if drain_ms > 0 {
        tracing::info!(drain_ms, "Draining background shutdown tasks");
        tokio::time::sleep(std::time::Duration::from_millis(drain_ms)).await;
    }

    Ok(())
}
