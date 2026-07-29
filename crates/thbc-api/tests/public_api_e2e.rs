//! End-to-end tests for the **public** surface (`/v1`, `public.rs`) — through the
//! real router, the real services, against `SimulatedLedger`.
//!
//! **Failure cases come first, and that ordering is the point.** Every route here
//! has a happy path that is trivially reachable and a set of refusals that are the
//! actual product: `409 timelock_not_expired` is F7 working, `422
//! insufficient_inventory` is F6 working, and a `400` on a zero amount is the money
//! type refusing to represent a no-op. A suite that only proved the happy path would
//! pass just as well against a router that never refused anything.
//!
//! What these tests cover: routing, extraction, the service sequencing, and the
//! `CoreError`/`PortError` → HTTP mapping in `state.rs`. What they do **not** cover:
//! the chain. `SimulatedLedger` models the treasury *as specified*, and for the
//! redemption path the treasury program does not implement the instructions at all —
//! in `chain-bridge` mode `escrow_redemption` and `reclaim_redemption` return
//! `Unsupported` by design. So a green run here says the service behaves correctly;
//! it says nothing about what the chain does. Do not read F7 coverage off this file.
//!
//! Authentication is absent on purpose: JWT is terminated at APISIX and this crate
//! never verifies it (`lib.rs:11-13`). These tests exercise the surface an
//! authenticated caller reaches, not the gateway in front of it.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use serde_json::{Value, json};
use thbc_core::bank_ref::BankRef;
use thbc_core::exchange::ExchangeParams;
use thbc_core::money::{Grx, Thb};
use thbc_core::ports::{
    Clock, DepositRepository, LedgerPort, PayoutPort, ReconciliationRepository, RedemptionRepository,
};
use thbc_ledger::SimulatedLedger;
use thbc_logic::adapters::{FixedClock, RecordingPayoutQueue, StubCompliance};
use thbc_logic::{
    IssuanceService, ReconciliationService, RedemptionService, ReserveService, TreasuryService,
};
use thbc_persistence::{InMemoryDepositRepo, InMemoryReconciliationRepo, InMemoryRedemptionRepo};
use tower::ServiceExt as _;

use thbc_api::AppState;

const DELTA: i64 = 86_400;
const TTL: i64 = 3_600;

/// 4 THB per whole GRX: `atoms * 4_000_000 / 1e9`. Named so the quote arithmetic in
/// the exchange tests reads as intent rather than as magic constants.
const GRX_ATOMS_PER_BAHT: u64 = 250_000_000;

/// `thbc_core::money::GRX_ATOMS_PER_WHOLE` as `u64` — the exported constant is `u128`
/// because the exchange math needs the headroom, and `Grx::from_atoms` takes `u64`.
const GRX_ATOMS_PER_WHOLE: u64 = 1_000_000_000;

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

struct Api {
    app: Router,
    ledger: Arc<SimulatedLedger>,
    clock: Arc<FixedClock>,
    issuance: Arc<IssuanceService>,
    redemption: Arc<RedemptionService>,
}

