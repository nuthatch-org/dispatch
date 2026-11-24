//! Weighted random provider selection.
//!
//! Higher QoS score → proportionally more traffic, but lower-scored providers
//! still receive some requests — enabling new provider discovery.
//!
//! Returns up to `k` providers for concurrent dispatch (first response wins).

use std::sync::Arc;

use rand::Rng;

use crate::registry::Provider;

/// Select up to `k` providers by QoS-weighted random sampling without replacement.
///
/// Providers with zero score are included with a small floor weight so they
/// can be probed and potentially graduate to higher weights.
///
/// `gateway_region` and `region_bonus`: when a provider's region matches the
/// gateway's region, `region_bonus` is added to its weight — preferring nearby
/// providers before latency data is established without hard-excluding others.
pub fn select(
    providers: &[Arc<Provider>],
    chain_head: u64,
    k: usize,
    gateway_region: Option<&str>,
    region_bonus: f64,
) -> Vec<Arc<Provider>> {
    if providers.is_empty() {
        return vec![];
    }

    let k = k.min(providers.len());
    let mut remaining: Vec<(usize, f64)> = providers
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let base = score_with_floor(p.qos.score(chain_head));
            let bonus = match (gateway_region, p.region.as_deref()) {
                (Some(gw), Some(pv)) if gw == pv => region_bonus,
                _ => 0.0,
            };
            (i, base + bonus)
        })
        .collect();

    let mut selected = Vec::with_capacity(k);
    let mut rng = rand::thread_rng();

    for _ in 0..k {
        let total: f64 = remaining.iter().map(|(_, w)| w).sum();
        if total <= 0.0 {
            break;
        }

        let threshold = rng.gen::<f64>() * total;
        let mut cumulative = 0.0;
        let mut chosen = 0;

        for (j, (_, weight)) in remaining.iter().enumerate() {
            cumulative += weight;
            if cumulative >= threshold {
                chosen = j;
                break;
            }
        }

        let (idx, _) = remaining.remove(chosen);
        selected.push(providers[idx].clone());
    }

    selected
}

/// Apply a small floor weight so new/unproven providers still receive traffic.
fn score_with_floor(score: f64) -> f64 {
    score.max(0.05) // 5% floor
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{qos::ProviderQos, registry::Provider};
    use alloy_primitives::Address;

    fn make_provider(endpoint: &str) -> Arc<Provider> {
        Arc::new(Provider {
            address: Address::ZERO,
            endpoint: endpoint.to_string(),
            chains: vec![1],
            region: None,
            chain_capabilities: std::collections::HashMap::new(),
            price_floor: None,
            qos: Arc::new(ProviderQos::default()),
            probes: Arc::default(),
        })
    }

    fn make_provider_with_region(endpoint: &str, region: &str) -> Arc<Provider> {
        Arc::new(Provider {
            address: Address::ZERO,
            endpoint: endpoint.to_string(),
            chains: vec![1],
            region: Some(region.to_string()),
            chain_capabilities: std::collections::HashMap::new(),
            price_floor: None,
            qos: Arc::new(ProviderQos::default()),
            probes: Arc::default(),
        })
    }

    #[test]
    fn select_returns_at_most_k() {
        let providers: Vec<_> = (0..5)
            .map(|i| make_provider(&format!("http://p{i}")))
            .collect();
        let selected = select(&providers, 1000, 3, None, 0.15);
        assert_eq!(selected.len(), 3);
    }

    #[test]
    fn select_no_duplicates() {
        let providers: Vec<_> = (0..5)
            .map(|i| make_provider(&format!("http://p{i}")))
            .collect();
        let selected = select(&providers, 1000, 5, None, 0.15);
        assert_eq!(selected.len(), 5);
        // All endpoints distinct
        let endpoints: std::collections::HashSet<_> =
            selected.iter().map(|p| &p.endpoint).collect();
        assert_eq!(endpoints.len(), 5);
    }

    #[test]
    fn select_empty_providers_returns_empty() {
        let selected = select(&[], 0, 3, None, 0.15);
        assert!(selected.is_empty());
    }

    #[test]
    fn select_k_larger_than_providers() {
        let providers: Vec<_> = (0..2)
            .map(|i| make_provider(&format!("http://p{i}")))
            .collect();
        let selected = select(&providers, 1000, 10, None, 0.15);
        assert_eq!(selected.len(), 2);
    }

    #[test]
    fn region_bonus_boosts_local_provider() {
        // One local provider (low QoS), one remote provider (same low QoS).
        // With a high region_bonus the local one should dominate selection.
        let local = make_provider_with_region("http://local", "eu-west");
        let remote = make_provider_with_region("http://remote", "us-east");
        let providers = vec![local, remote];

        let mut local_count = 0usize;
        for _ in 0..200 {
            let selected = select(&providers, 1000, 1, Some("eu-west"), 10.0);
            if selected[0].endpoint == "http://local" {
                local_count += 1;
            }
        }
        // With bonus=10 vs floor=0.05, local provider should win overwhelmingly.
        assert!(local_count > 150, "local_count={local_count}");
    }
}
