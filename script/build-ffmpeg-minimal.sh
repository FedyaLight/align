#!/usr/bin/env bash
# Build the two portable sidecars Align actually uses. Video is demuxed for
# timing, audio decoding, and AAF video metadata; no network or device IO.
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT_DIR="${1:-$ROOT_DIR/target/ffmpeg-minimal}"
FFMPEG_VERSION="${FFMPEG_VERSION:-9.0.1}"
ARCHIVE="ffmpeg-$FFMPEG_VERSION.tar.xz"
TEMP_DIR="$(mktemp -d)"
trap 'rm -rf "$TEMP_DIR"' EXIT

mkdir -p "$OUT_DIR"
OUT_DIR="$(cd "$OUT_DIR" && pwd)"
curl --fail --location --silent --show-error \
  "https://ffmpeg.org/releases/$ARCHIVE" \
  --output "$TEMP_DIR/$ARCHIVE"
tar -xf "$TEMP_DIR/$ARCHIVE" -C "$TEMP_DIR"
cd "$TEMP_DIR/ffmpeg-$FFMPEG_VERSION"

PCM_DECODERS="$(./configure --list-decoders | awk '{ for (i = 1; i <= NF; i++) if ($i ~ /^pcm_/) { printf "%s%s", sep, $i; sep="," } }')"
JOBS="$(getconf _NPROCESSORS_ONLN 2>/dev/null || sysctl -n hw.logicalcpu 2>/dev/null || echo 2)"

./configure \
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
  --enable-decoder=aac,aac_fixed,ac3,eac3,alac,mp2,mp3,mp3float,flac,opus,vorbis,h264,hevc,mpeg2video,mpeg4,prores,dnxhd,mjpeg,vp8,vp9,av1,"$PCM_DECODERS" \
  --enable-parser=aac,aac_latm,ac3,mpegaudio,opus,vorbis,h264,hevc,mpegvideo,mpeg4video,dnxhd,mjpeg,vp8,vp9,av1 \
  --enable-encoder=pcm_f32le,pcm_s32le \
  --enable-muxer=pcm_f32le,pcm_s32le \
  --enable-filter=abuffer,aformat,aresample,anull,abuffersink

make -j"$JOBS" ffmpeg ffprobe

EXE=""
case "$(uname -s)" in
  MINGW*|MSYS*|CYGWIN*) EXE=".exe" ;;
esac
cp "ffmpeg$EXE" "ffprobe$EXE" "$OUT_DIR/"
cp COPYING.LGPLv2.1 "$OUT_DIR/FFMPEG-LICENSE.txt"
mkdir -p "$OUT_DIR/Sources"
cp "$TEMP_DIR/$ARCHIVE" "$OUT_DIR/Sources/"
cp "$ROOT_DIR/script/build-ffmpeg-minimal.sh" "$OUT_DIR/Sources/"
cp ffbuild/config.mak "$OUT_DIR/Sources/config.mak"
printf '%s\n' "$FFMPEG_VERSION" > "$OUT_DIR/Sources/version.txt"
strip "$OUT_DIR/ffmpeg$EXE" "$OUT_DIR/ffprobe$EXE" 2>/dev/null || true

"$OUT_DIR/ffmpeg$EXE" -hide_banner -version | head -1
du -h "$OUT_DIR/ffmpeg$EXE" "$OUT_DIR/ffprobe$EXE"
