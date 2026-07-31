//! `admin-api` — operations plus the `E` regulator read surface (spec §9).
//!
//! # How this surface is actually gated
//!
//! Spec §9 asks for SSO + MFA + 4-eyes. What is deployed is **IAM + network
//! scope**, and the difference is disclosed rather than smoothed over:
//!
//! | Gate | Where | What it stops |
//! |---|---|---|
//! | IAM-issued JWT, `role == "admin"` | APISIX `plugin_config 2` | anyone without an operator grant |
//! | `ip-restriction`, internal CIDRs | APISIX route 61 | that token being used from the internet |
//! | [`require_admin_role`] | here, in-process | a misrouted path reaching these handlers |
//!
//! **IAM has no MFA**, so the token is single-factor, and there is no 4-eyes: one
//! operator can attest a reserve and confirm a burn alone. That is a real gap
//! against spec §9 and is recorded as such — do not restate this module as
//! "SSO + MFA + 4-eyes" because the header says admin.
//!
//! Two audiences with different rights:
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

/// Header the gateway sets from the JWT's `role` claim once it has verified the
/// token and the internal-CIDR gate. Overwritten on every admin request by
/// `apisix.yaml` `plugin_config 2`, so a client-supplied value cannot survive
/// that route.
pub const ADMIN_ROLE_HEADER: &str = "x-gridtokenx-user-role";

/// The value [`ADMIN_ROLE_HEADER`] must carry — `iam_core::Role::Admin`'s
/// `Display`, which is what lands in the JWT claim.
pub const ADMIN_ROLE: &str = "admin";

/// Fail-closed check that a request arrived through the gateway's admin gate.
///
/// # What this is, and what it is not
///
/// It is **defense in depth against misrouting**: if a future gateway edit maps a
/// path onto this router without `plugin_config 2`, these handlers refuse rather
/// than serving attestation to whoever asked. That failure mode is realistic —
/// this repo has no CI, so a bad route reaches the running stack unchallenged.
///
/// It is **not** authentication, and must never be described as such. The header
/// is forgeable by anyone who can address the service directly, and
/// `docker-compose.yml:1100` publishes a host port. Authentication is the JWT at
/// the gateway; the network gate is `ip-restriction`. This check only asserts
/// that those ran.
pub async fn require_admin_role(
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Result<axum::response::Response, crate::state::ApiError> {
    let role = request
        .headers()
        .get(ADMIN_ROLE_HEADER)
        .and_then(|v| v.to_str().ok());

    if role != Some(ADMIN_ROLE) {
        // The path is logged, the header value is not: it is caller-controlled
        // input and echoing it into logs is a log-injection vector.
        tracing::warn!(
            path = %request.uri().path(),
            header_present = role.is_some(),
            "admin surface reached without the gateway's admin role assertion"
        );
        return Err(crate::state::ApiError::new(
            axum::http::StatusCode::FORBIDDEN,
            "admin_role_required",
            "this surface is reachable only through the gateway's admin route",
        ));
    }
    Ok(next.run(request).await)
}

/// Returns a `Router<AppState>`; the caller supplies the state.
///
/// Every route carries [`require_admin_role`] — including the regulator reads.
/// The reads disclose the reserve, the supply and the reconciliation history;
/// none of that is public, and `E` reaches them through the same gateway route as
/// an operator.
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
        .layer(axum::middleware::from_fn(require_admin_role))
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
    // The history is the point of the check, not the current reading: a drift that
    // appeared and was resolved is invisible to a point-in-time result.
    let unhealthy_runs = state.reconciliation.unhealthy_runs().await?;

    Ok(Json(serde_json::json!({
        "unhealthy_runs_recorded": unhealthy_runs,
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
