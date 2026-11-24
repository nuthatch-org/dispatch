//! Provider registry — maps chain IDs to available providers.
//!
//! Populated at startup from static TOML config and refreshed at runtime
//! via subgraph-based dynamic discovery.

use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::Instant,
};

use alloy_primitives::Address;

use crate::{
    config::{CapabilityTier, ProviderConfig},
    qos::ProviderQos,
};

/// A registered RPC provider.
#[derive(Debug)]
pub struct Provider {
    pub address: Address,
    pub endpoint: String,
    pub chains: Vec<u64>,
    pub region: Option<String>,
    /// Per-chain capability map used for tier-aware routing.
    /// Key = chain ID; value = tiers this provider supports on that chain.
    pub chain_capabilities: HashMap<u64, Vec<CapabilityTier>>,
    pub price_floor: Option<crate::pricing::PriceFloor>,
    /// Shared with the provider's entry in the next registry, so history survives discovery.
    pub qos: Arc<ProviderQos>,
    pub probes: Arc<Mutex<HashMap<u64, ProbeRecord>>>,
}

/// The latest synthetic probe of one provider on one chain.
#[derive(Debug, Default, Clone)]
pub struct ProbeRecord {
    pub last_ok: Option<Instant>,
    pub block: Option<u64>,
    pub consecutive_failures: u32,
}

impl Provider {
    fn from_config(cfg: &ProviderConfig) -> Self {
        // If dynamic discovery populated per-chain capabilities, use them.
        // Otherwise derive from the global capabilities × declared chains.
        let chain_capabilities = if cfg.chain_capabilities.is_empty() {
            cfg.chains
                .iter()
                .map(|&id| (id, cfg.capabilities.clone()))
                .collect()
        } else {
            cfg.chain_capabilities.clone()
        };
        Self {
            address: cfg.address,
            endpoint: cfg.endpoint.trim_end_matches('/').to_string(),
            chains: cfg.chains.clone(),
            region: cfg.region.clone(),
            chain_capabilities,
            price_floor: cfg.price_floor,
            qos: Arc::default(),
            probes: Arc::default(),
        }
    }
}

/// Chain-level shared state (chain head block number, updated by probes).
#[derive(Debug, Default)]
pub struct ChainState {
    pub head: AtomicU64,
}

impl ChainState {
    /// Update the known chain head. Only stores if the new value is higher.
    pub fn update_head(&self, block: u64) {
        let current = self.head.load(Ordering::Relaxed);
        if block > current {
            self.head.store(block, Ordering::Relaxed);
        }
    }
}

/// The provider registry, shared across all request handlers and the probe task.
#[derive(Debug)]
pub struct Registry {
    /// All providers indexed for fast iteration during probing.
    all: Vec<Arc<Provider>>,
    /// chain_id → list of providers serving that chain.
    by_chain: HashMap<u64, Vec<Arc<Provider>>>,
    /// chain_id → shared chain state (head block).
    chain_state: HashMap<u64, Arc<ChainState>>,
}

impl Registry {
    pub fn from_config(providers: &[ProviderConfig]) -> Self {
        Self::build(providers, None)
    }

    /// A registry for a fresh provider list that keeps the QoS, probe history and chain heads
    /// already learned for providers and chains it still contains.
    pub fn refreshed(&self, providers: &[ProviderConfig]) -> Self {
        Self::build(providers, Some(self))
    }

    fn build(providers: &[ProviderConfig], previous: Option<&Registry>) -> Self {
        let all: Vec<Arc<Provider>> = providers
            .iter()
            .map(|c| {
                let mut p = Provider::from_config(c);
                if let Some(old) =
                    previous.and_then(|r| r.all.iter().find(|o| o.address == c.address))
                {
                    p.qos = old.qos.clone();
                    p.probes = old.probes.clone();
                }
                Arc::new(p)
            })
            .collect();

        let mut by_chain: HashMap<u64, Vec<Arc<Provider>>> = HashMap::new();
        let mut chain_state: HashMap<u64, Arc<ChainState>> = HashMap::new();

        for provider in &all {
            for &chain_id in &provider.chains {
                by_chain.entry(chain_id).or_default().push(provider.clone());
                chain_state.entry(chain_id).or_insert_with(|| {
                    previous
                        .and_then(|r| r.chain_state.get(&chain_id).cloned())
                        .unwrap_or_default()
                });
            }
        }

        Self {
            all,
            by_chain,
            chain_state,
        }
    }

    /// All registered providers (for probing).
    pub fn all_providers(&self) -> &[Arc<Provider>] {
        &self.all
    }

    /// Providers registered to serve a given chain, with their chain head.
    /// Returns `None` if the chain is unknown.
    pub fn providers_for_chain(&self, chain_id: u64) -> Option<(&[Arc<Provider>], u64)> {
        let providers = self.by_chain.get(&chain_id)?;
        let head = self
            .chain_state
            .get(&chain_id)
            .map(|s| s.head.load(Ordering::Relaxed))
            .unwrap_or(0);
        Some((providers, head))
    }

    pub fn chain_state(&self, chain_id: u64) -> Option<Arc<ChainState>> {
        self.chain_state.get(&chain_id).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(byte: u8, chains: Vec<u64>) -> ProviderConfig {
        toml::from_str(&format!(
            "address = \"{:?}\"\nendpoint = \"http://p{byte}\"\nchains = {chains:?}",
            alloy_primitives::Address::repeat_byte(byte)
        ))
        .unwrap()
    }

    #[test]
    fn refresh_keeps_history_for_providers_and_chains_that_remain() {
        let old = Registry::from_config(&[cfg(1, vec![1]), cfg(2, vec![1])]);
        old.all[0].qos.record_failure();
        old.chain_state(1).unwrap().update_head(500);

        let new = old.refreshed(&[cfg(1, vec![1, 10]), cfg(3, vec![1])]);
        assert!(Arc::ptr_eq(&new.all[0].qos, &old.all[0].qos));
        assert!(!Arc::ptr_eq(&new.all[1].qos, &old.all[1].qos));
        assert_eq!(new.providers_for_chain(1).unwrap().1, 500);
        assert_eq!(new.providers_for_chain(10).unwrap().1, 0);
    }
}
