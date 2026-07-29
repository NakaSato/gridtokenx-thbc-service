//! The localnet simulator.
//!
//! Spec §12: *"The prototype simulates the payment leg as an on-chain token transfer
//! on localnet. No claim in this document about fiat should be read as describing
//! running code."*
//!
//! This is that prototype. It models the treasury state the `treasury` program
//! *would* hold if §4 were implemented — including the two things that do not exist
//! on-chain today, the deposit nullifier set and the redemption escrow — and applies
//! the same guards the program would.
//!
//! It exists for one reason: the F1–F7 invariant tests in `tests/` need something to
//! execute against, and executing them against a model that faithfully implements the
//! spec is more informative than not executing them at all. It is emphatically **not**
//! evidence that the on-chain program does any of this. When `issue_thbc` and friends
//! land, these tests move to `LiteSVM` and this simulator becomes a fixture.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use async_trait::async_trait;
use thbc_core::bank_ref::BankRefHash;
use thbc_core::exchange::{ExchangeParams, Inventory};
use thbc_core::money::{Grx, Thb};
use thbc_core::ports::{LedgerPort, PortError, PortResult, TreasurySnapshot};
use thbc_core::redemption::{ConfirmOutcome, Redemption};
use thbc_core::reserve::{Attestation, ReserveState};

/// An escrowed redemption, as the `[b"redeem", user, seq]` record would hold it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct EscrowRecord {
    amount: Thb,
    escrowed_at: i64,
    /// True once `confirm_redemption` burned it. A burned record stays present so a
    /// late `reclaim_redemption` is rejected rather than silently re-minting.
    burned: bool,
}

#[derive(Debug)]
struct State {
    reserve: ReserveState,
    inventory: Inventory,
    params: ExchangeParams,
    /// `[b"deposit", H(bank_ref)]` — the F3 nullifier set. Account existence is the
    /// guarantee; a `HashSet` is the faithful model of it.
    nullifiers: HashSet<BankRefHash>,
    escrows: HashMap<(String, u64), EscrowRecord>,
    /// Per-user THBC balances, so the simulator can refuse an escrow the user cannot
    /// fund — the token program would.
    balances: HashMap<String, Thb>,
    now: i64,
}

/// In-memory treasury. `Clone` shares the same state, so a service and its test can
/// hold the same ledger.
#[derive(Debug)]
pub struct SimulatedLedger {
    state: Mutex<State>,
}

impl SimulatedLedger {
    /// A treasury with `attested_reserve = reserve`, attested at `now`.
    #[must_use]
    pub fn new(reserve: Thb, ttl_secs: i64, now: i64) -> Self {
        Self {
            state: Mutex::new(State {
                reserve: ReserveState::new(
                    Attestation::new(reserve, now, ttl_secs),
                    Thb::ZERO,
                    Thb::ZERO,
                ),
                inventory: Inventory {
                    thbc: Thb::ZERO,
                    grx: Grx::ZERO,
                },
                params: ExchangeParams {
                    grx_per_thbc_rate: 4_000_000,
                    fee_bps: 25,
                    paused: false,
                },
                nullifiers: HashSet::new(),
                escrows: HashMap::new(),
                balances: HashMap::new(),
                now,
            }),
        }
    }

