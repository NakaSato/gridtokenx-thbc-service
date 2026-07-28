# THBC Settlement Service — Architecture

> The payment leg of GridTokenX: how Thai baht enters and leaves the ledger.
> Specified in [`../docs/product-specs/THBC_ISSUER_SERVICE.md`](../docs/product-specs/THBC_ISSUER_SERVICE.md).
> Last reviewed: 2026-07-29

---

## 0. Read this first

**Most of what this service describes is not built.** Spec §12: `issue_thbc`,
`redeem_thbc_for_fiat`, the deposit nullifier PDA, the redemption escrow and
`reserve_encumbered` do not exist in the treasury program, and **no fiat rail of any
kind exists**. This service models the payment leg correctly and executes it against a
simulator. A model is not a guarantee.

The authoritative statement of what is actually enforced is
[`crates/thbc-core/src/invariant.rs`](crates/thbc-core/src/invariant.rs), served live
at `GET /v1/admin/invariants`. Prefer it to any prose, including this file. Today:

| Invariant | Status | Enforced by |
| :-- | :-- | :-- |
| F1 reserve sufficiency | partial | on-chain, but against `attested_reserve` only — `reserve_encumbered` is off-chain |
| F2 issuance conservation | partial | off-chain, **detective not preventive** |
| F3 deposit idempotency | **design only** | nothing — the nullifier PDA does not exist |
| F4 burn-before-wire | partial | the redemption state machine; no fiat rail to test against |
| F5 attestation freshness | **enforced** | on-chain |
| F6 backing-set purity | **violated** | `swap_grx_for_thbc` mints |
| F7 redemption liveness | **design only** | nothing — the escrow does not exist |
| F8 non-custody | **enforced** | structural |
| F9 attestation independence | **enforced** | on-chain |

Only **F5, F8, F9** may be described to a third party as guarantees.

---

## 1. Where this sits

```
                          bank (B)
                             │  webhook, mTLS + signature   [UNTRUSTED INPUT]
                             ▼
   user ── JWT ──▶  APISIX :4001  ──▶  thbc-service :4008
                             ▲                 │
   regulator (E) ─ SSO/MFA ──┘                 │
                                               ├─▶ Postgres :7001/gridtokenx_thbc
                                               │
                                               └─▶ Chain Bridge
                                                     ├── NATS :9001  (writes)
                                                     └── gRPC :5001  (reads)
                                                            │
                                                            ▼
                                                     Solana / treasury program
```

This service **never** holds a Solana RPC client. Every ledger interaction goes
through Chain Bridge, per the repo-wide rule.

---

## 2. Crate layout

Dependency direction is `server → api → logic → persistence → core`, never reversed.

| Crate | Async? | Owns |
| :-- | :-- | :-- |
| [`thbc-core`](crates/thbc-core) | **no** | money types, F1–F9 registry, deposit/redemption state machines, exchange math, reconciliation. Three dependencies: `serde`, `thiserror`, `sha2`. |
| [`thbc-ledger`](crates/thbc-ledger) | yes | `ChainBridgeLedger` (real) and `SimulatedLedger` (the §12 prototype) |
| [`thbc-persistence`](crates/thbc-persistence) | yes | Postgres and in-memory repositories |
| [`thbc-logic`](crates/thbc-logic) | yes | the §9 services — issuance, redemption, reserve, reconciliation, treasury |
| [`thbc-api`](crates/thbc-api) | yes | public / partner / admin routers |
| [`bin/thbc-service`](bin/thbc-service) | yes | config, DI wiring, graceful shutdown |

"Sync core, async edges": every invariant decision is a pure function over values in
`thbc-core`, so the whole F1–F9 suite runs without a runtime, a database, or a
validator. `thbc-core`'s only async surface is `ports.rs`, where the trait definitions
live — the things behind those traits are a database, a message bus, and a bank.

---

## 3. The two orderings that matter

Everything else in `thbc-logic` is plumbing. These two are the reason the layer exists.

