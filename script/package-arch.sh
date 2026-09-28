#!/usr/bin/env bash
# Create an Omarchy/Arch package using makepkg as an unprivileged user.
# Usage: script/package-arch.sh STAGED_DIRECTORY OUTPUT_DIRECTORY
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
STAGE="$(cd "${1:?staged release required}" && pwd)"
mkdir -p "${2:?output directory required}"
OUT="$(cd "$2" && pwd)"
VERSION="$(sed -n 's/^version = "\([^"]*\)"/\1/p' "$ROOT/Cargo.toml" | head -1)"
if [[ $(id -u) == 0 ]]; then
  echo 'Run this script as an unprivileged build user (makepkg requires it).' >&2
  exit 1
fi
for bin in align align-cli align-mcp align-aaf ffmpeg ffprobe; do
  test -x "$STAGE/$bin"
done
for notice in LICENSE THIRD_PARTY_NOTICES.txt FFMPEG-LICENSE.txt; do
  test -s "$STAGE/$notice"
done
test -s "$STAGE/ThirdPartyLicenses/BUILD-MANIFEST.json"
test -d "$STAGE/AAF-Licenses"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
mkdir -p "$TMP/input/payload" "$TMP/input/integration" "$TMP/build"
tar -C "$STAGE" --exclude=./AAF-Sources -cf - . | tar -C "$TMP/input/payload" -xf -
cp "$ROOT/Support/Linux/com.align.app.desktop" "$ROOT/Support/Linux/com.align.app.metainfo.xml" \
  "$ROOT/Support/AppIcon.png" "$TMP/input/integration/"
tar -C "$TMP/input" -czf "$TMP/build/align-payload.tar.gz" payload integration
HASH="$(sha256sum "$TMP/build/align-payload.tar.gz" | cut -d ' ' -f 1)"
sed -e "s/@VERSION@/$VERSION/g" -e "s/@PAYLOAD_SHA256@/$HASH/g" \
  "$ROOT/Support/Linux/PKGBUILD.in" > "$TMP/build/PKGBUILD"
(cd "$TMP/build" && makepkg --nodeps --noconfirm)
cp "$TMP/build/align-$VERSION-1-x86_64.pkg.tar.zst" "$OUT/"
# Retain the exact packaging recipe and its input for reproducibility.
mkdir -p "$OUT/BuildInputs"
cp "$TMP/build/PKGBUILD" "$TMP/build/align-payload.tar.gz" "$OUT/BuildInputs/"
(cd "$OUT" && sha256sum "align-$VERSION-1-x86_64.pkg.tar.zst" > "align-$VERSION-1-x86_64.pkg.tar.zst.sha256")
