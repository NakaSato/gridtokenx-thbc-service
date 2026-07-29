# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

`gridtokenx-thbc-service` is the **payment leg** of GridTokenX — how Thai baht enters and
leaves the ledger. It is a submodule; the superproject's `CLAUDE.md` (repo-wide rules)
still applies. This file covers only what is specific to this service.

Read [`ARCHITECTURE.md`](ARCHITECTURE.md) before non-trivial work — it holds the state
machines, trust boundaries and the honest test-coverage table.

---

## Commands

```bash
cd gridtokenx-thbc-service      # never cargo from the repo root — own workspace

cargo test                      # 199 tests, 14 suites, no infra (DB/chain/validator all optional)
cargo test -p thbc-core         # one crate
cargo test --test payment_leg   # one integration suite (thbc-logic)
cargo test f7_reclaim -- --nocapture   # one test by substring
cargo check
cargo clippy --all-targets -- -D warnings

cp .env.example .env
THBC_LEDGER_MODE=simulated \
THBC_SIMULATED_RESERVE_MINOR=1000000000000 \
  cargo run --bin thbc-service  # :4008
```

Both gates are green as of 2026-07-29. There is **no CI in this repo** — every gate is
manual, and a green local run is the only signal you get, so run them yourself.

The reconciler loop in `bin/thbc-service/src/main.rs` carries a scoped
`#[allow(clippy::match_same_arms)]`: two of its arms are silent for opposite reasons
("nothing was held" vs "the reserve snapshot is unavailable") and merging them would erase
a distinction the next person needs. Keep the reason comment if you touch it.

Docker: build context is **this directory**, not the repo root — unlike most services here,
nothing in `Cargo.toml` points at a sibling submodule except the light
`gridtokenx-blockchain-types` path dep.

---

## The invariant registry is the source of truth

[`crates/thbc-core/src/invariant.rs`](crates/thbc-core/src/invariant.rs) is the
authoritative, machine-readable statement of what this system actually guarantees, served
live at `GET /v1/admin/invariants`. **Prefer it to any prose, including `ARCHITECTURE.md`
and `README.md`.** Current: F1/F3/F5/F7 `Enforced`, F2/F4/F6 `Partial`, F8 `Violated`,
F9 `DesignOnly`.

Rules when touching it:

- **Never mark an invariant `Enforced` because the happy path passes.** `Enforced` means
  running code plus a test that exercises the *violating* path. A status that overstates
  reality is worse than no registry — this file is written to be read by a regulator.
- `Status::Violated` is strictly worse than `DesignOnly`: it means current code breaks it.
- Every non-`Enforced` entry carries its disclosed gap; keep it accurate.
- On-chain enforcement lives in `gridtokenx-anchor/programs/treasury` and is tested by
  `gridtokenx-anchor/tests/treasury_thbc_litesvm.ts` (27 LiteSVM cases, mutation-checked).
  Changing an invariant's story usually means changing both repos.

Load-bearing detail: F8 (non-custody) is `Violated` because `gridtokenx-iam-service` can
decrypt any user's signing key. *This service* holds no key and no port accepts one — but
do not restate that as a platform-level non-custody claim.

---

## Architecture

Dependency direction `bin → api → logic → persistence → core`, never reversed.

| Crate | Async? | Owns |
| :-- | :-- | :-- |
| `thbc-core` | **no** | money types, F1–F9 registry, deposit/redemption state machines, exchange math, reconciliation, and the port traits |
| `thbc-ledger` | yes | `ChainBridgeLedger` (real) + `SimulatedLedger` (spec §12 prototype) |
| `thbc-persistence` | yes | Postgres + in-memory repositories |
| `thbc-logic` | yes | issuance, redemption, reserve, reconciliation, treasury services |
| `thbc-api` | yes | public / partner / admin routers |
| `bin/thbc-service` | yes | config, DI wiring, background reconciler, graceful shutdown |

`thbc-core` depends on `serde`, `thiserror`, `sha2` and nothing else — which is why the
whole invariant suite runs with no runtime, database, or validator. Keep it that way: a new
dependency there costs the property that every invariant decision is a pure function.

Two orderings are the reason `thbc-logic` exists, both documented at their site:

- **§5.2 — attestation precedes issuance** (`issuance.rs`). The `Deposit` state machine has
  no `Screened → Issued` transition, so the F1-violating interval is unrepresentable.