impl Api {
    fn new(reserve_baht: u64) -> Self {
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
        // Ceiling above every amount here, so a screen failure never masquerades as
        // the refusal a test is actually asserting.
        let compliance = Arc::new(StubCompliance::new(
            Thb::from_baht(1_000_000).expect("fits"),
        ));

        let reserve = Arc::new(ReserveService::new(
            Arc::clone(&ledger) as Arc<dyn LedgerPort>,
            Arc::clone(&deposits) as Arc<dyn DepositRepository>,
            Arc::clone(&clock) as Arc<dyn Clock>,
        ));
        let issuance = Arc::new(IssuanceService::new(
            Arc::clone(&deposits) as Arc<dyn DepositRepository>,
            compliance,
            Arc::clone(&ledger) as Arc<dyn LedgerPort>,
            Arc::clone(&clock) as Arc<dyn Clock>,
        ));
        let redemption = Arc::new(RedemptionService::new(
            Arc::clone(&redemptions) as Arc<dyn RedemptionRepository>,
            Arc::clone(&ledger) as Arc<dyn LedgerPort>,
            payouts as Arc<dyn PayoutPort>,
            Arc::clone(&clock) as Arc<dyn Clock>,
            DELTA,
        ));
        let reconciliation = Arc::new(ReconciliationService::new(
            Arc::clone(&deposits) as Arc<dyn DepositRepository>,
            Arc::clone(&redemptions) as Arc<dyn RedemptionRepository>,
            Arc::clone(&reserve),
            history as Arc<dyn ReconciliationRepository>,
            Arc::clone(&clock) as Arc<dyn Clock>,
        ));

        let state = AppState {
            issuance: Arc::clone(&issuance),
            redemption: Arc::clone(&redemption),
            reserve,
            reconciliation,
            treasury: Arc::new(TreasuryService::new(
                Arc::clone(&ledger) as Arc<dyn LedgerPort>
            )),
            simulated: true,
        };

        Self {
            app: thbc_api::router(state),
            ledger,
            clock,
            issuance,
            redemption,
        }
    }

    /// Move both clocks together. The simulator keeps its own so it can model on-chain
    /// `Clock::get()`; letting them drift makes every timelock assertion meaningless.
    fn warp(&self, to: i64) {
        self.clock.set(to);
        self.ledger.set_now(to).expect("simulated clock");
    }

    /// Give a user a THBC balance the way the system actually would — through the
    /// partner deposit path — rather than by poking the ledger. A balance that
    /// appeared without a deposit would leave the F2 tally inconsistent and make any
    /// later refusal ambiguous.
    ///
    /// The beneficiary wallet and the user id are the same string on purpose: in the
    /// simulator the wallet *is* the balance key, and the redemption path keys off
    /// `user`. Passing distinct values issues to one account and redeems from another,
    /// which surfaces as an unrelated underflow several steps later.
    async fn fund_user(&self, reference: &str, baht: u64, user: &str) {
        self.issuance
            .handle_deposit(
                BankRef::new(reference).expect("valid ref"),
                Thb::from_baht(baht).expect("fits"),
                user,
                user,
            )
            .await
            .expect("deposit issued");
    }

    async fn get(&self, uri: &str) -> (StatusCode, Value) {
        self.send(Method::GET, uri, None).await
    }

    async fn post(&self, uri: &str, body: Value) -> (StatusCode, Value) {
        self.send(Method::POST, uri, Some(body)).await
    }

    /// POST with no body at all — the reclaim route takes none.
    async fn post_empty(&self, uri: &str) -> (StatusCode, Value) {
        self.send(Method::POST, uri, None).await
    }

    async fn send(&self, method: Method, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
        let mut req = Request::builder().method(method).uri(uri);
        let body = match body {
            Some(v) => {
                req = req.header(header::CONTENT_TYPE, "application/json");
                Body::from(serde_json::to_vec(&v).expect("serialize body"))
            }
            None => Body::empty(),
        };
        let res = self
            .app
            .clone()
            .oneshot(req.body(body).expect("build request"))
            .await
            .expect("router responded");

        let status = res.status();
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .expect("read body");
        // Router-level rejections (a bad path segment, an unparseable body) answer in
        // plain text, not JSON. Keep them readable instead of panicking on parse.
        let json = serde_json::from_slice(&bytes).unwrap_or_else(|_| {
            json!({ "raw": String::from_utf8_lossy(&bytes).to_string() })
        });
        (status, json)
    }
}

/// Assert on the stable machine-readable code, never the prose message. The message
/// is for humans and is free to change; the code is the contract a client matches on.
fn assert_code(body: &Value, expected: &str) {
    assert_eq!(
        body["code"], expected,
        "wrong error code; full body: {body}"
    );
}

