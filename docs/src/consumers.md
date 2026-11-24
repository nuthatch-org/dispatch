# Using the Network

Three ways to consume the Dispatch network: hit the gateway directly, run the local `dispatch-proxy`, or use the consumer SDK (trustless, signs receipts locally).

**Who manages on-chain GRT?**
- **Gateway** — the gateway is the on-chain payer. It maintains its own GRT escrow with each provider. You do not need to deposit GRT yourself; you need an API key from the gateway operator.
- **dispatch-proxy / Consumer SDK** — you are the on-chain payer. You must deposit GRT into `PaymentsEscrow` before making requests (see [Funding the escrow](#funding-the-escrow)).

---

## Via the Gateway

The gateway handles provider selection and TAP receipt signing. Every request needs an API key, sent as `X-Api-Key`, as `Authorization: Bearer`, or in the path as `/rpc/{chain_id}/{key}` for clients such as graph-node that cannot set headers. The gateway manages its own on-chain GRT flow; you do not need to fund an escrow account to use the gateway.

**Live gateway:** `https://gateway.lodestar-dashboard.com`

```bash
# curl
curl -s -X POST https://gateway.lodestar-dashboard.com/rpc/42161 \
  -H "Content-Type: application/json" \
  -H "X-Api-Key: dsp_YOUR_KEY" \
  -d '{"jsonrpc":"2.0","method":"eth_blockNumber","params":[],"id":1}'

# Check the attestation header
curl -si -X POST https://gateway.lodestar-dashboard.com/rpc/42161 \
  -H "Content-Type: application/json" \
  -H "X-Api-Key: dsp_YOUR_KEY" \
  -d '{"jsonrpc":"2.0","method":"eth_blockNumber","params":[],"id":1}' \
  | grep -E "x-drpc-attestation|result"
```

With ethers.js or viem:

```typescript
import { createPublicClient, http } from "viem";
import { arbitrum } from "viem/chains";

const client = createPublicClient({
  chain: arbitrum,
  transport: http("https://gateway.lodestar-dashboard.com/rpc/42161", {
    fetchOptions: {
      headers: { "X-Api-Key": "dsp_YOUR_KEY" },
    },
  }),
});

const block = await client.getBlockNumber();
```

A missing or unknown key returns `401 Unauthorized`.

**Routes:**
```
POST /rpc/{chain_id}     # chain ID in path
POST /rpc                # chain ID via X-Chain-Id header
```

Currently live: **Arbitrum One (42161)** — Standard and Archive tiers.

---

## dispatch-proxy (drop-in local server)

The easiest way to point any existing app at the Dispatch network without changing application code. Starts a standard JSON-RPC HTTP server on localhost; MetaMask, Viem, Ethers.js, and curl all work against it without modification.

```bash
cd proxy
npm install
npm start
```

On first run the proxy auto-generates a consumer keypair, saves it to `./consumer.key`, and tells you where to fund escrow. No key needed upfront.

```
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
dispatch-proxy v0.1.0
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
Chain:     Ethereum Mainnet (1)
Listening: http://localhost:8545
Consumer:  0xABCD...1234
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
⚠  New consumer key generated → ./consumer.key
Fund escrow at:  https://lodestar-dashboard.com/dispatch
Consumer address: 0xABCD...1234
Or use an existing funded key: DISPATCH_SIGNER_KEY=0x...
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
Add to MetaMask  →  Settings → Networks → Add a network
  RPC URL:  http://localhost:8545
  Chain ID: 1
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

[12:34:56] ✓ eth_blockNumber      42ms  0.000004 GRT   total: 0.000004 GRT
[12:34:57] ✓ eth_getBalance       38ms  0.000008 GRT   total: 0.000012 GRT
```

**Configuration:**

| Variable | Default | Description |
|---|---|---|
| `DISPATCH_SIGNER_KEY` | *(auto-generated)* | Consumer private key. If unset, loaded from `./consumer.key` or generated fresh |
| `DISPATCH_CHAIN_ID` | `1` | Chain to proxy (1 = Ethereum, 42161 = Arbitrum One, etc.) |
| `DISPATCH_PORT` | `8545` | Local port to listen on |
| `DISPATCH_BASE_PRICE_PER_CU` | `4000000000000` | GRT wei per compute unit |

The proxy handles provider discovery, TAP receipt signing, QoS-scored provider selection, CORS, and JSON-RPC batch requests. On exit (Ctrl+C) it prints a session summary of total requests and GRT spent.

Unlike the gateway, the proxy runs locally and signs receipts with your own key — you are the on-chain payer and pay providers directly from your own escrow. See [Funding the escrow](#funding-the-escrow) below.

---

## Consumer SDK

For trustless access — signs receipts locally and talks directly to providers, no gateway in the loop.

```bash
npm install @lodestar-dispatch/consumer-sdk
```

```typescript
import { DISPATCHClient } from "@lodestar-dispatch/consumer-sdk";

const client = new DISPATCHClient({
  chainId: 42161,                                               // Arbitrum One (only live chain)
  dataServiceAddress: "0x7101d5c1a5c89c3647f5118da118e56c023ba0b9",
  graphTallyCollector: "0x8f69F5C07477Ac46FBc491B1E6D91E2bb0111A9e",
  subgraphUrl: "https://api.studio.thegraph.com/query/1747796/rpc-network/v0.3.0",
  signerPrivateKey: process.env.CONSUMER_KEY as `0x${string}`,
  basePricePerCU: 4_000_000_000_000n,  // GRT wei per compute unit
});

const blockNumber = await client.request("eth_blockNumber", []);
const balance = await client.request("eth_getBalance", ["0x...", "latest"]);
```

The client discovers providers via the subgraph, selects one by QoS score, signs a TAP receipt per request, and tracks latency with an EMA.

### Low-level utilities

```typescript
import {
  discoverProviders,
  selectProvider,
  buildReceipt,
  signReceipt,
} from "@lodestar-dispatch/consumer-sdk";

// Discover active providers for a chain + tier
const providers = await discoverProviders(
  "https://api.studio.thegraph.com/query/1747796/rpc-network/v0.3.0",
  42161,  // chainId
  0,      // tier: 0 = Standard, 1 = Archive
);

const provider = selectProvider(providers);

// Build and sign a receipt
const receipt = buildReceipt(
  "0x7101d5c1a5c89c3647f5118da118e56c023ba0b9",  // dataService
  provider.address,                                // serviceProvider
  4_000_000_000_000n,                             // value (GRT wei)
);
const signed = await signReceipt(
  receipt,
  { verifyingContract: "0x8f69F5C07477Ac46FBc491B1E6D91E2bb0111A9e" },
  privateKey,
);
```

---

## Funding the escrow

> **Gateway users skip this section.** The gateway manages its own on-chain escrow. If you're calling `https://gateway.lodestar-dashboard.com` directly, you do not need to deposit GRT.

If you're using **dispatch-proxy** or the **consumer SDK**, you are the on-chain payer and must deposit GRT into `PaymentsEscrow` on Arbitrum One before providers will serve your requests.

### Via the Lodestar dashboard (easiest)

Go to [lodestar-dashboard.com/dispatch](https://lodestar-dashboard.com/dispatch). Connect MetaMask, paste your consumer address, and deposit GRT. The dashboard calls `depositTo()` on the PaymentsEscrow contract so you can fund any address's escrow directly — the consumer wallet itself needs no ETH or GRT. Useful for funding `dispatch-proxy` from a separate hot wallet.

### Manually (cast / ethers)

```solidity
// 1. Approve the escrow contract
GRT.approve(0xf6Fcc27aAf1fcD8B254498c9794451d82afC673E, amount);

// 2a. Deposit from your own address
PaymentsEscrow.deposit(
    0x8f69F5C07477Ac46FBc491B1E6D91E2bb0111A9e,  // collector: GraphTallyCollector
    providerAddress,                                // receiver: the indexer you're paying
    amount
);

// 2b. Or fund any address's escrow (useful for the proxy key)
PaymentsEscrow.depositTo(
    consumerAddress,                                // payer: the consumer key you're funding
    0x8f69F5C07477Ac46FBc491B1E6D91E2bb0111A9e,  // collector: GraphTallyCollector
    providerAddress,                                // receiver
    amount
);
```

Deposits are keyed by `(payer, collector, receiver)`. `dispatch-service` draws down automatically on each `collect()` cycle (hourly by default). Providers reject requests from addresses with zero escrow balance (checked on-chain every 30 seconds). Check your balance with:

```bash
cast call 0xf6Fcc27aAf1fcD8B254498c9794451d82afC673E \
  "getBalance(address,address,address)(uint256)" \
  <YOUR_ADDRESS> \
  0x8f69F5C07477Ac46FBc491B1E6D91E2bb0111A9e \
  <PROVIDER_ADDRESS> \
  --rpc-url https://arb1.arbitrum.io/rpc
```
