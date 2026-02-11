use crate::daemon::health::{HealthSnapshot, ScriptHealth};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use tokio::net::TcpListener;
use tokio::sync::watch;

pub async fn run_http_server(
    bind: String,
    port: u16,
    health: HealthSnapshot,
    mut shutdown_rx: watch::Receiver<bool>,
) {
    let app = Router::new()
        .route("/health", get(all_health))
        .route("/health/{name}", get(script_health))
        .with_state(health);

    let addr = format!("{}:{}", bind, port);
    let listener = match TcpListener::bind(&addr).await {
        Ok(l) => l,
        Err(e) => {
            tracing::error!(error = ?e, addr = %addr, "Failed to bind HTTP server");
            return;
        }
    };

    tracing::info!(addr = %addr, "HTTP health server listening");

    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            let _ = shutdown_rx.wait_for(|v| *v).await;
        })
        .await
        .unwrap_or_else(|e| {
            tracing::error!(error = ?e, "HTTP server error");
        });

    tracing::info!("HTTP health server stopped");
}

async fn all_health(State(health): State<HealthSnapshot>) -> impl IntoResponse {
    let snapshot = health.read().await;
    let scripts: Vec<ScriptHealth> = snapshot.values().cloned().collect();
    let any_failed = scripts.iter().any(|s| !s.healthy);

    let status = if any_failed {
        StatusCode::SERVICE_UNAVAILABLE
    } else {
        StatusCode::OK
    };

    (status, Json(scripts))
}

async fn script_health(
    State(health): State<HealthSnapshot>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    let snapshot = health.read().await;

    let Some(script) = snapshot.get(&name) else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": format!("script '{}' not found", name)})),
        );
    };

    let status = if script.healthy {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };

    (status, Json(serde_json::to_value(script).unwrap()))
}
