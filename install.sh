#!/usr/bin/env bash
#
# install.sh :: Lightning FM artist node installer
#
# Installs the daemon on a fresh Debian 12+ / Ubuntu 22.04+ box (VPS or Pi):
#   1. binary at /usr/local/bin/lfm-artist-node
#      (prebuilt from GitHub Releases, or your own target/release build)
#   2. system user lfm-artist (no shell)
#   3. config at /etc/lfm-artist-node.env (scaffolded, NEVER overwritten)
#   4. unit at /etc/systemd/system/lfm-artist-node.service (enabled, not started)
#
# Idempotent: re-running updates the binary and unit, leaves your env file alone.
#
# Usage:
#   sudo ./install.sh                 # latest release
#   sudo LFM_VERSION=v0.1.0 ./install.sh   # pin a release tag
#
# A locally built binary (cargo build --release) takes priority over a
# download, so `cargo build --release && sudo ./install.sh` also works.
# On a Pi 4 that build takes about 42 minutes; the prebuilt aarch64
# binary exists so you can skip it.

set -euo pipefail

REPO="Lightning-FM/lightning-fm-artist-nodes"
BIN_DEST="/usr/local/bin/lfm-artist-node"
ENV_FILE="/etc/lfm-artist-node.env"
UNIT_DEST="/etc/systemd/system/lfm-artist-node.service"
SERVICE_USER="lfm-artist"
VERSION="${LFM_VERSION:-latest}"
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

say()  { echo "[install] $1"; }
fail() { echo "[install] error: $1" >&2; exit 1; }

[[ "$(id -u)" -eq 0 ]] || fail "run as root: sudo ./install.sh"
command -v systemctl >/dev/null || fail "systemd required (no systemctl found)"
command -v curl >/dev/null || fail "curl required"

case "$(uname -m)" in
  aarch64|arm64) ARCH="aarch64" ;;
  x86_64|amd64)  ARCH="x86_64" ;;
  *) fail "unsupported architecture $(uname -m); build from source: cargo build --release" ;;
esac

# ---- 1. binary -------------------------------------------------------------

if [[ -x "$HERE/target/release/lfm-artist-node" ]]; then
  say "using locally built binary (target/release/lfm-artist-node)"
  install -m 0755 "$HERE/target/release/lfm-artist-node" "$BIN_DEST"
else
  if [[ "$VERSION" == "latest" ]]; then
    VERSION="$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" \
      | grep -m1 '"tag_name"' | cut -d'"' -f4)"
    [[ -n "$VERSION" ]] || fail "could not resolve the latest release tag; set LFM_VERSION=vX.Y.Z"
  fi
  TARBALL="lfm-artist-node-$VERSION-linux-$ARCH.tar.gz"
  URL="https://github.com/$REPO/releases/download/$VERSION/$TARBALL"
  TMP="$(mktemp -d)"
  trap 'rm -rf "$TMP"' EXIT
  say "downloading $TARBALL"
  curl -fsSL -o "$TMP/$TARBALL" "$URL" || fail "download failed: $URL"
  tar -xzf "$TMP/$TARBALL" -C "$TMP"
  [[ -f "$TMP/lfm-artist-node" ]] || fail "tarball did not contain lfm-artist-node"
  install -m 0755 "$TMP/lfm-artist-node" "$BIN_DEST"
  say "installed $VERSION to $BIN_DEST"
fi

# ---- 2. system user --------------------------------------------------------

if id -u "$SERVICE_USER" >/dev/null 2>&1; then
  say "user $SERVICE_USER already exists"
else
  useradd --system --home-dir /var/lib/lfm-artist-node --create-home \
    --shell /usr/sbin/nologin "$SERVICE_USER"
  say "created system user $SERVICE_USER"
fi

# ---- 3. env file (scaffold once, never overwrite) --------------------------

if [[ -f "$ENV_FILE" ]]; then
  say "config exists, leaving it alone: $ENV_FILE"
else
  if [[ -f "$HERE/deploy/lfm-artist-node.env.example" ]]; then
    install -m 0600 -o root -g root "$HERE/deploy/lfm-artist-node.env.example" "$ENV_FILE"
  else
    curl -fsSL -o "$ENV_FILE" \
      "https://raw.githubusercontent.com/$REPO/main/deploy/lfm-artist-node.env.example" \
      || fail "could not fetch the env template"
    chmod 0600 "$ENV_FILE"
    chown root:root "$ENV_FILE"
  fi
  say "scaffolded $ENV_FILE (mode 600; it will hold your seed)"
fi

# ---- 4. systemd unit -------------------------------------------------------

if [[ -f "$HERE/deploy/lfm-artist-node.service" ]]; then
  install -m 0644 "$HERE/deploy/lfm-artist-node.service" "$UNIT_DEST"
else
  curl -fsSL -o "$UNIT_DEST" \
    "https://raw.githubusercontent.com/$REPO/main/deploy/lfm-artist-node.service" \
    || fail "could not fetch the systemd unit"
  chmod 0644 "$UNIT_DEST"
fi
systemctl daemon-reload
systemctl enable lfm-artist-node >/dev/null
say "installed and enabled lfm-artist-node.service (not started)"

# ---- done ------------------------------------------------------------------

echo
say "next steps:"
say "  1. edit $ENV_FILE (12 vars; docs/onboarding.md walks through them)"
say "  2. sudo systemctl start lfm-artist-node"
say "  3. curl http://localhost:8090/health"
say "  4. journalctl -u lfm-artist-node -f"