### §5.2 — attestation precedes issuance

`IssuanceService::handle_deposit` ([`crates/thbc-logic/src/issuance.rs`](crates/thbc-logic/src/issuance.rs)):

```
observe → screen → [attestation checked] → attested → issue
```

If issuance preceded attestation, F1 would be violated for the interval between them.
The `Deposit` state machine makes that interval unrepresentable — there is no
transition from `Screened` to `Issued`.

The ceiling checked here is `attested_reserve − reserve_encumbered`, assembled from
the chain's attestation plus **this service's own** deposit records, because
`reserve_encumbered` is not an on-chain field. **This service is therefore stricter
than the chain**, and that asymmetry is a disclosed gap, not a safety margin: a caller
that bypasses this service gets the looser ceiling the program actually enforces.

### §6.2 (F4) — a confirmed burn precedes a fiat payout

`RedemptionService::process_payout` ([`crates/thbc-logic/src/redemption.rs`](crates/thbc-logic/src/redemption.rs))
is the only route to a payout, and it refuses every state but `Escrowed`.

The barrier is **confirmation**, not RPC acceptance — the same rule as
`build_and_submit_generation_mint`, which replies success only on
`ConfirmOutcome::Confirmed`. A `Submitted` reply buys nothing: it leaves the record in
`Requested` and does not start the Δ clock.

Within the function, state is persisted **before** the payout is enqueued. If the
process dies between the two, the record claims a payout that may not have been sent
and an operator investigates. The other order loses the record of a wire that *was*
sent, and a double payout is unrecoverable in a way a double burn is not.
`PayoutPort::enqueue` is idempotent on `(user, seq)` to make the retry safe.

---

## 4. State machines

### Deposit (on-ramp, §5)

```
                    ┌──────────────────────────────┐
                    ▼                              │
  Observed ──screen(pass)──▶ Screened ──attest──▶ Attested ──issue──▶ Issued ▪
     │                          │                                             
     │ screen(fail)             │ dispute                                     
     ▼                          ▼                                             
  Encumbered ▪              Disputed ──resolve──▶ Screened                    
```

`▪` = terminal. `Encumbered` and `Disputed` both hold cleared fiat that backs no
token, so both tighten the F1 ceiling (`Deposit::is_encumbering`).

A failed KYC does **not** discard the record: the money is real and still in the
reserve account. Losing it would make `attested_reserve` overstate free backing.

### Redemption (off-ramp, §6)

```
  Requested ──escrow CONFIRMED──▶ Escrowed ──confirm──▶ Confirmed ▪  (burns)
      │                              │  │
      │ escrow FAILED                │  └──enqueue──▶ PayoutQueued ──confirm──▶ Confirmed ▪
      ▼                              │                      │
   Failed ▪                          └───── t ≥ Δ ──────────┴──▶ Reclaimed ▪  (supply unchanged)
```

Supply falls at `Confirmed` and nowhere else. A reclaim returns the tokens, so it must
**never** appear in the redeemed tally — counting it would fabricate F2 drift
indistinguishable from an unbacked mint.

`PayoutQueued` is still reclaimable. This service having *queued* a payout is not
evidence `B` ever sent one, and the holder's recovery right cannot depend on the
issuer's own bookkeeping.

---

## 5. The F6 fix

[`crates/thbc-core/src/exchange.rs`](crates/thbc-core/src/exchange.rs) replaces the
minting swap with an inventory exchange:

```rust
transfer_checked(grx_in,   user_grx_ata   -> swap_vault)     // user pays GRX
transfer_checked(thbc_out, thbc_inventory -> user_thbc_ata)  // platform pays THBC
// thbc_supply UNCHANGED — no mint, no burn
```

Pricing is identical to the on-chain `compute_swap_grx_for_thbc`
(`gridtokenx-anchor/programs/treasury/src/lib.rs:67`) so quotes match execution to the
minor unit. The one substantive difference: where the program checks
`new_supply ≤ attested_reserve`, this checks `thbc_out ≤ inventory`. **Reserve headroom
is not consumed at all.**

