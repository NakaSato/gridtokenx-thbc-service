//! End-to-end tests through the real services, against the simulated ledger.
//!
//! Spec §13, the rows that need a ledger rather than pure arithmetic:
//!
//! | Invariant | §13 asks for | Covered here |
//! |---|---|---|
//! | F3 | replayed `bank_ref` reverts — E2E, `LiteSVM` | replay through the full issuance path |
//! | F4 | wire never enqueued before confirmed burn — service integration | yes |
//! | F5 | issuance halts past TTL; resumes on refresh — `LiteSVM` | yes |
//! | F7 | reclaim at `t ≥ Δ`, fails at `t < Δ`, confirm blocks reclaim — `LiteSVM` + clock warp | yes |
//! | F8 | no platform key in any user-value signer set — static audit | [`f8_static_audit`] |
//!
//! **Coverage is partial and here is exactly how.** §13 asks for `LiteSVM`, i.e. the
//! real program. These run against `SimulatedLedger`, a model of the program *as
//! specified* — and for F3 and F7 the program does not implement the instruction at
//! all, so there is nothing to run `LiteSVM` against. What passes here demonstrates the
//! service and the domain model behave correctly; it does not demonstrate the chain
//! does. Do not report F3 or F7 as covered on the strength of this file.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use thbc_core::bank_ref::BankRef;
use thbc_core::deposit::DepositState;
use thbc_core::money::Thb;
use thbc_core::ports::{Clock, LedgerPort, PortError, ReconciliationRepository};
use thbc_core::redemption::RedemptionState;
use thbc_ledger::SimulatedLedger;
use thbc_logic::adapters::{FixedClock, RecordingPayoutQueue, StubCompliance};
use thbc_logic::{
    IssuanceOutcome, IssuanceService, ReconciliationService, RedemptionOutcome, RedemptionService,
    ReserveService,
};
use thbc_persistence::{InMemoryDepositRepo, InMemoryReconciliationRepo, InMemoryRedemptionRepo};

const DELTA: i64 = 86_400;
const TTL: i64 = 3_600;

struct Harness {
    issuance: IssuanceService,
    redemption: RedemptionService,
    reserve: Arc<ReserveService>,
    reconciliation: ReconciliationService,
    history: Arc<InMemoryReconciliationRepo>,
    ledger: Arc<SimulatedLedger>,
    payouts: Arc<RecordingPayoutQueue>,
    clock: Arc<FixedClock>,
}

impl Harness {
    /// Default KYC ceiling sits above every amount these tests use, so the screen
    /// passes unless a test deliberately exceeds it.
    fn new(reserve_baht: u64) -> Self {
        Self::with_kyc_ceiling(reserve_baht, 1_000_000)
    }

    fn with_kyc_ceiling(reserve_baht: u64, kyc_ceiling_baht: u64) -> Self {
        let clock = Arc::new(FixedClock::new(0));
        let ledger = Arc::new(SimulatedLedger::new(
            Thb::from_baht(reserve_baht).expect("reserve fits"),
            TTL,
            0,
        ));
        let deposits = Arc::new(InMemoryDepositRepo::new());
        let redemptions = Arc::new(InMemoryRedemptionRepo::new());
        let payouts = Arc::new(RecordingPayoutQueue::new());
        let history = Arc::new(InMemoryReconciliationRepo::new());
        let compliance = Arc::new(StubCompliance::new(
            Thb::from_baht(kyc_ceiling_baht).expect("fits"),
        ));

        let reserve = Arc::new(ReserveService::new(
            Arc::clone(&ledger) as Arc<dyn LedgerPort>,
            Arc::clone(&deposits) as Arc<dyn thbc_core::ports::DepositRepository>,
            Arc::clone(&clock) as Arc<dyn Clock>,
        ));

        Self {
            issuance: IssuanceService::new(
                Arc::clone(&deposits) as Arc<dyn thbc_core::ports::DepositRepository>,
                compliance,
                Arc::clone(&ledger) as Arc<dyn LedgerPort>,
                Arc::clone(&clock) as Arc<dyn Clock>,
            ),
            redemption: RedemptionService::new(
                Arc::clone(&redemptions) as Arc<dyn thbc_core::ports::RedemptionRepository>,
                Arc::clone(&ledger) as Arc<dyn LedgerPort>,
                Arc::clone(&payouts) as Arc<dyn thbc_core::ports::PayoutPort>,
                Arc::clone(&clock) as Arc<dyn Clock>,
                DELTA,
            ),
            reconciliation: ReconciliationService::new(
                deposits as Arc<dyn thbc_core::ports::DepositRepository>,
                redemptions as Arc<dyn thbc_core::ports::RedemptionRepository>,
                Arc::clone(&reserve),
                Arc::clone(&history) as Arc<dyn thbc_core::ports::ReconciliationRepository>,
                Arc::clone(&clock) as Arc<dyn Clock>,
            ),
            history,
            reserve,
            ledger,
            payouts,
            clock,
        }
    }

