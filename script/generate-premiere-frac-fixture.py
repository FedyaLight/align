#!/usr/bin/env python3
"""Generate minimal synthetic Premiere fractional-placement fixture.

Creates short 48kHz media with known markers and two FCP7 XML sequences
(25 FPS and 30000/1001 FPS) that encode the CURRENT production placement
(integer start/end + subframeoffset in 1/80 frame, audio only).

Ground truth (exact timeline/source positions) is stored in
ground-truth.json independently of the XML bytes. The XML files mimic the
production writer field-for-field for these simple non-retimed items;
a Rust regression test pins the writer to the same encoding.

Media design:
- Mono recorder WAVs: low sine (file ID) + single-sample click at 0.1 s
  file offset. Click times are separated in the timeline for render checks.
- Stereo camera MOV (H.264 + PCM): L=330 Hz, R=990 Hz + clicks at 0.1 s.
- Layout per sequence (frame numbers identical, seconds differ by FPS):
    A1 rec_early   start 2.1   (near frame beginning)
    A1 rec_mid     start 80.5  (mid frame)
    A1 rec_late    start 156.9 (near frame end)
    A1 rec_control start 232.0 (whole-frame control)
    A1 rec_trim    start 310.25, source in 0.5 s (trim + gap check)
    V1+A2+A3 cam  start 0 (linked 3-item A/V group, stereo channels 1/2)

Usage:
  python3 script/generate-premiere-frac-fixture.py [--out DIR]
Default DIR: /tmp/align-premiere-frac
"""
import argparse
import hashlib
import json
import subprocess
import wave
from fractions import Fraction
from pathlib import Path
from urllib.parse import quote

import numpy as np

SR = 48000
CLICK_OFFSET_SAMPLES = 4800  # 0.1 s file offset
SINE_LEVEL = 0.10
CLICK_LEVEL = 0.90

FPS_PRESETS = {
    "25": {"timebase": 25, "ntsc": False, "fps": Fraction(25, 1)},
    "2997": {"timebase": 30, "ntsc": True, "fps": Fraction(30000, 1001)},
}

# (key, start_frames as Fraction, duration_frames, source_in_frames, freq_hz)
RECORDER_LAYOUT = [
    ("rec_early", Fraction(21, 10), 75, 0, 440.0),
    ("rec_mid", Fraction(161, 2), 75, 0, 550.0),
    ("rec_late", Fraction(1569, 10), 75, 0, 660.0),
    ("rec_control", Fraction(232, 1), 75, 0, 770.0),
    ("rec_trim", Fraction(1241, 4), 50, 12, 880.0),
]
CAM_DURATION_FRAMES = 363
CAM_FREQS = (330.0, 990.0)


def write_mono_wav(path: Path, seconds: float, freq: float, markers: list[int]) -> None:
    n = int(round(seconds * SR))
    t = np.arange(n) / SR
    data = SINE_LEVEL * np.sin(2 * np.pi * freq * t)
    for marker in markers:
        data[marker] = CLICK_LEVEL
    data = np.clip(data, -1.0, 1.0)
    pcm = (data * 32767).astype(np.int16)
    with wave.open(str(path), "wb") as w:
        w.setnchannels(1)
        w.setsampwidth(2)
        w.setframerate(SR)
        w.writeframes(pcm.tobytes())


def write_stereo_wav(path: Path, seconds: float, markers: list[int]) -> None:
    n = int(round(seconds * SR))
    t = np.arange(n) / SR
    left = SINE_LEVEL * np.sin(2 * np.pi * CAM_FREQS[0] * t)
    right = SINE_LEVEL * np.sin(2 * np.pi * CAM_FREQS[1] * t)
    for marker in markers:
        left[marker] = CLICK_LEVEL
        right[marker] = CLICK_LEVEL
    stereo = np.stack([np.clip(left, -1, 1), np.clip(right, -1, 1)], axis=1)
    pcm = (stereo * 32767).astype(np.int16)
    with wave.open(str(path), "wb") as w:
        w.setnchannels(2)
        w.setsampwidth(2)
        w.setframerate(SR)
        w.writeframes(pcm.tobytes())


