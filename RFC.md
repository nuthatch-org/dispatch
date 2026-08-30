# RFC: Dispatch — A JSON-RPC Data Service on The Graph Horizon

**Status:** Implemented (see codebase)
**Target:** Q3 2026 experimental window
**Authors:** TBD
**Based on:** GIP-0066 (Horizon), GIP-0054 (GraphTally), GIP-0042 (World of Data Services)

---

## 1. Summary

Dispatch is a decentralised JSON-RPC (Ethereum-compatible RPC) data service built on The Graph Protocol's Horizon framework. Providers stake GRT, register to serve specific chains, and earn micropayments per request. Consumers interact through a gateway that handles provider selection, payment, and quality scoring — or directly via the consumer SDK.

The central challenge Dispatch shares with every decentralised RPC protocol is that JSON-RPC responses have no canonical on-chain truth. You cannot prove on-chain that `eth_call` returned the right value. This RFC describes how Dispatch makes dishonesty unprofitable and detectable despite that limitation, and is explicit where economic incentives end and cryptographic guarantees would need to begin.

This is an independent community implementation of the Horizon data service pattern, fully compatible with Horizon's deployed contracts on Arbitrum One, but not affiliated with The Graph Foundation.

---

## 2. Horizon integration

### 2.1 Provisions

A provider's GRT stake is allocated to a data service via a *provision* — a record in HorizonStaking that binds a `(serviceProvider, dataService)` pair. The data service contract is the exclusive slashing authority over that provision. This design allows multiple independent data services to coexist with their own economic rules, rather than sharing a single global staking model.

For Dispatch, a provider's single provision covers all chains and tiers they serve. Stake is not split per chain.

### 2.2 GraphTally payments (TAP v2)

GraphTally moves payment trust off-chain. The flow is:

- Consumers pre-fund an escrow in `PaymentsEscrow`.
- Per request, the gateway signs a TAP receipt — an EIP-712 message containing the data service address, the provider's address, a random nonce, a GRT amount, and a nanosecond timestamp. The receipt is sent to the provider alongside the request.
- Providers accumulate receipts off-chain, then aggregate them into a Receipt Aggregate Voucher (RAV): a single signature over the sum of all receipt values. The `value_aggregate` field is monotonically increasing — it represents the cumulative total, not a delta.
- The provider submits the RAV on-chain via `RPCDataService.collect()`. The chain pulls the delta (new total minus previously redeemed) from escrow and pays out.

The monotonic `value_aggregate` invariant means: (1) the provider can only ever claim *more*, never less; (2) partial aggregation is safe — include all receipts, not just the latest batch; (3) there is no receipt-level on-chain settlement, only periodic RAV settlement.

### 2.3 DataService framework

`RPCDataService` inherits three Horizon base contracts: `DataService` (provision utilities and GraphDirectory), `DataServiceFees` (stake-backed fee locking), and `DataServicePausable` (emergency stop). The contract does not deploy these — they are composed in as library contracts. Any developer can do the same permissionlessly; GRT issuance rewards require Graph governance approval separately.

---

## 3. Unit of service

Unlike subgraphs, RPC is generic — any provider serving Ethereum mainnet returns the same data. The unit of service is a `(chainId, capabilityTier)` pair:

| Tier | Methods | Infrastructure required |
|---|---|---|
| Standard | All standard methods, last ~128 blocks | Full node |
| Archive | Full historical state at any block | Archive node |
| Debug/Trace | `debug_*` and `trace_*` methods | Full/archive node with debug APIs |

A provider activates service per `(chainId, tier)` independently. One archive node can advertise both Standard and Archive on the same chain from a single registration. WebSocket (`eth_subscribe`) is not a separate tier — it is a transport, available from any Standard provider.

The chain allowlist is governance-controlled. The default minimum provision is **10,000 GRT** per chain, adjustable per chain by the contract owner.

---

## 4. Provider lifecycle

```
register
  → startService(chainId, tier) × N
    → serve requests + collect fees
  → stopService(chainId, tier) × N
→ deregister
```

**Registration** establishes global identity. The provider supplies their endpoint URL, a geographic region (geohash), and optionally a `paymentsDestination` — a separate address to receive collected GRT. This decouples the operator signing key (used for attestations and on-chain transactions) from the payment wallet, allowing cold storage for GRT while a hot key handles operations.

**Service activation** (`startService`) records the `(chainId, tier)` pair on-chain and emits an event that the subgraph indexes. The contract checks that the provision meets the per-chain minimum at the moment of activation.