// ===========================================================================
// FAILURE CASES
// ===========================================================================

// ---------------------------------------------------------------------------
// POST /v1/redemptions
// ---------------------------------------------------------------------------

/// A zero redemption is refused by the money type before any ledger contact.
///
/// It also must leave nothing behind. `RedemptionService::request` allocates a `seq`
/// *before* validating, so the rejection has to unwind cleanly: the next valid request
/// must still receive seq 1. A burned sequence number would be a permanent hole in the
/// per-user record set that nothing later fills in.
#[tokio::test]
async fn redeeming_zero_is_400_and_persists_no_record() {
    let api = Api::new(1_000_000);
    api.fund_user("SCB-1", 1_000, "alice").await;

    let (status, body) = api
        .post("/v1/redemptions", json!({"user": "alice", "amount_minor": 0}))
        .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_code(&body, "invalid_request");

    let (status, _) = api.get("/v1/redemptions/alice/1").await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "a rejected request must not leave a redemption row behind"
    );

    let (status, body) = api
        .post(
            "/v1/redemptions",
            json!({"user": "alice", "amount_minor": Thb::from_baht(100).unwrap().minor()}),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["seq"], 1, "the rejected request must not consume a seq");
}

/// Redeeming more than the holder owns fails at the token balance, not at F1.
///
/// This is the escrow refusing to move tokens that do not exist — the same thing the
/// SPL token program would do. Nothing about the reserve is consulted.
#[tokio::test]
async fn redeeming_more_than_the_holder_owns_is_400() {
    let api = Api::new(1_000_000);
    api.fund_user("SCB-1", 1_000, "alice").await;

    let (status, body) = api
        .post(
            "/v1/redemptions",
            json!({"user": "alice", "amount_minor": Thb::from_baht(2_000).unwrap().minor()}),
        )
        .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_code(&body, "invalid_request");
}

/// A user with no deposit at all has no balance — same refusal, no special case.
#[tokio::test]
async fn redeeming_as_an_unknown_user_is_400() {
    let api = Api::new(1_000_000);

    let (status, body) = api
        .post(
            "/v1/redemptions",
            json!({"user": "nobody", "amount_minor": Thb::from_baht(1).unwrap().minor()}),
        )
        .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_code(&body, "invalid_request");
}

// ---------------------------------------------------------------------------
// GET /v1/redemptions/{user}/{seq}
// ---------------------------------------------------------------------------

#[tokio::test]
async fn status_of_unknown_redemption_is_404() {
    let api = Api::new(1_000_000);

    let (status, body) = api.get("/v1/redemptions/alice/99").await;

    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_code(&body, "not_found");
}

/// A non-numeric `seq` is rejected by extraction, before any handler runs.
///
/// Worth pinning: it must be a 400, not a 404. A 404 would tell a caller "no such
/// redemption" when the truth is "that is not a redemption id".
#[tokio::test]
async fn status_with_a_non_numeric_seq_is_400_at_the_router() {
    let api = Api::new(1_000_000);

    let (status, _) = api.get("/v1/redemptions/alice/not-a-number").await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
}

// ---------------------------------------------------------------------------
// POST /v1/redemptions/{user}/{seq}/reclaim — F7
// ---------------------------------------------------------------------------

/// **The headline refusal.** Reclaiming before Δ is a `409 timelock_not_expired`, and
/// that is the invariant working, not an outage.
///
/// The guard fires in the domain model before the ledger is touched, so a premature
/// attempt never reaches the chain.
#[tokio::test]
async fn reclaim_before_delta_is_409_timelock_not_expired() {
    let api = Api::new(1_000_000);
    api.fund_user("SCB-1", 1_000, "alice").await;
    api.post(
        "/v1/redemptions",
        json!({"user": "alice", "amount_minor": Thb::from_baht(100).unwrap().minor()}),
    )
    .await;

    let (status, body) = api.post_empty("/v1/redemptions/alice/1/reclaim").await;

    assert_eq!(status, StatusCode::CONFLICT);
    assert_code(&body, "timelock_not_expired");
}

