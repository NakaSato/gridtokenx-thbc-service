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
use thbc_core::ports::{Clock, DepositRepository, LedgerPort, PortResult, RedemptionRepository};
use thbc_core::reconcile::{LedgerTally, ReconciliationReport, Severity, reconcile};
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
    /// The two halves come from different places on purpose, and that is a disclosed
    /// weakness rather than a design flourish. `reserve_encumbered` is not an
    /// on-chain field (spec §4.1 marks it NEW), so the chain's F1 ceiling is
    /// `attested_reserve` while the ceiling reported here is
    /// `attested_reserve − encumbered`. **This service is stricter than the chain.**
    /// A caller that bypasses it gets the looser ceiling.
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
        let state = self.current().await?;
        let free = reserve.saturating_sub(state.encumbered);
        if state.supply > free {
            error!(
                supply = state.supply.minor(),
                free_backing = free.minor(),
                shortfall = state.supply.saturating_sub(free).minor(),
                "F1 BREACH: attested reserve does not cover outstanding supply"
            );
        }
        self.ledger.update_attestation(reserve).await?;
        info!(reserve = reserve.minor(), "attestation refreshed");
        Ok(())
    }
}

/// `reconciliation-service` — the F2 identity, checked hourly and daily (spec §9).
pub struct ReconciliationService {
    deposits: Arc<dyn DepositRepository>,
    redemptions: Arc<dyn RedemptionRepository>,
    reserve: Arc<ReserveService>,
    clock: Arc<dyn Clock>,
}

impl ReconciliationService {
    #[must_use]
    pub fn new(
        deposits: Arc<dyn DepositRepository>,
        redemptions: Arc<dyn RedemptionRepository>,
        reserve: Arc<ReserveService>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            deposits,
            redemptions,
            reserve,
            clock,
        }
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

        Ok(report)
    }
}
