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
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();

    let config = Config::from_env().context("load configuration")?;
    log_posture(&config);

    let state = startup::build(&config).await.context("wire dependencies")?;
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
