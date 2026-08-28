use std::{collections::HashMap, net::SocketAddr, sync::Arc, time::Instant};

use alloy_primitives::{keccak256, Address, Bytes};
use axum::{
    extract::{ConnectInfo, Path, State},
    http::HeaderMap,
    response::{IntoResponse, Response},
    routing::post,
    Json, Router,
};
use futures_util::future::join_all;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::{
    affinity, config::CapabilityTier, error::GatewayError, metrics, registry::Provider, selector,
    server::AppState,
};
use dispatch_tap::create_receipt;

pub fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/rpc/:chain_id/:consumer_address",
            post(rpc_handler_with_consumer),
        )
        .route("/rpc/:chain_id", post(rpc_handler))
        .route("/rpc", post(rpc_handler_unified))
}

/// Route handler for POST /rpc/{chain_id}
async fn rpc_handler(
    State(state): State<AppState>,
    Path(chain_id): Path<u64>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Response, GatewayError> {
    let consumer = extract_consumer_address(&headers)?;
    dispatch_rpc(state, chain_id, peer, consumer, body).await
}

/// Route handler for POST /rpc/{chain_id}/{consumer_address}
///
/// Consumer address embedded in the URL path — for clients that cannot set
/// custom HTTP headers (e.g. graph-node, Ethereum execution clients).
/// The address must be a valid checksummed or lowercase Ethereum address.
async fn rpc_handler_with_consumer(
    State(state): State<AppState>,
    Path((chain_id, consumer_str)): Path<(u64, String)>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Json(body): Json<Value>,
) -> Result<Response, GatewayError> {
    let consumer = consumer_str
        .parse::<Address>()
        .map_err(|_| GatewayError::InvalidConsumerAddress(consumer_str))?;
    dispatch_rpc(state, chain_id, peer, consumer, body).await
}

/// Unified multi-chain endpoint — chain selected via the `X-Chain-Id` header.
/// Defaults to Ethereum mainnet (chain ID 1) when the header is absent or unparseable.
async fn rpc_handler_unified(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Response, GatewayError> {
    let chain_id: u64 = headers
        .get("x-chain-id")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse().ok())
        .unwrap_or(1);
    let consumer = extract_consumer_address(&headers)?;
    dispatch_rpc(state, chain_id, peer, consumer, body).await
}

/// Shared dispatch logic. Applies rate limit, then handles single or batch JSON-RPC.
async fn dispatch_rpc(
    state: AppState,
    chain_id: u64,
    peer: SocketAddr,
    consumer: Address,
    body: Value,
) -> Result<Response, GatewayError> {
    // --- Per-IP rate limiting ---
    if let Some(limiter) = &state.rate_limiter {
        if limiter.check_key(&peer.ip()).is_err() {
            return Err(GatewayError::RateLimited);
        }
    }

    match body {
        Value::Array(items) => {
            if items.is_empty() {
                return Err(GatewayError::InvalidRequest("empty batch".to_string()));
            }
            let requests: Vec<JsonRpcRequest> = items
                .into_iter()
                .map(serde_json::from_value)
                .collect::<Result<_, _>>()
                .map_err(|e| GatewayError::InvalidRequest(e.to_string()))?;

            let responses: Vec<Value> = join_all(
                requests
                    .iter()
                    .map(|req| process_request(&state, chain_id, req, consumer)),
            )
            .await
            .into_iter()
            .map(|r| match r {
                Ok((resp, _attestation)) => serde_json::to_value(resp).unwrap_or(Value::Null),
                Err(e) => json!({
                    "jsonrpc": "2.0",
                    "id": Value::Null,
                    "error": { "code": -32000, "message": e.to_string() }
                }),
            })
            .collect();

            Ok(Json(Value::Array(responses)).into_response())
        }
        Value::Object(_) => {
            let request: JsonRpcRequest = serde_json::from_value(body)
                .map_err(|e| GatewayError::InvalidRequest(e.to_string()))?;
            let (response, attestation) =
                process_request(&state, chain_id, &request, consumer).await?;
            let mut resp = Json(response).into_response();
            if let Some(att) = attestation {
                if let Ok(val) = att.parse() {
                    resp.headers_mut().insert("x-drpc-attestation", val);
                }
            }
            Ok(resp)
        }
        _ => Err(GatewayError::InvalidRequest(
            "expected JSON object or array".to_string(),
        )),
    }
}

/// Extract and validate the `X-Consumer-Address` header.
///
/// Optional in the gateway-pays model — the receipt metadata always encodes
/// `gateway_payer_address`, not the consumer address. The header is accepted
/// for logging/analytics but defaults to `Address::ZERO` when absent.
fn extract_consumer_address(headers: &HeaderMap) -> Result<Address, GatewayError> {
    let Some(raw) = headers.get("x-consumer-address") else {
        return Ok(Address::ZERO);
    };
    let raw = raw
        .to_str()
        .map_err(|_| GatewayError::InvalidConsumerAddress("non-UTF8 header value".to_string()))?;

    raw.parse::<Address>()
        .map_err(|_| GatewayError::InvalidConsumerAddress(raw.to_string()))
}

// ---------------------------------------------------------------------------
// JSON-RPC types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
    pub method: String,
    #[serde(default)]
    pub params: Option<Value>,
    pub id: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<Value>,
    pub id: Option<Value>,
}

