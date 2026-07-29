//! `reserve-service` and `reconciliation-service` (spec §9).
//!
//! Attestation cadence, `reserve_encumbered` accounting, and the F2 identity check.
//!
//! Neither of these makes the reserve honest. `attested_reserve` is a number a single
//! signer writes, and spec §3 is explicit that every downstream guarantee reduces to
//! that number being true. What these services do is make a *lie* observable — a
//! reserve that stops covering supply shows up in the reconciliation history, which
//! is append-only and readable by `E`.

use std::sync::Arc;

use thbc_core::money::Thb;
use thbc_core::ports::{
    Clock, DepositRepository, LedgerPort, PortError, PortResult, ReconciliationRepository,
    RedemptionRepository,
};
use thbc_core::reconcile::{LedgerTally, ReconciliationReport, Severity, reconcile};
use thbc_core::redemption::ConfirmOutcome;
use thbc_core::reserve::ReserveState;
use tracing::{error, info, instrument, warn};

pub struct ReserveService {
    ledger: Arc<dyn LedgerPort>,
    deposits: Arc<dyn DepositRepository>,
    clock: Arc<dyn Clock>,
}

impl ReserveService {
    #[must_use]
    pub fn new(
        ledger: Arc<dyn LedgerPort>,
        deposits: Arc<dyn DepositRepository>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            ledger,
            deposits,
            clock,
        }
    }

    /// The reserve as this service understands it: the chain's attestation, with
    /// encumbrance from our own deposit records.
    ///
    /// The two halves still come from different places — the attestation is read from
    /// the chain, the encumbrance from our own deposit records, which remain the only
    /// place that knows a deposit cleared the bank and then failed KYC.
    ///
    /// What changed on 2026-07-29: `reserve_encumbered` is now an on-chain field, and
    /// [`ReserveService::attest`] publishes this same number with every attestation.
    /// So the chain enforces `attested_reserve − reserve_encumbered` too, and **this
    /// service is no longer stricter than the chain** — a caller that bypasses it now
    /// hits the same ceiling rather than a looser one. The remaining gap is temporal,
    /// not structural: the chain's copy is as fresh as the last attestation, so an
    /// encumbrance recorded since then is known here first.
    #[instrument(skip(self))]
    pub async fn current(&self) -> PortResult<ReserveState> {
        let snapshot = self.ledger.snapshot().await?;
        let encumbered = self.deposits.total_encumbered().await?;
        Ok(ReserveState::new(
            snapshot.reserve.attestation,
            encumbered,
            snapshot.reserve.supply,
        ))
    }

    /// Available headroom under the effective F1 ceiling.
    pub async fn headroom(&self) -> PortResult<Thb> {
        Ok(self.current().await?.headroom())
    }

    /// Whether issuance can proceed at all right now — used to decide whether to
    /// hold incoming deposits rather than let each one fail individually.
    #[instrument(skip(self))]
    pub async fn is_issuance_open(&self) -> PortResult<bool> {
        let state = self.current().await?;
        let now = self.clock.now();
        Ok(state.attestation.is_fresh(now) && !state.headroom().is_zero())
    }

    /// Push a fresh attestation. Attestor-signed (`A`), never the parameter admin (F9).
    ///
    /// Pre-checked against current supply: a downward re-attestation below outstanding
    /// supply is an F1 breach the moment it lands. It is not blocked — an honest
    /// attestor reporting a genuine shortfall must be able to say so, and suppressing
    /// it would hide exactly the §6.4 failure this system needs to surface — but it is
    /// logged at `error` so it cannot pass unnoticed.
    #[instrument(skip(self), fields(reserve = reserve.minor()))]
    pub async fn attest(&self, reserve: Thb) -> PortResult<()> {
        // The pre-read is ADVISORY: it only logs an F1-breach warning and gates
        // nothing. So an unavailable snapshot must not abort the attestation.
        //
        // This is not hypothetical. Against Chain Bridge, `snapshot` returns
        // `Unsupported` by design — three of the four fields it reports are not
        // on the `Treasury` account, and synthesising them would misreport the
        // F1 ceiling. Propagating that turned every attestation into a 501
        // whose message talked about `reserve_encumbered`, which reads like the
        // attestation itself is unimplemented. It is not: `update_attestation`
        // exists on-chain and is routed.
        //
        // Refusing to attest because we cannot *check* the result is also
        // backwards on its own terms: a stale attestation halts issuance (F5),
        // so blocking the refresh is strictly worse than performing it blind.
        match self.current().await {
            Ok(state) => {
                let free = reserve.saturating_sub(state.encumbered);
                if state.supply > free {
                    error!(
                        supply = state.supply.minor(),
                        free_backing = free.minor(),
                        shortfall = state.supply.saturating_sub(free).minor(),
                        "F1 BREACH: attested reserve does not cover outstanding supply"
                    );
                }
            }
            Err(PortError::Unsupported(why)) => {
                warn!(
                    reason = why,
                    "attesting without the pre-check: the ledger cannot report a snapshot, \
                     so an F1 breach in the NEW value would go unnoticed here (the chain \
                     still enforces its own ceiling on issuance)"
                );
            }
            Err(e) => return Err(e),
        }
        // The outcome is the whole point and must NOT be discarded.
        //
        // This previously did `update_attestation(...).await?;` and then logged
        // "attestation refreshed" unconditionally — so a `Failed` reply from the
        // bridge produced HTTP 200 and a success log while `attested_reserve`
        // stayed 0 on-chain. Reporting an unconfirmed write as done is the
        // accept-on-send weakening spec §6.2 forbids, and it is worse here than
        // for a payout: an operator who believes the attestation landed will not
        // re-send it, and issuance stays halted under F5 for a reason nothing
        // reports.
        // Read the encumbrance from our own deposit records and publish it WITH the
        // reserve, so the chain's F1 ceiling is the same one this service enforces.
        //
        // Unlike the advisory pre-check above, a failure here is fatal to the
        // attestation: attesting a reserve without its encumbrance would write a
        // ceiling that is too loose by exactly the encumbered amount, which is worse
        // than not attesting at all. A missed refresh only halts issuance under F5.
        let encumbered = self.deposits.total_encumbered().await?;
        match self.ledger.update_attestation(reserve, encumbered).await? {
            ConfirmOutcome::Confirmed => {
                info!(reserve = reserve.minor(), "attestation refreshed");
                Ok(())
            }
            // Unknown, not done. The transaction may still land, so this is not
            // `Failed` — but the caller must not treat it as success.
            ConfirmOutcome::Submitted => {
                warn!(
                    reserve = reserve.minor(),
                    "attestation submitted but NOT confirmed; attested_reserve may be unchanged"
                );
                Err(PortError::Transient(
                    "attestation submitted but not confirmed on-chain; re-send to verify".into(),
                ))
            }
            ConfirmOutcome::Failed => {
                error!(
                    reserve = reserve.minor(),
                    "attestation FAILED on-chain; attested_reserve is unchanged and issuance \
                     stays halted under F5"
                );
                Err(PortError::Rejected(
                    "attestation transaction failed on-chain; attested_reserve unchanged".into(),
                ))
            }
        }
    }
}

