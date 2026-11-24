// SPDX-License-Identifier: Apache-2.0
pragma solidity ^0.8.27;

import {OwnableUpgradeable} from "@openzeppelin/contracts-upgradeable/access/OwnableUpgradeable.sol";
import {UUPSUpgradeable} from "@openzeppelin/contracts-upgradeable/proxy/utils/UUPSUpgradeable.sol";

import {DataService} from "@graphprotocol/horizon/data-service/DataService.sol";
import {DataServiceFees} from "@graphprotocol/horizon/data-service/extensions/DataServiceFees.sol";
import {
    DataServicePausableUpgradeable
} from "@graphprotocol/horizon/data-service/extensions/DataServicePausableUpgradeable.sol";
import {IGraphPayments} from "@graphprotocol/horizon/interfaces/IGraphPayments.sol";
import {IGraphTallyCollector} from "@graphprotocol/horizon/interfaces/IGraphTallyCollector.sol";
import {IHorizonStaking} from "@graphprotocol/horizon/interfaces/IHorizonStaking.sol";

import {IRPCDataService} from "./interfaces/IRPCDataService.sol";

/// @dev The public `tokensCollected` getter, which IGraphTallyCollector does not declare.
interface IGraphTallyCollectorLedger {
    function tokensCollected(address dataService, bytes32 collectionId, address receiver, address payer)
        external
        view
        returns (uint256);
}