/// One second short of Δ still refuses. The boundary is `elapsed >= delta`, and an
/// off-by-one here would hand the holder their tokens back while the issuer still had
/// a valid window to confirm in.
#[tokio::test]
async fn reclaim_one_second_before_delta_still_refuses() {
    let api = Api::new(1_000_000);
    api.fund_user("SCB-1", 1_000, "alice").await;
    api.post(
        "/v1/redemptions",
        json!({"user": "alice", "amount_minor": Thb::from_baht(100).unwrap().minor()}),
    )
    .await;

    api.warp(DELTA - 1);
    let (status, body) = api.post_empty("/v1/redemptions/alice/1/reclaim").await;

    assert_eq!(status, StatusCode::CONFLICT);
    assert_code(&body, "timelock_not_expired");

    // And the status route agrees rather than telling the holder a different story.
    let (_, view) = api.get("/v1/redemptions/alice/1").await;
    assert_eq!(view["reclaim_in_secs"], 1);
    assert_eq!(view["reclaimable_now"], false);
}

#[tokio::test]
async fn reclaim_of_unknown_redemption_is_404() {
    let api = Api::new(1_000_000);

    let (status, body) = api.post_empty("/v1/redemptions/alice/7/reclaim").await;

    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_code(&body, "not_found");
}

/// Once the issuer has confirmed the burn, reclaim is closed **forever** — even past
/// Δ. The tokens are gone; letting the holder reclaim would restore supply the burn
/// already removed, which is F2 drift indistinguishable from an unbacked mint.
#[tokio::test]
async fn reclaim_after_the_issuer_confirms_is_409_invalid_state() {
    let api = Api::new(1_000_000);
    api.fund_user("SCB-1", 1_000, "alice").await;
    api.post(
        "/v1/redemptions",
        json!({"user": "alice", "amount_minor": Thb::from_baht(100).unwrap().minor()}),
    )
    .await;

    // The admin/issuer half of the flow, invoked directly — the public router has no
    // route for it, which is the point.
    api.redemption.confirm("alice", 1).await.expect("confirmed");

    // Well past Δ: this is not the timelock refusing, it is the state machine.
    api.warp(DELTA * 10);
    let (status, body) = api.post_empty("/v1/redemptions/alice/1/reclaim").await;

    assert_eq!(status, StatusCode::CONFLICT);
    assert_code(&body, "invalid_state");
}

/// Reclaim is not idempotent and must not pretend to be. A second call after a
/// successful reclaim is a state error, not a silent 200 — a repeated success would
/// read to a caller as a second restoration of tokens.
#[tokio::test]
async fn reclaiming_twice_is_409_invalid_state() {
    let api = Api::new(1_000_000);
    api.fund_user("SCB-1", 1_000, "alice").await;
    api.post(
        "/v1/redemptions",
        json!({"user": "alice", "amount_minor": Thb::from_baht(100).unwrap().minor()}),
    )
    .await;

    api.warp(DELTA);
    let (status, _) = api.post_empty("/v1/redemptions/alice/1/reclaim").await;
    assert_eq!(status, StatusCode::OK, "first reclaim should succeed at Δ");

    let (status, body) = api.post_empty("/v1/redemptions/alice/1/reclaim").await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_code(&body, "invalid_state");
}

// ---------------------------------------------------------------------------
// POST /v1/exchange/quote/buy
// ---------------------------------------------------------------------------

#[tokio::test]
async fn buy_quote_of_zero_grx_is_400() {
    let api = Api::new(1_000_000);

    let (status, body) = api
        .post("/v1/exchange/quote/buy", json!({"grx_in_atoms": 0}))
        .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_code(&body, "invalid_request");
}