def selected_markers(source_in: Fraction, source_out: Fraction) -> list[int]:
    """Head/body/tail probes, last probe 2 ms before the selected end."""
    return [round((source_in + Fraction(1, 500)) * SR),
            round((source_in + source_out) / 2 * SR),
            round((source_out - Fraction(1, 500)) * SR)]


def run(cmd: list) -> None:
    subprocess.run(cmd, check=True, capture_output=True)


def make_video(out: Path, fps_str: str, seconds: float, audio_wav: Path) -> None:
    silent = out.with_suffix(".silent.mp4")
    run([
        "ffmpeg", "-y", "-v", "error",
        "-f", "lavfi", "-i", f"color=c=0x204060:s=640x360:r={fps_str}:d={seconds}",
        "-fflags", "+bitexact", "-flags:v", "+bitexact",
        "-c:v", "libx264", "-pix_fmt", "yuv420p", "-preset", "veryfast",
        str(silent),
    ])
    run([
        "ffmpeg", "-y", "-v", "error",
        "-i", str(silent), "-i", str(audio_wav),
        "-fflags", "+bitexact", "-flags:v", "+bitexact", "-flags:a", "+bitexact",
        "-c:v", "copy", "-c:a", "pcm_s16le", "-shortest",
        str(out),
    ])
    silent.unlink(missing_ok=True)


def file_url(p: Path) -> str:
    # Mirror production writer: file:// + percent-encoded absolute path.
    return "file://" + quote(str(p), safe="-_.~/:")


def frames_to_seconds(frames: Fraction, fps: Fraction) -> Fraction:
    return frames / fps


def clipitem_xml(cid: str, name: str, start_f: int, end_f: int,
                 in_f: int, out_f: int, dur_f: int, rate_xml: str,
                 sub: int | None, links: list, file_ref: str,
                 source_channel: int | None) -> str:
    """links: list of (clipref, mediatype, trackindex, clipindex, groupindex|None).

    Full production-style link elements: Premiere does not rebuild A/V link
    groups from bare <linkclipref> entries (verified: zero groups imported).
    """
    x = f'              <clipitem id="{cid}">\n'
    x += f'                <name>{name}</name>\n'
    x += '                <enabled>TRUE</enabled>\n'
    x += f'                <duration>{dur_f}</duration>\n'
    x += rate_xml
    x += f'                <start>{start_f}</start>\n'
    x += f'                <end>{end_f}</end>\n'
    x += f'                <in>{in_f}</in>\n'
    x += f'                <out>{out_f}</out>\n'
    if sub:
        x += f'                <subframeoffset>{sub}</subframeoffset>\n'
    for ref, mt, ti, ci, gi in links:
        x += f'                <link><linkclipref>{ref}</linkclipref><mediatype>{mt}</mediatype>'
        x += f'<trackindex>{ti}</trackindex><clipindex>{ci}</clipindex>'
        if gi is not None:
            x += f'<groupindex>{gi}</groupindex>'
        x += '</link>\n'
    x += file_ref
    if source_channel is not None:
        x += ('                <sourcetrack><mediatype>audio</mediatype>'
              f'<trackindex>{source_channel}</trackindex></sourcetrack>\n')
    x += '              </clipitem>\n'
    return x


