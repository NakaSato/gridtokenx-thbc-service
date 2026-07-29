//! In-memory repositories.
//!
//! Used by the simulation mode and by tests that need the logic layer without a
//! database. They implement the same ports as the Postgres repositories, including
//! the uniqueness behaviour F3 depends on — an in-memory repo that quietly allowed a
//! duplicate nullifier would make the F3 tests pass for the wrong reason.

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use thbc_core::bank_ref::BankRefHash;
use thbc_core::deposit::{Deposit, DepositState};
use thbc_core::money::Thb;
use thbc_core::ports::{
    DepositRepository, PortError, PortResult, ReconciliationRepository, RedemptionRepository,
};
use thbc_core::reconcile::ReconciliationReport;
use thbc_core::redemption::Redemption;

fn poisoned() -> PortError {
    PortError::Transient("in-memory repository lock is poisoned".into())
}

#[derive(Debug, Default)]
pub struct InMemoryDepositRepo {
    rows: Mutex<HashMap<BankRefHash, Deposit>>,
}

impl InMemoryDepositRepo {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> PortResult<usize> {
        Ok(self.rows.lock().map_err(|_| poisoned())?.len())
    }

    pub fn is_empty(&self) -> PortResult<bool> {
        Ok(self.len()? == 0)
    }
}

#[async_trait]
impl DepositRepository for InMemoryDepositRepo {
    async fn insert(&self, deposit: &Deposit) -> PortResult<()> {
        let mut rows = self.rows.lock().map_err(|_| poisoned())?;
        let key = deposit.nullifier();
        if rows.contains_key(&key) {
            // Mirrors the Postgres UNIQUE violation. Off-chain F3.
            return Err(PortError::Conflict(format!(
                "bank_ref_hash {key} already recorded"
            )));
        }
        rows.insert(key, deposit.clone());
        Ok(())
    }

    async fn find(&self, nullifier: BankRefHash) -> PortResult<Option<Deposit>> {
        Ok(self
            .rows
            .lock()
            .map_err(|_| poisoned())?
            .get(&nullifier)
            .cloned())
    }

    async fn update(&self, deposit: &Deposit) -> PortResult<()> {
        let mut rows = self.rows.lock().map_err(|_| poisoned())?;
        let key = deposit.nullifier();
        if !rows.contains_key(&key) {
            return Err(PortError::NotFound);
        }
        rows.insert(key, deposit.clone());
        Ok(())
    }

    async fn total_encumbered(&self) -> PortResult<Thb> {
        let rows = self.rows.lock().map_err(|_| poisoned())?;
        rows.values()
            .filter(|d| d.is_encumbering())
            .try_fold(Thb::ZERO, |acc, d| acc.checked_add(d.amount))
            .map_err(PortError::Domain)
    }

    async fn held(&self) -> PortResult<Vec<Deposit>> {
        let rows = self.rows.lock().map_err(|_| poisoned())?;
        Ok(rows
            .values()
            .filter(|d| d.state == DepositState::Screened)
            .cloned()
            .collect())
    }

    async fn total_issued(&self) -> PortResult<Thb> {
        let rows = self.rows.lock().map_err(|_| poisoned())?;
        rows.values()
            .filter(|d| d.state == DepositState::Issued)
            .try_fold(Thb::ZERO, |acc, d| acc.checked_add(d.amount))
            .map_err(PortError::Domain)
    }
}

#[derive(Debug, Default)]
pub struct InMemoryRedemptionRepo {
    rows: Mutex<HashMap<(String, u64), Redemption>>,
}

impl InMemoryRedemptionRepo {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl RedemptionRepository for InMemoryRedemptionRepo {
    async fn insert(&self, redemption: &Redemption) -> PortResult<()> {
        let mut rows = self.rows.lock().map_err(|_| poisoned())?;
        let key = (redemption.user.clone(), redemption.seq);
        if rows.contains_key(&key) {
            return Err(PortError::Conflict(format!(
                "redemption ({}, {}) already recorded",
                redemption.user, redemption.seq
            )));
        }
        rows.insert(key, redemption.clone());
        Ok(())
    }