/// **F6.** A quote larger than platform inventory is a refusal, never a mint.
///
/// This is the single most important negative case on the exchange path: the
/// alternative implementation — minting the shortfall — would create THBC with no
/// fiat behind it and break F1 at the same time.
#[tokio::test]
async fn buy_quote_beyond_inventory_is_422_insufficient_inventory() {
    let api = Api::new(1_000_000);
    api.fund_user("SCB-1", 1_000, "alice").await;
    // Inventory is moved out of alice's existing balance — it is THBC the platform
    // holds, never conjured.
    api.ledger
        .fund_inventory("alice", Thb::from_baht(500).unwrap())
        .expect("inventory funded");

    // ~1,000 baht of THBC requested against 500 baht of inventory.
    let (status, body) = api
        .post(
            "/v1/exchange/quote/buy",
            json!({"grx_in_atoms": 1_000 * GRX_ATOMS_PER_BAHT}),
        )
        .await;

    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_code(&body, "insufficient_inventory");
}

/// An empty inventory refuses every quote rather than returning a zero-out quote a
/// caller might act on.
#[tokio::test]
async fn buy_quote_against_empty_inventory_is_422() {
    let api = Api::new(1_000_000);

    let (status, body) = api
        .post(
            "/v1/exchange/quote/buy",
            json!({"grx_in_atoms": 100 * GRX_ATOMS_PER_BAHT}),
        )
        .await;

    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_code(&body, "insufficient_inventory");
}

/// A paused exchange is 503, not 422 — the request was fine and retrying later is the
/// correct client behaviour.
#[tokio::test]
async fn buy_quote_while_paused_is_503() {
    let api = Api::new(1_000_000);
    api.fund_user("SCB-1", 1_000, "alice").await;
    api.ledger
        .fund_inventory("alice", Thb::from_baht(500).unwrap())
        .expect("inventory funded");
    api.ledger
        .set_params(ExchangeParams {
            grx_per_thbc_rate: 4_000_000,
            fee_bps: 25,
            paused: true,
        })
        .expect("params set");

    let (status, body) = api
        .post(
            "/v1/exchange/quote/buy",
            json!({"grx_in_atoms": 100 * GRX_ATOMS_PER_BAHT}),
        )
        .await;

    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_code(&body, "paused");
}

/// An unset rate is a *misconfiguration*, not a bad request — 500, and the code says
/// so. Blaming the caller for an operator's empty parameter would send them retrying
/// forever.
#[tokio::test]
async fn buy_quote_with_an_unset_rate_is_500_misconfigured() {
    let api = Api::new(1_000_000);
    api.ledger
        .set_params(ExchangeParams {
            grx_per_thbc_rate: 0,
            fee_bps: 25,
            paused: false,
        })
        .expect("params set");

    let (status, body) = api
        .post(
            "/v1/exchange/quote/buy",
            json!({"grx_in_atoms": 100 * GRX_ATOMS_PER_BAHT}),
        )
        .await;

    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_code(&body, "misconfigured");
}

// ---------------------------------------------------------------------------
// POST /v1/exchange/quote/sell
// ---------------------------------------------------------------------------

#[tokio::test]
async fn sell_quote_of_zero_thbc_is_400() {
    let api = Api::new(1_000_000);

    let (status, body) = api
        .post("/v1/exchange/quote/sell", json!({"thbc_in_minor": 0}))
        .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_code(&body, "invalid_request");
}

/// An empty GRX vault refuses every sell quote. Symmetric with the buy side: the vault
/// is finite and is drawn down rather than over-drawn.
///
/// `SimulatedLedger` starts `inventory.grx` at zero, so this is the default state and
/// the success case below has to seed it explicitly.
#[tokio::test]
async fn sell_quote_is_unfulfillable_while_grx_inventory_is_empty() {
    let api = Api::new(1_000_000);

    let (status, body) = api
        .post(
            "/v1/exchange/quote/sell",
            json!({"thbc_in_minor": Thb::from_baht(100).unwrap().minor()}),
        )
        .await;

    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_code(&body, "insufficient_inventory");
}

