FROM rust:1.82-slim AS builder

WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src/ src/
COPY examples/ examples/

# Build release binary
RUN cargo build --release --features http 2>&1 | tail -5

FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /build/target/release/recursive /usr/local/bin/recursive

# Default workspace
WORKDIR /workspace

ENTRYPOINT ["recursive"]