impl JsonRpcRequest {
    pub fn validate(&self) -> Result<(), GatewayError> {
        if self.jsonrpc != "2.0" {
            return Err(GatewayError::InvalidRequest(format!(
                "unsupported jsonrpc version: {}",
                self.jsonrpc
            )));
        }
        if self.method.is_empty() {
            return Err(GatewayError::InvalidRequest("method is empty".to_string()));
        }
        Ok(())
    }
}

/// Returns true if the request targets a specific historical block, requiring
/// an Archive-tier provider. Standard nodes only retain ~128 recent blocks.
fn requires_archive(method: &str, params: &Option<Value>) -> bool {
    fn is_historical(tag: &Value) -> bool {
        match tag {
            Value::String(s) => !matches!(s.as_str(), "latest" | "pending" | "safe" | "finalized"),
            Value::Number(_) => true,
            _ => false,
        }
    }

    let Some(Value::Array(arr)) = params.as_ref() else {
        return false;
    };

    match method {
        // blockTag is the second parameter (index 1).
        "eth_getBalance" | "eth_getCode" | "eth_getTransactionCount" | "eth_call" => {
            arr.get(1).is_some_and(is_historical)
        }
        // blockTag is the third parameter (index 2).
        "eth_getStorageAt" => arr.get(2).is_some_and(is_historical),
        // blockTag is the first parameter (index 0).
        "eth_getBlockByNumber" => arr.first().is_some_and(is_historical),
        // Filter object may contain fromBlock / toBlock.
        "eth_getLogs" => {
            if let Some(Value::Object(filter)) = arr.first() {
                filter.get("fromBlock").is_some_and(is_historical)
                    || filter.get("toBlock").is_some_and(is_historical)
            } else {
                false
            }
        }
        _ => false,
    }
}

/// Map a JSON-RPC method + params to the minimum capability tier required to serve it.
fn required_tier(method: &str, params: &Option<Value>) -> CapabilityTier {
    if method.starts_with("debug_") || method.starts_with("trace_") {
        CapabilityTier::Debug
    } else if requires_archive(method, params) {
        CapabilityTier::Archive
    } else {
        CapabilityTier::Standard
    }
}

/// Methods whose results are deterministic given the same chain state —
/// send to quorum_k providers and take the majority response.
fn requires_quorum(method: &str) -> bool {
    matches!(
        method,
        "eth_call"
            | "eth_getLogs"
            | "eth_getBalance"
            | "eth_getCode"
            | "eth_getTransactionCount"
            | "eth_getStorageAt"
            | "eth_getBlockByHash"
            | "eth_getTransactionByHash"
            | "eth_getTransactionReceipt"
    )
}

