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
//! [`LedgerPort::confirm_redemption`] is live too — the issuer-signed half of F7,
//! over `chain.tx.confirmredeem`.
//!
//! [`LedgerPort::escrow_redemption`] and [`LedgerPort::reclaim_redemption`] still
//! return [`PortError::Unsupported`], and that is a **design decision, not a
//! backlog item**. Both are user-signed: spec §6.1 step 1 is explicit that the
//! issuer cannot redeem on a holder's behalf, and reclaim exists so a holder can
//! recover tokens the issuer sat on. A bridge route for either would sign them
//! with the platform's Vault key, inverting the protection they provide — and
//! given that IAM can already decrypt user keys (see F8 in the invariant
//! registry), it would turn a latent custody capability into an exercised one on
//! exactly the path where a user is protecting themselves from the platform.
//!
//! [`LedgerPort::snapshot`] refuses for its own unrelated reason — three of the
//! four fields `TreasurySnapshot` carries are not on the `Treasury` account, and
//! synthesising them would report a tighter F1 ceiling than the chain enforces.
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
    EnvelopeSigner, canonical_confirm_redemption_bytes, canonical_issue_thbc_bytes,
    canonical_update_attestation_bytes,
};
use gridtokenx_blockchain_types::nats_schema::{
    ConfirmRedemptionMessage, ConfirmRedemptionResultMessage, IssueOutcome, IssueThbcMessage,
    IssueThbcResultMessage, UpdateAttestationMessage, UpdateAttestationResultMessage,
};
use thbc_core::bank_ref::BankRefHash;
use thbc_core::money::Thb;
use thbc_core::exchange::{ExchangeParams, Inventory};
use thbc_core::money::Grx;
use thbc_core::ports::{LedgerPort, PortError, PortResult, TreasurySnapshot};
use thbc_core::reserve::{Attestation, ReserveState};
use thbc_core::redemption::{ConfirmOutcome, Redemption};

/// NATS subject for attestation refresh.
///
/// Re-exported from the shared schema rather than retyped: the bridge's consumer
/// reads the same constant, so publisher and consumer cannot drift. A single
/// token under `chain.tx.` — NATS `*` matches exactly one token, so
/// `chain.tx.attest.update` would be captured by the bound stream and never
/// delivered.
pub use gridtokenx_blockchain_types::nats_schema::{
    ATTEST_SUBJECT, CONFIRM_REDEMPTION_SUBJECT, ISSUE_THBC_SUBJECT,
};

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
    /// Base58 address of the treasury state PDA (`[b"treasury"]`), and of the THBC
    /// inventory vault (`[b"thbc_inventory"]`).
    ///
    /// Carried as configuration rather than derived. Deriving a PDA needs the
    /// off-curve check, which needs curve25519 — and pulling that in would breach the
    /// chain-light property this crate is held to (workspace `Cargo.toml`: no
    /// solana-sdk, no anchor, no SPL, no tonic). Reading a `u64` at a known offset
    /// needs none of that, which is the whole reason the snapshot can be served over
    /// NATS at all.
    ///
    /// `None` disables the snapshot rather than guessing an address: a snapshot read
    /// from the wrong account would report a confident, wrong F1 ceiling.
    pub treasury_pda: Option<String>,
    pub inventory_vault: Option<String>,
}

