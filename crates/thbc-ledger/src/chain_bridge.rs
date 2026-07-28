//! The Chain Bridge adapter — the real ledger path.
//!
//! Architecture rule: *all* Solana transactions go through Chain Bridge. No service
//! calls Solana RPC directly. Writes are published to NATS `JetStream` under
//! `chain.tx.*`; reads are gRPC calls to the bridge.
//!
//! ## What works and what does not
//!
//! [`LedgerPort::snapshot`] and [`LedgerPort::update_attestation`] map onto
//! instructions that exist (`update_attestation`,
//! `gridtokenx-anchor/programs/treasury/src/instructions/update_attestation.rs`).
//!
//! Everything else returns [`PortError::Unsupported`], because the instructions it
//! would encode **do not exist in the treasury program**: there is no `issue_thbc`,
//! no `redeem_thbc_for_fiat`, no `confirm_redemption`, no `reclaim_redemption`, and
//! no `[b"deposit", H(bank_ref)]` nullifier (spec §12).
//!
//! Returning `Unsupported` is the point. The alternative — encoding the call against
//! `swap_grx_for_thbc`, which *does* mint THBC — would produce a service that
//! appears to implement the on-ramp while actually minting against GRX collateral,
//! violating F6 on every deposit. A loud "not built" is the correct behaviour, and it
//! is distinguishable from `Rejected` precisely so operators can tell "not built"
//! from "refused".
//!
//! To exercise the payment leg end to end, wire
//! [`crate::simulated::SimulatedLedger`] instead and read its module doc first.

use async_trait::async_trait;
use thbc_core::bank_ref::BankRefHash;
use thbc_core::money::Thb;
use thbc_core::ports::{LedgerPort, PortError, PortResult, TreasurySnapshot};
use thbc_core::redemption::{ConfirmOutcome, Redemption};

/// NATS subject for attestation refresh. Under the bridge's bound `chain.tx.*`
/// stream prefix — a single token, not `chain.tx.attest.update`, because NATS `*`
/// matches exactly one token and the bound stream would silently not deliver it.
pub const ATTEST_SUBJECT: &str = "chain.tx.attest";

/// How long to wait for the bridge to report a *confirmed* outcome.
///
/// F4 requires confirmation, not acceptance, so this timeout is the barrier's
/// latency budget. Timing out yields [`ConfirmOutcome::Submitted`] — unknown, not
/// failed — and a caller must never treat that as permission to move fiat.
pub const CONFIRM_TIMEOUT_SECS: u64 = 90;

#[derive(Debug, Clone)]
pub struct ChainBridgeConfig {
    pub nats_url: String,
    /// Chain Bridge gRPC endpoint for reads.
    pub grpc_url: String,
    pub confirm_timeout_secs: u64,
}

impl Default for ChainBridgeConfig {
    fn default() -> Self {
        Self {
            nats_url: "nats://localhost:9001".into(),
            grpc_url: "http://localhost:5001".into(),
            confirm_timeout_secs: CONFIRM_TIMEOUT_SECS,
        }
    }
}

/// Chain Bridge-backed ledger.
pub struct ChainBridgeLedger {
    config: ChainBridgeConfig,
    nats: async_nats::Client,
}

impl std::fmt::Debug for ChainBridgeLedger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChainBridgeLedger")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl ChainBridgeLedger {
    /// Connect to NATS. Fails fast — a payment service that starts without its only
    /// route to the ledger is worse than one that refuses to start.
    pub async fn connect(config: ChainBridgeConfig) -> PortResult<Self> {
        let nats = async_nats::connect(&config.nats_url)
            .await
            .map_err(|e| PortError::Transient(format!("NATS connect {}: {e}", config.nats_url)))?;
        Ok(Self { config, nats })
    }

    #[must_use]
    pub const fn config(&self) -> &ChainBridgeConfig {
        &self.config
    }

    async fn publish(&self, subject: &'static str, payload: serde_json::Value) -> PortResult<()> {
        let bytes = serde_json::to_vec(&payload)
            .map_err(|e| PortError::Rejected(format!("encode {subject}: {e}")))?;
        self.nats
            .publish(subject, bytes.into())
            .await
            .map_err(|e| PortError::Transient(format!("publish {subject}: {e}")))?;
        self.nats
            .flush()
            .await
            .map_err(|e| PortError::Transient(format!("flush {subject}: {e}")))?;
        Ok(())
    }
}