// ---------------------------------------------------------------------------
// Core dispatch
// ---------------------------------------------------------------------------

async fn process_request(
    state: &AppState,
    chain_id: u64,
    request: &JsonRpcRequest,
    consumer: Address,
) -> Result<(JsonRpcResponse, Option<String>), GatewayError> {
    request.validate()?;

    let candidates = {
        let registry = state.registry.load();
        let (providers, chain_head) = registry
            .providers_for_chain(chain_id)
            .ok_or(GatewayError::UnsupportedChain(chain_id))?;

        let tier = required_tier(&request.method, &request.params);
        let capable: Vec<_> = providers
            .iter()
            .filter(|p| {
                p.chain_capabilities
                    .get(&chain_id)
                    .is_some_and(|caps| caps.contains(&tier))
            })
            .cloned()
            .collect();

        if capable.is_empty() {
            return Err(GatewayError::NoProviders(chain_id));
        }

        // A filter lives inside ONE node. Route the follow-up anywhere else and a perfectly
        // healthy provider answers "filter not found", which reads as that provider being broken.
        // Pin to the owner, and deliberately do NOT fail over: a second opinion on a filter id is
        // meaningless, and failing over turns a clear error into an intermittent one.
        let pinned = affinity::pin_provider(
            &state.filter_affinity,
            chain_id,
            &request.method,
            &request.params,
            &capable,
            |p| p.address,
        );
        if let Some(provider) = pinned {
            vec![provider]
        } else {
            selector::select(
                &capable,
                chain_head,
                if requires_quorum(&request.method) {
                    state.config.qos.quorum_k
                } else {
                    state.config.qos.concurrent_k
                },
                state.config.gateway.region.as_deref(),
                state.config.qos.region_bonus,
            )
        }
    };

    let cu = cu_weight_for(&request.method);
    let receipt_value = cu as u128 * state.config.tap.base_price_per_cu;

    // Encode gateway payer address (bytes 0-19) + method name (bytes 20+) in metadata.
    // Providers extract bytes 0-19 for escrow routing — this is always the gateway's
    // operator wallet, which funds escrow for all providers. Consumers are tracked at
    // the gateway level only; they are not exposed to providers on-chain.
    // Bytes 20+ are the method name for per-method analytics and the receipt feed API.
    let mut meta_bytes = Vec::with_capacity(20 + request.method.len());
    meta_bytes.extend_from_slice(state.config.tap.gateway_payer_address.as_slice());
    meta_bytes.extend_from_slice(request.method.as_bytes());
    let consumer_metadata = Bytes::from(meta_bytes);

    let start = Instant::now();

    let (response, attestation, winner) = if requires_quorum(&request.method) {
        dispatch_quorum(
            state,
            chain_id,
            request,
            &candidates,
            receipt_value,
            consumer_metadata,
        )
        .await?
    } else {
        dispatch_concurrent(
            state,
            chain_id,
            request,
            &candidates,
            receipt_value,
            consumer_metadata,
        )
        .await?
    };

    // Remember which node owns a newly created filter, and forget it once uninstalled. Only on a
    // successful response: an errored create has no id to pin, and forgetting on a failed
    // uninstall would strand a filter the node still holds.
    if response.error.is_none() {
        if affinity::creates_filter(&request.method) {
            if let Some(id) = response
                .result
                .as_ref()
                .and_then(affinity::filter_id_from_result)
            {
                state
                    .filter_affinity
                    .remember(chain_id, &id, winner.address);
            }
        } else if affinity::releases_filter(&request.method) {
            if let Some(id) = affinity::filter_id_from_params(&request.params) {
                state.filter_affinity.forget(chain_id, &id);
            }
        }
    }

    let duration = start.elapsed().as_secs_f64();
    let outcome = if response.error.is_some() {
        "error"
    } else {
        "ok"
    };
    metrics::record(chain_id, &request.method, outcome, duration);

    tracing::debug!(
        method = %request.method,
        chain_id,
        provider = %winner.endpoint,
        consumer = %consumer,
        cu,
        "served"
    );

    Ok((response, attestation))
}

