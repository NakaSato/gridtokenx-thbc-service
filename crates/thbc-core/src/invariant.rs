//! The F1–F9 invariant registry.
//!
//! `docs/product-specs/THBC_ISSUER_SERVICE.md` §2 states nine invariants and §12
//! states which are actually implemented. Those two tables drift the moment someone
//! ships a fix and forgets the doc — and the failure mode is the bad one: a document
//! claiming a guarantee the code does not provide.
//!
//! So the status lives here, in code, and the service reports it. `admin-api` serves
//! this list to the `E` regulator surface, and the `KNOWN_LIMITATIONS.md` generator
//! reads it. If you implement F3, you change [`Status`] here and the doc, the API
//! response, and the limitations file all move together.
//!
//! **Do not mark an invariant [`Status::Enforced`] because the happy path passes.**
//! §13 is explicit: report partial coverage as partial.

use serde::{Deserialize, Serialize};

/// How much of an invariant is actually load-bearing today.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// Enforced by running code, with a test that exercises the *violating* path.
    Enforced,
    /// Code exists and is exercised, but a documented gap remains. Carries the gap.
    Partial,
    /// Specified, not built. The guarantee does not exist.
    DesignOnly,
    /// Known to be violated by current code. Strictly worse than `DesignOnly`:
    /// something is actively doing the wrong thing.
    Violated,
}

impl Status {
    /// True when the invariant may be described as a guarantee to a third party.
    #[must_use]
    pub const fn is_claimable(self) -> bool {
        matches!(self, Self::Enforced)
    }
}

/// Where the invariant is enforced. Off-chain enforcement is advisory: this service
/// can refuse to *ask* for a violating state transition, but only the program can
/// refuse to *perform* one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Enforcement {
    /// The Anchor program rejects the transaction. Binding.
    OnChain,
    /// The Solana runtime rejects it at the account level (e.g. `init` on an
    /// existing PDA). Binding, and not defeatable by an application bug.
    Runtime,
    /// This service refuses to submit. Advisory — a different caller could still try.
    OffChain,
    /// Nothing enforces it yet.
    Unenforced,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Invariant {
    /// `"F1"` … `"F9"`.
    pub id: &'static str,
    pub name: &'static str,
    pub statement: &'static str,
    pub status: Status,
    pub enforcement: Enforcement,
    /// The disclosed gap. `None` only when [`Status::Enforced`].
    pub gap: Option<&'static str>,
}

