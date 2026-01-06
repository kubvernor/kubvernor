// kubvernor-api: API definitions and types for kubvernor

use std::sync::Arc;

use axum::{Json, Router, extract::State, http::StatusCode, response::IntoResponse, routing::get};
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;

#[derive(Clone)]
pub struct AppState {
    // Add any state fields needed for admin operations
}

#[derive(Debug, Serialize, Deserialize)]
pub struct AdminResponse {
    pub status: String,
    pub message: String,
}

/// Admin handler - GET endpoint
pub async fn admin(State(_state): State<Arc<AppState>>) -> impl IntoResponse {
    let response = AdminResponse { status: "ok".to_string(), message: "Admin endpoint is running".to_string() };

    (StatusCode::OK, Json(response))
}

/// Create and return the admin router
pub fn create_router(state: Arc<AppState>) -> Router {
    Router::new().route("/admin", get(admin)).with_state(state)
}

/// Start the admin server on the specified port
pub async fn start_server(port: u16) -> Result<(), Box<dyn std::error::Error>> {
    let state = Arc::new(AppState {});
    let app = create_router(state);

    let listener = TcpListener::bind(format!("0.0.0.0:{}", port)).await?;
    let local_addr = listener.local_addr()?;

    tracing::info!("Admin server listening on http://{}", local_addr);

    axum::serve(listener, app).await?;

    Ok(())
}
