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

    let state = Arc::new(api::AppState {
        flows_dir: flows_dir.clone(),
        active_runs: Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new())),
    });

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

    let app = api::create_router(state)
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
    println!("  GET    /nodes                         - List built-in tools");

    // Graceful shutdown: listen for SIGTERM (Kubernetes) and Ctrl+C
    let shutdown = async {
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
    };

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown)
        .await
        .map_err(|e| IronCrewError::Validation(format!("Server error: {}", e)))?;

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
