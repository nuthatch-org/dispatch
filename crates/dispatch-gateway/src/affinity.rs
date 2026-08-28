//! Provider affinity for stateful JSON-RPC methods.
//!
//! Most JSON-RPC calls are stateless: any provider serving the chain can answer, so the gateway
//! picks whichever currently ranks best on QoS. Filters are not like that. `eth_newFilter` creates
//! state *inside one node* and returns an id that only that node can resolve. Route the follow-up
//! `eth_getFilterChanges` anywhere else and you get "filter not found" from a node that is behaving
//! perfectly — it has simply never heard of your filter.
//!
//! Because provider scores move continuously (latency EMA, block freshness), the follow-up call is
//! *likely* to land somewhere else. So filters are effectively broken without this map, and they
//! fail in the most confusing way available: intermittently, and blaming the wrong node.
//!
//! Entries are pinned to a provider address, expire on a TTL, and are capped in number. Filters
//! expire inside the nodes themselves after a few idle minutes, so a TTL here is not a leak; the
//! cap is there because filter ids are attacker-controllable in volume.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use alloy_primitives::Address;

/// Methods that CREATE node-local state and return a handle to it.
pub fn creates_filter(method: &str) -> bool {
    matches!(
        method,
        "eth_newFilter" | "eth_newBlockFilter" | "eth_newPendingTransactionFilter"
    )
}

/// Methods that USE a handle created earlier. The handle is always the first parameter.
pub fn uses_filter(method: &str) -> bool {
    matches!(
        method,
        "eth_getFilterChanges" | "eth_getFilterLogs" | "eth_uninstallFilter"
    )
}

/// Methods that RELEASE a handle, so its affinity can be dropped.
pub fn releases_filter(method: &str) -> bool {
    method == "eth_uninstallFilter"
}

#[derive(Clone, Copy)]
struct Entry {
    provider: Address,
    inserted: Instant,
}

/// Maps `(chain_id, filter_id)` to the provider that owns it.
pub struct FilterAffinity {
    entries: Mutex<HashMap<(u64, String), Entry>>,
    ttl: Duration,
    capacity: usize,
}

