use std::collections::HashMap;

use alloy_primitives::Address;
use anyhow::{Context, Result};
use serde::{Deserialize, Deserializer};

/// Deserialize a u128 from either a TOML integer (≤ i64::MAX) or a quoted
/// decimal string. TOML integers are 64-bit signed, so values that fit in u64
/// can be expressed as plain integers; larger values must be quoted strings.
fn deserialize_u128<'de, D: Deserializer<'de>>(d: D) -> Result<u128, D::Error> {
    use serde::de::Error;
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum U128Helper {
        Int(i64),
        Str(String),
    }
    match U128Helper::deserialize(d)? {
        U128Helper::Int(v) => u128::try_from(v).map_err(D::Error::custom),
        U128Helper::Str(s) => s.parse::<u128>().map_err(D::Error::custom),
    }
}

/// RPC capability tier a provider can serve.
///
/// Providers declare which tiers they support; the gateway filters the candidate
/// pool to only providers capable of serving a given request.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CapabilityTier {
    /// Standard full-node methods — last ~128 blocks.
    Standard,
    /// Full historical state — archive node required.
    Archive,
    /// `debug_*` and `trace_*` methods — debug API required.
    Debug,
}

#[derive(Debug, Deserialize, Clone)]
pub struct Config {
    pub gateway: GatewayConfig,
    pub tap: TapConfig,
    pub qos: QosConfig,
    /// Static providers (used at startup or when no subgraph is configured).
    #[serde(default)]
    pub providers: Vec<ProviderConfig>,
    /// Optional subgraph-based dynamic discovery.
    pub discovery: Option<DiscoveryConfig>,
    /// Optional per-IP rate limiting.
    pub rate_limit: Option<RateLimitConfig>,
    /// Optional dispatch-service URL for proxying receipt feed queries.
    pub service: Option<ServiceConfig>,
    /// Optional auto-provisioning: fund escrow for new providers automatically.
    pub provisioning: Option<ProvisioningConfig>,
    /// Read-only Arbitrum One access, for operator lookups and the RAV floor.
    pub chain: Option<ChainConfig>,
    /// Optional Seahorn (Solana structured data) proxy configuration.
    pub seahorn: Option<SeahornConfig>,
    /// API keys. Without this section the gateway serves anyone.
    pub auth: Option<crate::auth::AuthConfig>,
    /// USD price list. Without it, receipts are priced in GRT by `tap.base_price_per_cu`.
    pub pricing: Option<crate::pricing::PricingConfig>,
}

/// Read-only chain access. Kept apart from `[provisioning]`, which also needs a payer key that a
/// gateway reading the chain has no business holding.
#[derive(Debug, Deserialize, Clone)]
pub struct ChainConfig {
    pub arbitrum_rpc_url: String,
}

impl Config {
    /// `[chain]`, else the provisioner's RPC, else none.
    pub fn arbitrum_rpc_url(&self) -> Option<&str> {
        self.chain
            .as_ref()
            .map(|c| c.arbitrum_rpc_url.as_str())
            .or(self
                .provisioning
                .as_ref()
                .map(|p| p.arbitrum_rpc_url.as_str()))
    }
}

/// Connection details for the local dispatch-service instance.
/// When configured, the gateway proxies `/receipts/recent` and `/receipts`
/// to this URL so the dashboard can query receipt data through the public gateway.
#[derive(Debug, Deserialize, Clone)]
pub struct ServiceConfig {
    /// Base URL of the dispatch-service, e.g. "http://localhost:7700".
    pub url: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct GatewayConfig {
    #[serde(default = "default_host")]
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    /// Geographic region of this gateway instance (e.g. "us-east", "eu-west").
    /// Used to prefer nearby providers before latency data is established.
    pub region: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct TapConfig {
    /// Gateway operator private key (hex) — signs TAP receipts sent to providers.
    pub signer_private_key: String,
    /// Gateway operator wallet address — used as the payer in all TAP receipts.
    /// This wallet funds escrow for every provider the gateway routes to.
    /// Consumers interact with the gateway and are billed at the gateway level;
    /// individual providers only ever see this single payer address on-chain.
    pub gateway_payer_address: Address,
    /// RPCDataService contract address.
    pub data_service_address: Address,
    /// GRT wei charged per compute unit. Default ≈ $40/M requests at $0.09 GRT.
    #[serde(
        default = "default_base_price_per_cu",
        deserialize_with = "deserialize_u128"
    )]
    pub base_price_per_cu: u128,
    /// EIP-712 domain name for GraphTallyCollector.
    pub eip712_domain_name: String,
    /// Chain ID where GraphTallyCollector is deployed (42161 = Arbitrum One).
    #[serde(default = "default_tap_chain_id")]
    pub eip712_chain_id: u64,
    /// GraphTallyCollector contract address.
    #[serde(default = "default_tap_verifying_contract")]
    pub eip712_verifying_contract: Address,
    /// Where the last signed RAV per collection is kept. Put it on a volume: if it is lost, the
    /// next RAV for each collection starts from on-chain `tokensCollected`.
    #[serde(default = "default_rav_ledger_path")]
    pub rav_ledger_path: String,
    /// HorizonStaking, asked whether an attestation signer is an operator of its provider.
    #[serde(default = "default_horizon_staking")]
    pub horizon_staking_address: Address,
}

