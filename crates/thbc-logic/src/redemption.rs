//! `redemption-service` — the off-ramp (spec §6, §9).
//!
//! Owns the F4 barrier and the Δ timer. Read [`RedemptionService::process_payout`]
//! first: it is the only route to a fiat payout in this service, and every other
//! method exists to make sure it is reached in the right state.

use std::sync::Arc;

use thbc_core::money::Thb;
use thbc_core::ports::{
    Clock, LedgerPort, PayoutPort, PortError, PortResult, RedemptionRepository,
};
use thbc_core::redemption::{ConfirmOutcome, Redemption, RedemptionState};
use tracing::{info, instrument, warn};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RedemptionOutcome {
    /// Escrow confirmed on-chain; tokens held, Δ running. Payout not yet queued.
    Escrowed {
        user: String,
        seq: u64,
    },
    /// Escrow submitted but not confirmed. Nothing may proceed from here.
    Pending {
        user: String,
        seq: u64,
    },
    Failed {
        reason: String,
    },
}

pub struct RedemptionService {
    redemptions: Arc<dyn RedemptionRepository>,
    ledger: Arc<dyn LedgerPort>,
    payouts: Arc<dyn PayoutPort>,
    clock: Arc<dyn Clock>,
    /// Δ — how long a holder must wait before reclaiming (spec §6.3).
    delta_secs: i64,
}

impl RedemptionService {
    #[must_use]
    pub fn new(
        redemptions: Arc<dyn RedemptionRepository>,
        ledger: Arc<dyn LedgerPort>,
        payouts: Arc<dyn PayoutPort>,
        clock: Arc<dyn Clock>,
        delta_secs: i64,
    ) -> Self {
        Self {
            redemptions,
            ledger,
            payouts,
            clock,
            delta_secs,
        }
    }

    #[must_use]
    pub const fn delta_secs(&self) -> i64 {
        self.delta_secs
    }

    /// Current time as this service sees it. Exposed so a caller can compute "how
    /// long until reclaim" against the *same* clock the timelock is checked with —
    /// answering that from a second clock would drift from the guard.
    #[must_use]
    pub fn now(&self) -> i64 {
        self.clock.now()
    }

    /// Look one up. Read-only; drives the holder-facing status view.
    pub async fn status(&self, user: &str, seq: u64) -> PortResult<Redemption> {
        self.redemptions
            .find(user, seq)
            .await?
            .ok_or(PortError::NotFound)
    }

    /// Steps 1–2 of §6.1 — the user has signed `redeem_thbc_for_fiat`.
    ///
    /// The user signs; `B` cannot burn on the user's behalf. This service relays a
    /// transaction the user already authorised and never constructs a signer set of
    /// its own (F8).
    #[instrument(skip(self), fields(amount = amount.minor()))]
    pub async fn request(&self, user: &str, amount: Thb) -> PortResult<RedemptionOutcome> {
        let now = self.clock.now();
        let seq = self.redemptions.next_seq(user).await?;
        let mut redemption = Redemption::request(user, seq, amount, self.delta_secs, now)?;
        self.redemptions.insert(&redemption).await?;

        let outcome = self.ledger.escrow_redemption(&redemption).await?;
        redemption.apply_escrow_outcome(outcome, self.clock.now())?;
        self.redemptions.update(&redemption).await?;

        Ok(match redemption.state {
            RedemptionState::Escrowed => {
                info!(user, seq, "escrow confirmed; delta timer started");
                RedemptionOutcome::Escrowed {
                    user: user.into(),
                    seq,
                }
            }
            RedemptionState::Failed => RedemptionOutcome::Failed {
                reason: "escrow transaction failed".into(),
            },
            // Still `Requested`. The ledger said `Submitted`, which grants nothing.
            _ => RedemptionOutcome::Pending {
                user: user.into(),
                seq,
            },
        })
    }

