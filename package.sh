#!/usr/bin/env bash
# Build a portable release tarball: dist/mtop-<version>-<arch>.tar.gz
#
# The tarball contains a static (musl) binary plus install.sh/uninstall.sh,
# so target machines need no Rust toolchain:
#
#   tar xzf mtop-*.tar.gz && cd mtop-*/ && sudo ./install.sh
#
# Usage: ./package.sh [rust-target]   (default: <arch>-unknown-linux-musl)
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")"

ARCH="$(uname -m)"
TARGET="${1:-$ARCH-unknown-linux-musl}"
VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n1)"

if [[ "$TARGET" == *musl* ]] && ! rustup target list --installed | grep -qx "$TARGET"; then
    echo "==> Adding rust target $TARGET"
    rustup target add "$TARGET"
fi

echo "==> Building $TARGET"
cargo build --release --target "$TARGET"

NAME="mtop-$VERSION-${TARGET%%-*}"
STAGE="dist/$NAME"
rm -rf "$STAGE"
mkdir -p "$STAGE"
install -m 0755 "target/$TARGET/release/mtop" "$STAGE/mtop"
install -m 0755 install.sh uninstall.sh "$STAGE/"
cp README.md "$STAGE/" 2>/dev/null || true

tar -C dist -czf "dist/$NAME.tar.gz" "$NAME"
rm -rf "$STAGE"
echo "==> dist/$NAME.tar.gz"