/// The registry. Ordered F1..F9.
///
/// Statuses reflect the tree as of 2026-07-29, verified against
/// `gridtokenx-anchor/programs/treasury/src/`.
pub const INVARIANTS: [Invariant; 9] = [
    Invariant {
        id: "F1",
        name: "Reserve sufficiency",
        statement: "thbc_supply <= attested_reserve - reserve_encumbered at all times",
        // The ceiling USED to be enforced by `compute_swap_grx_for_thbc`
        // (`TreasuryError::PegBreach`). The F6 fix removed that instruction, and with
        // it the only caller — `PegBreach` now has zero call sites and
        // `attested_reserve` is written by `update_attestation` but never read for a
        // check.
        //
        // F1 is currently VACUOUS rather than enforced: no program mints THBC at all,
        // so supply cannot grow past anything. That is not the same as a guarantee,
        // and the ceiling must be re-attached to `issue_thbc` when it lands.
        status: Status::DesignOnly,
        enforcement: Enforcement::Unenforced,
        gap: Some(
            "vacuously true, not enforced: no program mints THBC, so supply cannot grow — \
             but `PegBreach` has no call site since the minting swap was removed, and \
             `reserve_encumbered` still is not an on-chain field. `issue_thbc` must \
             re-attach the ceiling as `attested_reserve - reserve_encumbered`",
        ),
    },
    Invariant {
        id: "F2",
        name: "Issuance conservation",
        statement: "sum(issued) - sum(redeemed) = thbc_supply",
        // Only checkable, not enforceable: nothing rejects a write for breaking it.
        // The reconciler detects drift after the fact.
        status: Status::Partial,
        enforcement: Enforcement::OffChain,
        gap: Some(
            "detective, not preventive — the reconciler reports drift hourly/daily but no \
             write is rejected for causing it; also currently reconciles against the \
             simulated ledger, since issue/redeem do not exist on-chain",
        ),
    },
    Invariant {
        id: "F3",
        name: "Deposit idempotency",
        statement: "one confirmed bank_ref => at most one issuance",
        status: Status::DesignOnly,
        enforcement: Enforcement::Unenforced,
        gap: Some(
            "the `[b\"deposit\", H(bank_ref)]` nullifier PDA does not exist on-chain. \
             This service dedupes on a UNIQUE index over bank_ref_hash, which stops \
             replays through this service and nothing else",
        ),
    },
    Invariant {
        id: "F4",
        name: "Burn-before-wire",
        statement: "on-chain burn confirmed strictly before fiat payout is enqueued",
        // The state machine in `redemption.rs` enforces the ordering, and it is the
        // only path to a payout. But there is no fiat rail behind it to get the
        // ordering wrong against.
        status: Status::Partial,
        enforcement: Enforcement::OffChain,
        gap: Some(
            "ordering is enforced by the redemption state machine, but no fiat rail \
             exists — the barrier has never been tested against a real payout queue",
        ),
    },
    Invariant {
        id: "F5",
        name: "Attestation freshness",
        statement: "now - attestation_ts <= attestation_ttl, else issuance halts",
        // WAS Enforced, by the freshness check in `swap_grx_for_thbc`. The F6 fix
        // removed that instruction, and the check went with it — deliberately: F5
        // guards *issuance*, and the replacement `exchange_*` path issues nothing, so
        // keeping it there would have cost liveness and protected nothing.
        //
        // The consequence is that `StaleAttestation` now has zero call sites. F5 is
        // unreachable rather than violated — nothing issues, so nothing can issue
        // against a stale reserve — but an unreachable guard is not a guarantee, and
        // this must be re-attached to `issue_thbc`.
        //
        // This is the registry doing its job: F5 was claimable before 2026-07-29 and
        // is not any more, and that had to be visible rather than assumed.
        status: Status::DesignOnly,
        enforcement: Enforcement::Unenforced,
        gap: Some(
            "unreachable, not enforced: the freshness check lived on the minting swap \
             and was removed with it. `StaleAttestation` has no call site. `issue_thbc` \
             must carry the check, since that is the instruction F5 exists to gate",
        ),
    },
    Invariant {
        id: "F6",
        name: "Backing-set purity",
        statement: "collateral backing THBC is fiat only; the exchange path never mints",
        // FIXED on-chain 2026-07-29: `swap_grx_for_thbc`/`redeem_thbc_for_grx` were
        // replaced by `exchange_grx_for_thbc`/`exchange_thbc_for_grx`, which transfer
        // against a `[b"thbc_inventory"]` vault. There is now no `mint_to` or `burn`
        // of THBC anywhere in any program.
        //
        // Still `Partial`, not `Enforced`, and the distinction is code vs *state*: the
        // code can no longer mint against GRX, but THBC already minted by the old swap
        // is still outstanding and is GRX-backed. F6 is a claim about what backs the
        // supply, so it cannot be claimed until that legacy supply is retired or a
        // re-init clears it.
        status: Status::Partial,
        enforcement: Enforcement::OnChain,
        gap: Some(
            "the code is fixed — the exchange path transfers from `[b\"thbc_inventory\"]` \
             and no program mints or burns THBC any more — but THBC minted by the \
             previous `swap_grx_for_thbc` is still outstanding on any chain that ran it, \
             and that supply is GRX-backed. Retiring or re-initialising it is what turns \
             this Enforced",
        ),
    },
    Invariant {
        id: "F7",
        name: "Redemption liveness",
        statement: "an honest holder obtains fiat or recovers THBC within delta",
        status: Status::DesignOnly,
        enforcement: Enforcement::Unenforced,
        gap: Some(
            "the timelocked redemption escrow does not exist on-chain. The token side is \
             modelled here and in the simulated ledger only. The fiat side is unsolved \
             even in design — see spec §6.4",
        ),
    },
    Invariant {
        id: "F8",
        name: "Non-custody",
        statement: "no GridTokenX key appears in a signer set that can move user THBC",
        // Structural: this service holds no user key and every user-value instruction
        // it constructs is user-signed. See `ports::LedgerPort` — no method takes a
        // user keypair.
        status: Status::Enforced,
        enforcement: Enforcement::OnChain,
        gap: None,
    },
    Invariant {
        id: "F9",
        name: "Attestation independence",
        statement: "the attestor key != the parameter-admin key",
        // programs/treasury/src/instructions/initialize.rs rejects equality; also
        // re-checked here in `reserve::TreasuryKeys::new` so a misconfigured deploy
        // fails at startup rather than at first attestation.
        status: Status::Enforced,
        enforcement: Enforcement::OnChain,
        gap: None,
    },
];

