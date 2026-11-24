/// POST /rav/aggregate
///
/// The indexer (service) calls this endpoint periodically to aggregate its stored
/// receipts into a signed RAV. The gateway:
///   1. Verifies each receipt was signed by itself.
///   2. Sums the values and takes the max timestamp.
///   3. Signs and returns a ReceiptAggregateVoucher (RAV).
///
/// The RAV is cumulative: `value_aggregate` is the last RAV this gateway signed for the collection
/// plus every receipt in the batch that it does not already cover. Resending covered receipts is
/// harmless and returns the same value.
use axum::{
    extract::{DefaultBodyLimit, State},
    routing::post,
    Json, Router,
};
use serde::{Deserialize, Serialize};

use alloy::{sol, sol_types::SolCall};
use alloy_primitives::{Address, Bytes, B256, U256};
use dispatch_tap::{
    collection_id, eip712_hash, recover_signer, sign_rav, Rav, SignedRav, SignedReceipt,
};

use crate::{
    error::GatewayError,
    rav_ledger::{fold, FoldError, ReceiptKey, UpdateError},
    server::AppState,
};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/rav/aggregate", post(aggregate_handler))
        .layer(DefaultBodyLimit::max(64 * 1024 * 1024)) // 64 MB — receipt batches can be large
}

#[derive(Debug, Deserialize)]
pub struct AggregateRequest {
    /// The indexer's on-chain service provider address.
    pub service_provider: Address,
    /// The consumer (payer) address whose escrow will be debited on-chain.
    /// When present, the RAV will have `payer = consumer_address` so the
    /// correct escrow account is charged. Omit only for backwards compatibility
    /// (falls back to the gateway's own signer address).
    pub payer: Option<Address>,
    /// All receipts to include in this RAV (for full cumulative aggregation, include
    /// all historical receipts, not just new ones).
    pub receipts: Vec<SignedReceipt>,
    /// Accepted for older callers and ignored: the floor comes from the gateway's own ledger.
    #[serde(default)]
    pub previous_rav_value: u128,
}

#[derive(Debug, Serialize)]
pub struct AggregateResponse {
    pub signed_rav: SignedRav,
}

async fn aggregate_handler(
    State(state): State<AppState>,
    Json(req): Json<AggregateRequest>,
) -> Result<Json<AggregateResponse>, GatewayError> {
    if req.receipts.is_empty() {
        return Err(GatewayError::InvalidRequest(
            "receipts batch is empty".into(),
        ));
    }

    // A RAV binds the payer's escrow, so it may only name a payer this gateway signs for.
    let payer = req.payer.unwrap_or(state.signer_address);
    if payer != state.signer_address && payer != state.config.tap.gateway_payer_address {
        return Err(GatewayError::InvalidRequest(format!(
            "payer {payer} is not one this gateway signs for"
        )));
    }
    let domain_sep = state.tap_domain_separator;

    // Derive data_service from the receipts — all must agree.
    // We accept any data service signed by this gateway (RPC, Seahorn, etc.)
    // rather than restricting to a single configured address.
    let data_service = req.receipts[0].receipt.data_service;

    let mut keys = Vec::with_capacity(req.receipts.len());
    for signed in &req.receipts {
        let r = &signed.receipt;

        if r.data_service != data_service {
            return Err(GatewayError::InvalidRequest(format!(
                "mixed data_service in batch: expected {data_service}, got {}",
                r.data_service
            )));
        }
        if r.service_provider != req.service_provider {
            return Err(GatewayError::InvalidRequest(format!(
                "receipt service_provider mismatch: expected {}, got {}",
                req.service_provider, r.service_provider
            )));
        }

        // Verify the receipt was signed by the gateway itself.
        // Note: receipts are signed by the gateway's signing key, not by the payer (consumer).
        let hash = eip712_hash(domain_sep, r);
        let recovered = recover_signer(hash, &signed.signature)
            .map_err(|e| GatewayError::InvalidRequest(format!("invalid receipt signature: {e}")))?;
        if recovered != state.signer_address {
            return Err(GatewayError::InvalidRequest(format!(
                "receipt not signed by this gateway: signer={recovered}"
            )));
        }

        keys.push(ReceiptKey {
            timestamp_ns: r.timestamp_ns,
            nonce: r.nonce,
            value: r.value,
        });
    }

    let cid = collection_id(payer, req.service_provider, data_service);

    // Only needed when the ledger has no record of this collection, e.g. after losing its file.
    let floor = match state.rav_ledger.get(cid).await {
        Some(_) => 0,
        None => on_chain_floor(&state, data_service, cid, req.service_provider, payer).await?,
    };

    let signed_rav = state
        .rav_ledger
        .update::<_, &str>(cid, |last| {
            let settled = fold(last, floor, keys).map_err(|e| match e {
                FoldError::NothingToAggregate => "no receipts to aggregate",
                FoldError::Overflow => "value_aggregate overflows u128",
            })?;
            let rav = Rav {
                collection_id: cid,
                payer,
                service_provider: req.service_provider,
                data_service,
                timestamp_ns: settled.timestamp_ns,
                value_aggregate: settled.value,
                metadata: Bytes::default(),
            };
            let signed =
                sign_rav(&state.signing_key, domain_sep, rav).map_err(|_| "RAV signing failed")?;
            Ok((settled, signed))
        })
        .await
        .map_err(|e| match e {
            UpdateError::Rejected(msg) => GatewayError::InvalidRequest(msg.into()),
            UpdateError::Persist(e) => GatewayError::Internal(e.context("persisting RAV ledger")),
        })?;

    tracing::info!(
        service_provider = %req.service_provider,
        receipts = req.receipts.len(),
        value_aggregate = signed_rav.rav.value_aggregate,
        "issued signed RAV"
    );

    Ok(Json(AggregateResponse { signed_rav }))
}

