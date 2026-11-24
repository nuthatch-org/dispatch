//! WebSocket proxy — /ws/{chain_id}
//!
//! Accepts a WebSocket upgrade from the client, selects a provider for the
//! requested chain, opens an upstream WebSocket to that provider's service,
//! and bidirectionally forwards all frames.
//!
//! Use case: eth_subscribe / eth_unsubscribe for real-time events.

use std::net::SocketAddr;

use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        ConnectInfo, Path, State,
    },
    http::HeaderMap,
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message as TungMsg;

use crate::{error::GatewayError, selector, server::AppState};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/ws/:chain_id", get(ws_handler))
        .route("/ws/:chain_id/:key", get(ws_handler_with_key))
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
    Path(chain_id): Path<u64>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Result<Response, GatewayError> {
    state.api_keys.authenticate(None, &headers)?;
    Ok(ws
        .on_upgrade(move |socket| proxy(socket, state, chain_id, addr))
        .into_response())
}

async fn ws_handler_with_key(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
    Path((chain_id, key)): Path<(u64, String)>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Result<Response, GatewayError> {
    state.api_keys.authenticate(Some(&key), &headers)?;
    Ok(ws
        .on_upgrade(move |socket| proxy(socket, state, chain_id, addr))
        .into_response())
}

async fn proxy(client: WebSocket, state: AppState, chain_id: u64, peer: SocketAddr) {
    // Select one provider for this chain.
    let provider = {
        let registry = state.registry.load();
        let (providers, chain_head) = match registry.providers_for_chain(chain_id) {
            Some(v) => v,
            None => {
                tracing::debug!(%peer, chain_id, "ws: unsupported chain");
                return;
            }
        };
        let window = state.config.answering_window();
        let routable: Vec<_> = providers
            .iter()
            .filter(|p| p.is_routable(chain_id, window))
            .cloned()
            .collect();
        let mut candidates = selector::select(
            &routable,
            chain_head,
            1,
            state.config.gateway.region.as_deref(),
            state.config.qos.region_bonus,
        );
        match candidates.pop() {
            Some(p) => p,
            None => {
                tracing::debug!(%peer, chain_id, "ws: no providers");
                return;
            }
        }
    };

    // Derive upstream WebSocket URL.
    let upstream_url = format!(
        "{}/ws/{}",
        provider
            .endpoint
            .replace("https://", "wss://")
            .replace("http://", "ws://"),
        chain_id
    );

    // One receipt pays for the connection; the provider refuses the upgrade without it.
    let request = match connection_request(&state, &upstream_url, provider.address) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, provider = %provider.endpoint, "ws: could not sign receipt");
            return;
        }
    };

    let (upstream, _) = match tokio_tungstenite::connect_async(request).await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, provider = %provider.endpoint, "ws: upstream connect failed");
            return;
        }
    };

    tracing::debug!(%peer, %upstream_url, "ws: proxy established");

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

    tracing::debug!(%peer, "ws: connection closed");
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

/// Price of one WebSocket connection, in CU.
const CONNECTION_CU: u128 = 10;

fn connection_request(
    state: &AppState,
    url: &str,
    provider: alloy_primitives::Address,
) -> anyhow::Result<tokio_tungstenite::tungstenite::handshake::client::Request> {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;

    let mut metadata = state.config.tap.gateway_payer_address.to_vec();
    metadata.extend_from_slice(b"eth_subscribe");
    let receipt = dispatch_tap::create_receipt(
        &state.signing_key,
        state.tap_domain_separator,
        state.config.tap.data_service_address,
        provider,
        CONNECTION_CU * state.config.tap.base_price_per_cu,
        metadata.into(),
    )?;
    let mut request = url.into_client_request()?;
    request
        .headers_mut()
        .insert("TAP-Receipt", serde_json::to_string(&receipt)?.parse()?);
    Ok(request)
}
