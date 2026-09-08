#!/usr/bin/env bash
# Packages the Rust Align port for macOS (arm64): Align.app bundle +
# align-cli, align-mcp, AAF and FFmpeg sidecars, ad-hoc signed.
# Usage: ./script/package-macos.sh [output-dir]   (default: ~/Downloads/Align-macOS)
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT_DIR="${1:-$HOME/Downloads/Align-macOS}"
APP_NAME="Align"
BUNDLE_ID="com.align.app"
# Default sidecars support portable decoding and AAF picture metadata.
BUNDLE_FFMPEG="${BUNDLE_FFMPEG:-1}"
FFMPEG_DIR="${FFMPEG_DIR:-}"

# Build the self-contained AAF module before touching an existing package.
AAF_BUILD_DIR="$(mktemp -d "${TMPDIR:-/tmp}/align-aaf-package.XXXXXX")"
trap 'rm -rf "$AAF_BUILD_DIR"' EXIT
python3 -m venv "$AAF_BUILD_DIR/venv"
"$AAF_BUILD_DIR/venv/bin/python" -m pip install -r "$ROOT_DIR/Support/aaf/build-requirements.txt"
"$AAF_BUILD_DIR/venv/bin/python" "$ROOT_DIR/script/build-aaf-sidecar.py" "$AAF_BUILD_DIR/dist"

if [[ "$BUNDLE_FFMPEG" == "1" ]]; then
  if [[ -z "$FFMPEG_DIR" ]]; then
    FFMPEG_DIR="$AAF_BUILD_DIR/ffmpeg"
    "$ROOT_DIR/script/build-ffmpeg-minimal.sh" "$FFMPEG_DIR"
  fi
  for tool in ffmpeg ffprobe; do
    test -x "$FFMPEG_DIR/$tool"
    "$FFMPEG_DIR/$tool" -version >/dev/null
  done
  test -f "$FFMPEG_DIR/FFMPEG-LICENSE.txt"
fi

echo "==> building release"
cargo build --release --manifest-path "$ROOT_DIR/Cargo.toml" -p align-cli -p align-mcp -p align-gpui

BIN_DIR="$ROOT_DIR/target/release"
APP_BUNDLE="$OUT_DIR/$APP_NAME.app"
APP_CONTENTS="$APP_BUNDLE/Contents"
APP_MACOS="$APP_CONTENTS/MacOS"
RESOURCES="$APP_CONTENTS/Resources"

echo "==> staging $OUT_DIR"
rm -rf "$OUT_DIR"
mkdir -p "$APP_MACOS" "$RESOURCES"

cp "$BIN_DIR/align" "$APP_MACOS/$APP_NAME"
chmod +x "$APP_MACOS/$APP_NAME"
cp "$ROOT_DIR/Support/AppIcon.icns" "$RESOURCES/AppIcon.icns" 2>/dev/null || true
cp "$ROOT_DIR/THIRD_PARTY_NOTICES.txt" "$RESOURCES/THIRD_PARTY_NOTICES.txt"
cp "$BIN_DIR/align-cli" "$OUT_DIR/align-cli"
cp "$BIN_DIR/align-cli" "$APP_MACOS/align-cli"
cp "$BIN_DIR/align-mcp" "$OUT_DIR/align-mcp"
cp "$BIN_DIR/align-mcp" "$APP_MACOS/align-mcp"
cp "$ROOT_DIR/THIRD_PARTY_NOTICES.txt" "$OUT_DIR/THIRD_PARTY_NOTICES.txt"
chmod +x "$OUT_DIR/align-cli" "$APP_MACOS/align-cli" "$OUT_DIR/align-mcp" "$APP_MACOS/align-mcp"
cp "$AAF_BUILD_DIR/dist/align-aaf" "$APP_MACOS/align-aaf"
cp "$AAF_BUILD_DIR/dist/align-aaf" "$OUT_DIR/align-aaf"
cp -R "$AAF_BUILD_DIR/dist/AAF-Licenses" "$RESOURCES/AAF-Licenses"
cp -R "$AAF_BUILD_DIR/dist/AAF-Licenses" "$OUT_DIR/AAF-Licenses"
chmod +x "$APP_MACOS/align-aaf" "$OUT_DIR/align-aaf"


