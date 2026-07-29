//! The Chain Bridge adapter — the real ledger path.
//!
//! Architecture rule: *all* Solana transactions go through Chain Bridge. No service
//! calls Solana RPC directly. Writes are published to NATS `JetStream` under
//! `chain.tx.*`; the bridge builds, signs, and submits, then replies on a
//! per-request subject.
//!
//! This service carries **no Solana types at all** — no `Pubkey`, no
//! `solana-sdk`, no `anchor`. That is not incidental. Invariant F8 (non-custody)
//! rests on the structural fact that no key here can move user funds, and the
//! cheapest way to keep a structural claim true is to make the alternative
//! impossible to write. It sends semantic intent — beneficiary wallet, minor
//! units, bank-ref hash — and the bridge encodes it.
//!
//! ## What works and what does not
//!
//! [`LedgerPort::issue`] and [`LedgerPort::update_attestation`] are live: both
//! map onto instructions that exist (`treasury::issue_thbc`,
//! `treasury::update_attestation`) **and** onto Chain Bridge routes that consume
//! them (`chain.tx.issuethbc`, `chain.tx.attest`). Note the second half of that
//! sentence: `chain.tx.attest` was published here long before any consumer
//! pulled it, so every attestation was captured by the bridge's stream and then
//! silently aged out. An instruction existing on-chain is not enough.
//!
//! The redemption methods and [`LedgerPort::snapshot`] still return
//! [`PortError::Unsupported`]. `snapshot` refuses for its own reason — three of
//! the four fields `TreasurySnapshot` carries are not on the `Treasury` account,
//! and synthesising them would report a tighter F1 ceiling than the chain
//! enforces.
//!
//! Returning `Unsupported` is the point. The alternative — encoding a redemption
//! against `exchange_thbc_for_grx`, which pays *GRX* — would produce a service
//! that appears to implement the off-ramp while actually settling in a volatile
//! asset. A loud "not built" is the correct behaviour, and it is distinguishable
//! from `Rejected` precisely so operators can tell "not built" from "refused".
//!
//! To exercise the payment leg without a broker, wire
//! [`crate::simulated::SimulatedLedger`] instead and read its module doc first.

use async_trait::async_trait;
use futures::StreamExt as _;
use gridtokenx_blockchain_types::envelope_auth::{
    EnvelopeSigner, canonical_issue_thbc_bytes, canonical_update_attestation_bytes,
};
use gridtokenx_blockchain_types::nats_schema::{
    IssueOutcome, IssueThbcMessage, IssueThbcResultMessage, UpdateAttestationMessage,
    UpdateAttestationResultMessage,
};
use thbc_core::bank_ref::BankRefHash;
use thbc_core::money::Thb;
use thbc_core::ports::{LedgerPort, PortError, PortResult, TreasurySnapshot};
use thbc_core::redemption::{ConfirmOutcome, Redemption};

/// NATS subject for attestation refresh.
///
/// Re-exported from the shared schema rather than retyped: the bridge's consumer
/// reads the same constant, so publisher and consumer cannot drift. A single
/// token under `chain.tx.` — NATS `*` matches exactly one token, so
/// `chain.tx.attest.update` would be captured by the bound stream and never
/// delivered.
pub use gridtokenx_blockchain_types::nats_schema::{ATTEST_SUBJECT, ISSUE_THBC_SUBJECT};

/// How long to wait for the bridge to report a *confirmed* outcome.
///
/// F4 requires confirmation, not acceptance, so this timeout is the barrier's
/// latency budget. Timing out yields [`ConfirmOutcome::Submitted`] — unknown, not
/// failed — and a caller must never treat that as permission to move fiat.
///
/// Stays below the bridge's `CHAIN_TX` stream `max_age` (default 120s), so this
/// can never still be waiting on a message the broker has already discarded.
pub const CONFIRM_TIMEOUT_SECS: u64 = 90;

