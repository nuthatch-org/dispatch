# Verification

Dispatch uses two complementary mechanisms to catch bad providers: **attestation** (cryptographic proof of what was served) and **quorum** (cross-provider consensus on deterministic results).

---

## Attestation

Every `dispatch-service` response carries a signed attestation header:

```
X-Drpc-Attestation: {"signer":"0x…","signature":"0x…"}
```

The provider signs the following message with its operator key:

```
keccak256(
    chain_id_be8        // chain ID as 8 bytes big-endian
    || method_utf8      // method name as UTF-8 bytes
    || keccak256(params_json)   // keccak of the serialised params field
    || keccak256(result_json)   // keccak of the serialised result (or error) field
)
```

The signature is a 65-byte secp256k1 ECDSA signature (`r || s || v`, Ethereum-style `v = 27/28`).

**Gateway verification** happens automatically on every response before it is forwarded to the consumer. The gateway recovers the signer from the signature and checks it matches the signer address stated in the header. A mismatch logs a warning and penalises the provider's QoS score.

Without slashing, attestations serve as:
- A tamper-evident audit trail — consumers can verify a provider claimed a specific response
- A QoS signal — providers that forge or omit attestations are deprioritised over time
- Future-proof groundwork — if slash infrastructure is added later, the format is already in place

---

## Cross-checks and quorum

Deterministic methods are answered by one provider. On a sampled share of them (`cross_check_rate`, default 2%), the gateway asks a second provider the same question in the background and compares the results. Only calls pinned to a settled block are sampled: a block hash, or a block number at least 64 blocks behind head. Two honest nodes asked about `latest` can disagree simply because their heads differ. A null or error on either side is inconclusive.

A disagreement marks both providers as suspect for `suspect_secs` (default 600). While a provider is suspect, every deterministic call routed to it goes to `quorum_k` providers (default 3) and the majority result wins. Outcomes are counted in `dispatch_gateway_cross_checks_total{outcome}`.

This costs about 1.02 receipts per deterministic call rather than three. `quorum_always = true` restores blanket quorum.

**Deterministic methods:**

| Method |
|---|
| `eth_call` |
| `eth_getLogs` |
| `eth_getBalance` |
| `eth_getCode` |
| `eth_getTransactionCount` |
| `eth_getStorageAt` |
| `eth_getBlockByHash` |
| `eth_getTransactionByHash` |
| `eth_getTransactionReceipt` |

All other methods (including `eth_blockNumber`, `eth_estimateGas`, `eth_sendRawTransaction`) are tried on one provider at a time, falling back to the next on failure. Only the provider that answers is paid.

---

## What is not implemented

- **Slashing** — `slash()` reverts with "not supported". RPC responses have no canonical on-chain truth to verify against, so there is no safe basis for slashing.
- **EIP-1186 Merkle proof verification** — on-demand proof attachment and MPT verification; deferred with slashing.
- **Block header trust oracle** — required for Merkle proof verification; not built.