    async fn find(&self, user: &str, seq: u64) -> PortResult<Option<Redemption>> {
        Ok(self
            .rows
            .lock()
            .map_err(|_| poisoned())?
            .get(&(user.to_string(), seq))
            .cloned())
    }

    async fn update(&self, redemption: &Redemption) -> PortResult<()> {
        let mut rows = self.rows.lock().map_err(|_| poisoned())?;
        let key = (redemption.user.clone(), redemption.seq);
        if !rows.contains_key(&key) {
            return Err(PortError::NotFound);
        }
        rows.insert(key, redemption.clone());
        Ok(())
    }

    async fn next_seq(&self, user: &str) -> PortResult<u64> {
        let rows = self.rows.lock().map_err(|_| poisoned())?;
        Ok(rows
            .keys()
            .filter(|(u, _)| u == user)
            .map(|(_, s)| *s)
            .max()
            .map_or(1, |m| m + 1))
    }

    async fn reclaimable(&self, now: i64) -> PortResult<Vec<Redemption>> {
        let rows = self.rows.lock().map_err(|_| poisoned())?;
        Ok(rows
            .values()
            .filter(|r| r.is_reclaimable(now))
            .cloned()
            .collect())
    }

    async fn total_redeemed(&self) -> PortResult<Thb> {
        let rows = self.rows.lock().map_err(|_| poisoned())?;
        // `supply_reduction` is zero for everything but `Confirmed`, so a reclaimed
        // redemption contributes nothing. Filtering on state here instead would be a
        // second place to get that rule wrong.
        rows.values()
            .try_fold(Thb::ZERO, |acc, r| acc.checked_add(r.supply_reduction()))
            .map_err(PortError::Domain)
    }

