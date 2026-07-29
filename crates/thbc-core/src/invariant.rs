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
        // `Enforced` as of 2026-07-29: the on-chain ceiling is now
        // `attested_reserve - reserve_encumbered`, exactly as §4.1 specifies.
        //
        // The field was previously called impossible without a layout change, which was
        // wrong in one specific way: a `Pubkey` (32 bytes) does not fit the spare
        // padding, but `reserve_encumbered` is a `u64`. The stored bumps end at offset
        // 259, the next 8-aligned offset is 264, and 264 + 8 = 272 — so it lands in the
        // tail of the old `_padding` without moving a single existing field. The
        // `Treasury` struct is STILL 272 bytes, every deployed PDA keeps deserializing,
        // and no re-init or realloc was needed. Pinned by `carved_fields_kept_their_offsets`
        // in the program's `state.rs`.
        //
        // On accounts initialized before the field existed those bytes are zero, so the
        // ceiling evaluates to `attested_reserve - 0` — identical to the old behaviour
        // until an attestation reports an encumbrance. `ReserveService::attest` now
        // publishes `total_encumbered()` with every attestation, so the chain enforces
        // the same ceiling this service does.
        //
        // Verified against the compiled program by four litesvm cases, three of which
        // die if the subtraction is removed (mutation-checked 2026-07-29).
        status: Status::Enforced,
        enforcement: Enforcement::OnChain,
        gap: None,
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
            "detective, not preventive — no write is rejected for causing drift; the \
             reconciler only reports it afterwards. It now runs on an interval and \
             appends every run to `reconciliation_runs`, so a breach that was later \
             resolved stays visible; until 2026-07-29 there was no scheduler at all and \
             nothing wrote that table, so spec §9's \"checked hourly and daily\" was \
             true of no deployment",
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
        // The localnet deployment was AUDITED on 2026-07-29 and is clean: `swap_vault`
        // holds 0 GRX. The old `swap_grx_for_thbc` pulled GRX collateral into that vault
        // whenever it minted, and the only instructions that drain it (`redeem_thbc_for_grx`
        // then, `exchange_thbc_for_grx` now) also return the THBC — so a zero balance
        // means no GRX-backed THBC is outstanding here. The 250_000 predating this work
        // therefore came from `issue_thbc`, i.e. it is fiat-referenced.
        //
        // Kept `Partial` regardless, and the distinction is worth stating: that audit is
        // a fact about ONE chain at ONE moment, established by a human reading a vault
        // balance. This binary can be pointed at any cluster, and nothing in it checks
        // provenance at startup or at runtime. Promoting F6 to `Enforced` on the strength
        // of a manual check would be exactly the pattern that produced the other defects
        // in this registry — a guarantee asserted from a one-off observation. What would
        // actually earn it: a startup assertion that `swap_vault` is empty, or a
        // documented retirement of the legacy supply.
        gap: Some(
            "the code is fixed — the exchange path transfers from `[b\"thbc_inventory\"]` \
             and no program mints or burns THBC any more — but THBC minted by the \
             previous `swap_grx_for_thbc` is GRX-backed and would still be outstanding on \
             any chain that ran it. The localnet deployment was audited 2026-07-29 and is \
             clean (`swap_vault` holds 0 GRX, so no GRX-backed THBC exists there), but \
             nothing in this service verifies that for the chain it is actually pointed \
             at. A startup check on `swap_vault`, or a documented retirement of the legacy \
             supply, is what turns this Enforced",
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
        // VERIFIED ON THE LIVE CHAIN 2026-07-29, which matters because until that day
        // it could not have worked: the `[b"redeem_escrow"]` vault had never been
        // created, so every `redeem_thbc_for_fiat` would have failed
        // `RedemptionEscrowNotInitialized`. This was `Enforced` on litesvm evidence
        // alone while being impossible in the deployed environment.
        //
        // Against the deployed program: escrowing 5_000 moved the tokens out of the
        // holder's ATA and into the escrow while `thbc_supply` stayed at 410_000 (the
        // F7 property — the holder's tokens are not destroyed on request and remain
        // recoverable); `reclaim_redemption` before delta was refused with
        // `TimelockNotExpired`; `confirm_redemption` burned exactly 5_000, leaving
        // tracked supply equal to mint supply; and a second confirm was refused
        // because the record had been closed.
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
        // Nothing decrypts those stored keys today — `decrypt_private_key*` is called
        // only from blockchain-core's own unit tests. That made this look like latent
        // custody awaiting a decision. It is not. Custody is the platform's settled,
        // documented posture, in three places:
        //
        //   1. The IAM function is named `provision_custodial_wallet` and says so:
        //      "custody is service-side by design so IAM can sign on the user's
        //      behalf" (`iam-logic/src/auth_service.rs:502,518`).
        //   2. blockchain-core lists "IAM stores encrypted custodial keys" as a
        //      load-bearing invariant of the shared crate (`CLAUDE.md`, invariant 6).
        //   3. The platform moved FURTHER toward custody: IAM's per-user `SignMessage`
        //      RPC was removed in favour of Chain Bridge signing with its single
        //      `platform_admin` Vault Transit key. `sign_message` now fails loudly
        //      (`trading-service/.../identity/mod.rs:48`), and no `.proto` in the tree
        //      declares per-user signing at all.
        //
        // So the platform's ability to move user assets is NOT latent — it is the
        // production settlement path (`execute_atomic_settlement`, where every escrow
        // is `ATA(platform, mint)` and the platform is sole signer). The stored
        // per-user keys are the unused residue of the capability that was removed.
        //
        // F8 was therefore never true of this system at any point. It cannot be fixed
        // by deleting those keys, and it cannot be fixed here at all: it would require
        // restoring per-user Ed25519 signing platform-wide, which is exactly the
        // blocker recorded against Option A in `docs/proposed/rec-production-settlement.md`.
        status: Status::Violated,
        enforcement: Enforcement::Unenforced,
        gap: Some(
            "GridTokenX is custodial by design, not by accident. IAM provisions each \
             user's keypair encrypted under service-only secrets (ENCRYPTION_SECRET + \
             MASTER_SECRET, no user password in the KDF), and production settlement \
             signs for every party with one platform_admin key. Per-user Ed25519 \
             signing was REMOVED, so custody is the direction the platform chose, not \
             a gap it drifted into. This service holds no key and no port accepts one, \
             but that is a property of one service, not of GridTokenX. Spec §3's \"P is \
             trusted for liveness only\" and §10's T4 \"P can censor, not steal\" are \
             false as written and should be retired rather than fixed",
        ),
    },
    Invariant {
        id: "F9",
        name: "Attestation independence",
        statement: "the attestor key != the parameter-admin key",
        // This entry claimed `Enforced`/`OnChain` and cited
        // `programs/treasury/src/instructions/initialize.rs` as rejecting
        // equality. It does not. `initialize` takes `attestor: Pubkey` and
        // writes `t.attestor = attestor` with no comparison, and the treasury
        // program defines no `AttestorIsAuthority` error at all — the only one
        // lives off-chain in this crate. Demonstrated on 2026-07-29:
        // `init-treasury.ts` sets `attestor = authority.publicKey` and the
        // program accepted it, so the deployed localnet treasury has
        // attestor == authority.
        //
        // There is also a live reason it currently MUST be that way. Chain
        // Bridge signs with a single Vault key: `issue_thbc` requires
        // `Treasury.authority` and `update_attestation` requires
        // `Treasury.attestor`, and the bridge passes `platform_admin` for both.
        //
        // Scoped precisely on 2026-07-29 — enforcing F9 takes THREE changes, and
        // any one alone breaks the deployment:
        //
        //   1. A distinct signing key for attestation (the shape
        //      `CHAIN_BRIDGE_REC_VALIDATOR_KEY_NAME` already uses for the REC
        //      co-signer).
        //   2. Relaxing Chain Bridge's authorization gate. `SignerPort` is already
        //      multi-key — `sign_message(key_name, ..)` — but `sign_and_submit`
        //      rejects any `key_id` other than `"platform_admin"`
        //      (gridtokenx-chain-bridge/CLAUDE.md, "Key ID not authorized"). That
        //      gate is the Reference Monitor's authorization check, so widening it
        //      is a security change, not a config tweak.
        //   3. A way to move the LIVE treasury's attestor. `Treasury.attestor` is
        //      written once at `initialize` with no setter, so this needs either a
        //      new `set_attestor` instruction or a re-init. A setter gated on
        //      `authority` would partly defeat the point — the parameter admin
        //      could grant itself attestation rights — so it should require the
        //      CURRENT attestor to sign its own rotation.
        //
        // Adding only the `require!(attestor != authority)` guard to `initialize`
        // is worse than nothing today: it does not touch the already-initialized
        // treasury, and it would make `scripts/init-treasury.ts` fail on any fresh
        // environment, since that script passes one key for both roles.
        //
        // `reserve::TreasuryKeys::new` still rejects equality, but that is this
        // service refusing to submit, not the chain refusing the transaction:
        // advisory, and a different caller bypasses it entirely.
        status: Status::DesignOnly,
        enforcement: Enforcement::Unenforced,
        gap: Some(
            "initialize does not compare attestor to authority and the treasury program has \
             no error for it; the deployed localnet treasury has attestor == authority. \
             Enforcing this requires a separate Vault key for attestation, because the \
             bridge signs both roles with platform_admin.",
        ),
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
        // F7 landed as the redemption escrow.
        assert_eq!(get("F7").unwrap().status, Status::Enforced);
        // F9 is the one DesignOnly entry, and it got there by being CORRECTED
        // rather than by regressing: it claimed Enforced/OnChain citing a check
        // in `initialize` that does not exist. Do not "fix" this by flipping it
        // back — enforcing it needs `require!(attestor != authority)` on-chain
        // AND a separate Vault key for attestation, because the bridge currently
        // signs both roles with platform_admin.
        assert_eq!(get("F9").unwrap().status, Status::DesignOnly);
        assert_eq!(get("F9").unwrap().enforcement, Enforcement::Unenforced);
        let design_only: Vec<_> = INVARIANTS
            .iter()
            .filter(|i| i.status == Status::DesignOnly)
            .map(|i| i.id)
            .collect();
        assert_eq!(
            design_only,
            ["F9"],
            "only F9 should be DesignOnly; a new one means work regressed"
        );
        // F8 was Enforced until the IAM custody finding. Restoring it needs an IAM
        // change (user-derived KDF, client-held keys, or retiring the claim outright),
        // never a doc edit.
        assert_eq!(get("F8").unwrap().status, Status::Violated);
        // F1 moved Partial -> Enforced on 2026-07-29, when `reserve_encumbered`
        // landed on-chain in the tail of the old `_padding` (offset 264, struct still
        // 272 bytes, no re-init). The ceiling is now `attested_reserve -
        // reserve_encumbered` exactly as spec §4.1 requires. Do NOT flip this back
        // without also removing the field and its four litesvm cases.
        assert_eq!(get("F1").unwrap().status, Status::Enforced);
        assert_eq!(get("F1").unwrap().enforcement, Enforcement::OnChain);
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
    fn disclosed_gaps_are_the_six_open_invariants() {
        // F2, F4, F6, F8 are short of Enforced for reasons about the OFF-chain half
        // or about legacy state, not missing code.
        //
        // F1 LEFT this list on 2026-07-29: `reserve_encumbered` is on-chain and the
        // ceiling is now the specified `attested_reserve - reserve_encumbered`.
        //
        // F9 joined them on 2026-07-29. It had claimed Enforced/OnChain citing a
        // check in `initialize` that does not exist — the program never compares
        // attestor to authority, and the deployed treasury has them equal.
        // F1, F3, F5 and F7 are claimable as guarantees.
        let gaps: Vec<_> = disclosed_gaps().iter().map(|i| i.id).collect();
        assert_eq!(gaps, ["F2", "F4", "F6", "F8", "F9"]);
    }
}
