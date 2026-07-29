//! THBC settlement service — the payment leg of `GridTokenX`.
//!
//! How Thai baht enters and leaves the ledger. Specified in
//! `docs/product-specs/THBC_ISSUER_SERVICE.md`.
//!
//! **Read this before deploying anything.** Spec §12: `issue_thbc`,
//! `redeem_thbc_for_fiat`, the deposit nullifier, the redemption escrow and
//! `reserve_encumbered` do not exist on-chain, and no fiat rail of any kind exists.
//! In `chain-bridge` mode this service starts, serves its read surface, and returns
//! `501 not_implemented` for most of the payment leg — which is the accurate answer.
//! In `simulated` mode everything works against an in-memory model of the treasury as
//! specified, and none of it is real.
//!
//! `GET /v1/admin/invariants` is the authoritative statement of what is actually
//! enforced. Prefer it to any prose, including this comment.

// Test code asserts that guards fire; unwrapping is the assertion.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

mod config;
mod startup;

use anyhow::{Context, Result};
use config::Config;
use thbc_core::invariant::{INVARIANTS, disclosed_gaps};
use tokio::net::TcpListener;
use tokio::signal;
use tower_http::trace::TraceLayer;
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();

    let config = Config::from_env().context("load configuration")?;
    log_posture(&config);

    let state = startup::build(&config).await.context("wire dependencies")?;
    spawn_reconciler(&state, config.reconcile_interval_secs);
    let app = thbc_api::router(state).layer(TraceLayer::new_for_http());

    let addr = format!("0.0.0.0:{}", config.http_port);
    let listener = TcpListener::bind(&addr)
        .await
        .with_context(|| format!("bind {addr}"))?;
    info!(%addr, "THBC settlement service listening");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("serve")?;

    info!("shut down cleanly");
    Ok(())
}

fn init_tracing() {
    // Structured JSON, `tracing` not `log`, matching every other service here.
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();
}

/// Log what this deployment does and does not guarantee, at startup, every time.
///
/// A reader of the logs should not have to find the spec to learn that six of nine
/// invariants are not enforced. Cheap to print, and the alternative is an operator
/// assuming the payment leg works because the process came up healthy.
fn log_posture(config: &Config) {
    let claimable: Vec<&str> = INVARIANTS
        .iter()
        .filter(|i| i.status.is_claimable())
        .map(|i| i.id)
        .collect();
    let gaps: Vec<&str> = disclosed_gaps().iter().map(|i| i.id).collect();

    info!(
        mode = ?config.ledger_mode,
        delta_secs = config.redemption_delta_secs,
        guaranteed = ?claimable,
        not_guaranteed = ?gaps,
        "THBC posture"
    );
    warn!(
        "GridTokenX is not the issuer, holds no fiat, and holds no user keys. \
         No fiat rail exists (spec §12). See GET /v1/admin/invariants."
    );
}