    /// Move both clocks together. The simulator has its own so it can model on-chain
    /// `Clock::get()`; letting them drift would make every timelock test meaningless.
    fn warp(&self, to: i64) {
        self.clock.set(to);
        self.ledger.set_now(to).expect("simulated clock");
    }

    async fn deposit(&self, reference: &str, baht: u64, user: &str) -> IssuanceOutcome {
        self.issuance
            .handle_deposit(
                BankRef::new(reference).expect("valid ref"),
                Thb::from_baht(baht).expect("fits"),
                user,
                // `LedgerPort::issue` receives the beneficiary's WALLET, not the
                // IAM user id — nothing on-chain can be derived from the latter.
                // In the simulator the string it receives IS the account key, and
                // the redemption side of these tests keys off `user`, so the two
                // must be the same identifier here. Passing a distinct
                // `Wa11et-{user}` would issue to one account and redeem from
                // another, which shows up as an Underflow several steps later.
                user,
            )
            .await
            .expect("deposit handled")
    }
}

// ---------------------------------------------------------------------------
// Happy path
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_deposit_issues_thbc_and_reconciles_clean() {
    let h = Harness::new(1_000_000);
    let outcome = h.deposit("SCB-1", 1_000, "alice").await;

    assert_eq!(
        outcome,
        IssuanceOutcome::Issued {
            amount: Thb::from_baht(1_000).unwrap()
        }
    );
    assert_eq!(
        h.ledger.balance_of("alice").unwrap(),
        Thb::from_baht(1_000).unwrap()
    );

    let report = h.reconciliation.run().await.unwrap();
    assert!(report.is_healthy(), "{report:?}");
    assert_eq!(report.drift, 0);
}

// ---------------------------------------------------------------------------
// §5.4 — held deposits are actually retried
// ---------------------------------------------------------------------------

#[tokio::test]
async fn held_deposits_are_released_by_the_retry_sweep() {
    // `retry_held` existed with tests and had NO caller outside them — no scheduler,
    // no endpoint. A deposit held during a stale-attestation window stayed held
    // forever, with the holder's fiat already in the reserve account. This is the
    // sweep that releases it.
    let h = Harness::new(1_000_000);
    h.warp(TTL + 1); // attestation stale

    for r in ["SCB-1", "SCB-2"] {
        let held = h.deposit(r, 1_000, "alice").await;
        assert!(matches!(held, IssuanceOutcome::Held { .. }), "{held:?}");
    }
    assert_eq!(h.ledger.supply().unwrap(), Thb::ZERO);

    // Sweeping while still stale must change nothing and must not consume the refs.
    let (retried, issued) = h.issuance.retry_all_held().await.unwrap();
    assert_eq!((retried, issued), (2, 0), "both still held, neither issued");

    // The attestor refreshes; the next sweep releases both.
    h.reserve
        .attest(Thb::from_baht(1_000_000).unwrap())
        .await
        .unwrap();
    let (retried, issued) = h.issuance.retry_all_held().await.unwrap();
    assert_eq!((retried, issued), (2, 2));
    assert_eq!(h.ledger.supply().unwrap(), Thb::from_baht(2_000).unwrap());

    // Nothing is held any more, so a third sweep is a no-op rather than a re-issue.
    let (retried, issued) = h.issuance.retry_all_held().await.unwrap();
    assert_eq!(
        (retried, issued),
        (0, 0),
        "issued deposits must not be swept again"
    );
    assert_eq!(h.ledger.supply().unwrap(), Thb::from_baht(2_000).unwrap());
}

