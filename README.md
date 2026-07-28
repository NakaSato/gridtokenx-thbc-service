# gridtokenx-thbc-service

The **payment leg** of GridTokenX: how Thai baht enters and leaves the ledger.

> **THBC** is a Thai-baht-referenced settlement token. One unit represents a claim on
> one Thai baht held in a segregated reserve account at a licensed financial
> institution, issued against fiat received and burned against fiat paid **by a
> licensed issuer partner**. GridTokenX operates the ledger integration and the
> settlement logic. GridTokenX is not the issuer, holds no fiat, and holds no user keys.

Spec: [`../docs/product-specs/THBC_ISSUER_SERVICE.md`](../docs/product-specs/THBC_ISSUER_SERVICE.md) ·
Design: [`ARCHITECTURE.md`](ARCHITECTURE.md)

---

## ⚠️ Status: design + simulation

**No fiat is held. No licence is held. No fiat rail exists.**

Most of the payment leg is not built on-chain. `issue_thbc`, `redeem_thbc_for_fiat`,
the deposit nullifier PDA and the redemption escrow do not exist in the treasury
program (spec §12). This service models them correctly and executes them against a
simulator; in `chain-bridge` mode it returns `501 not_implemented` for those routes,
which is the accurate answer.

Of the nine invariants, **only F5, F8 and F9 may be described as guarantees.** F6 is
actively *violated* by the live `swap_grx_for_thbc`, which mints THBC against GRX
collateral. The authoritative, machine-readable status is:

```bash
curl localhost:4008/v1/admin/invariants
```

---

## Quick start

```bash
cd gridtokenx-thbc-service
cargo test                                  # 149 tests, no infrastructure needed

cp .env.example .env
THBC_LEDGER_MODE=simulated \
THBC_SIMULATED_RESERVE_MINOR=1000000000000 \
  cargo run --bin thbc-service              # listens on :4008
```

Never run `cargo` from the repo root — each service is its own workspace.

### Walk the on-ramp

```bash
# Deposit webhook (partner-api). Signature header is required.
curl -X POST localhost:4008/v1/partner/webhooks/deposit \
  -H 'content-type: application/json' -H 'x-thbc-signature: dev' \
  -d '{"bank_ref":"SCB-20260729-0001","amount_minor":100000000,"beneficiary":"alice"}'
# → 201 {"status":"issued", ...}

# Replay it — the bank is supposed to retry (F3).
# Same call again → 200 {"status":"already_issued", ...}   no second issuance

# The reserve, and the F1 ceiling right now
curl localhost:4008/v1/admin/reserve

# Redeem. Note the response says "escrowed", never "redeemed".
curl -X POST localhost:4008/v1/redemptions \
  -H 'content-type: application/json' \
  -d '{"user":"alice","amount_minor":40000000}'

# Try to reclaim early → 409 timelock_not_expired (F7)
curl -X POST localhost:4008/v1/redemptions/alice/1/reclaim
```

---

## Configuration

Full list with rationale in [`.env.example`](.env.example). Two that will bite you:

- **`THBC_DATABASE_URL`**, not `DATABASE_URL`. The superproject's root `.env` points
  `DATABASE_URL` at the shared `gridtokenx` database, and this service must not
  migrate into it. Pointing this variable at `gridtokenx` is rejected at startup.
- **`THBC_LEDGER_MODE`** — `simulated` (everything works, none of it is real) or
  `chain-bridge` (the real path, mostly `501`). No default that could be silently
  wrong: an unrecognised value fails startup.

---

## Layout

```
crates/
  thbc-core/         sync domain — money, F1–F9 registry, state machines, exchange math
  thbc-ledger/       ChainBridgeLedger (real) + SimulatedLedger (the §12 prototype)
  thbc-persistence/  Postgres + in-memory repositories
  thbc-logic/        the §9 services — issuance, redemption, reserve, reconciliation
  thbc-api/          public / partner / admin routers
bin/thbc-service/    config, DI wiring, shutdown
migrations/          own database (gridtokenx_thbc), no cross-service JOINs
```

`thbc-core` is synchronous and depends on `serde`, `thiserror`, `sha2` and nothing
else — so every invariant test runs without a runtime, a database, or a validator.

---

## HTTP surface

Authentication is terminated at APISIX. This service does not verify JWTs or client
certificates; exposing its port directly publishes the admin surface.

| Method | Path | Notes |
| :-- | :-- | :-- |
| `GET` | `/health` `/ready` | reports `simulated_ledger` so it can't be mistaken for live |
| `POST` | `/v1/partner/webhooks/deposit` | bank webhook — **untrusted input**, at-least-once |
| `POST` | `/v1/redemptions` | user-signed redemption; returns `escrowed`, never `redeemed` |
| `GET` | `/v1/redemptions/{user}/{seq}` | includes `reclaim_in_secs` |
| `POST` | `/v1/redemptions/{user}/{seq}/reclaim` | F7 |
| `POST` | `/v1/exchange/quote/{buy,sell}` | inventory exchange; `changes_supply` is always `false` |
| `GET` | `/v1/admin/invariants` | **what is actually guaranteed** — regulator-readable |
| `GET` | `/v1/admin/reserve` | F1 ceiling, freshness, encumbrance |
| `GET` | `/v1/admin/reconciliation` | F2 identity + F1 solvency |
| `GET` | `/v1/admin/redemptions/queue` | `redemption_queue_len` + overdue records (§6.3) |
| `POST` | `/v1/admin/attestation` | attestor-signed reserve refresh |
| `POST` | `/v1/admin/redemptions/{confirm,payout}` | payout is the F4 barrier |

`409 burn_not_confirmed` and `409 timelock_not_expired` are the barriers working, not
outages.

---

## Related

- [`ARCHITECTURE.md`](ARCHITECTURE.md) — state machines, trust boundaries, honest test coverage, and what it takes to make this real
- [`../KNOWN_LIMITATIONS.md`](../KNOWN_LIMITATIONS.md) — the disclosed gaps
- `gridtokenx-anchor/programs/treasury` — the on-chain side, most of which is unwritten
- `gridtokenx-chain-bridge` — the only service that touches Solana RPC
