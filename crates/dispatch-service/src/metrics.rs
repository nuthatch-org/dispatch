//! Prometheus metrics for the provider: what it served, what it was paid, what it refused.

use lazy_static::lazy_static;
use prometheus::{
    register_counter_vec, register_gauge_vec, register_histogram_vec, CounterVec, Encoder,
    GaugeVec, HistogramVec, TextEncoder,
};

lazy_static! {
    pub static ref REQUESTS_TOTAL: CounterVec = register_counter_vec!(
        "dispatch_service_requests_total",
        "Paid requests forwarded to the node, by outcome",
        &["chain_id", "outcome"]
    )
    .unwrap();
    pub static ref REQUEST_DURATION: HistogramVec = register_histogram_vec!(
        "dispatch_service_request_duration_seconds",
        "Time to forward a paid request to the node and get its answer",
        &["chain_id"],
        vec![0.01, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0]
    )
    .unwrap();
    pub static ref RECEIPT_GRT_TOTAL: CounterVec = register_counter_vec!(
        "dispatch_service_receipt_grt_total",
        "GRT carried by accepted receipts, before aggregation and collection",
        &["chain_id"]
    )
    .unwrap();
    pub static ref REJECTIONS_TOTAL: CounterVec = register_counter_vec!(
        "dispatch_service_rejections_total",
        "Requests refused, by reason",
        &["reason"]
    )
    .unwrap();
    pub static ref RAVS_TOTAL: CounterVec = register_counter_vec!(
        "dispatch_service_ravs_total",
        "RAV aggregation attempts per payer, by outcome",
        &["outcome"]
    )
    .unwrap();
    pub static ref COLLECTS_TOTAL: CounterVec = register_counter_vec!(
        "dispatch_service_collects_total",
        "On-chain collect() attempts, by outcome",
        &["outcome"]
    )
    .unwrap();
    /// RAVs are cumulative, so this is the latest redeemed value per collection, not a sum of
    /// redemptions. Summed over collections it is everything this provider has collected.
    pub static ref REDEEMED_GRT: GaugeVec = register_gauge_vec!(
        "dispatch_service_redeemed_grt",
        "Cumulative GRT redeemed on-chain, per collection",
        &["collection_id"]
    )
    .unwrap();
}

pub fn grt(wei: u128) -> f64 {
    wei as f64 / 1e18
}

pub fn render() -> Vec<u8> {
    let mut buf = Vec::new();
    TextEncoder::new()
        .encode(&prometheus::gather(), &mut buf)
        .unwrap_or_default();
    buf
}
