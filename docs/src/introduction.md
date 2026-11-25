# Dispatch

> **Community project — not affiliated with or endorsed by The Graph Foundation or Edge & Node.**
> This is an independent exploration of what a JSON-RPC data service on Horizon might look like.

Dispatch is a decentralised JSON-RPC service built on [The Graph Protocol's Horizon framework](https://thegraph.com/docs/en/horizon/). Indexers stake GRT, register to serve specific chains, and get paid per request via [GraphTally](https://github.com/graphprotocol/graph-improvement-proposals/blob/main/gips/0054-graphtally.md) (TAP v2) micropayments.

Inspired by the [Q3 2026 "Experimental JSON-RPC Data Service"](https://thegraph.com/blog/graph-protocol-2026-technical-roadmap/) direction in The Graph's 2026 Technical Roadmap — but this codebase is an independent community effort, not an official implementation.

---

## What it does

An application calls `eth_getBalance`. Instead of hitting a centralised RPC provider, the request routes to a staked indexer in the Dispatch network. The indexer persists a TAP receipt and returns the data. GRT flows on-chain automatically via GraphTally.

That's the loop.

---

## Network status

| Component | Status |
|---|---|
| `RPCDataService` contract | ✅ Live on Arbitrum One |
| Subgraph | ✅ Live on The Graph Studio |
| npm packages | ✅ Published (`@lodestar-dispatch/consumer-sdk`, `@lodestar-dispatch/indexer-agent`) |
| Active providers | ⚠️ **1 of 3 answering** — `https://dispatch-rpc.cargopete.com` (Optimism, all tiers) |
| Receipt signing & validation | ✅ Working — every request carries a signed EIP-712 TAP receipt |
| Response attestation | ✅ Working — provider signs every response; gateway verifies before forwarding |
| Quorum consensus | ✅ Working — deterministic methods cross-checked across 3 providers |
| Receipt persistence | ✅ Working — stored in `tap_receipts` table |
| RAV aggregation (off-chain) | ✅ Working — gateway batches receipts into signed RAVs every 60s |
| On-chain `collect()` | ✅ Working — GRT settles on-chain automatically every hour |
| Provider on-chain registration | ✅ Confirmed on-chain |
| Multi-provider discovery | ✅ Working — subgraph-driven dynamic registry |
| Local demo | ✅ Working — full payment loop on Anvil |

The full payment loop is working end-to-end. Requests generate TAP receipts, the service aggregates them into RAVs every 60s, and calls `RPCDataService.collect()` every hour — pulling GRT from the consumer's escrow to the provider automatically.

---

## Relation to The Graph

Dispatch reuses most of the Horizon stack rather than reinventing it:

| Component | Status |
|---|---|
| HorizonStaking / GraphPayments / PaymentsEscrow | ✅ Reused as-is |
| GraphTallyCollector (TAP v2) | ✅ Reused as-is |
| `indexer-tap-agent` | ❌ Not used — TAP aggregation and on-chain collection are built into `dispatch-service` |
| `indexer-service-rs` TAP middleware | ✅ Logic ported to `dispatch-service` |
| Graph Node | ❌ Not needed — standard Ethereum clients only |
| POI / SubgraphService dispute system | ❌ Not applicable — JSON-RPC responses have no on-chain verifier |