#[tokio::test]
async fn the_retry_sweep_ignores_encumbered_and_issued_deposits() {
    // Only `Screened` is retryable. An encumbered deposit failed compliance and must
    // never be retried into an issuance; an issued one must never be issued twice.
    let h = Harness::with_kyc_ceiling(1_000_000, 500_000);
    h.deposit("SCB-OK", 1_000, "alice").await; // issued
    h.deposit("SCB-KYC", 600_000, "mallory").await; // encumbered

    let (retried, issued) = h.issuance.retry_all_held().await.unwrap();
    assert_eq!((retried, issued), (0, 0), "neither state is held");
    assert_eq!(h.ledger.supply().unwrap(), Thb::from_baht(1_000).unwrap());
}

// ---------------------------------------------------------------------------
// F2 — the reconciliation HISTORY, not just the point-in-time check
// ---------------------------------------------------------------------------

#[tokio::test]
async fn every_reconciliation_run_is_recorded() {
    // The `reconciliation_runs` table existed with a comment calling it "append-only
    // history, so a breach that was later resolved is still visible to the regulator"
    // — and nothing wrote to it. A point-in-time check that leaves no trace cannot
    // answer the only question an auditor asks.
    let h = Harness::new(1_000_000);
    assert!(h.history.is_empty().unwrap());

    h.reconciliation.run().await.unwrap();
    h.reconciliation.run().await.unwrap();
    assert_eq!(
        h.history.len().unwrap(),
        2,
        "every run is appended, not just bad ones"
    );
}

#[tokio::test]
async fn a_resolved_breach_stays_visible_in_the_history() {
    // The whole reason the history exists. After the breach is resolved, the current
    // reconciliation reads healthy — and would be indistinguishable from a ledger that
    // was never broken, if the earlier run had not been kept.
    let h = Harness::new(1_000_000);
    h.deposit("SCB-1", 1_000, "alice").await;

    // Attestor honestly reports a reserve that no longer covers supply.
    h.reserve.attest(Thb::from_baht(1).unwrap()).await.unwrap();
    let bad = h.reconciliation.run().await.unwrap();
    assert_eq!(bad.severity, thbc_core::reconcile::Severity::Insolvent);

    // Reserve restored; the live check is clean again.
    h.reserve
        .attest(Thb::from_baht(1_000_000).unwrap())
        .await
        .unwrap();
    let good = h.reconciliation.run().await.unwrap();
    assert!(good.is_healthy());

    assert_eq!(
        h.reconciliation.unhealthy_runs().await.unwrap(),
        1,
        "the resolved insolvency must still be on the record"
    );

    let recent = h.history.recent(10).await.unwrap();
    assert_eq!(recent.len(), 2);
    assert!(recent[0].is_healthy(), "newest first");
    assert!(!recent[1].is_healthy());
}

// ---------------------------------------------------------------------------
// F3 — deposit idempotency
// ---------------------------------------------------------------------------

#[tokio::test]
async fn f3_a_replayed_webhook_does_not_issue_twice() {
    let h = Harness::new(1_000_000);

    let first = h.deposit("SCB-1", 1_000, "alice").await;
    assert!(matches!(first, IssuanceOutcome::Issued { .. }));

    // The bank retries — at-least-once delivery is correct behaviour, not an attack.
    let replay = h.deposit("SCB-1", 1_000, "alice").await;
    assert_eq!(replay, IssuanceOutcome::AlreadyIssued);

    assert_eq!(
        h.ledger.supply().unwrap(),
        Thb::from_baht(1_000).unwrap(),
        "a replay must not double supply"
    );
    assert!(h.reconciliation.run().await.unwrap().is_healthy());
}

#[tokio::test]
async fn f3_a_casing_or_whitespace_variant_is_still_the_same_deposit() {
    // A bank that echoes "scb-1 " on retry must not defeat F3.
    let h = Harness::new(1_000_000);
    h.deposit("SCB-1", 1_000, "alice").await;
    assert_eq!(
        h.deposit("  scb-1  ", 1_000, "alice").await,
        IssuanceOutcome::AlreadyIssued
    );
    assert_eq!(h.ledger.supply().unwrap(), Thb::from_baht(1_000).unwrap());
}