# Both entry points discover adjacent sidecars.
if [[ "$BUNDLE_FFMPEG" == "1" ]]; then
  for tool in ffmpeg ffprobe; do
    cp "$FFMPEG_DIR/$tool" "$APP_MACOS/$tool"
    cp "$FFMPEG_DIR/$tool" "$OUT_DIR/$tool"
    chmod +x "$APP_MACOS/$tool" "$OUT_DIR/$tool"
  done
  cp "$FFMPEG_DIR/FFMPEG-LICENSE.txt" "$RESOURCES/FFMPEG-LICENSE.txt"
  cp "$FFMPEG_DIR/FFMPEG-LICENSE.txt" "$OUT_DIR/FFMPEG-LICENSE.txt"
fi

cat >"$APP_CONTENTS/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleExecutable</key>
  <string>$APP_NAME</string>
  <key>CFBundleIconFile</key>
  <string>AppIcon</string>
  <key>CFBundleIdentifier</key>
  <string>$BUNDLE_ID</string>
  <key>CFBundleName</key>
  <string>Align</string>
  <key>CFBundleDisplayName</key>
  <string>Align</string>
  <key>CFBundlePackageType</key>
  <string>APPL</string>
  <key>CFBundleShortVersionString</key>
  <string>0.1.0</string>
  <key>CFBundleVersion</key>
  <string>1</string>
  <key>LSMinimumSystemVersion</key>
  <string>15.0</string>
  <key>NSHighResolutionCapable</key>
  <true/>
</dict>
</plist>
PLIST

cat >"$OUT_DIR/README.txt" <<README
Align – кроссплатформенный синхронизатор медиа (Rust + GPUI)

  Align.app            — графическое приложение (двойной клик)
  align-cli            — batch CLI: sync / export / export-json
  align-mcp            — MCP server for AI agents (stdio)
  align-aaf            — автономный модуль AAF (Python пользователю не нужен)
  THIRD_PARTY_NOTICES.txt — лицензии сторонних компонентов
$([ "$BUNDLE_FFMPEG" == "1" ] && echo "  ffmpeg, ffprobe      — bundled sidecars для видеоконтейнеров
                         (находятся автоматически рядом с бинарниками)" || echo "  (без bundled ffmpeg: на macOS используется Apple backend;
   для portable-движка нужен системный ffmpeg/ffprobe в PATH)")

CLI:
  ./align-cli sync /path/to/media > result.json
  ./align-cli export /path/to/output /path/to/media
  ./align-cli export-json result.json /path/to/output

AI AGENTS:
  Open Align → Align → Use with AI Agents… and copy the MCP configuration
  and ready-to-use prompt.

Требования: macOS 15+ (Apple backend дергает loadTracks API 15+), Apple Silicon.
Подпись ad-hoc (локальный запуск). Для распространения нужны
Developer ID + notarization.
README

echo "==> ad-hoc sign + verify"
codesign --force --deep --sign - "$APP_BUNDLE" >/dev/null
plutil -lint "$APP_CONTENTS/Info.plist"
codesign --verify --deep --strict "$APP_BUNDLE"
codesign -dv --verbose=2 "$APP_BUNDLE" 2>&1 | head -5

echo "==> smoke test"
"$OUT_DIR/align-cli" --help >/dev/null 2>&1 || "$OUT_DIR/align-cli" >/dev/null 2>&1 || true
printf '%s\n' '{"jsonrpc":"2.0","id":1,"method":"tools/list"}' | "$OUT_DIR/align-mcp" | grep -q 'align_sync'
("$APP_MACOS/$APP_NAME" >/tmp/align-smoke.log 2>&1 & echo $! > /tmp/align-smoke.pid)
sleep 5
SMOKE=$(cat /tmp/align-smoke.pid)
if kill -0 "$SMOKE" 2>/dev/null; then echo "    app alive"; kill "$SMOKE"; else echo "    app exited early:"; head -5 /tmp/align-smoke.log; fi

echo "==> done: $OUT_DIR"
du -sh "$OUT_DIR"
