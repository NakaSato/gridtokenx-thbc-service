//! The fiat withdrawal (off-ramp) state machine — spec §6.
//!
//! Two invariants live here and they pull in opposite directions.
//!
//! **F4 (burn-before-wire)**: fiat must never leave ahead of token destruction. The
//! barrier is *confirmation*, not RPC acceptance — the same rule as
//! `build_and_submit_generation_mint`, which replies success only on
//! `ConfirmOutcome::Confirmed`. [`Redemption::enqueue_payout`] is the only route to a
//! payout and it refuses every state but [`RedemptionState::Escrowed`].
//!
//! **F7 (redemption liveness)**: steps 1–2 are irreversible and on-chain; steps 4–5
//! are a promise by `B`. So the burn is not immediate — the amount moves into a
//! record timelocked for Δ, and if `confirm_redemption` has not been called by Δ the
//! user reclaims. The user is then no worse off than before and `B`'s failure is
//! visible on-chain.
//!
//! What this does **not** solve, and must not be claimed to: if `B` takes the fiat
//! and never wires, the user recovers their THBC but the reserve is short, and F1 is
//! violated at the next honest attestation. See spec §6.4.

use serde::{Deserialize, Serialize};

use crate::error::{CoreError, CoreResult};
use crate::money::Thb;

/// Whether the chain reported a *confirmed* effect or merely accepted the submission.
///
/// The distinction is the entire content of F4. Mirrors the Chain Bridge outcome of
/// the same name; kept as its own type here so the core does not depend on the
/// bridge, and so `Submitted` can never be silently coerced to success.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfirmOutcome {
    /// The validator accepted the transaction. **Not** a guarantee it lands.
    Submitted,
    /// Confirmed on-chain. Only this may release fiat.
    Confirmed,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RedemptionState {
    /// User signed `redeem_thbc_for_fiat`; transaction submitted, not yet confirmed.
    /// Nothing may happen off-chain from here — this is the state F4 exists to stop.
    Requested,
    /// Escrow confirmed on-chain. Tokens are held in the redemption record, not
    /// burned, and the Δ timer runs from here.
    Escrowed,
    /// Fiat payout enqueued to the burn-service queue. Reached only from `Escrowed`.
    PayoutQueued,
    /// `B` wired and called `confirm_redemption`; tokens burned, supply decremented.
    /// Terminal.
    Confirmed,
    /// Δ elapsed with no confirm; user called `reclaim_redemption`. THBC restored,
    /// supply unchanged. Terminal.
    Reclaimed,
    /// The escrow transaction itself failed. No tokens moved. Terminal.
    Failed,
}

impl RedemptionState {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Requested => "requested",
            Self::Escrowed => "escrowed",
            Self::PayoutQueued => "payout_queued",
            Self::Confirmed => "confirmed",
            Self::Reclaimed => "reclaimed",
            Self::Failed => "failed",
        }
    }

    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Confirmed | Self::Reclaimed | Self::Failed)
    }

    /// States that count toward the publicly-readable `redemption_queue_len`.
    /// An issuer sitting on redemptions is visible to `E` through this count (§6.3).
    #[must_use]
    pub const fn is_pending(self) -> bool {
        matches!(self, Self::Escrowed | Self::PayoutQueued)
    }
}

/// One redemption, from the user's signature to fiat or reclaim.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Redemption {
    /// `[b"redeem", user, seq]`.
    pub user: String,
    pub seq: u64,
    pub amount: Thb,
    pub state: RedemptionState,
    pub requested_at: i64,
    /// When the escrow confirmed. The Δ clock starts here, not at `requested_at` —
    /// time spent waiting for confirmation is not the issuer's to spend.
    pub escrowed_at: Option<i64>,
    /// Δ in seconds.
    pub delta_secs: i64,
}

impl Redemption {
    pub fn request(
        user: impl Into<String>,
        seq: u64,
        amount: Thb,
        delta_secs: i64,
        now: i64,
    ) -> CoreResult<Self> {
        if amount.is_zero() {
            return Err(CoreError::ZeroAmount);
        }
        Ok(Self {
            user: user.into(),
            seq,
            amount,
            state: RedemptionState::Requested,
            requested_at: now,
            escrowed_at: None,
            delta_secs,
        })
    }

    fn reject(&self, to: RedemptionState) -> CoreError {
        CoreError::InvalidRedemptionTransition {
            from: self.state.as_str(),
            to: to.as_str(),
        }
    }

