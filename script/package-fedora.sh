#!/usr/bin/env bash
# Package a staged x86_64 Linux release. Run on Linux with rpm-build installed.
# Usage: [BUNDLE_FFMPEG=0|1] script/package-fedora.sh STAGED_DIRECTORY OUTPUT_DIRECTORY
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
STAGE="$(cd "${1:?staged release required}" && pwd)"
mkdir -p "${2:?output directory required}"
OUT="$(cd "$2" && pwd)"
VERSION="$(sed -n 's/^version = "\([^"]*\)"/\1/p' "$ROOT/Cargo.toml" | head -1)"
for bin in align align-cli align-aaf; do
  test -x "$STAGE/$bin"
done
for notice in LICENSE THIRD_PARTY_NOTICES.txt; do
  test -s "$STAGE/$notice"
done
test -d "$STAGE/AAF-Licenses"
test -s "$STAGE/align-aaf-runtime/base_library.zip"
test -d "$STAGE/ThirdPartyLicenses"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
mkdir -p "$TMP"/{BUILD,RPMS,SOURCES,SPECS,SRPMS,BUILDROOT}
# The release stages FFmpeg's runtime SONAMEs next to its executables.
# Nested AAF libraries are private to the frozen Python process.
shopt -s nullglob
libraries=("$STAGE"/*.so "$STAGE"/*.so.*)
excludes=(--exclude=./AAF-Sources)
rpm_options=()
case "${BUNDLE_FFMPEG:-1}" in
  1)
    test -x "$STAGE/ffmpeg"
    test -x "$STAGE/ffprobe"
    test -s "$STAGE/FFMPEG-LICENSE.txt"
    if (( ${#libraries[@]} )); then
      elfdeps="$(rpm --eval '%{_rpmconfigdir}')/elfdeps"
      # Match the precise capabilities (including symbol versions) supplied
      # by these libraries. Other requirements of the same ELFs remain visible.
      "$elfdeps" --provides "${libraries[@]}" | sort -u > "$TMP/private-provides"
      # Only FFmpeg resolves siblings via $ORIGIN. Keep system requirements
      # of the app and AAF launcher even when a sibling has the same SONAME.
      "$elfdeps" --requires "$STAGE/align" "$STAGE/align-cli" "$STAGE/align-aaf" |
        sort -u > "$TMP/app-requires"
      private_caps="$(comm -23 "$TMP/private-provides" "$TMP/app-requires" |
        sed 's/[^[:alnum:]_]/[&]/g' | paste -sd '|' -)"
      if [[ -n "$private_caps" ]]; then
        rpm_options+=(--define "__requires_exclude ^($private_caps)$")
      fi
    fi
    ;;
  0)
    excludes+=(--exclude=./ffmpeg --exclude=./ffprobe --exclude=./FFMPEG-LICENSE.txt)
    for library in "${libraries[@]}"; do
      excludes+=("--exclude=./${library##*/}")
    done
    ;;
  *) echo "BUNDLE_FFMPEG must be 0 or 1" >&2; exit 1 ;;
esac
tar -C "$STAGE" "${excludes[@]}" -cf "$TMP/SOURCES/align.tar" .
cat > "$TMP/SPECS/align.spec" <<SPEC
Name: align
Version: $VERSION
Release: 1
Summary: Audio and video synchronization for editors
License: GPL-3.0-only
URL: https://github.com/FedyaLight/align
Source0: align.tar
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
%global __provides_exclude_from ^/opt/align/.*
%global __requires_exclude_from ^/opt/align/(align-aaf-runtime|AAF-Licenses|ThirdPartyLicenses)/.*

%description
Sync your recordings. Keep your edit.
Align synchronizes camera footage and separately recorded audio using waveforms,
timecode, and recording metadata. Includes the desktop app, CLI, and
self-contained AAF module.

%install
mkdir -p %{buildroot}/opt/align %{buildroot}/usr/bin
tar -C %{buildroot}/opt/align -xf "%{SOURCE0}"
for executable in align align-cli; do
  ln -s /opt/align/\$executable %{buildroot}/usr/bin/\$executable
done
install -Dm644 "$ROOT/Support/Linux/com.align.app.desktop" %{buildroot}/usr/share/applications/com.align.app.desktop
install -Dm644 "$ROOT/Support/Linux/com.align.app.metainfo.xml" %{buildroot}/usr/share/metainfo/com.align.app.metainfo.xml
install -Dm644 "$ROOT/Support/AppIcon.png" %{buildroot}/usr/share/icons/hicolor/1024x1024/apps/com.align.app.png

%files
/opt/align
/usr/bin/align
/usr/bin/align-cli
/usr/share/applications/com.align.app.desktop
/usr/share/metainfo/com.align.app.metainfo.xml
/usr/share/icons/hicolor/1024x1024/apps/com.align.app.png
SPEC
rpmbuild --define "_topdir $TMP" "${rpm_options[@]}" -bb "$TMP/SPECS/align.spec"
cp "$TMP/RPMS/x86_64/align-$VERSION-1.x86_64.rpm" "$OUT/Align-$VERSION-Fedora-x86_64.rpm"
rpm -qpl "$OUT/Align-$VERSION-Fedora-x86_64.rpm"
(cd "$OUT" && sha256sum "Align-$VERSION-Fedora-x86_64.rpm" > "Align-$VERSION-Fedora-x86_64.rpm.sha256")
