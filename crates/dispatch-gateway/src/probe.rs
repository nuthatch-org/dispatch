//! Background probe task.
//!
//! Every `probe_interval_secs` seconds, sends a synthetic `eth_blockNumber`
//! to every provider for every chain they serve. Updates latency, availability,
//! and freshness (latest block) in each provider's QoS state.

use std::time::Instant;

use serde_json::Value;
use tokio::time::{interval, Duration, MissedTickBehavior};

use crate::server::AppState;

pub async fn run(state: AppState) {
    let probe_secs = state.config.qos.probe_interval_secs;
    let mut tick = interval(Duration::from_secs(probe_secs));
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);

    tracing::info!(probe_interval_secs = probe_secs, "probe task started");

    loop {
        tick.tick().await;
        probe_all(&state).await;
    }
}

async fn probe_all(state: &AppState) {
    // Snapshot the provider list into owned values so the registry guard can be dropped
    // before spawning tasks (which need to be 'static).
    let tasks: Vec<_> = {
        let registry = state.registry.load();
        registry
            .all_providers()
            .iter()
            .flat_map(|p| {
                let cs_map: Vec<_> = p
                    .chains
                    .iter()
                    .map(|&cid| (p.clone(), cid, registry.chain_state(cid)))
                    .collect();
                cs_map
            })
            .collect()
    }; // registry guard dropped here

    for (provider, chain_id, chain_state) in tasks {
        {
            let client = state.http_client.clone();
            let provider = provider.clone();

            tokio::spawn(async move {
                // Use the unauthenticated /block/{chain_id} endpoint — avoids
                // needing a signed TAP receipt just to probe for block freshness.
                let url = format!("{}/block/{}", provider.endpoint, chain_id);

                let start = Instant::now();

                // An answer without a block number is not an answer.
                let answered = match client.get(&url).send().await {
                    Ok(resp) if resp.status().is_success() => resp
                        .json::<Value>()
                        .await
                        .ok()
                        .and_then(|j| parse_block_number(&j)),
                    _ => None,
                };
                {
                    let mut probes = provider.probes.lock().unwrap();
                    let rec = probes.entry(chain_id).or_default();
                    match answered {
                        Some(block) => {
                            rec.last_ok = Some(Instant::now());
                            rec.block = Some(block);
                            rec.consecutive_failures = 0;
                        }
                        None => rec.consecutive_failures += 1,
                    }
                }

                match answered {
                    Some(block) => {
                        let ms = start.elapsed().as_millis() as u64;
                        provider.qos.record_success(ms);
                        provider.qos.update_latest_block(block);
                        if let Some(cs) = chain_state {
                            cs.update_head(block);
                        }
                        tracing::debug!(
                            provider = %provider.endpoint,
                            chain_id,
                            latency_ms = ms,
                            block,
                            "probe ok"
                        );
                    }
                    None => {
                        provider.qos.record_failure();
                        tracing::debug!(provider = %provider.endpoint, chain_id, "probe failed");
                    }
                }
            });
        }
    }
}

/// Parse the result of an eth_blockNumber response into a u64 block number.
fn parse_block_number(json: &Value) -> Option<u64> {
    let hex = json.get("result")?.as_str()?;
    let stripped = hex.strip_prefix("0x").unwrap_or(hex);
    u64::from_str_radix(stripped, 16).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_block_number_valid() {
        let v = json!({ "jsonrpc": "2.0", "result": "0x1a2b3c", "id": 1 });
        assert_eq!(parse_block_number(&v), Some(0x1a2b3c));
    }

    #[test]
    fn parse_block_number_missing_result() {
        let v = json!({ "jsonrpc": "2.0", "error": { "code": -32000, "message": "err" } });
        assert_eq!(parse_block_number(&v), None);
    }
}
