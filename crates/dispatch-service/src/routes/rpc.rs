use std::time::{SystemTime, UNIX_EPOCH};

use axum::{
    extract::{Path, State},
    http::HeaderMap,
    response::{IntoResponse, Response},
    routing::post,
    Json, Router,
};
use serde_json::Value;

use crate::{
    attestation, db,
    error::ServiceError,
    metrics,
    rpc::{proxy, types::JsonRpcRequest},
    server::AppState,
    tap,
};

pub fn router() -> Router<AppState> {
    Router::new().route("/rpc/:chain_id", post(rpc_handler))
}

async fn rpc_handler(
    State(state): State<AppState>,
    Path(chain_id): Path<u64>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Response, ServiceError> {
    let backend_url = state
        .config
        .chains
        .backends
        .get(&chain_id.to_string())
        .ok_or(ServiceError::UnsupportedChain(chain_id))?
        .clone();

    let items = match &body {
        Value::Array(a) => a.len() as u128,
        _ => 1,
    };
    let validated = admit(&state, &headers, items).await?;

    match body {
        Value::Array(items) => {
            let requests = parse_batch(items)?;
            tracing::debug!(count = requests.len(), chain_id, "dispatching batch");

            let started = std::time::Instant::now();
            let responses = observe(
                chain_id,
                started,
                proxy::forward_batch(&state.http_client, &backend_url, &requests).await,
            )?;
            paid(chain_id, validated.receipt.value);

            settle(&state, chain_id, &validated).await;

            Ok(Json(responses).into_response())
        }

        Value::Object(_) => {
            let request: JsonRpcRequest = serde_json::from_value(body)
                .map_err(|e| ServiceError::InvalidRequest(e.to_string()))?;
            request.validate()?;
            tracing::debug!(method = %request.method, chain_id, "dispatching");

            // --- Forward to backend Ethereum client ---
            let started = std::time::Instant::now();
            let response = observe(
                chain_id,
                started,
                proxy::forward(&state.http_client, &backend_url, &request).await,
            )?;
            paid(chain_id, validated.receipt.value);

            settle(&state, chain_id, &validated).await;

            // --- Sign the response ---
            let params_json =
                serde_json::to_string(&request.params).unwrap_or_else(|_| "null".to_string());
            let result_json = match (&response.result, &response.error) {
                (Some(r), _) => serde_json::to_string(r).unwrap_or_else(|_| "null".to_string()),
                (_, Some(e)) => serde_json::to_string(e).unwrap_or_else(|_| "null".to_string()),
                _ => "null".to_string(),
            };

            let mut resp = Json(response).into_response();

            match attestation::sign(
                &state.signing_key,
                state.signer_address,
                chain_id,
                &request.method,
                &params_json,
                &result_json,
            ) {
                Ok(att) => {
                    if let Ok(header_val) = serde_json::to_string(&att)
                        .map_err(|e| e.to_string())
                        .and_then(|s| {
                            s.parse()
                                .map_err(|e: axum::http::header::InvalidHeaderValue| e.to_string())
                        })
                    {
                        resp.headers_mut().insert("x-drpc-attestation", header_val);
                    }
                }
                Err(e) => tracing::warn!(error = %e, "failed to sign response"),
            }

            Ok(resp)
        }

        _ => Err(ServiceError::InvalidRequest(
            "expected JSON object or array".to_string(),
        )),
    }
}

/// Parse and validate a JSON-RPC batch. Returns an error if the batch is empty
/// or any item fails to deserialise or validate.
fn parse_batch(items: Vec<Value>) -> Result<Vec<JsonRpcRequest>, ServiceError> {
    if items.is_empty() {
        return Err(ServiceError::InvalidRequest("empty batch".to_string()));
    }
    items
        .into_iter()
        .map(|v| {
            let req: JsonRpcRequest = serde_json::from_value(v)
                .map_err(|e| ServiceError::InvalidRequest(e.to_string()))?;
            req.validate()?;
            Ok(req)
        })
        .collect()
}

/// Check the TAP receipt, the payer's escrow and its credit before serving anything.
pub(crate) async fn admit(
    state: &AppState,
    headers: &HeaderMap,
    items: u128,
) -> Result<tap::ValidatedReceipt, ServiceError> {
    // --- TAP receipt validation (shared by single and batch) ---
    let receipt_header = headers
        .get("TAP-Receipt")
        .ok_or(ServiceError::MissingReceipt)?
        .to_str()
        .map_err(|_| ServiceError::InvalidReceipt("non-UTF8 TAP-Receipt header".to_string()))?;

    let now_ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64;

    let validated = tap::validate_receipt(
        receipt_header,
        state.tap_domain_separator,
        &state.config.tap.authorized_senders,
        state.config.tap.data_service_address,
        state.config.indexer.service_provider_address,
        state.config.tap.max_receipt_age_ns,
        now_ns,
    )?;

    let min = state.config.tap.min_value_per_request.saturating_mul(items);
    if validated.receipt.value < min {
        return Err(ServiceError::Underpriced {
            got: validated.receipt.value,
            min,
        });
    }

    // --- Escrow balance pre-check (cached 30 s) ---
    // Check the consumer's (payer's) escrow, not the gateway signer's.
    let bypass = state.config.tap.bypass_consumers.contains(&validated.payer);
    // escrow_verified: true if on-chain balance confirmed > 0 (skips credit check below).
    let mut escrow_verified = false;
    if bypass {
        tracing::debug!(payer = %validated.payer, "escrow check bypassed (bypass_consumers list)");
    } else if let Some(checker) = &state.escrow_checker {
        match checker.balance(validated.payer).await {
            Ok(0) => {
                tracing::warn!(
                    payer = %validated.payer,
                    "consumer escrow balance is zero — rejecting request"
                );
                return Err(ServiceError::InsufficientEscrow);
            }
            Ok(bal) => {
                tracing::debug!(payer = %validated.payer, balance = bal, "escrow ok");
                escrow_verified = true; // On-chain escrow confirmed — skip credit gate.
            }
            Err(e) => {
                // Don't block the request if the check itself fails — log and continue.
                tracing::warn!(error = %e, payer = %validated.payer, "escrow check failed, proceeding anyway");
            }
        }
    }

    // --- Credit limit check (fallback when escrow is not configured or unverified) ---
    // Skipped when on-chain escrow was verified above — the chain is the payment gate.
    if !bypass && !escrow_verified {
        let credit = state.consumer_credit.read().unwrap();
        let served = credit.get(&validated.payer).copied().unwrap_or(0);
        if served >= state.config.tap.credit_threshold {
            tracing::warn!(
                payer = %validated.payer,
                served,
                threshold = state.config.tap.credit_threshold,
                "consumer credit limit reached"
            );
            return Err(ServiceError::CreditLimitExceeded);
        }
    }

    Ok(validated)
}

/// Count a served receipt against the payer's credit and keep it for aggregation.
pub(crate) async fn settle(state: &AppState, chain_id: u64, validated: &tap::ValidatedReceipt) {
    {
        let mut credit = state.consumer_credit.write().unwrap();
        *credit.entry(validated.payer).or_insert(0) += validated.receipt.value;
    }
    // Non-fatal if the DB is unavailable.
    if let Some(pool) = &state.db_pool {
        if let Err(e) = db::receipts::insert(pool, chain_id, validated).await {
            tracing::warn!(
                error = %e,
                signer = %validated.signer,
                chain_id,
                "failed to persist TAP receipt"
            );
        }
    }
}

fn observe<T>(
    chain_id: u64,
    started: std::time::Instant,
    result: Result<T, ServiceError>,
) -> Result<T, ServiceError> {
    let chain = chain_id.to_string();
    let outcome = if result.is_ok() { "ok" } else { "error" };
    metrics::REQUESTS_TOTAL
        .with_label_values(&[&chain, outcome])
        .inc();
    metrics::REQUEST_DURATION
        .with_label_values(&[&chain])
        .observe(started.elapsed().as_secs_f64());
    result
}

fn paid(chain_id: u64, value: u128) {
    metrics::RECEIPT_GRT_TOTAL
        .with_label_values(&[&chain_id.to_string()])
        .inc_by(metrics::grt(value));
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_batch_empty_returns_error() {
        assert!(matches!(
            parse_batch(vec![]),
            Err(ServiceError::InvalidRequest(_))
        ));
    }

    #[test]
    fn parse_batch_valid_items() {
        let items = vec![
            json!({"jsonrpc": "2.0", "method": "eth_blockNumber", "id": 1}),
            json!({"jsonrpc": "2.0", "method": "eth_chainId", "id": 2}),
        ];
        let reqs = parse_batch(items).unwrap();
        assert_eq!(reqs.len(), 2);
        assert_eq!(reqs[0].method, "eth_blockNumber");
        assert_eq!(reqs[1].method, "eth_chainId");
        assert_eq!(reqs[0].id, Some(json!(1)));
        assert_eq!(reqs[1].id, Some(json!(2)));
    }

    #[test]
    fn parse_batch_with_params() {
        let items = vec![
            json!({"jsonrpc": "2.0", "method": "eth_getBalance", "params": ["0xdeadbeef", "latest"], "id": 42}),
        ];
        let reqs = parse_batch(items).unwrap();
        assert_eq!(reqs[0].params, Some(json!(["0xdeadbeef", "latest"])));
    }

    #[test]
    fn parse_batch_missing_method_field() {
        let items = vec![json!({"jsonrpc": "2.0", "id": 1})];
        assert!(matches!(
            parse_batch(items),
            Err(ServiceError::InvalidRequest(_))
        ));
    }

    #[test]
    fn parse_batch_empty_method() {
        let items = vec![json!({"jsonrpc": "2.0", "method": "", "id": 1})];
        assert!(matches!(
            parse_batch(items),
            Err(ServiceError::InvalidRequest(_))
        ));
    }

    #[test]
    fn parse_batch_wrong_jsonrpc_version() {
        let items = vec![json!({"jsonrpc": "1.0", "method": "eth_blockNumber", "id": 1})];
        assert!(matches!(
            parse_batch(items),
            Err(ServiceError::InvalidRequest(_))
        ));
    }

    #[test]
    fn parse_batch_one_bad_item_fails_whole_batch() {
        let items = vec![
            json!({"jsonrpc": "2.0", "method": "eth_blockNumber", "id": 1}),
            json!({"jsonrpc": "2.0", "method": "", "id": 2}), // bad
            json!({"jsonrpc": "2.0", "method": "eth_chainId", "id": 3}),
        ];
        assert!(matches!(
            parse_batch(items),
            Err(ServiceError::InvalidRequest(_))
        ));
    }

    #[test]
    fn parse_batch_null_id_is_allowed() {
        // JSON null deserialises as None for Option<Value> — same as absent field.
        let items = vec![json!({"jsonrpc": "2.0", "method": "eth_blockNumber", "id": null})];
        let reqs = parse_batch(items).unwrap();
        assert_eq!(reqs[0].id, None);
    }

    #[test]
    fn parse_batch_no_id_field_is_allowed() {
        // id is Option<Value> — absent is fine (notification style)
        let items = vec![json!({"jsonrpc": "2.0", "method": "eth_blockNumber"})];
        let reqs = parse_batch(items).unwrap();
        assert_eq!(reqs[0].id, None);
    }

    #[tokio::test]
    async fn receipts_below_the_floor_are_refused_and_a_batch_must_cover_every_item() {
        use crate::routes::test_support::{receipt_header, serve};

        let (addr, state) = serve("min_value_per_request = 1000", "").await;
        let post = |value: u128, body: serde_json::Value| {
            let header = receipt_header(&state, value);
            async move {
                reqwest::Client::new()
                    .post(format!("http://{addr}/rpc/1"))
                    .header("TAP-Receipt", header)
                    .json(&body)
                    .send()
                    .await
                    .unwrap()
                    .status()
                    .as_u16()
            }
        };
        let call = json!({"jsonrpc": "2.0", "method": "eth_blockNumber", "params": [], "id": 1});

        assert_eq!(post(999, call.clone()).await, 402);
        // Admitted, then the unreachable backend answers 502: past the floor.
        assert_eq!(post(1000, call.clone()).await, 502);
        assert_eq!(post(1000, json!([call.clone(), call.clone()])).await, 402);
        assert_eq!(post(2000, json!([call.clone(), call])).await, 502);
    }
}