def build_sequence(seq_name: str, preset: dict, media: dict, fps_key: str) -> tuple:
    tb = preset["timebase"]
    ntsc = "TRUE" if preset["ntsc"] else "FALSE"
    fps = preset["fps"]
    rate_xml = (f'                <rate><timebase>{tb}</timebase>'
                f'<ntsc>{ntsc}</ntsc></rate>\n')
    items = []  # ground truth rows
    seq_dur_f = 0

    # Recorder items on A1.
    audio_clips = []
    for key, start_fr, dur_fr, src_in_f, freq in RECORDER_LAYOUT:
        start_whole = int(start_fr // 1)
        start_frac = start_fr - start_whole
        sub = int(round(float(start_frac) * 80))
        end_fr = start_fr + dur_fr
        end_whole = int(end_fr // 1)
        # Source trim in exact frames (no source quantization in this fixture).
        in_f = int(src_in_f)
        out_f = in_f + dur_fr
        # File duration: media length covers out_f.
        file_dur_f = out_f + 25
        tl_start_s = frames_to_seconds(start_fr, fps)
        tl_end_s = frames_to_seconds(end_fr, fps)
        src_in_exact = frames_to_seconds(Fraction(in_f, 1), fps)
        src_out_exact = frames_to_seconds(Fraction(out_f, 1), fps)
        items.append({
            "key": key, "kind": "audio", "track": 1,
            "file": media[key],
            "timeline_start_s": f"{tl_start_s.numerator}/{tl_start_s.denominator}",
            "timeline_end_s": f"{tl_end_s.numerator}/{tl_end_s.denominator}",
            "source_in_s": f"{src_in_exact.numerator}/{src_in_exact.denominator}",
            "source_out_s": f"{src_out_exact.numerator}/{src_out_exact.denominator}",
            "start_frame": start_whole, "subframeoffset_80": sub,
            "source_channels": [0],
            "marker_source_samples": selected_markers(src_in_exact, src_out_exact),
        })
        seq_dur_f = max(seq_dur_f, end_whole + (1 if sub else 0))
        wav = media[key]
        file_ref = (f'                <file id="file-{key}">\n'
                    f'                  <name>{Path(wav).name}</name>\n'
                    f'                  <pathurl>{file_url(Path(wav))}</pathurl>\n'
                    f'                  <duration>{file_dur_f}</duration>\n'
                    f'                  <rate><timebase>{tb}</timebase><ntsc>{ntsc}</ntsc></rate>\n'
                    '                  <media><audio><samplecharacteristics><depth>16</depth>'
                    '<samplerate>48000</samplerate></samplecharacteristics>'
                    '<channelcount>1</channelcount></audio></media>\n'
                    '                </file>\n')
        audio_clips.append(clipitem_xml(
            f"clipitem-{key}", Path(wav).name, start_whole, end_whole,
            in_f, out_f, file_dur_f, rate_xml, sub or None, [], file_ref, 1))

    # Camera linked group: 1 video + stereo audio on A2/A3.
    cam_path = media["cam"]
    cam_start, cam_dur = Fraction(0, 1), Fraction(CAM_DURATION_FRAMES, 1)
    cam_end = cam_start + cam_dur
    cam_file_dur = CAM_DURATION_FRAMES + 25
    cam_rate = rate_xml
    cam_file_ref = (f'                <file id="file-cam">\n'
                    f'                  <name>{Path(cam_path).name}</name>\n'
                    f'                  <pathurl>{file_url(Path(cam_path))}</pathurl>\n'
                    f'                  <duration>{cam_file_dur}</duration>\n'
                    f'                  <rate><timebase>{tb}</timebase><ntsc>{ntsc}</ntsc></rate>\n'
                    '                  <timecode><string>00:00:00:00</string>'
                    '<displayformat>NDF</displayformat>\n'
                    f'                    <rate><timebase>{tb}</timebase><ntsc>{ntsc}</ntsc></rate>\n'
                    '                  </timecode>\n'
                    '                  <media><video><samplecharacteristics><width>640</width>'
                    '<height>360</height></samplecharacteristics></video>'
                    '<audio><samplecharacteristics><depth>16</depth>'
                    '<samplerate>48000</samplerate></samplecharacteristics>'
                    '<channelcount>2</channelcount></audio></media>\n'
                    '                </file>\n')
    cam_file_ref2 = '                <file id="file-cam"/>\n'
    # Production-style links: video has no groupindex, audios share group 1.
    # Audio trackindex counts across all audio tracks (A1 recorders = 1).
    v_links = [("clipitem-cam-v", "video", 1, 1, None),
               ("clipitem-cam-a1", "audio", 2, 1, 1),
               ("clipitem-cam-a2", "audio", 3, 1, 1)]
    tl0 = frames_to_seconds(cam_start, fps)
    tl1 = frames_to_seconds(cam_end, fps)
    for ch in (0, 1):
        items.append({
            "key": f"cam_ch{ch}", "kind": "audio", "track": 2 + ch,
            "file": cam_path,
            "timeline_start_s": f"{tl0.numerator}/{tl0.denominator}",
            "timeline_end_s": f"{tl1.numerator}/{tl1.denominator}",
            "source_in_s": "0/1",
            "source_out_s": f"{tl1.numerator}/{tl1.denominator}",
            "start_frame": 0, "subframeoffset_80": 0,
            "source_channels": [ch],
            "marker_source_samples": selected_markers(Fraction(0), tl1),
        })
    items.append({
        "key": "cam_video", "kind": "video", "track": 1, "file": cam_path,
        "timeline_start_s": f"{tl0.numerator}/{tl0.denominator}",
        "timeline_end_s": f"{tl1.numerator}/{tl1.denominator}",
        "source_in_s": "0/1",
        "source_out_s": f"{tl1.numerator}/{tl1.denominator}",
        "start_frame": 0, "subframeoffset_80": 0,
    })
    seq_dur_f = max(seq_dur_f, CAM_DURATION_FRAMES)
    video_clip = clipitem_xml(
        "clipitem-cam-v", Path(cam_path).name, 0, CAM_DURATION_FRAMES,
        0, CAM_DURATION_FRAMES, cam_file_dur, cam_rate, None, v_links,
        cam_file_ref, None)
    audio_ch1 = clipitem_xml(
        "clipitem-cam-a1", Path(cam_path).name, 0, CAM_DURATION_FRAMES,
        0, CAM_DURATION_FRAMES, cam_file_dur, cam_rate, None, v_links,
        cam_file_ref2, 1)
    audio_ch2 = clipitem_xml(
        "clipitem-cam-a2", Path(cam_path).name, 0, CAM_DURATION_FRAMES,
        0, CAM_DURATION_FRAMES, cam_file_dur, cam_rate, None, v_links,
        cam_file_ref2, 2)

    tc_frame = tb * 3600
    xml = ('<?xml version="1.0" encoding="UTF-8"?>\n<!DOCTYPE xmeml>\n<xmeml version="5">\n'
           '  <project>\n    <name>AlignFrac</name>\n    <children>\n'
           f'      <sequence id="sequence-1">\n        <name>{seq_name}</name>\n'
           f'        <duration>{seq_dur_f}</duration>\n'
           f'        <rate><timebase>{tb}</timebase><ntsc>{ntsc}</ntsc></rate>\n'
           '        <timecode>\n          <string>01:00:00:00</string>\n'
           f'          <frame>{tc_frame}</frame>\n          <displayformat>NDF</displayformat>\n'
           f'          <rate><timebase>{tb}</timebase><ntsc>{ntsc}</ntsc></rate>\n'
           '        </timecode>\n'
           '        <media>\n          <video>\n'
           '            <format><samplecharacteristics>\n'
           f'              <rate><timebase>{tb}</timebase><ntsc>{ntsc}</ntsc></rate>\n'
           '              <width>640</width>\n              <height>360</height>\n'
           '              <pixelaspectratio>square</pixelaspectratio>\n'
           '              <fielddominance>none</fielddominance>\n'
           '            </samplecharacteristics></format>\n'
           '            <track>\n' + video_clip +
           '              <enabled>TRUE</enabled><locked>FALSE</locked>\n            </track>\n'
           '          </video>\n          <audio>\n'
           '            <format><samplecharacteristics><depth>16</depth>'
           '<samplerate>48000</samplerate></samplecharacteristics></format>\n'
           '            <track>\n' + "".join(audio_clips) +
           '              <enabled>TRUE</enabled><locked>FALSE</locked>\n            </track>\n'
           '            <track>\n' + audio_ch1 +
           '              <enabled>TRUE</enabled><locked>FALSE</locked>\n            </track>\n'
           '            <track>\n' + audio_ch2 +
           '              <enabled>TRUE</enabled><locked>FALSE</locked>\n            </track>\n'
           '          </audio>\n        </media>\n      </sequence>\n'
           '    </children>\n  </project>\n</xmeml>\n')
    return xml, items, seq_dur_f


def sha256(p: Path) -> str:
    return hashlib.sha256(p.read_bytes()).hexdigest()


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--out", default="/tmp/align-premiere-frac")
    args = ap.parse_args()
    out = Path(args.out)
    if out.exists():
        # Never silently mix runs: require empty or new directory.
        if any(out.iterdir()):
            raise SystemExit(f"refusing to reuse non-empty {out}; remove it or pass --out")
    out.mkdir(parents=True, exist_ok=True)

    # Media: recorder WAVs sized to cover out + margin; camera 16 s / 14 s.
    media25: dict = {}
    media29: dict = {}
    for key, _, dur_fr, src_in_f, freq in RECORDER_LAYOUT:
        need_s = src_in_f / 25.0 + dur_fr / 25.0 + 1.0
        p = out / f"{key}_25.wav"
        write_mono_wav(p, need_s, freq, selected_markers(Fraction(src_in_f, 25), Fraction(src_in_f + dur_fr, 25)))
        media25[key] = str(p)
        need_s29 = src_in_f / (30000 / 1001) + dur_fr / (30000 / 1001) + 1.0
        p29 = out / f"{key}_2997.wav"
        write_mono_wav(p29, need_s29, freq, selected_markers(Fraction(src_in_f * 1001, 30000), Fraction((src_in_f + dur_fr) * 1001, 30000)))
        media29[key] = str(p29)

    for fps_key, seconds, media in (("25", 16.0, media25), ("2997", 14.0, media29)):
        stereo = out / f"cam_stereo_{fps_key}.wav"
        write_stereo_wav(stereo, seconds, selected_markers(Fraction(0), Fraction(CAM_DURATION_FRAMES) / FPS_PRESETS[fps_key]["fps"]))
        fps_str = "25" if fps_key == "25" else "30000/1001"
        mov = out / f"cam_{fps_key}.mov"
        make_video(mov, fps_str, seconds, stereo)
        media["cam"] = str(mov)

    manifest = {"sample_rate": SR, "click_offset_samples": CLICK_OFFSET_SAMPLES,
                "sequences": {}}
    outputs = []
    for fps_key, seq_name in (("25", "AlignFrac 25"), ("2997", "AlignFrac 2997")):
        preset = FPS_PRESETS[fps_key]
        media = media25 if fps_key == "25" else media29
        xml, items, seq_dur = build_sequence(seq_name, preset, media, fps_key)
        xml_path = out / f"align-frac-{fps_key}.xml"
        xml_path.write_text(xml, encoding="utf-8")
        outputs.append(str(xml_path))
        manifest["sequences"][fps_key] = {
            "sequence": seq_name,
            "fps": f"{preset['fps'].numerator}/{preset['fps'].denominator}",
            "xml": str(xml_path),
            "items": items,
        }

    manifest_path = out / "ground-truth.json"
    manifest_path.write_text(json.dumps(manifest, indent=2), encoding="utf-8")
    media_files = []
    for seq in manifest["sequences"].values():
        for it in seq["items"]:
            media_files.append(Path(it["file"]))
    hashes = {str(p): sha256(Path(p)) for p in [manifest_path, *outputs, *media_files]}
    (out / "input-hashes.json").write_text(json.dumps(hashes, indent=2), encoding="utf-8")
    print(json.dumps({"out": str(out), "manifest": str(manifest_path),
                      "xml": outputs}, indent=2))


if __name__ == "__main__":
    main()
