#!/usr/bin/env bash
# Velopack can build a Windows installer on macOS, Linux, or Windows.
# Usage: VPK=/path/to/vpk script/package-windows.sh STAGED_DIRECTORY OUTPUT_DIRECTORY
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
STAGE="$(cd "${1:?staged release required}" && pwd)"
mkdir -p "${2:?output directory required}"
OUT="$(cd "$2" && pwd)"
VERSION="$(sed -n 's/^version = "\([^"]*\)"/\1/p' "$ROOT/Cargo.toml" | head -1)"
for bin in align align-cli align-mcp align-aaf ffmpeg ffprobe; do
  test -s "$STAGE/$bin.exe"
done
for notice in LICENSE THIRD_PARTY_NOTICES.txt FFMPEG-LICENSE.txt; do
  test -s "$STAGE/$notice"
done
test -d "$STAGE/AAF-Licenses"
test -d "$STAGE/ThirdPartyLicenses"
# Velopack needs the [win] directive only when packing on another OS.
target=()
case "$(uname -s)" in MINGW*|MSYS*|CYGWIN*) ;; *) target=('[win]') ;; esac
"${VPK:-vpk}" "${target[@]}" pack --packId com.align.app --packTitle Align \
  --packAuthors 'Align contributors' --packVersion "$VERSION" \
  --runtime win-x64 --packDir "$STAGE" --mainExe align.exe \
  --framework vcredist143-x64 \
  --icon "$ROOT/Support/Installer/Align.ico" \
  --splashImage "$ROOT/Support/Installer/Setup.gif" \
  --splashProgressColor '#0A84FF' --shortcuts StartMenuRoot \
  --exclude '(^|[/\\])AAF-Sources([/\\]|$)' \
  --outputDir "$OUT/updates" --noPortable
cp "$OUT/updates/com.align.app-win-Setup.exe" "$OUT/Align-$VERSION-Windows-x64-Setup.exe"
(cd "$OUT" && sha256sum "Align-$VERSION-Windows-x64-Setup.exe" > "Align-$VERSION-Windows-x64-Setup.exe.sha256")
