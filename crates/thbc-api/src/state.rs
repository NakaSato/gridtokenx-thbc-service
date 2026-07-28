//! Shared handler state and the error → HTTP mapping.

use std::sync::Arc;

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use thbc_core::error::CoreError;
use thbc_core::ports::PortError;
use thbc_logic::{
    IssuanceService, ReconciliationService, RedemptionService, ReserveService, TreasuryService,
};

/// Everything the handlers need. Injected via `State`, never a global static.
#[derive(Clone)]
pub struct AppState {
    pub issuance: Arc<IssuanceService>,
    pub redemption: Arc<RedemptionService>,
    pub reserve: Arc<ReserveService>,
    pub reconciliation: Arc<ReconciliationService>,
    pub treasury: Arc<TreasuryService>,
    /// True when the ledger behind these services is the simulator. Surfaced on every
    /// health and status response so a caller can never mistake simulated state for
    /// a real fiat rail.
    pub simulated: bool,
}

/// A typed API error. `thiserror` at the boundary, so clients get a stable code
/// rather than a prose string they end up matching on.
#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub code: &'static str,
    pub message: String,
}

impl ApiError {
    #[must_use]
    pub fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(serde_json::json!({ "code": self.code, "error": self.message })),
        )
            .into_response()
    }
}

impl From<PortError> for ApiError {
    fn from(e: PortError) -> Self {
        let (status, code) = match &e {
            // 503, not 500: the caller should retry, and a bank webhook that gets a
            // 5xx will be redelivered — which F3 exists to make safe.
            PortError::Transient(_) => (StatusCode::SERVICE_UNAVAILABLE, "transient"),

            // 501, and distinct from every other failure. Spec §12: most of the
            // payment leg is not built, and "not built" must never be reported as
            // "refused" or "broken".
            PortError::Unsupported(_) => (StatusCode::NOT_IMPLEMENTED, "not_implemented"),

            PortError::Rejected(_) => (StatusCode::UNPROCESSABLE_ENTITY, "rejected"),
            PortError::NotFound => (StatusCode::NOT_FOUND, "not_found"),
            PortError::Conflict(_) => (StatusCode::CONFLICT, "conflict"),
            PortError::Domain(d) => return Self::from(d.clone()),
        };
        Self::new(status, code, e.to_string())
    }
}

impl From<CoreError> for ApiError {
    fn from(e: CoreError) -> Self {
        let (status, code) = match &e {
            // The invariant breaches are 422, not 400: the request was well-formed
            // and the system refused it on policy. The code names the invariant so a
            // caller can act on it — `stale_attestation` means "retry after the next
            // refresh", `reserve_insufficient` means "do not retry".
            CoreError::ReserveInsufficient { .. } => {
                (StatusCode::UNPROCESSABLE_ENTITY, "reserve_insufficient")
            }
            CoreError::StaleAttestation { .. } => {
                (StatusCode::SERVICE_UNAVAILABLE, "stale_attestation")
            }
            CoreError::DuplicateBankRef { .. } => (StatusCode::CONFLICT, "duplicate_bank_ref"),
            CoreError::BurnNotConfirmed { .. } => (StatusCode::CONFLICT, "burn_not_confirmed"),
            CoreError::TimelockNotExpired { .. } => (StatusCode::CONFLICT, "timelock_not_expired"),
            CoreError::InsufficientInventory { .. } => {
                (StatusCode::UNPROCESSABLE_ENTITY, "insufficient_inventory")
            }
            CoreError::ReconciliationDrift { .. } => {
                (StatusCode::INTERNAL_SERVER_ERROR, "reconciliation_drift")
            }
            CoreError::Paused => (StatusCode::SERVICE_UNAVAILABLE, "paused"),

            // 422, not 400: the payload is well-formed, it just cannot be acted
            // on. Matches the same rejection the partner handler raises before
            // the deposit is recorded — this arm covers any path that reaches
            // `Deposit::observe` without going through it.
            CoreError::MissingBeneficiaryWallet => (
                StatusCode::UNPROCESSABLE_ENTITY,
                "missing_beneficiary_wallet",
            ),

            CoreError::ZeroAmount
            | CoreError::EmptyBankRef
            | CoreError::InvalidFeeBps { .. }
            | CoreError::MathOverflow
            | CoreError::Underflow => (StatusCode::BAD_REQUEST, "invalid_request"),

            CoreError::InvalidDepositTransition { .. }
            | CoreError::InvalidRedemptionTransition { .. } => {
                (StatusCode::CONFLICT, "invalid_state")
            }

            CoreError::AttestationInFuture { .. }
            | CoreError::AttestorIsAuthority
            | CoreError::RateNotSet => (StatusCode::INTERNAL_SERVER_ERROR, "misconfigured"),
        };
        Self::new(status, code, e.to_string())
    }
}

pub type ApiResult<T> = Result<T, ApiError>;
