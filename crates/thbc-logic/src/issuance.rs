//! `issuance-service` — the on-ramp state machine (spec §5, §9).
//!
//! Drives a deposit from a bank webhook to an on-chain issuance, in the order §5.1
//! specifies and no other. The single most important property of this file is that
//! **step 4 (attestation) precedes step 5 (issuance)**. Spec §5.2: if issuance
//! precedes attestation, F1 is violated for the interval between them.

use std::sync::Arc;

use thbc_core::bank_ref::BankRef;
use thbc_core::deposit::{Deposit, ScreenOutcome};
use thbc_core::money::Thb;
use thbc_core::ports::{
    Clock, CompliancePort, DepositRepository, LedgerPort, PortError, PortResult,
};
use thbc_core::redemption::ConfirmOutcome;
use tracing::{info, instrument, warn};

/// What happened to a deposit. Distinguishing these is the point: an operator
/// looking at a failed on-ramp needs to know whether the fiat is stuck, refused, or
/// simply waiting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IssuanceOutcome {
    /// THBC minted to the beneficiary.
    Issued { amount: Thb },
    /// Already issued against this `bank_ref` — a webhook replay. Not an error:
    /// the bank is *supposed* to retry, and the correct response is a quiet success.
    AlreadyIssued,
    /// Compliance refused. Fiat is in the reserve backing nothing and now counts
    /// toward `reserve_encumbered`; a return wire is queued (§5.4).
    Encumbered { amount: Thb, reason: String },
    /// Attestation was stale or the reserve was short. The deposit is held and
    /// retried after the next refresh — no issuance, and the `bank_ref` is not
    /// consumed (§5.4).
    Held { reason: String },
}

pub struct IssuanceService {
    deposits: Arc<dyn DepositRepository>,
    compliance: Arc<dyn CompliancePort>,
    ledger: Arc<dyn LedgerPort>,
    clock: Arc<dyn Clock>,
}