// ---------------------------------------------------------------------------
// Attestation verification
// ---------------------------------------------------------------------------

/// Parse and verify the X-Drpc-Attestation header.
/// Returns the recovered signer address as a string, or None if absent/invalid.
fn verify_attestation(
    attestation_header: Option<&str>,
    chain_id: u64,
    method: &str,
    params_json: &str,
    result_json: &str,
) -> Option<String> {
    let header = attestation_header?;

    #[derive(serde::Deserialize)]
    struct Att {
        signer: String,
        signature: String,
    }

    let att: Att = serde_json::from_str(header).ok()?;

    let params_hash = keccak256(params_json.as_bytes());
    let result_hash = keccak256(result_json.as_bytes());
    let mut msg = Vec::with_capacity(8 + method.len() + 64);
    msg.extend_from_slice(&chain_id.to_be_bytes());
    msg.extend_from_slice(method.as_bytes());
    msg.extend_from_slice(params_hash.as_slice());
    msg.extend_from_slice(result_hash.as_slice());
    let msg_hash = keccak256(&msg);

    let recovered = dispatch_tap::recover_signer(msg_hash, &att.signature).ok()?;

    if recovered.to_string().to_lowercase() != att.signer.to_lowercase() {
        tracing::warn!(
            stated = %att.signer,
            recovered = %recovered,
            "attestation signer mismatch"
        );
        return None;
    }

    Some(header.to_string())
}

// ---------------------------------------------------------------------------
// Type aliases for complex dispatch result tuples
// ---------------------------------------------------------------------------

type ProviderResult = (JsonRpcResponse, Option<String>, Arc<Provider>);
type QuorumTaskResult = Result<(JsonRpcResponse, String, Option<String>, Arc<Provider>), String>;

// ---------------------------------------------------------------------------
// Sequential dispatch — try providers in weighted order, fall back on failure.
//
// `candidates` are already ordered by weighted random score (highest weight
// first). The first provider that responds successfully wins and gets the
// receipt. Failures update QoS so the selector naturally routes less traffic
// to unhealthy providers over time.
// ---------------------------------------------------------------------------

async fn dispatch_concurrent(
    state: &AppState,
    chain_id: u64,
    request: &JsonRpcRequest,
    candidates: &[Arc<Provider>],
    receipt_value: u128,
    consumer_metadata: Bytes,
) -> Result<(JsonRpcResponse, Option<String>, Arc<Provider>), GatewayError> {
    let params_json = serde_json::to_string(&request.params).unwrap_or_else(|_| "null".to_string());

    for provider in candidates {
        let p = provider.clone();

        let signed = match create_receipt(
            &state.signing_key,
            state.tap_domain_separator,
            state.config.tap.data_service_address,
            p.address,
            receipt_value,
            consumer_metadata.clone(),
        ) {
            Ok(s) => s,
            Err(e) => {
                tracing::debug!(error = %e, "failed to create receipt");
                continue;
            }
        };

        let receipt_header = match serde_json::to_string(&signed) {
            Ok(h) => h,
            Err(e) => {
                tracing::debug!(error = %e, "failed to serialize receipt");
                continue;
            }
        };

        let url = format!("{}/rpc/{}", p.endpoint, chain_id);
        let start = Instant::now();

        let resp = match state
            .http_client
            .post(&url)
            .header("TAP-Receipt", receipt_header)
            .json(request)
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                p.qos.record_failure();
                tracing::debug!(error = %e, provider = %p.endpoint, "provider connection failed, trying next");
                continue;
            }
        };

        let ms = start.elapsed().as_millis() as u64;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            p.qos.record_failure();
            tracing::debug!(provider = %p.endpoint, %status, body, "provider returned error, trying next");
            continue;
        }

        let att_header = resp
            .headers()
            .get("x-drpc-attestation")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());

        let body = match resp.json::<JsonRpcResponse>().await {
            Ok(b) => b,
            Err(e) => {
                p.qos.record_failure();
                tracing::debug!(error = %e, provider = %p.endpoint, "invalid response, trying next");
                continue;
            }
        };

        let result_json = match (&body.result, &body.error) {
            (Some(r), _) => serde_json::to_string(r).unwrap_or_else(|_| "null".to_string()),
            (_, Some(e)) => serde_json::to_string(e).unwrap_or_else(|_| "null".to_string()),
            _ => "null".to_string(),
        };

        let attestation = verify_attestation(
            att_header.as_deref(),
            chain_id,
            &request.method,
            &params_json,
            &result_json,
        );

        p.qos.record_success(ms);
        return Ok((body, attestation, p));
    }

    Err(GatewayError::AllProvidersFailed(chain_id))
}

