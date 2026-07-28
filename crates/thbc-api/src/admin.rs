//! `admin-api` — operations plus the `E` regulator read surface (spec §9).
//!
//! Gated at the gateway by SSO + MFA + 4-eyes. Two audiences with different rights:
//!
//! - **Operators** — attestation, reconciliation, the redemption queue.
//! - **`E`, the regulator observer** — read-only. Actor table in spec §3: `E` is
//!   trusted for nothing and can steal nothing. Every route reachable by `E` here is
//!   a `GET`, and none of them expose a user's bank reference.
//!
//! The invariant status route is the important one. Spec §2 and §12 are prose tables
//! that drift; `/invariants` serves [`thbc_core::invariant::INVARIANTS`], which is
//! the same data the tests assert against. A regulator asking "what do you actually
//! guarantee" gets the machine's answer, not the brochure's.

use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::routing::{get, post};
use serde::{Deserialize, Serialize};
use thbc_core::invariant::{INVARIANTS, Invariant};
use thbc_core::money::Thb;
use tracing::instrument;

use crate::state::{ApiResult, AppState};

/// Returns a `Router<AppState>`; the caller supplies the state.
pub fn router() -> Router<AppState> {
    Router::new()
        // Regulator-readable (`E`) — all GET, no custody, no PII.
        .route("/invariants", get(invariants))
        .route("/reserve", get(reserve))
        .route("/reconciliation", get(reconciliation))
        .route("/redemptions/queue", get(redemption_queue))
        // Operator actions.
        .route("/attestation", post(attest))
        .route("/redemptions/confirm", post(confirm_redemption))
        .route("/redemptions/payout", post(process_payout))
}

#[derive(Debug, Serialize)]
pub struct InvariantView {
    pub id: &'static str,
    pub name: &'static str,
    pub statement: &'static str,
    pub status: thbc_core::invariant::Status,
    pub enforcement: thbc_core::invariant::Enforcement,
    pub gap: Option<&'static str>,
    /// Whether this may be stated to a third party as a guarantee.
    pub claimable: bool,
}

impl From<&'static Invariant> for InvariantView {
    fn from(i: &'static Invariant) -> Self {
        Self {
            id: i.id,
            name: i.name,
            statement: i.statement,
            status: i.status,
            enforcement: i.enforcement,
            gap: i.gap,
            claimable: i.status.is_claimable(),
        }
    }
}

/// F1–F9 with their real status. The authoritative answer to "what is guaranteed".
async fn invariants(State(state): State<AppState>) -> Json<serde_json::Value> {
    let views: Vec<InvariantView> = INVARIANTS.iter().map(InvariantView::from).collect();
    let claimable: Vec<&str> = INVARIANTS
        .iter()
        .filter(|i| i.status.is_claimable())
        .map(|i| i.id)
        .collect();

    Json(serde_json::json!({
        "invariants": views,
        "claimable": claimable,
        "simulated_ledger": state.simulated,
        "disclaimer":
            "Most of the payment leg is design, not running code (spec §12). No fiat rail \
             exists. Only invariants marked claimable may be described as guarantees.",
    }))
}

#[derive(Debug, Serialize)]
pub struct ReserveView {
    pub attested_reserve_minor: u64,
    pub attestation_ts: i64,
    pub attestation_age_secs: i64,
    pub attestation_fresh: bool,
    pub encumbered_minor: u64,
    /// `attested − encumbered` — the effective F1 ceiling this service enforces.
    pub free_backing_minor: u64,
    pub supply_minor: u64,
    pub headroom_minor: u64,
    pub f1_holds: bool,
    pub simulated: bool,
}

#[instrument(skip(state))]
async fn reserve(State(state): State<AppState>) -> ApiResult<Json<ReserveView>> {
    let s = state.reserve.current().await?;
    let now = state.redemption.now();
    Ok(Json(ReserveView {
        attested_reserve_minor: s.attestation.reserve.minor(),
        attestation_ts: s.attestation.ts,
        attestation_age_secs: s.attestation.age_secs(now),
        attestation_fresh: s.attestation.is_fresh(now),
        encumbered_minor: s.encumbered.minor(),
        free_backing_minor: s.free_backing().minor(),
        supply_minor: s.supply.minor(),
        headroom_minor: s.headroom().minor(),
        f1_holds: s.f1_holds(),
        simulated: state.simulated,
    }))
}

