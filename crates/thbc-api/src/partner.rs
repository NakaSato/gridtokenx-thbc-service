//! `partner-api` — the bank webhook ingress (spec §9).
//!
//! **Everything arriving here is untrusted input.** Signature verification and mTLS
//! establish that the message came from a key the bank controls; they say nothing
//! about whether the fiat exists. Spec T6: a compromised bank key produces a
//! perfectly valid signature over a lie. What bounds that is F1 — a forged deposit
//! still cannot mint past the attested reserve — not anything in this file.
//!
//! Transport security (mTLS, client-cert allowlist) is terminated at the gateway.
//! This router assumes it has already happened and never re-checks it; what it *does*
//! own is the payload signature, because that is bound to the message rather than to
//! the connection.

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use thbc_core::bank_ref::BankRef;
use thbc_core::money::Thb;
use thbc_logic::IssuanceOutcome;
use tracing::instrument;

use crate::state::{ApiError, ApiResult, AppState};

/// Header carrying the bank's signature over the raw request body.
pub const SIGNATURE_HEADER: &str = "x-thbc-signature";

#[derive(Debug, Deserialize)]
pub struct DepositWebhook {
    /// The bank's own unique transaction reference. Becomes `H(bank_ref)`, the F3
    /// nullifier seed. Never logged.
    pub bank_ref: String,
    /// THB in minor units (6 decimals). Integer — a JSON float here would not
    /// survive the F2 identity.
    pub amount_minor: u64,
    /// IAM user id that receives the THBC.
    pub beneficiary: String,
}

#[derive(Debug, Serialize)]
pub struct DepositResponse {
    pub status: &'static str,
    pub detail: String,
    /// Present when THBC was actually issued.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub amount_minor: Option<u64>,
    /// True when the ledger behind this response is the simulator.
    pub simulated: bool,
}

/// Returns a `Router<AppState>`; the caller supplies the state.
pub fn router() -> Router<AppState> {
    Router::new().route("/webhooks/deposit", post(deposit))
}

/// Bank deposit webhook — step 2 of spec §5.1.
///
/// **At-least-once.** The bank will retry, and a retry must be safe: a duplicate
/// `bank_ref` returns `200 already_issued` rather than an error, because the bank
/// retrying is correct behaviour and a 4xx would make it retry harder. F3 is what
/// makes that safe.
#[instrument(skip_all, fields(beneficiary = %body.beneficiary, amount = body.amount_minor))]
async fn deposit(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<DepositWebhook>,
) -> ApiResult<(StatusCode, Json<DepositResponse>)> {
    // The signature header must be present. Actually verifying it needs the bank's
    // public key, and there is no bank adapter (spec §12) — so this checks presence
    // and refuses to pretend it checked more. Rejecting the unsigned case is still
    // worth doing: it keeps the contract visible and fails closed if the gateway is
    // ever misconfigured to forward raw traffic.
    if !headers.contains_key(SIGNATURE_HEADER) {
        return Err(ApiError::new(
            StatusCode::UNAUTHORIZED,
            "missing_signature",
            format!("{SIGNATURE_HEADER} is required on partner webhooks"),
        ));
    }

    let bank_ref = BankRef::new(&body.bank_ref)?;
    let amount = Thb::from_minor(body.amount_minor);

    let outcome = state
        .issuance
        .handle_deposit(bank_ref, amount, &body.beneficiary)
        .await?;

    let (status, response) = match outcome {
        IssuanceOutcome::Issued { amount } => (
            StatusCode::CREATED,
            DepositResponse {
                status: "issued",
                detail: "THBC issued to beneficiary".into(),
                amount_minor: Some(amount.minor()),
                simulated: state.simulated,
            },
        ),
        // 200, not 409. The bank is supposed to retry; a conflict status would be
        // read as an error to escalate.
        IssuanceOutcome::AlreadyIssued => (
            StatusCode::OK,
            DepositResponse {
                status: "already_issued",
                detail: "duplicate bank_ref; no second issuance (F3)".into(),
                amount_minor: None,
                simulated: state.simulated,
            },
        ),
        IssuanceOutcome::Encumbered { amount, reason } => (
            StatusCode::ACCEPTED,
            DepositResponse {
                status: "encumbered",
                detail: format!("{reason}; fiat backs no token and a return wire is required"),
                amount_minor: Some(amount.minor()),
                simulated: state.simulated,
            },
        ),
        // 202: accepted and held. Not an error — the deposit is recorded and will be
        // retried after the next attestation refresh, and the bank_ref is not
        // consumed.
        IssuanceOutcome::Held { reason } => (
            StatusCode::ACCEPTED,
            DepositResponse {
                status: "held",
                detail: reason,
                amount_minor: None,
                simulated: state.simulated,
            },
        ),
    };

    Ok((status, Json(response)))
}