// ===========================================================================
// SUCCESS CASES
//
// Every public route proven reachable. These are not decoration: without them a
// router that failed *every* request would pass the entire failure suite above, so
// each refusal is only meaningful once its corresponding success is pinned.
//
// Two things are asserted throughout that a status code alone would miss — the exact
// arithmetic of a quote, and the wording of a redemption response. Both are places
// where a plausible-looking success would be wrong.
// ===========================================================================

/// Deposit → redeem → wait Δ → reclaim, through HTTP only.
///
/// The response wording is asserted, not just the status: step 1 must report
/// `escrowed` and never `redeemed`. The fiat leg is a promise by the issuer, and
/// telling a user their redemption succeeded here is the exact overclaim the design
/// exists to prevent.
#[tokio::test]
async fn full_redemption_lifecycle_escrow_then_reclaim_at_delta() {
    let api = Api::new(1_000_000);
    api.fund_user("SCB-1", 1_000, "alice").await;
    let amount = Thb::from_baht(100).unwrap().minor();

    let (status, body) = api
        .post(
            "/v1/redemptions",
            json!({"user": "alice", "amount_minor": amount}),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "escrowed");
    // Sequences are 1-based per user (`InMemoryRedemptionRepo::next_seq`).
    assert_eq!(body["seq"], 1);
    assert_eq!(body["reclaim_after_secs"], DELTA);

    let (status, view) = api.get("/v1/redemptions/alice/1").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(view["state"], "escrowed");
    assert_eq!(view["amount_minor"], amount);
    assert_eq!(view["reclaim_in_secs"], DELTA);
    assert_eq!(view["reclaimable_now"], false);

    api.warp(DELTA);
    let (_, view) = api.get("/v1/redemptions/alice/1").await;
    assert_eq!(view["reclaim_in_secs"], 0);
    assert_eq!(view["reclaimable_now"], true);

    let (status, body) = api.post_empty("/v1/redemptions/alice/1/reclaim").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "reclaimed");

    let (_, view) = api.get("/v1/redemptions/alice/1").await;
    assert_eq!(view["state"], "reclaimed");

    // The tokens are back with the holder, and supply never moved: a reclaim is not a
    // redemption and must never enter the redeemed tally.
    assert_eq!(
        api.ledger.balance_of("alice").unwrap(),
        Thb::from_baht(1_000).unwrap()
    );
}

/// A quote that fits inventory succeeds and declares `changes_supply: false` — stated
/// on every quote so the F6 claim is checkable from outside the process.
#[tokio::test]
async fn buy_quote_within_inventory_succeeds_and_declares_no_supply_change() {
    let api = Api::new(1_000_000);
    api.fund_user("SCB-1", 1_000, "alice").await;
    api.ledger
        .fund_inventory("alice", Thb::from_baht(500).unwrap())
        .expect("inventory funded");

    let grx_in = 100 * GRX_ATOMS_PER_BAHT;
    let (status, body) = api
        .post("/v1/exchange/quote/buy", json!({"grx_in_atoms": grx_in}))
        .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["in_amount"], grx_in);
    assert_eq!(body["changes_supply"], false);

    // 100 baht gross at 25 bps: 250_000 minor fee, 99_750_000 minor out.
    assert_eq!(body["fee"], 250_000);
    assert_eq!(body["out_amount"], 99_750_000);
}

/// The boundary case: a quote whose output is *exactly* inventory succeeds.
///
/// The guard is `net > inventory`, not `>=`. An off-by-one here would strand the last
/// unit of inventory permanently unquotable, which no error message would ever explain.
#[tokio::test]
async fn buy_quote_for_exactly_the_available_inventory_succeeds() {
    let api = Api::new(1_000_000);
    api.fund_user("SCB-1", 1_000, "alice").await;
    // Exactly the net output of the quote below, to the minor unit.
    api.ledger
        .fund_inventory("alice", Thb::from_minor(99_750_000))
        .expect("inventory funded");

    let (status, body) = api
        .post(
            "/v1/exchange/quote/buy",
            json!({"grx_in_atoms": 100 * GRX_ATOMS_PER_BAHT}),
        )
        .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["out_amount"], 99_750_000);
}

