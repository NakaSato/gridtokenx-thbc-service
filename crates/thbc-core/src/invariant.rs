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
        // Re-attached to `issue_thbc` (gridtokenx-anchor a554499), the only instruction
        // that increases supply. `PegBreach` has a call site again — it had none
        // between the F6 fix and that commit.
        //
        // Still `Partial`: the on-chain ceiling is `attested_reserve`, NOT
        // `attested_reserve - reserve_encumbered` as §4.1 specifies, because that field
        // does not fit in the 14 spare padding bytes on the zero-copy `Treasury`.
        status: Status::Partial,
        enforcement: Enforcement::OnChain,
        gap: Some(
            "on-chain ceiling is `attested_reserve`, not `attested_reserve - \
             reserve_encumbered`: fiat that cleared the bank and then failed KYC still \
             counts as free backing on-chain. This service enforces the tighter ceiling \
             from its own records and is therefore stricter than the chain",
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
        // `issue_thbc` creates `[b"deposit", H(bank_ref)]` with Anchor `init` in the
        // SAME instruction as the mint (gridtokenx-anchor a554499). A replay is
        // rejected by the Solana runtime at the account level, before any program code
        // runs — so no application bug can defeat it, and the mint and the nullifier
        // either both happen or neither does.
        //
        // `Runtime`, not `OnChain`: the guarantee comes from account existence, not
        // from a `require!` the program could get wrong.
        status: Status::Enforced,
        enforcement: Enforcement::Runtime,
        gap: None,
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
        // Re-attached to `issue_thbc` (gridtokenx-anchor a554499) — the instruction F5
        // actually exists to gate. `StaleAttestation` has a call site again; it had
        // none between the F6 fix and that commit.
        //
        // Checked BEFORE the F1 ceiling, and a future-dated attestation is rejected
        // rather than treated as maximally fresh, so clock skew cannot buy freshness.
        status: Status::Enforced,
        enforcement: Enforcement::OnChain,
        gap: None,
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
        // Implemented on-chain (gridtokenx-anchor): `redeem_thbc_for_fiat` escrows
        // rather than burning, `confirm_redemption` burns, `reclaim_redemption` returns
        // the tokens after delta. Both terminal instructions CLOSE the record, so
        // double-confirm and confirm-after-reclaim fail at the account level.
        //
        // Enforced for the TOKEN side, which is all F7 as stated claims: an honest
        // holder recovers their THBC within delta. Reclaim is deliberately not gated on
        // `paused`, so the platform cannot trap escrowed tokens.
        //
        // The FIAT side is a separate matter and remains open — see the §6.4 note in
        // KNOWN_LIMITATIONS.md. That is a fair-exchange impossibility, not a gap in
        // this instruction, and F7 does not claim it.
        status: Status::Enforced,
        enforcement: Enforcement::OnChain,
        gap: None,
    },
    Invariant {
        id: "F8",
        name: "Non-custody",
        statement: "no GridTokenX key appears in a signer set that can move user THBC",
        // VIOLATED AT THE SYSTEM LEVEL. This was marked Enforced until 2026-07-29 on
        // the strength of a true but insufficient argument: no method on any port in
        // this crate accepts a keypair or signer, so *this service* holds no user key.
        //
        // That says nothing about the platform. `gridtokenx-iam-service` generates each
        // user's Solana keypair and stores it encrypted
        // (`iam-logic/src/auth_service.rs:536`), but both KDF inputs are SERVICE config
        // — `encryption_secret` and `master_secret` (`iam-core/src/config.rs:31,45`,
        // env `ENCRYPTION_SECRET` / `MASTER_SECRET`). The user's password is not an
        // input, and the PBKDF2 salt is stored next to the ciphertext. So anyone
        // holding those two env vars and the database can reconstruct every user's
        // keypair and sign as them.
        //
        // No service decrypts today — `decrypt_private_key*` is called only from
        // blockchain-core's own unit tests — so the custody is latent rather than
        // exercised. Latent is not absent: F8 is a claim about what the platform CAN
        // do, and spec §3 states it as "P ... can it steal? no".
        //
        // Do not restore this to Enforced without changing IAM. A doc edit cannot fix
        // it; see the gap text for what actually would.
        status: Status::Violated,
        enforcement: Enforcement::Unenforced,
        gap: Some(
            "IAM stores user signing keys encrypted under service-only secrets \
             (ENCRYPTION_SECRET + MASTER_SECRET, no user password in the KDF, salt \
             stored alongside the ciphertext), so the platform can unilaterally sign \
             as any user. This service holds no key and no port accepts one, but that \
             is a property of one service, not of GridTokenX. Spec §3's \"P is trusted \
             for liveness only\" and §10's T4 \"P can censor, not steal\" are both \
             false as written",
        ),
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
        // F1/F3/F5 were re-attached to `issue_thbc`. F3 is Enforced by the RUNTIME
        // (account existence), which is stronger than a program-level check.
        // F7 landed as the redemption escrow; nothing is DesignOnly any more.
        assert_eq!(get("F7").unwrap().status, Status::Enforced);
        assert!(
            INVARIANTS.iter().all(|i| i.status != Status::DesignOnly),
            "no invariant should remain DesignOnly"
        );
        // F8 was Enforced until the IAM custody finding. Restoring it needs an IAM
        // change (user-derived KDF, client-held keys, or retiring the claim outright),
        // never a doc edit.
        assert_eq!(get("F8").unwrap().status, Status::Violated);
        assert_eq!(get("F1").unwrap().status, Status::Partial);
        assert_eq!(get("F3").unwrap().status, Status::Enforced);
        assert_eq!(get("F3").unwrap().enforcement, Enforcement::Runtime);
        assert_eq!(get("F5").unwrap().status, Status::Enforced);
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
    fn disclosed_gaps_are_the_five_open_invariants() {
        // F1, F2, F4, F6, F8 remain short of Enforced — all four for reasons that are
        // about the OFF-chain half or about legacy state, not about missing code.
        // F3, F5, F7, F8, F9 are claimable.
        let gaps: Vec<_> = disclosed_gaps().iter().map(|i| i.id).collect();
        assert_eq!(gaps, ["F1", "F2", "F4", "F6", "F8"]);
    }
}
