//! Dynamic provider discovery from `RPCDataService`'s own events: read straight from the chain, or
//! from a nuthatch nest that indexes them. Rebuilt every interval; a provider that deregisters or
//! stops its last service drops out.

use std::sync::Arc;

use tokio::time::{interval, Duration, MissedTickBehavior};

use crate::{
    chain_discovery::{ChainScanner, NestSource},
    config::DiscoverySource,
    server::AppState,
};

// ---------------------------------------------------------------------------
// Discovery loop
// ---------------------------------------------------------------------------

pub async fn run(state: AppState) {
    let cfg = match &state.config.discovery {
        Some(c) => c.clone(),
        None => return, // No discovery configured: static providers only.
    };

    let mut tick = interval(Duration::from_secs(cfg.interval_secs));
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);

    enum Source {
        Chain(ChainScanner),
        Nest(NestSource),
    }
    let mut source = match cfg.source {
        DiscoverySource::Nest => match &cfg.nest_url {
            Some(url) => Source::Nest(NestSource::new(url)),
            None => {
                tracing::error!("discovery source is nest but no nest_url is set");
                return;
            }
        },
        DiscoverySource::Chain => match state.config.arbitrum_rpc_url() {
            Some(url) => Source::Chain(ChainScanner::new(
                url.to_string(),
                state.config.tap.data_service_address,
                cfg.start_block
                    .unwrap_or(crate::chain_discovery::DEFAULT_START_BLOCK),
            )),
            None => {
                tracing::error!("discovery source is chain but no [chain] arbitrum_rpc_url is set");
                return;
            }
        },
    };

    tracing::info!(source = ?cfg.source, interval_secs = cfg.interval_secs, "discovery task started");

    loop {
        tick.tick().await;

        let fetched = match &mut source {
            Source::Chain(s) => s.poll(&state.http_client).await,
            Source::Nest(n) => n.poll(&state.http_client).await,
        };
        match fetched {
            Ok(mut providers) if !providers.is_empty() => {
                // If a static provider entry exists for the same address:
                //  - Override the endpoint (Docker internal hostname instead of public URL).
                //  - Merge any chain capabilities declared in static config but absent from
                //    the registry. This lets operators extend a provider's chain list before
                //    the on-chain startService() call is seen by discovery.
                for p in &mut providers {
                    if let Some(s) = state
                        .config
                        .providers
                        .iter()
                        .find(|s| s.address == p.address)
                    {
                        p.price_floor = s.price_floor;
                        if s.endpoint != p.endpoint {
                            tracing::debug!(
                                address = %p.address,
                                static_endpoint = %s.endpoint,
                                discovered_endpoint = %p.endpoint,
                                "using static endpoint override for provider"
                            );
                            p.endpoint = s.endpoint.clone();
                        }

                        // Derive static chain_capabilities (chains × capabilities when
                        // per-chain map is absent, which is the usual gateway.toml format).
                        let static_caps: std::collections::HashMap<
                            u64,
                            Vec<crate::config::CapabilityTier>,
                        > = if s.chain_capabilities.is_empty() {
                            s.chains
                                .iter()
                                .map(|&id| (id, s.capabilities.clone()))
                                .collect()
                        } else {
                            s.chain_capabilities.clone()
                        };

                        for (chain_id, tiers) in static_caps {
                            let entry = p.chain_capabilities.entry(chain_id).or_default();
                            for tier in tiers {
                                if !entry.contains(&tier) {
                                    tracing::debug!(
                                        address = %p.address,
                                        chain_id,
                                        tier = ?tier,
                                        "supplementing discovered capabilities with a static one"
                                    );
                                    entry.push(tier);
                                }
                            }
                            if !p.chains.contains(&chain_id) {
                                p.chains.push(chain_id);
                            }
                        }
                    }
                }
                let new_registry = state.registry.load().refreshed(&providers);
                state.registry.store(Arc::new(new_registry));
                tracing::info!(count = providers.len(), "provider registry refreshed");
            }
            Ok(_) => tracing::warn!("discovery found no active providers"),
            Err(e) => tracing::warn!(error = %e, "provider discovery failed"),
        }
    }
}
