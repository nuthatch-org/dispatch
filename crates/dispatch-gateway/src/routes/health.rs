use axum::{extract::State, routing::get, Json, Router};
use serde_json::{json, Value};

use crate::server::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/health", get(health))
        .route("/version", get(version))
        .route("/providers/:chain_id", get(providers_for_chain))
        .route("/pricing", get(pricing))
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

async fn providers_for_chain(
    State(state): State<AppState>,
    axum::extract::Path(chain_id): axum::extract::Path<u64>,
) -> Json<Value> {
    let registry = state.registry.load();
    match registry.providers_for_chain(chain_id) {
        None => Json(json!({ "chain_id": chain_id, "providers": [] })),
        Some((providers, head)) => {
            let list: Vec<Value> = providers
                .iter()
                .map(|p| {
                    json!({
                        "address": format!("{:?}", p.address),
                        "endpoint": p.endpoint,
                        "score": p.qos.score(head),
                    })
                })
                .collect();
            Json(json!({ "chain_id": chain_id, "chain_head": head, "providers": list }))
        }
    }
}

async fn pricing(State(state): State<AppState>) -> Json<Value> {
    match &state.pricer {
        Some(p) => Json(p.price_list()),
        None => Json(json!({
            "currency": "GRT",
            "grt_wei_per_cu": state.config.tap.base_price_per_cu.to_string(),
        })),
    }
}
