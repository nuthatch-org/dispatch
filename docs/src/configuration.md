# Configuration

---

## dispatch-service (`config.toml`)

```toml
[server]
host = "0.0.0.0"
port = 7700

[indexer]
# Your on-chain provider address (holds the GRT provision).
service_provider_address = "0x..."

# 32-byte hex ECDSA private key of your operator key.
# Signs response attestations and on-chain collect() transactions.
# Use a dedicated hot key — NOT your wallet or staking key.
operator_private_key = "0x..."

[tap]
# RPCDataService contract address.
data_service_address      = "0x7101d5c1a5c89c3647f5118da118e56c023ba0b9"

# Ethereum addresses authorised to send TAP receipts to this service.
# Derived from the gateway's signer_private_key. Leave empty to accept all.
authorized_senders        = ["0x..."]

# EIP-712 domain — must match the deployed GraphTallyCollector. Do not change.
eip712_domain_name        = "GraphTallyCollector"
eip712_chain_id           = 42161
eip712_verifying_contract = "0x8f69F5C07477Ac46FBc491B1E6D91E2bb0111A9e"

# Internal URL of your dispatch-gateway (for RAV aggregation).
aggregator_url            = "http://dispatch-gateway:8080"

# How often to aggregate receipts into RAVs (default: 60s).
# aggregation_interval_secs = 60

# Maximum unconfirmed GRT wei per consumer before rejecting requests (default: 0.1 GRT).
# credit_threshold = 100_000_000_000_000_000

# Arbitrum One RPC for on-chain escrow balance pre-checks (cached 30s per consumer).
# Falls back to [collector].arbitrum_rpc_url. Omit both to disable escrow checks.
# escrow_check_rpc_url = "https://arb1.arbitrum.io/rpc"

# PaymentsEscrow contract — defaults to the live Horizon deployment, no need to change.
# payments_escrow_address = "0xf6Fcc27aAf1fcD8B254498c9794451d82afC673E"

[chains]
# Chain IDs this node is registered to serve.
supported = [1, 42161]

[chains.backends]
# Internal RPC URL of your Ethereum node for each chain.
"1"     = "http://eth-node:8545"
"42161" = "http://arbitrum-node:8545"

[database]
url = "postgres://dispatch:dispatch@postgres:5432/dispatch"

[collector]
# Arbitrum One RPC for sending on-chain collect() transactions.
arbitrum_rpc_url      = "https://arb1.arbitrum.io/rpc"
collect_interval_secs = 3600   # collect GRT every hour
# min_collect_value = 0        # skip collect if accumulated value is below this (GRT wei)
```

---

## dispatch-gateway (`gateway.toml`)

```toml
[gateway]
host   = "0.0.0.0"
port   = 8080
region = "eu-west"   # optional — used for geographic routing bonus

[tap]
# 32-byte hex ECDSA private key — gateway signs TAP receipts with this.
# Its derived Ethereum address must be in each provider's authorized_senders list.
signer_private_key        = "0x..."

# RPCDataService contract address.
data_service_address      = "0x7101d5c1a5c89c3647f5118da118e56c023ba0b9"

# GRT wei per compute unit. Default ≈ $40/M requests at $0.09/GRT.
base_price_per_cu         = 4000000000000

# EIP-712 domain — must match the deployed GraphTallyCollector. Do not change.
eip712_domain_name        = "GraphTallyCollector"
eip712_chain_id           = 42161
eip712_verifying_contract = "0x8f69F5C07477Ac46FBc491B1E6D91E2bb0111A9e"

[qos]
probe_interval_secs = 10    # synthetic eth_blockNumber probe period
concurrent_k        = 3     # providers tried in order before giving up
cross_check_rate    = 0.02  # share of settled deterministic calls asked of a second provider
region_bonus        = 0.15  # score boost for providers in the same region

[discovery]
# The Graph subgraph URL for dynamic provider discovery.
source        = "nest"              # or "chain": read RPCDataService events straight from Arbitrum
nest_url      = "http://nest:8120"
interval_secs = 60

# Optional: static providers used at startup and as fallback.
[[providers]]
address      = "0x..."
endpoint     = "https://rpc.your-indexer.com"
chains       = [1, 42161]
region       = "eu-west"
capabilities = ["standard", "archive"]
```

---

## Environment variables

`dispatch-service`:

```bash
DISPATCH_CONFIG=/etc/dispatch/config.toml   # path to config file (default: config.toml)
RUST_LOG=info                                # log level: error, warn, info, debug, trace
```

`dispatch-gateway`:

```bash
DISPATCH_GATEWAY_CONFIG=/etc/dispatch/gateway.toml
RUST_LOG=info
```
