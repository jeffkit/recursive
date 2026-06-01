#!/usr/bin/env sh
# Recursive Agent installer
# Usage: curl -fsSL https://raw.githubusercontent.com/recursive-agent/recursive/main/install.sh | sh
# Or:    curl -fsSL https://raw.githubusercontent.com/recursive-agent/recursive/main/install.sh | sh -s -- --version v0.6.0

set -e

REPO="recursive-agent/recursive"
BINARY="recursive"
INSTALL_DIR="${RECURSIVE_INSTALL_DIR:-/usr/local/bin}"

# ── helpers ────────────────────────────────────────────────────────────────

info()  { printf '\033[0;34minfo\033[0m  %s\n' "$*"; }
ok()    { printf '\033[0;32m ok  \033[0m %s\n' "$*"; }
err()   { printf '\033[0;31merror\033[0m %s\n' "$*" >&2; exit 1; }
warn()  { printf '\033[0;33mwarn \033[0m %s\n' "$*" >&2; }

need_cmd() { command -v "$1" >/dev/null 2>&1 || err "required command not found: $1"; }

# ── argument parsing ────────────────────────────────────────────────────────

VERSION=""
while [ $# -gt 0 ]; do
    case "$1" in
        --version) VERSION="$2"; shift 2 ;;
        --install-dir) INSTALL_DIR="$2"; shift 2 ;;
        --help|-h)
            echo "Usage: install.sh [--version <tag>] [--install-dir <dir>]"
            echo "  --version     Install a specific release (default: latest)"
            echo "  --install-dir Installation directory (default: /usr/local/bin)"
            exit 0 ;;
        *) warn "Unknown option: $1"; shift ;;
    esac
done

# ── detect platform ─────────────────────────────────────────────────────────

detect_os() {
    case "$(uname -s)" in
        Linux*)  echo "linux" ;;
        Darwin*) echo "darwin" ;;
        MINGW*|MSYS*|CYGWIN*) echo "windows" ;;
        *) err "Unsupported OS: $(uname -s)" ;;
    esac
}

detect_arch() {
    case "$(uname -m)" in
        x86_64|amd64)  echo "x86_64" ;;
        aarch64|arm64) echo "aarch64" ;;
        *) err "Unsupported architecture: $(uname -m)" ;;
    esac
}

OS=$(detect_os)
ARCH=$(detect_arch)

# Map to release asset name
case "${OS}-${ARCH}" in
    linux-x86_64)   TARGET="x86_64-unknown-linux-musl" ;;
    linux-aarch64)  TARGET="aarch64-unknown-linux-musl" ;;
    darwin-x86_64)  TARGET="x86_64-apple-darwin" ;;
    darwin-aarch64) TARGET="aarch64-apple-darwin" ;;
    windows-x86_64) TARGET="x86_64-pc-windows-msvc"; BINARY="recursive.exe" ;;
    *) err "No prebuilt binary for ${OS}-${ARCH}. Build from source: cargo install recursive-agent" ;;
esac

# ── resolve version ──────────────────────────────────────────────────────────

need_cmd curl

if [ -z "$VERSION" ]; then
    info "Fetching latest release..."
    VERSION=$(curl -fsSL "https://api.github.com/repos/${REPO}/releases/latest" \
        | grep '"tag_name"' | sed 's/.*"tag_name": *"\(.*\)".*/\1/')
    [ -z "$VERSION" ] && err "Could not determine latest version. Pass --version explicitly."
fi

info "Installing recursive ${VERSION} for ${OS}/${ARCH}"

# ── download ─────────────────────────────────────────────────────────────────

ASSET="${BINARY}-${TARGET}"
if [ "$OS" = "windows" ]; then
    ASSET="${ASSET}.zip"
else
    ASSET="${ASSET}.tar.gz"
fi

DOWNLOAD_URL="https://github.com/${REPO}/releases/download/${VERSION}/${ASSET}"
TMPDIR=$(mktemp -d)
trap 'rm -rf "$TMPDIR"' EXIT

info "Downloading ${DOWNLOAD_URL}"
curl -fsSL --retry 3 --retry-delay 2 -o "${TMPDIR}/${ASSET}" "$DOWNLOAD_URL" \
    || err "Download failed. Check that ${VERSION} exists: https://github.com/${REPO}/releases"

# ── extract ───────────────────────────────────────────────────────────────────

need_cmd tar
info "Extracting..."
tar -xzf "${TMPDIR}/${ASSET}" -C "$TMPDIR"

# ── install ───────────────────────────────────────────────────────────────────

DEST="${INSTALL_DIR}/${BINARY}"

if [ -w "$INSTALL_DIR" ]; then
    mv "${TMPDIR}/${BINARY}" "$DEST"
    chmod +x "$DEST"
else
    info "Need sudo to write to ${INSTALL_DIR}"
    sudo mv "${TMPDIR}/${BINARY}" "$DEST"
    sudo chmod +x "$DEST"
fi

ok "Installed recursive ${VERSION} → ${DEST}"

# ── verify ────────────────────────────────────────────────────────────────────

if command -v recursive >/dev/null 2>&1; then
    ok "$(recursive --version 2>/dev/null || echo 'Installation verified')"
else
    warn "recursive is not in PATH. Add ${INSTALL_DIR} to your PATH:"
    warn "  export PATH=\"\$PATH:${INSTALL_DIR}\""
fi

echo ""
echo "  Get started:"
echo "    recursive            # open TUI"
echo "    recursive -p 'goal'  # one-shot run"
echo "    recursive --help     # all commands"
echo ""
