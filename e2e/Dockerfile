FROM rust:1.86-slim AS builder

WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src/ src/
COPY examples/ examples/
COPY crates/ crates/

# Build release binary
RUN cargo build --release -p recursive-agent 2>&1 | tail -5

FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    python3 \
    python3-pip \
    curl \
    jq \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /build/target/release/recursive /usr/local/bin/recursive

# Install Python SDK (for 20-python-sdk e2e test)
COPY sdk/python /sdk/python
RUN pip3 install --break-system-packages /sdk/python

# Default workspace
WORKDIR /workspace

# Keep container alive for docker exec
ENTRYPOINT ["sleep", "infinity"]