    fn lock(&self) -> PortResult<std::sync::MutexGuard<'_, State>> {
        // A poisoned mutex means a previous caller panicked mid-mutation, so the
        // modelled treasury may be torn. Refuse rather than read it.
        self.state
            .lock()
            .map_err(|_| PortError::Transient("simulated ledger state is poisoned".into()))
    }

    /// Advance the simulated clock. Δ and TTL tests warp time instead of sleeping.
    pub fn set_now(&self, now: i64) -> PortResult<()> {
        self.lock()?.now = now;
        Ok(())
    }

    pub fn set_params(&self, params: ExchangeParams) -> PortResult<()> {
        self.lock()?.params = params;
        Ok(())
    }

    /// Seed platform-held THBC for the exchange path.
    ///
    /// Takes it from *supply that already exists*: inventory is THBC the platform
    /// bought or was issued, never conjured. Fails if supply is insufficient, which
    /// is the same refusal F6 demands of the exchange path itself.
    pub fn fund_inventory(&self, from_user: &str, amount: Thb) -> PortResult<()> {
        let mut s = self.lock()?;
        let bal = s.balances.get(from_user).copied().unwrap_or(Thb::ZERO);
        let remaining = bal.checked_sub(amount).map_err(PortError::Domain)?;
        s.balances.insert(from_user.to_string(), remaining);
        s.inventory.thbc = s
            .inventory
            .thbc
            .checked_add(amount)
            .map_err(PortError::Domain)?;
        Ok(())
    }

    pub fn balance_of(&self, user: &str) -> PortResult<Thb> {
        Ok(self
            .lock()?
            .balances
            .get(user)
            .copied()
            .unwrap_or(Thb::ZERO))
    }

    pub fn supply(&self) -> PortResult<Thb> {
        Ok(self.lock()?.reserve.supply)
    }

    pub fn set_encumbered(&self, amount: Thb) -> PortResult<()> {
        self.lock()?.reserve.encumbered = amount;
        Ok(())
    }

    /// Whether `[b"deposit", H(bank_ref)]` exists.
    pub fn nullifier_exists(&self, n: BankRefHash) -> PortResult<bool> {
        Ok(self.lock()?.nullifiers.contains(&n))
    }
}

#[async_trait]
impl LedgerPort for SimulatedLedger {
    async fn snapshot(&self) -> PortResult<TreasurySnapshot> {
        let s = self.lock()?;
        let pending =
            u32::try_from(s.escrows.values().filter(|e| !e.burned).count()).unwrap_or(u32::MAX);
        Ok(TreasurySnapshot {
            reserve: s.reserve,
            inventory: s.inventory,
            params: s.params,
            redemption_queue_len: Some(pending),
        })
    }

    async fn issue(
        &self,
        beneficiary: &str,
        amount: Thb,
        nullifier: BankRefHash,
    ) -> PortResult<ConfirmOutcome> {
        let mut s = self.lock()?;
        let now = s.now;

        // F3 — modelled as Anchor `init` on `[b"deposit", H(bank_ref)]`: the account
        // already exists, so the runtime rejects the whole instruction. Checked
        // *before* the mint and in the same critical section, because that is the
        // property the on-chain construction has and a two-step check would not.
        if !s.nullifiers.insert(nullifier) {
            return Err(PortError::Rejected(format!(
                "F3: deposit nullifier {nullifier} already exists"
            )));
        }

        // F5 then F1, same order as the program.
        let new_supply = match s.reserve.check_issuance(amount, now) {
            Ok(v) => v,
            Err(e) => {
                // The instruction reverts as a unit, so the nullifier is not created.
                s.nullifiers.remove(&nullifier);
                return Err(PortError::Domain(e));
            }
        };

        s.reserve.supply = new_supply;
        let entry = s
            .balances
            .entry(beneficiary.to_string())
            .or_insert(Thb::ZERO);
        *entry = entry.checked_add(amount).map_err(PortError::Domain)?;
        Ok(ConfirmOutcome::Confirmed)
    }

    async fn escrow_redemption(&self, redemption: &Redemption) -> PortResult<ConfirmOutcome> {
        let mut s = self.lock()?;
        let now = s.now;
        let key = (redemption.user.clone(), redemption.seq);

        if s.escrows.contains_key(&key) {
            return Err(PortError::Conflict(format!(
                "redemption record [redeem, {}, {}] already exists",
                redemption.user, redemption.seq
            )));
        }

        // Move the tokens out of the user's balance into the escrow record. Supply is
        // deliberately unchanged: F7 says the burn happens at `confirm_redemption`,
        // not here, so the holder can still recover.
        let bal = s
            .balances
            .get(&redemption.user)
            .copied()
            .unwrap_or(Thb::ZERO);
        let remaining = bal
            .checked_sub(redemption.amount)
            .map_err(PortError::Domain)?;
        s.balances.insert(redemption.user.clone(), remaining);
        s.escrows.insert(
            key,
            EscrowRecord {
                amount: redemption.amount,
                escrowed_at: now,
                burned: false,
            },
        );

        Ok(ConfirmOutcome::Confirmed)
    }

