//! Service orchestration for the THBC payment leg.
//!
//! One module per off-chain service in spec §9:
//!
//! | Spec §9 service | Here |
//! |---|---|
//! | `issuance-service` | [`issuance::IssuanceService`] |
//! | `redemption-service` | [`redemption::RedemptionService`] |
//! | `reserve-service` | [`reserve::ReserveService`] |
//! | `reconciliation-service` | [`reserve::ReconciliationService`] |
//! | `treasury-service` | [`treasury::TreasuryService`] |
//! | `compliance-service` | [`adapters::StubCompliance`] — **not implemented** |
//!
//! These types hold no state of their own. They sequence calls across ports and
//! enforce the *ordering* the spec requires; the arithmetic and the state machines
//! live in `thbc-core`, where they are testable without a runtime.
//!
//! Two orderings are load-bearing and are the reason this layer exists:
//!
//! - **§5.2** — attestation precedes issuance. See
//!   [`issuance::IssuanceService::handle_deposit`].
//! - **§6.2 (F4)** — a confirmed burn precedes a fiat payout. See
//!   [`redemption::RedemptionService::process_payout`], the only route to a wire.

// Test code asserts that guards fire; unwrapping is the assertion. Denied in
// production code by the workspace lints.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod adapters;
pub mod issuance;
pub mod redemption;
pub mod reserve;
pub mod treasury;

pub use issuance::{IssuanceOutcome, IssuanceService};
pub use redemption::{RedemptionOutcome, RedemptionService};
pub use reserve::{ReconciliationService, ReserveService};
pub use treasury::TreasuryService;
