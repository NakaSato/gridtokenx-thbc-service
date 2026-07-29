//! F2 — the issuance conservation identity, and the F1 solvency check.
//!
//! `Σ issued − Σ redeemed = thbc_supply`, checked hourly and daily (spec §9,
//! `reconciliation-service`).
//!
//! This is **detective, not preventive**. Nothing here rejects a write. It compares
//! what this service believes it did against what the ledger says happened, and
//! reports the difference. That is the honest description of F2's status, and it is
//! why `invariant::INVARIANTS` marks F2 `Partial` rather than `Enforced`.

use serde::{Deserialize, Serialize};

use crate::error::{CoreError, CoreResult};
use crate::money::Thb;
use crate::reserve::ReserveState;

/// This service's own tally of confirmed issuances and redemptions.
///
/// Only *confirmed* effects count. A reclaimed redemption never reduced supply, so
/// it must not appear in `redeemed` — including it would manufacture drift that
/// looks exactly like an unbacked mint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct LedgerTally {
    pub issued: Thb,
    pub redeemed: Thb,
}

impl LedgerTally {
    #[must_use]
    pub const fn new(issued: Thb, redeemed: Thb) -> Self {
        Self { issued, redeemed }
    }

    /// `Σ issued − Σ redeemed`. Errors on underflow rather than saturating: more
    /// redeemed than issued is not "zero supply", it is a corrupt ledger.
    pub fn expected_supply(&self) -> CoreResult<Thb> {
        self.issued.checked_sub(self.redeemed)
    }
}

/// Severity of a reconciliation result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Ok,
    /// F2 drift: our tally and the ledger disagree. Something was recorded on one
    /// side and not the other.
    Drift,
    /// F1 breach: supply exceeds free backing. Tokens exist that fiat does not back.
    /// Strictly worse than drift.
    Insolvent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReconciliationReport {
    pub severity: Severity,
    /// `ledger_supply − expected_supply`. Positive means the ledger holds more THBC
    /// than we issued — the unbacked-mint direction.
    pub drift: i128,
    pub expected_supply: Thb,
    pub ledger_supply: Thb,
    pub free_backing: Thb,
    /// How far supply exceeds free backing. Zero when F1 holds.
    pub shortfall: Thb,
    pub checked_at: i64,
}

impl Severity {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Drift => "drift",
            Self::Insolvent => "insolvent",
        }
    }
}

impl ReconciliationReport {
    #[must_use]
    pub const fn is_healthy(&self) -> bool {
        matches!(self.severity, Severity::Ok)
    }
}

/// Run the F2 identity and the F1 solvency check together.
///
/// Both in one pass on purpose: an unbacked mint shows up as F2 drift *and* F1
/// insolvency, and seeing only one of the two invites the wrong diagnosis.
pub fn reconcile(
    tally: &LedgerTally,
    reserve: &ReserveState,
    now: i64,
) -> CoreResult<ReconciliationReport> {
    let expected = tally.expected_supply()?;
    let actual = reserve.supply;

    let drift = i128::from(actual.minor()) - i128::from(expected.minor());
    let free = reserve.free_backing();
    let shortfall = actual.saturating_sub(free);

    let severity = if !shortfall.is_zero() {
        Severity::Insolvent
    } else if drift != 0 {
        Severity::Drift
    } else {
        Severity::Ok
    };

    Ok(ReconciliationReport {
        severity,
        drift,
        expected_supply: expected,
        ledger_supply: actual,
        free_backing: free,
        shortfall,
        checked_at: now,
    })
}

