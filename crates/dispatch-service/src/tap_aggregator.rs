//! Background RAV aggregation task.
//!
//! Runs on a configurable interval. For each distinct payer (gateway signer)
//! present in `tap_receipts`, it:
//!   1. Reconstructs all stored receipts as `SignedReceipt` structs.
//!   2. POSTs them to the gateway's `/rav/aggregate` endpoint.
//!   3. Upserts the returned `SignedRav` into `tap_ravs`.
//!
//! `previous_rav_value` is computed as:
//!   max(local tap_ravs.value_aggregate, on-chain tokensCollected)
//!
//! The on-chain floor survives any local DB reset or data-service migration and
//! is the only guarantee of the monotonically-increasing invariant required by
//! GraphTallyCollector.  When no Arbitrum RPC URL is configured the service
//! falls back to the local DB floor and logs a warning.
//!
//! After a successful upsert, receipts covered by the RAV (timestamp_ns ≤
//! rav.timestamp_ns) are deleted to keep the table small.

use std::{sync::Arc, time::Duration};

use alloy_primitives::{Address, Bytes, U256};
use alloy_sol_types::{sol, SolCall};

use crate::{
    config::Config,
    db::{
        receipts::{
            delete_covered, distinct_payers, fetch_by_payer, fetch_rav_floor, upsert_rav, RavRow,
        },
        Pool,
    },
};

// ---------------------------------------------------------------------------
// On-chain tokensCollected query
// ---------------------------------------------------------------------------

sol! {
    /// GraphTallyCollector public mapping getter.
    /// tokensCollected[dataService][collectionId][receiver][payer]
    function tokensCollected(
        address dataService,
        bytes32 collectionId,
        address receiver,
        address payer
    ) external view returns (uint256 tokens);
}

/// Query `GraphTallyCollector.tokensCollected` on Arbitrum One.
///
/// Returns the on-chain high-water mark (GRT wei) for the given
/// (dataService, collectionId, serviceProvider, payer) tuple.  Used to seed
/// `previous_rav_value` so the monotonic invariant holds even after a DB reset.
async fn fetch_on_chain_tokens_collected(
    client: &reqwest::Client,
    rpc_url: &str,
    tally_collector: Address,
    data_service: Address,
    collection_id: alloy_primitives::B256,
    service_provider: Address,
    payer: Address,
) -> anyhow::Result<u128> {
    let call_data = tokensCollectedCall {
        dataService: data_service,
        collectionId: collection_id,
        receiver: service_provider,
        payer,
    }
    .abi_encode();

    let payload = serde_json::json!({
        "jsonrpc": "2.0",
        "id":      1,
        "method":  "eth_call",
        "params": [
            {
                "to":   format!("{:#x}", tally_collector),
                "data": format!("0x{}", hex::encode(&call_data)),
            },
            "latest"
        ]
    });

    let resp: serde_json::Value = client
        .post(rpc_url)
        .json(&payload)
        .send()
        .await?
        .json()
        .await?;

    let hex_result = resp["result"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("eth_call returned no result: {resp}"))?;

    let bytes = hex::decode(hex_result.trim_start_matches("0x"))?;
    if bytes.len() < 32 {
        anyhow::bail!("eth_call result too short ({} bytes)", bytes.len());
    }

    let value = U256::from_be_slice(&bytes[..32]);
    Ok(value.try_into().unwrap_or(u128::MAX))
}

// ---------------------------------------------------------------------------
// Aggregator loop
// ---------------------------------------------------------------------------

/// Spawn the aggregator loop. Returns immediately; the task runs until the
/// process exits.
pub fn spawn(config: Arc<Config>, pool: Pool) {
    let Some(url) = config.tap.aggregator_url.clone() else {
        tracing::info!("tap.aggregator_url not set — RAV aggregation disabled");
        return;
    };

    // Resolve Arbitrum RPC URL for the on-chain tokensCollected floor check.
    // Prefer tap.escrow_check_rpc_url (lighter, read-only endpoint); fall back
    // to collector.arbitrum_rpc_url if set.
    let rpc_url: Option<String> = config.tap.escrow_check_rpc_url.clone().or_else(|| {
        config
            .collector
            .as_ref()
            .map(|c| c.arbitrum_rpc_url.clone())
    });

    if rpc_url.is_none() {
        tracing::warn!(
            "no arbitrum_rpc_url configured (set tap.escrow_check_rpc_url or [collector]); \
             on-chain tokensCollected floor check disabled — RAV monotonicity depends on \
             local DB state only and will break if the DB is reset"
        );
    }

    let tally_collector = config.tap.eip712_verifying_contract;
    let interval = Duration::from_secs(config.tap.aggregation_interval_secs);
    tracing::info!(%url, interval_secs = interval.as_secs(), "RAV aggregator started");

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .expect("failed to build HTTP client");

    tokio::spawn(async move {
        loop {
            tokio::time::sleep(interval).await;
            if let Err(e) = run_once(
                &url,
                &config,
                &pool,
                &client,
                rpc_url.as_deref(),
                tally_collector,
            )
            .await
            {
                tracing::warn!("RAV aggregation cycle failed: {e:#}");
            }
        }
    });
}