    /// **Step 4 — the F4 barrier.** The only path to a fiat payout.
    ///
    /// Spec §6.2: fiat never leaves ahead of token destruction, and the barrier is
    /// *confirmation*, not RPC acceptance. The state machine refuses every state but
    /// `Escrowed`, so this function cannot queue a wire against an unconfirmed burn
    /// however it is called.
    ///
    /// Ordering within the function matters as much as the guard. The state is
    /// persisted **before** the payout is enqueued: if the process dies between the
    /// two, the recorded state says a payout was queued that may not have been, and
    /// the operator investigates a possible missing wire. The other order loses the
    /// record of a wire that *was* sent, and a double payout is unrecoverable in a
    /// way a double burn is not. [`PayoutPort::enqueue`] is idempotent on
    /// `(user, seq)` to make the retry safe.
    #[instrument(skip(self))]
    pub async fn process_payout(&self, user: &str, seq: u64) -> PortResult<()> {
        let mut redemption = self
            .redemptions
            .find(user, seq)
            .await?
            .ok_or(PortError::NotFound)?;

        // F4. Returns `BurnNotConfirmed` naming the offending state.
        redemption.enqueue_payout()?;
        self.redemptions.update(&redemption).await?;

        self.payouts.enqueue(user, seq, redemption.amount).await?;
        info!(
            user,
            seq,
            amount = redemption.amount.minor(),
            "payout enqueued after confirmed burn"
        );
        Ok(())
    }

    /// Step 5 — `B` wired and called `confirm_redemption`. The burn happens here.
    #[instrument(skip(self))]
    pub async fn confirm(&self, user: &str, seq: u64) -> PortResult<()> {
        let mut redemption = self
            .redemptions
            .find(user, seq)
            .await?
            .ok_or(PortError::NotFound)?;

        let outcome = self.ledger.confirm_redemption(user, seq).await?;
        if outcome != ConfirmOutcome::Confirmed {
            // Do not advance the record. An unconfirmed burn that we recorded as
            // confirmed would drop the amount out of `total_redeemed` and show up as
            // F2 drift — while also silently ending the holder's reclaim right.
            return Err(PortError::Rejected(format!(
                "confirm_redemption returned {outcome:?}"
            )));
        }

        redemption.confirm()?;
        self.redemptions.update(&redemption).await?;
        info!(user, seq, "redemption confirmed; supply decremented");
        Ok(())
    }

    /// F7 — the holder reclaims after Δ.
    ///
    /// Callable by the user at any time; the guard is the timelock, not permission.
    #[instrument(skip(self))]
    pub async fn reclaim(&self, user: &str, seq: u64) -> PortResult<()> {
        let mut redemption = self
            .redemptions
            .find(user, seq)
            .await?
            .ok_or(PortError::NotFound)?;
        let now = self.clock.now();

        // Checked here so a premature attempt never reaches the chain, and again
        // on-chain because that is where it actually binds.
        redemption.reclaim(now)?;

        let outcome = self.ledger.reclaim_redemption(user, seq).await?;
        if outcome != ConfirmOutcome::Confirmed {
            return Err(PortError::Rejected(format!(
                "reclaim_redemption returned {outcome:?}"
            )));
        }

        self.redemptions.update(&redemption).await?;
        warn!(
            user,
            seq,
            amount = redemption.amount.minor(),
            "F7 reclaim: issuer did not confirm within delta — tokens restored, \
             but any fiat B already took is NOT recovered (spec §6.4)"
        );
        Ok(())
    }

    /// The Δ sweep — redemptions the issuer has sat on past the timeout.
    ///
    /// Reports rather than acts. `reclaim_redemption` is user-signed, so this service
    /// cannot call it on a holder's behalf without holding their key, and holding
    /// their key is exactly what F8 forbids. The sweep exists to notify the holder
    /// and to make `B`'s failure visible.
    #[instrument(skip(self))]
    pub async fn sweep_reclaimable(&self) -> PortResult<Vec<Redemption>> {
        let now = self.clock.now();
        let due = self.redemptions.reclaimable(now).await?;
        if !due.is_empty() {
            warn!(
                count = due.len(),
                "redemptions past delta with no issuer confirmation — holders should reclaim"
            );
        }
        Ok(due)
    }

    /// `redemption_queue_len` — publicly readable by the `E` observer node (§6.3), so
    /// an issuer sitting on redemptions is visible without anyone gaining custody.
    pub async fn queue_len(&self) -> PortResult<u32> {
        self.redemptions.pending_count().await
    }
}
