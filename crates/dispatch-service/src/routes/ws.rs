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

    let validated = admit(&state, &headers, 1).await?;
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
    use crate::routes::test_support::serve;

    async fn status_of(url: &str) -> u16 {
        match tokio_tungstenite::connect_async(url).await {
            Err(tokio_tungstenite::tungstenite::Error::Http(resp)) => resp.status().as_u16(),
            other => panic!("expected the upgrade to be refused, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn disabled_by_default_and_refused_without_a_receipt_when_enabled() {
        let (off, _) = serve("", "").await;
        assert_eq!(status_of(&format!("ws://{off}/ws/1")).await, 404);
        let (on, _) = serve("", "websocket = true").await;
        assert_eq!(status_of(&format!("ws://{on}/ws/1")).await, 401);
    }
}