// ---------------------------------------------------------------------------
// One aggregation cycle
// ---------------------------------------------------------------------------

async fn run_once(
    aggregator_url: &str,
    config: &Config,
    pool: &Pool,
    client: &reqwest::Client,
    rpc_url: Option<&str>,
    tally_collector: Address,
) -> anyhow::Result<()> {
    let payers = distinct_payers(pool).await?;

    if payers.is_empty() {
        tracing::debug!("no receipts in db, skipping aggregation");
        return Ok(());
    }

    let service_provider = config.indexer.service_provider_address;
    let data_service = config.tap.data_service_address;
    let endpoint = format!("{aggregator_url}/rav/aggregate");

    for payer_hex in payers {
        if let Err(e) = aggregate_payer(
            pool,
            client,
            &endpoint,
            service_provider,
            data_service,
            &payer_hex,
            rpc_url,
            tally_collector,
        )
        .await
        {
            tracing::warn!(payer = %payer_hex, "RAV aggregation failed for payer: {e:#}");
        }
    }

    Ok(())
}

async fn aggregate_payer(
    pool: &Pool,
    client: &reqwest::Client,
    endpoint: &str,
    service_provider: alloy_primitives::Address,
    data_service: alloy_primitives::Address,
    payer_hex: &str,
    rpc_url: Option<&str>,
    tally_collector: alloy_primitives::Address,
) -> anyhow::Result<()> {
    let rows = fetch_by_payer(pool, payer_hex).await?;
    if rows.is_empty() {
        return Ok(());
    }

    let receipts: Vec<dispatch_tap::SignedReceipt> = rows
        .iter()
        .map(|row| {
            let value = row.value.parse::<u128>().unwrap_or(0);
            dispatch_tap::SignedReceipt {
                receipt: dispatch_tap::Receipt {
                    data_service,
                    service_provider,
                    timestamp_ns: row.timestamp_ns as u64,
                    nonce: row.nonce as u64,
                    value,
                    metadata: Bytes::from(row.metadata.clone()),
                },
                signature: row.signature.clone(),
            }
        })
        .collect();

    let payer: alloy_primitives::Address = payer_hex
        .parse()
        .map_err(|_| anyhow::anyhow!("invalid payer address in db: {payer_hex}"))?;

    let cid = dispatch_tap::collection_id(payer, service_provider, data_service);
    let cid_hex = format!("{cid:?}");

    // Local DB floor — may be 0 if the DB was reset or this is a fresh collection_id.
    let local_floor = fetch_rav_floor(pool, &cid_hex).await.unwrap_or(0);

    // On-chain floor — the GraphTallyCollector high-water mark.  This is the
    // authoritative lower bound: any RAV with valueAggregate <= tokensCollected
    // will revert on-chain.  Seeding from on-chain means a DB reset or a
    // data-service migration never causes collect() to revert indefinitely.
    let on_chain_floor = match rpc_url {
        Some(url) => match fetch_on_chain_tokens_collected(
            client,
            url,
            tally_collector,
            data_service,
            cid,
            service_provider,
            payer,
        )
        .await
        {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(
                    payer = %payer_hex,
                    "on-chain tokensCollected query failed, using local DB floor only: {e:#}"
                );
                0
            }
        },
        None => 0,
    };

    let previous_rav_value = local_floor.max(on_chain_floor);

    let body = serde_json::json!({
        "service_provider": service_provider,
        "payer": payer,
        "receipts": receipts,
        "previous_rav_value": previous_rav_value,
    });

    let resp = client
        .post(endpoint)
        .json(&body)
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("POST {endpoint} failed: {e}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        anyhow::bail!("aggregator returned {status}: {text}");
    }

    let resp_json: serde_json::Value = resp.json().await?;
    let signed_rav: dispatch_tap::SignedRav =
        serde_json::from_value(resp_json["signed_rav"].clone())?;

    let rav = &signed_rav.rav;
    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;

    let collection_id_hex = format!("{:?}", rav.collection_id);
    let payer_hex_lower = format!("{:?}", rav.payer);
    let sp_hex = format!("{:?}", rav.service_provider);
    let ds_hex = format!("{:?}", rav.data_service);

    upsert_rav(
        pool,
        RavRow {
            collection_id: &collection_id_hex,
            payer_address: &payer_hex_lower,
            service_provider: &sp_hex,
            data_service: &ds_hex,
            timestamp_ns: rav.timestamp_ns as i64,
            value_aggregate: &rav.value_aggregate.to_string(),
            signature: &signed_rav.signature,
            last_updated: now_secs,
        },
    )
    .await?;

    let pruned = delete_covered(pool, &payer_hex_lower, rav.timestamp_ns as i64).await?;

    tracing::info!(
        payer = %payer_hex,
        receipts = rows.len(),
        pruned,
        value_aggregate = %rav.value_aggregate,
        on_chain_floor,
        local_floor,
        "RAV updated"
    );

    Ok(())
}
