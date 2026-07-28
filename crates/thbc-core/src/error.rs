//! Domain errors for the payment leg.
//!
//! Every variant names the invariant or state-machine edge it protects, because the
//! error a service logs is the only evidence a reviewer has that the guard fired.

use thiserror::Error;

pub type CoreResult<T> = Result<T, CoreError>;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum CoreError {
    #[error("arithmetic overflow")]
    MathOverflow,

    #[error("arithmetic underflow — a liability exceeded its backing")]
    Underflow,

    #[error("amount must be greater than zero")]
    ZeroAmount,

    // ---- F1 / F5: issuance ceiling -------------------------------------------
    #[error(
        "F1 breach: issuing {requested} would put supply at {resulting_supply} \
         against free backing {free_backing} \
         (attested {attested_reserve} − encumbered {encumbered})"
    )]
    ReserveInsufficient {
        requested: u64,
        resulting_supply: u64,
        free_backing: u64,
        attested_reserve: u64,
        encumbered: u64,
    },

    #[error(
        "F5 breach: attestation is {age_secs}s old, ttl is {ttl_secs}s — \
         issuance is halted until the attestor refreshes"
    )]
    StaleAttestation { age_secs: i64, ttl_secs: i64 },

    #[error("attestation timestamp {ts} is in the future relative to now {now}")]
    AttestationInFuture { ts: i64, now: i64 },

    // ---- F3: deposit idempotency ---------------------------------------------
    #[error("F3: bank_ref {bank_ref_hash} has already been issued against")]
    DuplicateBankRef { bank_ref_hash: String },

    #[error("bank_ref must not be empty")]
    EmptyBankRef,

    // ---- F9: key separation ---------------------------------------------------
    #[error("F9 breach: attestor key must differ from the parameter-authority key")]
    AttestorIsAuthority,

    // ---- State machines (F4, F7) ----------------------------------------------
    #[error("invalid deposit transition: {from} -> {to}")]
    InvalidDepositTransition {
        from: &'static str,
        to: &'static str,
    },

    #[error("invalid redemption transition: {from} -> {to}")]
    InvalidRedemptionTransition {
        from: &'static str,
        to: &'static str,
    },

    #[error(
        "F4 breach: payout may not be enqueued from state {state} — \
         the burn must be CONFIRMED on-chain first, not merely submitted"
    )]
    BurnNotConfirmed { state: &'static str },

    #[error("F7: redemption is still inside the {delta_secs}s timelock ({elapsed_secs}s elapsed)")]
    TimelockNotExpired { elapsed_secs: i64, delta_secs: i64 },

    // ---- F6: backing-set purity ------------------------------------------------
    #[error(
        "F6: exchange needs {requested} THBC but platform inventory holds {available} — \
         the exchange path must never mint to cover a shortfall"
    )]
    InsufficientInventory { requested: u64, available: u64 },

    #[error("exchange rate is not configured")]
    RateNotSet,

    #[error("fee_bps must not exceed 10000 (100%)")]
    InvalidFeeBps { bps: u16 },

    // ---- F2: accounting identity -------------------------------------------------
    #[error(
        "F2 breach: issued {issued} − redeemed {redeemed} = {expected} \
         but ledger supply is {actual} (drift {drift})"
    )]
    ReconciliationDrift {
        issued: u64,
        redeemed: u64,
        expected: u64,
        actual: u64,
        drift: i128,
    },

    #[error("treasury is paused")]
    Paused,
}
