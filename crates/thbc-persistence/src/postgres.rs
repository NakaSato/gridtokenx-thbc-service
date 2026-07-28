//! Postgres repositories.
//!
//! ## Why runtime queries and not `sqlx::query_as!`
//!
//! The repo convention is the compile-time-checked macro. It is not used here,
//! deliberately: `query_as!` needs either a live `DATABASE_URL` at build time or a
//! committed `.sqlx` offline cache, and this service has neither yet — the database
//! it describes has never been provisioned. With the macro, `cargo check` on a fresh
//! clone fails before a single test runs.
//!
//! This is a temporary deviation, not a preference. When the DB exists, run
//! `cargo sqlx prepare` and migrate these to the macro form; the SQL is already
//! written to make that mechanical.
//!
//! ## Money in Postgres
//!
//! Amounts are `BIGINT` minor units, never `NUMERIC` or `DOUBLE PRECISION`. Postgres
//! `BIGINT` is signed and `Thb` is `u64`, so a value above `i64::MAX` cannot round
//! trip. That bound is ~9.2e18 minor units — about 9.2 trillion baht, comfortably
//! above any real reserve — and it is checked on write rather than assumed.

use async_trait::async_trait;
use sqlx::{PgPool, Row};
use thbc_core::bank_ref::{BankRef, BankRefHash};
use thbc_core::deposit::{Deposit, DepositState};
use thbc_core::money::Thb;
use thbc_core::ports::{DepositRepository, PortError, PortResult, RedemptionRepository};
use thbc_core::redemption::{Redemption, RedemptionState};

/// Postgres `BIGINT` is signed; `Thb` is not. Refuse rather than wrap.
fn to_i64(amount: Thb) -> PortResult<i64> {
    i64::try_from(amount.minor())
        .map_err(|_| PortError::Rejected(format!("amount {} exceeds BIGINT range", amount.minor())))
}

fn from_i64(v: i64) -> PortResult<Thb> {
    u64::try_from(v)
        .map(Thb::from_minor)
        .map_err(|_| PortError::Rejected(format!("negative amount {v} in database")))
}

fn parse_deposit_state(s: &str) -> PortResult<DepositState> {
    Ok(match s {
        "observed" => DepositState::Observed,
        "screened" => DepositState::Screened,
        "attested" => DepositState::Attested,
        "issued" => DepositState::Issued,
        "encumbered" => DepositState::Encumbered,
        "disputed" => DepositState::Disputed,
        other => {
            return Err(PortError::Rejected(format!(
                "unknown deposit state {other:?}"
            )));
        }
    })
}

fn parse_redemption_state(s: &str) -> PortResult<RedemptionState> {
    Ok(match s {
        "requested" => RedemptionState::Requested,
        "escrowed" => RedemptionState::Escrowed,
        "payout_queued" => RedemptionState::PayoutQueued,
        "confirmed" => RedemptionState::Confirmed,
        "reclaimed" => RedemptionState::Reclaimed,
        "failed" => RedemptionState::Failed,
        other => {
            return Err(PortError::Rejected(format!(
                "unknown redemption state {other:?}"
            )));
        }
    })
}

fn map_sqlx(e: sqlx::Error) -> PortError {
    match e {
        // 23505 = unique_violation. On `deposits.bank_ref_hash` this is off-chain F3
        // firing, and the caller must be able to tell it from a generic failure.
        sqlx::Error::Database(db) if db.code().as_deref() == Some("23505") => {
            PortError::Conflict(db.message().to_string())
        }
        sqlx::Error::RowNotFound => PortError::NotFound,
        other => PortError::Transient(other.to_string()),
    }
}

// ---------------------------------------------------------------------------
// Deposits
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct PgDepositRepository {
    pool: PgPool,
}