/// `reconciliation-service` — the F2 identity, checked hourly and daily (spec §9).
pub struct ReconciliationService {
    deposits: Arc<dyn DepositRepository>,
    redemptions: Arc<dyn RedemptionRepository>,
    reserve: Arc<ReserveService>,
    history: Arc<dyn ReconciliationRepository>,
    clock: Arc<dyn Clock>,
}

impl ReconciliationService {
    #[must_use]
    pub fn new(
        deposits: Arc<dyn DepositRepository>,
        redemptions: Arc<dyn RedemptionRepository>,
        reserve: Arc<ReserveService>,
        history: Arc<dyn ReconciliationRepository>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            deposits,
            redemptions,
            reserve,
            history,
            clock,
        }
    }

    /// Runs recorded so far that were not `Ok`.
    ///
    /// A point-in-time reconciliation cannot answer "was this ever broken?" — a drift
    /// that appeared and was resolved reads identically to one that never happened.
    /// This is what makes the history worth keeping rather than just logging.
    pub async fn unhealthy_runs(&self) -> PortResult<u64> {
        self.history.unhealthy_count().await
    }

    /// Run F2 and the F1 solvency check.
    ///
    /// Detective, not preventive: this rejects nothing. It compares what this service
    /// believes it did against what the ledger says happened. That is the honest
    /// description of F2's status and why the invariant registry marks it `Partial`.
    #[instrument(skip(self))]
    pub async fn run(&self) -> PortResult<ReconciliationReport> {
        let issued = self.deposits.total_issued().await?;
        // Confirmed redemptions only. A reclaimed one returned its tokens and never
        // reduced supply; counting it would fabricate drift indistinguishable from an
        // unbacked mint.
        let redeemed = self.redemptions.total_redeemed().await?;
        let state = self.reserve.current().await?;

        let report = reconcile(
            &LedgerTally::new(issued, redeemed),
            &state,
            self.clock.now(),
        )?;

        match report.severity {
            Severity::Ok => info!(supply = report.ledger_supply.minor(), "reconciled clean"),
            Severity::Drift => warn!(
                drift = report.drift,
                expected = report.expected_supply.minor(),
                actual = report.ledger_supply.minor(),
                "F2 DRIFT: issuance records and ledger supply disagree"
            ),
            Severity::Insolvent => error!(
                shortfall = report.shortfall.minor(),
                supply = report.ledger_supply.minor(),
                free_backing = report.free_backing.minor(),
                "F1 BREACH: outstanding THBC exceeds free fiat backing"
            ),
        }

        // Persist BEFORE returning, and do not let a storage failure swallow the
        // report: a caller that got a result must be able to trust it was recorded,
        // and an unrecorded breach is the one an auditor will ask about.
        self.history.record(&report).await?;

        Ok(report)
    }
}
