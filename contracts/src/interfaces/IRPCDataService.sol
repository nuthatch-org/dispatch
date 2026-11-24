// SPDX-License-Identifier: Apache-2.0
pragma solidity ^0.8.27;

/// @title IRPCDataService
/// @notice Interface for the Dispatch data service on The Graph Protocol's Horizon framework.
///
/// A provider (indexer) lifecycle:
///   register → startService (per chain/tier) → [collect]* → stopService → deregister
///
/// Provisions are managed via HorizonStaking: the provider calls
/// HorizonStaking.provision(provider, RPCDataService, tokens, maxVerifierCut, thawingPeriod)
/// before registering here.
interface IRPCDataService {
    // -------------------------------------------------------------------------
    // Types
    // -------------------------------------------------------------------------

    /// Capability tiers determine what infrastructure and methods a provider offers.
    enum CapabilityTier {
        Standard, // 0 — full node, last 128 blocks of state
        Archive, // 1 — full historical state
        Debug, // 2 — debug_* and trace_* methods
        WebSocket // 3 — real-time eth_subscribe
    }

    struct ChainConfig {
        bool enabled;
        uint256 minProvisionTokens;
    }

    struct ChainRegistration {
        uint64 chainId;
        CapabilityTier tier;
        string endpoint;
        bool active;
    }

    // -------------------------------------------------------------------------
    // Events
    // -------------------------------------------------------------------------

    event ChainAdded(uint256 indexed chainId, uint256 minProvisionTokens);
    event ChainRemoved(uint256 indexed chainId);
    event MinThawingPeriodSet(uint64 period);
    event ProviderRegistered(address indexed provider, string endpoint, string geoHash);
    event ProviderDeregistered(address indexed provider);
    event PaymentsDestinationSet(address indexed provider, address indexed destination);
    event ServiceStarted(address indexed provider, uint64 indexed chainId, CapabilityTier tier, string endpoint);
    event ServiceStopped(address indexed provider, uint64 indexed chainId, CapabilityTier tier);
    event FeesBurned(address indexed provider, uint256 amount);
    event FeesWithdrawn(address indexed to, uint256 amount);

    // -------------------------------------------------------------------------
    // Errors
    // -------------------------------------------------------------------------

    error ChainNotSupported(uint256 chainId);
    error ChainAlreadySupported(uint256 chainId);
    error ProviderAlreadyRegistered(address provider);
    error ProviderNotRegistered(address provider);
    error ActiveRegistrationsExist(address provider);
    error InsufficientProvision(uint256 required, uint256 actual);
    error ThawingPeriodTooShort(uint64 required, uint64 actual);
    error RegistrationNotFound(address provider, uint64 chainId, CapabilityTier tier);
    error InvalidServiceProvider(address expected, address actual);
    error InvalidPaymentType();
    error UnexpectedCollectedAmount(uint256 expected, uint256 actual);

    // -------------------------------------------------------------------------
    // Governance (owner-only)
    // -------------------------------------------------------------------------

    /// @notice Add a chain to the supported set.
    /// @param chainId EIP-155 chain ID.
    /// @param minProvisionTokens Minimum GRT stake required. Pass 0 to use the protocol default.
    function addChain(uint256 chainId, uint256 minProvisionTokens) external;

    /// @notice Remove a chain from the supported set. Existing registrations are unaffected
    ///         until providers call stopService.
    function removeChain(uint256 chainId) external;

    /// @notice Update the default minimum provision tokens.
    function setDefaultMinProvision(uint256 tokens) external;

    /// @notice Update the minimum thawing period.
    function setMinThawingPeriod(uint64 period) external;

    // -------------------------------------------------------------------------
    // Provider operations
    // -------------------------------------------------------------------------

    /// @notice Update the address that receives collected GRT fees.
    /// @dev Defaults to serviceProvider at registration.
    function setPaymentsDestination(address destination) external;

    // -------------------------------------------------------------------------
    // Provider views
    // -------------------------------------------------------------------------

    function isRegistered(address provider) external view returns (bool);

    function getChainRegistrations(address provider) external view returns (ChainRegistration[] memory);

    function activeRegistrationCount(address provider) external view returns (uint256);

    function paymentsDestination(address provider) external view returns (address);

    function minThawingPeriod() external view returns (uint64);
}
