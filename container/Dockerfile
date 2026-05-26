# Tau DB production Dockerfile.
#
# Multi-stage build:
#   chef    - rust + musl toolchain + cargo-chef
#   planner - derives recipe.json from Cargo manifests
#   builder - cooks dependency cache, then compiles a fully static musl binary
#   runtime - scratch image with only the stripped binary
#
# Build args:
#   RUST_VERSION  - must match rust-toolchain.toml (default: 1.94.1)
#   BUILD_PROFILE - release (default) or release-bench for fat-LTO

ARG RUST_VERSION=1.94.1
ARG BUILD_PROFILE=release

FROM rust:${RUST_VERSION}-alpine AS chef

RUN apk add --no-cache musl-dev \
    && cargo install cargo-chef --locked --version 0.1.71 \
    && rm -rf "$CARGO_HOME/registry"

WORKDIR /build

FROM chef AS planner

COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY src/ src/

RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder

ARG BUILD_PROFILE

COPY --from=planner /build/recipe.json recipe.json
RUN cargo chef cook --profile ${BUILD_PROFILE} --bin tau --recipe-path recipe.json

COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY src/ src/

RUN cargo build --profile ${BUILD_PROFILE} --bin tau \
    && strip target/${BUILD_PROFILE}/tau \
    && cp target/${BUILD_PROFILE}/tau /tau

FROM scratch

COPY --from=builder /tau /tau

EXPOSE 7070
EXPOSE 9100

VOLUME ["/data"]

ENTRYPOINT ["/tau"]
CMD ["0.0.0.0:7070", "--wal", "-w", "/data/tau.wal", "--users-file", "/data/users.json", "--metrics-port", "9100", "--log-level", "info"]
