//! `public-api` — the user-facing surface (spec §9). JWT-authenticated at the gateway.
//!
//! Redemption and exchange. Both are user-initiated and neither takes a user key:
//! the user signs on the client and this service relays (F8). If a handler here ever
//! needs a private key to work, F8 is what is being broken.

use axum::Json;
use axum::Router;
use axum::extract::{Path, State};
use axum::routing::{get, post};
use serde::{Deserialize, Serialize};
use thbc_core::money::{Grx, Thb};
use thbc_logic::RedemptionOutcome;
use tracing::instrument;

use crate::state::{ApiResult, AppState};

/// Returns a `Router<AppState>`; the caller supplies the state. Composed into the
/// full surface by [`crate::router`], which applies `with_state` once.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/redemptions", post(request_redemption))
        .route("/redemptions/{user}/{seq}", get(get_redemption))
        .route("/redemptions/{user}/{seq}/reclaim", post(reclaim))
        .route("/exchange/quote/buy", post(quote_buy))
        .route("/exchange/quote/sell", post(quote_sell))
}

#[derive(Debug, Deserialize)]
pub struct RedemptionRequest {
    pub user: String,
    pub amount_minor: u64,
}

#[derive(Debug, Serialize)]
pub struct RedemptionResponse {
    pub status: &'static str,
    pub user: String,
    pub seq: u64,
    /// Δ — after this many seconds without an issuer confirmation, the holder may
    /// reclaim (F7).
    pub reclaim_after_secs: i64,
    pub detail: &'static str,
}

/// Steps 1–2 of spec §6.1.
///
/// Returns as soon as the escrow outcome is known. The response deliberately reports
/// `escrowed`, never `redeemed`: the fiat leg is a promise by `B`, and §6.4 is clear
/// that the promise can be broken. Telling a user their redemption succeeded at this
/// point would be the overclaim the whole design is built to avoid.
#[instrument(skip_all, fields(user = %body.user, amount = body.amount_minor))]
async fn request_redemption(
    State(state): State<AppState>,
    Json(body): Json<RedemptionRequest>,
) -> ApiResult<Json<RedemptionResponse>> {
    let amount = Thb::from_minor(body.amount_minor);
    let outcome = state.redemption.request(&body.user, amount).await?;
    let delta = state.redemption.delta_secs();

    Ok(Json(match outcome {
        RedemptionOutcome::Escrowed { user, seq } => RedemptionResponse {
            status: "escrowed",
            user,
            seq,
            reclaim_after_secs: delta,
            detail: "tokens escrowed, not burned; reclaimable if the issuer does not confirm",
        },
        RedemptionOutcome::Pending { user, seq } => RedemptionResponse {
            status: "pending",
            user,
            seq,
            reclaim_after_secs: delta,
            detail: "escrow submitted but not confirmed; nothing proceeds until it is",
        },
        RedemptionOutcome::Failed { .. } => RedemptionResponse {
            status: "failed",
            user: body.user,
            seq: 0,
            reclaim_after_secs: delta,
            detail: "escrow transaction failed; no tokens moved",
        },
    }))
}

#[derive(Debug, Serialize)]
pub struct RedemptionStatus {
    pub user: String,
    pub seq: u64,
    pub amount_minor: u64,
    pub state: &'static str,
    /// Seconds until reclaim becomes possible. Zero once it is. `None` until the
    /// escrow confirms — the clock has not started.
    pub reclaim_in_secs: Option<i64>,
    pub reclaimable_now: bool,
}

#[instrument(skip(state))]
async fn get_redemption(
    State(state): State<AppState>,
    Path((user, seq)): Path<(String, u64)>,
) -> ApiResult<Json<RedemptionStatus>> {
    let r = state.redemption.status(&user, seq).await?;
    let now = state.redemption.now();

    Ok(Json(RedemptionStatus {
        reclaim_in_secs: r.escrow_age(now).map(|age| (r.delta_secs - age).max(0)),
        reclaimable_now: r.is_reclaimable(now),
        user: r.user,
        seq: r.seq,
        amount_minor: r.amount.minor(),
        state: r.state.as_str(),
    }))
}

/// F7 — the holder recovers their THBC after Δ.
///
/// A `409 timelock_not_expired` here is the normal early case, not a fault.
#[instrument(skip(state))]
async fn reclaim(
    State(state): State<AppState>,
    Path((user, seq)): Path<(String, u64)>,
) -> ApiResult<Json<serde_json::Value>> {
    state.redemption.reclaim(&user, seq).await?;
    Ok(Json(serde_json::json!({
        "status": "reclaimed",
        "detail":
            "THBC restored. Any fiat the issuer already took is NOT recovered by this \
             action — see spec §6.4.",
    })))
}

#[derive(Debug, Deserialize)]
pub struct BuyQuoteRequest {
    pub grx_in_atoms: u64,
}

#[derive(Debug, Deserialize)]
pub struct SellQuoteRequest {
    pub thbc_in_minor: u64,
}

#[derive(Debug, Serialize)]
pub struct QuoteResponse {
    pub in_amount: u64,
    pub out_amount: u64,
    pub fee: u64,
    /// Always false. The exchange path moves platform inventory and never changes
    /// supply (F6) — stated on every quote so it is checkable from outside.
    pub changes_supply: bool,
    pub detail: &'static str,
}

/// GRX → THBC against platform inventory (spec §7).
#[instrument(skip_all, fields(grx_in = body.grx_in_atoms))]
async fn quote_buy(
    State(state): State<AppState>,
    Json(body): Json<BuyQuoteRequest>,
) -> ApiResult<Json<QuoteResponse>> {
    let q = state
        .treasury
        .quote_buy_thbc(Grx::from_atoms(body.grx_in_atoms))
        .await?;
    Ok(Json(QuoteResponse {
        in_amount: q.grx_in.atoms(),
        out_amount: q.thbc_out.minor(),
        fee: q.fee.minor(),
        changes_supply: false,
        detail: "quoted against bounded platform inventory; not an AMM price",
    }))
}

/// THBC → GRX. The incoming THBC returns to inventory rather than being burned.
#[instrument(skip_all, fields(thbc_in = body.thbc_in_minor))]
async fn quote_sell(
    State(state): State<AppState>,
    Json(body): Json<SellQuoteRequest>,
) -> ApiResult<Json<QuoteResponse>> {
    let q = state
        .treasury
        .quote_sell_thbc(Thb::from_minor(body.thbc_in_minor))
        .await?;
    Ok(Json(QuoteResponse {
        in_amount: q.thbc_in.minor(),
        out_amount: q.grx_out.atoms(),
        fee: q.fee.atoms(),
        changes_supply: false,
        detail: "quoted against bounded platform inventory; not an AMM price",
    }))
}