/// The sell path, with the GRX vault seeded so it has a reachable success.
///
/// The GRX vault is funded directly rather than out of a balance, because GRX supply
/// is not modelled by this ledger at all — it lives in the GRX mint. See
/// `SimulatedLedger::fund_grx_inventory` for why that asymmetry with THBC inventory is
/// correct rather than a shortcut.
#[tokio::test]
async fn sell_quote_within_the_grx_vault_succeeds_and_declares_no_supply_change() {
    let api = Api::new(1_000_000);
    api.ledger
        .fund_grx_inventory(Grx::from_atoms(50 * GRX_ATOMS_PER_WHOLE))
        .expect("grx vault funded");

    let thbc_in = Thb::from_baht(100).unwrap().minor();
    let (status, body) = api
        .post(
            "/v1/exchange/quote/sell",
            json!({"thbc_in_minor": thbc_in}),
        )
        .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["in_amount"], thbc_in);
    assert_eq!(body["changes_supply"], false);

    // 100 baht at 4 THB/GRX = 25 GRX gross; 25 bps of that is 0.0625 GRX.
    assert_eq!(body["fee"], 62_500_000);
    assert_eq!(body["out_amount"], 24_937_500_000_u64);
}

/// A quote is a quote. Repeating one must not move inventory or supply — nothing is
/// reserved, nothing is executed.
///
/// This is what makes the exchange path's `changes_supply: false` claim true rather
/// than merely asserted: quoting is the only operation these routes perform.
#[tokio::test]
async fn quoting_repeatedly_moves_neither_inventory_nor_supply() {
    let api = Api::new(1_000_000);
    api.fund_user("SCB-1", 1_000, "alice").await;
    api.ledger
        .fund_inventory("alice", Thb::from_baht(500).unwrap())
        .expect("inventory funded");
    api.ledger
        .fund_grx_inventory(Grx::from_atoms(50 * GRX_ATOMS_PER_WHOLE))
        .expect("grx vault funded");

    let supply_before = api.ledger.supply().unwrap();
    let inventory_before = api.ledger.snapshot().await.unwrap().inventory;

    for _ in 0..3 {
        let (status, _) = api
            .post(
                "/v1/exchange/quote/buy",
                json!({"grx_in_atoms": 100 * GRX_ATOMS_PER_BAHT}),
            )
            .await;
        assert_eq!(status, StatusCode::OK);

        let (status, _) = api
            .post(
                "/v1/exchange/quote/sell",
                json!({"thbc_in_minor": Thb::from_baht(100).unwrap().minor()}),
            )
            .await;
        assert_eq!(status, StatusCode::OK);
    }

    assert_eq!(api.ledger.supply().unwrap(), supply_before);
    assert_eq!(
        api.ledger.snapshot().await.unwrap().inventory,
        inventory_before
    );
}

