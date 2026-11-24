// SPDX-License-Identifier: Apache-2.0
pragma solidity ^0.8.27;

import {Test} from "forge-std/Test.sol";
import {MessageHashUtils} from "@openzeppelin/contracts/utils/cryptography/MessageHashUtils.sol";
import {TransparentUpgradeableProxy} from "@openzeppelin/contracts/proxy/transparent/TransparentUpgradeableProxy.sol";
import {ERC1967Proxy} from "@openzeppelin/contracts/proxy/ERC1967/ERC1967Proxy.sol";

import {RPCDataService} from "../src/RPCDataService.sol";
import {IRPCDataService} from "../src/interfaces/IRPCDataService.sol";
import {IGraphPayments} from "@graphprotocol/horizon/interfaces/IGraphPayments.sol";
import {IGraphTallyCollector} from "@graphprotocol/horizon/interfaces/IGraphTallyCollector.sol";
import {IHorizonStakingTypes} from "@graphprotocol/interfaces/contracts/horizon/internal/IHorizonStakingTypes.sol";

import {GraphPayments} from "@graphprotocol/horizon/payments/GraphPayments.sol";
import {PaymentsEscrow} from "@graphprotocol/horizon/payments/PaymentsEscrow.sol";
import {GraphTallyCollector} from "@graphprotocol/horizon/payments/collectors/GraphTallyCollector.sol";
import {MockGRTToken} from "@graphprotocol/horizon/mocks/MockGRTToken.sol";
import {ControllerMock} from "@graphprotocol/horizon/mocks/ControllerMock.sol";

// ---------------------------------------------------------------------------
// Staking mock — minimal surface required by the payment contracts and DataService.
//
// GraphTallyCollector needs:
//   getProviderTokensAvailable(sp, ds) → must be > 0 for the provider to collect.
//
// GraphPayments needs:
//   getDelegationPool(sp, ds) → DelegationPool{shares=0} skips delegation cut.
//
// DataServiceFees (ProvisionTracker.lock) needs:
//   getTokensAvailable(sp, ds, delegationRatio) → provision tokens.
//
// DataService (_checkProvisionTokens, _checkProvisionParameters) needs:
//   getProvision(sp, ds) → Provision struct.
//
// DataService (onlyAuthorizedForProvision) needs:
//   isAuthorized(sp, operator, senderAddress) → bool.
// ---------------------------------------------------------------------------
contract MockHorizonStakingIntegration {
    mapping(address => mapping(address => IHorizonStakingTypes.Provision)) public provisions;

    function setProvision(address serviceProvider, address dataService, uint256 tokens, uint64 thawingPeriod) external {
        provisions[serviceProvider][dataService] = IHorizonStakingTypes.Provision({
            tokens: tokens,
            tokensThawing: 0,
            sharesThawing: 0,
            maxVerifierCut: 1_000_000,
            thawingPeriod: thawingPeriod,
            createdAt: uint64(block.timestamp),
            maxVerifierCutPending: 0,
            thawingPeriodPending: 0,
            lastParametersStagedAt: 0,
            thawingNonce: 0
        });
    }

    function getProvision(address serviceProvider, address dataService)
        external
        view
        returns (IHorizonStakingTypes.Provision memory)
    {
        return provisions[serviceProvider][dataService];
    }

    function isAuthorized(address serviceProvider, address, address operator) external pure returns (bool) {
        return serviceProvider == operator;
    }

    function getProviderTokensAvailable(address serviceProvider, address dataService) external view returns (uint256) {
        return provisions[serviceProvider][dataService].tokens;
    }

    function getTokensAvailable(address serviceProvider, address dataService, uint32) external view returns (uint256) {
        return provisions[serviceProvider][dataService].tokens;
    }

    function getDelegationPool(address, address) external pure returns (IHorizonStakingTypes.DelegationPool memory) {
        return IHorizonStakingTypes.DelegationPool({
            tokens: 0, shares: 0, tokensThawing: 0, sharesThawing: 0, thawingNonce: 0
        });
    }

    function slash(address, uint256, uint256, address) external {}
    function acceptProvisionParameters(address) external {}
}