    /// Step 2/3 — the escrow result came back from the chain.
    ///
    /// `Submitted` is deliberately **not** accepted. An accept-on-send here would
    /// weaken F4 to exactly the thing spec §6.2 says not to do.
    pub fn apply_escrow_outcome(&mut self, outcome: ConfirmOutcome, now: i64) -> CoreResult<()> {
        if self.state != RedemptionState::Requested {
            return Err(self.reject(RedemptionState::Escrowed));
        }
        match outcome {
            ConfirmOutcome::Confirmed => {
                self.state = RedemptionState::Escrowed;
                self.escrowed_at = Some(now);
                Ok(())
            }
            ConfirmOutcome::Failed => {
                self.state = RedemptionState::Failed;
                Ok(())
            }
            // Stay in `Requested` and keep waiting. Not an error — it is the normal
            // intermediate reply — but it grants nothing.
            ConfirmOutcome::Submitted => Ok(()),
        }
    }

    /// Step 4 — enqueue the THB payout. **The F4 barrier.**
    ///
    /// The only transition into a state where fiat moves, and it is reachable only
    /// from a confirmed escrow.
    pub fn enqueue_payout(&mut self) -> CoreResult<()> {
        if self.state != RedemptionState::Escrowed {
            return Err(CoreError::BurnNotConfirmed {
                state: self.state.as_str(),
            });
        }
        self.state = RedemptionState::PayoutQueued;
        Ok(())
    }

    /// Step 5 — `B` wired and called `confirm_redemption`. Tokens burn here; this is
    /// where supply actually decreases.
    pub fn confirm(&mut self) -> CoreResult<()> {
        match self.state {
            // Both are legitimate: the issuer may confirm a wire for a redemption
            // whose payout row this service has not yet marked queued.
            RedemptionState::Escrowed | RedemptionState::PayoutQueued => {
                self.state = RedemptionState::Confirmed;
                Ok(())
            }
            _ => Err(self.reject(RedemptionState::Confirmed)),
        }
    }

    /// Seconds since the escrow confirmed, or `None` if it never did.
    #[must_use]
    pub fn escrow_age(&self, now: i64) -> Option<i64> {
        self.escrowed_at.map(|t| now.saturating_sub(t))
    }

    /// Whether the holder may reclaim at `now` (F7).
    #[must_use]
    pub fn is_reclaimable(&self, now: i64) -> bool {
        self.state.is_pending()
            && self
                .escrow_age(now)
                .is_some_and(|age| age >= self.delta_secs)
    }

    /// F7 — after Δ with no confirm, restore the THBC to the holder.
    ///
    /// Reclaimable from `PayoutQueued` as well as `Escrowed`: this service having
    /// *queued* a payout is not evidence `B` ever sent one, and the holder's
    /// recovery right cannot depend on the issuer's own bookkeeping.
    pub fn reclaim(&mut self, now: i64) -> CoreResult<()> {
        if !self.state.is_pending() {
            return Err(self.reject(RedemptionState::Reclaimed));
        }
        let elapsed = self.escrow_age(now).unwrap_or(0);
        if elapsed < self.delta_secs {
            return Err(CoreError::TimelockNotExpired {
                elapsed_secs: elapsed,
                delta_secs: self.delta_secs,
            });
        }
        self.state = RedemptionState::Reclaimed;
        Ok(())
    }

