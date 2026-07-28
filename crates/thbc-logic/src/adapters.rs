//! Small port implementations with no infrastructure behind them.
//!
//! Clocks, and stubs for the two adapters spec §12 lists as **not implemented**:
//! the bank adapter and the KYC adapter. They live here rather than in a dedicated
//! crate because there is nothing to them — the moment either grows a real
//! integration it should move to its own crate at the edge.

use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use thbc_core::deposit::ScreenOutcome;
use thbc_core::money::Thb;
use thbc_core::ports::{Clock, CompliancePort, PayoutPort, PortError, PortResult};
use tracing::warn;

/// Wall-clock time.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
    }
}

/// A clock you can move. Δ and TTL behaviour is tested by warping time, never by
/// sleeping — a test that sleeps for Δ is a test nobody runs.
#[derive(Debug)]
pub struct FixedClock {
    now: Mutex<i64>,
}

impl FixedClock {
    #[must_use]
    pub const fn new(now: i64) -> Self {
        Self {
            now: Mutex::new(now),
        }
    }

    /// Set the current time. Silently ignored if the lock is poisoned — this is a
    /// test helper, and a poisoned clock means the test already failed.
    pub fn set(&self, now: i64) {
        if let Ok(mut guard) = self.now.lock() {
            *guard = now;
        }
    }

    pub fn advance(&self, secs: i64) {
        if let Ok(mut guard) = self.now.lock() {
            *guard = guard.saturating_add(secs);
        }
    }
}

impl Clock for FixedClock {
    fn now(&self) -> i64 {
        self.now.lock().map_or(0, |g| *g)
    }
}

/// Compliance stub. **There is no KYC adapter** (spec §12).
///
/// Passes everything below `auto_pass_ceiling` and refuses everything above it, so
/// the encumbrance path in §5.4 is reachable in a simulation. This is a test double
/// wearing a service's name; it screens nobody. NDID is the intended primary (§9).
///
/// Deliberately not the default in any non-simulation wiring — see
/// `thbc-service`'s startup, which refuses to construct this outside simulation mode.
#[derive(Debug)]
pub struct StubCompliance {
    auto_pass_ceiling: Thb,
}

impl StubCompliance {
    #[must_use]
    pub const fn new(auto_pass_ceiling: Thb) -> Self {
        Self { auto_pass_ceiling }
    }
}

#[async_trait]
impl CompliancePort for StubCompliance {
    async fn screen(&self, subject: &str, amount: Thb) -> PortResult<ScreenOutcome> {
        warn!(
            subject,
            amount = amount.minor(),
            "STUB compliance screen — no KYC, no sanctions list, no AML scoring"
        );
        Ok(if amount > self.auto_pass_ceiling {
            ScreenOutcome::Fail
        } else {
            ScreenOutcome::Pass
        })
    }
}

/// Payout stub. **There is no fiat rail of any kind** (spec §12).
///
/// Refuses every call. A stub that silently "succeeded" would let the F4 barrier be
/// declared tested against a queue that never existed — the exact overclaim §13 warns
/// about. Failing loudly keeps the gap visible in logs and in test output.
#[derive(Debug, Default)]
pub struct UnavailablePayoutQueue;

#[async_trait]
impl PayoutPort for UnavailablePayoutQueue {
    async fn enqueue(&self, user: &str, seq: u64, amount: Thb) -> PortResult<()> {
        warn!(
            user,
            seq,
            amount = amount.minor(),
            "payout requested but no fiat rail exists (spec §12)"
        );
        Err(PortError::Unsupported(
            "no fiat rail exists — the THB payout queue is not implemented (spec §12)",
        ))
    }
}

/// Records payouts in memory instead of sending them. For simulation and for tests
/// that need to assert *what* would have been wired, and in what order.
#[derive(Debug, Default)]
pub struct RecordingPayoutQueue {
    sent: Mutex<Vec<(String, u64, Thb)>>,
}

impl RecordingPayoutQueue {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn sent(&self) -> Vec<(String, u64, Thb)> {
        self.sent.lock().map(|g| g.clone()).unwrap_or_default()
    }

    pub fn len(&self) -> usize {
        self.sent.lock().map_or(0, |g| g.len())
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[async_trait]
impl PayoutPort for RecordingPayoutQueue {
    async fn enqueue(&self, user: &str, seq: u64, amount: Thb) -> PortResult<()> {
        let mut sent = self
            .sent
            .lock()
            .map_err(|_| PortError::Transient("payout recorder is poisoned".into()))?;

        // Idempotent on (user, seq), as the port contract requires: the caller may
        // retry after a crash between the burn and this call, and a double wire is
        // unrecoverable in a way a double burn is not.
        if sent.iter().any(|(u, s, _)| u == user && *s == seq) {
            return Ok(());
        }
        sent.push((user.to_string(), seq, amount));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fixed_clock_moves_only_when_told_to() {
        let c = FixedClock::new(1_000);
        assert_eq!(c.now(), 1_000);
        c.advance(500);
        assert_eq!(c.now(), 1_500);
        c.set(0);
        assert_eq!(c.now(), 0);
    }

    #[test]
    fn the_system_clock_returns_a_plausible_unix_timestamp() {
        // 2026-07-29 is ~1.78e9. Guards against a units mix-up (millis for secs).
        assert!(SystemClock.now() > 1_700_000_000);
    }

    #[tokio::test]
    async fn the_compliance_stub_can_reach_both_outcomes() {
        let c = StubCompliance::new(Thb::from_baht(1_000).unwrap());
        assert_eq!(
            c.screen("alice", Thb::from_baht(999).unwrap())
                .await
                .unwrap(),
            ScreenOutcome::Pass
        );
        assert_eq!(
            c.screen("alice", Thb::from_baht(1_001).unwrap())
                .await
                .unwrap(),
            ScreenOutcome::Fail
        );
    }

    #[tokio::test]
    async fn the_unavailable_payout_queue_refuses_rather_than_pretending() {
        let q = UnavailablePayoutQueue;
        let e = q
            .enqueue("alice", 1, Thb::from_baht(10).unwrap())
            .await
            .unwrap_err();
        assert!(matches!(e, PortError::Unsupported(_)), "got {e:?}");
    }

    #[tokio::test]
    async fn the_recording_queue_is_idempotent_on_user_and_seq() {
        let q = RecordingPayoutQueue::new();
        let amount = Thb::from_baht(10).unwrap();
        q.enqueue("alice", 1, amount).await.unwrap();
        q.enqueue("alice", 1, amount).await.unwrap();
        assert_eq!(q.len(), 1, "a retry must not produce a second wire");

        q.enqueue("alice", 2, amount).await.unwrap();
        assert_eq!(q.len(), 2, "a distinct redemption is a distinct wire");
    }
}
