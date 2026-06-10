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
#   BUILD_PROFILE - release (default) or release-lto for fat-LTO

ARG RUST_VERSION=1.94.1
ARG BUILD_PROFILE=release

FROM rust:${RUST_VERSION}-alpine AS chef

RUN apk add --no-cache musl-dev \
    && cargo install cargo-chef --locked --version 0.1.71 \
    && rm -rf "$CARGO_HOME/registry"

WORKDIR /build

FROM chef AS planner

COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates/ crates/

RUN cargo chef prepare --recipe-path recipe.json --bin tau

FROM chef AS builder

ARG BUILD_PROFILE

COPY --from=planner /build/recipe.json recipe.json
RUN cargo chef cook --profile ${BUILD_PROFILE} --bin tau --recipe-path recipe.json

COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates/ crates/

RUN cargo build --profile ${BUILD_PROFILE} --bin tau \
    && strip target/${BUILD_PROFILE}/tau \
    && cp target/${BUILD_PROFILE}/tau /tau

FROM scratch

# OCI annotations: GHCR uses image.source to link the package to the GitHub
# repository, which is what enables the "Inherit access from source repository"
# toggle and lets a public repo's package automatically resolve to public.
LABEL org.opencontainers.image.source="https://github.com/bxrne/tau"
LABEL org.opencontainers.image.url="https://github.com/bxrne/tau"
LABEL org.opencontainers.image.description="Tau time-series database server (scratch musl static)."
LABEL org.opencontainers.image.licenses="LicenseRef-PolyForm-Noncommercial-1.0.0"
LABEL org.opencontainers.image.title="tau"
LABEL org.opencontainers.image.documentation="https://github.com/bxrne/tau/blob/master/README.md"

COPY --from=builder /tau /tau

EXPOSE 7070
EXPOSE 9100

VOLUME ["/data"]

ENTRYPOINT ["/tau"]
CMD ["--config", "/data/tau-config.toml"]
