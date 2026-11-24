use alloy_primitives::Address;
use anyhow::{Context, Result};
use serde::{Deserialize, Deserializer};
use std::collections::HashMap;

/// Deserialize a u128 from either a TOML integer (i64 range) or a decimal string.
/// TOML doesn't have a u128 type; values above i64::MAX must be quoted strings.
fn de_u128<'de, D: Deserializer<'de>>(d: D) -> Result<u128, D::Error> {
    use serde::de::Error;
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Raw {
        Int(i64),
        Str(String),
    }
    match Raw::deserialize(d)? {
        Raw::Int(n) => u128::try_from(n).map_err(|_| D::Error::custom("negative u128")),
        Raw::Str(s) => s
            .trim()
            .replace('_', "")
            .parse::<u128>()
            .map_err(D::Error::custom),
    }
}

#[derive(Debug, Deserialize, Clone)]
pub struct Config {
    pub server: ServerConfig,
    pub indexer: IndexerConfig,
    pub tap: TapConfig,
    pub chains: ChainsConfig,
    pub database: Option<DatabaseConfig>,
    pub collector: Option<CollectorConfig>,
}

/// On-chain RAV collection config. Omit this section entirely to disable.
#[derive(Debug, Deserialize, Clone)]
pub struct CollectorConfig {
    /// Arbitrum One RPC URL for submitting collect() transactions.
    pub arbitrum_rpc_url: String,
    /// How often to check for unredeemed RAVs (seconds). Default: 3600.
    #[serde(default = "default_collect_interval_secs")]
    pub collect_interval_secs: u64,
    /// Skip RAVs whose value_aggregate is below this threshold (GRT wei).
    /// Avoids spending gas on dust. Default: 0 (collect everything).
    #[serde(default, deserialize_with = "de_u128")]
    pub min_collect_value: u128,
}

#[derive(Debug, Deserialize, Clone)]
pub struct DatabaseConfig {
    /// PostgreSQL connection URL, e.g. postgres://user:pass@localhost/dispatch
    pub url: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct ServerConfig {
    #[serde(default = "default_host")]
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
}

#[derive(Debug, Deserialize, Clone)]
pub struct IndexerConfig {
    /// This indexer's on-chain address (service provider).
    pub service_provider_address: Address,
    /// Hex-encoded 32-byte operator private key used for response attestations.
    pub operator_private_key: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct TapConfig {
    /// RPCDataService contract address (set after deployment).
    pub data_service_address: Address,
    /// Gateway addresses authorised to issue TAP receipts.
    pub authorized_senders: Vec<Address>,
    /// EIP-712 domain name for GraphTallyCollector (e.g. "GraphTallyCollector").
    pub eip712_domain_name: String,
    /// Chain ID where GraphTallyCollector is deployed (42161 = Arbitrum One).
    #[serde(default = "default_tap_chain_id")]
    pub eip712_chain_id: u64,
    /// GraphTallyCollector contract address.
    #[serde(default = "default_tap_verifying_contract")]
    pub eip712_verifying_contract: Address,
    /// Maximum age of a TAP receipt before it is rejected (nanoseconds).
    #[serde(default = "default_max_receipt_age_ns")]
    pub max_receipt_age_ns: u64,
    /// Base URL of the gateway's RAV aggregation endpoint.
    /// e.g. "http://dispatch-gateway:8080" → POST /rav/aggregate
    /// Omit to disable automatic RAV aggregation.
    pub aggregator_url: Option<String>,
    /// How often to run RAV aggregation (seconds). Default: 60.
    #[serde(default = "default_aggregation_interval_secs")]
    pub aggregation_interval_secs: u64,
    /// Maximum unconfirmed GRT wei a consumer may accumulate before being blocked.
    /// Resets after a successful on-chain collect(). Default: 0.1 GRT.
    #[serde(default = "default_credit_threshold", deserialize_with = "de_u128")]
    pub credit_threshold: u128,
    /// Lowest receipt value (GRT wei) accepted per request; a batch must cover every item.
    #[serde(default, deserialize_with = "de_u128")]
    pub min_value_per_request: u128,
    /// PaymentsEscrow contract address on Arbitrum One.
    /// Defaults to the live Horizon deployment.
    #[serde(default = "default_payments_escrow_address")]
    pub payments_escrow_address: Address,
    /// Arbitrum One RPC URL used for escrow balance checks.
    /// Falls back to [collector].arbitrum_rpc_url when omitted.
    /// Set this (or [collector]) to enable on-chain escrow pre-checks.
    pub escrow_check_rpc_url: Option<String>,
    /// Consumer addresses that bypass the escrow balance check.
    /// Useful for self-consumption (e.g. own indexer using own dispatch service).
    #[serde(default)]
    pub bypass_consumers: Vec<Address>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct ChainsConfig {
    /// Chain IDs this service is registered to serve.
    pub supported: Vec<u64>,
    /// Map of chain_id (as string) → backend RPC URL.
    pub backends: HashMap<String, String>,
    /// Serve `/ws/{chain_id}`. Each connection is paid by one receipt on upgrade, whatever it
    /// subscribes to, so it is off unless the operator accepts that.
    #[serde(default)]
    pub websocket: bool,
}

impl Config {
    pub fn load() -> Result<Self> {
        let path = std::env::var("DISPATCH_CONFIG").unwrap_or_else(|_| "config.toml".to_string());
        let contents = std::fs::read_to_string(&path)
            .with_context(|| format!("failed to read config from {path}"))?;
        toml::from_str(&contents).context("failed to parse config")
    }
}

fn default_host() -> String {
    "0.0.0.0".to_string()
}
fn default_port() -> u16 {
    7700
}
fn default_tap_chain_id() -> u64 {
    42161 // Arbitrum One
}
fn default_tap_verifying_contract() -> Address {
    // GraphTallyCollector on Arbitrum One
    "0x8f69F5C07477Ac46FBc491B1E6D91E2bb0111A9e"
        .parse()
        .unwrap()
}
fn default_max_receipt_age_ns() -> u64 {
    30_000_000_000 // 30 seconds
}
fn default_aggregation_interval_secs() -> u64 {
    60
}
fn default_collect_interval_secs() -> u64 {
    3600 // 1 hour
}
fn default_credit_threshold() -> u128 {
    100_000_000_000_000_000 // 0.1 GRT
}
fn default_payments_escrow_address() -> Address {
    // PaymentsEscrow on Arbitrum One (Graph Horizon)
    "0xf6Fcc27aAf1fcD8B254498c9794451d82afC673E"
        .parse()
        .unwrap()
}
