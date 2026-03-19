use std::path::Path;
use std::sync::Arc;

use crate::api;
use crate::utils::error::{IronCrewError, Result};

pub async fn cmd_serve(host: &str, port: u16, flows_dir: &Path) -> Result<()> {
    use tower_http::cors::CorsLayer;

    // Load .env from CWD
    dotenvy::dotenv().ok();

    let flows_dir = std::fs::canonicalize(flows_dir).unwrap_or_else(|_| flows_dir.to_path_buf());

    let state = Arc::new(api::AppState {
        flows_dir: flows_dir.clone(),
    });

    let app = api::create_router(state).layer(CorsLayer::permissive());

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
    println!("  POST   /flows/{{flow}}/run             - Run a crew");
    println!("  GET    /flows/{{flow}}/runs            - List runs for a flow");
    println!("  GET    /flows/{{flow}}/runs/{{id}}       - Get run details");
    println!("  DELETE /flows/{{flow}}/runs/{{id}}       - Delete a run");
    println!("  GET    /flows/{{flow}}/validate         - Validate a flow");
    println!("  GET    /flows/{{flow}}/agents           - List agents in a flow");
    println!("  GET    /nodes                         - List built-in tools");

    axum::serve(listener, app)
        .await
        .map_err(|e| IronCrewError::Validation(format!("Server error: {}", e)))?;

    Ok(())
}