/// Look an invariant up by id (`"F3"`).
#[must_use]
pub fn get(id: &str) -> Option<&'static Invariant> {
    INVARIANTS.iter().find(|i| i.id == id)
}

/// Invariants that must not be described as guarantees. This is what
/// `KNOWN_LIMITATIONS.md` and the regulator read surface are built from.
#[must_use]
pub fn disclosed_gaps() -> Vec<&'static Invariant> {
    INVARIANTS
        .iter()
        .filter(|i| !i.status.is_claimable())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_f1_through_f9_in_order() {
        let ids: Vec<_> = INVARIANTS.iter().map(|i| i.id).collect();
        assert_eq!(ids, ["F1", "F2", "F3", "F4", "F5", "F6", "F7", "F8", "F9"]);
    }

    #[test]
    fn every_non_enforced_invariant_discloses_its_gap() {
        // The point of the registry: you cannot mark something short of Enforced
        // without saying what is missing.
        for inv in &INVARIANTS {
            if inv.status.is_claimable() {
                assert!(
                    inv.gap.is_none(),
                    "{} is Enforced but carries a gap",
                    inv.id
                );
            } else {
                assert!(
                    inv.gap.is_some(),
                    "{} is not Enforced but discloses no gap",
                    inv.id
                );
            }
        }
    }

    #[test]
    fn spec_section_12_gaps_are_still_open() {
        // Guards against someone flipping a status to make a dashboard green.
        // Flip these deliberately, together with §12 of the spec, when the on-chain
        // work actually lands.
        assert_eq!(get("F3").unwrap().status, Status::DesignOnly);
        assert_eq!(get("F7").unwrap().status, Status::DesignOnly);
        // F1 and F5 became unenforceable when the F6 fix removed the minting swap that
        // carried their guards. Both must be re-attached to `issue_thbc`.
        assert_eq!(get("F1").unwrap().status, Status::DesignOnly);
        assert_eq!(get("F5").unwrap().status, Status::DesignOnly);
        // F6 moved Violated -> Partial when the on-chain exchange path stopped
        // minting (2026-07-29). It is NOT Enforced: legacy GRX-backed supply from the
        // old `swap_grx_for_thbc` may still be outstanding.
        assert_eq!(get("F6").unwrap().status, Status::Partial);
    }

    #[test]
    fn unenforced_invariants_are_never_claimable() {
        for inv in &INVARIANTS {
            if inv.enforcement == Enforcement::Unenforced {
                assert!(
                    !inv.status.is_claimable(),
                    "{} claims an unenforced guarantee",
                    inv.id
                );
            }
        }
    }

    #[test]
    fn disclosed_gaps_are_the_seven_open_invariants() {
        // F1..F7 are all short of Enforced. Only F8 and F9 may be stated as
        // guarantees, and both are structural rather than instruction-level — which
        // is precisely why they survived the removal of the minting swap.
        let gaps: Vec<_> = disclosed_gaps().iter().map(|i| i.id).collect();
        assert_eq!(gaps, ["F1", "F2", "F3", "F4", "F5", "F6", "F7"]);
    }
}
