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

impl ServiceError {
    /// Label for `dispatch_service_rejections_total`; None for failures after admission.
    fn rejection(&self) -> Option<&'static str> {
        Some(match self {
            ServiceError::MissingReceipt => "missing_receipt",
            ServiceError::InvalidReceipt(_) => "invalid_receipt",
            ServiceError::UnauthorizedSender(_) => "unauthorized_sender",
            ServiceError::ReceiptExpired => "receipt_expired",
            ServiceError::UnsupportedChain(_) => "unsupported_chain",
            ServiceError::InvalidRequest(_) => "invalid_request",
            ServiceError::CreditLimitExceeded => "credit_limit",
            ServiceError::InsufficientEscrow => "no_escrow",
            ServiceError::BackendError(_) | ServiceError::Internal(_) => return None,
        })
    }
}

impl IntoResponse for ServiceError {
    fn into_response(self) -> Response {
        if let Some(reason) = self.rejection() {
            crate::metrics::REJECTIONS_TOTAL
                .with_label_values(&[reason])
                .inc();
        }
        let (status, code, message) = match &self {
            ServiceError::MissingReceipt
            | ServiceError::InvalidReceipt(_)
            | ServiceError::UnauthorizedSender(_)
            | ServiceError::ReceiptExpired => (StatusCode::UNAUTHORIZED, -32001, self.to_string()),
            ServiceError::UnsupportedChain(_) => (StatusCode::NOT_FOUND, -32002, self.to_string()),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refusals_are_counted_by_reason_and_backend_failures_are_not() {
        let count = |reason: &str| {
            crate::metrics::REJECTIONS_TOTAL
                .with_label_values(&[reason])
                .get()
        };
        let before = count("missing_receipt");
        let _ = ServiceError::MissingReceipt.into_response();
        let _ = ServiceError::BackendError("down".into()).into_response();
        assert_eq!(count("missing_receipt"), before + 1.0);

        let text = String::from_utf8(crate::metrics::render()).unwrap();
        assert!(text.contains("dispatch_service_rejections_total{reason=\"missing_receipt\"}"));
        assert!(!text.contains("reason=\"backend"));
    }
}
