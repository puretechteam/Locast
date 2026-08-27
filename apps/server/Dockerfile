# Locast signaling server - production image.
#
# Multi-stage build per docs/ARCHITECTURE.md section 26.3. The runtime image
# is a slim Debian base so that a shell is available for in-container
# debugging during P0-T03 and P9 deployment work. Distroless can be
# substituted later by adding a static `openssl`/musl build.

FROM rust:1.82-bookworm AS builder
WORKDIR /build

# Cache dependency builds: copy manifests first, build a dummy crate that
# resolves all dependencies, then build the real one.
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY apps/server/Cargo.toml apps/server/Cargo.toml
COPY shared/protocol/Cargo.toml shared/protocol/Cargo.toml
COPY shared/crypto/Cargo.toml shared/crypto/Cargo.toml
COPY shared/manifest/Cargo.toml shared/manifest/Cargo.toml

RUN mkdir -p apps/server/src shared/protocol/src shared/crypto/src shared/manifest/src \
    && echo "fn main() {}" > apps/server/src/main.rs \
    && echo "" > shared/protocol/src/lib.rs \
    && echo "" > shared/crypto/src/lib.rs \
    && echo "" > shared/manifest/src/lib.rs \
    && cargo build --release -p locast-server || true

COPY apps/server apps/server
COPY shared shared

RUN cargo build --release -p locast-server

FROM debian:bookworm-slim AS runtime
RUN apt-get update \
    && apt-get install --no-install-recommends -y ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*

# Non-root user per least-privilege defaults.
RUN useradd --system --uid 1000 --create-home --shell /usr/sbin/nologin locast
USER locast

WORKDIR /app
COPY --from=builder /build/target/release/locast-server /app/locast-server

ENV LOCAST_BIND_ADDR=0.0.0.0:8787 \
    LOCAST_DATABASE_URL=/var/lib/locast/locast-server.db \
    LOCAST_LOG=info

# Database directory; the runtime data is mounted here in compose.
RUN mkdir -p /var/lib/locast && chown locast:locast /var/lib/locast

EXPOSE 8787
ENTRYPOINT ["/app/locast-server"]
