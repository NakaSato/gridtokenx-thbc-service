//! Money types for the payment leg.
//!
//! Two units, both integer minor units, never floats. A baht amount that has been
//! through an `f64` is not a baht amount you can reconcile against a bank statement,
//! and F2 (`Σ issued − Σ redeemed = supply`) is an exact identity — it does not
//! survive rounding.

use serde::{Deserialize, Serialize};

use crate::error::{CoreError, CoreResult};

/// THBC has 6 decimals, matching `THBC_DECIMALS` in
/// `gridtokenx-anchor/programs/treasury/src/state.rs:11`.
pub const THBC_DECIMALS: u32 = 6;

/// Minor units per whole baht: 1 THB = `1_000_000` THBC minor units.
pub const THB_MINOR_PER_BAHT: u64 = 1_000_000;

/// GRX atoms per whole GRX (9 decimals). Mirrors `GRX_ATOMS_PER_WHOLE` in
/// `gridtokenx-anchor/programs/treasury/src/lib.rs:41`.
pub const GRX_ATOMS_PER_WHOLE: u128 = 1_000_000_000;

/// A Thai baht amount in THBC minor units.
///
/// The same type carries fiat baht sitting in the reserve account and THBC minor
/// units on the ledger. That is deliberate: F1 (`supply ≤ attested_reserve`) compares
/// the two directly, and giving them separate types would only mean converting at
/// every comparison — the conversion is the bug surface, not the safety.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct Thb(u64);

impl Thb {
    pub const ZERO: Self = Self(0);

    #[must_use]
    pub const fn from_minor(minor: u64) -> Self {
        Self(minor)
    }

    /// Whole baht → minor units. Errors rather than wrapping on absurd inputs.
    pub fn from_baht(baht: u64) -> CoreResult<Self> {
        baht.checked_mul(THB_MINOR_PER_BAHT)
            .map(Self)
            .ok_or(CoreError::MathOverflow)
    }

    #[must_use]
    pub const fn minor(self) -> u64 {
        self.0
    }

    #[must_use]
    pub const fn is_zero(self) -> bool {
        self.0 == 0
    }

    pub fn checked_add(self, rhs: Self) -> CoreResult<Self> {
        self.0
            .checked_add(rhs.0)
            .map(Self)
            .ok_or(CoreError::MathOverflow)
    }

    /// Subtraction that refuses to go negative. Every caller in this crate is
    /// subtracting a liability from a backing figure, where an underflow is a
    /// solvency error, not a value to saturate.
    pub fn checked_sub(self, rhs: Self) -> CoreResult<Self> {
        self.0
            .checked_sub(rhs.0)
            .map(Self)
            .ok_or(CoreError::Underflow)
    }

    /// Saturating subtraction, for *reporting* a shortfall where the negative case
    /// is the thing being measured (e.g. how far a reserve is under water).
    #[must_use]
    pub const fn saturating_sub(self, rhs: Self) -> Self {
        Self(self.0.saturating_sub(rhs.0))
    }

    /// Basis-point fee on this amount, truncating toward zero — the same rounding
    /// as the on-chain `compute_swap_grx_for_thbc`
    /// (`programs/treasury/src/lib.rs:78`), so an off-chain quote and the on-chain
    /// execution agree to the minor unit.
    pub fn fee_bps(self, bps: u16) -> CoreResult<Self> {
        let fee = u128::from(self.0)
            .checked_mul(u128::from(bps))
            .ok_or(CoreError::MathOverflow)?
            / 10_000;
        u64::try_from(fee)
            .map(Self)
            .map_err(|_| CoreError::MathOverflow)
    }
}

impl std::fmt::Display for Thb {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let whole = self.0 / THB_MINOR_PER_BAHT;
        let frac = self.0 % THB_MINOR_PER_BAHT;
        write!(f, "{whole}.{frac:06} THB")
    }
}

/// A GRX amount in atoms (9 decimals).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct Grx(u64);

impl Grx {
    pub const ZERO: Self = Self(0);

    #[must_use]
    pub const fn from_atoms(atoms: u64) -> Self {
        Self(atoms)
    }

    #[must_use]
    pub const fn atoms(self) -> u64 {
        self.0
    }

    #[must_use]
    pub const fn is_zero(self) -> bool {
        self.0 == 0
    }

    pub fn checked_add(self, rhs: Self) -> CoreResult<Self> {
        self.0
            .checked_add(rhs.0)
            .map(Self)
            .ok_or(CoreError::MathOverflow)
    }

    pub fn checked_sub(self, rhs: Self) -> CoreResult<Self> {
        self.0
            .checked_sub(rhs.0)
            .map(Self)
            .ok_or(CoreError::Underflow)
    }
}

/// `GRX_ATOMS_PER_WHOLE` as `u64`. It is `u128` because the exchange math needs the
/// wider type, but the value (1e9) fits `u64` with room to spare.
const GRX_ATOMS_PER_WHOLE_U64: u64 = 1_000_000_000;

impl std::fmt::Display for Grx {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}.{:09} GRX",
            self.0 / GRX_ATOMS_PER_WHOLE_U64,
            self.0 % GRX_ATOMS_PER_WHOLE_U64
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_two_grx_scale_constants_agree() {
        // They are duplicated only because one call site needs u128 and another u64.
        assert_eq!(u128::from(GRX_ATOMS_PER_WHOLE_U64), GRX_ATOMS_PER_WHOLE);
    }

    #[test]
    fn baht_converts_to_minor_units() {
        assert_eq!(Thb::from_baht(1).unwrap().minor(), 1_000_000);
        assert_eq!(Thb::from_baht(0).unwrap(), Thb::ZERO);
    }

    #[test]
    fn baht_conversion_overflow_is_an_error_not_a_wrap() {
        assert!(matches!(
            Thb::from_baht(u64::MAX),
            Err(CoreError::MathOverflow)
        ));
    }

    #[test]
    fn checked_sub_refuses_to_go_negative() {
        let a = Thb::from_minor(10);
        let b = Thb::from_minor(11);
        assert!(matches!(a.checked_sub(b), Err(CoreError::Underflow)));
        assert_eq!(a.saturating_sub(b), Thb::ZERO);
    }

    #[test]
    fn fee_truncates_toward_zero_like_the_program() {
        // 25 bps on 12_000_000 = 30_000 exactly.
        assert_eq!(
            Thb::from_minor(12_000_000).fee_bps(25).unwrap().minor(),
            30_000
        );
        // 1 bps on 9_999 truncates to 0, matching integer division on-chain.
        assert_eq!(Thb::from_minor(9_999).fee_bps(1).unwrap().minor(), 0);
    }

    #[test]
    fn display_renders_six_decimals() {
        assert_eq!(Thb::from_minor(1_500_000).to_string(), "1.500000 THB");
        assert_eq!(Thb::from_minor(1).to_string(), "0.000001 THB");
    }

    #[test]
    fn thb_serializes_as_a_bare_integer() {
        // Wire format is minor units, transparent. A JSON float here would
        // reintroduce exactly the rounding F2 cannot tolerate.
        let json = serde_json::to_string(&Thb::from_minor(12_345)).unwrap();
        assert_eq!(json, "12345");
    }
}