#[derive(Debug, Deserialize, Clone)]
pub struct QosConfig {
    /// How often to probe all providers with synthetic eth_blockNumber requests.
    #[serde(default = "default_probe_interval_secs")]
    pub probe_interval_secs: u64,
    /// Number of providers to dispatch to concurrently (first response wins).
    #[serde(default = "default_concurrent_k")]
    pub concurrent_k: usize,
    /// Number of providers to query for quorum on deterministic methods.
    #[serde(default = "default_quorum_k")]
    pub quorum_k: usize,
    /// Score bonus added for providers in the same region as this gateway.
    #[serde(default = "default_region_bonus")]
    pub region_bonus: f64,
    /// Treat a response without an attestation bound to its provider as a provider failure.
    #[serde(default)]
    pub require_attestation: bool,
    /// Share of deterministic calls on settled blocks that a second provider is also asked.
    #[serde(default = "default_cross_check_rate")]
    pub cross_check_rate: f64,
    /// How long a provider caught disagreeing has every deterministic call sent to quorum.
    #[serde(default = "default_suspect_secs")]
    pub suspect_secs: u64,
    /// The old behaviour: every deterministic call to `quorum_k` providers, all of them paid.
    #[serde(default)]
    pub quorum_always: bool,
}

/// Static provider configuration.
/// Used as a fallback when no subgraph is configured, or as the initial set
/// before the first successful subgraph poll.
#[derive(Debug, Deserialize, Clone)]
pub struct ProviderConfig {
    /// Indexer's on-chain address (used as `service_provider` in TAP receipts).
    pub address: Address,
    /// Base URL of the indexer's dispatch-service endpoint, e.g. "https://rpc.example.com".
    pub endpoint: String,
    /// Chain IDs this provider is registered to serve.
    pub chains: Vec<u64>,
    /// Geographic region of this provider (e.g. "us-east", "eu-west").
    /// Matched against `[gateway].region` for proximity-aware routing.
    pub region: Option<String>,
    /// Capability tiers this provider supports. Defaults to `[standard]`.
    #[serde(default = "default_capabilities")]
    pub capabilities: Vec<CapabilityTier>,
    /// Per-chain capability overrides, populated by dynamic discovery.
    /// When non-empty, used in place of the global `capabilities` field for
    /// per-chain tier filtering. Empty for static (TOML) provider configs.
    #[serde(default)]
    pub chain_capabilities: HashMap<u64, Vec<CapabilityTier>>,
    /// The provider's own minimum price per class. Only applies with a `[pricing]` section.
    #[serde(default)]
    pub price_floor: Option<crate::pricing::PriceFloor>,
}

/// Dynamic provider discovery via The Graph subgraph.
#[derive(Debug, Deserialize, Clone)]
pub struct DiscoveryConfig {
    /// GraphQL endpoint of the RPC network subgraph.
    pub subgraph_url: String,
    /// How often to poll the subgraph for provider updates (seconds).
    #[serde(default = "default_discovery_interval_secs")]
    pub interval_secs: u64,
}

/// Per-IP rate limiting for the RPC endpoint.
#[derive(Debug, Deserialize, Clone)]
pub struct RateLimitConfig {
    /// Steady-state requests per second per IP address.
    #[serde(default = "default_rps")]
    pub requests_per_second: u32,
    /// Burst capacity above the steady-state rate.
    #[serde(default = "default_burst")]
    pub burst: u32,
}