    /// Effect on `thbc_supply`. Only a confirmed redemption reduces supply — a
    /// reclaim returns the tokens, so supply is unchanged. F2 depends on this being
    /// exactly right.
    #[must_use]
    pub fn supply_reduction(&self) -> Thb {
        match self.state {
            RedemptionState::Confirmed => self.amount,
            _ => Thb::ZERO,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DELTA: i64 = 86_400; // 24h

    fn redemption() -> Redemption {
        Redemption::request("user-1", 1, Thb::from_baht(500).unwrap(), DELTA, 0).unwrap()
    }

    fn escrowed(at: i64) -> Redemption {
        let mut r = redemption();
        r.apply_escrow_outcome(ConfirmOutcome::Confirmed, at)
            .unwrap();
        r
    }

    // ---- F4: burn-before-wire ------------------------------------------------

    #[test]
    fn f4_refuses_payout_before_the_escrow_is_confirmed() {
        let mut r = redemption();
        let err = r.enqueue_payout().unwrap_err();
        assert_eq!(err, CoreError::BurnNotConfirmed { state: "requested" });
    }

    #[test]
    fn f4_refuses_payout_on_accept_on_send() {
        // Spec §6.2: the barrier is confirmation, not RPC acceptance. A `Submitted`
        // reply must buy nothing at all.
        let mut r = redemption();
        r.apply_escrow_outcome(ConfirmOutcome::Submitted, 10)
            .unwrap();
        assert_eq!(r.state, RedemptionState::Requested);
        assert!(
            r.escrowed_at.is_none(),
            "the delta clock must not start on a submit"
        );
        assert_eq!(
            r.enqueue_payout().unwrap_err(),
            CoreError::BurnNotConfirmed { state: "requested" }
        );
    }

    #[test]
    fn f4_allows_payout_once_confirmed() {
        let mut r = escrowed(10);
        r.enqueue_payout().unwrap();
        assert_eq!(r.state, RedemptionState::PayoutQueued);
    }

    #[test]
    fn f4_refuses_payout_after_a_failed_escrow() {
        let mut r = redemption();
        r.apply_escrow_outcome(ConfirmOutcome::Failed, 10).unwrap();
        assert_eq!(r.state, RedemptionState::Failed);
        assert!(r.enqueue_payout().is_err());
    }

    #[test]
    fn f4_refuses_a_second_payout_for_the_same_redemption() {
        let mut r = escrowed(10);
        r.enqueue_payout().unwrap();
        assert_eq!(
            r.enqueue_payout().unwrap_err(),
            CoreError::BurnNotConfirmed {
                state: "payout_queued"
            }
        );
    }

    // ---- F7: reclaim liveness ------------------------------------------------

    #[test]
    fn f7_reclaim_fails_before_delta() {
        let mut r = escrowed(0);
        let err = r.reclaim(DELTA - 1).unwrap_err();
        assert_eq!(
            err,
            CoreError::TimelockNotExpired {
                elapsed_secs: DELTA - 1,
                delta_secs: DELTA
            }
        );
        assert!(
            r.state.is_pending(),
            "a refused reclaim must leave the record pending"
        );
    }

    #[test]
    fn f7_reclaim_succeeds_exactly_at_delta() {
        let mut r = escrowed(0);
        assert!(r.is_reclaimable(DELTA));
        r.reclaim(DELTA).unwrap();
        assert_eq!(r.state, RedemptionState::Reclaimed);
    }

    #[test]
    fn f7_confirm_blocks_reclaim() {
        let mut r = escrowed(0);
        r.confirm().unwrap();
        assert!(!r.is_reclaimable(DELTA * 10));
        assert!(
            r.reclaim(DELTA * 10).is_err(),
            "a confirmed burn cannot be undone"
        );
    }

    #[test]
    fn f7_reclaim_survives_the_issuer_queueing_a_payout_it_never_sent() {
        // The holder's recovery right cannot depend on this service's own bookkeeping.
        let mut r = escrowed(0);
        r.enqueue_payout().unwrap();
        r.reclaim(DELTA).unwrap();
        assert_eq!(r.state, RedemptionState::Reclaimed);
    }

    #[test]
    fn f7_delta_runs_from_confirmation_not_from_the_request() {
        // Time spent waiting for the chain is not the issuer's to spend.
        let mut r = redemption(); // requested at t=0
        r.apply_escrow_outcome(ConfirmOutcome::Confirmed, 1_000)
            .unwrap();
        assert!(
            !r.is_reclaimable(DELTA),
            "delta measured from t=1000, not t=0"
        );
        assert!(r.is_reclaimable(1_000 + DELTA));
    }

    #[test]
    fn f7_reclaim_is_not_possible_before_the_escrow_confirms() {
        let mut r = redemption();
        assert!(!r.is_reclaimable(DELTA * 100));
        assert!(r.reclaim(DELTA * 100).is_err());
    }

    // ---- supply effects (feeds F2) -------------------------------------------

    #[test]
    fn only_a_confirmed_redemption_reduces_supply() {
        let mut confirmed = escrowed(0);
        confirmed.confirm().unwrap();
        assert_eq!(confirmed.supply_reduction(), Thb::from_baht(500).unwrap());

        let mut reclaimed = escrowed(0);
        reclaimed.reclaim(DELTA).unwrap();
        assert_eq!(
            reclaimed.supply_reduction(),
            Thb::ZERO,
            "reclaimed tokens are returned"
        );

        assert_eq!(
            escrowed(0).supply_reduction(),
            Thb::ZERO,
            "escrowed is not yet burned"
        );
        assert_eq!(redemption().supply_reduction(), Thb::ZERO);
    }

    #[test]
    fn pending_states_are_the_publicly_observable_queue() {
        assert!(escrowed(0).state.is_pending());
        let mut queued = escrowed(0);
        queued.enqueue_payout().unwrap();
        assert!(queued.state.is_pending());

        let mut done = escrowed(0);
        done.confirm().unwrap();
        assert!(!done.state.is_pending());
        assert!(
            !redemption().state.is_pending(),
            "unconfirmed escrow is not yet a liability of B"
        );
    }

    #[test]
    fn zero_amount_redemptions_are_refused() {
        assert_eq!(
            Redemption::request("u", 1, Thb::ZERO, DELTA, 0).unwrap_err(),
            CoreError::ZeroAmount
        );
    }

    #[test]
    fn a_terminal_redemption_accepts_no_further_outcomes() {
        let mut r = escrowed(0);
        r.confirm().unwrap();
        assert!(
            r.apply_escrow_outcome(ConfirmOutcome::Confirmed, 5)
                .is_err()
        );
        assert!(r.confirm().is_err());
    }
}