impl Default for ChainBridgeConfig {
    fn default() -> Self {
        Self {
            nats_url: "nats://localhost:9001".into(),
            grpc_url: "http://localhost:5001".into(),
            confirm_timeout_secs: CONFIRM_TIMEOUT_SECS,
            service_identity: "spiffe://gridtokenx.th/prod/thbc-service".into(),
            treasury_pda: None,
            inventory_vault: None,
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

fn map_confirm_redeem_reply(reply: Option<ConfirmRedemptionResultMessage>) -> ConfirmOutcome {
    match reply {
        Some(r) => match r.outcome {
            IssueOutcome::Confirmed => ConfirmOutcome::Confirmed,
            IssueOutcome::Pending => ConfirmOutcome::Submitted,
            IssueOutcome::Failed => ConfirmOutcome::Failed,
        },
        // Same reasoning as `map_issue_reply`: no reply is UNKNOWN, not failed.
        // `confirm_redemption` closes the redemption record, so a retry after a
        // burn that actually landed fails permanently — and reporting `Failed`
        // here would also tell the service the holder's reclaim right survives
        // when it may not.
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

/// `(user, password)` from a URL's userinfo, if present. Password defaults to
/// empty when the userinfo has no `:`.
fn url_userinfo(url: &str) -> Option<(String, String)> {
    let after_scheme = url.split_once("://").map_or(url, |(_, rest)| rest);
    let authority = after_scheme
        .split(['/', '?'])
        .next()
        .unwrap_or(after_scheme);
    let (userinfo, _host) = authority.rsplit_once('@')?;
    let (user, pass) = userinfo.split_once(':').unwrap_or((userinfo, ""));
    Some((user.to_string(), pass.to_string()))
}

/// Connect to NATS honoring credentials embedded in the URL.
///
/// async-nats (0.37) **ignores** the userinfo component of a
/// `nats://user:pass@host` URL — auth is taken only from `ConnectOptions` — so a
/// broker with `authorization` enabled rejects a plain `async_nats::connect`
/// even when the URL carries valid credentials. The failure is an
/// `authorization violation` at connect, which reads like a wrong password
/// rather than dropped credentials.
///
/// Mirrors `connect_with_url_creds` in
/// `gridtokenx-aggregator-bridge/.../infra/mint.rs`, which mirrors
/// `gridtokenx-blockchain-core`'s `rpc::nats_provider`. Duplicated rather than
/// shared because this crate stays chain-light. Credentials are used verbatim
/// (no percent-decoding).
async fn connect_with_url_creds(url: &str) -> Result<async_nats::Client, async_nats::ConnectError> {
    match url_userinfo(url) {
        Some((user, pass)) => {
            async_nats::ConnectOptions::with_user_and_password(user, pass)
                .connect(url)
                .await
        }
        None => async_nats::connect(url).await,
    }
}

impl ChainBridgeLedger {
    /// Connect to NATS. Fails fast — a payment service that starts without its only
    /// route to the ledger is worse than one that refuses to start.
    pub async fn connect(config: ChainBridgeConfig) -> PortResult<Self> {
        let nats = connect_with_url_creds(&config.nats_url)
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

    /// One raw account read over `chain.query.account`.
    ///
    /// Plain NATS request/reply, deliberately not `JetStream` — a read wants neither
    /// durability nor replay.
    ///
    /// `Ok(None)` means the bridge said the account is definitely absent. A bridge
    /// that could not answer produces `Err`, and the two must not be collapsed:
    /// treating a failure as absence turns an unreachable RPC into an empty vault.
    async fn query_account(&self, pubkey: &str) -> PortResult<Option<Vec<u8>>> {
        use base64::Engine as _;
        use gridtokenx_blockchain_types::nats_schema::{
            AccountQueryMessage, AccountQueryResultMessage, ACCOUNT_QUERY_SUBJECT,
        };

        let msg = AccountQueryMessage {
            correlation_id: uuid::Uuid::new_v4().to_string(),
            pubkey: pubkey.to_string(),
            service_identity: self.config.service_identity.clone(),
            created_at_ms: Self::now_ms(),
        };
        let bytes = serde_json::to_vec(&msg)
            .map_err(|e| PortError::Rejected(format!("encode account query: {e}")))?;

        // Short timeout: this is a read on a request path, and a caller waiting the
        // full confirm timeout for a balance is worse than a prompt failure.
        let reply = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            self.nats.request(ACCOUNT_QUERY_SUBJECT, bytes.into()),
        )
        .await
        .map_err(|_| PortError::Transient("account query timed out".into()))?
        .map_err(|e| PortError::Transient(format!("account query: {e}")))?;

        let res: AccountQueryResultMessage = serde_json::from_slice(&reply.payload)
            .map_err(|e| PortError::Transient(format!("decode account query reply: {e}")))?;

        if let Some(err) = res.error {
            return Err(PortError::Transient(format!("bridge account query: {err}")));
        }
        if !res.exists {
            return Ok(None);
        }
        base64::engine::general_purpose::STANDARD
            .decode(&res.data_base64)
            .map(Some)
            .map_err(|e| PortError::Transient(format!("decode account bytes: {e}")))
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
    /// Reads the real treasury over `chain.query.account`.
    ///
    /// This used to return `Unsupported`, on the grounds that `reserve_encumbered`,
    /// `thbc_inventory` and `redemption_queue_len` were not on the account. Two of
    /// those three are no longer true: `reserve_encumbered` landed at offset 264
    /// (2026-07-29) and the inventory is just a token-account balance. The stale
    /// refusal is why the reconciler reported `Unsupported` on every tick.
    ///
    /// The bytes are decoded here rather than by the bridge. That is what keeps this
    /// crate chain-light: reading a `u64` at a known offset needs no solana-sdk, and
    /// the alternative — the gRPC client — would drag in solana-sdk and tonic, which
    /// the workspace manifest forbids.
    async fn snapshot(&self) -> PortResult<TreasurySnapshot> {
        /// 8-byte Anchor discriminator before the 272-byte zero-copy `Treasury`.
        const B: usize = 8;

        let (Some(treasury_pda), Some(inventory_vault)) =
            (&self.config.treasury_pda, &self.config.inventory_vault)
        else {
            return Err(PortError::Unsupported(
                "THBC_TREASURY_PDA / THBC_INVENTORY_VAULT are unset — refusing to guess \
                 an address, because a snapshot read from the wrong account would report \
                 a confident and wrong F1 ceiling",
            ));
        };

        let data = self.query_account(treasury_pda).await?.ok_or_else(|| {
            PortError::Rejected(format!("treasury account {treasury_pda} does not exist"))
        })?;

        // 8-byte Anchor discriminator, then the 272-byte zero-copy `Treasury`.
        // Offsets are pinned by `carved_fields_kept_their_offsets` in the program's
        // state.rs, so a layout change there fails that test rather than silently
        // misreading here.
        if data.len() < B + 272 {
            return Err(PortError::Rejected(format!(
                "treasury account is {} bytes, expected at least {}",
                data.len(),
                B + 272
            )));
        }
        // `expect_used` is denied workspace-wide and this is the payment leg, so no
        // fallible conversion here. The length check above guarantees every read below
        // is in bounds: the largest offset used is B + 264, and B + 264 + 8 == B + 272.
        let le8 = |off: usize| -> [u8; 8] {
            let mut b = [0u8; 8];
            b.copy_from_slice(&data[off..off + 8]);
            b
        };
        let u64_at = |off: usize| -> u64 { u64::from_le_bytes(le8(off)) };
        let i64_at = |off: usize| -> i64 { i64::from_le_bytes(le8(off)) };

        let attested_reserve = Thb::from_minor(u64_at(B + 176));
        let attestation_ts = i64_at(B + 184);
        let attestation_ttl = i64_at(B + 192);
        let thbc_supply = Thb::from_minor(u64_at(B + 200));
        let grx_per_thbc_rate = u64_at(B + 208);
        let fee_bps = {
            let mut b = [0u8; 2];
            b.copy_from_slice(&data[B + 248..B + 250]);
            u16::from_le_bytes(b)
        };
        let paused = data[B + 250] != 0;
        let reserve_encumbered = Thb::from_minor(u64_at(B + 264));

        // SPL token account: `amount` is a u64 at offset 64. Absent vault = zero
        // inventory is correct here (the vault genuinely holds nothing until
        // `initialize_thbc_inventory` runs); a query FAILURE propagates instead.
        let inventory_thbc = match self.query_account(inventory_vault).await? {
            Some(v) if v.len() >= 72 => {
                let mut b = [0u8; 8];
                b.copy_from_slice(&v[64..72]);
                Thb::from_minor(u64::from_le_bytes(b))
            }
            _ => Thb::ZERO,
        };

        Ok(TreasurySnapshot {
            reserve: ReserveState::new(
                Attestation::new(attested_reserve, attestation_ts, attestation_ttl),
                reserve_encumbered,
                thbc_supply,
            ),
            inventory: Inventory {
                thbc: inventory_thbc,
                // `swap_vault` is a separate account and is not needed by any
                // consumer of this snapshot today. Reported as zero rather than
                // guessed; adding it means another configured address.
                grx: Grx::ZERO,
            },
            params: ExchangeParams {
                grx_per_thbc_rate,
                fee_bps,
                paused,
            },
            // NOT derivable over this transport: counting live `[b"redeem", user, seq]`
            // records needs getProgramAccounts, which the bridge does not expose. `None`
            // says "not observable here" — distinct from `Some(0)`, which would claim an
            // empty queue and could hide an unserviced redemption from the `E` observer.
            redemption_queue_len: None,
        })
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

    async fn confirm_redemption(&self, user: &str, seq: u64) -> PortResult<ConfirmOutcome> {
        let correlation_id = uuid::Uuid::new_v4().to_string();
        let reply_subject = format!("chain.confirmredeem.result.{correlation_id}");

        let mut msg = ConfirmRedemptionMessage {
            correlation_id: correlation_id.clone(),
            // Stable per logical confirmation. `(user, seq)` IS the redemption's
            // identity — the same pair that keys the on-chain record — so a
            // re-send replays the bridge's recorded outcome instead of attempting
            // a second burn against a record that no longer exists.
            idempotency_key: format!("confirmredeem:{user}:{seq}"),
            reply_subject: reply_subject.clone(),
            user_wallet: user.to_string(),
            seq,
            service_identity: self.config.service_identity.clone(),
            created_at_ms: Self::now_ms(),
            auth: None,
        };
        if let Some(signer) = &self.signer {
            msg.auth = Some(signer.sign(&canonical_confirm_redemption_bytes(&msg)));
        }

        let reply: Option<ConfirmRedemptionResultMessage> = self
            .request(CONFIRM_REDEMPTION_SUBJECT, &msg, &reply_subject)
            .await?;

        if let Some(r) = &reply
            && let Some(err) = &r.error
        {
            tracing::warn!(
                correlation_id = %correlation_id,
                outcome = ?r.outcome,
                "redemption confirmation reply carried an error: {err}"
            );
        }
        Ok(map_confirm_redeem_reply(reply))
    }

    async fn reclaim_redemption(&self, _user: &str, _seq: u64) -> PortResult<ConfirmOutcome> {
        Err(PortError::Unsupported(
            "reclaim_redemption exists on-chain but Chain Bridge has no route for it yet; \
             it is USER-signed, so the platform relays and never reclaims on a holder's \
             behalf (F8)",
        ))
    }

    async fn update_attestation(
        &self,
        reserve: Thb,
        encumbered: Thb,
    ) -> PortResult<ConfirmOutcome> {
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
            reserve_encumbered_minor: encumbered.minor(),
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

    /// async-nats 0.37 drops URL userinfo, so credentials must be lifted out and
    /// passed via `ConnectOptions`. Getting this wrong surfaces as an
    /// `authorization violation` that looks like a wrong password.
    #[test]
    fn url_userinfo_extracts_user_and_password() {
        assert_eq!(
            url_userinfo("nats://gridtokenx:gridtokenx_nats_dev@nats:4222"),
            Some(("gridtokenx".to_string(), "gridtokenx_nats_dev".to_string()))
        );
    }

    #[test]
    fn url_userinfo_absent_returns_none() {
        assert_eq!(url_userinfo("nats://nats:4222"), None);
        // '@' beyond the authority (path/query) must not be mistaken for userinfo.
        assert_eq!(url_userinfo("nats://host:4222/path@x"), None);
    }

    #[test]
    fn url_userinfo_without_password_yields_empty_password() {
        assert_eq!(
            url_userinfo("nats://alice@nats:4222"),
            Some(("alice".to_string(), String::new()))
        );
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
        assert_eq!(map_confirm_redeem_reply(None), ConfirmOutcome::Submitted);
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

    /// The redemption burn has a second reason a lost reply must not read as
    /// `Failed`, on top of the retry hazard the issuance path shares.
    ///
    /// `confirm_redemption` CLOSES the `[b"redeem", user, seq]` record. So a retry
    /// after a burn that actually landed fails permanently at the runtime level —
    /// and, worse, reporting `Failed` tells the service the holder's reclaim right
    /// survives when the record may already be gone. `Submitted` keeps it unknown,
    /// which is the only honest answer and the only one that does not invite the
    /// service to act on a false belief about a holder's remedy.
    #[test]
    fn confirm_redeem_outcomes_map_onto_the_f4_barrier() {
        let reply = |outcome| {
            Some(ConfirmRedemptionResultMessage {
                correlation_id: "c1".into(),
                outcome,
                signature: None,
                error: None,
                slot: 0,
                deduplicated: false,
            })
        };
        assert_eq!(
            map_confirm_redeem_reply(reply(IssueOutcome::Confirmed)),
            ConfirmOutcome::Confirmed
        );
        assert_eq!(
            map_confirm_redeem_reply(reply(IssueOutcome::Pending)),
            ConfirmOutcome::Submitted,
            "Pending means the burn may still land — never report it as failed"
        );
        assert_eq!(
            map_confirm_redeem_reply(reply(IssueOutcome::Failed)),
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
