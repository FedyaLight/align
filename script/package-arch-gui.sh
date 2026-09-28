#!/usr/bin/env bash
# Wrap a native Arch package in Align's GTK4 installer. Build on x86_64 Linux.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
PACKAGE="$(realpath "${1:?Arch package required}")"
OUT="$(realpath -m "${2:?output .run path required}")"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
cc -O2 -Wall -Wextra -Werror -DALIGN_PACKAGE_ARCH "$ROOT/Support/Linux/installer.c" \
  $(pkg-config --cflags --libs gtk4) -o "$TMP/align-installer"
strip "$TMP/align-installer"
cp "$PACKAGE" "$TMP/Align.pkg.tar.zst"
cp "$ROOT/Support/AppIcon.png" "$TMP/Align.png"
mkdir -p "$(dirname "$OUT")"
cat > "$OUT" <<'SH'
#!/bin/sh
set -eu
if [ "$(uname -m)" != x86_64 ] || [ ! -x /usr/bin/pacman ]; then
  echo 'This installer requires Omarchy or Arch Linux on an x86_64 computer.' >&2
  exit 1
fi
if [ ! -x /usr/bin/pkexec ]; then
  echo 'Install polkit to use the graphical installer, or install the separate package with sudo pacman -U.' >&2
  exit 1
fi
unpack_dir=$(mktemp -d "${TMPDIR:-/tmp}/align-setup.XXXXXX")
trap 'rm -rf "$unpack_dir"' EXIT HUP INT TERM
payload_line=$(awk '/^__ALIGN_PAYLOAD__$/ { print NR + 1; exit }' "$0")
tail -n +"$payload_line" "$0" | tar -xz -C "$unpack_dir"
if { [ -n "${WAYLAND_DISPLAY:-}" ] || [ -n "${DISPLAY:-}" ]; } &&
   ! ldd "$unpack_dir/align-installer" 2>/dev/null | grep -q 'not found'; then
  "$unpack_dir/align-installer" "$unpack_dir/Align.pkg.tar.zst" "$unpack_dir/Align.png"
else
  echo 'Installing the native Arch package through pacman.'
  pkexec /usr/bin/pacman -U "$unpack_dir/Align.pkg.tar.zst"
fi
exit 0
__ALIGN_PAYLOAD__
SH
tar -czf - -C "$TMP" align-installer Align.pkg.tar.zst Align.png >> "$OUT"
chmod +x "$OUT"
(cd "$(dirname "$OUT")" && sha256sum "$(basename "$OUT")" > "$(basename "$OUT").sha256")
