#!/bin/bash
# Builds the RPM for Fedora. No arguments:
#
#   ./packaging/build-rpm.sh
#
# The binary is compiled with cargo out here and rpmbuild only packages it. It
# is simpler than doing it inside rpmbuild and it avoids having to declare
# cargo's whole dependency tree as BuildRequires.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SPEC="$ROOT/packaging/akebia.spec"

# The version lives in the binary's crate, not in the root Cargo.toml: the root
# only declares the workspace and has no `version` field.
VER=$(grep -m1 '^version' "$ROOT/crates/akebia-frontend/Cargo.toml" | sed 's/.*"\(.*\)"/\1/')
if [ -z "$VER" ]; then
    echo "error: could not read the version from crates/akebia-frontend/Cargo.toml" >&2
    exit 1
fi

echo "==> Building akebia v$VER..."
cargo build --release --manifest-path "$ROOT/Cargo.toml"

RPMBUILD="$ROOT/target/rpmbuild"
echo "==> Preparing the sources..."
rm -rf "$RPMBUILD"
mkdir -p "$RPMBUILD/SOURCES"
cp "$ROOT/target/release/akebia"   "$RPMBUILD/SOURCES/"
cp "$ROOT/README.md"               "$RPMBUILD/SOURCES/"
cp "$ROOT/COPYING"                 "$RPMBUILD/SOURCES/"
cp "$ROOT/data/akebia.desktop"     "$RPMBUILD/SOURCES/"

# The icon sizes are derived from the 1024 master instead of storing them all in
# the repository: they are eight PNGs that would always have to be regenerated
# together.
if ! command -v magick >/dev/null; then
    echo "error: ImageMagick is needed to generate the icon sizes" >&2
    echo "       sudo dnf install ImageMagick" >&2
    exit 1
fi
for size in 16 24 32 48 64 128 256 512; do
    magick "$ROOT/data/icons/akebia.png" -resize ${size}x${size} \
        "$RPMBUILD/SOURCES/akebia-${size}.png"
done

echo "==> Building the RPM..."
rpmbuild -bb \
    --define "_topdir $RPMBUILD" \
    --define "ver $VER" \
    --define "_sourcedir $RPMBUILD/SOURCES" \
    "$SPEC"

PACKAGE=$(find "$RPMBUILD/RPMS" -name '*.rpm' | head -1)
echo
echo "==> Done: $PACKAGE"
echo
echo "   Install:   sudo dnf install $PACKAGE"
echo "   Uninstall: sudo dnf remove akebia"