#[derive(Debug, Clone)]
pub struct ChainBridgeConfig {
    pub nats_url: String,
    /// Chain Bridge gRPC endpoint for reads.
    pub grpc_url: String,
    pub confirm_timeout_secs: u64,
    /// SPIFFE URI this service publishes under. Must match the SAN in the mTLS
    /// client certificate the envelope is signed with — the bridge verifies
    /// cert → CA → SAN == `service_identity` → signature, and rejects a
    /// mismatch.
    pub service_identity: String,
}

impl Default for ChainBridgeConfig {
    fn default() -> Self {
        Self {
            nats_url: "nats://localhost:9001".into(),
            grpc_url: "http://localhost:5001".into(),
            confirm_timeout_secs: CONFIRM_TIMEOUT_SECS,
            service_identity: "spiffe://gridtokenx.th/prod/thbc-service".into(),
        }
    }
}

/// Chain Bridge-backed ledger.
pub struct ChainBridgeLedger {
    config: ChainBridgeConfig,
    nats: async_nats::Client,
    jetstream: async_nats::jetstream::Context,
    /// Envelope signer built from the mTLS client cert + key. `None` in dev
    /// without cert material — but the bridge force-signs both subjects this
    /// adapter publishes to, so an unsigned envelope is rejected *there*.
    /// Failing at the bridge rather than here is deliberate: one enforcement
    /// point cannot disagree with itself.
    signer: Option<EnvelopeSigner>,
}

impl std::fmt::Debug for ChainBridgeLedger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChainBridgeLedger")
            .field("config", &self.config)
            .field("signed", &self.signer.is_some())
            .finish_non_exhaustive()
    }
}

/// Map an issuance reply — or its absence — onto the F4 barrier.
///
/// Pure, so the whole table is unit-testable without a broker. It exists as a
/// named function because one row of it is counter-intuitive and load-bearing.
fn map_issue_reply(reply: Option<IssueThbcResultMessage>) -> ConfirmOutcome {
    match reply {
        Some(r) => match r.outcome {
            IssueOutcome::Confirmed => ConfirmOutcome::Confirmed,
            // Submitted, but not confirmed inside the bridge's polling window.
            // The transaction may still land, so this is *unknown*, not failed.
            IssueOutcome::Pending => ConfirmOutcome::Submitted,
            IssueOutcome::Failed => ConfirmOutcome::Failed,
        },
        // No reply within the timeout, or the subscription closed.
        //
        // NOT `Failed`, and the distinction is not pedantic. `IssuanceService`
        // turns `Failed` into a `Held` reading "issuance transaction failed" — a
        // settled claim about a transaction whose fate is unknown. Worse, a
        // retry after a transaction that actually landed hits a *permanent*
        // failure: the `[b"deposit", H(bank_ref)]` nullifier is created with
        // Anchor `init`, so the second attempt reverts at the account level.
        // `Submitted` leaves the deposit in `attested` for the reconciler, which
        // is the only component that can resolve an unknown.
        None => ConfirmOutcome::Submitted,
    }
}

fn map_attest_reply(reply: Option<UpdateAttestationResultMessage>) -> ConfirmOutcome {
    match reply {
        Some(r) => match r.outcome {
            IssueOutcome::Confirmed => ConfirmOutcome::Confirmed,
            IssueOutcome::Pending => ConfirmOutcome::Submitted,
            IssueOutcome::Failed => ConfirmOutcome::Failed,
        },
        None => ConfirmOutcome::Submitted,
    }
}

impl ChainBridgeLedger {
    /// Connect to NATS. Fails fast — a payment service that starts without its only
    /// route to the ledger is worse than one that refuses to start.
    pub async fn connect(config: ChainBridgeConfig) -> PortResult<Self> {
        let nats = async_nats::connect(&config.nats_url)
            .await
            .map_err(|e| PortError::Transient(format!("NATS connect {}: {e}", config.nats_url)))?;
        let jetstream = async_nats::jetstream::new(nats.clone());
        let signer = EnvelopeSigner::from_env_paths();
        if signer.is_none() {
            tracing::warn!(
                "no mTLS cert/key for envelope signing (CHAIN_BRIDGE_CLIENT_CERT / \
                 CHAIN_BRIDGE_CLIENT_KEY); Chain Bridge force-signs chain.tx.issuethbc and \
                 chain.tx.attest, so every write will be rejected there"
            );
        }
        Ok(Self {
            config,
            nats,
            jetstream,
            signer,
        })
    }

