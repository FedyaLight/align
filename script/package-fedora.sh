#!/usr/bin/env bash
# Package a staged x86_64 Linux release. Run on Linux with rpm-build installed.
# Usage: script/package-fedora.sh STAGED_DIRECTORY OUTPUT_DIRECTORY
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
STAGE="$(cd "${1:?staged release required}" && pwd)"
mkdir -p "${2:?output directory required}"
OUT="$(cd "$2" && pwd)"
VERSION="$(sed -n 's/^version = "\([^"]*\)"/\1/p' "$ROOT/Cargo.toml" | head -1)"
for bin in align align-cli align-mcp align-aaf ffmpeg ffprobe; do
  test -x "$STAGE/$bin"
done
for notice in LICENSE THIRD_PARTY_NOTICES.txt FFMPEG-LICENSE.txt; do
  test -s "$STAGE/$notice"
done
test -d "$STAGE/AAF-Licenses"
test -d "$STAGE/ThirdPartyLicenses"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
mkdir -p "$TMP"/{BUILD,RPMS,SOURCES,SPECS,SRPMS,BUILDROOT}
cat > "$TMP/SPECS/align.spec" <<SPEC
Name: align
Version: $VERSION
Release: 1
Summary: Audio and video synchronization for editors
License: GPL-3.0-only
URL: https://github.com/FedyaLight/align
BuildArch: x86_64
Requires: libX11, libxcb, libxkbcommon, libxkbcommon-x11
Requires: libwayland-client.so.0()(64bit), libwayland-cursor.so.0()(64bit), libwayland-egl.so.1()(64bit)
Requires: alsa-lib, systemd-libs, vulkan-loader, mesa-vulkan-drivers
Requires: fontconfig, freetype, libstdc++, libgcc
Requires: hicolor-icon-theme
# Binary inputs are already release-stripped; do not rewrite PyInstaller blobs.
%global __os_install_post %{nil}
%global debug_package %{nil}
%global _build_id_links none
# License files may contain Perl/Python snippets; they are documentation.
%global __requires_exclude_from ^/opt/align/(AAF-Licenses|ThirdPartyLicenses)/.*$

%description
Sync your recordings. Keep your edit.
Align synchronizes camera footage and separately recorded audio using waveforms,
timecode, and recording metadata. Includes the desktop app, CLI, MCP server,
self-contained AAF module, FFmpeg, and ffprobe.

%install
mkdir -p %{buildroot}/opt/align %{buildroot}/usr/bin
tar -C "$STAGE" --exclude=./AAF-Sources -cf - . | tar -C %{buildroot}/opt/align -xf -
for executable in align align-cli align-mcp; do
  ln -s /opt/align/\$executable %{buildroot}/usr/bin/\$executable
done
install -Dm644 "$ROOT/Support/Linux/com.align.app.desktop" %{buildroot}/usr/share/applications/com.align.app.desktop
install -Dm644 "$ROOT/Support/Linux/com.align.app.metainfo.xml" %{buildroot}/usr/share/metainfo/com.align.app.metainfo.xml
install -Dm644 "$ROOT/Support/AppIcon.png" %{buildroot}/usr/share/icons/hicolor/1024x1024/apps/com.align.app.png

%files
/opt/align
/usr/bin/align
/usr/bin/align-cli
/usr/bin/align-mcp
/usr/share/applications/com.align.app.desktop
/usr/share/metainfo/com.align.app.metainfo.xml
/usr/share/icons/hicolor/1024x1024/apps/com.align.app.png
SPEC
rpmbuild --define "_topdir $TMP" -bb "$TMP/SPECS/align.spec"
cp "$TMP/RPMS/x86_64/align-$VERSION-1.x86_64.rpm" "$OUT/Align-$VERSION-Fedora-x86_64.rpm"
rpm -qpl "$OUT/Align-$VERSION-Fedora-x86_64.rpm"
(cd "$OUT" && sha256sum "Align-$VERSION-Fedora-x86_64.rpm" > "Align-$VERSION-Fedora-x86_64.rpm.sha256")