`ExchangeQuote` has no field that could express a supply change, so no caller can
request one — F6 holds by construction rather than by check.

The risk does not vanish. It moves onto the platform's balance sheet as GRX inventory
risk, which is the correct place for it (§7.2). `grx_per_thbc_rate` remains a disclosed
centralisation: a quoted market-maker rate against bounded inventory, not a peg. §7.3
rules out an AMM so the reference rate never becomes a market outcome.

**This fixes the off-chain half only.** `swap_grx_for_thbc` still calls `mint_to`
(`instructions/swap_grx_for_thbc.rs:97`) and `redeem_thbc_for_grx` still calls `burn`
(`instructions/redeem_thbc_for_grx.rs:71`). F6 remains `Violated` in the registry
until those change.

---

## 6. Trust boundaries

| Actor | Trusted for | Can it steal? |
| :-- | :-- | :-- |
| `U` user | nothing | — |
| `B` licensed issuer | fiat custody, honest issuance | **yes** — unbacked mint, or refuse redemption |
| `A` reserve attestor | reporting `R(t)` honestly | **yes** — inflating `R` lifts the F1 ceiling |
| `P` GridTokenX (this service) | **liveness only** | no (F8) |
| `E` regulator | nothing | no |

**The load-bearing assumption is `A`.** `attested_reserve` is a single `u64` written by
a single signer, and F1, F5, the peg claim and the solvency of every user balance all
reduce to that number being honest. Nothing here improves that; `ReserveService::attest`
logs an `error!` when an attestation fails to cover supply, which makes a lie visible
but does not prevent it. **T1 + T2 collusion is the single point of failure and no
mechanism in this design addresses it.**

### F8 is a property of `ports.rs`

No method on `LedgerPort`, `DepositRepository`, `RedemptionRepository`,
`CompliancePort` or `PayoutPort` accepts a private key, keypair, or signer. The
platform relays transactions the user already signed; it cannot construct a signer set
that moves user THBC. `sweep_reclaimable` deliberately *reports* overdue redemptions
rather than reclaiming them — reclaiming would need the holder's key. `P` can censor
(T4); it cannot steal.

---

## 7. What the Chain Bridge adapter refuses, and why

[`ChainBridgeLedger`](crates/thbc-ledger/src/chain_bridge.rs) returns
`PortError::Unsupported` → `501 not_implemented` for `issue`, `escrow_redemption`,
`confirm_redemption`, `reclaim_redemption` and `snapshot`.

That is correct, not lazy. The alternative — routing issuance to `swap_grx_for_thbc`,
which does exist — would produce a service that appears to implement the on-ramp while
minting against GRX collateral, violating F6 on every deposit. `Unsupported` is a
distinct variant from `Rejected` precisely so operators can tell "not built" from
"the chain said no".

`snapshot` refuses for a subtler reason: three of the four fields it returns
(`reserve_encumbered`, `thbc_inventory`, `redemption_queue_len`) are not on the
treasury account. Synthesising them would report a *tighter* F1 ceiling than the chain
actually enforces.

To exercise the payment leg end to end, run with `THBC_LEDGER_MODE=simulated` and read
[`SimulatedLedger`](crates/thbc-ledger/src/simulated.rs)'s module doc first.

---

## 8. Persistence

Own database (`gridtokenx_thbc`). No foreign key or JOIN to `users` or any other
service's table — the DB-per-service split is mid-flight and a new cross-service JOIN
is what makes it un-finishable.

Two deliberate deviations, both documented at their site:

1. **Runtime `sqlx` queries, not `query_as!`.** The macro needs a live `DATABASE_URL`
   at build time or a committed `.sqlx` cache, and this database has never been
   provisioned — with the macro, `cargo check` fails on a fresh clone. Temporary: run
   `cargo sqlx prepare` and migrate to the macro form once the DB exists.
