//! Prometheus metrics for the gateway.

use lazy_static::lazy_static;
use prometheus::{
    register_counter_vec, register_histogram_vec, CounterVec, Encoder, HistogramVec, TextEncoder,
};

lazy_static! {
    pub static ref REQUESTS_TOTAL: CounterVec = register_counter_vec!(
        "dispatch_gateway_requests_total",
        "Total RPC requests handled by the gateway",
        &["chain_id", "method", "outcome"]
    )
    .unwrap();
    pub static ref REQUEST_DURATION: HistogramVec = register_histogram_vec!(
        "dispatch_gateway_request_duration_seconds",
        "RPC request round-trip duration",
        &["chain_id", "method"],
        vec![0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0]
    )
    .unwrap();
    pub static ref ATTESTATIONS_TOTAL: CounterVec = register_counter_vec!(
        "dispatch_gateway_attestations_total",
        "Provider responses by attestation outcome: bound, unbound or missing",
        &["provider", "outcome"]
    )
    .unwrap();
    pub static ref KEY_REQUESTS_TOTAL: CounterVec = register_counter_vec!(
        "dispatch_gateway_key_requests_total",
        "Requests served per API key",
        &["key", "chain_id"]
    )
    .unwrap();
    pub static ref KEY_CU_TOTAL: CounterVec = register_counter_vec!(
        "dispatch_gateway_key_cu_total",
        "Compute units charged per API key",
        &["key", "chain_id"]
    )
    .unwrap();
}

/// Meter one served request against the caller.
pub fn record_usage(key: &str, chain_id: u64, cu: u32) {
    let chain = chain_id.to_string();
    KEY_REQUESTS_TOTAL.with_label_values(&[key, &chain]).inc();
    KEY_CU_TOTAL
        .with_label_values(&[key, &chain])
        .inc_by(cu as f64);
}

/// Increment the request counter and observe the duration.
pub fn record(chain_id: u64, method: &str, outcome: &str, duration_secs: f64) {
    let chain = chain_id.to_string();
    REQUESTS_TOTAL
        .with_label_values(&[&chain, method, outcome])
        .inc();
    REQUEST_DURATION
        .with_label_values(&[&chain, method])
        .observe(duration_secs);
}

/// Render all registered metrics as a Prometheus text exposition.
pub fn render() -> Vec<u8> {
    let encoder = TextEncoder::new();
    let families = prometheus::gather();
    let mut buf = Vec::new();
    encoder.encode(&families, &mut buf).unwrap_or_default();
    buf
}
