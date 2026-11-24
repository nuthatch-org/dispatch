// SPDX-License-Identifier: Apache-2.0
pragma solidity ^0.8.27;

import {Script, console2} from "forge-std/Script.sol";
import {TimelockController} from "@openzeppelin/contracts/governance/TimelockController.sol";
import {RPCDataService} from "../src/RPCDataService.sol";

/// @notice Move RPCDataService ownership from the deployer EOA to a Safe behind a timelock.
///
/// Upgrades and every owner setter then take `TIMELOCK_DELAY` seconds from a Safe proposal.
/// The pause guardian is untouched, so an emergency pause still takes effect at once.
///
/// Ship any pending implementation upgrade before running this: afterwards it waits on the delay.
///
/// Usage:
///   RPC_DATA_SERVICE_ADDRESS=0x... SAFE=0x... TIMELOCK_DELAY=172800 \
///   forge script script/HandOverToTimelock.s.sol --rpc-url arbitrum_one \
///     --private-key $PRIVATE_KEY --broadcast --verify -vvvv
contract HandOverToTimelock is Script {
    uint256 public constant MIN_DELAY = 48 hours;
    uint256 public constant MAX_DELAY = 72 hours;

    function run() external {
        RPCDataService service = RPCDataService(vm.envAddress("RPC_DATA_SERVICE_ADDRESS"));
        address safe = vm.envAddress("SAFE");
        uint256 delay = vm.envUint("TIMELOCK_DELAY");

        vm.startBroadcast();
        TimelockController timelock = handOver(service, safe, delay);
        vm.stopBroadcast();

        console2.log("TIMELOCK_ADDRESS", address(timelock));
    }

    function handOver(RPCDataService service, address safe, uint256 delay) public returns (TimelockController) {
        require(delay >= MIN_DELAY && delay <= MAX_DELAY, "delay outside 48-72h");
        // transferOwnership is single-step; a typo here is permanent.
        require(safe.code.length > 0, "SAFE has no code");

        address[] memory roles = new address[](1);
        roles[0] = safe;
        TimelockController timelock = new TimelockController(delay, roles, roles, address(0));

        require(timelock.hasRole(timelock.PROPOSER_ROLE(), safe), "safe is not proposer");
        require(timelock.hasRole(timelock.EXECUTOR_ROLE(), safe), "safe is not executor");
        require(!timelock.hasRole(timelock.DEFAULT_ADMIN_ROLE(), msg.sender), "deployer kept admin");

        service.transferOwnership(address(timelock));
        require(service.owner() == address(timelock), "ownership did not move");
        return timelock;
    }
}
