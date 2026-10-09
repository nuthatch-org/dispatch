# Audit scope — RPCDataService

**A brief somebody else can fund.** Nuthatch has no budget for an external audit and will
not acquire one. What we can do is make an audit cheap to buy and hard to waste: a tight scope, an
honest list of what has already been established, and an explicit list of what not to pay for.

Companion to [`audit-disposition.md`](audit-disposition.md), which handles the *previous* audit's
findings. This one is forward-looking: what an auditor should look at now.

Written 2026-08-30 against `contracts/src/RPCDataService.sol`.

---

## The surface, measured rather than described

| | |
|---|---|
| Contract | **365 lines**, one file |
| External / public functions | **13** |
| Upgradeability | UUPS behind an ERC-1967 proxy; `_authorizeUpgrade` is `onlyOwner` |
| Settlement | `GraphTallyCollector` — per-query TAP receipts aggregated into RAVs |
| Existing tests | 36 across four files, including a cross-language EIP-712 check and a PoC disproving H-1 |
| Ownership today | a single EOA (see *Known weaknesses*) |

Function list, so a quote can be written without cloning: `initialize`, `addChain`, `removeChain`,
`setDefaultMinProvision`, `setMinThawingPeriod`, `deregister`, `setPaymentsDestination`, `slash`,
`isRegistered`, `getChainRegistrations`, `activeRegistrationCount`, `setPauseGuardian`,
`withdrawFees`, plus the inherited `register` / `startService` / `stopService` / `collect` from
Graph's `DataService` base.

## What is already established, and should not be re-derived

An auditor charging to rediscover these is charging for our homework.

- **The payment path works against the deployed protocol.** 18.44 GRT has been settled on Arbitrum
  One through `GraphTallyCollector`. This is not a contract whose collection is theoretical.
- **The collect payload encoding is correct.** `GraphTallyCollector` decodes
  `(SignedRAV, dataServiceCut, receiverDestination)` and that is exactly what this contract encodes.
  Worth stating because a sibling service of ours encoded four fields against a six-field struct and
  every real collection would have reverted, invisibly, behind a green suite.
- **H-1 is disproved by proof-of-concept, not by argument.** `H1CollectOrdering.t.sol` shows the fee
  payment does not survive the revert. What remains is a checks-effects-interactions *style* issue,
  and it is in scope below as style rather than as a High.
- **H-2, H-3, M-1 and L-2 are gone by deletion.** The April audit targeted `drpc-service`, a larger
  contract with a rewards pool, trusted state roots and EIP-1186 fraud proofs. None of that exists
  here; `slash()` reverts. **The cheapest remediation is deletion and it has already happened.**
- **EIP-712 hashing is checked cross-language**, so signature-domain drift between the contract and
  the Rust gateway is covered.

## What to actually audit

In descending order of what we would pay for first.

1. **The `collect` path end to end**, including the cuts arithmetic and the burn. Money moves here
   and the arithmetic is ours rather than inherited.
2. **Upgradeability and ownership.** UUPS with `onlyOwner` upgrade authority, currently an EOA. The
   question is not "is UUPS correct" but "what does this specific owner being compromised buy an
   attacker", stated plainly enough for an operator to act on.
3. **Registration and provisioning invariants.** `register` / `startService` / `stopService` /
   `deregister` against Graph's `DataService` base: can a provider reach a state where they are
   collectable while not actually provisioned, or unregistered while still holding a claim.
4. **Pausing.** `M-2` is inherited from Graph's `DataServicePausableUpgradeable` and is bounded, but
   the interaction between a pause and an in-flight collection is ours.
5. **The CEI style issue** left over from H-1. Low, and worth fixing while somebody competent is
   already reading the file.

## What is explicitly out of scope

Say so in the engagement, because these are where an auditor's hours quietly go.

- **Slashing and fraud proofs.** `slash()` reverts and that is a product decision, not an omission:
  EIP-1186 and slashing are deliberately out of scope for this service. Do not price a fraud-proof
  review.
- **Issuance and rewards.** There is none in this contract.
- **The gateway.** Rust, off-chain, separately reviewable, and not what a contract auditor is for.
- **Graph's own Horizon contracts.** `GraphTallyCollector`, `PaymentsEscrow` and `HorizonStaking` are
  upstream and separately audited.

## Known weaknesses we would disclose unprompted

An auditor finds these in an hour; better they arrive knowing.

- **Owner is a single EOA.** Multisig is the obvious remediation and has not been done.
- **No operator is currently serving.** The contract is live and unclaimed — a decision, not a
  failure. It means the audit covers code with no production traffic behind it today.
- **The contract is unaudited in its current shape.** The April audit was of a different, larger
  contract. Treat any reference to it as historical.

## Why a scope document rather than an audit

We build these services and do not operate them, and we do not have money for audits. A tight scope
with the settled parts fenced off is the difference between an affordable engagement and an
open-ended one, and it is a thing we can produce and a foundation, a DAO or an operator can act on.

If you fund one: the disposition doc and this scope are the brief, the repository is MIT, and we
will answer questions from whoever you hire.
