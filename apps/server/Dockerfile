# Locast signaling server - production image.
#
# Multi-stage build per docs/ARCHITECTURE.md section 26.3. The runtime image
# is a slim Debian base so that a shell is available for in-container
# debugging during P0-T03 and P9 deployment work. Distroless can be
# substituted later by adding a static `openssl`/musl build.
#
# Section 21.17 hardening (asserted by the `hardening` CI job, P8-T08):
# the binary is built with the hardening RUSTFLAGS below, and the
# container runs as a non-root user.

FROM rust:1.82-bookworm AS builder
WORKDIR /build

# Section 21.17 hardening flags. Builds pass an explicit `--target` so
# these apply to the server binary only, not to build scripts or proc
# macros (which cannot be built with `+crt-static`).
ENV RUSTFLAGS="-C target-feature=+crt-static -C link-arg=-fstack-protector-strong"

# Cache dependency builds: copy manifests first, build a dummy crate that
# resolves all dependencies, then build the real one.
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY apps/server/Cargo.toml apps/server/Cargo.toml
COPY shared/protocol/Cargo.toml shared/protocol/Cargo.toml
COPY shared/crypto/Cargo.toml shared/crypto/Cargo.toml
COPY shared/manifest/Cargo.toml shared/manifest/Cargo.toml

# The desktop client is a workspace member but is not part of this build
# context; drop it from the members list so Cargo can load the workspace.
RUN sed -i '\#"apps/client/src-tauri",#d' Cargo.toml

RUN mkdir -p apps/server/src shared/protocol/src shared/crypto/src shared/manifest/src \
    && echo "fn main() {}" > apps/server/src/main.rs \
    && echo "" > shared/protocol/src/lib.rs \
    && echo "" > shared/crypto/src/lib.rs \
    && echo "" > shared/manifest/src/lib.rs \
    && cargo build --release -p locast-server --target "$(rustc -vV | sed -n 's/^host: //p')" || true

COPY apps/server apps/server
COPY shared shared

RUN TARGET="$(rustc -vV | sed -n 's/^host: //p')" \
    && cargo build --release -p locast-server --target "$TARGET" \
    && cp "target/$TARGET/release/locast-server" /build/locast-server

FROM debian:bookworm-slim AS runtime
RUN apt-get update \
    && apt-get install --no-install-recommends -y ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*

# Non-root user per least-privilege defaults. The data directory is
# created (as root) before switching users.
RUN useradd --system --uid 1000 --create-home --shell /usr/sbin/nologin locast \
    && mkdir -p /var/lib/locast \
    && chown locast:locast /var/lib/locast
USER locast

WORKDIR /app
COPY --from=builder /build/locast-server /app/locast-server

ENV LOCAST_BIND_ADDR=0.0.0.0:8787 \
    LOCAST_DATABASE_URL=/var/lib/locast/locast-server.db \
    LOCAST_LOG=info

EXPOSE 8787
ENTRYPOINT ["/app/locast-server"]