// ---------------------------------------------------------------------------
// Integration test — uses REAL GraphTallyCollector, GraphPayments, PaymentsEscrow.
//
// Why: mocking the payment contracts masks EIP-712 hashing bugs, signer
// authorisation failures, and RAV replay issues. HorizonStaking is the
// only thing we mock, because provision validation logic is simple and
// deterministic (SubstreamsDataService pattern).
// ---------------------------------------------------------------------------
contract RPCDataServiceIntegrationTest is Test {
    // -----------------------------------------------------------------------
    // Protocol contracts (real)
    // -----------------------------------------------------------------------
    MockGRTToken internal grt;
    ControllerMock internal controller;
    MockHorizonStakingIntegration internal staking;
    GraphPayments internal payments;
    PaymentsEscrow internal escrow;
    GraphTallyCollector internal tallyCollector;

    // -----------------------------------------------------------------------
    // Our contract under test
    // -----------------------------------------------------------------------
    RPCDataService internal service;

    // -----------------------------------------------------------------------
    // Actors
    // -----------------------------------------------------------------------
    address internal owner = makeAddr("owner");
    address internal pauseGuardian = makeAddr("pauseGuardian");
    address internal governor = makeAddr("governor");
    address internal provider = makeAddr("provider");
    address internal gateway = makeAddr("gateway");

    uint256 internal constant SUFFICIENT_PROVISION = 25_000e18;
    uint64 internal constant SUFFICIENT_THAWING = 14 days;
    uint64 internal constant CHAIN_ETH_MAINNET = 1;
    uint256 internal constant GRT_AMOUNT = 1_000e18;

    // EIP-712 typehash for ReceiptAggregateVoucher — must match GraphTallyCollector exactly.
    bytes32 private constant RAV_TYPEHASH = keccak256(
        "ReceiptAggregateVoucher(bytes32 collectionId,address payer,address serviceProvider,address dataService,uint64 timestampNs,uint128 valueAggregate,bytes metadata)"
    );

    // -----------------------------------------------------------------------
    // setUp — deploy protocol stack using address prediction so GraphDirectory
    // immutables resolve correctly before each contract is constructed.
    // -----------------------------------------------------------------------
    function setUp() public {
        // 1. Base contracts (order determines nonces used below)
        grt = new MockGRTToken();
        controller = new ControllerMock(governor);
        staking = new MockHorizonStakingIntegration();

        // 2. Fix the proxy addresses up front. Predicting them from the test's nonce broke when
        //    forge began spending nonces of its own during deployment.
        address predictedPaymentsProxy = makeAddr("GraphPaymentsProxy");
        address predictedEscrowProxy = makeAddr("PaymentsEscrowProxy");

        // 3. Register all proxies before deploying any GraphDirectory-based contract
        //    (GraphDirectory reads from controller in its constructor and stores as immutables).
        //    Peripheral contracts not used in these tests get address(1) as a dummy.
        controller.setContractProxy(keccak256("GraphToken"), address(grt));
        controller.setContractProxy(keccak256("Staking"), address(staking));
        controller.setContractProxy(keccak256("GraphPayments"), predictedPaymentsProxy);
        controller.setContractProxy(keccak256("PaymentsEscrow"), predictedEscrowProxy);
        controller.setContractProxy(keccak256("EpochManager"), address(1));
        controller.setContractProxy(keccak256("RewardsManager"), address(1));
        controller.setContractProxy(keccak256("GraphTokenGateway"), address(1));
        controller.setContractProxy(keccak256("GraphProxyAdmin"), address(1));
        controller.setContractProxy(keccak256("Curation"), address(1));

        // 4. Deploy GraphPayments (impl + proxy at its fixed address).
        GraphPayments paymentsImpl = new GraphPayments(address(controller), 0); // protocolCut=0
        deployCodeTo(
            "TransparentUpgradeableProxy.sol:TransparentUpgradeableProxy",
            abi.encode(address(paymentsImpl), address(1), abi.encodeCall(GraphPayments.initialize, ())),
            predictedPaymentsProxy
        );
        payments = GraphPayments(predictedPaymentsProxy);

        // 5. Deploy PaymentsEscrow (impl + proxy at its fixed address).
        PaymentsEscrow escrowImpl = new PaymentsEscrow(address(controller), 0); // withdrawThawing=0
        deployCodeTo(
            "TransparentUpgradeableProxy.sol:TransparentUpgradeableProxy",
            abi.encode(address(escrowImpl), address(1), abi.encodeCall(PaymentsEscrow.initialize, ())),
            predictedEscrowProxy
        );
        escrow = PaymentsEscrow(predictedEscrowProxy);

        // 6. GraphTallyCollector — not upgradeable.
        //    revokeSignerThawingPeriod=0: signers can be revoked immediately (fine for tests).
        tallyCollector = new GraphTallyCollector("GraphTallyCollector", "1", address(controller), 0);

        // 7. Our contract under test.
        RPCDataService serviceImpl = new RPCDataService(address(controller), address(tallyCollector));
        service = RPCDataService(
            address(
                new ERC1967Proxy(
                    address(serviceImpl), abi.encodeCall(RPCDataService.initialize, (owner, pauseGuardian))
                )
            )
        );

        // 8. Governance: enable chain 1 (Ethereum mainnet).
        vm.prank(owner);
        service.addChain(CHAIN_ETH_MAINNET, 0);

        // 9. Provider provisions stake in mock staking.
        staking.setProvision(provider, address(service), SUFFICIENT_PROVISION, SUFFICIENT_THAWING);
    }

    // -----------------------------------------------------------------------
    // Test: full collect() flow with real EIP-712 verification
    //
    // Validates:
    //   - Signer authorisation proof (keccak + ethSignedMessageHash)
    //   - RAV EIP-712 signature verification in GraphTallyCollector
    //   - RPCDataService correctly passes receiverDestination to the collector
    //   - GRT flows from escrow → GraphPayments → paymentsDestination wallet
    // -----------------------------------------------------------------------
    function test_collect_feesReachPaymentsDestination() public {
        uint256 signerPrivKey = 0xabcdef1234;
        address signer = vm.addr(signerPrivKey);
        address paymentWallet = makeAddr("paymentWallet");

        // Provider registers with a separate payment wallet.
        _register(provider, "https://rpc.example.com", "u1hx", paymentWallet);
        assertEq(service.paymentsDestination(provider), paymentWallet);

        // Gateway authorises the signer in GraphTallyCollector.
        _authorizeSigner(gateway, signerPrivKey, signer);

        // Gateway mints GRT, approves escrow, and deposits into the payer-collector-receiver bucket.
        grt.mint(gateway, GRT_AMOUNT);
        vm.startPrank(gateway);
        grt.approve(address(escrow), GRT_AMOUNT);
        escrow.deposit(address(tallyCollector), provider, GRT_AMOUNT);
        vm.stopPrank();

        // Build RAV and sign it with the signer's key.
        IGraphTallyCollector.ReceiptAggregateVoucher memory rav = IGraphTallyCollector.ReceiptAggregateVoucher({
            collectionId: bytes32(0),
            payer: gateway,
            serviceProvider: provider,
            dataService: address(service),
            timestampNs: uint64(block.timestamp) * 1_000_000_000,
            valueAggregate: uint128(GRT_AMOUNT),
            metadata: ""
        });
        bytes memory sig = _signRAV(signerPrivKey, rav);

        // Collect: anyone can call on behalf of the provider.
        uint256 walletBefore = grt.balanceOf(paymentWallet);
        service.collect(
            provider,
            IGraphPayments.PaymentTypes.QueryFee,
            abi.encode(IGraphTallyCollector.SignedRAV({rav: rav, signature: sig}), GRT_AMOUNT)
        );

        // 2% total cut: 1% burned + 1% retained by data service. Remainder reaches paymentWallet.
        uint256 totalCut = GRT_AMOUNT * (service.BURN_CUT_PPM() + service.DATA_SERVICE_CUT_PPM()) / 1_000_000;
        assertEq(grt.balanceOf(paymentWallet), walletBefore + GRT_AMOUNT - totalCut);
        assertEq(grt.balanceOf(provider), 0); // not the provider address itself
    }

    function test_collect_partialThenRemainder_locksWhatTheCollectorCollects() public {
        uint256 signerPrivKey = 0xabcdef1234;
        address signer = vm.addr(signerPrivKey);
        _register(provider, "https://rpc.example.com", "u1hx", makeAddr("paymentWallet"));
        _authorizeSigner(gateway, signerPrivKey, signer);

        grt.mint(gateway, GRT_AMOUNT);
        vm.startPrank(gateway);
        grt.approve(address(escrow), GRT_AMOUNT);
        escrow.deposit(address(tallyCollector), provider, GRT_AMOUNT);
        vm.stopPrank();

        IGraphTallyCollector.ReceiptAggregateVoucher memory rav = IGraphTallyCollector.ReceiptAggregateVoucher({
            collectionId: bytes32(0),
            payer: gateway,
            serviceProvider: provider,
            dataService: address(service),
            timestampNs: uint64(block.timestamp) * 1_000_000_000,
            valueAggregate: uint128(GRT_AMOUNT),
            metadata: ""
        });
        IGraphTallyCollector.SignedRAV memory signed =
            IGraphTallyCollector.SignedRAV({rav: rav, signature: _signRAV(signerPrivKey, rav)});

        uint256 part = GRT_AMOUNT / 4;
        service.collect(provider, IGraphPayments.PaymentTypes.QueryFee, abi.encode(signed, part));
        // Zero asks the collector for everything not yet collected.
        service.collect(provider, IGraphPayments.PaymentTypes.QueryFee, abi.encode(signed, uint256(0)));

        assertEq(tallyCollector.tokensCollected(address(service), bytes32(0), provider, gateway), GRT_AMOUNT);
        assertEq(service.feesProvisionTracker(provider), GRT_AMOUNT * service.STAKE_TO_FEES_RATIO());
    }

    function test_collect_revertsOnInvalidSignature() public {
        address paymentWallet = makeAddr("paymentWallet");
        _register(provider, "https://rpc.example.com", "u1hx", paymentWallet);

        uint256 realSignerKey = 0xabcdef1234;
        address realSigner = vm.addr(realSignerKey);
        _authorizeSigner(gateway, realSignerKey, realSigner);

        grt.mint(gateway, GRT_AMOUNT);
        vm.startPrank(gateway);
        grt.approve(address(escrow), GRT_AMOUNT);
        escrow.deposit(address(tallyCollector), provider, GRT_AMOUNT);
        vm.stopPrank();

        IGraphTallyCollector.ReceiptAggregateVoucher memory rav = IGraphTallyCollector.ReceiptAggregateVoucher({
            collectionId: bytes32(0),
            payer: gateway,
            serviceProvider: provider,
            dataService: address(service),
            timestampNs: uint64(block.timestamp) * 1_000_000_000,
            valueAggregate: uint128(GRT_AMOUNT),
            metadata: ""
        });

        // Sign with a DIFFERENT key — not authorised by gateway.
        uint256 wrongKey = 0xdeadbeef;
        bytes memory badSig = _signRAV(wrongKey, rav);

        vm.expectRevert(); // GraphTallyCollectorInvalidRAVSigner or similar
        service.collect(
            provider,
            IGraphPayments.PaymentTypes.QueryFee,
            abi.encode(IGraphTallyCollector.SignedRAV({rav: rav, signature: badSig}), GRT_AMOUNT)
        );
    }

    function test_collect_revertsOnWrongPaymentType() public {
        address paymentWallet = makeAddr("paymentWallet");
        _register(provider, "https://rpc.example.com", "u1hx", paymentWallet);

        // IndexingFee is not QueryFee — should revert immediately.
        vm.expectRevert(abi.encodeWithSelector(IRPCDataService.InvalidPaymentType.selector));
        service.collect(
            provider,
            IGraphPayments.PaymentTypes.IndexingFee,
            abi.encode(bytes32(0), uint256(0)) // data doesn't matter — revert is earlier
        );
    }

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    function _register(address _provider, string memory endpoint, string memory geo, address dest) internal {
        vm.prank(_provider);
        service.register(_provider, abi.encode(endpoint, geo, dest));
    }

    /// @dev Produce and submit a signer authorisation proof.
    ///      The signer signs: ethSignedMessageHash(keccak(chainId ‖ tallyCollector ‖ "authorizeSignerProof" ‖ deadline ‖ authorizer))
    function _authorizeSigner(address authorizer, uint256 signerPrivKey, address signer) internal {
        uint256 proofDeadline = block.timestamp + 1 hours;
        bytes32 messageHash = keccak256(
            abi.encodePacked(block.chainid, address(tallyCollector), "authorizeSignerProof", proofDeadline, authorizer)
        );
        bytes32 digest = MessageHashUtils.toEthSignedMessageHash(messageHash);
        (uint8 v, bytes32 r, bytes32 s) = vm.sign(signerPrivKey, digest);
        bytes memory proof = abi.encodePacked(r, s, v);

        vm.prank(authorizer);
        tallyCollector.authorizeSigner(signer, proofDeadline, proof);
    }

    /// @dev Sign a RAV with EIP-712 using the GraphTallyCollector domain.
    function _signRAV(uint256 signerPrivKey, IGraphTallyCollector.ReceiptAggregateVoucher memory rav)
        internal
        view
        returns (bytes memory)
    {
        bytes32 domainSeparator = keccak256(
            abi.encode(
                keccak256("EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)"),
                keccak256("GraphTallyCollector"),
                keccak256("1"),
                block.chainid,
                address(tallyCollector)
            )
        );

        bytes32 structHash = keccak256(
            abi.encode(
                RAV_TYPEHASH,
                rav.collectionId,
                rav.payer,
                rav.serviceProvider,
                rav.dataService,
                rav.timestampNs,
                rav.valueAggregate,
                keccak256(rav.metadata)
            )
        );

        bytes32 digest = keccak256(abi.encodePacked("\x19\x01", domainSeparator, structHash));
        (uint8 v, bytes32 r, bytes32 s) = vm.sign(signerPrivKey, digest);
        return abi.encodePacked(r, s, v);
    }
}
