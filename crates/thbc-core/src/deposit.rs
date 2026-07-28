//! The fiat deposit (on-ramp) state machine — spec §5.
//!
//! The ordering constraint is the reason this is a machine and not a function.
//! Spec §5.2: reserve attestation **must** precede issuance. If issuance precedes
//! attestation, F1 is violated for the interval between them. The type system here
//! makes that interval unrepresentable — there is no transition from `Screened` to
//! `Issued`, only `Screened -> Attested -> Issued`.

use serde::{Deserialize, Serialize};

use crate::bank_ref::{BankRef, BankRefHash};
use crate::error::{CoreError, CoreResult};
use crate::money::Thb;

/// Outcome of the compliance screen (spec §5 step 3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScreenOutcome {
    Pass,
    /// KYC/sanctions/AML rejected. Fiat has already cleared the bank, so this does
    /// not mean "nothing happened" — it means the fiat is encumbered (§5.4).
    Fail,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DepositState {
    /// Webhook received from `B`, signature verified. **Untrusted input** — the
    /// amount here is a claim, not a fact, until reconciliation agrees with it.
    Observed,
    /// Compliance screen passed.
    Screened,
    /// `A` has re-attested and the reserve covers this deposit. Only from here may
    /// issuance proceed (§5.2).
    Attested,
    /// `issue_thbc` confirmed on-chain. Terminal, and irreversible.
    Issued,
    /// Screen failed. Fiat sits in the reserve backing nothing; it is counted into
    /// `reserve_encumbered` and a return wire is queued (§5.4). Terminal.
    Encumbered,
    /// Bank and webhook disagree on the amount. Reconciliation must resolve it before
    /// anything else happens; no issuance from here (§5.4).
    Disputed,
}

impl DepositState {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Observed => "observed",
            Self::Screened => "screened",
            Self::Attested => "attested",
            Self::Issued => "issued",
            Self::Encumbered => "encumbered",
            Self::Disputed => "disputed",
        }
    }

    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Issued | Self::Encumbered)
    }
}

/// One fiat deposit, tracked from webhook to issuance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Deposit {
    pub bank_ref: BankRef,
    /// Amount the webhook claimed.
    pub amount: Thb,
    /// The user who gets the THBC — an IAM user id. Identity for screening and
    /// the audit trail, NOT an address: nothing on-chain can be derived from it.
    pub beneficiary: String,
    /// The beneficiary's Solana owner wallet, base58.
    ///
    /// Supplied by the partner on the deposit webhook. This service deliberately
    /// holds no Solana toolchain (F8) and no IAM client, so it cannot derive or
    /// resolve this — it carries the string through to Chain Bridge, which
    /// derives the associated token account under the THBC mint's own token
    /// program. Stored on the deposit so a held deposit can be retried later
    /// without the partner re-sending it.
    pub beneficiary_wallet: String,
    pub state: DepositState,
    pub observed_at: i64,
}

impl Deposit {
    /// Step 2 of §5.1 — a signature-verified webhook has arrived.
    pub fn observe(
        bank_ref: BankRef,
        amount: Thb,
        beneficiary: impl Into<String>,
        beneficiary_wallet: impl Into<String>,
        now: i64,
    ) -> CoreResult<Self> {
        if amount.is_zero() {
            return Err(CoreError::ZeroAmount);
        }
        let beneficiary_wallet = beneficiary_wallet.into();
        // An empty wallet would reach the bridge, fail `Pubkey::from_str`, and
        // burn the bank_ref for nothing. Refuse before the deposit is recorded.
        if beneficiary_wallet.trim().is_empty() {
            return Err(CoreError::MissingBeneficiaryWallet);
        }
        Ok(Self {
            bank_ref,
            amount,
            beneficiary: beneficiary.into(),
            beneficiary_wallet,
            state: DepositState::Observed,
            observed_at: now,
        })
    }

    #[must_use]
    pub fn nullifier(&self) -> BankRefHash {
        self.bank_ref.hash()
    }

    fn expect(&self, want: DepositState, to: DepositState) -> CoreResult<()> {
        if self.state == want {
            Ok(())
        } else {
            Err(CoreError::InvalidDepositTransition {
                from: self.state.as_str(),
                to: to.as_str(),
            })
        }
    }

    /// Step 3 — compliance screen. A failure encumbers the fiat rather than
    /// discarding the record: the money is real and still in the reserve account.
    pub fn screen(&mut self, outcome: ScreenOutcome) -> CoreResult<()> {
        let to = match outcome {
            ScreenOutcome::Pass => DepositState::Screened,
            ScreenOutcome::Fail => DepositState::Encumbered,
        };
        self.expect(DepositState::Observed, to)?;
        self.state = to;
        Ok(())
    }

    /// Amount mismatch between the bank statement and the webhook (§5.4). Callable
    /// from `Observed` or `Screened` — reconciliation runs on its own schedule and
    /// may notice after the screen has already passed.
    pub fn dispute(&mut self) -> CoreResult<()> {
        match self.state {
            DepositState::Observed | DepositState::Screened => {
                self.state = DepositState::Disputed;
                Ok(())
            }
            other => Err(CoreError::InvalidDepositTransition {
                from: other.as_str(),
                to: DepositState::Disputed.as_str(),
            }),
        }
    }

    /// Resolve a dispute in favour of `settled` being the true amount. Returns to
    /// `Screened` — the compliance screen already passed and is not re-run, but the
    /// attestation step still has to happen against the corrected figure.
    pub fn resolve_dispute(&mut self, settled: Thb) -> CoreResult<()> {
        if settled.is_zero() {
            return Err(CoreError::ZeroAmount);
        }
        self.expect(DepositState::Disputed, DepositState::Screened)?;
        self.amount = settled;
        self.state = DepositState::Screened;
        Ok(())
    }

