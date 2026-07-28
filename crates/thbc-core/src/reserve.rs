//! Reserve accounting: the F1 ceiling, F5 freshness, F9 key separation.
//!
//! This is the load-bearing trust assumption of the whole payment leg. Spec §3:
//! `attested_reserve` is a single `u64` written by a single signer, and F1, F5, the
//! peg claim and the solvency of every user balance all reduce to that number being
//! honest. Nothing in this module improves that. It enforces the *arithmetic* around
//! the number; it cannot tell you the number is true.

use serde::{Deserialize, Serialize};

use crate::error::{CoreError, CoreResult};
use crate::money::Thb;

/// An opaque Solana public key. This crate does not depend on `solana-sdk` — the
/// core stays framework-free, and the only thing it needs from a key is identity.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct KeyId(pub String);

/// The three privileged keys, checked for F9 separation once at startup.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TreasuryKeys {
    /// Sets params, pauses. Must differ from `attestor`.
    pub authority: KeyId,
    /// Writes `attested_reserve`. Must differ from `authority`.
    pub attestor: KeyId,
    /// The licensed issuer partner `B` — sole caller of issue/redeem.
    pub issuer: KeyId,
}

impl TreasuryKeys {
    /// F9. Rejected here as well as on-chain so a misconfigured deployment dies at
    /// startup instead of at the first attestation, when fiat is already in flight.
    pub fn new(authority: KeyId, attestor: KeyId, issuer: KeyId) -> CoreResult<Self> {
        if authority == attestor {
            return Err(CoreError::AttestorIsAuthority);
        }
        Ok(Self {
            authority,
            attestor,
            issuer,
        })
    }
}

/// A reserve attestation: what `A` claims is in the segregated accounts, and when.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attestation {
    /// THB in segregated accounts, in minor units.
    pub reserve: Thb,
    /// Unix timestamp the attestation was written.
    pub ts: i64,
    /// Maximum age in seconds before issuance halts (F5).
    pub ttl_secs: i64,
}

impl Attestation {
    #[must_use]
    pub const fn new(reserve: Thb, ts: i64, ttl_secs: i64) -> Self {
        Self {
            reserve,
            ts,
            ttl_secs,
        }
    }

    /// Age in seconds at `now`. Negative if the attestation is stamped in the future.
    #[must_use]
    pub const fn age_secs(&self, now: i64) -> i64 {
        now.saturating_sub(self.ts)
    }

    /// F5. A future-dated attestation is rejected rather than treated as fresh —
    /// otherwise a clock-skewed or malicious attestor buys unlimited freshness by
    /// stamping far ahead.
    pub fn check_fresh(&self, now: i64) -> CoreResult<()> {
        let age = self.age_secs(now);
        if age < 0 {
            return Err(CoreError::AttestationInFuture { ts: self.ts, now });
        }
        if age > self.ttl_secs {
            return Err(CoreError::StaleAttestation {
                age_secs: age,
                ttl_secs: self.ttl_secs,
            });
        }
        Ok(())
    }

    #[must_use]
    pub fn is_fresh(&self, now: i64) -> bool {
        self.check_fresh(now).is_ok()
    }
}

/// The reserve side of the treasury: what is attested, what is spoken for, and what
/// is outstanding on the ledger.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReserveState {
    pub attestation: Attestation,
    /// Fiat received and sitting in the reserve account but backing nothing —
    /// typically a deposit that cleared the bank and then failed KYC (spec §4.1).
    /// Without this, `attested_reserve` overstates free backing.
    pub encumbered: Thb,
    /// THBC outstanding on the ledger.
    pub supply: Thb,
}

impl ReserveState {
    #[must_use]
    pub const fn new(attestation: Attestation, encumbered: Thb, supply: Thb) -> Self {
        Self {
            attestation,
            encumbered,
            supply,
        }
    }

    /// Backing actually available to issue against: `attested − encumbered`.
    ///
    /// Saturating, not checked: if encumbrance exceeds the attested reserve the
    /// answer is "no free backing", and that must be reportable rather than an
    /// error — it is precisely the state an operator needs to see.
    #[must_use]
    pub fn free_backing(&self) -> Thb {
        self.attestation.reserve.saturating_sub(self.encumbered)
    }

    /// Headroom left under the F1 ceiling.
    #[must_use]
    pub fn headroom(&self) -> Thb {
        self.free_backing().saturating_sub(self.supply)
    }

    /// True when F1 currently holds. Checked *as a state*, independently of any
    /// proposed issuance, so the reconciler can catch a breach that arrived by some
    /// other route — a downward re-attestation, for instance.
    #[must_use]
    pub fn f1_holds(&self) -> bool {
        self.supply <= self.free_backing()
    }

