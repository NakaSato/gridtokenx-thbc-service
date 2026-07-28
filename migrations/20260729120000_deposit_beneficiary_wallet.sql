-- Add the beneficiary's Solana owner wallet to deposits.
--
-- `beneficiary` is an IAM user id. Nothing on-chain can be derived from it, and
-- this service holds neither an IAM client nor a Solana toolchain (F8 — no key
-- here may move user funds), so it cannot resolve one to the other. Chain Bridge
-- cannot either: it has no IAM client. The partner therefore supplies the wallet
-- on the deposit webhook and it is stored here, so a deposit that lands in
-- `held` can be retried after the next attestation refresh without the partner
-- re-sending anything.
--
-- Chain Bridge derives the associated token account from this owner wallet under
-- the THBC mint's own token program. Storing the ATA instead would be strictly
-- worse: the on-chain account is constrained by `token::mint = thbc_mint` only,
-- which checks the mint and not the owner, so a supplied token account is an
-- unvalidated destination.
--
-- Backfilled with '' rather than left NULL so the column can be NOT NULL and the
-- read path needs no Option. Pre-existing rows cannot have a wallet — the field
-- did not exist when they were written — and an empty value fails the
-- `Deposit::observe` guard, so such a row can never be silently issued to a
-- garbage address. There are no issued rows to lose: nothing could issue before
-- this change (the ledger's `issue` returned Unsupported).
ALTER TABLE deposits
    ADD COLUMN IF NOT EXISTS beneficiary_wallet TEXT NOT NULL DEFAULT '';

-- No index: the wallet is carried, never queried on.
