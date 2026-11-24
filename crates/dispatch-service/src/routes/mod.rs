pub mod health;
pub mod receipts;
pub mod rpc;
#[cfg(test)]
mod test_support;
pub mod ws;

use crate::server::AppState;
use axum::Router;

pub fn router(state: AppState) -> Router {
    Router::new()
        .merge(health::router())
        .merge(receipts::router())
        .merge(rpc::router())
        .merge(ws::router())
        .with_state(state)
}
