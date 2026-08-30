# Deployed Addresses

---

## Dispatch contracts

| Contract | Network | Address |
|---|---|---|
| RPCDataService | Arbitrum One (42161) | `0x7101d5c1a5c89c3647f5118da118e56c023ba0b9` |

Subgraph: `https://api.studio.thegraph.com/query/1747796/rpc-network/v0.3.0`

---

## Horizon contracts — Arbitrum One (42161)

| Contract | Address |
|---|---|
| HorizonStaking | `0x00669A4CF01450B64E8A2A20E9b1FCB71E61eF03` |
| GraphPayments | `0x7Aae8ae011927BC36Cb4d0d3e81f2E6E30daE06D` |
| PaymentsEscrow | `0xf6Fcc27aAf1fcD8B254498c9794451d82afC673E` |
| GraphTallyCollector | `0x8f69F5C07477Ac46FBc491B1E6D91E2bb0111A9e` |
| SubgraphService | `0xb2Bb92d0DE618878E438b55D5846cfecD9301105` |
| DisputeManager | `0x0Ab2B043138352413Bb02e67E626a70320E3BD46` |
| RewardsManager | `0x971B9d3d0Ae3ECa029CAB5eA1fB0F72c85e6a525` |
| GRT Token | `0x9623063377AD1B27544C965cCd7342f7EA7e88C7` |

---

## Horizon contracts — Arbitrum Sepolia (421614, testnet)

| Contract | Address |
|---|---|
| HorizonStaking | `0x865365C425f3A593Ffe698D9c4E6707D14d51e08` |
| GraphTallyCollector | `0x382863e7B662027117449bd2c49285582bbBd21B` |
| PaymentsEscrow | `0x1e4dC4f9F95E102635D8F7ED71c5CdbFa20e2d02` |
| SubgraphService | `0xc24A3dAC5d06d771f657A48B20cE1a671B78f26b` |

---

## Active providers

> Read from Arbitrum One 2026-08-28 by scanning `ProviderRegistered` / `ProviderDeregistered` and
> `ServiceStarted` on the proxy, then confirming with `isRegistered()` and
> `activeRegistrationCount()`. Not from memory.

**Two independent providers, both registered and both serving.**

| Address | Endpoint | Active registrations | Chains / tiers |
|---|---|---|---|
| `0xb43b2cccceada5292732a8c58ae134adefce09bb` | `https://rpc.cargopete.com` | 5 | 42161 Standard + Archive; 1, 56, 8453 Debug |
| `0x575267eed09c338fae5716a486a7b58a5749a292` | — | 2 | 42161 Standard; 8453 Debug |

`0x575267ee…` deregistered at block 456,950,409 and re-registered ten blocks later; it is
registered now.

**A note on quorum.** Deterministic methods are dispatched to three providers and the majority
result wins. Arbitrum One Standard — the busiest lane — currently has **two** providers, so a
three-way quorum cannot form there and the fallback path is what actually runs. Worth keeping in
view when reading any claim about quorum-verified responses.