    /// Step 4 — `A` has re-attested and the reserve covers this deposit.
    ///
    /// This is the §5.2 ordering barrier. The caller must have verified the
    /// attestation against [`crate::reserve::ReserveState::check_issuance`]; this
    /// transition only records that it happened.
    pub fn mark_attested(&mut self) -> CoreResult<()> {
        self.expect(DepositState::Screened, DepositState::Attested)?;
        self.state = DepositState::Attested;
        Ok(())
    }

    /// Step 5 — `issue_thbc` confirmed. Only reachable from `Attested`.
    pub fn mark_issued(&mut self) -> CoreResult<()> {
        self.expect(DepositState::Attested, DepositState::Issued)?;
        self.state = DepositState::Issued;
        Ok(())
    }

    /// Whether this deposit's fiat should count toward `reserve_encumbered`.
    #[must_use]
    pub const fn is_encumbering(&self) -> bool {
        matches!(
            self.state,
            DepositState::Encumbered | DepositState::Disputed
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn deposit() -> Deposit {
        Deposit::observe(
            BankRef::new("SCB-1").unwrap(),
            Thb::from_baht(1_000).unwrap(),
            "user-1",
            "Wa11etOwner",
            0,
        )
        .unwrap()
    }

    #[test]
    fn happy_path_runs_observe_screen_attest_issue() {
        let mut d = deposit();
        d.screen(ScreenOutcome::Pass).unwrap();
        d.mark_attested().unwrap();
        d.mark_issued().unwrap();
        assert_eq!(d.state, DepositState::Issued);
        assert!(d.state.is_terminal());
    }

    // ---- §5.2 ordering: attestation must precede issuance --------------------

    #[test]
    fn issuance_cannot_skip_attestation() {
        // The whole point of §5.2. Issuing from `Screened` would violate F1 for the
        // window between mint and attestation.
        let mut d = deposit();
        d.screen(ScreenOutcome::Pass).unwrap();
        let err = d.mark_issued().unwrap_err();
        assert_eq!(
            err,
            CoreError::InvalidDepositTransition {
                from: "screened",
                to: "issued"
            }
        );
        assert_eq!(
            d.state,
            DepositState::Screened,
            "a rejected transition must not mutate"
        );
    }

    #[test]
    fn issuance_cannot_skip_the_compliance_screen() {
        let mut d = deposit();
        assert!(d.mark_attested().is_err());
        assert!(d.mark_issued().is_err());
        assert_eq!(d.state, DepositState::Observed);
    }

    // ---- §5.4 failure modes --------------------------------------------------

    #[test]
    fn failed_kyc_encumbers_the_fiat_rather_than_dropping_it() {
        let mut d = deposit();
        d.screen(ScreenOutcome::Fail).unwrap();
        assert_eq!(d.state, DepositState::Encumbered);
        assert!(
            d.is_encumbering(),
            "cleared fiat backing nothing must tighten the F1 ceiling"
        );
        assert!(d.state.is_terminal());
    }

    #[test]
    fn an_encumbered_deposit_can_never_be_issued() {
        let mut d = deposit();
        d.screen(ScreenOutcome::Fail).unwrap();
        assert!(d.mark_attested().is_err());
        assert!(d.mark_issued().is_err());
    }

    #[test]
    fn an_amount_mismatch_blocks_issuance_until_resolved() {
        let mut d = deposit();
        d.screen(ScreenOutcome::Pass).unwrap();
        d.dispute().unwrap();
        assert!(d.is_encumbering());
        assert!(
            d.mark_attested().is_err(),
            "no issuance until reconciliation resolves"
        );

        d.resolve_dispute(Thb::from_baht(900).unwrap()).unwrap();
        assert_eq!(d.amount, Thb::from_baht(900).unwrap());
        d.mark_attested().unwrap();
        d.mark_issued().unwrap();
    }

    #[test]
    fn a_dispute_can_be_raised_before_the_screen_runs() {
        let mut d = deposit();
        d.dispute().unwrap();
        assert_eq!(d.state, DepositState::Disputed);
    }

    #[test]
    fn an_issued_deposit_is_frozen() {
        let mut d = deposit();
        d.screen(ScreenOutcome::Pass).unwrap();
        d.mark_attested().unwrap();
        d.mark_issued().unwrap();
        assert!(d.dispute().is_err(), "issuance is irreversible");
        assert!(d.screen(ScreenOutcome::Fail).is_err());
    }

    #[test]
    fn the_screen_does_not_run_twice() {
        let mut d = deposit();
        d.screen(ScreenOutcome::Pass).unwrap();
        assert!(d.screen(ScreenOutcome::Pass).is_err());
    }

    #[test]
    fn zero_amount_deposits_are_refused_at_the_door() {
        let err =
            Deposit::observe(BankRef::new("X").unwrap(), Thb::ZERO, "u", "W", 0).unwrap_err();
        assert_eq!(err, CoreError::ZeroAmount);
    }

    /// An empty wallet would reach Chain Bridge, fail to parse as a pubkey, and
    /// consume the `bank_ref` for nothing. Refuse before the deposit exists.
    #[test]
    fn a_missing_beneficiary_wallet_is_refused_at_the_door() {
        for wallet in ["", "   "] {
            let err = Deposit::observe(
                BankRef::new("X").unwrap(),
                Thb::from_baht(1).unwrap(),
                "u",
                wallet,
                0,
            )
            .unwrap_err();
            assert_eq!(err, CoreError::MissingBeneficiaryWallet);
        }
    }

    #[test]
    fn nullifier_is_derived_from_the_normalised_bank_ref() {
        let d = deposit();
        assert_eq!(d.nullifier(), BankRef::new("scb-1").unwrap().hash());
    }
}
