# THBC settlement service.
#
# Self-contained: unlike most services here it does NOT depend on sibling submodule
# paths (`../gridtokenx-telemetry`, `../gridtokenx-blockchain-core`), so the build
# context is this directory alone.

FROM rust:1.97-slim AS builder

RUN apt-get update && apt-get install -y --no-install-recommends \
        pkg-config libssl-dev ca-certificates \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build

# Manifests first, so the dependency layer caches across source edits.
COPY Cargo.toml Cargo.lock ./
COPY crates/thbc-core/Cargo.toml       crates/thbc-core/
COPY crates/thbc-ledger/Cargo.toml     crates/thbc-ledger/
COPY crates/thbc-persistence/Cargo.toml crates/thbc-persistence/
COPY crates/thbc-logic/Cargo.toml      crates/thbc-logic/
COPY crates/thbc-api/Cargo.toml        crates/thbc-api/
COPY bin/thbc-service/Cargo.toml       bin/thbc-service/

RUN mkdir -p crates/thbc-core/src crates/thbc-ledger/src crates/thbc-persistence/src \
             crates/thbc-logic/src crates/thbc-api/src bin/thbc-service/src \
    && touch crates/thbc-core/src/lib.rs crates/thbc-ledger/src/lib.rs \
             crates/thbc-persistence/src/lib.rs crates/thbc-logic/src/lib.rs \
             crates/thbc-api/src/lib.rs \
    && echo 'fn main() {}' > bin/thbc-service/src/main.rs \
    && cargo build --release --bin thbc-service \
    && rm -rf crates bin

COPY crates crates
COPY bin bin
COPY migrations migrations

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

COPY --from=builder /build/target/release/thbc-service /usr/local/bin/thbc-service
COPY --from=builder /build/migrations ./migrations

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
