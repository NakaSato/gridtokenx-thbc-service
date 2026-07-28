//! Ledger adapters — the only code in this service that talks to the chain.
//!
//! Two implementations of [`thbc_core::ports::LedgerPort`]:
//!
//! - [`ChainBridgeLedger`] — the real path, over Chain Bridge (NATS writes, gRPC
//!   reads). Most of its surface returns `Unsupported`, because the §4 instructions
//!   it would call do not exist yet. Read its module doc for why that is correct
//!   rather than lazy.
//! - [`SimulatedLedger`] — an in-memory model of the treasury *as specified*,
//!   including the nullifier set and redemption escrow. This is the §12 prototype and
//!   what the invariant suite executes against.
//!
//! Neither holds a Solana RPC client, and neither has a method that accepts a user
//! private key (F8).

// Test code asserts that guards fire; unwrapping is the assertion. Denied in
// production code by the workspace lints.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod chain_bridge;
pub mod simulated;

pub use chain_bridge::{ChainBridgeConfig, ChainBridgeLedger};
pub use simulated::SimulatedLedger;
