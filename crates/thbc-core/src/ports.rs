//! Ports — the traits the logic layer depends on, implemented at the edges.
//!
//! Per the repo's dependency rule (`server → api → logic → persistence → core`),
//! contracts are defined here and implemented in `thbc-persistence` / `thbc-ledger`.
//!
//! The business logic in this crate — [`crate::money`], [`crate::reserve`],
//! [`crate::deposit`], [`crate::redemption`], [`crate::exchange`],
//! [`crate::reconcile`] — is entirely synchronous and framework-free, per
//! "sync core, async edges". These port traits are the seam, so they are async: the
//! things behind them are a database, a message bus, and a bank.
//!
//! **F8 is a property of this file.** No method here accepts a user private key,
//! keypair, or signature-producing handle. The platform can ask the chain to do
//! things on its own behalf and can relay a transaction the user already signed; it
//! cannot construct a signer set that moves user THBC. If a future method needs a
//! user key to work, F8 is what you are about to break.

use async_trait::async_trait;

use crate::bank_ref::BankRefHash;
use crate::deposit::Deposit;
use crate::exchange::{ExchangeParams, Inventory};
use crate::money::Thb;
use crate::redemption::{ConfirmOutcome, Redemption};
use crate::reserve::ReserveState;

/// Errors crossing a port boundary. Deliberately coarse: the logic layer decides
/// policy, and the only distinction it needs is "retry", "this will never work", and
/// "the chain refused".
#[derive(Debug, thiserror::Error)]
pub enum PortError {
    #[error("transient failure, safe to retry: {0}")]
    Transient(String),

    #[error("the chain rejected the request: {0}")]
    Rejected(String),

    /// The instruction this call needs does not exist on-chain yet. Distinct from
    /// `Rejected` so the caller can report "not built" rather than "refused" —
    /// see spec §12.
    #[error("not implemented on-chain: {0}")]
    Unsupported(&'static str),

    #[error("not found")]
    NotFound,

    /// A uniqueness constraint fired — for a deposit, this is F3 doing its job.
    #[error("conflict: {0}")]
    Conflict(String),

    #[error(transparent)]
    Domain(#[from] crate::error::CoreError),
}

pub type PortResult<T> = Result<T, PortError>;

/// A wall-clock source. Injected rather than called directly so the Δ timelock and
/// attestation TTL can be tested by moving time instead of sleeping.
pub trait Clock: Send + Sync {
    /// Unix seconds.
    fn now(&self) -> i64;
}

/// The treasury's on-chain state, as read through Chain Bridge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TreasurySnapshot {
    pub reserve: ReserveState,
    pub inventory: Inventory,
    pub params: ExchangeParams,
    /// Pending redemptions — the count `E` observes (spec §6.3).
    pub redemption_queue_len: u32,
}

/// The single door to the ledger.
///
/// Every method goes through Chain Bridge — NATS `JetStream` for writes, gRPC for
/// reads. No implementation of this trait may hold a Solana RPC client.
#[async_trait]
pub trait LedgerPort: Send + Sync {
    async fn snapshot(&self) -> PortResult<TreasurySnapshot>;

    /// `issue_thbc(amount, bank_ref_hash)` — signed by the issuer `B`.
    ///
    /// The nullifier PDA `[b"deposit", H(bank_ref)]` must be created with `init` in
    /// the *same* instruction as the mint, so a replay reverts at the account level
    /// (F3). An implementation that creates it in a separate transaction has not
    /// implemented F3.
    ///
    /// `beneficiary` is the recipient's **owner wallet** (base58), NOT an IAM user
    /// id and NOT a token account:
    ///
    /// * Not a user id, because nothing on-chain can be derived from one and this
    ///   service holds no IAM client to resolve it.
    /// * Not a token account, because the on-chain account is constrained by
    ///   `token::mint = thbc_mint` alone — that checks the mint, not that the
    ///   account's owner is the beneficiary — so a supplied token account would be
    ///   an unvalidated destination. The implementation derives the associated
    ///   token account from this owner under the mint's own token program.
    ///
    /// Returning [`ConfirmOutcome::Submitted`] means the outcome is **unknown**,
    /// not failed. Callers must not retry on it: the `init`-created nullifier makes
    /// a retry of an issuance that actually landed fail permanently.
    async fn issue(
        &self,
        beneficiary: &str,
        amount: Thb,
        nullifier: BankRefHash,
    ) -> PortResult<ConfirmOutcome>;

