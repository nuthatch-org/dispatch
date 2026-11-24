use std::{collections::BTreeMap, time::Duration};

use axum::{extract::State, routing::get, Json, Router};
use serde_json::{json, Value};

use crate::{registry::Registry, server::AppState};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/health", get(health))
        .route("/version", get(version))
        .route("/providers/:chain_id", get(providers_for_chain))
        .route("/status", get(status))
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

/// Registered against answering, per provider and per chain, from the gateway's own probes.
/// The on-chain registry cannot say whether an endpoint answers; this can.
async fn status(State(state): State<AppState>) -> Json<Value> {
    let registry = state.registry.load();
    let window = state.config.answering_window();
    Json(status_report(&registry, window))
}

pub fn status_report(registry: &Registry, window: Duration) -> Value {
    let mut chains: BTreeMap<u64, (u64, usize, usize)> = BTreeMap::new();
    let mut answering_total = 0;

    let providers: Vec<Value> = registry
        .all_providers()
        .iter()
        .map(|p| {
            let probes = p.probes.lock().unwrap().clone();
            let mut any = false;
            let per_chain: Vec<Value> = p
                .chains
                .iter()
                .map(|&chain_id| {
                    let head = registry.providers_for_chain(chain_id).map_or(0, |(_, h)| h);
                    let rec = probes.get(&chain_id).cloned().unwrap_or_default();
                    let answering = rec.last_ok.is_some_and(|t| t.elapsed() <= window);
                    any |= answering;
                    let entry = chains.entry(chain_id).or_insert((head, 0, 0));
                    entry.1 += 1;
                    entry.2 += answering as usize;
                    let mut tiers: Vec<String> = p
                        .chain_capabilities
                        .get(&chain_id)
                        .map(|t| t.iter().map(|t| format!("{t:?}").to_lowercase()).collect())
                        .unwrap_or_default();
                    tiers.sort();
                    json!({
                        "chain_id": chain_id,
                        "answering": answering,
                        "tiers": tiers,
                        "block": rec.block,
                        "head_lag": rec.block.map(|b| head.saturating_sub(b)),
                        "last_ok_secs_ago": rec.last_ok.map(|t| t.elapsed().as_secs()),
                        "consecutive_failures": rec.consecutive_failures,
                    })
                })
                .collect();
            answering_total += any as usize;
            json!({
                "address": format!("{:?}", p.address),
                "endpoint": p.endpoint,
                "region": p.region,
                "answering": any,
                "chains": per_chain,
            })
        })
        .collect();

    let chains: Vec<Value> = chains
        .into_iter()
        .map(|(chain_id, (head, registered, answering))| {
            json!({
                "chain_id": chain_id,
                "head": head,
                "registered": registered,
                "answering": answering,
            })
        })
        .collect();

    json!({
        "registered": providers.len(),
        "answering": answering_total,
        "chains": chains,
        "providers": providers,
        "tiers": {
            "standard": "eth_* against recent state (about the last 128 blocks)",
            "archive": "eth_* against any historical block",
            "debug": "debug_* and trace_*",
        },
    })
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

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::*;
    use crate::{config::ProviderConfig, registry::ProbeRecord};

    #[test]
    fn a_registered_provider_that_never_answered_is_not_answering() {
        let cfg = |b: u8| -> ProviderConfig {
            toml::from_str(&format!(
                "address = \"{:?}\"\nendpoint = \"http://p{b}\"\nchains = [42161]\ncapabilities = [\"standard\", \"archive\"]",
                alloy_primitives::Address::repeat_byte(b)
            ))
            .unwrap()
        };
        let registry = Registry::from_config(&[cfg(1), cfg(2)]);
        registry.chain_state(42161).unwrap().update_head(1_000);
        registry.all_providers()[0].probes.lock().unwrap().insert(
            42161,
            ProbeRecord {
                last_ok: Some(Instant::now()),
                block: Some(990),
                consecutive_failures: 0,
            },
        );

        let r = status_report(&registry, Duration::from_secs(30));
        assert_eq!(r["registered"], 2);
        assert_eq!(r["answering"], 1);
        assert_eq!(r["chains"][0]["answering"], 1);
        assert_eq!(r["providers"][0]["chains"][0]["head_lag"], 10);
        assert_eq!(
            r["providers"][0]["chains"][0]["tiers"],
            json!(["archive", "standard"])
        );
        assert_eq!(r["providers"][1]["answering"], false);
    }
}
