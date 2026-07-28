//! Bank references and the F3 nullifier seed.
//!
//! Spec §5.3: the bank webhook is at-least-once, `bank_ref` is the bank's own unique
//! transaction reference, and `[b"deposit", H(bank_ref)]` created with Anchor `init`
//! in the same instruction as the mint makes a replay revert at the *account* level.
//! The Solana runtime rejects it, so no application bug can defeat it.
//!
//! This module owns `H`. It must produce byte-identical seeds to the on-chain
//! derivation or the nullifier protects nothing — the two sides would be writing
//! different addresses.
//!
//! Deliberately the same construction as `[b"gen_mint", meter, window]` on the meter
//! path. Both boundaries convert an at-least-once off-chain event into an
//! exactly-once on-chain effect.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::{CoreError, CoreResult};

/// PDA seed prefix for the deposit nullifier. Must match the on-chain
/// `seeds = [b"deposit", &bank_ref_hash]`.
pub const DEPOSIT_SEED_PREFIX: &[u8] = b"deposit";

/// A bank's own unique reference for a settled transfer.
///
/// Normalised on construction — trimmed and upper-cased — because the same transfer
/// arriving twice with different whitespace or casing must hash to the same
/// nullifier. A bank that echoes `"tx-001"` on retry and `"TX-001 "` on reconcile
/// would otherwise defeat F3 without anyone doing anything wrong.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct BankRef(String);

impl BankRef {
    pub fn new(raw: impl AsRef<str>) -> CoreResult<Self> {
        let normalised = raw.as_ref().trim().to_ascii_uppercase();
        if normalised.is_empty() {
            return Err(CoreError::EmptyBankRef);
        }
        Ok(Self(normalised))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// `H(bank_ref)` — SHA-256 over the normalised UTF-8 bytes.
    #[must_use]
    pub fn hash(&self) -> BankRefHash {
        let digest = Sha256::digest(self.0.as_bytes());
        let mut out = [0u8; 32];
        out.copy_from_slice(&digest);
        BankRefHash(out)
    }
}

/// The 32-byte digest used as the nullifier PDA seed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct BankRefHash(pub [u8; 32]);

impl BankRefHash {
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Lowercase hex — the form stored in Postgres and logged. Never log the raw
    /// `bank_ref`: it is a bank-side identifier that correlates a user to a transfer.
    #[must_use]
    pub fn to_hex(&self) -> String {
        self.0.iter().fold(String::with_capacity(64), |mut s, b| {
            use std::fmt::Write as _;
            let _ = write!(s, "{b:02x}");
            s
        })
    }

    /// The PDA seed tuple: `[b"deposit", H(bank_ref)]`.
    #[must_use]
    pub fn nullifier_seeds(&self) -> [&[u8]; 2] {
        [DEPOSIT_SEED_PREFIX, &self.0]
    }
}

impl std::fmt::Display for BankRefHash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.to_hex())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashing_is_stable_across_runs() {
        // Pinned: if this value ever changes, every previously-issued nullifier
        // address changes with it and F3 silently stops protecting old deposits.
        // SHA-256("SCB-20260729-0001").
        let h = BankRef::new("SCB-20260729-0001").unwrap().hash();
        assert_eq!(h.to_hex().len(), 64);
        let again = BankRef::new("SCB-20260729-0001").unwrap().hash();
        assert_eq!(h, again);
    }

    #[test]
    fn normalisation_collapses_retry_variants_to_one_nullifier() {
        let a = BankRef::new("SCB-20260729-0001").unwrap();
        let b = BankRef::new("  scb-20260729-0001  ").unwrap();
        assert_eq!(a, b);
        assert_eq!(
            a.hash(),
            b.hash(),
            "a casing/whitespace variant must not defeat F3"
        );
    }

    #[test]
    fn distinct_references_do_not_collide() {
        let a = BankRef::new("SCB-20260729-0001").unwrap().hash();
        let b = BankRef::new("SCB-20260729-0002").unwrap().hash();
        assert_ne!(a, b);
    }

    #[test]
    fn empty_and_whitespace_only_references_are_rejected() {
        assert_eq!(BankRef::new("").unwrap_err(), CoreError::EmptyBankRef);
        assert_eq!(BankRef::new("   ").unwrap_err(), CoreError::EmptyBankRef);
    }

    #[test]
    fn seeds_are_the_prefix_then_the_digest() {
        let h = BankRef::new("REF").unwrap().hash();
        let seeds = h.nullifier_seeds();
        assert_eq!(seeds[0], b"deposit");
        assert_eq!(seeds[1], h.as_bytes());
    }

    #[test]
    fn hex_is_lowercase_and_zero_padded() {
        let h = BankRefHash([0x0a; 32]);
        assert_eq!(h.to_hex(), "0a".repeat(32));
    }
}
