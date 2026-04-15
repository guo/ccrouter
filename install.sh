#!/usr/bin/env bash
# Install ccrouter — the Claude Code LLM router
# Usage: curl -fsSL https://raw.githubusercontent.com/guo/ccrouter/master/install.sh | sh
#
# Override install location:
#   INSTALL_DIR=/usr/local/bin curl -fsSL ... | sh
# Override version:
#   VERSION=v0.1.8 curl -fsSL ... | sh

set -euo pipefail

REPO="guo/ccrouter"
BIN="ccrouter"
INSTALL_DIR="${INSTALL_DIR:-$HOME/.local/bin}"

# Detect OS and architecture
OS="$(uname -s)"
ARCH="$(uname -m)"

case "${OS}" in
  Darwin)
    TARGET="universal-apple-darwin"
    ;;
  Linux)
    case "${ARCH}" in
      x86_64)  TARGET="x86_64-unknown-linux-musl" ;;
      aarch64) TARGET="aarch64-unknown-linux-musl" ;;
      arm64)   TARGET="aarch64-unknown-linux-musl" ;;
      *)
        echo "Unsupported architecture: ${ARCH}" >&2
        exit 1
        ;;
    esac
    ;;
  *)
    echo "Unsupported OS: ${OS}" >&2
    exit 1
    ;;
esac

# Resolve latest release tag
TAG="${VERSION:-}"
if [ -z "${TAG}" ]; then
  TAG=$(curl -fsSL "https://api.github.com/repos/${REPO}/releases/latest" \
    | grep '"tag_name"' | sed 's/.*"tag_name": "\(.*\)".*/\1/')
fi

if [ -z "${TAG}" ]; then
  echo "Could not determine latest release tag." >&2
  exit 1
fi

ARCHIVE="${BIN}-${TAG}-${TARGET}.tar.gz"
URL="https://github.com/${REPO}/releases/download/${TAG}/${ARCHIVE}"

echo "Installing ccrouter ${TAG} (${TARGET}) → ${INSTALL_DIR}/${BIN}"
echo ""

TMP=$(mktemp -d)
trap 'rm -rf "${TMP}"' EXIT

curl -fsSL "${URL}" -o "${TMP}/${ARCHIVE}"
tar xzf "${TMP}/${ARCHIVE}" -C "${TMP}"

# Ensure install dir exists (create user-local dirs without sudo; system dirs may need it)
if [ ! -d "${INSTALL_DIR}" ]; then
  if mkdir -p "${INSTALL_DIR}" 2>/dev/null; then
    :
  else
    echo "Creating ${INSTALL_DIR} (requires sudo)..."
    sudo mkdir -p "${INSTALL_DIR}"
  fi
fi

if [ -w "${INSTALL_DIR}" ]; then
  mv "${TMP}/${BIN}" "${INSTALL_DIR}/${BIN}"
else
  sudo mv "${TMP}/${BIN}" "${INSTALL_DIR}/${BIN}"
fi

if [ -w "${INSTALL_DIR}/${BIN}" ]; then
  chmod +x "${INSTALL_DIR}/${BIN}"
else
  sudo chmod +x "${INSTALL_DIR}/${BIN}"
fi

# Strip macOS quarantine xattr so Gatekeeper doesn't block first-run.
# Harmless no-op if the xattr isn't set (e.g. on Linux, or when curl didn't set it).
if [ "${OS}" = "Darwin" ]; then
  xattr -dr com.apple.quarantine "${INSTALL_DIR}/${BIN}" 2>/dev/null || true
fi

# PATH check — warn if the install dir isn't on PATH so users don't hit "command not found"
case ":${PATH}:" in
  *":${INSTALL_DIR}:"*)
    ;;
  *)
    echo ""
    echo "⚠  ${INSTALL_DIR} is not on your PATH."
    echo "   Add this to your shell profile (~/.zshrc or ~/.bashrc):"
    echo "     export PATH=\"${INSTALL_DIR}:\$PATH\""
    echo ""
    ;;
esac

echo "Installed: $(${INSTALL_DIR}/${BIN} --version)"
echo ""
echo "Next steps:"
echo "  ccrouter setup          # point Claude Code at ccrouter"
echo "  ccrouter start -d       # start the proxy (background)"
echo "  ccrouter list           # see configured profiles"
