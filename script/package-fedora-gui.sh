#!/usr/bin/env bash
# Wrap an RPM in Align's branded GTK4 installer. Build on x86_64 Linux.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
RPM="$(realpath "${1:?RPM file required}")"
OUT="$(realpath -m "${2:?output .run path required}")"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
cc -O2 -Wall -Wextra -Werror "$ROOT/Support/Linux/installer.c" \
  $(pkg-config --cflags --libs gtk4) -o "$TMP/align-installer"
strip "$TMP/align-installer"
cp "$RPM" "$TMP/Align.rpm"
cp "$ROOT/Support/AppIcon.png" "$TMP/Align.png"
mkdir -p "$(dirname "$OUT")"
cat > "$OUT" <<'SH'
#!/bin/sh
set -eu
if [ "$(uname -m)" != x86_64 ] || ! command -v dnf >/dev/null; then
  echo 'This installer requires Fedora on an x86_64 computer.' >&2
  exit 1
fi
unpack_dir=$(mktemp -d "${TMPDIR:-/tmp}/align-setup.XXXXXX")
trap 'rm -rf "$unpack_dir"' EXIT HUP INT TERM
payload_line=$(awk '/^__ALIGN_PAYLOAD__$/ { print NR + 1; exit }' "$0")
tail -n +"$payload_line" "$0" | tar -xz -C "$unpack_dir"
if ! ldd "$unpack_dir/align-installer" 2>/dev/null | grep -q 'not found'; then
  "$unpack_dir/align-installer" "$unpack_dir/Align.rpm" "$unpack_dir/Align.png"
else
  echo 'GTK 4 is required for the graphical installer. Installing the RPM through DNF.'
  pkexec /usr/bin/dnf --assumeyes install "$unpack_dir/Align.rpm"
fi
exit 0
__ALIGN_PAYLOAD__
SH
tar -czf - -C "$TMP" align-installer Align.rpm Align.png >> "$OUT"
chmod +x "$OUT"
(cd "$(dirname "$OUT")" && sha256sum "$(basename "$OUT")" > "$(basename "$OUT").sha256")
