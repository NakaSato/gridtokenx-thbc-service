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
    Clock, CompliancePort, DepositRepository, LedgerPort, PayoutPort, ReconciliationRepository,
    RedemptionRepository,
};
use thbc_ledger::{ChainBridgeConfig, ChainBridgeLedger, SimulatedLedger};
use thbc_logic::adapters::{
    RecordingPayoutQueue, StubCompliance, SystemClock, UnavailablePayoutQueue,
};
use thbc_logic::{
    IssuanceService, ReconciliationService, RedemptionService, ReserveService, TreasuryService,
};
use thbc_persistence::{
    InMemoryDepositRepo, InMemoryReconciliationRepo, InMemoryRedemptionRepo, PgDepositRepository,
    PgReconciliationRepository, PgRedemptionRepository,
};
use tracing::{info, warn};

use crate::config::{Config, LedgerMode};

/// Build the application state from config.
pub async fn build(config: &Config) -> Result<AppState> {
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);

    let (deposits, redemptions, history) = build_repositories(config).await?;

    // ---- Ledger -----------------------------------------------------------
    let ledger: Arc<dyn LedgerPort> = match config.ledger_mode {
        LedgerMode::ChainBridge => {
            let bridge = ChainBridgeLedger::connect(ChainBridgeConfig {
                nats_url: config.nats_url.clone(),
                grpc_url: config.chain_bridge_grpc_url.clone(),
                service_identity: config.service_identity.clone(),
                treasury_pda: config.treasury_pda.clone(),
                inventory_vault: config.inventory_vault.clone(),
                ..ChainBridgeConfig::default()
            })
            .await
            .map_err(|e| anyhow::anyhow!("connect to Chain Bridge: {e}"))?;
            info!(nats = %config.nats_url, "ledger: Chain Bridge");
            warn!(
                "issue_thbc, update_attestation and confirm_redemption are live. \
                 redeem_thbc_for_fiat and reclaim_redemption return 501 BY DESIGN: both \
                 are user-signed, and a bridge route would sign them with the platform \
                 key — inverting the protection they exist to give the holder."
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
            history,
            Arc::clone(&clock),
        )),
        treasury: Arc::new(TreasuryService::new(ledger)),
        reserve,
        simulated: config.is_simulated(),
    })
}

/// The three repositories, Postgres-backed or in-memory.
///
/// Extracted from [`build`] purely for length. Grouped rather than built inline so the
/// Postgres/in-memory choice is made ONCE — three independent `if let` blocks would let
/// a future edit end up with, say, durable deposits and an in-memory reconciliation
/// history, which would silently defeat the append-only property that history exists
/// for.
type Repositories = (
    Arc<dyn DepositRepository>,
    Arc<dyn RedemptionRepository>,
    Arc<dyn ReconciliationRepository>,
);

async fn build_repositories(config: &Config) -> Result<Repositories> {
    if let Some(url) = &config.database_url {
        run_migrations(config, url).await?;

        let pool = sqlx::PgPool::connect(url)
            .await
            .context("connect to the THBC database")?;
        info!("connected to Postgres");
        Ok((
            Arc::new(PgDepositRepository::new(pool.clone())),
            Arc::new(PgRedemptionRepository::new(pool.clone())),
            Arc::new(PgReconciliationRepository::new(pool)),
        ))
    } else {
        // Only reachable in simulated mode — `Config::validate` rejects a missing
        // DATABASE_URL under chain-bridge.
        warn!(
            "no DATABASE_URL: using in-memory repositories. All deposit and redemption \
             records are lost on restart, and with them the off-chain half of F3, the \
             entire F2 tally, and the reconciliation history."
        );
        Ok((
            Arc::new(InMemoryDepositRepo::new()),
            Arc::new(InMemoryRedemptionRepo::new()),
            Arc::new(InMemoryReconciliationRepo::new()),
        ))
    }
}

/// Apply migrations on a **dedicated, short-lived pool**, then drop it.
///
/// `sqlx::migrate!` takes a Postgres advisory lock for the duration of the run.
/// Advisory locks are session-scoped, and the stack's pooler (pgdog) fronts the
/// runtime database in **transaction** mode — where a session's connection is
/// handed to other transactions between statements, so the lock can be taken on one
/// backend and released against another, or outlive the migration entirely.
///
/// Every other service here solves this the same way: migrate through a separate
/// **session-mode** pooler alias (`gridtokenx_thbc_migrate`), configured in
/// `docker/pgdog/pgdog.toml`. `THBC_MIGRATION_DATABASE_URL` carries it.
///
/// When that variable is unset — running against Postgres directly, as tests and
/// local dev do — this falls back to the runtime URL, which is correct because
/// there is no pooler in between to break the lock.
///
/// The pool is closed before the runtime pool opens so the migration connection is
/// never reused for queries.
async fn run_migrations(config: &Config, runtime_url: &str) -> Result<()> {
    let (url, via) = match &config.migration_database_url {
        Some(u) => (u.as_str(), "session-mode migration alias"),
        None => (runtime_url, "runtime URL (no pooler alias configured)"),
    };

    let pool = sqlx::PgPool::connect(url)
        .await
        .context("connect to the THBC database for migrations")?;
    let result = thbc_persistence::migrate(&pool).await;
    pool.close().await;
    result.context("run THBC migrations")?;

    info!(via, "migrations applied");
    Ok(())
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