impl PgDepositRepository {
    #[must_use]
    pub const fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl DepositRepository for PgDepositRepository {
    async fn insert(&self, deposit: &Deposit) -> PortResult<()> {
        // No ON CONFLICT DO NOTHING: a duplicate must surface as a conflict, not be
        // swallowed. Off-chain F3 is only useful if the caller hears about it.
        sqlx::query(
            "INSERT INTO deposits
               (bank_ref_hash, bank_ref, amount_minor, beneficiary, state, observed_at)
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(deposit.nullifier().to_hex())
        .bind(deposit.bank_ref.as_str())
        .bind(to_i64(deposit.amount)?)
        .bind(&deposit.beneficiary)
        .bind(deposit.state.as_str())
        .bind(deposit.observed_at)
        .execute(&self.pool)
        .await
        .map_err(map_sqlx)?;
        Ok(())
    }

    async fn find(&self, nullifier: BankRefHash) -> PortResult<Option<Deposit>> {
        let row = sqlx::query(
            "SELECT bank_ref, amount_minor, beneficiary, state, observed_at
             FROM deposits WHERE bank_ref_hash = $1",
        )
        .bind(nullifier.to_hex())
        .fetch_optional(&self.pool)
        .await
        .map_err(map_sqlx)?;

        let Some(row) = row else { return Ok(None) };
        let bank_ref: String = row.try_get("bank_ref").map_err(map_sqlx)?;
        let amount: i64 = row.try_get("amount_minor").map_err(map_sqlx)?;
        let state: String = row.try_get("state").map_err(map_sqlx)?;

        Ok(Some(Deposit {
            bank_ref: BankRef::new(bank_ref).map_err(PortError::Domain)?,
            amount: from_i64(amount)?,
            beneficiary: row.try_get("beneficiary").map_err(map_sqlx)?,
            state: parse_deposit_state(&state)?,
            observed_at: row.try_get("observed_at").map_err(map_sqlx)?,
        }))
    }

    async fn update(&self, deposit: &Deposit) -> PortResult<()> {
        // `bank_ref_hash` is never updated — it is the identity, and changing it
        // would orphan the nullifier.
        let result = sqlx::query(
            "UPDATE deposits SET amount_minor = $2, state = $3, updated_at = NOW()
             WHERE bank_ref_hash = $1",
        )
        .bind(deposit.nullifier().to_hex())
        .bind(to_i64(deposit.amount)?)
        .bind(deposit.state.as_str())
        .execute(&self.pool)
        .await
        .map_err(map_sqlx)?;

        if result.rows_affected() == 0 {
            return Err(PortError::NotFound);
        }
        Ok(())
    }

    async fn total_encumbered(&self) -> PortResult<Thb> {
        // Must match `Deposit::is_encumbering`. Both states hold cleared fiat that
        // backs no token.
        let row = sqlx::query(
            "SELECT COALESCE(SUM(amount_minor), 0)::BIGINT AS total
             FROM deposits WHERE state IN ('encumbered', 'disputed')",
        )
        .fetch_one(&self.pool)
        .await
        .map_err(map_sqlx)?;
        from_i64(row.try_get("total").map_err(map_sqlx)?)
    }

    async fn total_issued(&self) -> PortResult<Thb> {
        let row = sqlx::query(
            "SELECT COALESCE(SUM(amount_minor), 0)::BIGINT AS total
             FROM deposits WHERE state = 'issued'",
        )
        .fetch_one(&self.pool)
        .await
        .map_err(map_sqlx)?;
        from_i64(row.try_get("total").map_err(map_sqlx)?)
    }
}

// ---------------------------------------------------------------------------
// Redemptions
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct PgRedemptionRepository {
    pool: PgPool,
}

impl PgRedemptionRepository {
    #[must_use]
    pub const fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    fn row_to_redemption(row: &sqlx::postgres::PgRow) -> PortResult<Redemption> {
        let seq: i64 = row.try_get("seq").map_err(map_sqlx)?;
        let amount: i64 = row.try_get("amount_minor").map_err(map_sqlx)?;
        let state: String = row.try_get("state").map_err(map_sqlx)?;
        Ok(Redemption {
            user: row.try_get("user_id").map_err(map_sqlx)?,
            seq: u64::try_from(seq)
                .map_err(|_| PortError::Rejected(format!("negative seq {seq}")))?,
            amount: from_i64(amount)?,
            state: parse_redemption_state(&state)?,
            requested_at: row.try_get("requested_at").map_err(map_sqlx)?,
            escrowed_at: row.try_get("escrowed_at").map_err(map_sqlx)?,
            delta_secs: row.try_get("delta_secs").map_err(map_sqlx)?,
        })
    }
}

#[async_trait]
impl RedemptionRepository for PgRedemptionRepository {
    async fn insert(&self, redemption: &Redemption) -> PortResult<()> {
        let seq = i64::try_from(redemption.seq)
            .map_err(|_| PortError::Rejected(format!("seq {} exceeds BIGINT", redemption.seq)))?;
        sqlx::query(
            "INSERT INTO redemptions
               (user_id, seq, amount_minor, state, requested_at, escrowed_at, delta_secs)
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(&redemption.user)
        .bind(seq)
        .bind(to_i64(redemption.amount)?)
        .bind(redemption.state.as_str())
        .bind(redemption.requested_at)
        .bind(redemption.escrowed_at)
        .bind(redemption.delta_secs)
        .execute(&self.pool)
        .await
        .map_err(map_sqlx)?;
        Ok(())
    }

    async fn find(&self, user: &str, seq: u64) -> PortResult<Option<Redemption>> {
        let seq = i64::try_from(seq).map_err(|_| PortError::NotFound)?;
        let row = sqlx::query(
            "SELECT user_id, seq, amount_minor, state, requested_at, escrowed_at, delta_secs
             FROM redemptions WHERE user_id = $1 AND seq = $2",
        )
        .bind(user)
        .bind(seq)
        .fetch_optional(&self.pool)
        .await
        .map_err(map_sqlx)?;

        row.as_ref().map(Self::row_to_redemption).transpose()
    }

    async fn update(&self, redemption: &Redemption) -> PortResult<()> {
        let seq = i64::try_from(redemption.seq).map_err(|_| PortError::NotFound)?;
        let result = sqlx::query(
            "UPDATE redemptions SET state = $3, escrowed_at = $4, updated_at = NOW()
             WHERE user_id = $1 AND seq = $2",
        )
        .bind(&redemption.user)
        .bind(seq)
        .bind(redemption.state.as_str())
        .bind(redemption.escrowed_at)
        .execute(&self.pool)
        .await
        .map_err(map_sqlx)?;

        if result.rows_affected() == 0 {
            return Err(PortError::NotFound);
        }
        Ok(())
    }

    async fn next_seq(&self, user: &str) -> PortResult<u64> {
        // Racy under concurrency: two requests for the same user can read the same
        // max. The UNIQUE (user_id, seq) index turns that into a conflict on insert
        // rather than a duplicated record, and the caller retries. A sequence per
        // user would avoid the retry but not the need for the index.
        let row = sqlx::query(
            "SELECT COALESCE(MAX(seq), 0)::BIGINT AS max_seq FROM redemptions WHERE user_id = $1",
        )
        .bind(user)
        .fetch_one(&self.pool)
        .await
        .map_err(map_sqlx)?;
        let max: i64 = row.try_get("max_seq").map_err(map_sqlx)?;
        Ok(u64::try_from(max).unwrap_or(0) + 1)
    }

    async fn reclaimable(&self, now: i64) -> PortResult<Vec<Redemption>> {
        let rows = sqlx::query(
            "SELECT user_id, seq, amount_minor, state, requested_at, escrowed_at, delta_secs
             FROM redemptions
             WHERE state IN ('escrowed', 'payout_queued')
               AND escrowed_at IS NOT NULL
               AND ($1 - escrowed_at) >= delta_secs
             ORDER BY escrowed_at ASC",
        )
        .bind(now)
        .fetch_all(&self.pool)
        .await
        .map_err(map_sqlx)?;

        rows.iter().map(Self::row_to_redemption).collect()
    }

    async fn total_redeemed(&self) -> PortResult<Thb> {
        // Only 'confirmed'. A reclaimed redemption returned its tokens and never
        // reduced supply — including it here would manufacture F2 drift that looks
        // identical to an unbacked mint.
        let row = sqlx::query(
            "SELECT COALESCE(SUM(amount_minor), 0)::BIGINT AS total
             FROM redemptions WHERE state = 'confirmed'",
        )
        .fetch_one(&self.pool)
        .await
        .map_err(map_sqlx)?;
        from_i64(row.try_get("total").map_err(map_sqlx)?)
    }

    async fn pending_count(&self) -> PortResult<u32> {
        let row = sqlx::query(
            "SELECT COUNT(*)::BIGINT AS n FROM redemptions
             WHERE state IN ('escrowed', 'payout_queued')",
        )
        .fetch_one(&self.pool)
        .await
        .map_err(map_sqlx)?;
        let n: i64 = row.try_get("n").map_err(map_sqlx)?;
        Ok(u32::try_from(n).unwrap_or(u32::MAX))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Round-trip and range checks run without a database. Query behaviour is
    // exercised by the integration tests, which need a real Postgres.

    #[test]
    fn deposit_states_round_trip_through_their_wire_strings() {
        for s in [
            DepositState::Observed,
            DepositState::Screened,
            DepositState::Attested,
            DepositState::Issued,
            DepositState::Encumbered,
            DepositState::Disputed,
        ] {
            assert_eq!(parse_deposit_state(s.as_str()).unwrap(), s);
        }
    }

    #[test]
    fn redemption_states_round_trip_through_their_wire_strings() {
        for s in [
            RedemptionState::Requested,
            RedemptionState::Escrowed,
            RedemptionState::PayoutQueued,
            RedemptionState::Confirmed,
            RedemptionState::Reclaimed,
            RedemptionState::Failed,
        ] {
            assert_eq!(parse_redemption_state(s.as_str()).unwrap(), s);
        }
    }

    #[test]
    fn an_unknown_state_string_is_rejected_not_defaulted() {
        // Defaulting an unrecognised state would silently resurrect a terminal
        // redemption as `Requested`.
        assert!(parse_deposit_state("wat").is_err());
        assert!(parse_redemption_state("wat").is_err());
    }

    #[test]
    fn amounts_above_bigint_range_are_refused() {
        assert!(to_i64(Thb::from_minor(u64::MAX)).is_err());
        assert_eq!(to_i64(Thb::from_minor(i64::MAX as u64)).unwrap(), i64::MAX);
    }

    #[test]
    fn a_negative_amount_from_the_database_is_refused() {
        // Would mean a corrupt row or a bad migration; reading it as a huge u64 is
        // the worst possible interpretation.
        assert!(from_i64(-1).is_err());
        assert_eq!(from_i64(0).unwrap(), Thb::ZERO);
    }
}
