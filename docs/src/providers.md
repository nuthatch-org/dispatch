# Running a Provider

This guide walks through everything needed to join the Dispatch network as a provider — from staking GRT to receiving your first payment. By the end you will have `dispatch-service` running, registered on-chain, and serving live traffic.

---

## What you need

| Requirement | Details |
|---|---|
| **GRT** | ≥ 555 GRT on Arbitrum One for the provision |
| **ETH on Arbitrum** | Small amount for gas (~0.005 ETH is plenty) |
| **Ethereum node(s)** | Full or archive node for each chain you want to serve |
| **Server** | Linux VPS with 2+ vCPUs, ≥ 4 GB RAM, SSD |
| **PostgreSQL** | For TAP receipt + RAV persistence (Docker Compose sets this up automatically) |
| **Public HTTPS endpoint** | Consumers and the gateway need to reach your `dispatch-service` |

---

## 1. Keys

You need two separate keys:

**Provider key** — your on-chain identity. This is the address that holds the GRT provision in HorizonStaking and appears on-chain as `serviceProvider`. You call staking transactions with this key, but it does not need to be on the server.

**Operator key** — a hot key on the server. `dispatch-service` uses this key to sign response attestations and on-chain `collect()` transactions. It must be authorised in HorizonStaking to act on behalf of the provider address.

If you want to keep things simple, you can use the same key for both — `isAuthorized` always returns true when `msg.sender == serviceProvider`. For better security use separate keys.

Generate a fresh operator key if you don't have one:

```bash
cast wallet new
```

Note the address — you will need it in step 2 and again in the service config.

---

## 2. Stake on Horizon

All staking happens on **Arbitrum One** via the HorizonStaking contract at `0x00669A4CF01450B64E8A2A20E9b1FCB71E61eF03`.

### 2a. Stake GRT

If your GRT is in a wallet (not yet staked), approve and stake it:

```bash
# Approve HorizonStaking to spend your GRT
cast send 0x9623063377AD1B27544C965cCd7342f7EA7e88C7 \
  "approve(address,uint256)" \
  0x00669A4CF01450B64E8A2A20E9b1FCB71E61eF03 \
  10000000000000000000000 \
  --private-key $PROVIDER_KEY \
  --rpc-url https://arb1.arbitrum.io/rpc

# Stake to your provider address
cast send 0x00669A4CF01450B64E8A2A20E9b1FCB71E61eF03 \
  "stakeTo(address,uint256)" \
  $PROVIDER_ADDRESS \
  10000000000000000000000 \
  --private-key $PROVIDER_KEY \
  --rpc-url https://arb1.arbitrum.io/rpc
```

Replace the amount with your desired stake in wei (1e18 per GRT). The minimum required by `RPCDataService` is **555 GRT** (`555000000000000000000`).

### 2b. Create a provision

A provision locks a portion of your staked GRT specifically for `RPCDataService`. This is what the contract checks when you register.

```bash
cast send 0x00669A4CF01450B64E8A2A20E9b1FCB71E61eF03 \
  "provision(address,address,uint256,uint32,uint64)" \
  $PROVIDER_ADDRESS \
  0x7101d5c1a5c89c3647f5118da118e56c023ba0b9 \
  555000000000000000000 \
  1000000 \
  1209600 \
  --private-key $PROVIDER_KEY \
  --rpc-url https://arb1.arbitrum.io/rpc
```