    /// Gate an issuance of `amount`: F5 then F1. Returns the resulting supply.
    ///
    /// Ordering matters and is the ordering of spec §5.2: freshness first, because a
    /// stale `attested_reserve` makes the F1 comparison meaningless rather than
    /// merely conservative.
    pub fn check_issuance(&self, amount: Thb, now: i64) -> CoreResult<Thb> {
        if amount.is_zero() {
            return Err(CoreError::ZeroAmount);
        }
        self.attestation.check_fresh(now)?;

        let resulting = self.supply.checked_add(amount)?;
        let free = self.free_backing();
        if resulting > free {
            return Err(CoreError::ReserveInsufficient {
                requested: amount.minor(),
                resulting_supply: resulting.minor(),
                free_backing: free.minor(),
                attested_reserve: self.attestation.reserve.minor(),
                encumbered: self.encumbered.minor(),
            });
        }
        Ok(resulting)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys() -> TreasuryKeys {
        TreasuryKeys::new(
            KeyId("auth".into()),
            KeyId("attestor".into()),
            KeyId("issuer".into()),
        )
        .unwrap()
    }

    fn state(reserve: u64, encumbered: u64, supply: u64, ts: i64) -> ReserveState {
        ReserveState::new(
            Attestation::new(Thb::from_minor(reserve), ts, 3600),
            Thb::from_minor(encumbered),
            Thb::from_minor(supply),
        )
    }

    // ---- F9 ----------------------------------------------------------------

    #[test]
    fn f9_rejects_attestor_equal_to_authority() {
        let same = KeyId("same".into());
        let err = TreasuryKeys::new(same.clone(), same, KeyId("issuer".into())).unwrap_err();
        assert_eq!(err, CoreError::AttestorIsAuthority);
    }

    #[test]
    fn f9_allows_distinct_keys() {
        assert_ne!(keys().authority, keys().attestor);
    }

    #[test]
    fn f9_does_not_constrain_the_issuer_key() {
        // Spec §3 says B and A *should* not be the same entity, and notes that in the
        // current simulation they are. That is a disclosed limitation, not a check —
        // encoding it as a hard error here would make the simulation unrunnable and
        // hide the disclosure.
        let attestor = KeyId("shared".into());
        assert!(TreasuryKeys::new(KeyId("auth".into()), attestor.clone(), attestor).is_ok());
    }

    // ---- F5 ----------------------------------------------------------------

    #[test]
    fn f5_accepts_attestation_inside_ttl_including_the_boundary() {
        let a = Attestation::new(Thb::from_minor(100), 1_000, 3_600);
        assert!(a.is_fresh(1_000));
        assert!(
            a.is_fresh(4_600),
            "age == ttl must be fresh, matching `age > ttl` on-chain"
        );
    }

    #[test]
    fn f5_halts_issuance_one_second_past_ttl() {
        let s = state(1_000_000, 0, 0, 1_000);
        let err = s.check_issuance(Thb::from_minor(1), 4_601).unwrap_err();
        assert_eq!(
            err,
            CoreError::StaleAttestation {
                age_secs: 3_601,
                ttl_secs: 3_600
            }
        );
    }

    #[test]
    fn f5_resumes_after_a_refresh() {
        let stale = state(1_000_000, 0, 0, 1_000);
        assert!(stale.check_issuance(Thb::from_minor(1), 99_999).is_err());

        let mut refreshed = stale;
        refreshed.attestation.ts = 99_999;
        assert!(refreshed.check_issuance(Thb::from_minor(1), 99_999).is_ok());
    }

    #[test]
    fn f5_rejects_a_future_dated_attestation() {
        let a = Attestation::new(Thb::from_minor(100), 5_000, 3_600);
        assert_eq!(
            a.check_fresh(1_000).unwrap_err(),
            CoreError::AttestationInFuture {
                ts: 5_000,
                now: 1_000
            }
        );
    }

    // ---- F1 ----------------------------------------------------------------

    #[test]
    fn f1_permits_issuance_exactly_to_the_ceiling() {
        let s = state(1_000_000, 0, 400_000, 0);
        assert_eq!(s.headroom().minor(), 600_000);
        let resulting = s.check_issuance(Thb::from_minor(600_000), 0).unwrap();
        assert_eq!(resulting.minor(), 1_000_000);
    }

    #[test]
    fn f1_rejects_one_minor_unit_over_the_ceiling() {
        let s = state(1_000_000, 0, 400_000, 0);
        let err = s.check_issuance(Thb::from_minor(600_001), 0).unwrap_err();
        assert!(matches!(
            err,
            CoreError::ReserveInsufficient {
                free_backing: 1_000_000,
                ..
            }
        ));
    }

    #[test]
    fn f1_ceiling_tightens_by_the_encumbered_amount() {
        // The whole reason `reserve_encumbered` exists (spec §4.1): 200k of the
        // attested reserve is fiat backing nothing, so it must not raise the ceiling.
        let s = state(1_000_000, 200_000, 400_000, 0);
        assert_eq!(s.free_backing().minor(), 800_000);
        assert!(s.check_issuance(Thb::from_minor(400_000), 0).is_ok());
        assert!(s.check_issuance(Thb::from_minor(400_001), 0).is_err());
    }

    #[test]
    fn f1_can_be_breached_by_a_downward_reattestation() {
        // Not hypothetical: `A` re-attesting a lower reserve is exactly what an
        // honest attestor does after `B` fails to wire (spec §6.4). No issuance is
        // involved, so `check_issuance` never runs — only `f1_holds` catches it.
        let mut s = state(1_000_000, 0, 900_000, 0);
        assert!(s.f1_holds());
        s.attestation.reserve = Thb::from_minor(800_000);
        assert!(
            !s.f1_holds(),
            "supply 900k over free backing 800k must report a breach"
        );
        assert_eq!(s.headroom(), Thb::ZERO);
    }

    #[test]
    fn f1_reports_no_free_backing_when_encumbrance_exceeds_the_reserve() {
        let s = state(100, 500, 0, 0);
        assert_eq!(s.free_backing(), Thb::ZERO);
        assert!(s.check_issuance(Thb::from_minor(1), 0).is_err());
    }

    #[test]
    fn zero_amount_issuance_is_rejected_before_any_other_check() {
        let s = state(1_000_000, 0, 0, 1_000);
        // Stale attestation *and* zero amount — zero wins, so the caller gets the
        // error describing their own input.
        assert_eq!(
            s.check_issuance(Thb::ZERO, 999_999).unwrap_err(),
            CoreError::ZeroAmount
        );
    }
}