/// Auto-provisioning: automatically fund escrow for newly discovered providers.
#[derive(Debug, Deserialize, Clone)]
pub struct ProvisioningConfig {
    /// Arbitrum One RPC URL used to send on-chain transactions.
    pub arbitrum_rpc_url: String,
    /// Private key of the gateway payer wallet (hex). Used to sign approve/deposit txns.
    pub gateway_payer_private_key: String,
    /// GRT token contract address on Arbitrum One.
    #[serde(default = "default_grt_token_address")]
    pub grt_token_address: Address,
    /// PaymentsEscrow contract address on Arbitrum One.
    #[serde(default = "default_payments_escrow_address")]
    pub escrow_address: Address,
    /// TAP collector contract (GraphTallyCollector) — passed as `collector` to deposit().
    #[serde(default = "default_tap_verifying_contract")]
    pub collector_address: Address,
    /// GRT wei to deposit per provider when their escrow is below the threshold.
    #[serde(
        default = "default_deposit_per_provider",
        deserialize_with = "deserialize_u128"
    )]
    pub deposit_per_provider: u128,
    /// GRT wei threshold — deposit when escrow balance falls below this.
    #[serde(
        default = "default_min_escrow_threshold",
        deserialize_with = "deserialize_u128"
    )]
    pub min_escrow_threshold: u128,
    /// How often to check and top-up provider escrow (seconds).
    #[serde(default = "default_provision_interval_secs")]
    pub interval_secs: u64,
}

/// Seahorn (Solana structured data service) proxy configuration.
///
/// When present, the gateway exposes a `/solana/*path` route that proxies
/// REST queries to the Seahorn provider, attaching a signed TAP receipt.
#[derive(Debug, Deserialize, Clone)]
pub struct SeahornConfig {
    /// Base URL of the Seahorn provider, e.g. "https://solana.lodestar-dashboard.com".
    pub endpoint: String,
    /// Seahorn provider's on-chain service provider address.
    pub service_provider: Address,
    /// SolanaDataService contract address — written into every TAP receipt.
    pub data_service_address: Address,
    /// GRT wei charged per Seahorn query.
    #[serde(
        default = "default_seahorn_price_grt_wei",
        deserialize_with = "deserialize_u128"
    )]
    pub price_grt_wei: u128,
}

impl Config {
    pub fn load() -> Result<Self> {
        let path =
            std::env::var("DISPATCH_GATEWAY_CONFIG").unwrap_or_else(|_| "gateway.toml".to_string());
        let contents = std::fs::read_to_string(&path)
            .with_context(|| format!("failed to read gateway config from {path}"))?;
        toml::from_str(&contents).context("failed to parse gateway config")
    }
}

fn default_capabilities() -> Vec<CapabilityTier> {
    vec![CapabilityTier::Standard]
}
fn default_region_bonus() -> f64 {
    0.15
}
fn default_host() -> String {
    "0.0.0.0".to_string()
}
fn default_port() -> u16 {
    8080
}
fn default_base_price_per_cu() -> u128 {
    4_000_000_000_000
} // 4e-6 GRT/CU; see pricing_math test
fn default_rav_ledger_path() -> String {
    "rav-ledger.json".to_string()
}
fn default_horizon_staking() -> Address {
    "0x00669A4CF01450B64E8A2A20E9b1FCB71E61eF03"
        .parse()
        .unwrap()
}
fn default_tap_chain_id() -> u64 {
    42161
}
fn default_tap_verifying_contract() -> Address {
    "0x8f69F5C07477Ac46FBc491B1E6D91E2bb0111A9e"
        .parse()
        .unwrap()
}
fn default_probe_interval_secs() -> u64 {
    10
}
fn default_concurrent_k() -> usize {
    3
}
fn default_cross_check_rate() -> f64 {
    0.02
}
fn default_suspect_secs() -> u64 {
    600
}
fn default_quorum_k() -> usize {
    3
}
fn default_discovery_interval_secs() -> u64 {
    60
}
fn default_rps() -> u32 {
    100
}
fn default_burst() -> u32 {
    20
}
fn default_grt_token_address() -> Address {
    "0x9623063377AD1B27544C965cCd7342f7EA7e88C7"
        .parse()
        .unwrap()
}
fn default_payments_escrow_address() -> Address {
    "0xf6Fcc27aAf1fcD8B254498c9794451d82afC673E"
        .parse()
        .unwrap()
}
fn default_deposit_per_provider() -> u128 {
    100_000_000_000_000_000_000
} // 100 GRT
fn default_min_escrow_threshold() -> u128 {
    10_000_000_000_000_000_000
} // 10 GRT
fn default_provision_interval_secs() -> u64 {
    600
} // 10 minutes
fn default_seahorn_price_grt_wei() -> u128 {
    10_000_000_000_000
} // 10e12 GRT wei ≈ $0.00026 per query at $0.026/GRT

