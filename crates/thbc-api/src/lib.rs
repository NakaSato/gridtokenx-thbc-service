//! HTTP surface for the THBC settlement service.
//!
//! Three gateways with three different trust levels (spec §9):
//!
//! | Router | Gateway auth | Trust |
//! |---|---|---|
//! | [`public`] | JWT | authenticated user; holds their own keys (F8) |
//! | [`partner`] | mTLS + client-cert allowlist | **untrusted input** — see the module doc |
//! | [`admin`] | SSO + MFA + 4-eyes | operators, plus the `E` regulator read surface |
//!
//! Authentication is terminated at the gateway (APISIX), not here. This crate does
//! not verify JWTs or client certificates, and mounting these routers on a
//! publicly-reachable port without that gateway in front exposes the admin surface.
//! The one check kept in-process is the partner webhook signature header, because it
//! binds to the message rather than to the connection.
//!
//! Handlers are thin: parse, call a service, map the result. Every invariant decision
//! is made in `thbc-core` and sequenced in `thbc-logic`.

// Test code asserts that guards fire; unwrapping is the assertion. Denied in
// production code by the workspace lints.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod admin;
pub mod partner;
pub mod public;
pub mod state;

use axum::Router;
use axum::extract::State;
use axum::routing::get;

pub use state::{ApiError, ApiResult, AppState};

/// Mount all three routers plus health.
///
/// Prefixes are distinct so the gateway can apply a different auth policy to each,
/// and so a misrouted admin call cannot land on a public handler.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/ready", get(ready))
        .nest("/v1", public::router())
        .nest("/v1/partner", partner::router())
        .nest("/v1/admin", admin::router())
        .with_state(state)
}

/// Liveness. Deliberately reports `simulated` — an operator hitting health must not
/// be able to mistake a simulated ledger for a live one.
async fn health(State(state): State<AppState>) -> axum::Json<serde_json::Value> {
    axum::Json(serde_json::json!({
        "status": "ok",
        "service": "thbc-settlement",
        "simulated_ledger": state.simulated,
        "fiat_rail": "none",
    }))
}

/// Readiness — can this service actually issue right now?
///
/// Returns 200 with `issuance_open: false` rather than a 503 when the attestation is
/// stale or headroom is exhausted. Those are expected operating states, not faults,
/// and flapping the pod out of the load balancer for them would take down the
/// read-only regulator surface with them.
async fn ready(State(state): State<AppState>) -> axum::Json<serde_json::Value> {
    let open = state.reserve.is_issuance_open().await.unwrap_or(false);
    axum::Json(serde_json::json!({
        "status": "ok",
        "issuance_open": open,
        "simulated_ledger": state.simulated,
    }))
}