// ---------------------------------------------------------------------------
// Quorum dispatch — majority result wins (deterministic methods)
// ---------------------------------------------------------------------------

async fn dispatch_quorum(
    state: &AppState,
    chain_id: u64,
    request: &JsonRpcRequest,
    candidates: &[Arc<Provider>],
    receipt_value: u128,
    consumer_metadata: Bytes,
) -> Result<(JsonRpcResponse, Option<String>, Arc<Provider>), GatewayError> {
    let params_json = serde_json::to_string(&request.params).unwrap_or_else(|_| "null".to_string());

    // Collect all responses concurrently, then take majority.
    let futures: Vec<_> = candidates
        .iter()
        .map(|provider| {
            let client = state.http_client.clone();
            let signing_key = state.signing_key.clone();
            let domain_sep = state.tap_domain_separator;
            let data_service = state.config.tap.data_service_address;
            let req = request.clone();
            let p = provider.clone();
            let params_json = params_json.clone();
            let method = request.method.clone();
            let metadata = consumer_metadata.clone();

            async move {
                let signed = create_receipt(
                    &signing_key,
                    domain_sep,
                    data_service,
                    p.address,
                    receipt_value,
                    metadata,
                )
                .map_err(|e| e.to_string())?;

                let receipt_header = serde_json::to_string(&signed).map_err(|e| e.to_string())?;
                let url = format!("{}/rpc/{}", p.endpoint, chain_id);
                let start = Instant::now();

                let resp = client
                    .post(&url)
                    .header("TAP-Receipt", receipt_header)
                    .json(&req)
                    .send()
                    .await
                    .map_err(|e| format!("connection failed: {e}"))?;

                let ms = start.elapsed().as_millis() as u64;

                if !resp.status().is_success() {
                    let status = resp.status();
                    let body = resp.text().await.unwrap_or_default();
                    p.qos.record_failure();
                    return Err(format!("HTTP {} body={}", status, body));
                }

                let att_header = resp
                    .headers()
                    .get("x-drpc-attestation")
                    .and_then(|v| v.to_str().ok())
                    .map(|s| s.to_string());

                let body = resp
                    .json::<JsonRpcResponse>()
                    .await
                    .map_err(|e| format!("invalid response: {e}"))?;

                let result_json = match (&body.result, &body.error) {
                    (Some(r), _) => serde_json::to_string(r).unwrap_or_else(|_| "null".to_string()),
                    (_, Some(e)) => serde_json::to_string(e).unwrap_or_else(|_| "null".to_string()),
                    _ => "null".to_string(),
                };

                let attestation = verify_attestation(
                    att_header.as_deref(),
                    chain_id,
                    &method,
                    &params_json,
                    &result_json,
                );

                p.qos.record_success(ms);
                Ok::<_, String>((body, result_json, attestation, p))
            }
        })
        .collect();

    let results: Vec<QuorumTaskResult> = join_all(futures).await;

    // Group successful responses by their serialised result value.
    let mut buckets: HashMap<String, Vec<ProviderResult>> = HashMap::new();

    for r in results {
        match r {
            Ok((resp, result_json, att, provider)) => {
                buckets
                    .entry(result_json)
                    .or_default()
                    .push((resp, att, provider));
            }
            Err(e) => tracing::debug!(error = %e, "quorum provider failed"),
        }
    }

    if buckets.is_empty() {
        return Err(GatewayError::AllProvidersFailed(chain_id));
    }

    // Pick the bucket with the most votes; log a warning if providers disagreed.
    let majority_key = buckets
        .iter()
        .max_by_key(|(_, v)| v.len())
        .map(|(k, _)| k.clone())
        .unwrap();

    if buckets.len() > 1 {
        tracing::warn!(
            method = %request.method,
            chain_id,
            buckets = buckets.len(),
            majority_votes = buckets[&majority_key].len(),
            total = buckets.values().map(|v| v.len()).sum::<usize>(),
            "quorum disagreement detected"
        );
    }

    let (response, attestation, winner) = buckets
        .remove(&majority_key)
        .unwrap()
        .into_iter()
        .next()
        .unwrap();

    Ok((response, attestation, winner))
}