    /// Relay a user-signed `redeem_thbc_for_fiat`. Returns only when the chain has
    /// reported an outcome — the caller needs `Confirmed`, not `Submitted`, to pass
    /// the F4 barrier.
    async fn escrow_redemption(&self, redemption: &Redemption) -> PortResult<ConfirmOutcome>;

    /// `confirm_redemption(id)` — issuer-signed, after the wire is sent. Burns.
    async fn confirm_redemption(&self, user: &str, seq: u64) -> PortResult<ConfirmOutcome>;

    /// `reclaim_redemption(id)` — user-signed, after Δ. Restores the THBC.
    async fn reclaim_redemption(&self, user: &str, seq: u64) -> PortResult<ConfirmOutcome>;

    /// `update_attestation(reserve)` — attestor-signed.
    async fn update_attestation(&self, reserve: Thb) -> PortResult<ConfirmOutcome>;
}

#[async_trait]
pub trait DepositRepository: Send + Sync {
    /// Persist a newly observed deposit.
    ///
    /// Must fail with [`PortError::Conflict`] if the nullifier already exists. This
    /// is the off-chain half of F3 — necessary, and nowhere near sufficient: it stops
    /// replays through *this service* only. The account-level guarantee needs the
    /// on-chain nullifier PDA, which does not exist yet.
    async fn insert(&self, deposit: &Deposit) -> PortResult<()>;

    async fn find(&self, nullifier: BankRefHash) -> PortResult<Option<Deposit>>;

    async fn update(&self, deposit: &Deposit) -> PortResult<()>;

    /// Total fiat cleared but backing nothing — feeds `reserve_encumbered`.
    async fn total_encumbered(&self) -> PortResult<Thb>;

    /// Σ of confirmed issuances, for F2.
    async fn total_issued(&self) -> PortResult<Thb>;
}

#[async_trait]
pub trait RedemptionRepository: Send + Sync {
    async fn insert(&self, redemption: &Redemption) -> PortResult<()>;

    async fn find(&self, user: &str, seq: u64) -> PortResult<Option<Redemption>>;

    async fn update(&self, redemption: &Redemption) -> PortResult<()>;

    /// Next sequence number for a user's `[b"redeem", user, seq]` record.
    async fn next_seq(&self, user: &str) -> PortResult<u64>;

    /// Redemptions past Δ awaiting reclaim — what the Δ monitor sweeps.
    async fn reclaimable(&self, now: i64) -> PortResult<Vec<Redemption>>;

    /// Σ of *confirmed* redemptions, for F2. Reclaimed redemptions must be excluded.
    async fn total_redeemed(&self) -> PortResult<Thb>;

    async fn pending_count(&self) -> PortResult<u32>;
}

/// KYC / sanctions / AML. NDID is the intended primary (spec §9).
#[async_trait]
pub trait CompliancePort: Send + Sync {
    async fn screen(&self, subject: &str, amount: Thb)
    -> PortResult<crate::deposit::ScreenOutcome>;
}

/// The THB payout queue. Only ever reached through the F4 barrier.
#[async_trait]
pub trait PayoutPort: Send + Sync {
    /// Enqueue a THB wire to the user's verified bank account.
    ///
    /// Implementations must be idempotent on `(user, seq)`: the caller may retry
    /// after a crash between the burn and this call, and a double wire is
    /// unrecoverable in a way a double burn is not.
    async fn enqueue(&self, user: &str, seq: u64, amount: Thb) -> PortResult<()>;
}
