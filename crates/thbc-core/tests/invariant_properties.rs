//! Property tests for the arithmetic invariants — spec §13, the `proptest` rows.
//!
//! | Invariant | §13 test | Here |
//! |---|---|---|
//! | F1 | supply never exceeds attested reserve across random issue/redeem sequences | [`f1`] |
//! | F2 | `Σ issued − Σ redeemed = supply` after arbitrary interleaving | [`f2`] |
//! | F6 | exchange path never changes `thbc_supply` | [`f6`] |
//!
//! These test the **domain model**, which is where the arithmetic lives. They do not
//! test the on-chain program, and passing here says nothing about whether the
//! treasury program enforces the same thing — for F6 it demonstrably does not.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use proptest::prelude::*;
use thbc_core::exchange::{ExchangeParams, Inventory, quote_grx_for_thbc, quote_thbc_for_grx};
use thbc_core::money::{Grx, Thb};
use thbc_core::reconcile::{LedgerTally, Severity, reconcile};
use thbc_core::reserve::{Attestation, ReserveState};

/// One step in a randomly generated ledger history.
#[derive(Debug, Clone, Copy)]
enum Op {
    Issue(u64),
    /// Confirmed redemption — burns, reduces supply.
    Redeem(u64),
    /// Escrowed then reclaimed. Carries no amount because the amount is exactly
    /// what must not matter: the tokens came back, so neither supply nor the tally
    /// moves regardless of size.
    Reclaim,
    /// The attestor re-attests, up or down.
    Attest(u64),
}

fn op_strategy() -> impl Strategy<Value = Op> {
    prop_oneof![
        (1u64..=1_000_000).prop_map(Op::Issue),
        (1u64..=1_000_000).prop_map(Op::Redeem),
        Just(Op::Reclaim),
        (0u64..=10_000_000).prop_map(Op::Attest),
    ]
}

/// Replay a history against the domain model, applying each operation only if the
/// model permits it — exactly as the service would.
struct Model {
    state: ReserveState,
    tally: LedgerTally,
}

impl Model {
    fn new(reserve: u64, encumbered: u64) -> Self {
        Self {
            state: ReserveState::new(
                Attestation::new(Thb::from_minor(reserve), 0, i64::MAX),
                Thb::from_minor(encumbered),
                Thb::ZERO,
            ),
            tally: LedgerTally::default(),
        }
    }

    fn apply(&mut self, op: Op) {
        match op {
            Op::Issue(n) => {
                let amount = Thb::from_minor(n);
                // The F1 gate. A refused issuance changes nothing, including the tally.
                if let Ok(new_supply) = self.state.check_issuance(amount, 0) {
                    self.state.supply = new_supply;
                    if let Ok(t) = self.tally.issued.checked_add(amount) {
                        self.tally.issued = t;
                    }
                }
            }
            Op::Redeem(n) => {
                let amount = Thb::from_minor(n);
                // Cannot burn more than exists.
                if let Ok(new_supply) = self.state.supply.checked_sub(amount) {
                    self.state.supply = new_supply;
                    if let Ok(t) = self.tally.redeemed.checked_add(amount) {
                        self.tally.redeemed = t;
                    }
                }
            }
            // Escrow-then-reclaim: supply unchanged and, critically, the tally is
            // untouched. Counting a reclaim as redeemed is the single easiest way to
            // break F2, so the property suite exercises it on every history.
            Op::Reclaim => {}
            Op::Attest(n) => self.state.attestation.reserve = Thb::from_minor(n),
        }
    }
}

