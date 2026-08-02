# syntax=docker/dockerfile:1.4
# Three-stage build using cargo-chef to cache dependency compilation.
# The `cooker` stage compiles all 588 dependency crates from a recipe
# (generated without source) — it only re-runs when Cargo.toml/Cargo.lock
# change. The `builder` stage copies real source + the prebuilt target/
# dir and compiles only the 3 workspace crates, so a src/ change costs
# ~2-3 min instead of a full ~10 min rebuild.

# ── Stage 1: planner — generate the dependency recipe (NO source needed) ──
# cargo chef prepare only reads manifests (Cargo.toml/Cargo.lock/.cargo/) — it
# does NOT need src/. Omitting `COPY src` keeps this layer CACHED across src/
# changes, so the downstream cooker layer (which COPYs recipe.json) also stays
# CACHED. This is what makes worktree builds fast: the planner+cooker chain is
# identical for any worktree of the same manifests.
FROM rust:1.88-slim AS planner
WORKDIR /app
RUN cargo install cargo-chef --locked 2>&1 | tail -1
COPY Cargo.toml Cargo.lock ./
COPY .cargo .cargo
COPY crates crates
RUN cargo chef prepare --recipe-path recipe.json

# ── Stage 2: cooker — compile dependencies from the recipe (cached layer) ──
# This layer only rebuilds when recipe.json changes (i.e. Cargo.toml/lock).
# Source changes do NOT invalidate it — that is the whole point.
FROM rust:1.88-slim AS cooker
WORKDIR /app
RUN cargo install cargo-chef --locked 2>&1 | tail -1
COPY --from=planner /app/recipe.json recipe.json
RUN cargo chef cook --release --recipe-path recipe.json 2>&1 | tail -5

# ── Stage 3: builder — compile the actual project on top of cached deps ──
FROM rust:1.88-slim AS builder
WORKDIR /app
COPY Cargo.toml Cargo.lock providers.toml ./
COPY .cargo .cargo
COPY crates crates
COPY src src
# Copy the prebuilt target/ from the cooker stage — cargo sees the
# dependencies as already compiled and only builds the 3 workspace crates.
COPY --from=cooker /app/target target
RUN cargo build --release -p recursive-cli 2>&1 | tail -5

FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    python3 \
    python3-pip \
    nodejs \
    curl \
    jq \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/target/release/recursive /usr/local/bin/recursive

# Install Python SDK (for 20-python-sdk e2e test)
COPY sdk/python /sdk/python
RUN pip3 install --break-system-packages /sdk/python

# Copy TypeScript SDK dist (pre-built; no npm install needed — zero runtime deps)
COPY sdk/typescript/dist /sdk/typescript/dist
COPY sdk/typescript/package.json /sdk/typescript/package.json

# Default workspace
WORKDIR /workspace

# Keep container alive for docker exec
ENTRYPOINT ["sleep", "infinity"]