/// Run the F2 reconciliation on a fixed interval, and report redemptions past Δ.
///
/// **This did not exist until 2026-07-29.** `ReconciliationService::run` and
/// `sweep_reclaimable` were reachable only by calling the admin endpoints, so spec
/// §9's "checked hourly and daily" was not true of any deployment, and no holder was
/// ever told their redemption had gone unconfirmed. Both were built, documented as
/// operating, and never scheduled.
///
/// Failures are logged and the loop continues. A reconciler that dies on the first
/// transient database error is worse than useless: it stops reporting drift precisely
/// when something has started going wrong, and its silence is indistinguishable from
/// "all clear".
///
/// The first run happens immediately rather than after one interval, so a deployment
/// with a stale or drifting ledger says so at boot instead of an hour later.
fn spawn_reconciler(state: &thbc_api::AppState, interval_secs: u64) {
    if interval_secs == 0 {
        warn!("reconciler DISABLED (THBC_RECONCILE_INTERVAL_SECS=0) — F2 drift will go unreported");
        return;
    }

    let reconciliation = std::sync::Arc::clone(&state.reconciliation);
    let issuance = std::sync::Arc::clone(&state.issuance);
    let redemption = std::sync::Arc::clone(&state.redemption);
    info!(interval_secs, "reconciler scheduled");

    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(interval_secs));
        // Skip missed ticks rather than firing a burst to catch up: reconciliation is
        // a point-in-time check, so ten backlogged runs would report the same state
        // ten times and say nothing extra.
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        // `snapshot` returns `Unsupported` in chain-bridge mode until the Treasury
        // account carries reserve_encumbered / thbc_inventory / redemption_queue_len
        // (spec §4.1). Without this latch the loop would log an ERROR every tick,
        // forever, for a condition that is a known gap rather than a fault — and an
        // hourly error nobody can act on is how operators learn to ignore the one
        // alarm that matters. Report it once, then stay quiet until it changes.
        let mut unsupported_reported = false;

        loop {
            ticker.tick().await;

            match reconciliation.run().await {
                // `run` already logs at the right level per severity — info/warn/error
                // — so re-logging the healthy case here would just be noise.
                Ok(_) => unsupported_reported = false,
                Err(thbc_core::ports::PortError::Unsupported(why)) => {
                    if !unsupported_reported {
                        warn!(
                            "reconciliation is UNAVAILABLE in this configuration and F2 drift \
                             will go undetected: {why}. This message is logged once, not per \
                             tick; it will repeat if reconciliation starts working and fails \
                             again."
                        );
                        unsupported_reported = true;
                    }
                }
                Err(e) => error!("reconciliation run failed: {e}"),
            }

            // Spec §5.4's "held, retried after refresh". The refresh is what unsticks
            // them, and nothing else polls — so this is the only thing that ever
            // releases a deposit held during a stale-attestation window.
            //
            // `match_same_arms` fires on the two silent arms below and merging them is
            // exactly wrong: "nothing was held" and "the reserve snapshot is unavailable"
            // are opposite conditions that happen to share an empty body. Merged, the
            // next person to give one of them a log line has to re-derive the split.
            #[allow(clippy::match_same_arms)]
            match issuance.retry_all_held().await {
                // Nothing was held — the common case, and silent on purpose. Logging it
                // hourly would bury the retried/issued line that actually matters.
                Ok((0, _)) => {}
                Ok((retried, issued)) => {
                    info!(retried, issued, "retried held deposits");
                }
                // Same `Unsupported` case: the retry needs a reserve snapshot too. It
                // is silent here rather than latched separately, because the
                // reconciliation warning above already names the cause and this would
                // only restate it.
                Err(thbc_core::ports::PortError::Unsupported(_)) => {}
                Err(e) => error!("held-deposit retry sweep failed: {e}"),
            }

            match redemption.sweep_reclaimable().await {
                Ok(due) if !due.is_empty() => {
                    // `sweep_reclaimable` warns with the count; this adds the identities
                    // an operator needs to actually contact the holders. It cannot
                    // reclaim for them — that needs their key, which is what F8 is
                    // about — so notification is the whole of the remedy available here.
                    for r in &due {
                        warn!(
                            user = %r.user,
                            seq = r.seq,
                            amount = r.amount.minor(),
                            "redemption past delta with no issuer confirmation — holder may reclaim"
                        );
                    }
                }
                Ok(_) => {}
                Err(e) => error!("reclaimable sweep failed: {e}"),
            }
        }
    });
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut sig) = signal::unix::signal(signal::unix::SignalKind::terminate()) {
            sig.recv().await;
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => {}
        () = terminate => {}
    }

    // In-flight requests drain. Nothing here is a two-phase commit: a redemption
    // interrupted between its burn and its payout row stays in `escrowed`, which is
    // recoverable — the holder can still reclaim after Δ. That is F7 doing its job on
    // a crash as well as on a dishonest issuer.
    info!("shutdown signal received; draining");
}