#[cfg(test)]
mod tests {
    use super::*;

    /// Pricing proof for default base_price_per_cu = 4_000_000_000_000 GRT wei/CU.
    ///
    /// GRT has 18 decimal places → 4e12 / 1e18 = 4e-6 GRT per CU.
    /// The gateway dispatches to 3 providers concurrently; all receive a receipt.
    /// Effective consumer cost = per-provider receipt × 3.
    ///
    /// At $0.09/GRT:
    ///   eth_blockNumber ( 1 CU):  $1.08/M calls  (Alchemy: $4.50/M)
    ///   eth_getBalance  ( 5 CU):  $5.40/M calls  (Alchemy: $4.50/M)
    ///   eth_call        (10 CU): $10.80/M calls  (Alchemy: $11.70/M)
    ///   eth_getLogs     (20 CU): $21.60/M calls  (Alchemy: $33.75/M)
    ///
    /// Break-even vs Alchemy on eth_call: ~$0.10/GRT.
    #[test]
    fn pricing_math() {
        let base = default_base_price_per_cu();
        assert_eq!(base, 4_000_000_000_000_u128);

        // 1M CUs in GRT wei = exactly 4 GRT (4×10^18 wei)
        assert_eq!(base * 1_000_000, 4_000_000_000_000_000_000_u128);

        // Per-method receipt values (GRT wei, single provider)
        assert_eq!(base, 4_000_000_000_000_u128); // eth_blockNumber
        assert_eq!(5_u128 * base, 20_000_000_000_000_u128); // eth_getBalance
        assert_eq!(10_u128 * base, 40_000_000_000_000_u128); // eth_call
        assert_eq!(20_u128 * base, 80_000_000_000_000_u128); // eth_getLogs

        // USD cost per million calls (×3 concurrent, $0.09/GRT):
        // receipt_wei × 3 × 1e6 × $0.09 / 1e18
        let factor: f64 = 3.0 * 1_000_000.0 * 0.09 / 1e18;
        let eth_call_usd_per_m = 40_000_000_000_000_f64 * factor;
        let eth_get_logs_usd_per_m = 80_000_000_000_000_f64 * factor;
        assert!(
            (eth_call_usd_per_m - 10.80).abs() < 0.01,
            "eth_call: ${eth_call_usd_per_m:.2}/M"
        );
        assert!(
            (eth_get_logs_usd_per_m - 21.60).abs() < 0.01,
            "eth_getLogs: ${eth_get_logs_usd_per_m:.2}/M"
        );
    }

    #[test]
    fn chain_rpc_does_not_need_provisioning() {
        let base = r#"
            [gateway]
            host = "127.0.0.1"
            port = 0
            [tap]
            signer_private_key = "0x01"
            gateway_payer_address = "0x0000000000000000000000000000000000000000"
            data_service_address = "0x7101d5c1a5c89c3647f5118da118e56c023ba0b9"
            eip712_domain_name = "GraphTallyCollector"
            [qos]
        "#;
        let none: Config = toml::from_str(base).unwrap();
        assert_eq!(none.arbitrum_rpc_url(), None);

        let with_chain: Config = toml::from_str(&format!(
            "{base}\n[chain]\narbitrum_rpc_url = \"https://arb1.arbitrum.io/rpc\""
        ))
        .unwrap();
        assert_eq!(
            with_chain.arbitrum_rpc_url(),
            Some("https://arb1.arbitrum.io/rpc")
        );
        assert!(with_chain.provisioning.is_none());
    }

    #[test]
    fn a_provider_floor_parses_from_its_config_entry() {
        let p: ProviderConfig = toml::from_str(
            r#"
            address = "0xCfBB471617Ae3F6fDa2D9F142115d59deAB50C5b"
            endpoint = "https://dispatch-rpc.cargopete.com"
            chains = [10]
            [price_floor]
            standard_usd_per_million = 4.0
            trace_usd_per_million = 8.0
            "#,
        )
        .unwrap();
        let f = p.price_floor.unwrap();
        assert_eq!(f.standard_usd_per_million, Some(4.0));
        assert_eq!(f.archive_usd_per_million, None);
        assert_eq!(f.trace_usd_per_million, Some(8.0));
    }
}
