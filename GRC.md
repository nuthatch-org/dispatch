# GRC-005: Dispatch — An Experimental JSON-RPC Data Service on Horizon

**Stage:** RFC (Request for Comment)
**GRC:** 005
**Authors:** @cargopete (Petko Pavlovski)

---

## Summary

This GRC introduces Dispatch — a community-built JSON-RPC data service running on The Graph's Horizon framework. The contract is deployed on Arbitrum One, the subgraph is live, and the first provider is serving real traffic with a fully working payment loop.

This is an independent community experiment, not an official Graph Foundation or Edge & Node initiative. The goal of this GRC is to share the design openly, get feedback on the core mechanisms, and explore whether the Graph community wants to develop this direction further.

---

## Background: why RPC?

Every dApp on Earth quietly depends on Alchemy or Infura. When your frontend calls `eth_getBalance`, that request almost certainly hits a centralised API run by a handful of companies. They can go down, rate-limit you, reprice overnight, or — in the extreme — be compelled to censor specific addresses.

The Graph has done a remarkable job decentralising subgraph data. But the most fundamental piece of Ethereum infrastructure — plain JSON-RPC — has stayed centralised. Pocket Network and Lava have tried to address this with their own token networks. The question this experiment asks is: can The Graph's existing infrastructure — Horizon staking, GraphTally payments, the indexer ecosystem — support a decentralised RPC market without a new token or new payment primitives?

The answer, as far as this experiment has demonstrated: yes.

---

## The core idea

The Graph's Horizon framework makes it straightforward to define new data services. A data service is a contract that implements a standard interface (`IDataService`): providers register their stake, offer a service, and get paid per unit of work via GraphTally micropayments. The SubgraphService uses this for subgraph indexing. Dispatch uses the same infrastructure for JSON-RPC.

The insight is that Horizon already has all the pieces: `HorizonStaking` manages stake and slashing authority. `GraphTallyCollector` handles per-request micropayments. `PaymentsEscrow` holds consumer funds. What was missing was one layer — a `DataService` implementation that speaks JSON-RPC rather than subgraph POIs.

`RPCDataService` is that layer. From a staking, payment, and GRT flow perspective, it is nearly identical to SubgraphService.

---

## How it works

### Providers

A provider is a Graph indexer who runs an Ethereum node (Geth, Erigon, Reth, or similar) alongside the `dispatch-service` binary. They stake at least 10,000 GRT via a Horizon provision, register with `RPCDataService`, then call `startService` for each chain and capability tier they want to serve.

Capability tiers reflect the actual infrastructure differences between node types:

- **Standard** — any full node: recent chain state (~128 blocks), all standard RPC methods
- **Archive** — archive node: full historical state at any block number
- **Debug/Trace** — debug APIs enabled: `debug_traceTransaction`, `trace_block`, etc.

A provider running one archive node can register for both Standard and Archive on the same chain. Their stake is shared across all chains they serve — there is no per-chain stake splitting.

### Consumers

Consumers interact through two paths:

**Via the gateway** (easy path): A single HTTP endpoint. The gateway handles provider discovery, quality scoring, payment signing, and routing. The consumer needs no configuration beyond the endpoint URL and a GRT deposit in escrow. This path trusts the gateway to route honestly.

**Via the consumer SDK** (trustless path): The SDK discovers providers from the subgraph, signs TAP receipts with the consumer's own key, and dispatches requests directly. No third party can forge payments on the consumer's behalf. The consumer pays per request directly to the provider they choose.

### Payments

Payments use GraphTally (TAP v2) — the same micropayment primitive that subgraph queries use today.

The flow is:
1. Consumers deposit GRT into `PaymentsEscrow` before making requests.
2. For each request, the gateway signs a small EIP-712 receipt — essentially a cheque for a specific amount of GRT made out to a specific provider. This receipt travels alongside the JSON-RPC request.
3. The provider validates the receipt signature before serving the request. Invalid or forged receipts are rejected.
4. Receipts accumulate off-chain. Periodically (every 60 seconds), they are aggregated into a single signed voucher (a RAV — Receipt Aggregate Voucher) with a cumulative total value.
5. The provider submits the RAV on-chain via `RPCDataService.collect()`. GRT flows from the consumer's escrow to the provider's wallet.

The cumulative RAV value is monotonically increasing — it always represents the running total, never resets. This means the provider can batch partial receipts safely, and there is no window where previously-claimed GRT can be disputed.

### Stake-backed accountability

When a provider collects fees, `RPCDataService` locks five times the collected amount in their stake for the duration of the thawing period (minimum 14 days). This is the same 5:1 stake-to-fees ratio as SubgraphService.

The mechanism: a provider with 10,000 GRT can collect at most 2,000 GRT before their entire provision is tied up in stake claims. This creates a meaningful cost-of-fraud: a provider who serves bad responses and gets caught loses far more in locked stake than they gain from the fraudulent fees.

---

## The verification problem

This is the genuinely hard design question, and it is worth being direct about it.

For subgraphs, The Graph uses Proof of Indexing — a deterministic hash over the indexed state that lets the network detect disagreements between providers. JSON-RPC has no equivalent. Most RPC responses cannot be efficiently proved correct on-chain:

- `eth_call` results require full EVM re-execution to verify
- `eth_blockNumber` and similar methods depend on which block a node has synced to — two honest nodes may honestly disagree
- `eth_estimateGas` is explicitly non-deterministic by design

Dispatch currently handles this with two mechanisms:

**Attestations.** Every response carries a provider-signed cryptographic commitment to the `(chain, method, params, result)` tuple. This creates a tamper-evident audit trail — a consumer can prove that a provider *claimed* a specific response. Providers that omit or forge attestations are penalised in quality scoring and receive less traffic.

**Quorum.** For deterministic methods (`eth_call`, `eth_getLogs`, `eth_getBalance`, etc.), the gateway queries multiple providers and takes the majority result. A provider in the minority is penalised. This makes systematic lying expensive — you need to control a majority of the selected providers.

What these two mechanisms do *not* give you is cryptographic proof of correctness. They make dishonesty unprofitable and detectable, but cannot eliminate it. **Dispatch is currently economically secure, not cryptographically secure.** This is the same position as Pocket Network and Lava today.

A stronger model exists for a subset of methods: EIP-1186 Merkle-Patricia Trie proofs. For methods like `eth_getBalance`, `eth_getStorageAt`, and `eth_getCode`, responses can be verified against Ethereum's state root without EVM re-execution. This would enable on-chain fraud proofs and genuine slashing. It requires a trusted block header oracle and an on-chain MPT verifier — both exist as building blocks but have not been integrated. The `slash()` function on `RPCDataService` currently reverts unconditionally. Slashing is a clear future direction, not a current capability.

---

## What is and isn't implemented

**Working:**
- `RPCDataService` contract on Arbitrum One
- `dispatch-service` and `dispatch-gateway` Rust binaries
- Full TAP payment loop: receipts → RAV aggregation → on-chain `collect()` → GRT to provider
- Dynamic provider discovery via subgraph
- Quality scoring, geographic routing, quorum dispatch, rate limiting
- Consumer SDK and indexer agent npm packages

**Not implemented:**
- **Slashing** — `slash()` reverts. There is no mechanism to penalise a provider on-chain for serving wrong responses.
- **Permissionless chain registration** — chains are added by the contract owner. A bond-based permissionless model is a natural future step but is not built.
- **GRT issuance rewards** — providers earn query fees only. GRT issuance would require Graph governance approval.

Implementation status in brief:

| Component | Status |
|---|---|
| `RPCDataService` contract | ✅ Live on Arbitrum One |
| Subgraph | ✅ Live on The Graph Studio |
| npm packages | ✅ Published (`@lodestar-dispatch/consumer-sdk`, `@lodestar-dispatch/indexer-agent`) |
| Active providers | ✅ 1 — `https://rpc.cargopete.com` (Arbitrum One, Standard + Archive) |
| Full payment loop | ✅ Working end-to-end — receipts → RAVs every 60s → `collect()` every hour → GRT to provider |
| Dynamic provider discovery | ✅ Working — gateway polls subgraph every 60s |
| Slashing | ❌ Not implemented — `slash()` reverts |

---

## What this means for the Graph

Nothing in the existing network changes. SubgraphService, HorizonStaking, GraphTallyCollector, and PaymentsEscrow are all reused unchanged. Existing indexers and delegators are unaffected.

Existing Graph indexers are the natural first movers. If you are already staking GRT and running an Ethereum node, the barrier to becoming a Dispatch provider is one configuration file and a registration transaction.

The payment primitives are already battle-tested. TAP receipts, RAV aggregation, and GraphTallyCollector are the same primitives the Subgraph network uses in production today.

The most significant open question is governance, not engineering. The contract parameters — minimum stake, thawing period, chain allowlist, CU pricing — are currently owner-controlled. If this grows, the community needs to decide how governance of those parameters should work.

---

## Open questions

These are the design questions worth the community's attention:

**On verification:** Is the current economic model (attestations + quorum) sufficient to bootstrap a real market? Or is the absence of cryptographic guarantees a fundamental barrier to adoption? What would it take to add EIP-1186 proof verification and on-chain slashing?

**On pricing:** The current default is roughly $40 per million requests. What is the right pricing model across method types and capability tiers? Should archive queries carry a different base price than standard queries at the protocol level?

**On the gateway's trust role:** The managed gateway path requires trusting a centralised operator for routing. Is this an acceptable tradeoff for ease of use, or should the trustless SDK path be the primary user interface? How should the network evolve so the gateway becomes less necessary?

**On chain addition:** Owner-controlled chain allowlists are safe but not permissionless. A bond-based model (where anyone can add a chain by locking GRT) would be more aligned with The Graph's values. What is the right threshold, and who governs it?

**On minimum stake:** 10,000 GRT minimum provision with a 5:1 stake-to-fees ratio means a provider can collect at most 2,000 GRT before their full provision is locked. Is this enough economic skin-in-the-game given that slashing doesn't exist yet?

---

## Deployed addresses (Arbitrum One, chain ID 42161)

| Contract | Address |
|---|---|
| HorizonStaking | `0x00669A4CF01450B64E8A2A20E9b1FCB71E61eF03` |
| GraphPayments | `0x7Aae8ae011927BC36Cb4d0d3e81f2E6E30daE06D` |
| PaymentsEscrow | `0xf6Fcc27aAf1fcD8B254498c9794451d82afC673E` |
| GraphTallyCollector | `0x8f69F5C07477Ac46FBc491B1E6D91E2bb0111A9e` |
| RPCDataService | `0x7101d5c1a5c89c3647f5118da118e56c023ba0b9` |

Subgraph: `https://api.studio.thegraph.com/query/1747796/rpc-network/v0.2.0`

GitHub: `https://github.com/cargopete/dispatch`
