# Dispatch Economics — Profit Calculation

There are two separate revenue streams for a Dispatch provider, and they work very differently.

---

## Stream 1: Subgraph Indexing Rewards

This only applies if you are also running as a Graph Protocol subgraph indexer. It is GRT inflation, distributed each epoch proportionally to signal and stake:

```
monthly_reward = monthly_issuance × (subgraph_signal / total_signal) × (our_allocation / subgraph_total_allocation)
```

- Network monthly issuance: ~26.1M GRT (as of Q2 2026)
- Total network signal: ~9M GRT
- Top indexers earn thousands of GRT/month from well-chosen allocations

These rewards are **not paid per-request** — they accrue continuously and are claimed when an allocation is closed. The key levers are subgraph selection (signal-to-stake ratio) and total GRT provisioned (self-stake + delegated).

This is the steady base income. Dispatch query fees are the variable upside.

---

## Stream 2: Dispatch RPC Query Fees

Fully pay-per-request via **GraphTally (TAP v2)** micropayments. No epoch, no allocation — every request generates a signed receipt, receipts aggregate into RAVs, RAVs are redeemed on-chain hourly.

### Pricing model

With a `[pricing]` section the gateway prices in USD per million requests, by class: standard, archive, and trace (`debug_*`, `trace_*` and `eth_getLogs`). The suggested list is $1, $3 and $4, just under dRPC's flat $4.20 to $6. Each receipt's GRT value is worked out from the current GRT/USD rate when it is signed, so providers are still paid in GRT and the buyer's price does not move with it. `GET /pricing` publishes the live list and rate. The CU model below applies when `[pricing]` is absent.

The gateway charges per **compute unit (CU)**, where one CU = `4_000_000_000_000` GRT wei = `4e-6 GRT`.

Method complexity sets the CU cost. Values below are confirmed from live receipt data:

| RPC Method | CUs | Per-provider receipt |
|---|---|---|
| `eth_chainId` | 1 | 4e-6 GRT |
| `eth_getBlockByHash` | 5 | 20e-6 GRT |
| `eth_getBlockByNumber` | 5 | 20e-6 GRT |
| `eth_getBlockReceipts` | 10 | 40e-6 GRT |
| `eth_call` | 10 | 40e-6 GRT |
| `eth_getLogs` | 20 | 80e-6 GRT |

**Concurrent dispatch multiplier:** The gateway is configured to dispatch to up to `concurrent_k = 3` providers simultaneously (first response wins), and all providers that respond receive a signed receipt — that is the cost of censorship-resistance and latency optimisation. With the network currently at 1 active provider, the effective multiplier is ×1. Once multiple providers are live, consumers pay the per-provider receipt value × k for each request.

### USD cost at different GRT prices

| Method | At $0.0246/GRT | At $0.09/GRT | Alchemy (reference) |
|---|---|---|---|
| `eth_blockNumber` /M calls | ~$0.35 | ~$1.08 | $4.50 |
| `eth_call` /M calls | ~$2.95 | ~$10.80 | $11.70 |
| `eth_getLogs` /M calls | ~$5.90 | ~$21.60 | $33.75 |

At $0.09/GRT Dispatch is competitive with or cheaper than Alchemy on every method. At today's depressed GRT price it is significantly cheaper — meaning providers earn less per request, but consumers get a good deal.

This math is verified by a test in the gateway codebase: `crates/dispatch-gateway/src/config.rs::tests::pricing_math`.

---

## Fee Distribution (on-chain)

When a provider calls `RPCDataService.collect()`, fees flow through the Horizon stack:

```
RAV value (gross)
  ├── ~2% protocol tax  → burned / treasury (GraphPayments)
  ├── 2% data service cut (RPCDataService)
  │     ├── 1% burned   (BURN_CUT_PPM = 10_000 ppm)
  │     └── 1% retained as data service revenue
  ├── delegator cut     (set by provider, e.g. 10% of remainder)
  └── provider          (remainder — typically ~86%)
```

Constants from `contracts/src/RPCDataService.sol`:

```solidity
uint256 public constant BURN_CUT_PPM       = 10_000;  // 1%
uint256 public constant DATA_SERVICE_CUT_PPM = 10_000;  // 1%
uint256 public constant STAKE_TO_FEES_RATIO  = 5;      // 5× stake locked per GRT collected
```

**Stake locking:** each `collect()` call locks `fees × 5` GRT in a stake claim for 14 days (dispute window). A provider collecting 100 GRT/month needs at least 500 GRT provisioned — identical to how SubgraphService works.

---

## Payment Lifecycle

```
Consumer signs TAP receipt per request (EIP-712, random nonce, CU-weighted value)
    ↓  (every 60 seconds)
dispatch-service aggregates receipts → RAV (Receipt Aggregate Voucher, monotonically increasing)
    ↓  (every hour)
provider calls RPCDataService.collect(signedRAV)
    → GraphTallyCollector validates RAV signature
    → PaymentsEscrow debits consumer's on-chain deposit
    → GraphPayments distributes: protocol tax → data service → delegators → provider
    → GRT lands in provider's paymentsDestination wallet
```

Consumers must pre-fund an escrow on-chain. The provider checks escrow balance before serving requests (configurable via `credit_threshold`; self-consumption can bypass this check via `bypass_consumers`).

---

## Provider P&L — Worked Example

Assume a provider handling **10M `eth_call` requests/month** at $0.09/GRT:

| Item | Value |
|---|---|
| Gross receipts (10M × 40e-6 GRT) | 400 GRT |
| Data service cut (2%) | –8 GRT |
| Protocol tax (~2%) | –8 GRT |
| Delegator cut (10%) | –38.4 GRT |
| **Provider net** | **~345.6 GRT ≈ $31** |
| Backend node cost (Chainstack Growth) | ~$200/month |

At 10M calls/month the Dispatch fees alone do not cover a dedicated archive node. Volume is everything — this becomes meaningful at **hundreds of millions of requests per month**, which requires either consumer acquisition or acting as the RPC backbone for other indexers.

The immediate value for early providers is not query-fee income but:
1. Production validation of the payment loop
2. First-mover positioning as the network grows
3. Dogfooding as your own consumer (self-consumption offsets some RPC costs)

---

## Infrastructure Cost Scenarios

Provider economics vary significantly depending on how the backend RPC node is sourced. Three realistic setups:

### Scenario A — Chainstack Growth ($49/month)

Chainstack's Growth plan gives archive access across all major chains for a flat $49/month. No per-call pricing within the plan limits. This is the lowest-friction starting point.

| Item | Value |
|---|---|
| Monthly node cost | ~$49 |
| Break-even at $0.09/GRT | ~4.5M eth_call equivalents/month (~150K/day) |
| Break-even at $0.0246/GRT | ~16.6M eth_call equivalents/month (~550K/day) |

550K calls/day is achievable from a moderate consumer base or a handful of indexers dogfooding the network. This is the most realistic near-term entry point for a new provider.

### Scenario B — Chainstack Business / dedicated ($200/month)

Higher plan or a dedicated node add-on. Suitable if you're serving high volumes and hitting Growth plan rate limits, or need guaranteed throughput SLAs.

| Item | Value |
|---|---|
| Monthly node cost | ~$200 |
| Break-even at $0.09/GRT | ~18.5M eth_call equivalents/month (~620K/day) |
| Break-even at $0.0246/GRT | ~67.8M eth_call equivalents/month (~2.3M/day) |

2.3M calls/day is substantial — this tier only makes sense once external consumer traffic is well established.

### Scenario C — Self-hosted archive node (~$130/month)

Running your own Erigon / Reth / Nethermind instance on dedicated hardware. Typical hardware cost is €117–150/month (e.g. Hetzner AX102: Ryzen 9 7950X3D, 128 GB DDR5, 2×1.92 TB NVMe). No per-call costs once running.

| Item | Value |
|---|---|
| Monthly node cost | ~$130 (hardware only, at 1 EUR = 1.10 USD) |
| Marginal cost per request | $0 |
| Break-even at $0.09/GRT | ~12M eth_call equivalents/month (~400K/day) |
| Break-even at $0.0246/GRT | ~44M eth_call equivalents/month (~1.5M/day) |

**The key upside:** beyond break-even, every additional request is pure margin — no per-call cost eating into revenue. At high volume this is significantly more profitable than a managed provider. The catch is operational overhead (syncing, disk management, upgrades) and the disk footprint: Arbitrum One alone is ~3.3 TB and growing at ~100 GB/month.

If you are already running an archive node as a Graph Protocol subgraph indexer, **the marginal cost of joining Dispatch as a provider is essentially zero** — the node is already running and paid for by subgraph indexing revenue. In that case the break-even is the cost of the small Dispatch VM (~€5–10/month on Hetzner Cloud) plus the operator time to deploy, and any Dispatch query fees are nearly pure profit.

---

## Break-even Summary

| Setup | Monthly cost | Break-even calls/day at $0.09 GRT | Break-even calls/day at $0.0246 GRT |
|---|---|---|---|
| Chainstack Growth | $49 | ~150K | ~550K |
| Chainstack Business | $200 | ~620K | ~2.3M |
| Self-hosted (standalone) | ~$130 | ~400K | ~1.5M |
| Self-hosted (already running) | ~$10 | ~30K | ~110K |

**GRT price sensitivity** (10M eth_call equivalents/month):

| GRT price | Revenue | Covers $49 node | Covers $130 node | Covers $200 node |
|---|---|---|---|---|
| $0.0246 | ~$8.50 | 17% | 6.5% | 4.3% |
| $0.05 | ~$17.30 | 35% | 13% | 8.7% |
| $0.09 | ~$31.10 | 63% | 24% | 15.6% |
| $0.20 | ~$69.10 | 141% ✓ | 53% | 34.6% |
| $0.50 | ~$172.80 | ✓ | ✓ | 86% |

Query fees are denominated in GRT, so each dollar of GRT appreciation directly multiplies USD revenue per request. The Chainstack Growth tier becomes profitable on 10M/month at ~$0.15 GRT; self-hosted at ~$0.20 GRT; Business tier at ~$0.65 GRT.

---

## Current State (Lodestar, May 2026)

- **Active providers:** 1 (`https://rpc.cargopete.com`)
- **Dispatch query fee revenue:** ~$0 net — operator is the only consumer (self-consumption loop)
- **Self-consumption escrow:** 9,363 GRT deposited; ~63 GRT/day consumed; ~149 days runway
- **Net cost of self-consumption:** ~2% protocol/data-service fees on recycled GRT — negligible
- **Real profit from Dispatch:** starts when external consumers appear

The subgraph indexing rewards (~5,578 GRT/month) are the primary income source today. Dispatch is live, the payment loop is proven, and the platform is ready for external consumer traffic.