- **§6.2 (F4) — a confirmed burn precedes a fiat payout** (`redemption.rs`). The barrier is
  *confirmation*, not RPC acceptance. State is persisted **before** the payout is enqueued:
  a double payout is unrecoverable in a way a double burn is not.

Supply falls at `Confirmed` and nowhere else. A reclaim returns the tokens and must **never**
enter the redeemed tally — counting it fabricates F2 drift indistinguishable from an
unbacked mint.

---

## Conventions specific to this service

**`Unsupported` is the correct answer, not a TODO.** In `chain-bridge` mode
`ChainBridgeLedger` returns `PortError::Unsupported` → `501 not_implemented` for `issue`,
`escrow_redemption`, `confirm_redemption`, `reclaim_redemption` and `snapshot`. Do not
"implement" these by routing to instructions that happen to exist — routing issuance to
`swap_grx_for_thbc` would mint against GRX collateral and violate F6 on every deposit.
`Unsupported` is a distinct variant from `Rejected` so operators can tell "not built" from
"the chain said no". Read the module doc in `crates/thbc-ledger/src/chain_bridge.rs` first.

**`THBC_LEDGER_MODE`: unset defaults to `chain-bridge`, set-but-unrecognised is a hard
startup failure.** The asymmetry is deliberate — forgetting the variable gets the honest
mode, which `501`s the unbuilt routes loudly; defaulting to `simulated` would serve a fake
ledger to anyone who forgot. `simulated` makes everything work and none of it real, so the
service logs its posture at startup and reports `simulated_ledger` on `/health`.

**`THBC_DATABASE_URL`, never `DATABASE_URL`.** Config uses `dotenvy::from_filename(".env")`
in *this* directory, not `dotenvy::dotenv()` which walks up and finds the superproject's
shared-`gridtokenx` URL. A URL naming the `gridtokenx` database is rejected at startup — for
both the runtime and the migration URL.

**Migrations run on their own pool.** `sqlx::migrate!` takes a session-scoped advisory lock
that pgdog's transaction pooling would break, so `THBC_MIGRATION_DATABASE_URL` points at the
session-mode alias and `startup::run_migrations` closes that pool before the runtime pool
opens. Leave it unset when connecting straight to Postgres (local dev, tests).

**Runtime `sqlx` queries, not `query_as!`** — deliberate and temporary. The macro needs a
live `DATABASE_URL` at build time or a committed `.sqlx` cache, and this database has never
been provisioned, so the macro form breaks `cargo check` on a fresh clone.

**All money is `BIGINT` minor units** (6 decimals) — never `NUMERIC`, never floats. F2 is an
exact identity and does not survive rounding.

**`unwrap_used` and `expect_used` are `deny` workspace-wide.** The payment leg must not panic
on a value from a bank, a user, or the chain. Test modules re-allow them locally via
`#![cfg_attr(test, allow(...))]` in each `lib.rs` and `#![allow(...)]` atop each integration
test — follow that pattern rather than sprinkling per-site allows. `missing_errors_doc` is
allowed on purpose: `CoreError`/`PortError` variants already document themselves against the
invariant they protect.

**No Solana RPC client, ever.** Writes go over NATS JetStream, reads over Chain Bridge gRPC.
The `gridtokenx-blockchain-types` path dep is the deliberately-extracted light half of
`gridtokenx-blockchain-core` (serde/base64/p256/sha2 only, no solana-sdk/anchor/SPL) — do not
re-create a local mirror of those wire types.

**Own database (`gridtokenx_thbc`), no cross-service JOINs or foreign keys.**

---

## Keeping the docs honest

`README.md` and `ARCHITECTURE.md` were reconciled against the code on 2026-07-29 (stale test
count, a stale F6 paragraph naming instructions that no longer exist, and a stale
`THBC_LEDGER_MODE` claim). The drift pattern to watch for: **prose that describes the
on-chain side**, which lives in `gridtokenx-anchor` and moves independently of this repo.

- Verify against `gridtokenx-anchor/programs/treasury/src/instructions/` before repeating any
  claim about what an instruction does. `swap_grx_for_thbc` / `redeem_thbc_for_grx` are gone;
  `exchange_grx_for_thbc` / `exchange_thbc_for_grx` replaced them.
- Run the doc-lint gate from the superproject — it catches broken links and stale `path:line`
  citations, but only for **git-tracked** files, so a new untracked doc is silently skipped:

  ```bash
  cd .. && python3 scripts/lint-docs.py --root gridtokenx-thbc-service
  ```