    #[must_use]
    pub const fn config(&self) -> &ChainBridgeConfig {
        &self.config
    }

    fn now_ms() -> u64 {
        // The bridge rejects envelopes older than 55s, so a saturating cast is
        // safe here in a way it would not be for a value that must round-trip:
        // u64 milliseconds do not overflow until the year 584942417.
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
    }

    /// Publish a signed intent and await the bridge's reply on `reply_subject`.
    ///
    /// Three details here are load-bearing and easy to get subtly wrong:
    ///
    /// 1. **Subscribe before publishing.** The bridge can reply faster than a
    ///    later subscribe would be established, and the reply rides core NATS —
    ///    there is no replay to fall back on.
    /// 2. **Publish to `JetStream` and await the `PubAck`.** The previous
    ///    implementation used core-NATS fire-and-forget with no ack, so a broker
    ///    or stream fault was completely silent.
    /// 3. **A timeout is `Ok(None)`, not `Err`.** "No answer" is a real,
    ///    expected outcome the caller must map deliberately — see
    ///    [`map_issue_reply`].
    async fn request<T, R>(
        &self,
        subject: &'static str,
        msg: &T,
        reply_subject: &str,
    ) -> PortResult<Option<R>>
    where
        T: serde::Serialize,
        R: serde::de::DeserializeOwned,
    {
        let mut subscriber = self
            .nats
            .subscribe(reply_subject.to_string())
            .await
            .map_err(|e| PortError::Transient(format!("subscribe {reply_subject}: {e}")))?;

        let bytes = serde_json::to_vec(msg)
            .map_err(|e| PortError::Rejected(format!("encode {subject}: {e}")))?;

        self.jetstream
            .publish(subject.to_string(), bytes.into())
            .await
            .map_err(|e| PortError::Transient(format!("publish {subject}: {e}")))?
            .await
            .map_err(|e| PortError::Transient(format!("publish ack {subject}: {e}")))?;

        let timeout = std::time::Duration::from_secs(self.config.confirm_timeout_secs);
        let Ok(Some(reply)) = tokio::time::timeout(timeout, subscriber.next()).await else {
            return Ok(None);
        };

        let decoded: R = serde_json::from_slice(&reply.payload)
            .map_err(|e| PortError::Transient(format!("decode reply on {reply_subject}: {e}")))?;
        Ok(Some(decoded))
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

    /// `beneficiary` is the **owner wallet** (base58), not an IAM user id and not
    /// a token account. See [`LedgerPort::issue`] for why the port carries a
    /// wallet: nothing on-chain can be derived from a user id, and this service
    /// has no IAM client to resolve one.
    async fn issue(
        &self,
        beneficiary: &str,
        amount: Thb,
        nullifier: BankRefHash,
    ) -> PortResult<ConfirmOutcome> {
        let correlation_id = uuid::Uuid::new_v4().to_string();
        let reply_subject = format!("chain.issue.result.{correlation_id}");

        let mut msg = IssueThbcMessage {
            correlation_id: correlation_id.clone(),
            // Stable per logical issuance, so a re-send replays the bridge's
            // recorded outcome instead of attempting a second on-chain issuance.
            // The nullifier IS this deposit's logical identity.
            idempotency_key: format!("issue:{}", nullifier.to_hex()),
            reply_subject: reply_subject.clone(),
            beneficiary_wallet: beneficiary.to_string(),
            amount_minor: amount.minor(),
            bank_ref_hash: *nullifier.as_bytes(),
            service_identity: self.config.service_identity.clone(),
            created_at_ms: Self::now_ms(),
            auth: None,
        };
        if let Some(signer) = &self.signer {
            msg.auth = Some(signer.sign(&canonical_issue_thbc_bytes(&msg)));
        }

        let reply: Option<IssueThbcResultMessage> = self
            .request(ISSUE_THBC_SUBJECT, &msg, &reply_subject)
            .await?;

        if let Some(r) = &reply
            && let Some(err) = &r.error
        {
            tracing::warn!(
                correlation_id = %correlation_id,
                outcome = ?r.outcome,
                "issuance reply carried an error: {err}"
            );
        }
        Ok(map_issue_reply(reply))
    }

    async fn escrow_redemption(&self, _redemption: &Redemption) -> PortResult<ConfirmOutcome> {
        Err(PortError::Unsupported(
            "redeem_thbc_for_fiat exists on-chain but Chain Bridge has no route for it \
             yet. Note it is USER-signed: the bridge would relay a transaction the \
             holder already signed, never construct the signer set itself (F8)",
        ))
    }

    async fn confirm_redemption(&self, _user: &str, _seq: u64) -> PortResult<ConfirmOutcome> {
        Err(PortError::Unsupported(
            "confirm_redemption exists on-chain but Chain Bridge has no route for it yet",
        ))
    }

    async fn reclaim_redemption(&self, _user: &str, _seq: u64) -> PortResult<ConfirmOutcome> {
        Err(PortError::Unsupported(
            "reclaim_redemption exists on-chain but Chain Bridge has no route for it yet; \
             it is USER-signed, so the platform relays and never reclaims on a holder's \
             behalf (F8)",
        ))
    }

    async fn update_attestation(&self, reserve: Thb) -> PortResult<ConfirmOutcome> {
        let correlation_id = uuid::Uuid::new_v4().to_string();
        let reply_subject = format!("chain.attest.result.{correlation_id}");

        let mut msg = UpdateAttestationMessage {
            correlation_id,
            // No effect dedup: this is a last-writer-wins field write, and a
            // replay only re-stamps `attestation_ts` — which is exactly the
            // freshness signal F5 wants refreshed.
            idempotency_key: String::new(),
            reply_subject: reply_subject.clone(),
            attested_reserve_minor: reserve.minor(),
            service_identity: self.config.service_identity.clone(),
            created_at_ms: Self::now_ms(),
            auth: None,
        };
        if let Some(signer) = &self.signer {
            msg.auth = Some(signer.sign(&canonical_update_attestation_bytes(&msg)));
        }

        let reply: Option<UpdateAttestationResultMessage> =
            self.request(ATTEST_SUBJECT, &msg, &reply_subject).await?;
        Ok(map_attest_reply(reply))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use thbc_core::bank_ref::BankRef;

    // These assert the envelope shape and the F4 mapping without a broker. The
    // connected paths need NATS and live in the e2e suite.

    #[test]
    fn default_config_points_at_the_dev_ports() {
        // 9000s = messaging, 5000s = gRPC mesh, per the repo port scheme.
        let c = ChainBridgeConfig::default();
        assert!(c.nats_url.contains("9001"));
        assert!(c.grpc_url.contains("5001"));
        assert_eq!(c.confirm_timeout_secs, CONFIRM_TIMEOUT_SECS);
        assert!(c.service_identity.ends_with("/thbc-service"));
    }

    #[test]
    fn subjects_are_single_tokens_under_chain_tx() {
        // `chain.tx.*` binds exactly one token; `chain.tx.attest.update` would not be
        // delivered by the bridge's stream.
        for s in [ATTEST_SUBJECT, ISSUE_THBC_SUBJECT] {
            assert_eq!(s.matches('.').count(), 2, "{s}");
            assert!(s.starts_with("chain.tx."), "{s}");
        }
    }

    #[test]
    fn confirm_timeout_stays_under_the_bridge_stream_max_age() {
        // CHAIN_TX defaults to max_age = 120s. Waiting longer than that would
        // block on a message the broker has already discarded.
        const { assert!(CONFIRM_TIMEOUT_SECS < 120) };
    }

    #[test]
    fn unsupported_errors_name_the_missing_instruction() {
        // Operators must be able to tell "not built" from "the chain said no".
        let e = PortError::Unsupported("confirm_redemption does not exist");
        assert!(e.to_string().starts_with("not implemented on-chain"));
    }

    /// The F4 barrier. A timeout and a `Pending` must BOTH be `Submitted`:
    /// reporting either as `Failed` invites a retry of an issuance that may have
    /// landed, and that retry fails permanently on the `init`-created nullifier.
    #[test]
    fn a_timeout_is_submitted_never_confirmed_or_failed() {
        assert_eq!(map_issue_reply(None), ConfirmOutcome::Submitted);
        assert_eq!(map_attest_reply(None), ConfirmOutcome::Submitted);
    }

    #[test]
    fn issue_reply_outcomes_map_onto_the_f4_barrier() {
        let reply = |outcome| {
            Some(IssueThbcResultMessage {
                correlation_id: "c1".into(),
                outcome,
                signature: None,
                error: None,
                slot: 0,
                deduplicated: false,
            })
        };
        assert_eq!(
            map_issue_reply(reply(IssueOutcome::Confirmed)),
            ConfirmOutcome::Confirmed
        );
        assert_eq!(
            map_issue_reply(reply(IssueOutcome::Pending)),
            ConfirmOutcome::Submitted,
            "Pending means the tx may still land — never report it as failed"
        );
        assert_eq!(
            map_issue_reply(reply(IssueOutcome::Failed)),
            ConfirmOutcome::Failed
        );
    }

    fn sample_issue_msg() -> IssueThbcMessage {
        let nullifier = BankRef::new("REF-1").unwrap().hash();
        IssueThbcMessage {
            correlation_id: "c1".into(),
            idempotency_key: format!("issue:{}", nullifier.to_hex()),
            reply_subject: "chain.issue.result.c1".into(),
            beneficiary_wallet: "Wa11etOwner".into(),
            amount_minor: 250_000,
            bank_ref_hash: *nullifier.as_bytes(),
            service_identity: "spiffe://gridtokenx.th/prod/thbc-service".into(),
            created_at_ms: 1_700_000_000_000,
            auth: None,
        }
    }

    /// The envelope must stay free of Solana types — that is what keeps this
    /// service chain-light, and F8's structural claim with it.
    #[test]
    fn the_issue_envelope_carries_no_solana_types() {
        let json: serde_json::Value =
            serde_json::from_slice(&serde_json::to_vec(&sample_issue_msg()).unwrap()).unwrap();
        assert!(
            json["beneficiary_wallet"].is_string(),
            "the wallet travels as an opaque base58 string, never a Pubkey"
        );
        assert!(
            json["amount_minor"].is_u64(),
            "a baht amount through an f64 cannot be reconciled against a bank statement (F2)"
        );
        assert!(json["bank_ref_hash"].is_array());
        // `auth` is omitted from the wire when unsigned (skip_serializing_if).
        assert!(json.get("auth").is_none());
    }

    #[test]
    fn the_idempotency_key_is_the_nullifier_hex() {
        let nullifier = BankRef::new("REF-1").unwrap().hash();
        assert_eq!(
            sample_issue_msg().idempotency_key,
            format!("issue:{}", nullifier.to_hex())
        );
    }

    /// Pins that this adapter signs the canonical bytes of the message it
    /// actually sends. Tamper-detection itself is proven in blockchain-types.
    #[test]
    fn the_signed_bytes_cover_the_beneficiary_and_the_amount() {
        let msg = sample_issue_msg();

        let mut redirected = sample_issue_msg();
        redirected.beneficiary_wallet = "attacker".into();
        assert_ne!(
            canonical_issue_thbc_bytes(&msg),
            canonical_issue_thbc_bytes(&redirected)
        );

        let mut inflated = sample_issue_msg();
        inflated.amount_minor = u64::MAX;
        assert_ne!(
            canonical_issue_thbc_bytes(&msg),
            canonical_issue_thbc_bytes(&inflated)
        );
    }
}
