use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum GatewayError {
    #[error("no providers available for chain {0}")]
    NoProviders(u64),

    #[error("all providers failed for chain {0}")]
    AllProvidersFailed(u64),

    #[error("provider error: {0}")]
    ProviderError(String),

    #[error("invalid JSON-RPC request: {0}")]
    InvalidRequest(String),

    #[error("chain {0} not supported")]
    UnsupportedChain(u64),

    #[error("receipt signing failed: {0}")]
    SigningError(String),

    #[error("missing or unknown API key")]
    Unauthorized,

    #[error("rate limit exceeded")]
    RateLimited,

    #[error("daily quota of {0} requests reached for this key; it resets at 00:00 UTC")]
    DailyQuotaExceeded(u64),

    /// Consumer provided a header value that isn't a valid Ethereum address.
    #[error("invalid X-Consumer-Address: {0}")]
    InvalidConsumerAddress(String),

    #[error("internal error: {0}")]
    Internal(#[from] anyhow::Error),
}

impl IntoResponse for GatewayError {
    fn into_response(self) -> Response {
        let (status, code, message) = match &self {
            GatewayError::InvalidRequest(_) => (StatusCode::BAD_REQUEST, -32600, self.to_string()),
            GatewayError::UnsupportedChain(_) => (StatusCode::NOT_FOUND, -32002, self.to_string()),
            GatewayError::Unauthorized => (StatusCode::UNAUTHORIZED, -32001, self.to_string()),
            GatewayError::RateLimited | GatewayError::DailyQuotaExceeded(_) => {
                (StatusCode::TOO_MANY_REQUESTS, -32005, self.to_string())
            }
            GatewayError::NoProviders(_) | GatewayError::AllProvidersFailed(_) => {
                (StatusCode::SERVICE_UNAVAILABLE, -32003, self.to_string())
            }
            GatewayError::ProviderError(_) | GatewayError::SigningError(_) => {
                (StatusCode::BAD_GATEWAY, -32603, self.to_string())
            }
            GatewayError::InvalidConsumerAddress(_) => {
                (StatusCode::PAYMENT_REQUIRED, -32004, self.to_string())
            }
            GatewayError::Internal(_) => {
                tracing::error!(error = %self, "internal gateway error");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    -32603,
                    "internal error".to_string(),
                )
            }
        };

        (
            status,
            Json(json!({
                "jsonrpc": "2.0",
                "id": null,
                "error": { "code": code, "message": message }
            })),
        )
            .into_response()
    }
}
