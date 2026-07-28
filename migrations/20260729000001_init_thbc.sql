-- THBC settlement service — initial schema.
--
-- Own database (`gridtokenx_thbc`). No foreign key to `users` or any other
-- service's table: the DB-per-service split is mid-flight and a new cross-service
-- JOIN is what makes it un-finishable. `beneficiary` and `user_id` hold the IAM
-- user id as an opaque string, resolved over the wire when a name is needed.
--
-- All money is BIGINT minor units (6 decimals). Never NUMERIC, never DOUBLE
-- PRECISION: F2 is an exact identity and does not survive rounding.

-- ---------------------------------------------------------------------------
-- Deposits (on-ramp, spec §5)
-- ---------------------------------------------------------------------------

CREATE TABLE IF NOT EXISTS deposits (
    -- H(bank_ref) as lowercase hex — the same digest that seeds the on-chain
    -- nullifier PDA [b"deposit", H(bank_ref)]. PRIMARY KEY, so a replayed webhook
    -- fails on insert.
    --
    -- This is the OFF-CHAIN half of F3 and it is not the guarantee. It stops
    -- replays that arrive through this service; it does nothing about a second
    -- issuer path, a manual transaction, or this service being run twice against
    -- two databases. The account-level guarantee needs the nullifier PDA, which
    -- does not exist on-chain (spec §12).
    bank_ref_hash   CHAR(64)    PRIMARY KEY,

    -- The bank's own reference, normalised (trimmed, upper-cased) before hashing.
    -- Stored for reconciliation against the bank statement. Correlates a user to a
    -- transfer, so it is never logged.
    bank_ref        TEXT        NOT NULL,

    amount_minor    BIGINT      NOT NULL CHECK (amount_minor > 0),
    beneficiary     TEXT        NOT NULL,

    state           TEXT        NOT NULL
        CHECK (state IN ('observed', 'screened', 'attested', 'issued', 'encumbered', 'disputed')),

    observed_at     BIGINT      NOT NULL,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- `total_encumbered` and `total_issued` are the hot reads (they feed the F1 ceiling
-- and the F2 identity on every reconcile).
CREATE INDEX IF NOT EXISTS deposits_state_idx ON deposits (state);

-- Reconciliation looks deposits up by the bank's reference, not by the digest.
CREATE INDEX IF NOT EXISTS deposits_bank_ref_idx ON deposits (bank_ref);

-- ---------------------------------------------------------------------------
-- Redemptions (off-ramp, spec §6)
-- ---------------------------------------------------------------------------

CREATE TABLE IF NOT EXISTS redemptions (
    -- Mirrors the on-chain record PDA [b"redeem", user, seq].
    user_id         TEXT        NOT NULL,
    seq             BIGINT      NOT NULL CHECK (seq > 0),

    amount_minor    BIGINT      NOT NULL CHECK (amount_minor > 0),

    state           TEXT        NOT NULL
        CHECK (state IN ('requested', 'escrowed', 'payout_queued', 'confirmed', 'reclaimed', 'failed')),

    requested_at    BIGINT      NOT NULL,

    -- When the escrow CONFIRMED on-chain. NULL until then. The delta clock runs from
    -- this column, not from requested_at: time spent waiting for confirmation is not
    -- the issuer's to spend. NULL here is also what makes the F4 barrier checkable in
    -- SQL — no confirmation, no payout.
    escrowed_at     BIGINT,

    delta_secs      BIGINT      NOT NULL CHECK (delta_secs > 0),

    created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),

    PRIMARY KEY (user_id, seq),

    -- F4, as a table constraint. A row cannot claim a payout was queued, or a burn
    -- confirmed, without an escrow confirmation timestamp. `next_seq` is racy by
    -- design and the PK absorbs that; this constraint absorbs a logic bug that tries
    -- to wire before the burn lands.
    CONSTRAINT redemption_payout_requires_confirmed_escrow CHECK (
        state IN ('requested', 'failed') OR escrowed_at IS NOT NULL
    )
);

-- The delta sweep: pending redemptions ordered by escrow time.
CREATE INDEX IF NOT EXISTS redemptions_pending_idx
    ON redemptions (escrowed_at)
    WHERE state IN ('escrowed', 'payout_queued');

-- ---------------------------------------------------------------------------
-- Reconciliation history (F2, spec §9)
-- ---------------------------------------------------------------------------

CREATE TABLE IF NOT EXISTS reconciliation_runs (
    id                  BIGSERIAL   PRIMARY KEY,
    checked_at          BIGINT      NOT NULL,
    severity            TEXT        NOT NULL CHECK (severity IN ('ok', 'drift', 'insolvent')),

    -- Signed: positive means the ledger holds more THBC than we issued — the
    -- unbacked-mint direction.
    drift               BIGINT      NOT NULL,

    expected_supply     BIGINT      NOT NULL,
    ledger_supply       BIGINT      NOT NULL,
    free_backing        BIGINT      NOT NULL,
    shortfall           BIGINT      NOT NULL,

    created_at          TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- Append-only history, so a breach that was later resolved is still visible to the
-- regulator observer. Queried newest-first.
CREATE INDEX IF NOT EXISTS reconciliation_runs_checked_at_idx
    ON reconciliation_runs (checked_at DESC);

CREATE INDEX IF NOT EXISTS reconciliation_runs_unhealthy_idx
    ON reconciliation_runs (checked_at DESC)
    WHERE severity <> 'ok';
