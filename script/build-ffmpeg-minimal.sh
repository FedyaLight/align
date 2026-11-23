#!/usr/bin/env bash
# Build the two portable sidecars Align uses for containers it does not read
# natively (MTS/M2TS, MXF, R3D, AVI, Matroska, compressed audio such as AC-3).
# QuickTime/MP4 is handled in-process. Video is only demuxed and parsed for
# timing and dimensions, never decoded, so no video decoders are built. The
# mov muxer and trim/delay/pad filters serve camera-file export from those
# containers. No network or device IO.
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT_DIR="${1:-$ROOT_DIR/target/ffmpeg-minimal}"
FFMPEG_VERSION="${FFMPEG_VERSION:-9.0.1}"
ARCHIVE="ffmpeg-$FFMPEG_VERSION.tar.xz"
TEMP_DIR="$(mktemp -d)"
trap 'rm -rf "$TEMP_DIR"' EXIT

mkdir -p "$OUT_DIR"
OUT_DIR="$(cd "$OUT_DIR" && pwd)"
curl --fail --location --silent --show-error --retry 5 --retry-all-errors \
  "https://ffmpeg.org/releases/$ARCHIVE" \
  --output "$TEMP_DIR/$ARCHIVE"
tar -xf "$TEMP_DIR/$ARCHIVE" -C "$TEMP_DIR"
cd "$TEMP_DIR/ffmpeg-$FFMPEG_VERSION"

PCM_DECODERS="$(./configure --list-decoders | awk '{ for (i = 1; i <= NF; i++) if ($i ~ /^pcm_/) { printf "%s%s", sep, $i; sep="," } }')"
JOBS="$(getconf _NPROCESSORS_ONLN 2>/dev/null || sysctl -n hw.logicalcpu 2>/dev/null || echo 2)"

# MinGW-w64 implements clock_gettime and nanosleep, which FFmpeg's timers
# use, in winpthreads. A default MSYS2 build therefore imports
# libwinpthread-1.dll even though FFmpeg already uses native Win32 threads.
# Linking statically leaves only Windows system DLLs, so the sidecars run on
# machines without MSYS2. Make targets also need the .exe suffix there.
EXE=""
platform_options=()
case "$(uname -s)" in
  Darwin)
    # The tools must run on the oldest macOS Align supports (Info.plist
    # LSMinimumSystemVersion), not only on the build machine's.
    platform_options=(--enable-shared --disable-static --install-name-dir=@loader_path)
    export MACOSX_DEPLOYMENT_TARGET="${MACOSX_DEPLOYMENT_TARGET:-15.0}"
    ;;
  Linux)
    command -v patchelf >/dev/null || { echo "patchelf is required" >&2; exit 1; }
    platform_options=(--enable-shared --disable-static)
    ;;
  MINGW*|MSYS*|CYGWIN*)
    EXE=".exe"
    platform_options=(--extra-ldflags=-static)
    ;;
esac

# The guarded expansion keeps an empty array valid under `set -u` in bash 3.2.
./configure \
  ${platform_options[@]+"${platform_options[@]}"} \
  --disable-everything \
  --disable-autodetect \
  --disable-doc \
  --disable-debug \
  --disable-network \
  --disable-avdevice \
  --disable-swscale \
  --enable-ffmpeg \
  --enable-ffprobe \
  --enable-avcodec \
  --enable-avformat \
  --enable-avfilter \
  --enable-swresample \
  --enable-protocol=file,pipe \
  --enable-demuxer=mov,mpegts,mxf,mxf_d10,r3d,avi,asf,matroska,wav,aiff,mp3,flac,ogg \
  --enable-decoder=aac,ac3,eac3,alac,mp2,mp3float,flac,opus,vorbis,"$PCM_DECODERS" \
  --enable-parser=aac,aac_latm,ac3,mpegaudio,opus,vorbis,h264,hevc,mpegvideo,mpeg4video \
  --enable-encoder=pcm_f32le,pcm_s32le \
  --enable-muxer=pcm_f32le,pcm_s32le,mov \
  --enable-filter=abuffer,aformat,aresample,anull,abuffersink,atrim,asetpts,adelay,apad

make -j"$JOBS" "ffmpeg$EXE" "ffprobe$EXE"
cp "ffmpeg$EXE" "ffprobe$EXE" "$OUT_DIR/"
# Ship only runtime SONAMEs (no development symlinks). Each executable
# and each transitive dependency resolves siblings in its own directory.
case "$(uname -s)" in
  Darwin)
    for library in lib*/*.dylib; do
      [[ -L "$library" ]] && continue
      cp "$library" "$OUT_DIR/"
      strip -x "$OUT_DIR/${library##*/}"
      codesign -f -s - "$OUT_DIR/${library##*/}"
    done
    ;;
  Linux)
    for library in lib*/*.so.*; do
      [[ -L "$library" ]] && continue
      soname="$(patchelf --print-soname "$library")"
      cp "$library" "$OUT_DIR/$soname"
      strip --strip-unneeded "$OUT_DIR/$soname"
      patchelf --set-rpath '$ORIGIN' "$OUT_DIR/$soname"
    done
    for tool in ffmpeg ffprobe; do patchelf --set-rpath '$ORIGIN' "$OUT_DIR/$tool"; done
    for binary in "$OUT_DIR/ffmpeg" "$OUT_DIR/ffprobe" "$OUT_DIR"/*.so.*; do
      [[ "$(patchelf --print-rpath "$binary")" == '$ORIGIN' ]]
      while read -r needed; do
        case "$needed" in
          libav*.so.*|libsw*.so.*) test -f "$OUT_DIR/$needed" ;;
          */*) echo "Unexpected dependency path: $needed" >&2; exit 1 ;;
        esac
      done < <(patchelf --print-needed "$binary")
    done
    ;;
esac
cp COPYING.LGPLv2.1 "$OUT_DIR/FFMPEG-LICENSE.txt"
if [[ -n "$EXE" ]]; then
  # The static link embeds winpthreads (MIT AND BSD-3-Clause); binary
  # redistribution must carry its notice. A missing notice stops the build.
  {
    printf '\n\nffmpeg.exe and ffprobe.exe statically include winpthreads from the mingw-w64 project:\n\n'
    cat "${MINGW_PREFIX:-/mingw64}/share/licenses/winpthreads/COPYING"
  } >> "$OUT_DIR/FFMPEG-LICENSE.txt"
fi
mkdir -p "$OUT_DIR/Sources"
cp "$TEMP_DIR/$ARCHIVE" "$OUT_DIR/Sources/"
cp "$ROOT_DIR/script/build-ffmpeg-minimal.sh" "$OUT_DIR/Sources/"
cp ffbuild/config.mak "$OUT_DIR/Sources/config.mak"
printf '%s\n' "$FFMPEG_VERSION" > "$OUT_DIR/Sources/version.txt"
strip "$OUT_DIR/ffmpeg$EXE" "$OUT_DIR/ffprobe$EXE" 2>/dev/null || true

if [[ "$(uname -s)" == Darwin ]]; then
  codesign --force --sign - "$OUT_DIR/ffmpeg" "$OUT_DIR/ffprobe"
fi

for tool in ffmpeg ffprobe; do
  "$OUT_DIR/$tool$EXE" -hide_banner -version >/dev/null
done
du -h "$OUT_DIR/ffmpeg$EXE" "$OUT_DIR/ffprobe$EXE"
