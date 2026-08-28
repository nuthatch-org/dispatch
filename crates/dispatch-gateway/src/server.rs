use std::{net::SocketAddr, num::NonZeroU32, sync::Arc};

use alloy_primitives::{Address, B256};
use anyhow::Result;
use arc_swap::ArcSwap;
use governor::{DefaultKeyedRateLimiter, Quota, RateLimiter};
use k256::ecdsa::SigningKey;
use tower_http::trace::TraceLayer;

use crate::affinity::FilterAffinity;
use crate::{config::Config, discovery, probe, provisioner, registry::Registry, routes};

pub type IpRateLimiter = DefaultKeyedRateLimiter<std::net::IpAddr>;

/// Shared application state — cheaply cloneable, lives for the process lifetime.
#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub http_client: reqwest::Client,
    /// Live registry, atomically swappable by the discovery task.
    pub registry: Arc<ArcSwap<Registry>>,
    pub signing_key: Arc<SigningKey>,
    /// Pre-computed EIP-712 domain separator for GraphTallyCollector.
    pub tap_domain_separator: B256,
    /// Ethereum address derived from `signing_key` — used as `payer` in RAVs.
    pub signer_address: Address,
    /// Optional per-IP rate limiter (None when rate_limit is not configured).
    pub rate_limiter: Option<Arc<IpRateLimiter>>,
    /// Which provider owns which filter. Filters are node-local state, so the follow-up call must
    /// reach the same node that created them; see `affinity`.
    pub filter_affinity: Arc<FilterAffinity>,
}

pub async fn run(config: Config) -> Result<()> {
    let signing_key = {
        let bytes = hex::decode(config.tap.signer_private_key.trim_start_matches("0x"))?;
        SigningKey::from_slice(&bytes)?
    };

    let tap_domain_separator = dispatch_tap::domain_separator(
        &config.tap.eip712_domain_name,
        config.tap.eip712_chain_id,
        config.tap.eip712_verifying_contract,
    );

    let signer_address = dispatch_tap::address_from_key(&signing_key);
    let registry = Arc::new(ArcSwap::from_pointee(Registry::from_config(
        &config.providers,
    )));

    let rate_limiter = config.rate_limit.as_ref().map(|rl| {
        let quota = Quota::per_second(
            NonZeroU32::new(rl.requests_per_second).unwrap_or(NonZeroU32::new(100).unwrap()),
        )
        .allow_burst(NonZeroU32::new(rl.burst).unwrap_or(NonZeroU32::new(20).unwrap()));
        Arc::new(RateLimiter::dashmap(quota))
    });

    let state = AppState {
        config: Arc::new(config.clone()),
        http_client: reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .build()?,
        registry,
        signing_key: Arc::new(signing_key),
        tap_domain_separator,
        signer_address,
        rate_limiter,
        // 5 minutes matches the idle window after which nodes drop filters themselves, so an
        // entry never outlives the thing it points at. The cap is a memory bound, not a
        // capacity plan: filter ids are attacker-controllable in volume.
        filter_affinity: Arc::new(FilterAffinity::new(
            std::time::Duration::from_secs(300),
            100_000,
        )),
    };

    // Initialise prometheus metrics (lazy statics — triggers registration).
    let _ = &*crate::metrics::REQUESTS_TOTAL;
    let _ = &*crate::metrics::REQUEST_DURATION;

    tokio::spawn(probe::run(state.clone()));
    tokio::spawn(discovery::run(state.clone()));
    tokio::spawn(provisioner::run(state.clone()));

    let app = routes::router(state.clone()).layer(TraceLayer::new_for_http());

    let addr: SocketAddr = format!("{}:{}", config.gateway.host, config.gateway.port).parse()?;
    tracing::info!(%addr, "dispatch-gateway starting");

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await?;
    Ok(())
}
