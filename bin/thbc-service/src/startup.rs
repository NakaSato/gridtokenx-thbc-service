//! Dependency wiring.
//!
//! Traits are defined in `thbc-core::ports`, implemented in `thbc-persistence` /
//! `thbc-ledger`, and assembled here — the repo's trait-based DI pattern.
//!
//! This is also where the simulation boundary is enforced. The stub screener and the
//! recording payout queue are test doubles, and they are only reachable in simulated
//! mode. In `chain-bridge` mode the compliance port and the payout port are wired to
//! implementations that **refuse**, because there is no KYC adapter and no fiat rail
//! (spec §12). A service that quietly passed everyone's KYC would be far worse than
//! one that will not start.

use std::sync::Arc;

use anyhow::{Context, Result};
use thbc_api::AppState;
use thbc_core::money::Thb;
use thbc_core::ports::{
    Clock, CompliancePort, DepositRepository, LedgerPort, PayoutPort, RedemptionRepository,
};
use thbc_ledger::{ChainBridgeConfig, ChainBridgeLedger, SimulatedLedger};
use thbc_logic::adapters::{
    RecordingPayoutQueue, StubCompliance, SystemClock, UnavailablePayoutQueue,
};
use thbc_logic::{
    IssuanceService, ReconciliationService, RedemptionService, ReserveService, TreasuryService,
};
use thbc_persistence::{
    InMemoryDepositRepo, InMemoryRedemptionRepo, PgDepositRepository, PgRedemptionRepository,
};
use tracing::{info, warn};

use crate::config::{Config, LedgerMode};

/// Build the application state from config.
pub async fn build(config: &Config) -> Result<AppState> {
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);

    // ---- Persistence ------------------------------------------------------
    let (deposits, redemptions): (Arc<dyn DepositRepository>, Arc<dyn RedemptionRepository>) =
        if let Some(url) = &config.database_url {
            let pool = sqlx::PgPool::connect(url)
                .await
                .context("connect to the THBC database")?;
            thbc_persistence::migrate(&pool)
                .await
                .context("run THBC migrations")?;
            info!("connected to Postgres and applied migrations");
            (
                Arc::new(PgDepositRepository::new(pool.clone())),
                Arc::new(PgRedemptionRepository::new(pool)),
            )
        } else {
            // Only reachable in simulated mode — `Config::validate` rejects a missing
            // DATABASE_URL under chain-bridge.
            warn!(
                "no DATABASE_URL: using in-memory repositories. All deposit and \
                 redemption records are lost on restart, and with them the off-chain \
                 half of F3 and the entire F2 tally."
            );
            (
                Arc::new(InMemoryDepositRepo::new()),
                Arc::new(InMemoryRedemptionRepo::new()),
            )
        };

    // ---- Ledger -----------------------------------------------------------
    let ledger: Arc<dyn LedgerPort> = match config.ledger_mode {
        LedgerMode::ChainBridge => {
            let bridge = ChainBridgeLedger::connect(ChainBridgeConfig {
                nats_url: config.nats_url.clone(),
                grpc_url: config.chain_bridge_grpc_url.clone(),
                ..ChainBridgeConfig::default()
            })
            .await
            .map_err(|e| anyhow::anyhow!("connect to Chain Bridge: {e}"))?;
            info!(nats = %config.nats_url, "ledger: Chain Bridge");
            warn!(
                "issue_thbc, redeem_thbc_for_fiat, confirm_redemption, reclaim_redemption \
                 and the deposit nullifier do not exist on-chain (spec §12). Those routes \
                 will return 501 not_implemented."
            );
            Arc::new(bridge)
        }
        LedgerMode::Simulated => {
            warn!(
                "ledger: SIMULATED. This models the treasury as specified, including \
                 instructions that do not exist. Nothing here is on a chain and no fiat \
                 is involved."
            );
            Arc::new(SimulatedLedger::new(
                Thb::from_minor(config.simulated_reserve_minor),
                config.attestation_ttl_secs,
                clock.now(),
            ))
        }
    };

    // ---- Compliance and payouts -------------------------------------------
    //
    // Both are unimplemented (spec §12). In simulated mode they get doubles so the
    // flows are exercisable; in chain-bridge mode they get implementations that
    // refuse, so the gap cannot be mistaken for a working integration.
    let (compliance, payouts): (Arc<dyn CompliancePort>, Arc<dyn PayoutPort>) =
        if config.is_simulated() {
            (
                Arc::new(StubCompliance::new(Thb::from_minor(
                    config.stub_kyc_ceiling_minor,
                ))),
                Arc::new(RecordingPayoutQueue::new()),
            )
        } else {
            (
                Arc::new(RefusingCompliance),
                Arc::new(UnavailablePayoutQueue),
            )
        };

    // ---- Services ---------------------------------------------------------
    let reserve = Arc::new(ReserveService::new(
        Arc::clone(&ledger),
        Arc::clone(&deposits),
        Arc::clone(&clock),
    ));

    Ok(AppState {
        issuance: Arc::new(IssuanceService::new(
            Arc::clone(&deposits),
            compliance,
            Arc::clone(&ledger),
            Arc::clone(&clock),
        )),
        redemption: Arc::new(RedemptionService::new(
            Arc::clone(&redemptions),
            Arc::clone(&ledger),
            payouts,
            Arc::clone(&clock),
            config.redemption_delta_secs,
        )),
        reconciliation: Arc::new(ReconciliationService::new(
            Arc::clone(&deposits),
            Arc::clone(&redemptions),
            Arc::clone(&reserve),
            Arc::clone(&clock),
        )),
        treasury: Arc::new(TreasuryService::new(ledger)),
        reserve,
        simulated: config.is_simulated(),
    })
}

/// Compliance port for non-simulated mode: refuses every screen.
///
/// There is no KYC adapter (spec §12). The alternatives are to pass everyone — which
/// would make the service appear to do compliance while doing none, and would issue
/// THBC to unscreened subjects — or to fail closed. Fail closed.
struct RefusingCompliance;

#[async_trait::async_trait]
impl CompliancePort for RefusingCompliance {
    async fn screen(
        &self,
        _subject: &str,
        _amount: Thb,
    ) -> thbc_core::ports::PortResult<thbc_core::deposit::ScreenOutcome> {
        Err(thbc_core::ports::PortError::Unsupported(
            "no KYC adapter exists (spec §12); refusing to screen rather than auto-passing",
        ))
    }
}
