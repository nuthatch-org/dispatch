/// GET|POST /solana/*path
///
/// Proxy route for Seahorn (Solana structured data service) queries.
///
/// Signs a TAP receipt with the Seahorn data_service_address and forwards the
/// request to the configured Seahorn provider endpoint. The provider validates
/// the receipt and returns PostgREST query results.
use axum::{
    body::Body,
    extract::State,
    http::{Request, StatusCode},
    response::{IntoResponse, Response},
    routing::any,
    Router,
};
use dispatch_tap::create_receipt;

use crate::server::AppState;

pub fn router() -> Router<AppState> {
    Router::new().route("/solana/*path", any(seahorn_handler))
}

async fn seahorn_handler(State(state): State<AppState>, req: Request<Body>) -> Response {
    let Some(seahorn) = state.config.seahorn.as_ref() else {
        return (StatusCode::NOT_FOUND, "Seahorn not configured").into_response();
    };

    // Build the forwarded URL: strip the leading "/solana" prefix, keep the rest.
    let uri = req.uri();
    let suffix = uri.path().strip_prefix("/solana").unwrap_or(uri.path());
    let mut target = format!("{}{}", seahorn.endpoint.trim_end_matches('/'), suffix);
    if let Some(qs) = uri.query() {
        target.push('?');
        target.push_str(qs);
    }

    // Sign a TAP receipt for this query.
    let signed = match create_receipt(
        &state.signing_key,
        state.tap_domain_separator,
        seahorn.data_service_address,
        seahorn.service_provider,
        seahorn.price_grt_wei,
        alloy_primitives::Bytes::default(),
    ) {
        Ok(r) => r,
        Err(e) => {
            tracing::error!("TAP receipt signing failed: {e}");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    let receipt_header = match serde_json::to_string(&signed) {
        Ok(h) => h,
        Err(e) => {
            tracing::error!("receipt serialisation failed: {e}");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    // Forward the original method, headers, and body.
    let method = reqwest::Method::from_bytes(req.method().as_str().as_bytes())
        .unwrap_or(reqwest::Method::GET);

    let mut builder = state
        .http_client
        .request(method, &target)
        .header("TAP-Receipt", receipt_header);

    // Forward safe, non-hop-by-hop headers.
    for (name, value) in req.headers() {
        let n = name.as_str();
        if matches!(n, "content-type" | "accept" | "accept-encoding" | "prefer") {
            if let Ok(v) = value.to_str() {
                builder = builder.header(n, v);
            }
        }
    }

    let body_bytes = match axum::body::to_bytes(req.into_body(), 4 * 1024 * 1024).await {
        Ok(b) => b,
        Err(e) => {
            tracing::error!("failed to read request body: {e}");
            return StatusCode::BAD_REQUEST.into_response();
        }
    };
    if !body_bytes.is_empty() {
        builder = builder.body(body_bytes.to_vec());
    }

    let upstream = match builder.send().await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(%target, "Seahorn upstream error: {e}");
            return StatusCode::BAD_GATEWAY.into_response();
        }
    };

    let status = StatusCode::from_u16(upstream.status().as_u16())
        .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let content_type = upstream
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/json")
        .to_string();

    let resp_body = match upstream.bytes().await {
        Ok(b) => b,
        Err(e) => {
            tracing::error!("failed to read Seahorn response body: {e}");
            return StatusCode::BAD_GATEWAY.into_response();
        }
    };

    tracing::info!(%target, status = status.as_u16(), bytes = resp_body.len(), "seahorn proxied");

    (status, [("content-type", content_type.as_str())], resp_body).into_response()
}