sol! {
    function tokensCollected(
        address dataService,
        bytes32 collectionId,
        address receiver,
        address payer
    ) external view returns (uint256 tokens);
}

/// What GraphTallyCollector has already paid out for this collection, or zero when the gateway has
/// no chain access configured.
async fn on_chain_floor(
    state: &AppState,
    data_service: Address,
    cid: B256,
    receiver: Address,
    payer: Address,
) -> Result<u128, GatewayError> {
    let Some(p) = &state.config.provisioning else {
        return Ok(0);
    };
    let data = tokensCollectedCall {
        dataService: data_service,
        collectionId: cid,
        receiver,
        payer,
    }
    .abi_encode();
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "eth_call",
        "params": [
            { "to": format!("{:#x}", p.collector_address), "data": format!("0x{}", hex::encode(data)) },
            "latest"
        ]
    });
    let resp: serde_json::Value = state
        .http_client
        .post(&p.arbitrum_rpc_url)
        .json(&body)
        .send()
        .await
        .map_err(|e| GatewayError::Internal(anyhow::anyhow!("tokensCollected: {e}")))?
        .json()
        .await
        .map_err(|e| GatewayError::Internal(anyhow::anyhow!("tokensCollected: {e}")))?;
    let raw = resp["result"]
        .as_str()
        .and_then(|h| hex::decode(h.trim_start_matches("0x")).ok())
        .filter(|b| b.len() >= 32)
        .ok_or_else(|| GatewayError::Internal(anyhow::anyhow!("tokensCollected: {resp}")))?;
    U256::from_be_slice(&raw[..32])
        .try_into()
        .map_err(|_| GatewayError::Internal(anyhow::anyhow!("tokensCollected exceeds u128")))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use arc_swap::ArcSwap;
    use k256::ecdsa::SigningKey;

    use super::*;
    use crate::{
        affinity::FilterAffinity, config::Config, rav_ledger::RavLedger, registry::Registry,
    };

    const KEY: &str = "0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d";

    fn state() -> AppState {
        let config: Config = toml::from_str(&format!(
            r#"
            [gateway]
            host = "127.0.0.1"
            port = 0
            [tap]
            signer_private_key = "{KEY}"
            gateway_payer_address = "0x0000000000000000000000000000000000000000"
            data_service_address = "0x7101d5c1a5c89c3647f5118da118e56c023ba0b9"
            eip712_domain_name = "GraphTallyCollector"
            [qos]
            "#
        ))
        .unwrap();
        let signing_key =
            SigningKey::from_slice(&hex::decode(KEY.trim_start_matches("0x")).unwrap()).unwrap();
        AppState {
            tap_domain_separator: dispatch_tap::domain_separator(
                &config.tap.eip712_domain_name,
                config.tap.eip712_chain_id,
                config.tap.eip712_verifying_contract,
            ),
            signer_address: dispatch_tap::address_from_key(&signing_key),
            signing_key: Arc::new(signing_key),
            registry: Arc::new(ArcSwap::from_pointee(Registry::from_config(&[]))),
            http_client: reqwest::Client::new(),
            rate_limiter: None,
            filter_affinity: Arc::new(FilterAffinity::new(std::time::Duration::from_secs(1), 1)),
            rav_ledger: Arc::new(RavLedger::load(None).unwrap()),
            config: Arc::new(config),
        }
    }

    fn receipt(state: &AppState, provider: Address, value: u128) -> SignedReceipt {
        dispatch_tap::create_receipt(
            &state.signing_key,
            state.tap_domain_separator,
            state.config.tap.data_service_address,
            provider,
            value,
            Bytes::default(),
        )
        .unwrap()
    }

    async fn aggregate(
        state: &AppState,
        provider: Address,
        payer: Option<Address>,
        receipts: Vec<SignedReceipt>,
        previous_rav_value: u128,
    ) -> Result<u128, GatewayError> {
        aggregate_handler(
            State(state.clone()),
            Json(AggregateRequest {
                service_provider: provider,
                payer,
                receipts,
                previous_rav_value,
            }),
        )
        .await
        .map(|r| r.0.signed_rav.rav.value_aggregate)
    }

    #[tokio::test]
    async fn caller_cannot_inflate_or_replay_its_way_to_a_bigger_rav() {
        let s = state();
        let provider = Address::repeat_byte(0xaa);
        let first = receipt(&s, provider, 10);

        let v = aggregate(
            &s,
            provider,
            None,
            vec![first.clone(), first.clone()],
            u128::MAX / 2,
        )
        .await
        .unwrap();
        assert_eq!(v, 10, "floor ignored and duplicate counted once");

        let v = aggregate(&s, provider, None, vec![first.clone()], 0)
            .await
            .unwrap();
        assert_eq!(v, 10, "replaying a covered receipt changes nothing");

        let v = aggregate(&s, provider, None, vec![first, receipt(&s, provider, 5)], 0)
            .await
            .unwrap();
        assert_eq!(v, 15, "only the new receipt is added");
    }

    #[tokio::test]
    async fn refuses_a_payer_the_gateway_does_not_sign_for() {
        let s = state();
        let provider = Address::repeat_byte(0xaa);
        let err = aggregate(
            &s,
            provider,
            Some(Address::repeat_byte(0xbb)),
            vec![receipt(&s, provider, 10)],
            0,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, GatewayError::InvalidRequest(_)));
    }
}