/// Same check, but as a hard error — for callers that must refuse to proceed on a
/// broken identity rather than merely record it.
pub fn assert_f2(tally: &LedgerTally, reserve: &ReserveState) -> CoreResult<()> {
    let expected = tally.expected_supply()?;
    if expected == reserve.supply {
        return Ok(());
    }
    Err(CoreError::ReconciliationDrift {
        issued: tally.issued.minor(),
        redeemed: tally.redeemed.minor(),
        expected: expected.minor(),
        actual: reserve.supply.minor(),
        drift: i128::from(reserve.supply.minor()) - i128::from(expected.minor()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reserve::Attestation;

    fn reserve(attested: u64, encumbered: u64, supply: u64) -> ReserveState {
        ReserveState::new(
            Attestation::new(Thb::from_minor(attested), 0, 3_600),
            Thb::from_minor(encumbered),
            Thb::from_minor(supply),
        )
    }

    fn tally(issued: u64, redeemed: u64) -> LedgerTally {
        LedgerTally::new(Thb::from_minor(issued), Thb::from_minor(redeemed))
    }

    #[test]
    fn f2_holds_on_a_balanced_ledger() {
        let r = reconcile(
            &tally(1_000_000, 400_000),
            &reserve(1_000_000, 0, 600_000),
            0,
        )
        .unwrap();
        assert_eq!(r.severity, Severity::Ok);
        assert_eq!(r.drift, 0);
        assert!(r.is_healthy());
    }

    #[test]
    fn f2_flags_an_unbacked_mint_as_positive_drift() {
        // Ledger holds 700k, we only issued 600k net: 100k came from somewhere else.
        let r = reconcile(
            &tally(1_000_000, 400_000),
            &reserve(1_000_000, 0, 700_000),
            0,
        )
        .unwrap();
        assert_eq!(r.severity, Severity::Drift);
        assert_eq!(r.drift, 100_000);
    }

    #[test]
    fn f2_flags_a_missed_issuance_record_as_negative_drift() {
        let r = reconcile(
            &tally(1_000_000, 400_000),
            &reserve(1_000_000, 0, 500_000),
            0,
        )
        .unwrap();
        assert_eq!(r.severity, Severity::Drift);
        assert_eq!(r.drift, -100_000);
    }

    #[test]
    fn insolvency_outranks_drift() {
        // Both are wrong here; the report must lead with the solvency breach.
        let r = reconcile(&tally(500_000, 0), &reserve(400_000, 0, 900_000), 0).unwrap();
        assert_eq!(r.severity, Severity::Insolvent);
        assert_eq!(r.shortfall.minor(), 500_000);
        assert_eq!(r.drift, 400_000);
    }

    #[test]
    fn encumbered_fiat_can_make_a_balanced_ledger_insolvent() {
        // F2 is perfectly satisfied and the system is still short: 300k of the
        // attested reserve backs nothing. This is precisely the case that
        // `reserve_encumbered` was added to surface (spec §4.1).
        let r = reconcile(
            &tally(1_000_000, 0),
            &reserve(1_000_000, 300_000, 1_000_000),
            0,
        )
        .unwrap();
        assert_eq!(r.drift, 0, "F2 identity holds exactly");
        assert_eq!(r.severity, Severity::Insolvent);
        assert_eq!(r.shortfall.minor(), 300_000);
    }

    #[test]
    fn a_reclaimed_redemption_must_not_be_tallied_as_redeemed() {
        // Reclaimed tokens were never burned. Counting them would fabricate drift
        // indistinguishable from an unbacked mint.
        let with_reclaim_counted = tally(1_000_000, 400_000);
        let correct = tally(1_000_000, 0);
        let state = reserve(1_000_000, 0, 1_000_000);

        assert_eq!(
            reconcile(&correct, &state, 0).unwrap().severity,
            Severity::Ok
        );
        assert_eq!(
            reconcile(&with_reclaim_counted, &state, 0).unwrap().drift,
            400_000,
            "mis-tallying a reclaim looks exactly like a 400k unbacked mint"
        );
    }

    #[test]
    fn more_redeemed_than_issued_is_a_corrupt_ledger_not_zero_supply() {
        assert_eq!(
            tally(100, 200).expected_supply().unwrap_err(),
            CoreError::Underflow
        );
        assert!(reconcile(&tally(100, 200), &reserve(1_000, 0, 0), 0).is_err());
    }

    #[test]
    fn assert_f2_returns_a_typed_error_carrying_both_sides() {
        let err = assert_f2(&tally(1_000_000, 0), &reserve(1_000_000, 0, 900_000)).unwrap_err();
        assert_eq!(
            err,
            CoreError::ReconciliationDrift {
                issued: 1_000_000,
                redeemed: 0,
                expected: 1_000_000,
                actual: 900_000,
                drift: -100_000,
            }
        );
    }

    #[test]
    fn assert_f2_passes_a_balanced_ledger() {
        assert!(assert_f2(&tally(1_000_000, 250_000), &reserve(1_000_000, 0, 750_000)).is_ok());
    }

    #[test]
    fn an_empty_ledger_reconciles() {
        let r = reconcile(&LedgerTally::default(), &reserve(0, 0, 0), 0).unwrap();
        assert_eq!(r.severity, Severity::Ok);
    }
}