// ---------------------------------------------------------------------------
// CU weights
// ---------------------------------------------------------------------------

fn cu_weight_for(method: &str) -> u32 {
    match method {
        "eth_chainId" | "net_version" | "eth_blockNumber" => 1,
        "eth_getBalance"
        | "eth_getTransactionCount"
        | "eth_getCode"
        | "eth_getStorageAt"
        | "eth_sendRawTransaction"
        | "eth_getBlockByHash"
        | "eth_getBlockByNumber" => 5,
        "eth_call"
        | "eth_estimateGas"
        | "eth_getTransactionReceipt"
        | "eth_getTransactionByHash" => 10,
        "eth_getLogs" => 20,
        _ => 10,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn required_tier_debug_methods() {
        assert_eq!(
            required_tier("debug_traceCall", &None),
            CapabilityTier::Debug
        );
        assert_eq!(
            required_tier("debug_traceTransaction", &None),
            CapabilityTier::Debug
        );
        assert_eq!(required_tier("trace_call", &None), CapabilityTier::Debug);
        assert_eq!(
            required_tier("trace_replayTransaction", &None),
            CapabilityTier::Debug
        );
    }

    #[test]
    fn required_tier_standard_methods() {
        let latest = Some(json!(["0xdeadbeef", "latest"]));
        assert_eq!(required_tier("eth_call", &latest), CapabilityTier::Standard);
        assert_eq!(
            required_tier("eth_getBalance", &latest),
            CapabilityTier::Standard
        );
        assert_eq!(
            required_tier("eth_getLogs", &None),
            CapabilityTier::Standard
        );
        assert_eq!(
            required_tier("net_version", &None),
            CapabilityTier::Standard
        );
    }

    #[test]
    fn required_tier_archive_methods() {
        let params_idx1_hex = Some(json!(["0xdeadbeef", "0x100"]));
        assert_eq!(
            required_tier("eth_getBalance", &params_idx1_hex),
            CapabilityTier::Archive
        );
        assert_eq!(
            required_tier("eth_getCode", &params_idx1_hex),
            CapabilityTier::Archive
        );
        assert_eq!(
            required_tier("eth_getTransactionCount", &params_idx1_hex),
            CapabilityTier::Archive
        );

        let params_earliest = Some(json!(["0xdeadbeef", "earliest"]));
        assert_eq!(
            required_tier("eth_getBalance", &params_earliest),
            CapabilityTier::Archive
        );

        let params_num = Some(json!(["0xdeadbeef", 1_000_000u64]));
        assert_eq!(
            required_tier("eth_getBalance", &params_num),
            CapabilityTier::Archive
        );

        let storage = Some(json!(["0xdeadbeef", "0x0", "0x100"]));
        assert_eq!(
            required_tier("eth_getStorageAt", &storage),
            CapabilityTier::Archive
        );

        let call = Some(json!([{"to": "0xabc", "data": "0x"}, "0x100"]));
        assert_eq!(required_tier("eth_call", &call), CapabilityTier::Archive);

        let by_num = Some(json!(["0x100", false]));
        assert_eq!(
            required_tier("eth_getBlockByNumber", &by_num),
            CapabilityTier::Archive
        );
        let by_latest = Some(json!(["latest", false]));
        assert_eq!(
            required_tier("eth_getBlockByNumber", &by_latest),
            CapabilityTier::Standard
        );

        let logs = Some(json!([{"fromBlock": "0x100", "toBlock": "0x200"}]));
        assert_eq!(required_tier("eth_getLogs", &logs), CapabilityTier::Archive);
        let logs_latest = Some(json!([{"fromBlock": "latest", "toBlock": "latest"}]));
        assert_eq!(
            required_tier("eth_getLogs", &logs_latest),
            CapabilityTier::Standard
        );
    }

    #[test]
    fn quorum_methods_are_deterministic() {
        assert!(requires_quorum("eth_call"));
        assert!(requires_quorum("eth_getLogs"));
        assert!(requires_quorum("eth_getBalance"));
        assert!(requires_quorum("eth_getTransactionReceipt"));
        assert!(!requires_quorum("eth_blockNumber"));
        assert!(!requires_quorum("eth_estimateGas"));
        assert!(!requires_quorum("eth_sendRawTransaction"));
    }

    // --- JsonRpcRequest::validate ---

    #[test]
    fn validate_accepts_valid_request() {
        let req = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            method: "eth_blockNumber".to_string(),
            params: None,
            id: Some(json!(1)),
        };
        assert!(req.validate().is_ok());
    }

    #[test]
    fn validate_rejects_wrong_jsonrpc_version() {
        let req = JsonRpcRequest {
            jsonrpc: "1.0".to_string(),
            method: "eth_blockNumber".to_string(),
            params: None,
            id: None,
        };
        assert!(matches!(
            req.validate(),
            Err(GatewayError::InvalidRequest(_))
        ));
    }

    #[test]
    fn validate_rejects_empty_method() {
        let req = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            method: String::new(),
            params: None,
            id: None,
        };
        assert!(matches!(
            req.validate(),
            Err(GatewayError::InvalidRequest(_))
        ));
    }

    // --- cu_weight_for ---

    #[test]
    fn cu_weight_known_methods() {
        assert_eq!(cu_weight_for("eth_blockNumber"), 1);
        assert_eq!(cu_weight_for("eth_chainId"), 1);
        assert_eq!(cu_weight_for("eth_getBalance"), 5);
        assert_eq!(cu_weight_for("eth_call"), 10);
        assert_eq!(cu_weight_for("eth_getLogs"), 20);
        assert_eq!(cu_weight_for("eth_getTransactionReceipt"), 10);
        assert_eq!(cu_weight_for("eth_sendRawTransaction"), 5);
    }

    #[test]
    fn cu_weight_unknown_defaults_to_10() {
        assert_eq!(cu_weight_for("eth_newPendingTransactionFilter"), 10);
        assert_eq!(cu_weight_for("debug_traceCall"), 10);
    }

    // --- extract_consumer_address ---

    #[test]
    fn extract_consumer_address_missing_header() {
        // X-Consumer-Address is optional; absent → Address::ZERO
        let headers = HeaderMap::new();
        assert_eq!(extract_consumer_address(&headers).unwrap(), Address::ZERO);
    }

    #[test]
    fn extract_consumer_address_invalid_address() {
        let mut headers = HeaderMap::new();
        headers.insert("x-consumer-address", "not-an-address".parse().unwrap());
        assert!(matches!(
            extract_consumer_address(&headers),
            Err(GatewayError::InvalidConsumerAddress(_))
        ));
    }

    #[test]
    fn extract_consumer_address_valid() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-consumer-address",
            "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266"
                .parse()
                .unwrap(),
        );
        let addr = extract_consumer_address(&headers).unwrap();
        assert_eq!(
            addr,
            "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266"
                .parse::<Address>()
                .unwrap()
        );
    }

    // --- consumer address from URL path (rpc_handler_with_consumer) ---

    #[test]
    fn path_consumer_address_valid_checksummed() {
        let addr: Address = "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266"
            .parse()
            .expect("valid checksummed address");
        assert_eq!(
            addr,
            "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266"
                .parse::<Address>()
                .unwrap()
        );
    }

    #[test]
    fn path_consumer_address_valid_lowercase() {
        let addr: Address = "0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266"
            .parse()
            .expect("valid lowercase address");
        assert_eq!(
            addr.to_string().to_lowercase(),
            "0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266"
        );
    }

    #[test]
    fn path_consumer_address_invalid_rejects() {
        let result = "not-an-address".parse::<Address>();
        assert!(result.is_err());
    }

    // --- verify_attestation ---

    #[test]
    fn verify_attestation_absent_returns_none() {
        assert!(verify_attestation(None, 1, "eth_blockNumber", "[]", r#""0x100""#).is_none());
    }

    #[test]
    fn verify_attestation_malformed_json_returns_none() {
        assert!(
            verify_attestation(Some("not-json"), 1, "eth_blockNumber", "[]", r#""0x100""#)
                .is_none()
        );
    }

    #[test]
    fn verify_attestation_signer_mismatch_returns_none() {
        use dispatch_tap::address_from_key;
        use k256::ecdsa::SigningKey;

        let key = SigningKey::from_slice(&[0x42u8; 32]).unwrap();

        let chain_id: u64 = 1;
        let method = "eth_blockNumber";
        let params_json = "[]";
        let result_json = r#""0x100""#;

        let params_hash = keccak256(params_json.as_bytes());
        let result_hash = keccak256(result_json.as_bytes());
        let mut msg = Vec::new();
        msg.extend_from_slice(&chain_id.to_be_bytes());
        msg.extend_from_slice(method.as_bytes());
        msg.extend_from_slice(params_hash.as_slice());
        msg.extend_from_slice(result_hash.as_slice());
        let msg_hash = keccak256(&msg);

        let (sig, rec_id) = key.sign_prehash_recoverable(msg_hash.as_slice()).unwrap();
        let mut sig_bytes = [0u8; 65];
        sig_bytes[..64].copy_from_slice(&sig.to_bytes());
        sig_bytes[64] = rec_id.to_byte() + 27;
        let sig_hex = format!("0x{}", hex::encode(sig_bytes));

        let _signer = address_from_key(&key);

        // Correct signature but wrong stated signer → mismatch
        let att = json!({
            "signer": "0x0000000000000000000000000000000000000001",
            "signature": sig_hex,
        })
        .to_string();

        assert!(
            verify_attestation(Some(&att), chain_id, method, params_json, result_json).is_none()
        );
    }

    #[test]
    fn verify_attestation_valid() {
        use dispatch_tap::address_from_key;
        use k256::ecdsa::SigningKey;

        let key = SigningKey::from_slice(&[0x42u8; 32]).unwrap();
        let signer = address_from_key(&key);

        let chain_id: u64 = 1;
        let method = "eth_blockNumber";
        let params_json = "[]";
        let result_json = r#""0x100""#;

        let params_hash = keccak256(params_json.as_bytes());
        let result_hash = keccak256(result_json.as_bytes());
        let mut msg = Vec::new();
        msg.extend_from_slice(&chain_id.to_be_bytes());
        msg.extend_from_slice(method.as_bytes());
        msg.extend_from_slice(params_hash.as_slice());
        msg.extend_from_slice(result_hash.as_slice());
        let msg_hash = keccak256(&msg);

        let (sig, rec_id) = key.sign_prehash_recoverable(msg_hash.as_slice()).unwrap();
        let mut sig_bytes = [0u8; 65];
        sig_bytes[..64].copy_from_slice(&sig.to_bytes());
        sig_bytes[64] = rec_id.to_byte() + 27;
        let sig_hex = format!("0x{}", hex::encode(sig_bytes));

        let att = json!({
            "signer": signer.to_string().to_lowercase(),
            "signature": sig_hex,
        })
        .to_string();

        assert!(
            verify_attestation(Some(&att), chain_id, method, params_json, result_json).is_some()
        );
    }
}
