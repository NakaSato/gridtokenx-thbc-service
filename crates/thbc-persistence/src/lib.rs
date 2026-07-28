//! Persistence adapters for the THBC payment leg.
//!
//! Two implementations of each repository port:
//!
//! - [`postgres`] — the real store. Runtime `sqlx` queries rather than `query_as!`;
//!   see that module's doc for why, and what has to change to get back to the repo
//!   convention.
//! - [`memory`] — in-memory, for the simulation mode and unit tests. Reproduces the
//!   uniqueness behaviour F3 leans on, so tests cannot pass for the wrong reason.
//!
//! Migrations live in `migrations/` at the service root and are applied with
//! `sqlx migrate run`. This service has **its own database** (`gridtokenx_thbc`) and
//! must not JOIN to another service's tables — the DB-per-service split is mid-flight
//! and new cross-service JOINs are what make it un-finishable.

// Test code asserts that guards fire; unwrapping is the assertion. Denied in
// production code by the workspace lints.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod memory;
pub mod postgres;

pub use memory::{InMemoryDepositRepo, InMemoryRedemptionRepo};
pub use postgres::{PgDepositRepository, PgRedemptionRepository};

/// Apply the migrations in `migrations/`.
///
/// # Errors
/// If the database is unreachable or a migration fails.
pub async fn migrate(pool: &sqlx::PgPool) -> Result<(), sqlx::migrate::MigrateError> {
    sqlx::migrate!("../../migrations").run(pool).await
}
