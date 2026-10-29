# syntax=docker/dockerfile:1.7
# Converge image. Build context is the repo root:
#   docker compose -f docker/docker-compose-build.yml build
#
# Cross-compiles, never emulates: the builder runs on $BUILDPLATFORM and only
# targets $TARGETARCH (same approach as slopstream-server, kagami-web and quantifyme-v2).

FROM --platform=$BUILDPLATFORM rust:1.92-bookworm AS builder
ARG TARGETARCH
WORKDIR /app

RUN set -eux; \
    case "$TARGETARCH" in \
      amd64) triple=x86_64-unknown-linux-gnu; cross="" ;; \
      arm64) triple=aarch64-unknown-linux-gnu; cross="gcc-aarch64-linux-gnu libc6-dev-arm64-cross" ;; \
      *) echo "unsupported TARGETARCH=$TARGETARCH" >&2; exit 1 ;; \
    esac; \
    echo "$triple" > /target-triple; \
    if [ -n "$cross" ]; then apt-get update; apt-get install -y --no-install-recommends $cross; rm -rf /var/lib/apt/lists/*; fi; \
    rustup target add "$triple"

ENV CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc \
    CC_aarch64_unknown_linux_gnu=aarch64-linux-gnu-gcc \
    AR_aarch64_unknown_linux_gnu=aarch64-linux-gnu-ar

# Dependency-cache layer: real manifests, dummy sources. Versions are
# neutralised so a release bump does not invalidate the whole crate graph.
# EVERY workspace member's manifest must be copied here: cargo loads the
# whole workspace first. CI checks this.
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates/core/Cargo.toml crates/core/
COPY crates/server/Cargo.toml crates/server/
RUN set -eux; \
    sed -i '/^\[workspace.package\]/,/^\[/ s/^version = .*/version = "0.0.0"/' Cargo.toml; \
    mkdir -p crates/core/src crates/server/src; \
    echo '' > crates/core/src/lib.rs; \
    echo '' > crates/server/src/lib.rs; \
    echo 'fn main(){}' > crates/server/src/main.rs; \
    cargo build --release -p converge-server --target "$(cat /target-triple)"; \
    rm -rf crates/*/src

# Real build. Migrations, templates and static assets are embedded at compile time.
COPY . .
RUN set -eux; \
    triple="$(cat /target-triple)"; \
    touch crates/core/src/lib.rs crates/server/src/lib.rs crates/server/src/main.rs; \
    cargo build --release -p converge-server --target "$triple"; \
    cp "target/$triple/release/converge-server" /converge-server

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd -g 1000 converge \
    && useradd -u 1000 -g converge -M -s /usr/sbin/nologin converge
COPY --from=builder /converge-server /usr/local/bin/converge-server
# No volume: everything the server keeps lives in Postgres.
USER converge
EXPOSE 8080
CMD ["converge-server", "serve"]
