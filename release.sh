#!/usr/bin/env bash
# release.sh — bump version, build, install locally, commit, tag, push
# Usage: ./release.sh 0.1.7

set -euo pipefail

VERSION="${1:-}"
if [ -z "$VERSION" ]; then
  echo "Usage: $0 <version>   e.g. $0 0.1.7" >&2
  exit 1
fi

TAG="v${VERSION}"
INSTALL_DIR="${INSTALL_DIR:-$HOME/.local/bin}"

echo "==> Bumping version to $VERSION"
sed -i '' "s/^version = \".*\"/version = \"${VERSION}\"/" Cargo.toml
# Update Cargo.lock too
cargo generate-lockfile --quiet 2>/dev/null || true

echo "==> Building release binary"
cargo build --release

echo "==> Installing to $INSTALL_DIR/ccrouter"
mkdir -p "$INSTALL_DIR"
cp target/release/ccrouter "$INSTALL_DIR/ccrouter"
chmod +x "$INSTALL_DIR/ccrouter"
# Use the freshly built binary for the smoke check — macOS anti-tampering
# occasionally SIGKILLs the copied file under $INSTALL_DIR on first run.
target/release/ccrouter --version

echo "==> Committing version bump"
git add Cargo.toml Cargo.lock
git commit -m "chore: bump version to $VERSION"
git push

echo "==> Tagging $TAG and pushing"
git tag "$TAG"
git push origin "$TAG"

echo ""
echo "Done. GitHub Actions will build binaries for $TAG."
echo "Monitor: gh run list --repo guo/ccrouter --limit 3"