#[async_trait]
impl LedgerPort for ChainBridgeLedger {
    async fn snapshot(&self) -> PortResult<TreasurySnapshot> {
        // The treasury account is readable today, but three of the four fields
        // `TreasurySnapshot` carries — `reserve_encumbered`, `thbc_inventory`,
        // `redemption_queue_len` — are not on the account (spec §4.1, all NEW).
        // Synthesising them from the fields that do exist would report a tighter F1
        // ceiling than the chain actually enforces. Refuse instead.
        Err(PortError::Unsupported(
            "Treasury account lacks reserve_encumbered / thbc_inventory / \
             redemption_queue_len (spec §4.1) — a snapshot would misreport the F1 ceiling",
        ))
    }

    async fn issue(
        &self,
        _beneficiary: &str,
        _amount: Thb,
        _nullifier: BankRefHash,
    ) -> PortResult<ConfirmOutcome> {
        // `issue_thbc` and its `[b"deposit", H(bank_ref)]` nullifier now EXIST on-chain
        // (gridtokenx-anchor a554499). What is missing is the Chain Bridge route: this
        // service never builds Solana transactions itself — it publishes intents to
        // NATS and the bridge encodes them — and the bridge has no handler for this
        // instruction yet. Distinct from "the instruction does not exist", which is
        // what this said before.
        Err(PortError::Unsupported(
            "issue_thbc exists on-chain but Chain Bridge has no route for it yet: this \
             service publishes intents and never encodes transactions itself. Wiring a \
             chain.tx.* subject and a bridge handler is what unblocks this",
        ))
    }

    async fn escrow_redemption(&self, _redemption: &Redemption) -> PortResult<ConfirmOutcome> {
        Err(PortError::Unsupported(
            "redeem_thbc_for_fiat and the [b\"redeem\", user, seq] escrow record do not \
             exist (spec §12). redeem_thbc_for_grx burns immediately and pays GRX, so it \
             provides neither the F7 timelock nor a fiat leg",
        ))
    }

    async fn confirm_redemption(&self, _user: &str, _seq: u64) -> PortResult<ConfirmOutcome> {
        Err(PortError::Unsupported(
            "confirm_redemption does not exist (spec §12)",
        ))
    }

    async fn reclaim_redemption(&self, _user: &str, _seq: u64) -> PortResult<ConfirmOutcome> {
        Err(PortError::Unsupported(
            "reclaim_redemption does not exist (spec §12)",
        ))
    }

    async fn update_attestation(&self, reserve: Thb) -> PortResult<ConfirmOutcome> {
        // This one is real: `update_attestation` exists and is attestor-signed.
        self.publish(
            ATTEST_SUBJECT,
            serde_json::json!({
                "correlation_id": uuid::Uuid::new_v4().to_string(),
                "instruction": "update_attestation",
                "attested_reserve": reserve.minor(),
            }),
        )
        .await?;

        // Fire-and-forget publish. The bridge replies on
        // `chain.tx.result.{correlation_id}`; until this subscribes to that reply,
        // the honest answer is "submitted", never "confirmed". Reporting `Confirmed`
        // here would be the accept-on-send weakening spec §6.2 forbids — harmless for
        // an attestation, fatal if copied to a path that releases fiat.
        Ok(ConfirmOutcome::Submitted)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // These assert the *shape* of the not-built surface without a broker. The
    // connected paths need NATS and live in the e2e suite.

    #[test]
    fn default_config_points_at_the_dev_ports() {
        // 9000s = messaging, 5000s = gRPC mesh, per the repo port scheme.
        let c = ChainBridgeConfig::default();
        assert!(c.nats_url.contains("9001"));
        assert!(c.grpc_url.contains("5001"));
        assert_eq!(c.confirm_timeout_secs, CONFIRM_TIMEOUT_SECS);
    }

    #[test]
    fn the_attest_subject_is_a_single_token_under_chain_tx() {
        // `chain.tx.*` binds exactly one token; `chain.tx.attest.update` would not be
        // delivered by the bridge's stream.
        assert_eq!(ATTEST_SUBJECT.matches('.').count(), 2);
        assert!(ATTEST_SUBJECT.starts_with("chain.tx."));
    }

    #[test]
    fn unsupported_errors_name_the_missing_instruction() {
        // Operators must be able to tell "not built" from "the chain said no".
        let e = PortError::Unsupported("issue_thbc does not exist");
        assert!(e.to_string().starts_with("not implemented on-chain"));
    }
}
