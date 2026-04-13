use axum::{
    extract::State,
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use serde_json::json;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::{mpsc, RwLock};
use tracing::info;

use crate::{config::Config, handler::handle_messages};

pub type SharedState = Arc<RwLock<Config>>;

pub async fn run(config: Config, config_rx: mpsc::Receiver<Config>) -> anyhow::Result<()> {
    let addr: SocketAddr = format!("{}:{}", config.proxy.host, config.proxy.port)
        .parse()
        .map_err(|e| anyhow::anyhow!("Invalid bind address: {}", e))?;

    let state: SharedState = Arc::new(RwLock::new(config));

    // Spawn config reload listener
    let reload_state = state.clone();
    tokio::spawn(async move {
        let mut rx = config_rx;
        while let Some(new_config) = rx.recv().await {
            let mut guard = reload_state.write().await;
            *guard = new_config;
        }
    });

    let app = Router::new()
        .route("/v1/messages", post(handle_messages))
        .route("/health", get(health_handler))
        .route("/status", get(status_handler))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(&addr).await?;
    info!("ccrouter listening on http://{}", addr);

    axum::serve(listener, app).await?;
    Ok(())
}

async fn health_handler() -> impl IntoResponse {
    (StatusCode::OK, Json(json!({ "status": "ok" })))
}

async fn status_handler(State(state): State<SharedState>) -> impl IntoResponse {
    let config = state.read().await;
    let profile = config.active_profile();
    let resp = json!({
        "active_profile": config.active.profile,
        "provider": profile.map(|p| json!({
            "id": p.id,
            "name": p.name,
            "base_url": p.base_url,
            "format": format!("{:?}", p.format),
        })),
        "proxy": {
            "host": config.proxy.host,
            "port": config.proxy.port,
        }
    });
    Json(resp)
}