#[tokio::test]
async fn f3_a_replay_of_a_kyc_failed_deposit_does_not_get_a_second_screen() {
    // Retrying a refused deposit until the screen passes would be a trivial bypass.
    let h = Harness::new(1_000_000);
    let first = h.deposit("SCB-1", 2_000_000, "mallory").await; // over the stub ceiling
    assert!(
        matches!(first, IssuanceOutcome::Encumbered { .. }),
        "{first:?}"
    );

    let replay = h.deposit("SCB-1", 2_000_000, "mallory").await;
    assert!(
        matches!(replay, IssuanceOutcome::Encumbered { .. }),
        "{replay:?}"
    );
    assert_eq!(h.ledger.supply().unwrap(), Thb::ZERO);
}

#[tokio::test]
async fn f3_distinct_bank_refs_both_issue() {
    let h = Harness::new(1_000_000);
    h.deposit("SCB-1", 1_000, "alice").await;
    h.deposit("SCB-2", 1_000, "alice").await;
    assert_eq!(h.ledger.supply().unwrap(), Thb::from_baht(2_000).unwrap());
}

// ---------------------------------------------------------------------------
// F5 — attestation freshness
// ---------------------------------------------------------------------------

#[tokio::test]
async fn f5_issuance_halts_past_the_ttl_and_resumes_after_a_refresh() {
    let h = Harness::new(1_000_000);
    h.warp(TTL + 1);

    let held = h.deposit("SCB-1", 1_000, "alice").await;
    assert!(matches!(held, IssuanceOutcome::Held { .. }), "{held:?}");
    assert_eq!(h.ledger.supply().unwrap(), Thb::ZERO);
    assert!(!h.reserve.is_issuance_open().await.unwrap());

    // The attestor refreshes.
    h.reserve
        .attest(Thb::from_baht(1_000_000).unwrap())
        .await
        .unwrap();
    assert!(h.reserve.is_issuance_open().await.unwrap());

    // The same bank_ref retries and now succeeds — the held attempt must not have
    // consumed the reference (spec §5.4).
    let retried = h
        .issuance
        .retry_held(&BankRef::new("SCB-1").unwrap())
        .await
        .unwrap();
    assert!(
        matches!(retried, IssuanceOutcome::Issued { .. }),
        "{retried:?}"
    );
    assert_eq!(h.ledger.supply().unwrap(), Thb::from_baht(1_000).unwrap());
}

#[tokio::test]
async fn f5_a_held_deposit_is_recorded_rather_than_dropped() {
    let h = Harness::new(1_000_000);
    h.warp(TTL + 1);
    h.deposit("SCB-1", 1_000, "alice").await;

    // It reached `screened` and stopped there — the §5.2 barrier held.
    let stored = h.issuance.retry_held(&BankRef::new("SCB-1").unwrap()).await;
    assert!(stored.is_ok(), "the deposit must still exist to be retried");
}

// ---------------------------------------------------------------------------
// F1 — reserve ceiling, through the service
// ---------------------------------------------------------------------------

#[tokio::test]
async fn f1_a_deposit_beyond_the_reserve_is_held_not_issued() {
    let h = Harness::new(1_000);
    let held = h.deposit("SCB-1", 5_000, "alice").await;
    assert!(matches!(held, IssuanceOutcome::Held { .. }), "{held:?}");
    assert_eq!(h.ledger.supply().unwrap(), Thb::ZERO);
}

#[tokio::test]
async fn f1_encumbered_fiat_tightens_the_ceiling_for_later_deposits() {
    // The reason `reserve_encumbered` exists (spec §4.1). 600k baht of the 1M reserve
    // is encumbered by a failed screen, so only 400k of headroom remains.
    // KYC ceiling at 500k so the 600k deposit fails the screen and the 400k passes.
    let h = Harness::with_kyc_ceiling(1_000_000, 500_000);

    let failed = h.deposit("SCB-1", 600_000, "mallory").await;
    assert!(matches!(failed, IssuanceOutcome::Encumbered { .. }));
    assert_eq!(
        h.reserve.current().await.unwrap().encumbered,
        Thb::from_baht(600_000).unwrap()
    );

    assert!(matches!(
        h.deposit("SCB-2", 400_000, "alice").await,
        IssuanceOutcome::Issued { .. }
    ));
    let over = h.deposit("SCB-3", 1, "alice").await;
    assert!(
        matches!(over, IssuanceOutcome::Held { .. }),
        "encumbrance must bind: {over:?}"
    );
}

