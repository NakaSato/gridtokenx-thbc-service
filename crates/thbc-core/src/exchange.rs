//! Inventory exchange — spec §7. The F6 fix.
//!
//! The old path minted THBC against GRX collateral while consuming fiat-reserve
//! headroom (`swap_grx_for_thbc` calls `mint_to`,
//! `gridtokenx-anchor/programs/treasury/src/instructions/swap_grx_for_thbc.rs:97`).
//! That put a volatile asset in the backing set of a fiat-referenced token and made
//! the peg a governance parameter.
//!
//! Here, exchange moves tokens that already exist between the platform's inventory
//! and the user. `thbc_supply` and `attested_reserve` are untouched, so F1 and F6
//! hold **by construction** rather than by check.
//!
//! The type system carries that: [`ExchangeQuote`] has no field that could express a
//! supply change, so no caller can request one. The risk does not vanish — it moves
//! onto the platform's own balance sheet as GRX inventory risk, which is the correct
//! place for it (§7.2). `rate` is an admin parameter and remains a disclosed
//! centralisation: a quoted market-maker rate, not a peg.

use serde::{Deserialize, Serialize};

use crate::error::{CoreError, CoreResult};
use crate::money::{GRX_ATOMS_PER_WHOLE, Grx, Thb};

/// Platform-held inventory backing the exchange path. Bounded on purpose — §7.3
/// rules out an AMM, so the quote is against finite inventory, not a curve.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Inventory {
    /// THBC the platform holds for the exchange path (`[b"thbc_inventory"]`).
    pub thbc: Thb,
    /// GRX held against exchange (`[b"swap_vault"]`).
    pub grx: Grx,
}

/// Admin-set exchange parameters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExchangeParams {
    /// THBC minor units per one whole GRX. Mirrors `grx_per_thbc_rate`.
    pub grx_per_thbc_rate: u64,
    pub fee_bps: u16,
    pub paused: bool,
}

impl ExchangeParams {
    fn validate(&self) -> CoreResult<()> {
        if self.paused {
            return Err(CoreError::Paused);
        }
        if self.grx_per_thbc_rate == 0 {
            return Err(CoreError::RateNotSet);
        }
        if self.fee_bps > 10_000 {
            return Err(CoreError::InvalidFeeBps { bps: self.fee_bps });
        }
        Ok(())
    }
}

/// A priced exchange, ready to execute as two transfers.
///
/// Note what is absent: no `mint`, no `burn`, no `new_supply`. Compare
/// `compute_swap_grx_for_thbc`, whose return tuple carries `new_supply` precisely
/// because it mints.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExchangeQuote {
    pub grx_in: Grx,
    pub thbc_out: Thb,
    pub fee: Thb,
    /// Inventory after execution.
    pub inventory_after: Inventory,
}

/// A priced THBC → GRX exchange.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReverseQuote {
    pub thbc_in: Thb,
    pub grx_out: Grx,
    pub fee: Grx,
    pub inventory_after: Inventory,
}

/// `exchange_grx_for_thbc` — user pays GRX, platform pays THBC from inventory.
///
/// Pricing is identical to the on-chain `compute_swap_grx_for_thbc`
/// (`programs/treasury/src/lib.rs:67`) so quotes match execution to the minor unit:
/// `gross = grx_in * rate / 1e9`, fee truncating, `net = gross − fee`.
///
/// The one substantive difference: where the program checks `new_supply ≤
/// attested_reserve`, this checks `thbc_out ≤ inventory`. Reserve headroom is not
/// consumed at all.
pub fn quote_grx_for_thbc(
    grx_in: Grx,
    params: &ExchangeParams,
    inventory: Inventory,
) -> CoreResult<ExchangeQuote> {
    if grx_in.is_zero() {
        return Err(CoreError::ZeroAmount);
    }
    params.validate()?;

    let gross = u128::from(grx_in.atoms())
        .checked_mul(u128::from(params.grx_per_thbc_rate))
        .ok_or(CoreError::MathOverflow)?
        / GRX_ATOMS_PER_WHOLE;
    let gross = Thb::from_minor(u64::try_from(gross).map_err(|_| CoreError::MathOverflow)?);

    let fee = gross.fee_bps(params.fee_bps)?;
    let net = gross.saturating_sub(fee);
    if net.is_zero() {
        return Err(CoreError::ZeroAmount);
    }

    // F6: a shortfall is a refusal, never a mint.
    if net > inventory.thbc {
        return Err(CoreError::InsufficientInventory {
            requested: net.minor(),
            available: inventory.thbc.minor(),
        });
    }

    Ok(ExchangeQuote {
        grx_in,
        thbc_out: net,
        fee,
        inventory_after: Inventory {
            // The fee stays in inventory — it is revenue, not a supply change.
            thbc: inventory.thbc.checked_sub(net)?,
            grx: inventory.grx.checked_add(grx_in)?,
        },
    })
}