/// @title RPCDataService
/// @notice Decentralised JSON-RPC data service built on The Graph Protocol's Horizon framework.
///
/// Providers (indexers) stake GRT via HorizonStaking provisions, register here,
/// then call startService for each (chainId, tier) pair they wish to serve.
/// Gateways pay per-request via GraphTally (TAP v2); providers collect fees by
/// submitting signed RAVs to collect().
///
/// @dev Inherits DataService (provision utilities, GraphDirectory), DataServiceFees
///      (stake-backed fee locking), DataServicePausableUpgradeable (emergency stop).
///      Deployed as a UUPS upgradeable proxy on Arbitrum One.
contract RPCDataService is
    OwnableUpgradeable,
    UUPSUpgradeable,
    DataService,
    DataServiceFees,
    DataServicePausableUpgradeable,
    IRPCDataService
{
    // -------------------------------------------------------------------------
    // Constants
    // -------------------------------------------------------------------------

    /// @notice Default minimum GRT provision per chain.
    uint256 public constant DEFAULT_MIN_PROVISION = 555e18;

    /// @notice Fraction of collected fees burned by the data service, in PPM (1% = 10_000).
    uint256 public constant BURN_CUT_PPM = 10_000;

    /// @notice Fraction of collected fees retained by the data service as revenue, in PPM (1% = 10_000).
    uint256 public constant DATA_SERVICE_CUT_PPM = 10_000;

    /// @notice Absolute lower bound on the thawing period.
    uint64 public constant MIN_THAWING_PERIOD = 14 days;

    /// @notice Stake locked per GRT of fees collected. Matches SubgraphService.
    uint256 public constant STAKE_TO_FEES_RATIO = 5;

    // -------------------------------------------------------------------------
    // Storage
    // -------------------------------------------------------------------------

    /// @notice Per-chain configuration (governance-controlled allowlist).
    mapping(uint256 => ChainConfig) public supportedChains;

    /// @notice Whether a provider has registered with this service.
    mapping(address => bool) public registeredProviders;

    /// @notice Address that receives collected GRT for each provider.
    /// @dev Defaults to the provider address. Separates the operator key
    ///      (used for signing) from the payment wallet (cold storage etc.).
    mapping(address => address) public paymentsDestination;

    /// @notice Chain registrations per provider (active and historical).
    mapping(address => ChainRegistration[]) internal _providerChains;

    /// @notice GraphTallyCollector used to redeem TAP receipts on-chain.
    /// @dev Immutable — baked into implementation bytecode, accessible via delegatecall.
    IGraphTallyCollector private immutable GRAPH_TALLY_COLLECTOR;

    /// @notice Governance-adjustable thawing period (lower-bounded by MIN_THAWING_PERIOD).
    uint64 public minThawingPeriod;

    /// @dev Reserved storage slots for future upgrades.
    uint256[50] private __gap;

    // -------------------------------------------------------------------------
    // Constructor (implementation only)
    // -------------------------------------------------------------------------

    /// @dev Sets immutables and locks the implementation against direct initialisation.
    ///      All mutable setup happens in initialize().
    /// @param controller The Graph Protocol controller address (GraphDirectory).
    /// @param graphTallyCollector Address of the deployed GraphTallyCollector.
    constructor(address controller, address graphTallyCollector) DataService(controller) {
        GRAPH_TALLY_COLLECTOR = IGraphTallyCollector(graphTallyCollector);
        _disableInitializers();
    }

    // -------------------------------------------------------------------------
    // Initializer (proxy)
    // -------------------------------------------------------------------------

    /// @notice Initialise the proxy. Called exactly once via ERC1967Proxy deployment.
    /// @param owner_ Initial owner (governance multisig).
    /// @param pauseGuardian Address authorised to pause the service in an emergency.
    function initialize(address owner_, address pauseGuardian) external initializer {
        __Ownable_init(owner_);
        __DataService_init();
        __DataServicePausable_init();

        minThawingPeriod = MIN_THAWING_PERIOD;
        _setProvisionTokensRange(DEFAULT_MIN_PROVISION, type(uint256).max);
        _setThawingPeriodRange(MIN_THAWING_PERIOD, type(uint64).max);
        _setVerifierCutRange(0, uint32(1_000_000)); // 0–100% in PPM
        _setPauseGuardian(pauseGuardian, true);
    }

    // -------------------------------------------------------------------------
    // UUPS
    // -------------------------------------------------------------------------

    /// @notice Authorise a contract upgrade. Owner-only.
    function _authorizeUpgrade(address) internal override onlyOwner {}

    // -------------------------------------------------------------------------
    // Governance
    // -------------------------------------------------------------------------

    /// @inheritdoc IRPCDataService
    function addChain(uint256 chainId, uint256 minProvisionTokens) external onlyOwner {
        supportedChains[chainId] = ChainConfig({
            enabled: true, minProvisionTokens: minProvisionTokens == 0 ? DEFAULT_MIN_PROVISION : minProvisionTokens
        });
        emit ChainAdded(chainId, minProvisionTokens == 0 ? DEFAULT_MIN_PROVISION : minProvisionTokens);
    }

    /// @inheritdoc IRPCDataService
    function removeChain(uint256 chainId) external onlyOwner {
        supportedChains[chainId].enabled = false;
        emit ChainRemoved(chainId);
    }

    /// @inheritdoc IRPCDataService
    function setDefaultMinProvision(uint256 tokens) external onlyOwner {
        // No storage variable — default is applied per-chain via addChain.
        // Emit for indexer tooling awareness (chainId=0 signals "default").
        emit ChainAdded(0, tokens);
    }

    /// @inheritdoc IRPCDataService
    function setMinThawingPeriod(uint64 period) external onlyOwner {
        if (period < MIN_THAWING_PERIOD) revert ThawingPeriodTooShort(MIN_THAWING_PERIOD, period);
        minThawingPeriod = period;
        emit MinThawingPeriodSet(period);
    }

    // -------------------------------------------------------------------------
    // IDataService — lifecycle
    // -------------------------------------------------------------------------

    /// @notice Register as an RPC provider.
    /// @param serviceProvider The provider's address.
    /// @param data ABI-encoded (string endpoint, string geoHash, address paymentsDestination).
    function register(address serviceProvider, bytes calldata data)
        external
        override
        whenNotPaused
        onlyAuthorizedForProvision(serviceProvider)
    {
        if (registeredProviders[serviceProvider]) {
            revert ProviderAlreadyRegistered(serviceProvider);
        }

        // Validate provision meets protocol minimums (uses ranges set in initialize).
        _checkProvisionTokens(serviceProvider);
        _checkProvisionParameters(serviceProvider, false);

        (string memory endpoint, string memory geoHash, address dest) = abi.decode(data, (string, string, address));
        registeredProviders[serviceProvider] = true;
        paymentsDestination[serviceProvider] = dest == address(0) ? serviceProvider : dest;

        emit ProviderRegistered(serviceProvider, endpoint, geoHash);
    }

    /// @notice Deregister as an RPC provider.
    /// @dev All chain registrations must be stopped first.
    function deregister(address serviceProvider, bytes calldata) external onlyAuthorizedForProvision(serviceProvider) {
        if (!registeredProviders[serviceProvider]) revert ProviderNotRegistered(serviceProvider);
        if (activeRegistrationCount(serviceProvider) > 0) revert ActiveRegistrationsExist(serviceProvider);

        registeredProviders[serviceProvider] = false;
        emit ProviderDeregistered(serviceProvider);
    }

    /// @inheritdoc IRPCDataService
    function setPaymentsDestination(address destination) external {
        if (!registeredProviders[msg.sender]) revert ProviderNotRegistered(msg.sender);
        address dest = destination == address(0) ? msg.sender : destination;
        paymentsDestination[msg.sender] = dest;
        emit PaymentsDestinationSet(msg.sender, dest);
    }

    /// @notice Activate RPC service for a specific chain and capability tier.
    /// @param serviceProvider The provider's address.
    /// @param data ABI-encoded (uint64 chainId, uint8 tier, string endpoint).
    function startService(address serviceProvider, bytes calldata data)
        external
        override
        whenNotPaused
        onlyAuthorizedForProvision(serviceProvider)
    {
        if (!registeredProviders[serviceProvider]) revert ProviderNotRegistered(serviceProvider);

        (uint64 chainId, uint8 tier, string memory endpoint) = abi.decode(data, (uint64, uint8, string));

        ChainConfig storage cfg = supportedChains[chainId];
        if (!cfg.enabled) revert ChainNotSupported(chainId);

        // Validate provision meets the per-chain minimum.
        IHorizonStaking.Provision memory provision = _graphStaking().getProvision(serviceProvider, address(this));
        if (provision.tokens < cfg.minProvisionTokens) {
            revert InsufficientProvision(cfg.minProvisionTokens, provision.tokens);
        }

        // Reactivate an existing (stopped) entry if one exists rather than pushing a new
        // one, so the _providerChains array does not grow without bound across start/stop
        // cycles and activeRegistrationCount() stays gas-bounded.
        ChainRegistration[] storage regs = _providerChains[serviceProvider];
        for (uint256 i = 0; i < regs.length; i++) {
            if (regs[i].chainId == chainId && uint8(regs[i].tier) == tier) {
                regs[i].active = true;
                regs[i].endpoint = endpoint;
                emit ServiceStarted(serviceProvider, chainId, CapabilityTier(tier), endpoint);
                return;
            }
        }

        regs.push(ChainRegistration({chainId: chainId, tier: CapabilityTier(tier), endpoint: endpoint, active: true}));
        emit ServiceStarted(serviceProvider, chainId, CapabilityTier(tier), endpoint);
    }

    /// @notice Deactivate RPC service for a specific chain and tier.
    /// @param serviceProvider The provider's address.
    /// @param data ABI-encoded (uint64 chainId, uint8 tier).
    function stopService(address serviceProvider, bytes calldata data)
        external
        override
        onlyAuthorizedForProvision(serviceProvider)
    {
        (uint64 chainId, uint8 tier) = abi.decode(data, (uint64, uint8));

        ChainRegistration[] storage regs = _providerChains[serviceProvider];
        for (uint256 i = 0; i < regs.length; i++) {
            if (regs[i].chainId == chainId && uint8(regs[i].tier) == tier && regs[i].active) {
                regs[i].active = false;
                emit ServiceStopped(serviceProvider, chainId, CapabilityTier(tier));
                return;
            }
        }
        revert RegistrationNotFound(serviceProvider, chainId, CapabilityTier(tier));
    }

    /// @notice Collect fees by submitting a signed Receipt Aggregate Voucher (RAV).
    ///
    /// Flow:
    ///   RPCDataService.collect() → GraphTallyCollector.collect()
    ///     → PaymentsEscrow.collect() → GraphPayments.collect()
    ///     → distributes: protocol tax → data service cut → delegator cut → provider
    ///
    /// @param serviceProvider The provider collecting fees.
    /// @param paymentType Must be QueryFee.
    /// @param data ABI-encoded (SignedRAV, tokensToCollect).
    /// @return fees Total GRT collected by the service provider.
    function collect(address serviceProvider, IGraphPayments.PaymentTypes paymentType, bytes calldata data)
        external
        override
        whenNotPaused
        returns (uint256 fees)
    {
        if (paymentType != IGraphPayments.PaymentTypes.QueryFee) revert InvalidPaymentType();

        if (!registeredProviders[serviceProvider]) revert ProviderNotRegistered(serviceProvider);

        (IGraphTallyCollector.SignedRAV memory signedRav, uint256 tokensToCollect) =
            abi.decode(data, (IGraphTallyCollector.SignedRAV, uint256));

        if (signedRav.rav.serviceProvider != serviceProvider) {
            revert InvalidServiceProvider(serviceProvider, signedRav.rav.serviceProvider);
        }

        // Release any expired stake claims before locking new ones.
        _releaseStake(serviceProvider, 0);

        // Lock before the external call. This mirrors the collector's own amount rule, and the
        // result is checked against what it actually collected below.
        uint256 expectedFees = tokensToCollect;
        if (expectedFees == 0) {
            uint256 alreadyCollected = IGraphTallyCollectorLedger(address(GRAPH_TALLY_COLLECTOR))
                .tokensCollected(address(this), signedRav.rav.collectionId, serviceProvider, signedRav.rav.payer);
            if (signedRav.rav.valueAggregate > alreadyCollected) {
                expectedFees = signedRav.rav.valueAggregate - alreadyCollected;
            }
        }
        if (expectedFees > 0) {
            // Released after the dispute window.
            _lockStake(serviceProvider, expectedFees * STAKE_TO_FEES_RATIO, block.timestamp + minThawingPeriod);
        }

        // Collect via GraphTallyCollector → PaymentsEscrow → GraphPayments.
        // The RAV's dataService field must equal address(this) — enforced by GraphTallyCollector.
        // Fees flow to paymentsDestination[serviceProvider], not necessarily serviceProvider itself.
        // 2% total is routed to this contract as the data service cut: 1% burned, 1% retained as revenue.
        uint256 balanceBefore = _graphToken().balanceOf(address(this));
        fees = GRAPH_TALLY_COLLECTOR.collect(
            paymentType,
            abi.encode(signedRav, BURN_CUT_PPM + DATA_SERVICE_CUT_PPM, paymentsDestination[serviceProvider]),
            tokensToCollect
        );

        if (fees != expectedFees) revert UnexpectedCollectedAmount(expectedFees, fees);

        uint256 received = _graphToken().balanceOf(address(this)) - balanceBefore;
        if (received > 0) {
            uint256 burned = received * BURN_CUT_PPM / (BURN_CUT_PPM + DATA_SERVICE_CUT_PPM);
            _graphToken().burn(burned);
            emit FeesBurned(serviceProvider, burned);
        }
    }

    /// @notice Slash is not implemented — this data service does not support on-chain dispute slashing.
    function slash(address, bytes calldata) external pure override {
        revert("slashing not supported");
    }

    /// @notice Accept pending changes to this provider's provision parameters.
    /// @dev Two-step process: provider calls HorizonStaking.setProvisionParameters,
    ///      then this function accepts the queued change.
    function acceptProvisionPendingParameters(address serviceProvider, bytes calldata)
        external
        override
        onlyAuthorizedForProvision(serviceProvider)
    {
        _acceptProvisionParameters(serviceProvider);
    }

    // -------------------------------------------------------------------------
    // IRPCDataService views
    // -------------------------------------------------------------------------

    /// @inheritdoc IRPCDataService
    function isRegistered(address provider) external view override returns (bool) {
        return registeredProviders[provider];
    }

    /// @inheritdoc IRPCDataService
    function getChainRegistrations(address provider) external view override returns (ChainRegistration[] memory) {
        return _providerChains[provider];
    }

    /// @inheritdoc IRPCDataService
    function activeRegistrationCount(address provider) public view override returns (uint256 count) {
        ChainRegistration[] storage regs = _providerChains[provider];
        for (uint256 i = 0; i < regs.length; i++) {
            if (regs[i].active) count++;
        }
    }

    /// @notice Grant or revoke pause guardian status.
    function setPauseGuardian(address guardian, bool allowed) external onlyOwner {
        _setPauseGuardian(guardian, allowed);
    }

    /// @notice Withdraw accumulated data service revenue to `to`.
    function withdrawFees(address to, uint256 amount) external onlyOwner {
        require(to != address(0), "zero address");
        _graphToken().transfer(to, amount);
        emit FeesWithdrawn(to, amount);
    }
}