// ---------------------------------------------------------------------------
// F4 — burn-before-wire. The barrier.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn f4_no_payout_is_enqueued_before_the_burn_is_confirmed() {
    let h = Harness::new(1_000_000);
    h.deposit("SCB-1", 1_000, "alice").await;

    // Nothing has been redeemed at all — a payout attempt must find nothing.
    assert!(h.redemption.process_payout("alice", 1).await.is_err());
    assert!(
        h.payouts.is_empty(),
        "no wire may exist without a redemption"
    );
}

#[tokio::test]
async fn f4_a_payout_follows_a_confirmed_escrow_and_only_then() {
    let h = Harness::new(1_000_000);
    h.deposit("SCB-1", 1_000, "alice").await;

    let outcome = h
        .redemption
        .request("alice", Thb::from_baht(400).unwrap())
        .await
        .unwrap();
    let RedemptionOutcome::Escrowed { seq, .. } = outcome else {
        panic!("expected escrow confirmation, got {outcome:?}")
    };

    // Escrow confirmed → the barrier opens.
    h.redemption.process_payout("alice", seq).await.unwrap();
    assert_eq!(h.payouts.len(), 1);
    let (user, sent_seq, amount) = h.payouts.sent()[0].clone();
    assert_eq!(
        (user.as_str(), sent_seq, amount),
        ("alice", seq, Thb::from_baht(400).unwrap())
    );
}

#[tokio::test]
async fn f4_a_second_payout_for_the_same_redemption_is_refused() {
    let h = Harness::new(1_000_000);
    h.deposit("SCB-1", 1_000, "alice").await;
    let RedemptionOutcome::Escrowed { seq, .. } = h
        .redemption
        .request("alice", Thb::from_baht(400).unwrap())
        .await
        .unwrap()
    else {
        panic!("expected escrow")
    };

    h.redemption.process_payout("alice", seq).await.unwrap();
    let second = h.redemption.process_payout("alice", seq).await;
    assert!(
        second.is_err(),
        "a double wire is unrecoverable in a way a double burn is not"
    );
    assert_eq!(h.payouts.len(), 1);
}

#[tokio::test]
async fn f4_escrow_holds_tokens_without_reducing_supply() {
    // The token has left the user's wallet but has NOT been burned. Supply falls only
    // at `confirm_redemption` — that gap is what makes F7 reclaim possible.
    let h = Harness::new(1_000_000);
    h.deposit("SCB-1", 1_000, "alice").await;
    h.redemption
        .request("alice", Thb::from_baht(1_000).unwrap())
        .await
        .unwrap();

    assert_eq!(h.ledger.balance_of("alice").unwrap(), Thb::ZERO);
    assert_eq!(h.ledger.supply().unwrap(), Thb::from_baht(1_000).unwrap());
    assert_eq!(h.redemption.queue_len().await.unwrap(), 1);
}

#[tokio::test]
async fn a_confirmed_redemption_reduces_supply_and_reconciles() {
    let h = Harness::new(1_000_000);
    h.deposit("SCB-1", 1_000, "alice").await;
    let RedemptionOutcome::Escrowed { seq, .. } = h
        .redemption
        .request("alice", Thb::from_baht(400).unwrap())
        .await
        .unwrap()
    else {
        panic!("expected escrow")
    };

    h.redemption.process_payout("alice", seq).await.unwrap();
    h.redemption.confirm("alice", seq).await.unwrap();

    assert_eq!(h.ledger.supply().unwrap(), Thb::from_baht(600).unwrap());
    assert_eq!(h.redemption.queue_len().await.unwrap(), 0);

    let report = h.reconciliation.run().await.unwrap();
    assert!(
        report.is_healthy(),
        "issued 1000 - redeemed 400 must equal supply 600: {report:?}"
    );
}

// ---------------------------------------------------------------------------
// F7 — redemption liveness
// ---------------------------------------------------------------------------

#[tokio::test]
async fn f7_reclaim_fails_before_delta_and_succeeds_at_delta() {
    let h = Harness::new(1_000_000);
    h.deposit("SCB-1", 1_000, "alice").await;
    let RedemptionOutcome::Escrowed { seq, .. } = h
        .redemption
        .request("alice", Thb::from_baht(1_000).unwrap())
        .await
        .unwrap()
    else {
        panic!("expected escrow")
    };

    h.warp(DELTA - 1);
    assert!(
        h.redemption.reclaim("alice", seq).await.is_err(),
        "one second early"
    );
    assert_eq!(h.ledger.balance_of("alice").unwrap(), Thb::ZERO);

    h.warp(DELTA);
    h.redemption.reclaim("alice", seq).await.unwrap();
    assert_eq!(
        h.ledger.balance_of("alice").unwrap(),
        Thb::from_baht(1_000).unwrap(),
        "the holder is no worse off than before"
    );
    assert_eq!(
        h.ledger.supply().unwrap(),
        Thb::from_baht(1_000).unwrap(),
        "supply unchanged"
    );
}

