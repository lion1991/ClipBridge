# syntax=docker/dockerfile:1.7
# Multi-stage build: compile in a Rust toolchain image, ship the stripped
# binary in a distroless base. Final image is ~25MB.

FROM rust:1.92-slim-bookworm AS builder
WORKDIR /src

# Install only the bits we need to build (musl-tools omitted; we link glibc).
RUN apt-get update && apt-get install -y --no-install-recommends \
        pkg-config libssl-dev ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Cache deps across rebuilds: copy manifests first, fetch, then sources.
COPY Cargo.toml Cargo.lock ./
COPY core/Cargo.toml core/Cargo.toml
COPY relay/Cargo.toml relay/Cargo.toml
COPY tools/uniffi-bindgen/Cargo.toml tools/uniffi-bindgen/Cargo.toml
RUN mkdir -p core/src relay/src tools/uniffi-bindgen/src \
    && echo 'pub fn _stub() {}' > core/src/lib.rs \
    && echo 'fn main() {}' > relay/src/main.rs \
    && echo 'pub fn _stub() {}' > relay/src/lib.rs \
    && echo 'fn main() {}' > tools/uniffi-bindgen/src/main.rs \
    && cargo build --release -p clipbridge-relay \
    && rm -rf core relay tools

# Real sources.
COPY core ./core
COPY relay ./relay
COPY tools ./tools

# Force a rebuild of our crates (deps are already cached).
RUN touch core/src/lib.rs relay/src/main.rs \
    && cargo build --release -p clipbridge-relay

FROM gcr.io/distroless/cc-debian12:nonroot
WORKDIR /app
COPY --from=builder /src/target/release/clipbridge-relay /app/clipbridge-relay

# Default bind. Override with -e CLIPBRIDGE_BIND=0.0.0.0:PORT if needed.
ENV CLIPBRIDGE_BIND=0.0.0.0:8787 RUST_LOG=info
EXPOSE 8787

USER nonroot
ENTRYPOINT ["/app/clipbridge-relay"]