impl FilterAffinity {
    pub fn new(ttl: Duration, capacity: usize) -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            ttl,
            capacity,
        }
    }

    /// Records that `filter_id` on `chain_id` lives on `provider`.
    pub fn remember(&self, chain_id: u64, filter_id: &str, provider: Address) {
        self.remember_at(chain_id, filter_id, provider, Instant::now());
    }

    fn remember_at(&self, chain_id: u64, filter_id: &str, provider: Address, now: Instant) {
        let mut map = self.entries.lock().expect("affinity mutex poisoned");
        map.retain(|_, e| now.duration_since(e.inserted) < self.ttl);
        // Filter ids are attacker-controllable in volume, so refuse rather than grow without
        // bound. Dropping a new entry costs one consumer a broken filter; growing without bound
        // costs everyone the gateway.
        if map.len() >= self.capacity && !map.contains_key(&(chain_id, filter_id.to_string())) {
            return;
        }
        map.insert(
            (chain_id, filter_id.to_string()),
            Entry {
                provider,
                inserted: now,
            },
        );
    }

    /// The provider that owns this filter, if we still know and it has not expired.
    pub fn lookup(&self, chain_id: u64, filter_id: &str) -> Option<Address> {
        self.lookup_at(chain_id, filter_id, Instant::now())
    }

    fn lookup_at(&self, chain_id: u64, filter_id: &str, now: Instant) -> Option<Address> {
        let map = self.entries.lock().expect("affinity mutex poisoned");
        map.get(&(chain_id, filter_id.to_string()))
            .filter(|e| now.duration_since(e.inserted) < self.ttl)
            .map(|e| e.provider)
    }

    /// Drops an entry after `eth_uninstallFilter`.
    pub fn forget(&self, chain_id: u64, filter_id: &str) {
        self.entries
            .lock()
            .expect("affinity mutex poisoned")
            .remove(&(chain_id, filter_id.to_string()));
    }

    pub fn len(&self) -> usize {
        self.entries.lock().expect("affinity mutex poisoned").len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// The filter id from a request's params — always the first positional parameter.
///
/// Takes an `Option` because JSON-RPC params are optional; a filter call without them is
/// malformed and yields `None`, which falls back to ordinary routing and lets the node return its
/// own error rather than the gateway inventing one.
pub fn filter_id_from_params(params: &Option<serde_json::Value>) -> Option<String> {
    params
        .as_ref()?
        .as_array()?
        .first()?
        .as_str()
        .map(str::to_string)
}

/// The filter id a create-call returned, from the JSON-RPC `result`.
pub fn filter_id_from_result(result: &serde_json::Value) -> Option<String> {
    result.as_str().map(str::to_string)
}

/// Which single provider must serve this request, if any.
///
/// `Some(p)` means the request touches node-local state that only `p` holds, so it goes to `p`
/// alone. `None` means ordinary QoS selection applies.
///
/// Returns `None` when the owner is no longer among the capable providers — the provider has gone
/// away, been deregistered, or dropped the tier. Falling back to normal routing there gets a
/// "filter not found" from a live node, which is the truth: the filter really is gone.
pub fn pin_provider<P: Clone>(
    affinity: &FilterAffinity,
    chain_id: u64,
    method: &str,
    params: &Option<serde_json::Value>,
    capable: &[P],
    address_of: impl Fn(&P) -> Address,
) -> Option<P> {
    if !uses_filter(method) {
        return None;
    }
    let id = filter_id_from_params(params)?;
    let owner = affinity.lookup(chain_id, &id)?;
    capable.iter().find(|p| address_of(p) == owner).cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: Address = Address::new([0xaa; 20]);
    const B: Address = Address::new([0xbb; 20]);

    fn affinity() -> FilterAffinity {
        FilterAffinity::new(Duration::from_secs(300), 10_000)
    }

    #[test]
    fn method_classification_covers_the_whole_filter_lifecycle() {
        assert!(creates_filter("eth_newFilter"));
        assert!(creates_filter("eth_newBlockFilter"));
        assert!(creates_filter("eth_newPendingTransactionFilter"));
        assert!(uses_filter("eth_getFilterChanges"));
        assert!(uses_filter("eth_getFilterLogs"));
        assert!(uses_filter("eth_uninstallFilter"));
        assert!(releases_filter("eth_uninstallFilter"));
        // Stateless neighbours that must NOT be pinned.
        for m in ["eth_getLogs", "eth_call", "eth_blockNumber"] {
            assert!(!creates_filter(m) && !uses_filter(m), "{m}");
        }
    }

    #[test]
    fn a_remembered_filter_routes_back_to_the_provider_that_created_it() {
        let a = affinity();
        a.remember(42161, "0x1", A);
        assert_eq!(a.lookup(42161, "0x1"), Some(A));
        assert_ne!(a.lookup(42161, "0x1"), Some(B));
    }

    /// The same filter id on two chains is two different filters on two different nodes.
    #[test]
    fn affinity_is_scoped_per_chain() {
        let a = affinity();
        a.remember(42161, "0x1", A);
        a.remember(1, "0x1", B);
        assert_eq!(a.lookup(42161, "0x1"), Some(A));
        assert_eq!(a.lookup(1, "0x1"), Some(B));
    }

    #[test]
    fn an_unknown_filter_is_none_so_the_caller_falls_back_rather_than_guessing() {
        assert_eq!(affinity().lookup(42161, "0xdeadbeef"), None);
    }

    #[test]
    fn entries_expire_on_the_ttl() {
        let a = FilterAffinity::new(Duration::from_secs(300), 10);
        let t0 = Instant::now();
        a.remember_at(42161, "0x1", A, t0);
        assert_eq!(
            a.lookup_at(42161, "0x1", t0 + Duration::from_secs(299)),
            Some(A)
        );
        assert_eq!(
            a.lookup_at(42161, "0x1", t0 + Duration::from_secs(301)),
            None
        );
    }

    #[test]
    fn uninstalling_forgets_the_entry() {
        let a = affinity();
        a.remember(42161, "0x1", A);
        a.forget(42161, "0x1");
        assert_eq!(a.lookup(42161, "0x1"), None);
        assert!(a.is_empty());
    }

    /// Filter ids are attacker-controllable in volume; the map must not be a memory-exhaustion
    /// vector just because it fixes a routing bug.
    #[test]
    fn the_map_is_capped_and_refuses_rather_than_growing() {
        let a = FilterAffinity::new(Duration::from_secs(300), 3);
        for i in 0..10 {
            a.remember(42161, &format!("0x{i}"), A);
        }
        assert_eq!(a.len(), 3);
    }

    /// A cap that blocked *updates* would strand live filters as soon as it filled.
    #[test]
    fn an_existing_entry_can_still_be_refreshed_at_capacity() {
        let a = FilterAffinity::new(Duration::from_secs(300), 2);
        a.remember(42161, "0x1", A);
        a.remember(42161, "0x2", A);
        a.remember(42161, "0x1", B);
        assert_eq!(a.lookup(42161, "0x1"), Some(B));
        assert_eq!(a.len(), 2);
    }

    /// Expiry must reclaim room, or a busy hour permanently wedges the map at capacity.
    #[test]
    fn expiry_frees_capacity_for_new_filters() {
        let a = FilterAffinity::new(Duration::from_secs(300), 2);
        let t0 = Instant::now();
        a.remember_at(42161, "0x1", A, t0);
        a.remember_at(42161, "0x2", A, t0);
        a.remember_at(42161, "0x3", A, t0 + Duration::from_secs(301));
        assert_eq!(
            a.lookup_at(42161, "0x3", t0 + Duration::from_secs(301)),
            Some(A)
        );
    }

    #[test]
    fn filter_ids_are_read_from_the_first_param_and_the_bare_result() {
        let params = Some(serde_json::json!(["0xabc", "ignored"]));
        assert_eq!(filter_id_from_params(&params), Some("0xabc".into()));
        assert_eq!(filter_id_from_params(&Some(serde_json::json!([]))), None);
        assert_eq!(
            filter_id_from_params(&Some(serde_json::json!([1234]))),
            None
        );
        assert_eq!(
            filter_id_from_params(&None),
            None,
            "absent params must not panic"
        );
        assert_eq!(
            filter_id_from_result(&serde_json::json!("0xabc")),
            Some("0xabc".into())
        );
        assert_eq!(filter_id_from_result(&serde_json::json!({"a": 1})), None);
    }

    // --- pin_provider: the decision the router actually makes -------------------------------

    #[derive(Clone, Debug, PartialEq)]
    struct P(Address);
    fn addr_of(p: &P) -> Address {
        p.0
    }

    fn params(id: &str) -> Option<serde_json::Value> {
        Some(serde_json::json!([id]))
    }

    #[test]
    fn a_filter_follow_up_is_pinned_to_its_owner_not_the_best_ranked_provider() {
        let a = affinity();
        a.remember(42161, "0x1", B);
        // B is deliberately LAST in the candidate list, i.e. the worst QoS score.
        let capable = vec![P(A), P(B)];
        let pinned = pin_provider(
            &a,
            42161,
            "eth_getFilterChanges",
            &params("0x1"),
            &capable,
            addr_of,
        );
        assert_eq!(
            pinned,
            Some(P(B)),
            "must follow the filter, not the ranking"
        );
    }

    #[test]
    fn stateless_methods_are_never_pinned() {
        let a = affinity();
        a.remember(42161, "0x1", B);
        for m in [
            "eth_getLogs",
            "eth_call",
            "eth_blockNumber",
            "eth_newFilter",
        ] {
            assert_eq!(
                pin_provider(&a, 42161, m, &params("0x1"), &[P(A), P(B)], addr_of),
                None,
                "{m} must go through normal selection"
            );
        }
    }

    #[test]
    fn an_unknown_filter_falls_back_to_normal_selection() {
        let a = affinity();
        let capable = vec![P(A), P(B)];
        assert_eq!(
            pin_provider(
                &a,
                42161,
                "eth_getFilterChanges",
                &params("0xnope"),
                &capable,
                addr_of
            ),
            None
        );
    }

    /// The owner going away must not pin to nothing and must not pin to a stranger; it falls back,
    /// and the live node truthfully reports the filter as missing.
    #[test]
    fn a_departed_owner_falls_back_rather_than_pinning_to_someone_else() {
        let a = affinity();
        a.remember(42161, "0x1", B);
        let capable = vec![P(A)]; // B is gone
        assert_eq!(
            pin_provider(
                &a,
                42161,
                "eth_getFilterChanges",
                &params("0x1"),
                &capable,
                addr_of
            ),
            None
        );
    }

    #[test]
    fn a_filter_call_with_no_params_falls_back_instead_of_panicking() {
        let a = affinity();
        a.remember(42161, "0x1", B);
        assert_eq!(
            pin_provider(
                &a,
                42161,
                "eth_getFilterChanges",
                &None,
                &[P(A), P(B)],
                addr_of
            ),
            None
        );
    }

    #[test]
    fn uninstall_is_pinned_too_or_it_would_uninstall_on_the_wrong_node() {
        let a = affinity();
        a.remember(42161, "0x1", B);
        assert_eq!(
            pin_provider(
                &a,
                42161,
                "eth_uninstallFilter",
                &params("0x1"),
                &[P(A), P(B)],
                addr_of
            ),
            Some(P(B))
        );
    }
}
