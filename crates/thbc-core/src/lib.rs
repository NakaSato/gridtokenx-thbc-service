//! # thbc-core — the THBC settlement domain
//!
//! The payment leg of `GridTokenX`: how Thai baht enters and leaves the ledger.
//! Specified in `docs/product-specs/THBC_ISSUER_SERVICE.md`.
//!
//! ## What THBC is
//!
//! A Thai-baht-referenced settlement token. One unit represents a claim on one Thai
//! baht held in a segregated reserve account at a licensed financial institution.
//! Issued against fiat received and burned against fiat paid, **by a licensed issuer
//! partner**. `GridTokenX` operates the ledger integration and the settlement logic.
//! `GridTokenX` is not the issuer, holds no fiat, and holds no user keys.
//!
//! ## What this crate is
//!
//! Synchronous, framework-free domain logic — "sync core, async edges". It has three
//! dependencies (`serde`, `thiserror`, `sha2`) and knows nothing about HTTP,
//! Postgres, NATS, or Solana. Everything it does is a pure function over values, so
//! every invariant below is testable without a runtime, a database, or a validator.
//!
//! ## Read this before trusting anything here
//!
//! Most of the payment leg **is not built**. Spec §12: `issue_thbc`,
//! `redeem_thbc_for_fiat`, the deposit nullifier, the redemption escrow, and
//! `reserve_encumbered` do not exist on-chain, and no fiat rail of any kind exists.
//! This crate models them correctly and the simulated ledger executes them, but a
//! model is not a guarantee.
//!
//! [`invariant::INVARIANTS`] is the machine-readable statement of what is and is not
//! actually enforced. Consult it rather than the prose. Today **F3, F5, F8 and F9** may
//! be described as guarantees; F1, F2, F4 and F6 are partial; F7 is design-only.
//!
//! F6 — the exchange path minting THBC against GRX collateral — was fixed on-chain on
//! 2026-07-29: `swap_grx_for_thbc`/`redeem_thbc_for_grx` became
//! `exchange_grx_for_thbc`/`exchange_thbc_for_grx`, which transfer against a
//! `[b"thbc_inventory"]` vault. No program mints or burns THBC any more. That fix also
//! removed the only call sites of the F1 ceiling and the F5 freshness check, because
//! both lived on the minting swap. `issue_thbc` (a554499) re-attached them to the
//! instruction they actually belong to, and brought F3 with it: the `[b"deposit",
//! H(bank_ref)]` nullifier is created with `init` in the same instruction as the mint,
//! so a replayed webhook is rejected by the **runtime**, not by application code.
//!
//! ## Module map
//!
//! | Module | Owns |
//! |---|---|
//! | [`money`] | `Thb` / `Grx` integer minor units. No floats, anywhere. |
//! | [`invariant`] | F1–F9 and their real status. |
//! | [`reserve`] | F1 ceiling, F5 freshness, F9 key separation. |
//! | [`bank_ref`] | `H(bank_ref)` and the F3 nullifier seed. |
//! | [`deposit`] | On-ramp state machine; §5.2 ordering barrier. |
//! | [`redemption`] | Off-ramp state machine; F4 barrier and F7 timelock. |
//! | [`exchange`] | Inventory exchange — the F6 fix, supply-preserving by construction. |
//! | [`reconcile`] | F2 identity and F1 solvency, as a detective check. |
//! | [`ports`] | Traits the edges implement. F8 lives in this file's shape. |

// Test code asserts that guards fire; unwrapping is the assertion. Denied in
// production code by the workspace lints.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod bank_ref;
pub mod deposit;
pub mod error;
pub mod exchange;
pub mod invariant;
pub mod money;
pub mod ports;
pub mod reconcile;
pub mod redemption;
pub mod reserve;

pub use bank_ref::{BankRef, BankRefHash};
pub use deposit::{Deposit, DepositState, ScreenOutcome};
pub use error::{CoreError, CoreResult};
pub use exchange::{ExchangeParams, ExchangeQuote, Inventory, ReverseQuote};
pub use invariant::{Enforcement, INVARIANTS, Invariant, Status};
pub use money::{Grx, Thb};
pub use reconcile::{LedgerTally, ReconciliationReport, Severity};
pub use redemption::{ConfirmOutcome, Redemption, RedemptionState};
pub use reserve::{Attestation, KeyId, ReserveState, TreasuryKeys};
