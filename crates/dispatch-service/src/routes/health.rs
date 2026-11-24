use axum::{
    extract::{Path, State},
    routing::get,
    Json, Router,
};
use serde_json::{json, Value};

use crate::server::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/health", get(health))
        .route("/version", get(version))
        .route("/chains", get(chains))
        .route("/block/:chain_id", get(block_number))
        .route("/metrics", get(metrics))
}

async fn metrics() -> ([(axum::http::HeaderName, &'static str); 1], Vec<u8>) {
    (
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; version=0.0.4",
        )],
        crate::metrics::render(),
    )
}

async fn health() -> Json<Value> {
    Json(json!({ "status": "ok" }))
}

async fn version() -> Json<Value> {
    Json(json!({
        "version": env!("CARGO_PKG_VERSION"),
        "service": env!("CARGO_PKG_NAME"),
    }))
}

async fn chains(State(state): State<AppState>) -> Json<Value> {
    Json(json!({ "supported": state.config.chains.supported }))
}

/// Unauthenticated probe endpoint — returns the current block number for a chain.
/// Used by gateways to track chain head for QoS freshness scoring.
async fn block_number(Path(chain_id): Path<u64>, State(state): State<AppState>) -> Json<Value> {
    let Some(backend_url) = state.config.chains.backends.get(&chain_id.to_string()) else {
        return Json(json!({ "error": "chain not supported" }));
    };

    let req = json!({ "jsonrpc": "2.0", "method": "eth_blockNumber", "params": [], "id": 1 });
    match state.http_client.post(backend_url).json(&req).send().await {
        Ok(resp) if resp.status().is_success() => match resp.json::<Value>().await {
            Ok(json) => Json(json),
            Err(_) => Json(json!({ "error": "invalid backend response" })),
        },
        _ => Json(json!({ "error": "backend unavailable" })),
    }
}
