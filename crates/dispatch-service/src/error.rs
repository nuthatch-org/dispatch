use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ServiceError {
    #[error("missing TAP-Receipt header")]
    MissingReceipt,

    #[error("invalid TAP receipt: {0}")]
    InvalidReceipt(String),

    #[error("unauthorized sender: {0}")]
    UnauthorizedSender(String),

    #[error("receipt expired")]
    ReceiptExpired,

    #[error("chain {0} not supported")]
    UnsupportedChain(u64),

    #[error("websocket is not enabled on this provider")]
    WebSocketDisabled,

    #[error("backend RPC error: {0}")]
    BackendError(String),

    #[error("invalid JSON-RPC request: {0}")]
    InvalidRequest(String),

    #[error("credit limit exceeded — fund escrow at https://lodestar-dashboard.com/dispatch")]
    CreditLimitExceeded,

    #[error("escrow balance is zero — fund escrow at https://lodestar-dashboard.com/dispatch")]
    InsufficientEscrow,

    #[error("internal error: {0}")]
    Internal(#[from] anyhow::Error),
}

impl IntoResponse for ServiceError {
    fn into_response(self) -> Response {
        let (status, code, message) = match &self {
            ServiceError::MissingReceipt
            | ServiceError::InvalidReceipt(_)
            | ServiceError::UnauthorizedSender(_)
            | ServiceError::ReceiptExpired => (StatusCode::UNAUTHORIZED, -32001, self.to_string()),
            ServiceError::UnsupportedChain(_) | ServiceError::WebSocketDisabled => {
                (StatusCode::NOT_FOUND, -32002, self.to_string())
            }
            ServiceError::CreditLimitExceeded | ServiceError::InsufficientEscrow => {
                (StatusCode::PAYMENT_REQUIRED, -32005, self.to_string())
            }
            ServiceError::BackendError(_) | ServiceError::InvalidRequest(_) => {
                (StatusCode::BAD_GATEWAY, -32603, self.to_string())
            }
            ServiceError::Internal(_) => {
                tracing::error!(error = %self, "internal error");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    -32603,
                    "internal error".to_string(),
                )
            }
        };

        let body = Json(json!({
            "jsonrpc": "2.0",
            "id": null,
            "error": { "code": code, "message": message }
        }));
        (status, body).into_response()
    }
}
