#!/usr/bin/env bash
# Create a Finder drag-to-Applications DMG from a validated Align.app bundle.
# Usage: script/create-macos-dmg.sh /path/to/Align.app /path/to/Align-macOS-arm64.dmg
set -euo pipefail
ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
APP="${1:?Supply Align.app}"
OUTPUT="${2:?Supply output DMG}"
test -d "$APP/Contents"
if [[ -e "$OUTPUT" ]]; then
  echo "Destination already exists: $OUTPUT" >&2
  exit 1
fi
codesign --verify --deep --strict "$APP"
TMP_DIR="$(mktemp -d "${TMPDIR:-/tmp}/align-dmg.XXXXXX")"
trap 'rm -rf "$TMP_DIR"' EXIT
mkdir -p "$(dirname "$OUTPUT")"
swift "$ROOT_DIR/script/render-dmg-background.swift" "$TMP_DIR/install.png"
"${PYTHON:-python3}" - "$APP" "$OUTPUT" "$TMP_DIR/install.png" <<'PYTHON'
from pathlib import Path
import sys
import dmgbuild
app, output, background = map(lambda value: str(Path(value).resolve()), sys.argv[1:])
dmgbuild.build_dmg(output, 'Align', settings={
    'format': 'UDZO',
    'files': [app],
    'symlinks': {'Applications': '/Applications'},
    'background': background,
    # Finder bounds include its title bar and may retain the user's path bar.
    # Leave vertical room around the 720 x 440 background on first open.
    'window_rect': ((180, 120), (720, 500)),
    'icon_locations': {'Align.app': (190, 230), 'Applications': (530, 230)},
    'icon_size': 96,
    'text_size': 13,
    'default_view': 'icon-view',
    'show_toolbar': False,
    'show_status_bar': False,
    'show_pathbar': False,
    'show_sidebar': False,
})
PYTHON
hdiutil verify "$OUTPUT"
shasum -a 256 "$OUTPUT" | awk -v name="$(basename "$OUTPUT")" '{print $1 "  " name}' > "$OUTPUT.sha256"
echo "$OUTPUT"