    async fn pending_count(&self) -> PortResult<u32> {
        let rows = self.rows.lock().map_err(|_| poisoned())?;
        Ok(
            u32::try_from(rows.values().filter(|r| r.state.is_pending()).count())
                .unwrap_or(u32::MAX),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use thbc_core::bank_ref::BankRef;
    use thbc_core::deposit::ScreenOutcome;
    use thbc_core::redemption::ConfirmOutcome;

    fn deposit(reference: &str, baht: u64) -> Deposit {
        Deposit::observe(
            BankRef::new(reference).unwrap(),
            Thb::from_baht(baht).unwrap(),
            "alice",
            "Wa11etAlice",
            0,
        )
        .unwrap()
    }

    #[tokio::test]
    async fn a_duplicate_nullifier_conflicts() {
        let repo = InMemoryDepositRepo::new();
        repo.insert(&deposit("REF-1", 100)).await.unwrap();
        assert!(matches!(
            repo.insert(&deposit("REF-1", 100)).await,
            Err(PortError::Conflict(_))
        ));
        assert_eq!(repo.len().unwrap(), 1);
    }

    #[tokio::test]
    async fn a_normalised_variant_of_the_same_ref_also_conflicts() {
        let repo = InMemoryDepositRepo::new();
        repo.insert(&deposit("REF-1", 100)).await.unwrap();
        assert!(matches!(
            repo.insert(&deposit("  ref-1 ", 100)).await,
            Err(PortError::Conflict(_))
        ));
    }

    #[tokio::test]
    async fn totals_count_only_the_states_they_claim_to() {
        let repo = InMemoryDepositRepo::new();

        let mut issued = deposit("A", 100);
        issued.screen(ScreenOutcome::Pass).unwrap();
        issued.mark_attested().unwrap();
        issued.mark_issued().unwrap();
        repo.insert(&issued).await.unwrap();

        let mut failed = deposit("B", 50);
        failed.screen(ScreenOutcome::Fail).unwrap();
        repo.insert(&failed).await.unwrap();

        // Observed but not screened — neither issued nor encumbering.
        repo.insert(&deposit("C", 25)).await.unwrap();

        assert_eq!(
            repo.total_issued().await.unwrap(),
            Thb::from_baht(100).unwrap()
        );
        assert_eq!(
            repo.total_encumbered().await.unwrap(),
            Thb::from_baht(50).unwrap()
        );
    }

    #[tokio::test]
    async fn updating_an_unknown_deposit_is_not_found() {
        let repo = InMemoryDepositRepo::new();
        assert!(matches!(
            repo.update(&deposit("A", 1)).await,
            Err(PortError::NotFound)
        ));
    }

    #[tokio::test]
    async fn sequence_numbers_are_per_user_and_start_at_one() {
        let repo = InMemoryRedemptionRepo::new();
        assert_eq!(repo.next_seq("alice").await.unwrap(), 1);

        let r = Redemption::request("alice", 1, Thb::from_baht(10).unwrap(), 100, 0).unwrap();
        repo.insert(&r).await.unwrap();
        assert_eq!(repo.next_seq("alice").await.unwrap(), 2);
        assert_eq!(
            repo.next_seq("bob").await.unwrap(),
            1,
            "sequences do not leak across users"
        );
    }

    #[tokio::test]
    async fn a_reclaimed_redemption_is_not_counted_as_redeemed() {
        // F2 depends on this: counting a reclaim would look like an unbacked mint.
        let repo = InMemoryRedemptionRepo::new();
        let amount = Thb::from_baht(10).unwrap();

        let mut confirmed = Redemption::request("alice", 1, amount, 100, 0).unwrap();
        confirmed
            .apply_escrow_outcome(ConfirmOutcome::Confirmed, 0)
            .unwrap();
        confirmed.confirm().unwrap();
        repo.insert(&confirmed).await.unwrap();

        let mut reclaimed = Redemption::request("alice", 2, amount, 100, 0).unwrap();
        reclaimed
            .apply_escrow_outcome(ConfirmOutcome::Confirmed, 0)
            .unwrap();
        reclaimed.reclaim(100).unwrap();
        repo.insert(&reclaimed).await.unwrap();

        assert_eq!(
            repo.total_redeemed().await.unwrap(),
            amount,
            "only the confirmed one"
        );
    }

    #[tokio::test]
    async fn only_escrowed_and_queued_redemptions_are_reclaimable_and_pending() {
        let repo = InMemoryRedemptionRepo::new();
        let amount = Thb::from_baht(10).unwrap();

        let unconfirmed = Redemption::request("alice", 1, amount, 100, 0).unwrap();
        repo.insert(&unconfirmed).await.unwrap();

        let mut escrowed = Redemption::request("alice", 2, amount, 100, 0).unwrap();
        escrowed
            .apply_escrow_outcome(ConfirmOutcome::Confirmed, 0)
            .unwrap();
        repo.insert(&escrowed).await.unwrap();

        assert_eq!(repo.pending_count().await.unwrap(), 1);
        assert!(
            repo.reclaimable(50).await.unwrap().is_empty(),
            "inside delta"
        );

        let due = repo.reclaimable(100).await.unwrap();
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].seq, 2);
    }
}

/// In-memory reconciliation history. Append-only, like its Postgres counterpart —
/// there is deliberately no way to remove a run, because the point of the record is
/// that a drift which was later resolved stays visible.
#[derive(Debug, Default)]
pub struct InMemoryReconciliationRepo {
    runs: Mutex<Vec<ReconciliationReport>>,
}

impl InMemoryReconciliationRepo {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> PortResult<usize> {
        Ok(self.runs.lock().map_err(|_| poisoned())?.len())
    }

    pub fn is_empty(&self) -> PortResult<bool> {
        Ok(self.len()? == 0)
    }
}

#[async_trait]
impl ReconciliationRepository for InMemoryReconciliationRepo {
    async fn record(&self, report: &ReconciliationReport) -> PortResult<()> {
        self.runs.lock().map_err(|_| poisoned())?.push(*report);
        Ok(())
    }

    async fn recent(&self, limit: u32) -> PortResult<Vec<ReconciliationReport>> {
        let runs = self.runs.lock().map_err(|_| poisoned())?;
        Ok(runs.iter().rev().take(limit as usize).copied().collect())
    }

    async fn unhealthy_count(&self) -> PortResult<u64> {
        let runs = self.runs.lock().map_err(|_| poisoned())?;
        Ok(runs.iter().filter(|r| !r.is_healthy()).count() as u64)
    }
}
