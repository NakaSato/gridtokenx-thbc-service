//! Configuration, read from the environment.
//!
//! No hardcoded ports — every one comes from an env var with a documented default,
//! per the repo's port scheme (4000s gateways, 5000s gRPC mesh, 7000s persistence,
//! 9000s messaging).

use std::env;

use anyhow::{Context, Result, bail};

/// Which ledger the service talks to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LedgerMode {
    /// Chain Bridge over NATS. Most of the payment leg returns `501 not_implemented`,
    /// because the §4 instructions do not exist. This is the honest production mode.
    ChainBridge,
    /// In-memory model of the treasury *as specified*. Everything works and none of
    /// it is real. Spec §12.
    Simulated,
}

impl LedgerMode {
    fn parse(s: &str) -> Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "chain-bridge" | "chain_bridge" | "chainbridge" => Ok(Self::ChainBridge),
            "simulated" | "sim" => Ok(Self::Simulated),
            other => bail!("THBC_LEDGER_MODE must be 'chain-bridge' or 'simulated', got {other:?}"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    pub http_port: u16,
    pub database_url: Option<String>,

    /// Session-mode pooler alias used only to run migrations (`THBC_MIGRATION_DATABASE_URL`).
    ///
    /// `sqlx::migrate!` takes a session-scoped advisory lock, which a
    /// transaction-mode pooler will break. Unset means "no pooler in between" —
    /// migrations then run on [`Self::database_url`], which is correct for a direct
    /// Postgres connection.
    pub migration_database_url: Option<String>,

    pub nats_url: String,
    pub chain_bridge_grpc_url: String,
    pub ledger_mode: LedgerMode,

    /// Δ — how long a holder waits before reclaiming an unconfirmed redemption (F7).
    pub redemption_delta_secs: i64,

    /// Attestation TTL for the simulated ledger (F5). The real one reads it from the
    /// treasury account.
    pub attestation_ttl_secs: i64,

    /// Starting attested reserve for the simulated ledger, in minor units.
    pub simulated_reserve_minor: u64,

    /// Compliance auto-pass ceiling for the stub screener, in minor units.
    /// Deposits above it fail the screen, so the §5.4 encumbrance path is reachable.
    pub stub_kyc_ceiling_minor: u64,
}

fn var_or<T: std::str::FromStr>(key: &str, default: T) -> Result<T>
where
    T::Err: std::fmt::Display,
{
    match env::var(key) {
        Ok(v) => v.parse::<T>().map_err(|e| anyhow::anyhow!("{key}: {e}")),
        Err(_) => Ok(default),
    }
}

/// This service's database URL variable.
///
/// **Deliberately not `DATABASE_URL`.** That name is set in the superproject's root
/// `.env` and points at the shared `gridtokenx` database. A service that read it
/// would run its own migrations into the shared database the moment anyone started it
/// from the repo — which is exactly what happened once during development, creating
/// `deposits`, `redemptions` and `reconciliation_runs` in `gridtokenx`.
///
/// The DB-per-service split is mid-flight and this service is greenfield, so it gets
/// its own database from the start. Point this at `gridtokenx_thbc`.
pub const DATABASE_URL_VAR: &str = "THBC_DATABASE_URL";

/// Session-mode pooler alias for migrations. See [`Config::migration_database_url`].
pub const MIGRATION_DATABASE_URL_VAR: &str = "THBC_MIGRATION_DATABASE_URL";

/// The database name from a Postgres URL — the last path segment, minus any query.
///
/// Substring matching would be wrong in both directions here: it would reject the
/// legitimate `gridtokenx_thbc`, and it would accept a host called
/// `gridtokenx.example.com` serving the database `gridtokenx`.
fn database_name(url: &str) -> &str {
    url.rsplit('/')
        .next()
        .unwrap_or("")
        .split('?')
        .next()
        .unwrap_or("")
}

impl Config {
    /// Load from the environment.
    ///
    /// Reads `.env` from the current directory only — `dotenvy::from_filename`, not
    /// `dotenvy::dotenv`, which walks *up* the tree and would pick up the
    /// superproject's `.env` along with its shared-database `DATABASE_URL`.
    pub fn from_env() -> Result<Self> {
        let _ = dotenvy::from_filename(".env");

        let ledger_mode = LedgerMode::parse(
            &env::var("THBC_LEDGER_MODE").unwrap_or_else(|_| "chain-bridge".into()),
        )?;

        let config = Self {
            http_port: var_or("THBC_HTTP_PORT", 4008u16).context("THBC_HTTP_PORT")?,
            database_url: env::var(DATABASE_URL_VAR).ok(),
            migration_database_url: env::var(MIGRATION_DATABASE_URL_VAR).ok(),
            nats_url: env::var("NATS_URL").unwrap_or_else(|_| "nats://localhost:9001".into()),
            chain_bridge_grpc_url: env::var("CHAIN_BRIDGE_GRPC_URL")
                .unwrap_or_else(|_| "http://localhost:5001".into()),
            ledger_mode,
            redemption_delta_secs: var_or("THBC_REDEMPTION_DELTA_SECS", 86_400i64)?,
            attestation_ttl_secs: var_or("THBC_ATTESTATION_TTL_SECS", 3_600i64)?,
            simulated_reserve_minor: var_or("THBC_SIMULATED_RESERVE_MINOR", 0u64)?,
            stub_kyc_ceiling_minor: var_or("THBC_STUB_KYC_CEILING_MINOR", 1_000_000_000u64)?,
        };
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<()> {
        if self.redemption_delta_secs <= 0 {
            bail!(
                "THBC_REDEMPTION_DELTA_SECS must be positive — a non-positive delta makes \
                 every redemption instantly reclaimable and F7 meaningless"
            );
        }
        if self.attestation_ttl_secs <= 0 {
            bail!(
                "THBC_ATTESTATION_TTL_SECS must be positive — a non-positive TTL means no \
                 attestation is ever fresh and F5 halts all issuance"
            );
        }

        // The stub screener and the recording payout queue are test doubles. Wiring
        // them behind the real Chain Bridge would produce a service that looks like
        // it does KYC and looks like it wires fiat, and does neither.
        if self.ledger_mode == LedgerMode::ChainBridge && self.database_url.is_none() {
            bail!(
                "{DATABASE_URL_VAR} is required in chain-bridge mode — the in-memory \
                 repositories lose every deposit and redemption record on restart, taking \
                 the off-chain half of F3 and the whole F2 tally with them"
            );
        }

        // This service owns its database. Migrating into a database another service
        // owns creates `deposits` / `redemptions` tables beside tables that already
        // have owners, and adds a cross-service JOIN surface the DB-per-service
        // migration is trying to remove.
        // Checked on BOTH urls: the migration alias is the one that actually runs
        // CREATE TABLE, so a correct runtime url with a shared-database migration
        // alias is the worst case — it would look right and still write into
        // `gridtokenx`.
        for (var, url) in [
            (DATABASE_URL_VAR, self.database_url.as_ref()),
            (
                MIGRATION_DATABASE_URL_VAR,
                self.migration_database_url.as_ref(),
            ),
        ] {
            let Some(url) = url else { continue };
            if database_name(url) == "gridtokenx" {
                bail!(
                    "{var} points at the shared `gridtokenx` database. This service owns \
                     its own schema and must use a dedicated database (`gridtokenx_thbc`); \
                     running its migrations into the shared one creates \
                     deposits/redemptions tables beside other services' data"
                );
            }
        }
        Ok(())
    }

    #[must_use]
    pub const fn is_simulated(&self) -> bool {
        matches!(self.ledger_mode, LedgerMode::Simulated)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ledger_mode_accepts_the_documented_spellings() {
        assert_eq!(
            LedgerMode::parse("simulated").unwrap(),
            LedgerMode::Simulated
        );
        assert_eq!(LedgerMode::parse("SIM").unwrap(), LedgerMode::Simulated);
        assert_eq!(
            LedgerMode::parse(" chain-bridge ").unwrap(),
            LedgerMode::ChainBridge
        );
        assert_eq!(
            LedgerMode::parse("chain_bridge").unwrap(),
            LedgerMode::ChainBridge
        );
    }

    #[test]
    fn an_unknown_ledger_mode_fails_rather_than_defaulting() {
        // Defaulting to `simulated` would silently run a fake ledger in production;
        // defaulting to `chain-bridge` would silently 501 a working simulation.
        assert!(LedgerMode::parse("production").is_err());
    }

    fn base() -> Config {
        Config {
            http_port: 4008,
            database_url: Some("postgres://localhost:7001/gridtokenx_thbc".into()),
            migration_database_url: None,
            nats_url: "nats://localhost:9001".into(),
            chain_bridge_grpc_url: "http://localhost:5001".into(),
            ledger_mode: LedgerMode::Simulated,
            redemption_delta_secs: 86_400,
            attestation_ttl_secs: 3_600,
            simulated_reserve_minor: 0,
            stub_kyc_ceiling_minor: 1_000_000_000,
        }
    }

    #[test]
    fn a_valid_config_passes() {
        assert!(base().validate().is_ok());
    }

    #[test]
    fn a_non_positive_delta_is_rejected() {
        // Δ = 0 makes every redemption instantly reclaimable, which reads as "F7
        // implemented" while removing the issuer's window entirely.
        assert!(
            Config {
                redemption_delta_secs: 0,
                ..base()
            }
            .validate()
            .is_err()
        );
        assert!(
            Config {
                redemption_delta_secs: -1,
                ..base()
            }
            .validate()
            .is_err()
        );
    }

    #[test]
    fn a_non_positive_attestation_ttl_is_rejected() {
        assert!(
            Config {
                attestation_ttl_secs: 0,
                ..base()
            }
            .validate()
            .is_err()
        );
    }

    #[test]
    fn chain_bridge_mode_requires_a_database() {
        let c = Config {
            ledger_mode: LedgerMode::ChainBridge,
            database_url: None,
            ..base()
        };
        assert!(c.validate().is_err());
    }

    #[test]
    fn pointing_at_the_shared_gridtokenx_database_is_rejected() {
        // This actually happened during development: `dotenvy::dotenv()` walked up to
        // the superproject `.env`, picked up its shared-database `DATABASE_URL`, and
        // the service migrated `deposits` / `redemptions` / `reconciliation_runs` into
        // `gridtokenx`. Two fixes, both needed: read `THBC_DATABASE_URL` instead of
        // `DATABASE_URL`, and refuse the shared database by name if someone sets it
        // anyway.
        for url in [
            "postgres://localhost:7001/gridtokenx",
            "postgresql://user:pw@localhost:7001/gridtokenx",
            "postgresql://user:pw@localhost:7001/gridtokenx?sslmode=disable",
        ] {
            let c = Config {
                database_url: Some(url.into()),
                ..base()
            };
            assert!(c.validate().is_err(), "{url} must be refused");
        }
    }

    #[test]
    fn a_shared_database_migration_alias_is_rejected_too() {
        // The nastiest misconfiguration: runtime url correct, migration alias wrong.
        // The alias is what runs CREATE TABLE, so this would look right and still
        // write `deposits` / `redemptions` into the shared database.
        let c = Config {
            database_url: Some("postgres://pgdog:6432/gridtokenx_thbc".into()),
            migration_database_url: Some("postgres://pgdog:6432/gridtokenx".into()),
            ..base()
        };
        assert!(c.validate().is_err());
    }

    #[test]
    fn the_session_mode_migration_alias_is_accepted() {
        let c = Config {
            database_url: Some("postgres://pgdog:6432/gridtokenx_thbc".into()),
            migration_database_url: Some("postgres://pgdog:6432/gridtokenx_thbc_migrate".into()),
            ..base()
        };
        assert!(c.validate().is_ok());
    }

    #[test]
    fn database_name_parses_the_last_segment_only() {
        assert_eq!(
            database_name("postgres://u:p@host:6432/gridtokenx"),
            "gridtokenx"
        );
        assert_eq!(
            database_name("postgres://u:p@host:6432/gridtokenx_thbc"),
            "gridtokenx_thbc"
        );
        assert_eq!(
            database_name("postgres://u:p@host:6432/gridtokenx?sslmode=disable"),
            "gridtokenx"
        );
        // A host that merely contains the name must not be mistaken for it.
        assert_eq!(
            database_name("postgres://u:p@gridtokenx.example.com/thbc"),
            "thbc"
        );
    }

    #[test]
    fn a_dedicated_database_is_accepted() {
        // Substring matching would wrongly reject this one — the check is on the
        // parsed database name, not on whether the URL contains "gridtokenx".
        let c = Config {
            database_url: Some("postgresql://u:p@localhost:7001/gridtokenx_thbc".into()),
            ..base()
        };
        assert!(c.validate().is_ok());
    }

    #[test]
    fn simulated_mode_runs_without_a_database() {
        let c = Config {
            ledger_mode: LedgerMode::Simulated,
            database_url: None,
            ..base()
        };
        assert!(c.validate().is_ok());
    }
}
