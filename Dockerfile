# syntax=docker/dockerfile:1.7
#
# Production container image for the Recursive coding agent.
#
# Multi-stage:
#   1. `builder` — full Rust toolchain, builds release binary with
#      the `http` feature so the container can serve the HTTP API.
#   2. `runtime` — debian:bookworm-slim, ca-certificates only,
#      non-root user, single binary copied in.
#
# Why bookworm-slim instead of distroless: ca-certificates is required
# for outbound HTTPS to LLM providers; distroless's static variant has
# no shell for HEALTHCHECK; the debian variant of distroless adds
# complexity without much size savings vs bookworm-slim (~75 MB).

# Track `stable` rather than pinning a specific version: the recursive-cli
# build picks up indirect deps (e.g. time@0.3.47) whose MSRV moves with
# stable, so a pinned rust version in this Dockerfile silently breaks
# every time a transitive dep bumps its minimum. The release workflow
# (`.github/workflows/release.yml`) uses dtolnay/rust-toolchain@stable,
# so the CI verification and the published image stay in lockstep.
#
# Note: Docker Hub's `rust:stable-slim` tag does NOT exist (the official
# image only ships `slim-bookworm` → `latest`, plus `1.x-slim` and `1-slim`
# that track stable). `1-slim` recently started tracking Debian 13 (trixie)
# with GLIBC 2.41, but the runtime stage uses debian:bookworm-slim (GLIBC 2.36).
# Pinning to `1-slim-bookworm` keeps the same Rust stable tracking while matching
# the runtime's GLIBC, avoiding a "version `GLIBC_2.39` not found" crash at startup.
FROM rust:1-slim-bookworm AS builder

WORKDIR /build

# Cargo metadata + workspace crates first — these change rarely and
# Docker layer caching keeps the dep download/compile out of most
# rebuilds even without a cargo-chef-style stub.
COPY Cargo.toml Cargo.lock ./
# `providers.toml` is referenced from `src/providers.rs` via
# `include_str!("../providers.toml")` — a compile-time include that
# cargo will not find unless we copy the file into the build context
# alongside the workspace metadata.
COPY providers.toml ./
COPY crates/ crates/

# Real source. Examples and tests are excluded via .dockerignore so
# they don't slow the build context. We use `--bin recursive` to skip
# building the examples and bench targets (faster, smaller layer).
COPY src/ src/

# BuildKit cache mounts speed repeated builds significantly without
# committing those caches into the final image. `sharing=locked`
# serialises concurrent access — required for multi-arch builds
# (linux/amd64 + linux/arm64 run in parallel inside one buildx call)
# where the default `shared` mode races on unpacking the same crate
# and fails with "File exists (os error 17)" on
# `.../blake3-1.8.5/.cargo-ok` (and similar).
RUN --mount=type=cache,target=/build/target,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git,sharing=locked \
    cargo build --release -p recursive-cli --features http --bin recursive && \
    cp target/release/recursive /tmp/recursive

# ──────────────────────────────────────────────────────────────────────
# Stage 2: runtime
# ──────────────────────────────────────────────────────────────────────
FROM debian:bookworm-slim AS runtime

# Only what we need to run a Rust binary that talks HTTPS.
# `wget` is preinstalled in bookworm-slim; we use it for HEALTHCHECK.
RUN apt-get update && \
    apt-get install -y --no-install-recommends ca-certificates && \
    rm -rf /var/lib/apt/lists/*

# Non-root user — k8s securityContext default. uid/gid 1000 matches
# the conventional "first user" id used by most base images.
RUN groupadd --system --gid 1000 recursive && \
    useradd --system --uid 1000 --gid 1000 \
            --create-home --home-dir /home/recursive \
            recursive

COPY --from=builder /tmp/recursive /usr/local/bin/recursive

USER recursive
WORKDIR /workspace

# Default port for the recursive HTTP server. Operators may override
# the address via CMD overrides.
EXPOSE 3000

# Liveness probe target: the /health endpoint is auth-exempt (g135)
# and returns 200 "ok" once the server has bound its listener.
HEALTHCHECK --interval=30s --timeout=3s --start-period=5s --retries=3 \
    CMD wget -qO- http://127.0.0.1:3000/health || exit 1

ENTRYPOINT ["/usr/local/bin/recursive"]
CMD ["http", "--addr", "0.0.0.0:3000"]
