// SPDX-License-Identifier: Apache-2.0
pragma solidity ^0.8.27;

// H-1 re-scope PoC (2026-08-28).
//
// The 2026-04-15 audit rated "Stake-Locking Bypass via _lockStake Revert After Fee Collection"
// as High, but its own triager note declined to confirm it:
//
//   "If it returns normally and the parent then reverts, Solidity's normal revert semantics WOULD
//    roll back all state changes in the entire transaction ... Rate as High pending PoC
//    confirmation."
//
// The audit asked for exactly one experiment. This is it, built to its own success criteria:
//
//   "Success criteria: GRT received at paymentsDestination > 0 AND _lockStake did not execute."
//
// A mock collector moves real GRT to paymentsDestination and returns a fee large enough that
// `fees * STAKE_TO_FEES_RATIO` exceeds the provider's available stake, so `_lockStake` reverts
// after the transfer has already happened inside the sub-call.

import {Test} from "forge-std/Test.sol";
import {ERC1967Proxy} from "@openzeppelin/contracts/proxy/ERC1967/ERC1967Proxy.sol";

import {RPCDataService} from "../src/RPCDataService.sol";
import {IHorizonStakingTypes} from "@graphprotocol/interfaces/contracts/horizon/internal/IHorizonStakingTypes.sol";
import {IGraphPayments} from "@graphprotocol/horizon/interfaces/IGraphPayments.sol";
import {IGraphTallyCollector} from "@graphprotocol/horizon/interfaces/IGraphTallyCollector.sol";

/// @dev Enough of an ERC20 for `balanceOf` and `burn`, plus a mint for the mock collector.
contract MockGraphToken {
    mapping(address => uint256) public balanceOf;

    function mint(address to, uint256 amount) external {
        balanceOf[to] += amount;
    }

    function burn(uint256 amount) external {
        balanceOf[msg.sender] -= amount;
    }

    function transfer(address to, uint256 amount) external returns (bool) {
        balanceOf[msg.sender] -= amount;
        balanceOf[to] += amount;
        return true;
    }
}

/// @dev A collector that really does move funds and really does return a fee, so that a failure to
///      persist can only be the transaction unwinding rather than the mock declining to pay.
contract MockGraphTallyCollector {
    MockGraphToken public immutable TOKEN;
    uint256 public feeToReturn;
    address public destination;

    constructor(MockGraphToken token) {
        TOKEN = token;
    }

    function setPayout(uint256 fee, address dest) external {
        feeToReturn = fee;
        destination = dest;
    }

    function collect(IGraphPayments.PaymentTypes, bytes calldata, uint256) external returns (uint256) {
        // Settle before returning, exactly as the audit hypothesised the real chain does.
        TOKEN.mint(destination, feeToReturn);
        return feeToReturn;
    }
}

contract MockStakingWithAvailability {
    mapping(address => mapping(address => IHorizonStakingTypes.Provision)) public provisions;
    mapping(address => uint256) public available;

    function setProvision(address sp, address ds, uint256 tokens, uint64 thawing) external {
        provisions[sp][ds] = IHorizonStakingTypes.Provision({
            tokens: tokens,
            tokensThawing: 0,
            sharesThawing: 0,
            maxVerifierCut: 1_000_000,
            thawingPeriod: thawing,
            createdAt: uint64(block.timestamp),
            maxVerifierCutPending: 0,
            thawingPeriodPending: 0,
            lastParametersStagedAt: 0,
            thawingNonce: 0
        });
    }

    function setTokensAvailable(address sp, uint256 amount) external {
        available[sp] = amount;
    }

    function getProvision(address sp, address ds) external view returns (IHorizonStakingTypes.Provision memory) {
        return provisions[sp][ds];
    }

    function getTokensAvailable(address sp, address, uint32) external view returns (uint256) {
        return available[sp];
    }

    function isAuthorized(address sp, address, address operator) external pure returns (bool) {
        return sp == operator;
    }

    function slash(address, uint256, uint256, address) external {}
    function acceptProvisionParameters(address) external {}
}

contract MockControllerWithToken {
    mapping(bytes32 => address) private _contracts;

    constructor(address staking_, address token_) {
        address dummy = address(1);
        _contracts[keccak256("GraphToken")] = token_;
        _contracts[keccak256("Staking")] = staking_;
        _contracts[keccak256("GraphPayments")] = dummy;
        _contracts[keccak256("PaymentsEscrow")] = dummy;
        _contracts[keccak256("EpochManager")] = dummy;
        _contracts[keccak256("RewardsManager")] = dummy;
        _contracts[keccak256("GraphTokenGateway")] = dummy;
        _contracts[keccak256("GraphProxyAdmin")] = dummy;
        _contracts[keccak256("Curation")] = dummy;
    }

    function getContractProxy(bytes32 id) external view returns (address) {
        return _contracts[id];
    }
}

contract H1CollectOrderingTest is Test {
    RPCDataService service;
    MockStakingWithAvailability staking;
    MockGraphToken token;
    MockGraphTallyCollector collector;

    address owner = makeAddr("owner");
    address pauseGuardian = makeAddr("pauseGuardian");
    address provider = makeAddr("provider");
    address destination = makeAddr("destination");

    uint256 constant FEE = 500e18;
    uint256 constant PROVISION = 10_000e18;

    function setUp() public {
        staking = new MockStakingWithAvailability();
        token = new MockGraphToken();
        collector = new MockGraphTallyCollector(token);
        MockControllerWithToken controller = new MockControllerWithToken(address(staking), address(token));

        RPCDataService impl = new RPCDataService(address(controller), address(collector));
        service = RPCDataService(
            address(new ERC1967Proxy(address(impl), abi.encodeCall(RPCDataService.initialize, (owner, pauseGuardian))))
        );

        staking.setProvision(provider, address(service), PROVISION, 14 days);
        vm.prank(owner);
        service.addChain(1, 0);
        vm.prank(provider);
        // register(url, geohash, paymentsDestination) — the destination is where the collector
        // pays, and is exactly the address the audit's success criterion watches.
        service.register(provider, abi.encode("https://rpc.example", "u1hx", destination));

        collector.setPayout(FEE, destination);
    }

    function _collect() internal {
        IGraphTallyCollector.SignedRAV memory rav;
        rav.rav.serviceProvider = provider;
        rav.rav.dataService = address(service);
        service.collect(provider, IGraphPayments.PaymentTypes.QueryFee, abi.encode(rav, FEE));
    }

    /// The audit's exact experiment. Available stake is one wei short of `FEE * 5`, so `_lockStake`
    /// reverts after the collector has already moved GRT to `destination`.
    function test_H1_feePaymentDoesNotSurviveTheLockStakeRevert() public {
        staking.setTokensAvailable(provider, FEE * 5 - 1);
        assertEq(token.balanceOf(destination), 0, "precondition");

        vm.expectRevert();
        _collect();

        // The audit's success criterion was "GRT received at paymentsDestination > 0". It is not.
        // The sub-call's transfer is unwound with the parent frame, as EVM atomicity requires.
        assertEq(
            token.balanceOf(destination), 0, "H-1 CONFIRMED EXPLOITABLE: fees persisted through a reverting _lockStake"
        );
    }

    /// The control: with enough stake the same call succeeds and the money does move, proving the
    /// test above fails for the intended reason rather than because the mock never pays.
    function test_H1_control_theMockReallyDoesPayWhenLockingSucceeds() public {
        staking.setTokensAvailable(provider, FEE * 5);
        _collect();
        assertEq(token.balanceOf(destination), FEE, "the mock must actually move funds");
    }
}