/// `exchange_thbc_for_grx` — user pays THBC into inventory, platform pays GRX from
/// the swap vault. `grx_out = thbc_in * 1e9 / rate`, matching
/// `compute_redeem_thbc_for_grx` (`programs/treasury/src/lib.rs:100`) — except that
/// the on-chain version *burns* the incoming THBC and this one does not.
pub fn quote_thbc_for_grx(
    thbc_in: Thb,
    params: &ExchangeParams,
    inventory: Inventory,
) -> CoreResult<ReverseQuote> {
    if thbc_in.is_zero() {
        return Err(CoreError::ZeroAmount);
    }
    params.validate()?;

    let gross = u128::from(thbc_in.minor())
        .checked_mul(GRX_ATOMS_PER_WHOLE)
        .ok_or(CoreError::MathOverflow)?
        / u128::from(params.grx_per_thbc_rate);
    let gross = Grx::from_atoms(u64::try_from(gross).map_err(|_| CoreError::MathOverflow)?);

    let fee_atoms = u128::from(gross.atoms())
        .checked_mul(u128::from(params.fee_bps))
        .ok_or(CoreError::MathOverflow)?
        / 10_000;
    let fee = Grx::from_atoms(u64::try_from(fee_atoms).map_err(|_| CoreError::MathOverflow)?);
    let net = Grx::from_atoms(gross.atoms().saturating_sub(fee.atoms()));
    if net.is_zero() {
        return Err(CoreError::ZeroAmount);
    }

    // The GRX vault is finite too; refuse rather than over-draw it.
    if net > inventory.grx {
        return Err(CoreError::InsufficientInventory {
            requested: net.atoms(),
            available: inventory.grx.atoms(),
        });
    }

    Ok(ReverseQuote {
        thbc_in,
        grx_out: net,
        fee,
        inventory_after: Inventory {
            thbc: inventory.thbc.checked_add(thbc_in)?,
            grx: inventory.grx.checked_sub(net)?,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params() -> ExchangeParams {
        ExchangeParams {
            grx_per_thbc_rate: 4_000_000,
            fee_bps: 25,
            paused: false,
        }
    }

    fn inventory() -> Inventory {
        Inventory {
            thbc: Thb::from_minor(1_000_000_000),
            grx: Grx::from_atoms(1_000_000_000_000),
        }
    }

    // ---- pricing parity with the on-chain math -------------------------------

    #[test]
    fn pricing_matches_the_on_chain_swap_math() {
        // Same vector as the program's own unit test
        // (programs/treasury/src/lib.rs:175): 3 GRX at rate 4_000_000, 25 bps.
        // gross = 3e9 * 4e6 / 1e9 = 12_000_000; fee = 30_000; net = 11_970_000.
        let q = quote_grx_for_thbc(Grx::from_atoms(3_000_000_000), &params(), inventory()).unwrap();
        assert_eq!(q.thbc_out.minor(), 11_970_000);
        assert_eq!(q.fee.minor(), 30_000);
    }

    #[test]
    fn reverse_pricing_matches_the_on_chain_redeem_math() {
        // programs/treasury/src/lib.rs:233 — 12_000_000 THBC at rate 4_000_000
        // gives 3 GRX, before the fee this path also charges.
        let p = ExchangeParams {
            fee_bps: 0,
            ..params()
        };
        let q = quote_thbc_for_grx(Thb::from_minor(12_000_000), &p, inventory()).unwrap();
        assert_eq!(q.grx_out.atoms(), 3_000_000_000);
    }

    // ---- F6 ------------------------------------------------------------------

    #[test]
    fn f6_refuses_rather_than_minting_when_inventory_is_short() {
        let thin = Inventory {
            thbc: Thb::from_minor(1_000_000),
            ..inventory()
        };
        let err = quote_grx_for_thbc(Grx::from_atoms(3_000_000_000), &params(), thin).unwrap_err();
        assert_eq!(
            err,
            CoreError::InsufficientInventory {
                requested: 11_970_000,
                available: 1_000_000
            }
        );
    }

    #[test]
    fn f6_exchange_at_exactly_the_inventory_boundary_succeeds() {
        let exact = Inventory {
            thbc: Thb::from_minor(11_970_000),
            ..inventory()
        };
        let q = quote_grx_for_thbc(Grx::from_atoms(3_000_000_000), &params(), exact).unwrap();
        assert_eq!(q.inventory_after.thbc, Thb::ZERO);
    }

    #[test]
    fn f6_reserve_headroom_is_irrelevant_to_the_exchange_path() {
        // The structural claim of §7: a fully-subscribed reserve (zero headroom)
        // does not block an exchange, because no supply change is requested.
        // Under the old minting path this same call would hit PegBreach.
        let q = quote_grx_for_thbc(Grx::from_atoms(3_000_000_000), &params(), inventory());
        assert!(q.is_ok(), "exchange must not consume reserve headroom");
    }

    #[test]
    fn f6_inventory_conserves_total_thbc_across_a_round_trip() {
        // What the user gains, inventory loses. Nothing is created.
        let inv = inventory();
        let total_before = inv.thbc;
        let q = quote_grx_for_thbc(Grx::from_atoms(3_000_000_000), &params(), inv).unwrap();
        assert_eq!(
            q.inventory_after.thbc.checked_add(q.thbc_out).unwrap(),
            total_before
        );
    }

    #[test]
    fn f6_reverse_direction_returns_thbc_to_inventory_rather_than_burning_it() {
        let inv = inventory();
        let q = quote_thbc_for_grx(Thb::from_minor(12_000_000), &params(), inv).unwrap();
        assert_eq!(
            q.inventory_after.thbc,
            inv.thbc.checked_add(Thb::from_minor(12_000_000)).unwrap()
        );
    }

    #[test]
    fn f6_grx_vault_shortfall_is_also_a_refusal() {
        let thin = Inventory {
            grx: Grx::from_atoms(1_000),
            ..inventory()
        };
        assert!(matches!(
            quote_thbc_for_grx(Thb::from_minor(12_000_000), &params(), thin),
            Err(CoreError::InsufficientInventory { .. })
        ));
    }

    // ---- parameter guards ----------------------------------------------------

    #[test]
    fn a_paused_treasury_quotes_nothing() {
        let p = ExchangeParams {
            paused: true,
            ..params()
        };
        assert_eq!(
            quote_grx_for_thbc(Grx::from_atoms(1_000_000_000), &p, inventory()).unwrap_err(),
            CoreError::Paused
        );
        assert_eq!(
            quote_thbc_for_grx(Thb::from_minor(1_000_000), &p, inventory()).unwrap_err(),
            CoreError::Paused
        );
    }

    #[test]
    fn an_unset_rate_quotes_nothing() {
        let p = ExchangeParams {
            grx_per_thbc_rate: 0,
            ..params()
        };
        assert_eq!(
            quote_grx_for_thbc(Grx::from_atoms(1_000_000_000), &p, inventory()).unwrap_err(),
            CoreError::RateNotSet
        );
    }

    #[test]
    fn a_fee_over_one_hundred_percent_is_rejected() {
        let p = ExchangeParams {
            fee_bps: 10_001,
            ..params()
        };
        assert!(matches!(
            quote_grx_for_thbc(Grx::from_atoms(1_000_000_000), &p, inventory()),
            Err(CoreError::InvalidFeeBps { bps: 10_001 })
        ));
    }

    #[test]
    fn dust_that_prices_to_zero_is_rejected_not_silently_free() {
        // 1 atom at rate 4e6 gives gross = 0 — the user would pay GRX for nothing.
        assert_eq!(
            quote_grx_for_thbc(Grx::from_atoms(1), &params(), inventory()).unwrap_err(),
            CoreError::ZeroAmount
        );
    }

    #[test]
    fn a_hundred_percent_fee_leaves_nothing_and_is_rejected() {
        let p = ExchangeParams {
            fee_bps: 10_000,
            ..params()
        };
        assert_eq!(
            quote_grx_for_thbc(Grx::from_atoms(3_000_000_000), &p, inventory()).unwrap_err(),
            CoreError::ZeroAmount
        );
    }

    #[test]
    fn zero_input_is_rejected_in_both_directions() {
        assert_eq!(
            quote_grx_for_thbc(Grx::ZERO, &params(), inventory()).unwrap_err(),
            CoreError::ZeroAmount
        );
        assert_eq!(
            quote_thbc_for_grx(Thb::ZERO, &params(), inventory()).unwrap_err(),
            CoreError::ZeroAmount
        );
    }

    #[test]
    fn an_extreme_rate_overflows_into_an_error_not_a_wrap() {
        let p = ExchangeParams {
            grx_per_thbc_rate: u64::MAX,
            fee_bps: 0,
            paused: false,
        };
        let huge = Inventory {
            thbc: Thb::from_minor(u64::MAX),
            ..inventory()
        };
        assert!(quote_grx_for_thbc(Grx::from_atoms(u64::MAX), &p, huge).is_err());
    }
}