2. **`THBC_DATABASE_URL`, not `DATABASE_URL`**, and `.env` is read from this directory
   only. The superproject's root `.env` sets `DATABASE_URL` to the shared `gridtokenx`
   database; `dotenvy::dotenv()` walks *up* and finds it. During development that
   caused this service to migrate `deposits` / `redemptions` /
   `reconciliation_runs` into the shared database. `Config::validate` now also rejects
   a URL whose database name is `gridtokenx`.

All money is `BIGINT` minor units — never `NUMERIC`, never `DOUBLE PRECISION`. F2 is
an exact identity and does not survive rounding.

---

## 9. Test coverage, honestly

`cargo test` — 149 tests, no infrastructure required.

| Invariant | §13 asks for | What actually runs |
| :-- | :-- | :-- |
| F1 | proptest over random issue/redeem | ✅ proptest + service integration |
| F2 | proptest over arbitrary interleaving | ✅ proptest + reconciler agreement |
| F3 | E2E, **LiteSVM** | ⚠️ service-level replay only — the nullifier PDA does not exist, so there is nothing to run LiteSVM against |
| F4 | service integration test | ✅ full, against a recording payout queue |
| F5 | **LiteSVM** | ⚠️ against the simulator |
| F6 | proptest + CI grep for `mint_to` in `exchange_*.rs` | ⚠️ proptest ✅; **no CI grep — there is no CI in this repo** |
| F7 | **LiteSVM** with clock warp | ⚠️ simulator with clock warp; the escrow does not exist on-chain |
| F8 | static audit + negative test | ✅ both |
| F9 | unit test | ✅ |

**Do not report F3, F5 or F7 as covered on the strength of this suite.** They exercise
`SimulatedLedger`, a model of the program *as specified*; for F3 and F7 the program
implements nothing to test. When the instructions land, these move to LiteSVM and the
simulator becomes a fixture.

There is no CI in this repo — every gate is manual. A green local run is the only
signal you get.

---

## 10. Running it

```bash
cd gridtokenx-thbc-service

cargo test                  # 149 tests, no infra
cargo clippy --all-targets -- -D warnings

cp .env.example .env        # then edit
THBC_LEDGER_MODE=simulated \
THBC_SIMULATED_RESERVE_MINOR=1000000000000 \
  cargo run --bin thbc-service
```

Never `cargo` from the repo root — each service is its own workspace.

Worth hitting first:

```bash
curl localhost:4008/v1/admin/invariants   # what is actually guaranteed
curl localhost:4008/v1/admin/reserve      # the F1 ceiling right now
curl localhost:4008/v1/admin/reconciliation
```

---

## 11. To make this real

In dependency order:

1. **Treasury state** (`gridtokenx-anchor/programs/treasury/src/state.rs`) — add
   `issuer`, `thbc_inventory`, `reserve_encumbered`, `redemption_queue_len`. The struct
   is `zero_copy` and hand-padded to 272 bytes; adding fields means re-padding and
   re-initialising.
2. **F6** — rewrite `swap_grx_for_thbc` / `redeem_thbc_for_grx` as inventory transfers.
   The off-chain math is already written and pinned to the current pricing.
3. **F3** — `issue_thbc(amount, bank_ref_hash)` creating `[b"deposit", H(bank_ref)]`
   with Anchor `init` in the **same instruction** as the mint. A separate transaction
   does not implement F3.
4. **F7** — `redeem_thbc_for_fiat` / `confirm_redemption` / `reclaim_redemption` with
   the Δ-timelocked escrow record.
5. **Adapters** — a real KYC provider (NDID) and a bank rail. Until then the compliance
   port refuses every screen in non-simulated mode rather than auto-passing.

§6.4 stays open regardless: if `B` takes the fiat and never wires, the holder recovers
their THBC and the reserve is short. That is fair exchange between an on-chain action
and an off-chain one, which has a known impossibility result without a trusted third
party. It is not a bug to be fixed here.