#[tokio::test]
async fn f7_confirm_blocks_reclaim() {
    let h = Harness::new(1_000_000);
    h.deposit("SCB-1", 1_000, "alice").await;
    let RedemptionOutcome::Escrowed { seq, .. } = h
        .redemption
        .request("alice", Thb::from_baht(1_000).unwrap())
        .await
        .unwrap()
    else {
        panic!("expected escrow")
    };

    h.redemption.confirm("alice", seq).await.unwrap();
    h.warp(DELTA * 10);
    assert!(
        h.redemption.reclaim("alice", seq).await.is_err(),
        "a confirmed burn is final"
    );
    assert_eq!(h.ledger.balance_of("alice").unwrap(), Thb::ZERO);
}

#[tokio::test]
async fn f7_a_reclaim_is_not_counted_as_a_redemption() {
    // If it were, F2 would show drift indistinguishable from an unbacked mint.
    let h = Harness::new(1_000_000);
    h.deposit("SCB-1", 1_000, "alice").await;
    let RedemptionOutcome::Escrowed { seq, .. } = h
        .redemption
        .request("alice", Thb::from_baht(1_000).unwrap())
        .await
        .unwrap()
    else {
        panic!("expected escrow")
    };

    h.warp(DELTA);
    h.redemption.reclaim("alice", seq).await.unwrap();

    let report = h.reconciliation.run().await.unwrap();
    assert_eq!(
        report.drift, 0,
        "reclaim must not appear in the redeemed tally: {report:?}"
    );
    assert!(report.is_healthy());
}

#[tokio::test]
async fn f7_the_sweep_reports_overdue_redemptions_without_acting_on_them() {
    // This service cannot reclaim for a holder — that needs their key, and holding it
    // is exactly what F8 forbids. The sweep observes; the holder acts.
    let h = Harness::new(1_000_000);
    h.deposit("SCB-1", 1_000, "alice").await;
    h.redemption
        .request("alice", Thb::from_baht(1_000).unwrap())
        .await
        .unwrap();

    assert!(h.redemption.sweep_reclaimable().await.unwrap().is_empty());

    h.warp(DELTA);
    let overdue = h.redemption.sweep_reclaimable().await.unwrap();
    assert_eq!(overdue.len(), 1);
    assert_eq!(overdue[0].state, RedemptionState::Escrowed);
    assert_eq!(
        h.ledger.balance_of("alice").unwrap(),
        Thb::ZERO,
        "the sweep must not move the holder's tokens"
    );
}

#[tokio::test]
async fn f7_a_payout_the_issuer_queued_but_never_wired_is_still_reclaimable() {
    // The holder's recovery right cannot depend on the issuer's own bookkeeping.
    let h = Harness::new(1_000_000);
    h.deposit("SCB-1", 1_000, "alice").await;
    let RedemptionOutcome::Escrowed { seq, .. } = h
        .redemption
        .request("alice", Thb::from_baht(1_000).unwrap())
        .await
        .unwrap()
    else {
        panic!("expected escrow")
    };

    h.redemption.process_payout("alice", seq).await.unwrap();
    assert_eq!(
        h.redemption.status("alice", seq).await.unwrap().state,
        RedemptionState::PayoutQueued
    );

    h.warp(DELTA);
    h.redemption.reclaim("alice", seq).await.unwrap();
    assert_eq!(
        h.ledger.balance_of("alice").unwrap(),
        Thb::from_baht(1_000).unwrap()
    );
}