impl IssuanceService {
    #[must_use]
    pub fn new(
        deposits: Arc<dyn DepositRepository>,
        compliance: Arc<dyn CompliancePort>,
        ledger: Arc<dyn LedgerPort>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            deposits,
            compliance,
            ledger,
            clock,
        }
    }

    /// The reserve state this service issues against: the chain's attestation and
    /// supply, with encumbrance from our own deposit records.
    ///
    /// The encumbrance half cannot come from the snapshot, because
    /// `reserve_encumbered` is not an on-chain field (spec §4.1 marks it NEW). Taking
    /// the snapshot's reserve at face value would issue against
    /// `attested_reserve` — fiat that cleared the bank and then failed KYC would keep
    /// counting as free backing, and the F1 ceiling would be looser than §4.1
    /// specifies by exactly the encumbered amount.
    ///
    /// So this service is **stricter than the chain**, and that asymmetry is a
    /// disclosed gap, not a safety margin: a caller that bypasses this service gets
    /// the looser ceiling the program actually enforces.
    async fn effective_reserve(&self) -> PortResult<thbc_core::reserve::ReserveState> {
        let snapshot = self.ledger.snapshot().await?;
        let encumbered = self.deposits.total_encumbered().await?;
        Ok(thbc_core::reserve::ReserveState::new(
            snapshot.reserve.attestation,
            encumbered,
            snapshot.reserve.supply,
        ))
    }

    /// Handle a signature-verified bank webhook.
    ///
    /// The webhook is **untrusted input** even after signature verification (§5.4,
    /// T6): a compromised bank key produces a perfectly valid signature over a lie.
    /// What bounds the damage is F1 — a forged deposit still cannot mint past the
    /// attested reserve — not anything this function does.
    ///
    /// `bank_ref` is never logged: it correlates a user to a transfer. The digest is.
    #[instrument(skip(self, bank_ref), fields(amount = amount.minor(), beneficiary))]
    pub async fn handle_deposit(
        &self,
        bank_ref: BankRef,
        amount: Thb,
        beneficiary: &str,
        beneficiary_wallet: &str,
    ) -> PortResult<IssuanceOutcome> {
        let now = self.clock.now();
        let deposit = Deposit::observe(bank_ref, amount, beneficiary, beneficiary_wallet, now)?;
        let nullifier = deposit.nullifier();

        // ---- Step 2: record the observation. Off-chain F3. ------------------
        //
        // A conflict means this bank_ref was seen before. Report the *previous*
        // outcome rather than re-running the flow: replaying a deposit that
        // previously failed KYC must not get a second screen.
        match self.deposits.insert(&deposit).await {
            Ok(()) => {}
            Err(PortError::Conflict(_)) => {
                let existing = self.deposits.find(nullifier).await?;
                info!(nullifier = %nullifier, "F3: duplicate bank_ref, replaying prior outcome");
                return Ok(match existing {
                    Some(d) if d.state == thbc_core::deposit::DepositState::Issued => {
                        IssuanceOutcome::AlreadyIssued
                    }
                    Some(d) if d.is_encumbering() => IssuanceOutcome::Encumbered {
                        amount: d.amount,
                        reason: "previously encumbered".into(),
                    },
                    _ => IssuanceOutcome::Held {
                        reason: "duplicate bank_ref, prior attempt incomplete".into(),
                    },
                });
            }
            Err(e) => return Err(e),
        }

        let mut deposit = deposit;

        // ---- Step 3: compliance screen --------------------------------------
        let screen = self.compliance.screen(beneficiary, amount).await?;
        deposit.screen(screen)?;
        if screen == ScreenOutcome::Fail {
            // The fiat cleared the bank. It is real, it is in the reserve account,
            // and it backs nothing — so it must tighten the F1 ceiling rather than
            // be forgotten. `reserve-service` picks it up via `total_encumbered`.
            self.deposits.update(&deposit).await?;
            warn!(nullifier = %nullifier, "KYC failed; fiat encumbered, return wire required");
            return Ok(IssuanceOutcome::Encumbered {
                amount,
                reason: "compliance screen failed".into(),
            });
        }
        self.deposits.update(&deposit).await?;

        // ---- Step 4: attestation MUST precede issuance (§5.2) ---------------
        //
        // Read the snapshot and check the ceiling *before* asking for the mint. The
        // program checks it again — this is not the enforcement, it is the ordering.
        // Doing it here means a deposit that cannot be backed never reaches the
        // chain, so the `bank_ref` is not consumed and the retry after the next
        // attestation refresh still works.
        let state = self.effective_reserve().await?;
        if let Err(e) = state.check_issuance(amount, now) {
            info!(nullifier = %nullifier, error = %e, "deposit held pending attestation refresh");
            return Ok(IssuanceOutcome::Held {
                reason: e.to_string(),
            });
        }
        deposit.mark_attested()?;
        self.deposits.update(&deposit).await?;

        // ---- Step 5: issue --------------------------------------------------
        //
        // The ledger gets the beneficiary's WALLET, not the IAM user id: nothing
        // on-chain can be derived from the latter.
        let outcome = self
            .ledger
            .issue(beneficiary_wallet, amount, nullifier)
            .await?;
        match outcome {
            ConfirmOutcome::Confirmed => {
                deposit.mark_issued()?;
                self.deposits.update(&deposit).await?;
                info!(nullifier = %nullifier, amount = amount.minor(), "issued");
                Ok(IssuanceOutcome::Issued { amount })
            }
            // Submitted is not issued. The deposit stays in `attested` and the
            // reconciler resolves it — marking it issued on a submit would break F2
            // the moment the transaction failed to land.
            ConfirmOutcome::Submitted => Ok(IssuanceOutcome::Held {
                reason: "issuance submitted but not confirmed".into(),
            }),
            ConfirmOutcome::Failed => Ok(IssuanceOutcome::Held {
                reason: "issuance transaction failed".into(),
            }),
        }
    }

    /// Retry every deposit stuck in `Screened`.
    ///
    /// Spec §5.4 says a deposit refused for a stale attestation is "held, retried
    /// after refresh". Nothing performed that retry until 2026-07-29: `retry_held`
    /// existed and had tests, but no scheduler and no endpoint reached it, so a
    /// deposit held during a stale window stayed held forever — with the holder's
    /// fiat already sitting in the reserve account.
    ///
    /// Returns `(retried, issued)`. Errors on individual deposits are logged and the
    /// sweep continues: one deposit that cannot be issued must not block the rest,
    /// and the common case for a held batch is that they all fail together until the
    /// attestation refreshes and then all succeed.
    #[instrument(skip(self))]
    pub async fn retry_all_held(&self) -> PortResult<(usize, usize)> {
        let held = self.deposits.held().await?;
        if held.is_empty() {
            return Ok((0, 0));
        }

        let mut issued = 0usize;
        for deposit in &held {
            match self.retry_held(&deposit.bank_ref).await {
                Ok(IssuanceOutcome::Issued { .. }) => issued += 1,
                Ok(IssuanceOutcome::Held { reason }) => {
                    // Expected while the reserve is stale or short — the deposit stays
                    // held and is picked up again next sweep.
                    info!(nullifier = %deposit.nullifier(), "still held: {reason}");
                }
                Ok(other) => info!(nullifier = %deposit.nullifier(), "retry gave {other:?}"),
                Err(e) => warn!(nullifier = %deposit.nullifier(), "retry failed: {e}"),
            }
        }

        if issued > 0 {
            info!(retried = held.len(), issued, "held deposits released");
        }
        Ok((held.len(), issued))
    }

    /// Retry a held deposit after an attestation refresh (§5.4).
    #[instrument(skip(self))]
    pub async fn retry_held(&self, bank_ref: &BankRef) -> PortResult<IssuanceOutcome> {
        let nullifier = bank_ref.hash();
        let mut deposit = self
            .deposits
            .find(nullifier)
            .await?
            .ok_or(PortError::NotFound)?;

        // Only a screened deposit is retryable. Anything else is either done, refused,
        // or waiting on reconciliation.
        if deposit.state != thbc_core::deposit::DepositState::Screened {
            return Ok(IssuanceOutcome::Held {
                reason: format!("deposit is {}, not retryable", deposit.state.as_str()),
            });
        }

        let now = self.clock.now();
        let state = self.effective_reserve().await?;
        if let Err(e) = state.check_issuance(deposit.amount, now) {
            return Ok(IssuanceOutcome::Held {
                reason: e.to_string(),
            });
        }

        deposit.mark_attested()?;
        self.deposits.update(&deposit).await?;

        // The wallet was captured on the original webhook and stored, so a retry
        // needs nothing from the partner.
        let outcome = self
            .ledger
            .issue(&deposit.beneficiary_wallet, deposit.amount, nullifier)
            .await?;
        if outcome == ConfirmOutcome::Confirmed {
            deposit.mark_issued()?;
            self.deposits.update(&deposit).await?;
            return Ok(IssuanceOutcome::Issued {
                amount: deposit.amount,
            });
        }
        Ok(IssuanceOutcome::Held {
            reason: format!("issuance {outcome:?}"),
        })
    }
}