proptest! {
    /// **F1** — supply never exceeds free backing across random issue/redeem sequences.
    ///
    /// Note the guard: this holds only while no *downward* re-attestation happens
    /// (`Op::Attest` is excluded here and tested separately below). An attestor
    /// lowering the reserve below outstanding supply breaches F1 without any
    /// issuance, which is the §6.4 failure and cannot be arithmetic-ed away.
    #[test]
    fn f1_supply_never_exceeds_free_backing(
        reserve in 0u64..10_000_000,
        encumbered in 0u64..2_000_000,
        ops in prop::collection::vec(
            prop_oneof![
                (1u64..=1_000_000).prop_map(Op::Issue),
                (1u64..=1_000_000).prop_map(Op::Redeem),
                Just(Op::Reclaim),
            ],
            0..40,
        ),
    ) {
        let mut model = Model::new(reserve, encumbered);
        for op in ops {
            model.apply(op);
            prop_assert!(
                model.state.f1_holds(),
                "F1 breached: supply {} > free backing {}",
                model.state.supply.minor(),
                model.state.free_backing().minor(),
            );
        }
    }

    /// **F2** — `Σ issued − Σ redeemed = supply` after arbitrary interleaving,
    /// including reclaims and re-attestations.
    #[test]
    fn f2_conservation_holds_after_arbitrary_interleaving(
        reserve in 0u64..10_000_000,
        encumbered in 0u64..2_000_000,
        ops in prop::collection::vec(op_strategy(), 0..40),
    ) {
        let mut model = Model::new(reserve, encumbered);
        for op in ops {
            model.apply(op);
        }

        let expected = model.tally.expected_supply().expect("redeemed never exceeds issued");
        prop_assert_eq!(
            expected, model.state.supply,
            "F2 drift: issued {} - redeemed {} != supply {}",
            model.tally.issued.minor(), model.tally.redeemed.minor(), model.state.supply.minor(),
        );

        // And the reconciler agrees — it is the thing that would actually report this
        // in production, so a passing identity that the reconciler flags is still a bug.
        let report = reconcile(&model.tally, &model.state, 0).expect("reconcile");
        prop_assert_eq!(report.drift, 0);
        prop_assert!(report.severity != Severity::Drift);
    }

    /// **F6** — the exchange path never changes supply, in either direction.
    ///
    /// Structural rather than statistical: the quote types carry no supply field, so
    /// what is actually asserted is that inventory and the user's holding conserve
    /// the total between them. A quote that could mint would have to break that sum.
    #[test]
    fn f6_exchange_conserves_total_thbc(
        grx_in in 1u64..100_000_000_000,
        rate in 1u64..100_000_000,
        fee_bps in 0u16..=10_000,
        inv_thbc in 0u64..10_000_000_000,
        inv_grx in 0u64..10_000_000_000_000,
    ) {
        let params = ExchangeParams { grx_per_thbc_rate: rate, fee_bps, paused: false };
        let inventory = Inventory {
            thbc: Thb::from_minor(inv_thbc),
            grx: Grx::from_atoms(inv_grx),
        };

        // Only successful quotes say anything. A refusal (dust, insufficient
        // inventory, overflow) is F6 working — it declines rather than minting.
        if let Ok(q) = quote_grx_for_thbc(Grx::from_atoms(grx_in), &params, inventory) {
            let after = q.inventory_after.thbc.checked_add(q.thbc_out)
                .expect("conserved total cannot overflow the pre-existing total");
            prop_assert_eq!(
                after, inventory.thbc,
                "F6: exchange created or destroyed THBC",
            );
            prop_assert!(q.thbc_out <= inventory.thbc, "paid out more than inventory held");
        }
    }

    /// **F6, reverse** — THBC sold returns to inventory rather than being burned.
    #[test]
    fn f6_reverse_exchange_conserves_total_thbc(
        thbc_in in 1u64..10_000_000_000,
        rate in 1u64..100_000_000,
        fee_bps in 0u16..=10_000,
        inv_thbc in 0u64..10_000_000_000,
        inv_grx in 0u64..10_000_000_000_000,
    ) {
        let params = ExchangeParams { grx_per_thbc_rate: rate, fee_bps, paused: false };
        let inventory = Inventory {
            thbc: Thb::from_minor(inv_thbc),
            grx: Grx::from_atoms(inv_grx),
        };

        if let Ok(q) = quote_thbc_for_grx(Thb::from_minor(thbc_in), &params, inventory) {
            let expected = inventory.thbc.checked_add(q.thbc_in).expect("no overflow");
            prop_assert_eq!(
                q.inventory_after.thbc, expected,
                "F6: incoming THBC was burned instead of returning to inventory",
            );
            prop_assert!(q.grx_out <= inventory.grx, "paid out more GRX than the vault held");
        }
    }

    /// A quote is never *silently* free: any accepted exchange moves a positive
    /// amount in both directions.
    #[test]
    fn an_accepted_quote_always_moves_value_both_ways(
        grx_in in 1u64..100_000_000_000,
        rate in 1u64..100_000_000,
        fee_bps in 0u16..10_000,
        inv_thbc in 0u64..10_000_000_000,
    ) {
        let params = ExchangeParams { grx_per_thbc_rate: rate, fee_bps, paused: false };
        let inventory = Inventory { thbc: Thb::from_minor(inv_thbc), grx: Grx::ZERO };

        if let Ok(q) = quote_grx_for_thbc(Grx::from_atoms(grx_in), &params, inventory) {
            prop_assert!(!q.thbc_out.is_zero(), "user paid GRX and received nothing");
            prop_assert!(!q.grx_in.is_zero());
        }
    }
}

/// **F1, the case the property test excludes.** A downward re-attestation breaches
/// F1 with no issuance involved, and no arithmetic prevents it.
///
/// Called out as its own test rather than folded into the property run because it is
/// a *true* statement about the system, not a bug: an honest attestor reporting a
/// genuine shortfall must be able to say so. The system's job is to make it visible,
/// which is what the reconciler's `Insolvent` severity does.
#[test]
fn f1_can_be_breached_by_a_downward_reattestation_and_the_reconciler_says_so() {
    let mut model = Model::new(1_000_000, 0);
    model.apply(Op::Issue(900_000));
    assert!(model.state.f1_holds());

    model.apply(Op::Attest(500_000));
    assert!(
        !model.state.f1_holds(),
        "supply 900k against 500k backing must breach F1"
    );

    let report = reconcile(&model.tally, &model.state, 0).expect("reconcile");
    assert_eq!(report.severity, Severity::Insolvent);
    assert_eq!(report.shortfall.minor(), 400_000);
    // F2 still holds exactly — the identity is untouched by the reserve moving.
    assert_eq!(report.drift, 0);
}
