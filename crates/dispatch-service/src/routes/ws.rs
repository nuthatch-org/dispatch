//! WebSocket proxy — /ws/{chain_id}
//!
//! Accepts a WebSocket upgrade from the gateway and proxies it to the
//! configured backend Ethereum node for the requested chain.
//! Enables eth_subscribe / eth_unsubscribe for real-time event streams.

use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Path, State,
    },
    http::HeaderMap,
    response::IntoResponse,
    routing::get,
    Router,
};
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message as TungMsg;

use crate::{
    error::ServiceError,
    routes::rpc::{admit, settle},
    server::AppState,
};

pub fn router() -> Router<AppState> {
    Router::new().route("/ws/:chain_id", get(ws_handler))
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
    Path(chain_id): Path<u64>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, ServiceError> {
    if !state.config.chains.websocket {
        return Err(ServiceError::WebSocketDisabled);
    }
    let backend_http = state
        .config
        .chains
        .backends
        .get(&chain_id.to_string())
        .ok_or(ServiceError::UnsupportedChain(chain_id))?
        .clone();

    let validated = admit(&state, &headers).await?;
    settle(&state, chain_id, &validated).await;

    // Convert the HTTP backend URL to a WebSocket URL.
    let backend_ws = backend_http
        .replace("https://", "wss://")
        .replace("http://", "ws://");

    Ok(ws.on_upgrade(move |socket| proxy(socket, backend_ws, chain_id)))
}

async fn proxy(client: WebSocket, backend_url: String, chain_id: u64) {
    let (upstream, _) = match tokio_tungstenite::connect_async(&backend_url).await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, chain_id, "ws: backend connect failed");
            return;
        }
    };

    tracing::debug!(chain_id, %backend_url, "ws: proxy established");

    let (mut client_sink, mut client_stream) = client.split();
    let (mut up_sink, mut up_stream) = upstream.split();

    let client_to_up = async {
        while let Some(Ok(msg)) = client_stream.next().await {
            match axum_to_tung(msg) {
                Some(m) => {
                    if up_sink.send(m).await.is_err() {
                        break;
                    }
                }
                None => break,
            }
        }
    };

    let up_to_client = async {
        while let Some(Ok(msg)) = up_stream.next().await {
            match tung_to_axum(msg) {
                Some(m) => {
                    if client_sink.send(m).await.is_err() {
                        break;
                    }
                }
                None => break,
            }
        }
    };

    tokio::select! {
        _ = client_to_up => {}
        _ = up_to_client => {}
    }

    tracing::debug!(chain_id, "ws: connection closed");
}

fn axum_to_tung(msg: Message) -> Option<TungMsg> {
    match msg {
        Message::Text(t) => Some(TungMsg::Text(t)),
        Message::Binary(b) => Some(TungMsg::Binary(b)),
        Message::Ping(b) => Some(TungMsg::Ping(b)),
        Message::Pong(b) => Some(TungMsg::Pong(b)),
        Message::Close(_) => None,
    }
}

fn tung_to_axum(msg: TungMsg) -> Option<Message> {
    match msg {
        TungMsg::Text(t) => Some(Message::Text(t)),
        TungMsg::Binary(b) => Some(Message::Binary(b)),
        TungMsg::Ping(b) => Some(Message::Ping(b)),
        TungMsg::Pong(b) => Some(Message::Pong(b)),
        TungMsg::Close(_) => None,
        TungMsg::Frame(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, sync::Arc, sync::RwLock};

    use k256::ecdsa::SigningKey;

    use super::*;
    use crate::config::Config;

    async fn serve(websocket: bool) -> String {
        let config: Config = toml::from_str(&format!(
            r#"
            [server]
            [indexer]
            service_provider_address = "0x70997970C51812dc3A010C7d01b50e0d17dc79C8"
            operator_private_key = "0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d"
            [tap]
            data_service_address = "0x7101d5c1a5c89c3647f5118da118e56c023ba0b9"
            authorized_senders = []
            eip712_domain_name = "GraphTallyCollector"
            [chains]
            supported = [1]
            websocket = {websocket}
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
            signer_address: Default::default(),
            consumer_credit: Arc::new(RwLock::new(HashMap::new())),
            escrow_checker: None,
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, router().with_state(state))
                .await
                .unwrap()
        });
        format!("ws://{addr}/ws/1")
    }

    async fn status_of(url: &str) -> u16 {
        match tokio_tungstenite::connect_async(url).await {
            Err(tokio_tungstenite::tungstenite::Error::Http(resp)) => resp.status().as_u16(),
            other => panic!("expected the upgrade to be refused, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn disabled_by_default_and_refused_without_a_receipt_when_enabled() {
        assert_eq!(status_of(&serve(false).await).await, 404);
        assert_eq!(status_of(&serve(true).await).await, 401);
    }
}