Arguments:
- `serviceProvider` — your provider address
- `dataService` — `0x7101d5c1a5c89c3647f5118da118e56c023ba0b9` (RPCDataService)
- `tokens` — amount in wei, minimum `555000000000000000000` (555 GRT)
- `maxVerifierCut` — `1000000` (100% in PPM — the contract cannot slash, so this doesn't matter in practice)
- `thawingPeriod` — `1209600` (14 days in seconds — the contract minimum)

### 2c. Authorise your operator key

If your provider key and operator key are different, authorise the operator:

```bash
cast send 0x00669A4CF01450B64E8A2A20E9b1FCB71E61eF03 \
  "setOperator(address,address,bool)" \
  0x7101d5c1a5c89c3647f5118da118e56c023ba0b9 \
  $OPERATOR_ADDRESS \
  true \
  --private-key $PROVIDER_KEY \
  --rpc-url https://arb1.arbitrum.io/rpc
```

Arguments:
- `dataService` — `0x7101d5c1a5c89c3647f5118da118e56c023ba0b9` (RPCDataService)
- `operator` — your operator address (derived from the hot key on your server)
- `allowed` — `true`

> If you use the same key for both provider and operator, skip this step.

### Verify the provision

```bash
cast call 0x00669A4CF01450B64E8A2A20E9b1FCB71E61eF03 \
  "getProvision(address,address)(uint256,uint256,uint256,uint32,uint64,uint64,uint32,uint64,uint256,uint32)" \
  $PROVIDER_ADDRESS \
  0x7101d5c1a5c89c3647f5118da118e56c023ba0b9 \
  --rpc-url https://arb1.arbitrum.io/rpc
```

The first number is `tokens`. It should be ≥ `555000000000000000000` (555 GRT).

---

## 3. Configure dispatch-service

Clone the repo and copy the example config:

```bash
git clone https://github.com/cargopete/dispatch.git
cd dispatch
cp docker/config.example.toml docker/config.toml
```

Edit `docker/config.toml`:

```toml
[server]
host = "0.0.0.0"
port = 7700

[indexer]
# Your on-chain provider address (the one holding the GRT provision).
service_provider_address = "0xYOUR_PROVIDER_ADDRESS"

# 32-byte hex ECDSA private key of your OPERATOR key.
# This key signs response attestations and on-chain collect() transactions.
# Use a dedicated hot key — NOT your wallet or staking key.
operator_private_key = "0xYOUR_OPERATOR_PRIVATE_KEY"

[tap]
# RPCDataService contract address — do not change this.
data_service_address      = "0x7101d5c1a5c89c3647f5118da118e56c023ba0b9"

# Address(es) of gateway signers that are allowed to send you TAP receipts.
# This is the Ethereum address derived from the gateway's signer_private_key.
# Leave empty ([]) to accept receipts from any sender (less secure but simpler
# when starting out — tighten this once you know your gateway's signer address).
authorized_senders        = []

# EIP-712 domain — must match the deployed GraphTallyCollector. Do not change.
eip712_domain_name        = "GraphTallyCollector"
eip712_chain_id           = 42161
eip712_verifying_contract = "0x8f69F5C07477Ac46FBc491B1E6D91E2bb0111A9e"

# Internal URL of your dispatch-gateway (if you're running one).
# The service posts receipts here for RAV aggregation every 60s.
aggregator_url            = "http://dispatch-gateway:8080"

[chains]
# Chain IDs you want to serve — must have a backend URL for each.
supported = [42161]

[chains.backends]
# Internal RPC URL of your Ethereum node for each chain.
"42161" = "http://your-arbitrum-node:8545"
# "1"   = "http://your-eth-node:8545"

[database]
url = "postgres://dispatch:dispatch@postgres:5432/dispatch"

[collector]
# Arbitrum One RPC for sending the on-chain collect() transaction.
arbitrum_rpc_url      = "https://arb1.arbitrum.io/rpc"
collect_interval_secs = 3600   # collect GRT every hour
```

### Key settings explained

**`service_provider_address`** — your on-chain provider address. This is the address with the GRT provision, registered in `RPCDataService`. It does not need to hold any ETH or signing keys on the server.

**`operator_private_key`** — the hot key on this server. Its address must be authorised as an operator in HorizonStaking (step 2c). It signs TAP response attestations and broadcasts on-chain `collect()` transactions, so it needs a small amount of ETH on Arbitrum One for gas.

**`authorized_senders`** — list of gateway signer addresses allowed to send TAP receipts to this service. If you're routing traffic through the public Dispatch gateway, add its signer address:

```toml
authorized_senders = ["0x7D14ae5f20cc2f6421317386Aa8E79e8728353d9"]
```

Leave empty (`[]`) during initial setup to accept receipts from any sender — tighten this once you've confirmed the payment loop is working.

**`aggregator_url`** — the URL the service POSTs raw receipts to every 60 seconds for RAV aggregation. Two options:

- **Using the public Dispatch gateway** (most providers): `aggregator_url = "https://gateway.lodestar-dashboard.com"`
- **Running your own gateway** (Docker Compose): `aggregator_url = "http://dispatch-gateway:8080"`

The gateway verifies that each receipt was signed by itself, aggregates them into a signed RAV, and returns it for on-chain collection.

**`[collector]`** — when present, `dispatch-service` automatically calls `RPCDataService.collect()` on a timer, pulling GRT from the consumer's escrow to your `paymentsDestination`. If you omit this section, collection does not happen and receipts accumulate without being redeemed.

---

## 4. Run with Docker Compose

Docker Compose is the recommended deployment. It runs `dispatch-service`, `dispatch-gateway`, and PostgreSQL together with health checks and automatic restarts.

```bash
# Copy and fill in both config files
cp docker/config.example.toml  docker/config.toml   # indexer service
cp docker/gateway.example.toml docker/gateway.toml   # gateway (optional)

# Start everything
docker compose -f docker/docker-compose.yml up -d dispatch-service dispatch-gateway postgres
```

Check that all three containers are healthy:

```bash
docker ps
```

You should see `(healthy)` next to `dispatch-service`, `dispatch-gateway`, and `postgres`.

Check the service logs:

```bash
docker logs docker-dispatch-service-1 --tail 30
```

Expected output on startup:

```
INFO dispatch_service::db: database migrations applied
INFO dispatch_service::tap_aggregator: RAV aggregator started url=http://dispatch-gateway:8080 interval_secs=60
INFO dispatch_service::collector: on-chain RAV collector started interval_secs=3600
INFO dispatch_service::server: dispatch-service starting addr=0.0.0.0:7700
```

---

## 5. Register on-chain

Once the service is running, register your provider in `RPCDataService` and activate each chain you want to serve. The **indexer agent** handles this automatically.

### Using the npm package

```bash
npm install @lodestar-dispatch/indexer-agent
```

Create `agent.config.json`:

```json
{
  "arbitrumRpcUrl": "https://arb1.arbitrum.io/rpc",
  "rpcDataServiceAddress": "0x7101d5c1a5c89c3647f5118da118e56c023ba0b9",
  "operatorPrivateKey": "0xYOUR_OPERATOR_PRIVATE_KEY",
  "providerAddress": "0xYOUR_PROVIDER_ADDRESS",
  "endpoint": "https://rpc.your-domain.com",
  "geoHash": "u1hx",
  "paymentsDestination": "0xYOUR_PAYMENT_WALLET",
  "services": [
    { "chainId": 42161, "tier": 0 },
    { "chainId": 42161, "tier": 1 }
  ]
}
```

Run it:

```bash
AGENT_CONFIG=./agent.config.json npx dispatch-indexer-agent
```

The agent calls `register()`, `startService()` for each entry in `services`, and `stopService()` / `deregister()` on SIGTERM. It reconciles on-chain state against the config on every run — safe to run on a cron or as a persistent daemon.

### Config fields

| Field | Description |
|---|---|
| `operatorPrivateKey` | Hot key on your server — must be authorised as operator in HorizonStaking |
| `providerAddress` | Your on-chain provider address (holds the GRT provision) |
| `endpoint` | Public HTTPS base URL of your `dispatch-service`, reachable by gateways and consumers |
| `geoHash` | [Geohash](https://geohash.softeng.kr/) of your server location — used for geographic routing. 4 characters is sufficient (e.g. `u1hx` for Amsterdam, `dr4g` for New York) |
| `paymentsDestination` | Address that receives collected GRT. If omitted, defaults to `providerAddress`. Use a cold wallet here |
| `services` | List of `{ chainId, tier }` pairs — see Capability tiers below |

### Verify registration

```bash
cast call 0x7101d5c1a5c89c3647f5118da118e56c023ba0b9 \
  "isRegistered(address)(bool)" \
  $PROVIDER_ADDRESS \
  --rpc-url https://arb1.arbitrum.io/rpc
```

Should return `true`.

```bash
cast call 0x7101d5c1a5c89c3647f5118da118e56c023ba0b9 \
  "getChainRegistrations(address)" \
  $PROVIDER_ADDRESS \
  --rpc-url https://arb1.arbitrum.io/rpc
```

Should show your registered `(chainId, tier)` pairs with `active = true`.

---

## 6. Expose your endpoint

Your `dispatch-service` must be reachable at a public HTTPS URL. Port 7700 by default — put it behind nginx or Caddy with a TLS cert.

Minimal nginx config:

```nginx
server {
    server_name rpc.your-domain.com;

    location / {
        proxy_pass         http://127.0.0.1:7700;
        proxy_set_header   Host $host;
        proxy_set_header   X-Real-IP $remote_addr;

        # WebSocket support
        proxy_http_version 1.1;
        proxy_set_header   Upgrade $http_upgrade;
        proxy_set_header   Connection "upgrade";
        proxy_read_timeout 3600s;
    }

    listen 443 ssl;
    ssl_certificate     /etc/letsencrypt/live/rpc.your-domain.com/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/rpc.your-domain.com/privkey.pem;
}
```

Test it:

```bash
curl -s https://rpc.your-domain.com/health
```

Should return `{"status":"ok"}`.

---

## 7. Verify the payment loop

Make a test request through your service (with a valid TAP receipt) and confirm the full loop works. The easiest way is the smoke test binary:

```bash
DISPATCH_ENDPOINT=https://rpc.your-domain.com \
DISPATCH_SIGNER_KEY=<any-key-in-authorized_senders-or-any-key-if-empty> \
DISPATCH_PROVIDER_ADDRESS=$PROVIDER_ADDRESS \
cargo run --bin dispatch-smoke
```

All 5 checks should pass. After 60 seconds, check service logs for:

```
INFO dispatch_service::tap_aggregator: RAV aggregated collection_id=... value=...
```

After an hour (or force a collect manually):

```
INFO dispatch_service::collector: collect() success tx=0x...
```

GRT lands in your `paymentsDestination` wallet.

---

## Capability tiers

Not all Ethereum nodes can answer all requests. A standard full node only keeps recent state (~128 blocks) — ask it for a balance at block 1,000,000 and it fails. A node without debug APIs enabled can't serve `debug_traceTransaction`. If a gateway routed those requests blindly it would just get errors.

Capability tiers are how the network avoids that. Each tier describes a distinct infrastructure capability. You declare which tiers your node supports at registration time, and the gateway only routes requests to providers that can actually answer them.

| Tier | Value | What it serves | Node requirement |
|---|---|---|---|
| Standard | `0` | All standard JSON-RPC methods, recent ~128 blocks | Any full node |
| Archive | `1` | Historical state at any block number | Archive node (~10–20× more disk) |
| Debug/Trace | `2` | `debug_*` and `trace_*` methods | Full/archive node with debug APIs enabled (`--http.api=debug,trace`) |
| WebSocket | `3` | `eth_subscribe`, real-time event streams | Full node with a WebSocket endpoint |

### One registration per (chain, tier) pair

Registration is granular. You call `startService(chainId, tier, endpoint)` once for each capability you want to advertise — each call is a separate on-chain record. This means you can mix and match freely:

- Archive on Ethereum mainnet, Standard only on Arbitrum
- Debug on one chain, nothing on another
- WebSocket on all chains, Archive on none

The `services` array in your indexer agent config maps directly to these calls:

```json
"services": [
  { "chainId": 1,     "tier": 0 },
  { "chainId": 1,     "tier": 1 },
  { "chainId": 42161, "tier": 0 }
]
```

This registers Standard and Archive on Ethereum mainnet, and Standard only on Arbitrum One. Three `startService` calls, three on-chain records.

### Stake is shared

Your staked GRT covers all tiers and all chains — there is no per-tier or per-chain stake splitting. The full provision applies regardless of how many (chain, tier) pairs you register for.

### Start with what your node supports

If you're running a standard full node, register tier `0` only. If it's an archive node, add tier `1`. Only enable tier `2` if you've explicitly enabled debug/trace APIs on your node — requests routed to you will fail otherwise and hurt your QoS score.

---

## Supported chains

| Chain | Chain ID |
|---|---|
| Ethereum | 1 |
| Arbitrum One | 42161 |
| Optimism | 10 |
| Base | 8453 |
| Polygon | 137 |
| BNB Chain | 56 |
| Avalanche C-Chain | 43114 |
| zkSync Era | 324 |
| Linea | 59144 |
| Scroll | 534352 |

Chains are governance-controlled. New chains are added via `RPCDataService.addChain()`.

---

## Managing your provision

**Add more stake to your provision** (if you want to serve more chains or increase your safety margin):

```bash
cast send 0x00669A4CF01450B64E8A2A20E9b1FCB71E61eF03 \
  "addToProvision(address,address,uint256)" \
  $PROVIDER_ADDRESS \
  0x7101d5c1a5c89c3647f5118da118e56c023ba0b9 \
  5000000000000000000000 \
  --private-key $PROVIDER_KEY \
  --rpc-url https://arb1.arbitrum.io/rpc
```

**Start thawing** (to eventually remove GRT from the provision):

```bash
cast send 0x00669A4CF01450B64E8A2A20E9b1FCB71E61eF03 \
  "thaw(address,address,uint256)" \
  $PROVIDER_ADDRESS \
  0x7101d5c1a5c89c3647f5118da118e56c023ba0b9 \
  10000000000000000000000 \
  --private-key $PROVIDER_KEY \
  --rpc-url https://arb1.arbitrum.io/rpc
```

After the 14-day thawing period, call `deprovision` to release the tokens back to idle stake, then `unstake` to return them to your wallet.

**Update your payments destination** (without re-registering):

```bash
cast send 0x7101d5c1a5c89c3647f5118da118e56c023ba0b9 \
  "setPaymentsDestination(address)" \
  $NEW_WALLET \
  --private-key $OPERATOR_KEY \
  --rpc-url https://arb1.arbitrum.io/rpc
```

**Stop serving a chain** (without deregistering):

Send `stopService` via the indexer agent by removing the entry from `services` in `agent.config.json` and re-running. Or call directly:

```bash
cast send 0x7101d5c1a5c89c3647f5118da118e56c023ba0b9 \
  "stopService(address,bytes)" \
  $PROVIDER_ADDRESS \
  $(cast abi-encode "f(uint64,uint8)" 42161 0) \
  --private-key $OPERATOR_KEY \
  --rpc-url https://arb1.arbitrum.io/rpc
```

---

## Deployed addresses (Arbitrum One)

| Contract | Address |
|---|---|
| HorizonStaking | `0x00669A4CF01450B64E8A2A20E9b1FCB71E61eF03` |
| GRT Token | `0x9623063377AD1B27544C965cCd7342f7EA7e88C7` |
| GraphTallyCollector | `0x8f69F5C07477Ac46FBc491B1E6D91E2bb0111A9e` |
| PaymentsEscrow | `0xf6Fcc27aAf1fcD8B254498c9794451d82afC673E` |
| GraphPayments | `0x7Aae8ae011927BC36Cb4d0d3e81f2E6E30daE06D` |
| RPCDataService | `0x7101d5c1a5c89c3647f5118da118e56c023ba0b9` |
