# THBC settlement service.
#
# ⚠️ The build context is the SUPERPROJECT ROOT, not this directory.
#
# `thbc-ledger` depends on `../gridtokenx-blockchain-core/crates/blockchain-types`
# for the `chain.tx.*` wire schema and envelope signing, so cargo must be able to
# resolve that sibling path. Build with:
#
#     docker build -f gridtokenx-thbc-service/Dockerfile .
#
# (compose does this via `context: ./` + `dockerfile: gridtokenx-thbc-service/...`,
# the same shape aggregator-bridge uses.) Every COPY below is therefore prefixed
# with the submodule directory.
#
# Sharing the schema crate does NOT make this service chain-heavy:
# `blockchain-types` pulls in no solana-sdk, anchor, SPL or tonic. See the
# dependency comment in Cargo.toml.

FROM rust:1.97-slim AS builder

RUN apt-get update && apt-get install -y --no-install-recommends \
        pkg-config libssl-dev ca-certificates \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build

# The sibling schema crate must be present before the manifest pass, or cargo
# cannot resolve the path dependency. It is small and changes rarely, so it goes
# in its own early layer.
COPY gridtokenx-blockchain-core/crates/blockchain-types \
     gridtokenx-blockchain-core/crates/blockchain-types

WORKDIR /build/gridtokenx-thbc-service

# Manifests first, so the dependency layer caches across source edits.
COPY gridtokenx-thbc-service/Cargo.toml gridtokenx-thbc-service/Cargo.lock ./
COPY gridtokenx-thbc-service/crates/thbc-core/Cargo.toml        crates/thbc-core/
COPY gridtokenx-thbc-service/crates/thbc-ledger/Cargo.toml      crates/thbc-ledger/
COPY gridtokenx-thbc-service/crates/thbc-persistence/Cargo.toml crates/thbc-persistence/
COPY gridtokenx-thbc-service/crates/thbc-logic/Cargo.toml       crates/thbc-logic/
COPY gridtokenx-thbc-service/crates/thbc-api/Cargo.toml         crates/thbc-api/
COPY gridtokenx-thbc-service/bin/thbc-service/Cargo.toml        bin/thbc-service/

RUN mkdir -p crates/thbc-core/src crates/thbc-ledger/src crates/thbc-persistence/src \
             crates/thbc-logic/src crates/thbc-api/src bin/thbc-service/src \
    && touch crates/thbc-core/src/lib.rs crates/thbc-ledger/src/lib.rs \
             crates/thbc-persistence/src/lib.rs crates/thbc-logic/src/lib.rs \
             crates/thbc-api/src/lib.rs \
    && echo 'fn main() {}' > bin/thbc-service/src/main.rs \
    && cargo build --release --bin thbc-service \
    && rm -rf crates bin

COPY gridtokenx-thbc-service/crates crates
COPY gridtokenx-thbc-service/bin bin
COPY gridtokenx-thbc-service/migrations migrations

# `migrations/` is embedded by `sqlx::migrate!` at compile time, so it must be
# present here even though the runtime image also carries a copy for reference.
# Touch the roots so cargo rebuilds them rather than reusing the stub artifacts.
RUN touch crates/*/src/lib.rs bin/thbc-service/src/main.rs \
    && cargo build --release --bin thbc-service

FROM debian:bookworm-slim AS runtime

RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*

# Non-root. This service reaches a bank webhook ingress and a payment ledger; there is
# no reason for it to run as root.
RUN useradd --system --create-home --uid 10001 thbc
USER thbc
WORKDIR /home/thbc

COPY --from=builder /build/gridtokenx-thbc-service/target/release/thbc-service \
     /usr/local/bin/thbc-service
COPY --from=builder /build/gridtokenx-thbc-service/migrations ./migrations

# 4000s = gateways. Sits behind APISIX, which terminates JWT / mTLS / SSO —
# publishing this port directly exposes the admin surface.
EXPOSE 4008

# `/health` is liveness only. `/ready` reports `issuance_open`, which is false on a
# stale attestation or exhausted headroom — both expected operating states, not
# faults, so readiness must not gate on it or the regulator read surface goes down
# with issuance.
HEALTHCHECK --interval=30s --timeout=3s --start-period=10s --retries=3 \
    CMD curl -fsS http://localhost:4008/health || exit 1

ENTRYPOINT ["/usr/local/bin/thbc-service"]