    async fn confirm_redemption(&self, user: &str, seq: u64) -> PortResult<ConfirmOutcome> {
        let mut s = self.lock()?;
        let key = (user.to_string(), seq);
        let record = *s.escrows.get(&key).ok_or(PortError::NotFound)?;
        if record.burned {
            return Err(PortError::Rejected("redemption already confirmed".into()));
        }

        // The burn — the only place supply decreases.
        s.reserve.supply = s
            .reserve
            .supply
            .checked_sub(record.amount)
            .map_err(PortError::Domain)?;
        s.escrows.insert(
            key,
            EscrowRecord {
                burned: true,
                ..record
            },
        );
        Ok(ConfirmOutcome::Confirmed)
    }

    async fn reclaim_redemption(&self, user: &str, seq: u64) -> PortResult<ConfirmOutcome> {
        let mut s = self.lock()?;
        let now = s.now;
        let key = (user.to_string(), seq);
        let record = *s.escrows.get(&key).ok_or(PortError::NotFound)?;

        if record.burned {
            return Err(PortError::Rejected(
                "F7: redemption was confirmed; the burn is irreversible".into(),
            ));
        }

        // The Δ check is the on-chain one. The service-side machine checks it too,
        // but a caller that skipped the service must still be refused here.
        let delta = SIMULATED_DELTA_SECS;
        let elapsed = now.saturating_sub(record.escrowed_at);
        if elapsed < delta {
            return Err(PortError::Rejected(format!(
                "F7: timelock not expired ({elapsed}s of {delta}s)"
            )));
        }

        // Tokens go back to the holder. Supply never changed, so nothing to restore.
        let entry = s.balances.entry(user.to_string()).or_insert(Thb::ZERO);
        *entry = entry
            .checked_add(record.amount)
            .map_err(PortError::Domain)?;
        s.escrows.remove(&key);
        Ok(ConfirmOutcome::Confirmed)
    }

    async fn update_attestation(
        &self,
        reserve: Thb,
        encumbered: Thb,
    ) -> PortResult<ConfirmOutcome> {
        let mut s = self.lock()?;
        let now = s.now;
        s.reserve.attestation.reserve = reserve;
        // Mirror the chain: the simulator's F1 ceiling must be `reserve - encumbered`
        // too, or the simulated ledger would accept issuances the real treasury
        // rejects and the simulation would be optimistic about exactly the invariant
        // it is meant to model.
        //
        // This lives on `ReserveState`, not on `Attestation`, because
        // `ReserveState::check_issuance` already compares against `free_backing()`
        // (= reserve − encumbered). So the ceiling logic needed no change here at all;
        // the encumbrance simply had no writer until now.
        s.reserve.encumbered = encumbered;
        s.reserve.attestation.ts = now;
        Ok(ConfirmOutcome::Confirmed)
    }
}

/// Δ for the simulator. A real deployment reads this from treasury params; the
/// simulator fixes it at 24h, matching the service default.
const SIMULATED_DELTA_SECS: i64 = 86_400;

#[cfg(test)]
mod tests {
    use super::*;
    use thbc_core::bank_ref::BankRef;

    fn ledger() -> SimulatedLedger {
        SimulatedLedger::new(Thb::from_baht(1_000_000).unwrap(), 3_600, 0)
    }

    fn nullifier(s: &str) -> BankRefHash {
        BankRef::new(s).unwrap().hash()
    }

    #[tokio::test]
    async fn issuance_mints_to_the_beneficiary_and_raises_supply() {
        let l = ledger();
        let amount = Thb::from_baht(1_000).unwrap();
        l.issue("alice", amount, nullifier("REF-1")).await.unwrap();
        assert_eq!(l.balance_of("alice").unwrap(), amount);
        assert_eq!(l.supply().unwrap(), amount);
    }

