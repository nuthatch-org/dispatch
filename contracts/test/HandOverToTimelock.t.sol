// SPDX-License-Identifier: Apache-2.0
pragma solidity ^0.8.27;

import {Test} from "forge-std/Test.sol";
import {ERC1967Proxy} from "@openzeppelin/contracts/proxy/ERC1967/ERC1967Proxy.sol";
import {OwnableUpgradeable} from "@openzeppelin/contracts-upgradeable/access/OwnableUpgradeable.sol";
import {TimelockController} from "@openzeppelin/contracts/governance/TimelockController.sol";

import {RPCDataService} from "../src/RPCDataService.sol";
import {HandOverToTimelock} from "../script/HandOverToTimelock.s.sol";
import {MockController, MockHorizonStaking} from "./RPCDataService.t.sol";

contract HandOverToTimelockTest is Test {
    RPCDataService service;
    HandOverToTimelock script;
    TimelockController timelock;
    MockController controller;

    address eoa = makeAddr("eoa");
    address safe = makeAddr("safe");
    address pauseGuardian = makeAddr("pauseGuardian");
    uint256 constant DELAY = 48 hours;

    function setUp() public {
        controller = new MockController(address(new MockHorizonStaking()));
        RPCDataService impl = new RPCDataService(address(controller), address(0));
        service = RPCDataService(
            address(new ERC1967Proxy(address(impl), abi.encodeCall(RPCDataService.initialize, (eoa, pauseGuardian))))
        );
        vm.etch(safe, hex"00");

        script = new HandOverToTimelock();
        vm.prank(eoa);
        service.transferOwnership(address(script));
        timelock = script.handOver(service, safe, DELAY);
    }

    function test_ownerIsTheTimelockAndTheEoaIsLockedOut() public {
        assertEq(service.owner(), address(timelock));
        vm.prank(eoa);
        vm.expectRevert(abi.encodeWithSelector(OwnableUpgradeable.OwnableUnauthorizedAccount.selector, eoa));
        service.addChain(1, 0);
    }

    function test_safeProposalWaitsForTheDelay() public {
        bytes memory call = abi.encodeCall(RPCDataService.addChain, (1, 0));
        vm.prank(safe);
        timelock.schedule(address(service), 0, call, 0, 0, DELAY);

        vm.warp(block.timestamp + DELAY - 1);
        vm.prank(safe);
        vm.expectRevert();
        timelock.execute(address(service), 0, call, 0, 0);

        vm.warp(block.timestamp + 1);
        vm.prank(safe);
        timelock.execute(address(service), 0, call, 0, 0);
        (bool enabled,) = service.supportedChains(1);
        assertTrue(enabled);
    }

    function test_upgradeGoesThroughTheTimelock() public {
        address next = address(new RPCDataService(address(controller), address(0)));
        bytes memory call = abi.encodeCall(service.upgradeToAndCall, (next, ""));

        vm.prank(eoa);
        vm.expectRevert();
        service.upgradeToAndCall(next, "");

        vm.prank(safe);
        timelock.schedule(address(service), 0, call, 0, 0, DELAY);
        vm.warp(block.timestamp + DELAY);
        vm.prank(safe);
        timelock.execute(address(service), 0, call, 0, 0);
        bytes32 slot = 0x360894a13ba1a3210667c828492db98dca3e2076cc3735a920a3ca505d382bbc;
        assertEq(address(uint160(uint256(vm.load(address(service), slot)))), next);
    }

    function test_pauseGuardianStillActsImmediately() public {
        vm.prank(pauseGuardian);
        service.pause();
        assertTrue(service.paused());
    }

    function test_refusesAnAddressWithoutCode() public {
        RPCDataService other = RPCDataService(
            address(
                new ERC1967Proxy(
                    address(new RPCDataService(address(controller), address(0))),
                    abi.encodeCall(RPCDataService.initialize, (address(script), pauseGuardian))
                )
            )
        );
        vm.expectRevert("SAFE has no code");
        script.handOver(other, makeAddr("typo"), DELAY);
    }

    function test_refusesADelayOutsideTheWindow() public {
        vm.expectRevert("delay outside 48-72h");
        script.handOver(service, safe, 1 hours);
    }
}
