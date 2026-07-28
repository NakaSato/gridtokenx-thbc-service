//! `treasury-service` — THBC inventory, GRX inventory, rate quoting (spec §7, §9).
//!
//! The exchange path. Every quote here comes from
//! [`thbc_core::exchange`], which is supply-preserving by construction — there is no
//! code path in this file that can request a mint or a burn, because the quote types
//! it handles have no field that could express one.
//!
//! `grx_per_thbc_rate` is an admin parameter and a disclosed centralisation (§7.2):
//! the platform sets the price at which it will exchange its own inventory. It is a
//! quoted market-maker rate against bounded inventory, not a peg, and §7.3 rules out
//! an AMM precisely so the reference rate never becomes a market outcome.

use std::sync::Arc;

use thbc_core::exchange::{ExchangeQuote, ReverseQuote, quote_grx_for_thbc, quote_thbc_for_grx};
use thbc_core::money::{Grx, Thb};
use thbc_core::ports::{LedgerPort, PortResult};
use tracing::instrument;

pub struct TreasuryService {
    ledger: Arc<dyn LedgerPort>,
}

impl TreasuryService {
    #[must_use]
    pub fn new(ledger: Arc<dyn LedgerPort>) -> Self {
        Self { ledger }
    }

    /// Quote GRX → THBC against platform inventory.
    ///
    /// Note what is *not* consulted: reserve headroom. Under the old minting path a
    /// fully-subscribed reserve blocked every exchange, because each one consumed
    /// fiat-reserve capacity. Here it is irrelevant — no supply change is requested,
    /// so F1 cannot be approached, let alone breached.
    #[instrument(skip(self), fields(grx_in = grx_in.atoms()))]
    pub async fn quote_buy_thbc(&self, grx_in: Grx) -> PortResult<ExchangeQuote> {
        let s = self.ledger.snapshot().await?;
        Ok(quote_grx_for_thbc(grx_in, &s.params, s.inventory)?)
    }

    /// Quote THBC → GRX. The incoming THBC returns to inventory; it is not burned.
    #[instrument(skip(self), fields(thbc_in = thbc_in.minor()))]
    pub async fn quote_sell_thbc(&self, thbc_in: Thb) -> PortResult<ReverseQuote> {
        let s = self.ledger.snapshot().await?;
        Ok(quote_thbc_for_grx(thbc_in, &s.params, s.inventory)?)
    }

    /// Inventory available to the exchange path — the bound on how much can be
    /// exchanged before quotes start failing (§7.3).
    pub async fn available_inventory(&self) -> PortResult<thbc_core::exchange::Inventory> {
        Ok(self.ledger.snapshot().await?.inventory)
    }
}