/// Two redemptions by one holder get sequential, independently addressable seqs.
///
/// Worth pinning because the status and reclaim routes are keyed on `(user, seq)`: if
/// seqs collided, one redemption's reclaim would silently act on another's record.
#[tokio::test]
async fn successive_redemptions_get_sequential_and_independent_seqs() {
    let api = Api::new(1_000_000);
    api.fund_user("SCB-1", 1_000, "alice").await;

    let first = Thb::from_baht(300).unwrap().minor();
    let second = Thb::from_baht(200).unwrap().minor();

    let (status, body) = api
        .post(
            "/v1/redemptions",
            json!({"user": "alice", "amount_minor": first}),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["seq"], 1);

    let (status, body) = api
        .post(
            "/v1/redemptions",
            json!({"user": "alice", "amount_minor": second}),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["seq"], 2);

    let (_, view) = api.get("/v1/redemptions/alice/1").await;
    assert_eq!(view["amount_minor"], first);
    let (_, view) = api.get("/v1/redemptions/alice/2").await;
    assert_eq!(view["amount_minor"], second);

    // Reclaiming one leaves the other untouched.
    api.warp(DELTA);
    let (status, _) = api.post_empty("/v1/redemptions/alice/1/reclaim").await;
    assert_eq!(status, StatusCode::OK);

    let (_, view) = api.get("/v1/redemptions/alice/2").await;
    assert_eq!(view["state"], "escrowed", "seq 2 must be unaffected");
}

/// Redemptions are per-user. Two holders both start at seq 1 and cannot see or act on
/// each other's records.
#[tokio::test]
async fn redemption_seqs_are_scoped_per_user() {
    let api = Api::new(1_000_000);
    api.fund_user("SCB-1", 1_000, "alice").await;
    api.fund_user("SCB-2", 1_000, "bob").await;
    let amount = Thb::from_baht(100).unwrap().minor();

    for user in ["alice", "bob"] {
        let (status, body) = api
            .post("/v1/redemptions", json!({"user": user, "amount_minor": amount}))
            .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["seq"], 1, "{user} should start at seq 1");
    }

    // Alice's seq 1 exists; carol has nothing at seq 1.
    let (status, _) = api.get("/v1/redemptions/alice/1").await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = api.get("/v1/redemptions/carol/1").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// A partial redemption escrows only what was asked for; the remainder stays spendable
/// and can be redeemed down to exactly zero.
#[tokio::test]
async fn a_partial_redemption_leaves_the_remainder_spendable() {
    let api = Api::new(1_000_000);
    api.fund_user("SCB-1", 1_000, "alice").await;

    let (status, _) = api
        .post(
            "/v1/redemptions",
            json!({"user": "alice", "amount_minor": Thb::from_baht(300).unwrap().minor()}),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        api.ledger.balance_of("alice").unwrap(),
        Thb::from_baht(700).unwrap(),
        "only the escrowed amount leaves the holder's balance"
    );

    // The exact remainder still redeems.
    let (status, _) = api
        .post(
            "/v1/redemptions",
            json!({"user": "alice", "amount_minor": Thb::from_baht(700).unwrap().minor()}),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(api.ledger.balance_of("alice").unwrap(), Thb::ZERO);

    // And one minor unit past zero does not.
    let (status, body) = api
        .post("/v1/redemptions", json!({"user": "alice", "amount_minor": 1}))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_code(&body, "invalid_request");
}

/// Once the issuer confirms, the holder's status view says `confirmed` and stops
/// offering reclaim — permanently, not just until Δ.
///
/// The `reclaimable_now: false` at `10Δ` is the assertion that matters: a view that
/// went on advertising reclaim after the burn would invite a request that can only
/// ever 409.
#[tokio::test]
async fn status_after_the_issuer_confirms_reports_confirmed_and_never_reclaimable() {
    let api = Api::new(1_000_000);
    api.fund_user("SCB-1", 1_000, "alice").await;
    api.post(
        "/v1/redemptions",
        json!({"user": "alice", "amount_minor": Thb::from_baht(100).unwrap().minor()}),
    )
    .await;

    api.redemption.confirm("alice", 1).await.expect("confirmed");

    let (status, view) = api.get("/v1/redemptions/alice/1").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(view["state"], "confirmed");
    assert_eq!(view["reclaimable_now"], false);

    api.warp(DELTA * 10);
    let (_, view) = api.get("/v1/redemptions/alice/1").await;
    assert_eq!(view["state"], "confirmed");
    assert_eq!(
        view["reclaimable_now"], false,
        "a burned redemption must never be advertised as reclaimable"
    );
}
