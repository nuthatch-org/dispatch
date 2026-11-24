//! A real service on a local port, for route tests.

use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{Arc, RwLock},
};

use alloy_primitives::{Address, Bytes};
use k256::ecdsa::SigningKey;

use crate::{config::Config, server::AppState};

pub const GATEWAY_KEY: [u8; 32] = [0x42; 32];
pub const PROVIDER: &str = "0x70997970C51812dc3A010C7d01b50e0d17dc79C8";
pub const DATA_SERVICE: &str = "0x7101d5c1a5c89c3647f5118da118e56c023ba0b9";

/// `tap_extra` and `chains_extra` are TOML lines appended to those sections.
pub async fn serve(tap_extra: &str, chains_extra: &str) -> (SocketAddr, AppState) {
    let config: Config = toml::from_str(&format!(
        r#"
        [server]
        [indexer]
        service_provider_address = "{PROVIDER}"
        operator_private_key = "0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d"
        [tap]
        data_service_address = "{DATA_SERVICE}"
        authorized_senders = []
        eip712_domain_name = "GraphTallyCollector"
        {tap_extra}
        [chains]
        supported = [1]
        {chains_extra}
        [chains.backends]
        "1" = "http://127.0.0.1:9"
        "#
    ))
    .unwrap();
    let state = AppState {
        tap_domain_separator: dispatch_tap::domain_separator(
            &config.tap.eip712_domain_name,
            config.tap.eip712_chain_id,
            config.tap.eip712_verifying_contract,
        ),
        config: Arc::new(config),
        http_client: reqwest::Client::new(),
        db_pool: None,
        signing_key: Arc::new(SigningKey::from_slice(&[7u8; 32]).unwrap()),
        signer_address: Address::ZERO,
        consumer_credit: Arc::new(RwLock::new(HashMap::new())),
        escrow_checker: None,
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = super::router(state.clone());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (addr, state)
}

pub fn receipt_header(state: &AppState, value: u128) -> String {
    let signed = dispatch_tap::create_receipt(
        &SigningKey::from_slice(&GATEWAY_KEY).unwrap(),
        state.tap_domain_separator,
        DATA_SERVICE.parse().unwrap(),
        PROVIDER.parse().unwrap(),
        value,
        Bytes::default(),
    )
    .unwrap();
    serde_json::to_string(&signed).unwrap()
}
