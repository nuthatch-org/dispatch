# Audit disposition — RPCDataService

> Re-scoped 2026-08-28 against `contracts/src/RPCDataService.sol` at `1f03d7b` (365 lines).
> Every claim below was checked by reading the current source or by running a test, not inferred.

> **Forward-looking companion:** [`audit-scope.md`](audit-scope.md) is the brief somebody else can
> fund — what to audit now, what is already established and should not be re-derived, and what is
> explicitly out of scope. This document handles the *previous* audit's findings; that one handles
> the next audit's shape.

## Why this document exists

The security assessment at `.context/outputs/1/audit-report.md` is dated **2026-04-15** and was run
against `/Users/pepe/Projects/drpc-service/contracts/src/RPCDataService.sol` — a **different,
larger contract** that had a rewards pool, owner-set trusted state roots, and EIP-1186 fraud-proof
slashing. None of that exists any more. The contract today is 365 lines, `slash()` is
`external pure` and reverts, and there is no issuance.

Carrying "3 High, 2 Medium, 2 Low" forward as if it described the deployed contract would have
mis-scoped the external audit and, worse, sent a vendor looking for code that is not there.

## Summary

**No finding from the 2026-04-15 assessment describes a live vulnerability in the current
contract.** Four were remediated by deletion, one was already fixed, one was disproved by
experiment, and one is an inherited property of Graph's own separately-audited base contract.

| ID | Original severity | Status today | Why |
|---|---|---|---|
| H-1 | High | **Disproved** | PoC shows the fee payment does not survive the revert. Reduces to a CEI style issue. |
| H-2 | High | **Gone** | No rewards pool exists. |
| H-3 | High | **Gone** | No trusted state roots; `slash()` reverts. |
| M-1 | Medium | **Gone** | Depended on the fraud-proof path removed with H-3. |
| M-2 | Medium | **Inherited, bounded** | Comes from Graph's `DataServicePausableUpgardeable`, not our code. |
| L-1 | Low | **Already fixed** | `startService` reactivates instead of pushing. |
| L-2 | Low | **Gone** | No issuance in this contract. |

## H-1 — stake-locking bypass. Disproved.

The finding claimed a provider could be paid without stake being locked, because
`GRAPH_TALLY_COLLECTOR.collect()` runs *before* `_lockStake()` and the payment was assumed to
finalise independently of the parent frame.

The assessment's own triager declined to confirm it:

> "If it returns normally and the parent then reverts, Solidity's normal revert semantics WOULD
> roll back all state changes in the entire transaction ... Rate as High pending PoC confirmation."

It asked for one experiment, with an explicit success criterion: *"GRT received at
paymentsDestination > 0 AND `_lockStake` did not execute."*

That experiment is now `contracts/test/H1CollectOrdering.t.sol`. A mock collector really does move
GRT to `paymentsDestination` and really does return a non-zero fee; available stake is set one wei
short of `fees * STAKE_TO_FEES_RATIO` so `_lockStake` reverts after the transfer.

- `test_H1_feePaymentDoesNotSurviveTheLockStakeRevert` — the destination balance is **0**. The
  success criterion is not met.
- `test_H1_control_theMockReallyDoesPayWhenLockingSucceeds` — with sufficient stake the same call
  succeeds and the full fee lands. The negative result above is therefore not the mock quietly
  declining to pay, which is the way this test would otherwise pass for the wrong reason.

**Disposition: not exploitable.** The sub-call is part of the same transaction and unwinds with it.
What remains is a checks-effects-interactions ordering wart: the doomed path burns gas before it
reverts, and if a future refactor ever wraps the collector call in `try/catch`, the finding becomes
real. Worth reordering on style grounds. Not worth an audit round on its own.

**Retained as a regression test**, because the property "a failed stake lock must not leave fees
paid" is exactly the kind of thing a later refactor breaks silently.

## H-2, H-3, M-1, L-2 — remediated by deletion

The current contract has no rewards pool, so `withdrawRewardsPool` (H-2) does not exist;
`withdrawFees` withdraws the accrued 1% data-service cut, which is revenue the contract is
supposed to hold. There are no owner-settable trusted state roots and no fraud-proof path, so H-3
and its chain-id variant M-1 have nothing to attack. There is no issuance, so L-2's `issuancePerCU`
truncation is moot.

This is the cheapest remediation there is, and it is why the external audit will buy new coverage
rather than re-checking fixes.

## M-2 — pause guardian can lift its own pause. Inherited and bounded.

`unpause()` is `onlyPauseGuardian` in
`@graphprotocol/horizon/data-service/extensions/DataServicePausableUpgradeable.sol`. It is Graph's
base contract, separately audited, and not something to fork over.

Bounded by two things: the owner can revoke a guardian at any time via `setPauseGuardian(g, false)`,
and a guardian can only pause and unpause — it cannot move funds, upgrade, or change parameters.

**Disposition: accepted.** Operationally, do not appoint a guardian you would not trust to unpause.

## L-1 — unbounded `_providerChains`. Already fixed.

`startService` reactivates a matching stopped entry rather than pushing a new one, with the
reasoning in a comment at `RPCDataService.sol:223`. The array is therefore bounded by the number of
distinct (chain, tier) pairs a provider has ever registered, not by start/stop churn.
`activeRegistrationCount()` is still a linear scan, but over a small self-limiting set.

**Disposition: fixed.**

## What the external audit should actually be scoped to

Not remediation. A fresh review of 365 lines, with the reviewer told plainly that:

- `slash()` reverts by design — this data service has no on-chain dispute path, and the
  consequence (no issuance eligibility, economic rather than cryptographic security) is accepted
  and documented in `ROADMAP.md` under "Deliberately out of scope".
- The contract is UUPS with `_authorizeUpgrade` gated on `onlyOwner`, and the owner is currently a
  single EOA. Moving it to a Safe is tracked separately and is the highest-value non-code change.
- The live pair is proxy `0x7101d5c1a5c89c3647f5118da118e56c023ba0b9` → implementation
  `0x3527a12af6256634df6aa9cc2896ed9588e12de3` on Arbitrum One. The address
  `0xA983b18B8291F0c317Ba4Fe0dc0f7cc9373AF078` appears in older material, holds code, and is a
  **superseded implementation** — auditing it would review the wrong bytecode.