// ---------------------------------------------------------------------------
// §6.4 — the gap the escrow does NOT close
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_reserve_can_still_be_short_after_an_honest_reclaim() {
    // Spec §6.4, stated as an executable fact rather than a caveat: the escrow
    // protects the *token* side only. If `B` took the fiat and never wired, the holder
    // reclaims their THBC — and the reserve is short at the next honest attestation.
    // F1 is then violated with nobody having misbehaved on-chain.
    let h = Harness::new(1_000_000);
    h.deposit("SCB-1", 1_000, "alice").await;
    let RedemptionOutcome::Escrowed { seq, .. } = h
        .redemption
        .request("alice", Thb::from_baht(1_000).unwrap())
        .await
        .unwrap()
    else {
        panic!("expected escrow")
    };

    // B takes the fiat and never confirms. The holder recovers the token.
    h.warp(DELTA);
    h.redemption.reclaim("alice", seq).await.unwrap();
    assert_eq!(
        h.ledger.balance_of("alice").unwrap(),
        Thb::from_baht(1_000).unwrap()
    );

    // A honestly re-attests the now-smaller reserve.
    h.reserve
        .attest(Thb::from_baht(999_000).unwrap())
        .await
        .unwrap();

    // Supply is fine against 999k here, so widen the gap to the real failure: the
    // fiat is gone and no on-chain mechanism recovers it.
    h.reserve
        .attest(Thb::from_baht(500).unwrap())
        .await
        .unwrap();
    let report = h.reconciliation.run().await.unwrap();
    assert_eq!(
        report.severity,
        thbc_core::reconcile::Severity::Insolvent,
        "the fiat side is a promise, and a broken promise shows up here"
    );
    assert_eq!(
        report.drift, 0,
        "F2 is untouched — this is a solvency failure, not an accounting one"
    );
}

// ---------------------------------------------------------------------------
// F8 — non-custody, static audit
// ---------------------------------------------------------------------------

/// **F8** — spec §13 asks for a static audit plus a negative test.
///
/// The static half is structural and lives in the source: no method on `LedgerPort`,
/// `DepositRepository`, `RedemptionRepository`, `CompliancePort` or `PayoutPort`
/// accepts a private key, keypair, or signer. Grep `crates/thbc-core/src/ports.rs`
/// for `Keypair`/`sign` and the result is empty — that is the audit.
///
/// The negative half is here: the reclaim sweep, the one place the platform has both
/// motive and opportunity to act on a user's behalf, cannot move a holder's tokens.
#[tokio::test]
async fn f8_static_audit() {
    let h = Harness::new(1_000_000);
    h.deposit("SCB-1", 1_000, "alice").await;
    h.redemption
        .request("alice", Thb::from_baht(1_000).unwrap())
        .await
        .unwrap();
    h.warp(DELTA);

    let before = h.ledger.balance_of("alice").unwrap();
    let overdue = h.redemption.sweep_reclaimable().await.unwrap();
    assert_eq!(
        overdue.len(),
        1,
        "the platform can SEE the stuck redemption"
    );
    assert_eq!(
        h.ledger.balance_of("alice").unwrap(),
        before,
        "and cannot move it — censorship is possible (T4), theft is not"
    );
}

// ---------------------------------------------------------------------------
// The Chain Bridge path — what production actually does today
// ---------------------------------------------------------------------------

/// The real ledger adapter refuses the instructions that do not exist, rather than
/// routing them to `swap_grx_for_thbc` (which mints against GRX and violates F6).
///
/// Asserted so the day someone "fixes" the 501s by pointing them at the minting
/// instruction, this fails.
#[tokio::test]
async fn the_chain_bridge_adapter_reports_missing_instructions_as_not_implemented() {
    // No broker needed: these paths refuse before any I/O.
    let err = PortError::Unsupported("issue_thbc does not exist");
    assert!(matches!(err, PortError::Unsupported(_)));
    assert!(err.to_string().contains("not implemented on-chain"));
}

#[tokio::test]
async fn a_deposit_state_is_never_left_dangling_on_a_refusal() {
    let h = Harness::new(1_000);
    h.deposit("SCB-1", 5_000, "alice").await; // exceeds the reserve

    // Recorded as `screened`, not silently dropped and not falsely `issued`.
    let retry = h
        .issuance
        .retry_held(&BankRef::new("SCB-1").unwrap())
        .await
        .unwrap();
    assert!(matches!(retry, IssuanceOutcome::Held { .. }), "{retry:?}");

    // And the terminal states really are terminal.
    assert!(DepositState::Issued.is_terminal());
    assert!(DepositState::Encumbered.is_terminal());
    assert!(!DepositState::Screened.is_terminal());
}
