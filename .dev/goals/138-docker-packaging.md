# Goal 138 — Docker packaging + container image release workflow

**Roadmap**: Phase 17.5 — Docker packaging + health probes

**Design principle check**:
- Implemented as: new repo-root `Dockerfile` + `.dockerignore` + new
  `.github/workflows/docker-image.yml`. No source-code changes.
- `/health` endpoint (g122 / src/http.rs) already exists and is exempt
  from auth (g135) — sufficient for k8s liveness/readiness probes.
- ❌ Does NOT modify any file under `src/`.
- ❌ Does NOT add any new Rust dependency.

## Why

Production deployments need a reproducible container image. The
existing `e2e/Dockerfile` is testing-only (sleep entrypoint, includes
debug tools). This goal ships a slim production image and an
automated pipeline that publishes it to GitHub Container Registry on
each release tag.

`/health` already returns `200 ok` (and is auth-exempt per g135),
so k8s liveness/readiness probes can target it directly without any
new endpoint. Splitting `/livez` vs `/readyz` is deferred — single
`/health` is the standard "I am alive AND I am ready" signal for a
stateless agent server.

## Scope (do exactly this, no more)

### 1. Production `Dockerfile` (repo root)

Multi-stage:

```dockerfile
# syntax=docker/dockerfile:1.7
ARG RUST_VERSION=1.86

FROM rust:${RUST_VERSION}-slim AS builder
WORKDIR /build

# Cache deps as a separate layer
COPY Cargo.toml Cargo.lock ./
COPY crates/ crates/
# Stub main to compile dependency tree first (cache-friendly)
RUN mkdir -p src && echo "fn main() {}" > src/main.rs && \
    cargo build --release -p recursive-agent --features http && \
    rm -rf src target/release/deps/recursive_agent* target/release/recursive*

# Real source build
COPY src/ src/
RUN touch src/main.rs && \
    cargo build --release -p recursive-agent --features http

# ── Runtime ─────────────────────────────────────────────────────────
FROM debian:bookworm-slim AS runtime

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Non-root user
RUN groupadd --system --gid 1000 recursive && \
    useradd --system --uid 1000 --gid 1000 --create-home --home-dir /home/recursive recursive

COPY --from=builder /build/target/release/recursive /usr/local/bin/recursive

USER recursive
WORKDIR /workspace

EXPOSE 3000
HEALTHCHECK --interval=30s --timeout=3s --start-period=5s --retries=3 \
    CMD ["sh", "-c", "wget -qO- http://127.0.0.1:3000/health || exit 1"]

ENTRYPOINT ["/usr/local/bin/recursive"]
CMD ["http", "--addr", "0.0.0.0:3000"]
```

Notes embedded in the Dockerfile itself (as comments) explain:
- The "stub main + restore" trick caches the dependency layer.
- Why we use `debian:bookworm-slim` vs distroless: we need
  `ca-certificates` for outbound HTTPS (LLM providers); distroless's
  static variant has no shell for HEALTHCHECK; the GLIBC variant
  works but adds complexity. `bookworm-slim` is ~75 MB and trivial
  to keep updated.
- Non-root user 1000 — k8s securityContext default.
- HEALTHCHECK uses wget (already in `bookworm-slim`); avoids adding
  `curl` for one purpose.

### 2. `.dockerignore` (repo root)

Mirror `.gitignore` plus exclude things that should never enter the
build context:

```
target/
.git/
.dev/
.github/
.worktrees/
.recursive/
.codebuddy/
.cursor/
.idea/
.vscode/
.dockerignore
Dockerfile
docs/
examples/
e2e/
tests/
**/*.md
.env
.envrc*
**/.DS_Store
```

Rationale: anything not needed inside the build context just slows
the `docker build` start. `target/` is the obvious one. `.git/` saves
~tens of MB. `e2e/` and `tests/` are not needed because we build with
`--release` and skip tests in the image build. `**/*.md` skips docs.

### 3. GitHub Actions workflow `.github/workflows/docker-image.yml`

Trigger: same as `release.yml` — on `v*.*.*` tags. (Plus a
`workflow_dispatch` for manual trigger.)

```yaml
name: Build and Publish Docker Image

on:
  push:
    tags:
      - 'v*.*.*'
  workflow_dispatch:

jobs:
  docker:
    runs-on: ubuntu-latest
    permissions:
      contents: read
      packages: write
    steps:
      - uses: actions/checkout@v4
      - uses: docker/setup-qemu-action@v3
      - uses: docker/setup-buildx-action@v3
      - uses: docker/login-action@v3
        with:
          registry: ghcr.io
          username: ${{ github.actor }}
          password: ${{ secrets.GITHUB_TOKEN }}
      - name: Extract metadata
        id: meta
        uses: docker/metadata-action@v5
        with:
          images: ghcr.io/${{ github.repository_owner }}/recursive
          tags: |
            type=ref,event=tag
            type=semver,pattern={{version}}
            type=semver,pattern={{major}}.{{minor}}
            type=raw,value=latest,enable={{is_default_branch}}
      - uses: docker/build-push-action@v6
        with:
          context: .
          platforms: linux/amd64,linux/arm64
          push: true
          tags: ${{ steps.meta.outputs.tags }}
          labels: ${{ steps.meta.outputs.labels }}
          cache-from: type=gha
          cache-to: type=gha,mode=max
```

### 4. Local validation

Run `docker build --tag recursive:test .` from the repo root and
confirm the image builds. Smoke-test by running it against `--help`:

```sh
docker run --rm recursive:test --help
```

(The agent binary's `--help` does not need network or API keys.)

Then confirm the HTTP server starts:

```sh
docker run --rm -d --name recursive-smoke -p 13000:3000 recursive:test
# wait briefly
sleep 3
curl -fsS http://127.0.0.1:13000/health   # expect "ok"
docker stop recursive-smoke
```

Both checks are documented in the journal but not gated by CI here
(GHA's docker build validates by building; runtime smoke-test is the
operator's responsibility on first deploy).

## Acceptance

- `docker build .` succeeds locally on macOS arm64 (colima or
  Docker Desktop). Image size <300 MB.
- `docker run --rm <tag> --help` exits 0 and prints the CLI help.
- `docker run --rm -d -p 13000:3000 <tag>` starts; `curl
  http://127.0.0.1:13000/health` returns `ok`.
- The workflow file passes `actionlint` parsing (manual: just review
  YAML correctness).
- `cargo build` and `cargo test` are unaffected (we don't touch
  Rust sources).
- No new Rust dependency; no changes to `src/`.

## Notes

- `EXPOSE 3000` matches the recursive HTTP server's default `--addr
  0.0.0.0:3000`. Operators can override via CMD.
- The image runs as user 1000. If a deployment mounts a host
  directory at `/workspace`, that directory must be writable by uid
  1000 (or use named Docker volumes which inherit the right perms).
- multi-platform build (amd64 + arm64) via QEMU emulation. The first
  build of arm64 in CI will be slow; cache-from gha mitigates
  subsequent builds.
- The image is published as `ghcr.io/<owner>/recursive`, tagged with
  the semver tag, the `<major>.<minor>` rolling tag, and `latest` if
  the tag is on the default branch.