    #[tokio::test]
    async fn f3_a_replayed_bank_ref_is_rejected_at_the_account_level() {
        let l = ledger();
        let amount = Thb::from_baht(1_000).unwrap();
        l.issue("alice", amount, nullifier("REF-1")).await.unwrap();

        let err = l
            .issue("alice", amount, nullifier("REF-1"))
            .await
            .unwrap_err();
        assert!(matches!(err, PortError::Rejected(_)), "got {err:?}");
        assert_eq!(l.supply().unwrap(), amount, "a replay must not move supply");
    }

    #[tokio::test]
    async fn a_reverted_issuance_leaves_no_nullifier_behind() {
        // The instruction is atomic: if the F1 check fails, the nullifier account is
        // not created either — otherwise a deposit rejected for a stale attestation
        // could never be retried after the refresh (spec §5.4).
        let l = SimulatedLedger::new(Thb::from_baht(100).unwrap(), 3_600, 0);
        let n = nullifier("REF-1");
        let too_much = Thb::from_baht(1_000).unwrap();

        assert!(l.issue("alice", too_much, n).await.is_err());
        assert!(
            !l.nullifier_exists(n).unwrap(),
            "a reverted issuance must not burn the ref"
        );

        l.issue("alice", Thb::from_baht(100).unwrap(), n)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn f5_issuance_halts_past_the_ttl_and_resumes_after_a_refresh() {
        let l = ledger();
        l.set_now(10_000).unwrap();
        let amount = Thb::from_baht(1).unwrap();
        assert!(l.issue("alice", amount, nullifier("A")).await.is_err());

        l.update_attestation(Thb::from_baht(1_000_000).unwrap(), Thb::ZERO)
            .await
            .unwrap();
        l.issue("alice", amount, nullifier("B")).await.unwrap();
    }

    /// The encumbrance is not decoration: it must tighten the ceiling the simulator
    /// enforces, or simulated runs would pass issuances the real treasury refuses.
    #[tokio::test]
    async fn f1_encumbered_fiat_does_not_back_issuance() {
        let l = ledger();
        l.set_now(10_000).unwrap();
        // 1_000 baht attested, 900 of it encumbered => 100 baht of real backing.
        l.update_attestation(
            Thb::from_baht(1_000).unwrap(),
            Thb::from_baht(900).unwrap(),
        )
        .await
        .unwrap();

        // 500 fits under the bare reserve and is refused against free backing.
        assert!(
            l.issue("alice", Thb::from_baht(500).unwrap(), nullifier("A"))
                .await
                .is_err(),
            "encumbered fiat must not back issuance"
        );
        // 100 is exactly the free backing and must be allowed.
        l.issue("alice", Thb::from_baht(100).unwrap(), nullifier("B"))
            .await
            .unwrap();
    }

    /// Re-attesting with a smaller encumbrance must widen the ceiling again — the
    /// field is last-writer-wins, not a high-water mark.
    #[tokio::test]
    async fn f1_releasing_an_encumbrance_restores_headroom() {
        let l = ledger();
        l.set_now(10_000).unwrap();
        l.update_attestation(
            Thb::from_baht(1_000).unwrap(),
            Thb::from_baht(900).unwrap(),
        )
        .await
        .unwrap();
        assert!(l
            .issue("alice", Thb::from_baht(500).unwrap(), nullifier("A"))
            .await
            .is_err());

        // KYC clears: the encumbrance is released.
        l.update_attestation(Thb::from_baht(1_000).unwrap(), Thb::ZERO)
            .await
            .unwrap();
        l.issue("alice", Thb::from_baht(500).unwrap(), nullifier("B"))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn f1_issuance_cannot_exceed_the_attested_reserve() {
        let l = SimulatedLedger::new(Thb::from_baht(100).unwrap(), 3_600, 0);
        l.issue("alice", Thb::from_baht(100).unwrap(), nullifier("A"))
            .await
            .unwrap();
        assert!(
            l.issue("alice", Thb::from_minor(1), nullifier("B"))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn f7_escrow_holds_tokens_without_burning_them() {
        let l = ledger();
        let amount = Thb::from_baht(1_000).unwrap();
        l.issue("alice", amount, nullifier("A")).await.unwrap();

        let r = Redemption::request("alice", 1, amount, 86_400, 0).unwrap();
        l.escrow_redemption(&r).await.unwrap();

        assert_eq!(
            l.balance_of("alice").unwrap(),
            Thb::ZERO,
            "tokens left the wallet"
        );
        assert_eq!(
            l.supply().unwrap(),
            amount,
            "but supply is unchanged — not yet burned"
        );
        assert_eq!(l.snapshot().await.unwrap().redemption_queue_len, Some(1));
    }

    #[tokio::test]
    async fn f7_reclaim_after_delta_restores_the_tokens() {
        let l = ledger();
        let amount = Thb::from_baht(1_000).unwrap();
        l.issue("alice", amount, nullifier("A")).await.unwrap();
        let r = Redemption::request("alice", 1, amount, 86_400, 0).unwrap();
        l.escrow_redemption(&r).await.unwrap();

        l.set_now(86_399).unwrap();
        assert!(
            l.reclaim_redemption("alice", 1).await.is_err(),
            "one second early"
        );

        l.set_now(86_400).unwrap();
        l.reclaim_redemption("alice", 1).await.unwrap();
        assert_eq!(l.balance_of("alice").unwrap(), amount);
        assert_eq!(
            l.supply().unwrap(),
            amount,
            "a reclaim never changes supply"
        );
    }

    #[tokio::test]
    async fn f7_confirm_burns_and_then_blocks_reclaim_forever() {
        let l = ledger();
        let amount = Thb::from_baht(1_000).unwrap();
        l.issue("alice", amount, nullifier("A")).await.unwrap();
        let r = Redemption::request("alice", 1, amount, 86_400, 0).unwrap();
        l.escrow_redemption(&r).await.unwrap();

        l.confirm_redemption("alice", 1).await.unwrap();
        assert_eq!(
            l.supply().unwrap(),
            Thb::ZERO,
            "confirm is where supply falls"
        );

        l.set_now(999_999).unwrap();
        assert!(l.reclaim_redemption("alice", 1).await.is_err());
        assert_eq!(l.balance_of("alice").unwrap(), Thb::ZERO);
    }

    #[tokio::test]
    async fn a_user_cannot_escrow_more_than_they_hold() {
        let l = ledger();
        l.issue("alice", Thb::from_baht(10).unwrap(), nullifier("A"))
            .await
            .unwrap();
        let r = Redemption::request("alice", 1, Thb::from_baht(11).unwrap(), 86_400, 0).unwrap();
        assert!(l.escrow_redemption(&r).await.is_err());
    }

    #[tokio::test]
    async fn confirming_twice_is_rejected() {
        let l = ledger();
        let amount = Thb::from_baht(10).unwrap();
        l.issue("alice", amount, nullifier("A")).await.unwrap();
        let r = Redemption::request("alice", 1, amount, 86_400, 0).unwrap();
        l.escrow_redemption(&r).await.unwrap();
        l.confirm_redemption("alice", 1).await.unwrap();
        assert!(l.confirm_redemption("alice", 1).await.is_err());
    }

    #[tokio::test]
    async fn funding_inventory_moves_existing_tokens_and_never_creates_them() {
        let l = ledger();
        let amount = Thb::from_baht(1_000).unwrap();
        l.issue("platform", amount, nullifier("A")).await.unwrap();
        let supply_before = l.supply().unwrap();

        l.fund_inventory("platform", amount).unwrap();
        assert_eq!(l.snapshot().await.unwrap().inventory.thbc, amount);
        assert_eq!(
            l.supply().unwrap(),
            supply_before,
            "F6: inventory is not a mint"
        );
    }

}