**Fee collection** (`collect`) accepts a signed RAV, delegates to `GraphTallyCollector` for EIP-712 signature and escrow verification, routes collected GRT to `paymentsDestination`, then locks `fees × 5` of the provider's stake for the thawing period (minimum 14 days). The **5:1 stake-to-fees ratio** means a provider must have five GRT at risk for every GRT they collect. This is the primary economic deterrent against fraud — a provider with 10,000 GRT can collect at most 2,000 GRT before their entire provision is tied up in stake claims.

**Deregistration** requires all chain services to be stopped first, preventing a provider from escaping locked stake claims by deregistering during the dispute window.

---

## 5. Payment flow in practice

```
Consumer request
    ↓
dispatch-gateway
    signs TAP receipt (EIP-712, random nonce, CU-weighted value)
    selects provider via QoS scoring
    ↓
dispatch-service
    validates TAP receipt (signature, sender authorisation, staleness)
    forwards to Ethereum node
    persists receipt to PostgreSQL
    signs response attestation
    ↓
Periodically (every 60s):
    dispatch-service → gateway /rav/aggregate → receives signed RAV
Periodically (every hour):
    dispatch-service → RPCDataService.collect() → GRT to provider
```

**Receipt validation** at the provider: the EIP-712 signature is recovered; the signer must be in the `authorized_senders` list (the gateway's operator key); the timestamp must be within the staleness window (default 30 seconds). An invalid receipt rejects the request — the provider works for free otherwise.

**EIP-712 domain** for all TAP receipts and RAVs: name `"GraphTallyCollector"`, chain ID 42161 (Arbitrum One), verifying contract `0x8f69F5C07477Ac46FBc491B1E6D91E2bb0111A9e`. This domain must match exactly on both the gateway (signing) and provider (verifying) sides.

**The consumer pays upfront.** GRT is deposited into `PaymentsEscrow` before requests are made. If escrow runs dry, RAV collection fails. This is an inherent bootstrapping consideration for new consumers.

---

## 6. Verification: the hard problem

For subgraphs, correctness is checkable via Proof of Indexing — a deterministic hash of the indexed state. RPC has no equivalent. Most JSON-RPC methods are either:

- **Non-deterministic** — gas estimates, mempool state, latest-block queries where two honest providers may honestly disagree
- **State-dependent** — responses depend on chain head, which moves continuously
- **Expensive to re-execute** — `eth_call` requires EVM re-execution; no efficient on-chain verifier exists

Dispatch uses two complementary mechanisms that provide economic deterrence, not cryptographic guarantees:

### 6.1 Attestations

Every response from `dispatch-service` carries a signed header (`x-drpc-attestation`). The provider signs:

```
keccak256(
    chain_id (8 bytes, big-endian)
    || method (UTF-8 bytes)
    || keccak256(params JSON)
    || keccak256(result JSON)
)
```

with its operator key. This creates a tamper-evident log: a consumer or gateway can prove that a provider *claimed* a specific response to a specific request.

The gateway verifies every attestation before forwarding the response. Providers that forge, omit, or produce inconsistent attestations are penalised in QoS scoring and receive less traffic over time.

**What this does not give you:** proof that the response was correct. Without a trusted state root and on-chain MPT verifier, an attestation cannot be used to slash a provider.

### 6.2 Quorum

For deterministic methods (`eth_call`, `eth_getLogs`, `eth_getBalance`, `eth_getCode`, `eth_getTransactionCount`, `eth_getStorageAt`, `eth_getBlockByHash`, `eth_getTransactionByHash`, `eth_getTransactionReceipt`), the gateway dispatches to multiple providers concurrently and takes the majority result. Minority responses trigger QoS penalties and a logged warning.

Quorum makes systematic lying expensive: to consistently return a false result, an attacker must control a majority of the providers that the gateway selects for a given request. The default quorum size is 3.

**What this does not give you:** certainty against a compromised gateway or a large-scale sybil attack on the provider pool.

### 6.3 What is not implemented

`slash()` on `RPCDataService` reverts unconditionally. The theoretically stronger model — EIP-1186 Merkle-Patricia Trie proof verification for a subset of methods — would enable on-chain fraud proofs, but requires:

1. A trusted block header oracle posting state roots on-chain
2. An on-chain MPT verifier (Solidity implementations exist)
3. A challenger mechanism with slashing consequences

The methods amenable to MPT proof verification are: `eth_getBalance`, `eth_getStorageAt`, `eth_getCode`, `eth_getTransactionCount` (from account trie), and `eth_getBlockByHash` (from header hash). This covers the highest-value read methods. `eth_call` requires EVM re-execution and remains out of reach for efficient on-chain verification.

Dispatch is currently *economically secure* (lying costs stake and traffic) but not *cryptographically secure* (no on-chain verification of response correctness). This is the same position as Pocket Network and Lava Network today.

---

## 7. Gateway and network topology

**Two consumer paths:**

**Via dispatch-gateway** (managed path): The gateway owns the TAP signing key, selects providers, issues payments, and enforces quorum. The consumer trusts the gateway to route honestly and not collude with dishonest providers. This is the easy path — one HTTP endpoint, no configuration.

**Via consumer-sdk** (trustless path): The SDK discovers providers from the subgraph, holds the consumer's own signing key, and issues TAP receipts directly. No third party can forge receipts on the consumer's behalf. Response verification is still attestation + quorum (same limitations), but the payment leg is fully decentralised.

**Provider discovery:** `RPCDataService` emits events on registration, `startService`, and `stopService`. The RPC network subgraph indexes these. The gateway polls the subgraph every 60 seconds to rebuild its provider registry. Between polls, providers are probed with synthetic `eth_blockNumber` calls every 10 seconds for liveness and freshness tracking.

**QoS scoring:** composite score per provider:
- 35% latency (EMA of probe response times; 0ms = 1.0, 500ms = 0.0)
- 35% availability (probe success rate over all history)
- 30% freshness (exponential decay by blocks behind chain head)

New providers start with an optimistic score of 1.0. A geographic bonus rewards same-region providers until latency data accumulates. Traffic is distributed via weighted-random selection among top-k candidates — not winner-take-all, so new providers can enter the routing pool.

**Concurrent dispatch:** requests go to up to 3 providers simultaneously. The first valid response wins (for non-quorum methods). For quorum methods, all responses are compared. This costs 3× the GRT per request but reduces tail latency and adds a correctness check.

---

## 8. Pricing

The gateway prices by compute unit (CU) weight multiplied by `base_price_per_cu`. The hardcoded default is `4_000_000_000_000` GRT wei per CU. Since GRT has 18 decimal places, this is **4×10⁻⁶ GRT per CU**.

The TAP receipt `value` for a single request is `cu_weight × base_price_per_cu` GRT wei. Because the gateway dispatches to **3 providers concurrently** for all methods, the effective consumer cost is **3× the per-provider receipt** — all three receive and will eventually claim their receipts.

**CU weights** (source: `dispatch-gateway/src/routes/rpc.rs`, `cu_weight_for`):

| Method | CU |
|---|---|
| `eth_chainId`, `eth_blockNumber`, `net_version` | 1 |
| `eth_getBalance`, `eth_getCode`, `eth_getTransactionCount`, `eth_getStorageAt`, `eth_sendRawTransaction`, block queries | 5 |
| `eth_call`, `eth_estimateGas`, `eth_getTransactionReceipt`, `eth_getTransactionByHash` | 10 |
| `eth_getLogs` | 20 |
| Unknown / unrecognised method | 10 |

**Effective cost per million calls (×3 concurrent dispatch) vs Alchemy:**

| Method | CU | $0.05/GRT | $0.09/GRT | $0.15/GRT | Alchemy |
|---|---|---|---|---|---|
| `eth_blockNumber` | 1 | $0.60/M | $1.08/M | $1.80/M | $4.50/M |
| `eth_getBalance` | 5 | $3.00/M | $5.40/M | $9.00/M | $4.50/M |
| `eth_call` | 10 | $6.00/M | $10.80/M | $18.00/M | $11.70/M |
| `eth_getLogs` | 20 | $12.00/M | $21.60/M | $36.00/M | $33.75/M |

At ~$0.09/GRT, Dispatch is cost-competitive with Alchemy: 8% cheaper on `eth_call`, 36% cheaper on `eth_getLogs`. Break-even on `eth_call` is at **~$0.10/GRT** — above that price, centralised providers become cheaper. These numbers are asserted in `dispatch-gateway/src/config.rs` (`pricing_math` test).

CU weight is baked into the TAP receipt's `value` field. The provider receives payment per request regardless of outcome; the gateway adjusts how much it pays based on method cost.

There is no GRT issuance for this data service. Revenue is query fees only.

---

## 9. Key design decisions

**Single provision covers all chains.** One GRT provision backs all chains a provider serves. This minimises capital requirements but means chain-specific stake isolation is not possible. A provider being slashed (if slashing existed) for misbehaviour on one chain would have their entire multi-chain provision at risk.

**TAP aggregation built into dispatch-service.** Rather than running a separate `indexer-tap-agent` binary, receipt aggregation and on-chain collection are embedded in `dispatch-service`. This simplifies deployment for solo operators. The tradeoff: it's non-standard relative to the broader Graph indexer ecosystem, and would need to be refactored for operators already running `indexer-tap-agent`.

**paymentsDestination decoupling.** The payment recipient address is stored separately from the operator address on-chain. This allows cold-storage GRT wallets and separates operational key risk from fund custody.

**Governance-controlled chain allowlist.** New chains require an owner transaction. This prevents griefing via bogus chain IDs and allows the operator to vet that backends actually exist. The alternative — permissionless chain addition with a bond requirement — is the natural next step.

**Quorum methods are hard-coded.** The set of deterministic methods that trigger quorum dispatch is defined in the gateway binary, not in the contract or protocol. This means changing the quorum set requires a gateway update, not a governance action. Whether that's appropriate depends on how formal the protocol should be.

---

## 10. Known gaps

| Gap | Why it exists | What would close it |
|---|---|---|
| No slashing | No on-chain truth for RPC responses | EIP-1186 MPT proofs + block header oracle |
| No permissionless chain addition | Owner-only allowlist for operational safety | Bond-based permissionless addition |
| No GRT issuance rewards | Not Graph-governance-approved | Graph governance proposal |
| Gateway is a trust boundary | Architectural simplicity; SDK is the trustless alternative | TEE execution, P2P routing |
| No consumer-side escrow visibility | Consumers can't easily see remaining escrow balance | Read-through to PaymentsEscrow |
| Quorum set size is fixed at 3 | Hard-coded default; not chain/method-tunable | Configurable per method or tier |
| No economic consequences for quorum minority | QoS penalty only; no stake-at-risk | Slashing infrastructure |

---

## 11. Open questions

| Question | Notes |
|---|---|
| Is 10,000 GRT sufficient skin in the game? | At 5:1 ratio, a provider collects at most 2,000 GRT before full provision is locked. Adequate for Phase 1; may constrain high-volume providers. |
| Can a gateway front-run providers economically? | The gateway selects which provider gets traffic. A compromised gateway can steer requests to a sybil, collect payments on behalf of that sybil, and split earnings. This is not a Dispatch-specific problem — it's inherent to any managed gateway. |
| Should quorum size scale with chain head proximity? | For freshness-sensitive methods, a quorum of 3 providers all 5 blocks behind head may give worse results than a single up-to-date provider. |
| What happens when a provider's escrow is exhausted mid-RAV-window? | The provider served requests but the RAV cannot be fully redeemed. Partial redemption is supported by TAP. |
| How should per-chain minimum provisions scale with chain fee revenue? | High-fee chains (Ethereum mainnet) arguably warrant higher minimums. Currently uniform. |

---

## 12. Deployed addresses (Arbitrum One, chain ID 42161)

| Contract | Address |
|---|---|
| HorizonStaking | `0x00669A4CF01450B64E8A2A20E9b1FCB71E61eF03` |
| GraphPayments | `0x7Aae8ae011927BC36Cb4d0d3e81f2E6E30daE06D` |
| PaymentsEscrow | `0xf6Fcc27aAf1fcD8B254498c9794451d82afC673E` |
| GraphTallyCollector | `0x8f69F5C07477Ac46FBc491B1E6D91E2bb0111A9e` |
| RPCDataService | `0x7101d5c1a5c89c3647f5118da118e56c023ba0b9` |

Subgraph: `https://api.studio.thegraph.com/query/1747796/rpc-network/v0.2.0`

---

## 13. Competitive landscape

| Protocol | Verification model | Token | Notes |
|---|---|---|---|
| Pocket Network | Proof-of-Relay + quorum | POKT | CU-based pricing; no Merkle proofs |
| Lava Network | On-chain QoS scoring + cross-reference | LAVA | Spec-based chain definitions |
| Infura DIN | Watcher nodes + EigenLayer stake | — | Progressively decentralising |
| **Dispatch** | **Attestations + quorum** | **GRT** | **Horizon-native; shared stake with subgraphs** |

The Graph's unique position: RPC providers, subgraph indexers, and Substreams operators can share the same GRT stake, network identity, and payment infrastructure. One stake, one payment system, one network.

---

## 14. Source references

| Repo | Contents |
|---|---|
| `github.com/graphprotocol/contracts` | Horizon contracts (`packages/horizon/`), SubgraphService |
| `github.com/graphprotocol/substreams-data-service` | Second Horizon data service; reference for `paymentsDestination` pattern and integration test strategy |
| `github.com/graphprotocol/indexer-rs` | `indexer-service-rs`, `indexer-tap-agent` |
| `github.com/graphprotocol/indexer` | `indexer-agent` (TypeScript) |
