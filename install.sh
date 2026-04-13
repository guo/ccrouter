#!/usr/bin/env bash
# Install ccrouter — the Claude Code LLM router
# Usage: curl -fsSL https://raw.githubusercontent.com/guo/ccrouter/master/install.sh | sh

set -euo pipefail

REPO="guo/ccrouter"
BIN="ccrouter"
INSTALL_DIR="${INSTALL_DIR:-/usr/local/bin}"

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

if [ -w "${INSTALL_DIR}" ]; then
  mv "${TMP}/${BIN}" "${INSTALL_DIR}/${BIN}"
else
  sudo mv "${TMP}/${BIN}" "${INSTALL_DIR}/${BIN}"
fi

chmod +x "${INSTALL_DIR}/${BIN}"

echo "Installed: $(${INSTALL_DIR}/${BIN} --version)"
echo ""
echo "Next steps:"
echo "  ccrouter setup          # point Claude Code at ccrouter"
echo "  ccrouter start &        # start the proxy"
echo "  ccrouter list           # see configured profiles"
