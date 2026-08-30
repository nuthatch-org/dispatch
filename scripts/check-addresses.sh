#!/usr/bin/env bash
# Do the addresses in this repository point at contracts that exist?
#
# On 2026-08-30 two of them did not. `RPCDataService` was listed everywhere as
# 0xA983b18B8291F0c317Ba4Fe0dc0f7cc9373AF078, which is a stray implementation the live proxy has
# never pointed at, and `GraphPayments` was listed as 0xb98a3D452E43e40C70F3c0B03C5c7B56A8B3b8CA,
# which holds no code at all. Neither was a documentation typo: the first was the default in
# `proxy/src/index.ts`, the `data_service_address` in both example gateway configs, and the address
# the subgraph indexed.
#
# The reason it went unnoticed for months is that every one of those failures is quiet. A subgraph
# pointed at an address with no events syncs perfectly and returns nothing. A TAP receipt signed
# against the wrong data service verifies locally and fails only at redemption. And the stray
# implementation *answers*: it returned the same thawing range, verifier cut and owner as the live
# proxy, and a minimum provision of 10,000 GRT against the real 555.
#
# So this asks the chain. Run it before a release and whenever an address changes.
#
#   RPC=https://arb1.arbitrum.io/rpc scripts/check-addresses.sh
set -uo pipefail

RPC="${RPC:-https://arb1.arbitrum.io/rpc}"
EIP1967_SLOT=0x360894a13ba1a3210667c828492db98dca3e2076cc3735a920a3ca505d382bbc

command -v cast >/dev/null || { echo "needs foundry's cast on PATH" >&2; exit 2; }

fail=0

# name | address | expect: proxy (upgradeable, must have a non-empty EIP-1967 slot) or code (any contract)
check() {
  local name="$1" addr="$2" expect="$3"
  local code slot
  code=$(cast code "$addr" --rpc-url "$RPC" 2>/dev/null || echo 0x)
  if [ "${#code}" -le 3 ]; then
    printf '  ✗ %-22s %s  NO CODE AT THIS ADDRESS\n' "$name" "$addr"
    fail=1
    return
  fi
  if [ "$expect" = "proxy" ]; then
    slot=$(cast storage "$addr" "$EIP1967_SLOT" --rpc-url "$RPC" 2>/dev/null || echo "")
    if [ -z "$slot" ] || [ "$slot" = "0x0000000000000000000000000000000000000000000000000000000000000000" ]; then
      printf '  ✗ %-22s %s  NOT A PROXY: this is an implementation or a bare contract.\n' "$name" "$addr"
      printf '      An implementation answers most calls with stale values rather than reverting.\n'
      fail=1
      return
    fi
  fi
  printf '  ✓ %-22s %s\n' "$name" "$addr"
}

echo "Arbitrum One, via $RPC"
check RPCDataService      0x7101d5c1a5c89c3647f5118da118e56c023ba0b9 proxy
check HorizonStaking      0x00669A4CF01450B64E8A2A20E9b1FCB71E61eF03 proxy
check PaymentsEscrow      0xf6Fcc27aAf1fcD8B254498c9794451d82afC673E proxy
check GraphPayments       0x7Aae8ae011927BC36Cb4d0d3e81f2E6E30daE06D proxy
check GraphTallyCollector 0x8f69F5C07477Ac46FBc491B1E6D91E2bb0111A9e code
check L2GraphToken        0x9623063377AD1B27544C965cCd7342f7EA7e88C7 code

# Every address checked above must also be the one the repository actually uses. A table that is
# right while the config beside it is wrong is the exact shape of the bug this script exists for.
echo
echo "References in tracked files:"
if grep -rIn --exclude-dir={node_modules,.git,target,out,cache,dist} \
     -e 0xA983b18B8291F0c317Ba4Fe0dc0f7cc9373AF078 \
     -e 0xb98a3D452E43e40C70F3c0B03C5c7B56A8B3b8CA . \
     | grep -v 'docs/audit-disposition.md' | grep -v 'scripts/check-addresses.sh'; then
  echo "  ✗ a known-dead address is still referenced above"
  fail=1
else
  echo "  ✓ no known-dead address referenced outside the write-up that explains it"
fi

exit "$fail"