/// Run the F2 identity and F1 solvency check on demand.
#[instrument(skip(state))]
async fn reconciliation(State(state): State<AppState>) -> ApiResult<Json<serde_json::Value>> {
    let r = state.reconciliation.run().await?;
    Ok(Json(serde_json::json!({
        "severity": r.severity,
        "drift": r.drift,
        "expected_supply_minor": r.expected_supply.minor(),
        "ledger_supply_minor": r.ledger_supply.minor(),
        "free_backing_minor": r.free_backing.minor(),
        "shortfall_minor": r.shortfall.minor(),
        "checked_at": r.checked_at,
        "healthy": r.is_healthy(),
        "simulated": state.simulated,
    })))
}

/// `redemption_queue_len` plus the individual overdue records (§6.3).
///
/// Readable by `E` so an issuer sitting on redemptions is publicly observable without
/// anyone gaining custody. Amounts and sequence numbers only — no bank details.
#[instrument(skip(state))]
async fn redemption_queue(State(state): State<AppState>) -> ApiResult<Json<serde_json::Value>> {
    let pending = state.redemption.queue_len().await?;
    let overdue = state.redemption.sweep_reclaimable().await?;

    Ok(Json(serde_json::json!({
        "redemption_queue_len": pending,
        "overdue_count": overdue.len(),
        "overdue": overdue.iter().map(|r| serde_json::json!({
            "user": r.user,
            "seq": r.seq,
            "amount_minor": r.amount.minor(),
            "escrowed_at": r.escrowed_at,
            "delta_secs": r.delta_secs,
        })).collect::<Vec<_>>(),
        "detail":
            "Overdue entries are redemptions the issuer has not confirmed within delta. \
             Holders may reclaim; this service cannot reclaim on their behalf (F8).",
    })))
}

#[derive(Debug, Deserialize)]
pub struct AttestRequest {
    pub reserve_minor: u64,
}

/// Push a fresh attestation. Attestor-signed (`A`), never the parameter admin (F9).
///
/// Spec T1: this is the system's load-bearing trust assumption and it is
/// **unmitigated**. A single signer writes a single `u64`, and inflating it lifts the
/// F1 ceiling for everyone. Key separation from `authority` is the only structural
/// defence, and it does not defend against a dishonest `A`.
#[instrument(skip_all, fields(reserve = body.reserve_minor))]
async fn attest(
    State(state): State<AppState>,
    Json(body): Json<AttestRequest>,
) -> ApiResult<Json<serde_json::Value>> {
    state
        .reserve
        .attest(Thb::from_minor(body.reserve_minor))
        .await?;
    Ok(Json(serde_json::json!({
        "status": "attested",
        "reserve_minor": body.reserve_minor,
        "warning":
            "attested_reserve is a single u64 from a single signer; every downstream \
             guarantee reduces to it being honest (spec §3, T1)",
    })))
}

#[derive(Debug, Deserialize)]
pub struct RedemptionRef {
    pub user: String,
    pub seq: u64,
}

/// `confirm_redemption` — the issuer wired and is burning.
#[instrument(skip_all, fields(user = %body.user, seq = body.seq))]
async fn confirm_redemption(
    State(state): State<AppState>,
    Json(body): Json<RedemptionRef>,
) -> ApiResult<Json<serde_json::Value>> {
    state.redemption.confirm(&body.user, body.seq).await?;
    Ok(Json(serde_json::json!({ "status": "confirmed" })))
}

/// Enqueue the THB payout — **the F4 barrier**. Refuses unless the burn is confirmed.
///
/// A `409 burn_not_confirmed` here is the barrier working, not an outage.
#[instrument(skip_all, fields(user = %body.user, seq = body.seq))]
async fn process_payout(
    State(state): State<AppState>,
    Json(body): Json<RedemptionRef>,
) -> ApiResult<Json<serde_json::Value>> {
    state
        .redemption
        .process_payout(&body.user, body.seq)
        .await?;
    Ok(Json(serde_json::json!({ "status": "payout_queued" })))
}
