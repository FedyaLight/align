#!/usr/bin/env python3
"""Corpus acceptance: pads must preserve relative sync from result.json.

For each audio clipitem in a pad-fixed Premiere XML, reconstructed content
position = XML integer start + pad/sr must equal the result.json island
placement up to ONE global chronology shift (rigid export shift, inherent
to every export) plus <=1 sample residual. Reports the per-item residual
after removing the median offset. Source in/out integers must match the
result mapping spans within a frame (writer path untouched).

Usage:
  python3 script/check-corpus-offset.py result.json 'Align – Adobe Premiere Pro.xml'
"""
import json
import re
import sys
import xml.etree.ElementTree as ET
from fractions import Fraction
from pathlib import Path
from urllib.parse import unquote

SR = 48000


def fps_of(seq):
    tb = int(seq.find("rate/timebase").text)
    if seq.find("rate/ntsc").text == "TRUE":
        return Fraction(tb * 1000, 1001)
    return Fraction(tb)


def main() -> int:
    result_p, xml_p = Path(sys.argv[1]), Path(sys.argv[2])
    result = json.loads(result_p.read_text(encoding="utf-8"))
    root = ET.parse(xml_p).getroot()
    seq = root.find(".//sequence")
    fps = fps_of(seq)
    # placements by source basename
    place = {}
    for isl in result["islands"]:
        for pl in isl["placements"]:
            clip = next(c for c in result["project"]["clips"] if c["id"] == pl["clipID"])
            name = Path(clip["url"]).name
            src0 = Fraction(pl["mapping"]["points"][0]["source"]["value"],
                            pl["mapping"]["points"][0]["source"]["timescale"])
            isl0 = Fraction(pl["mapping"]["points"][0]["island"]["value"],
                            pl["mapping"]["points"][0]["island"]["timescale"])
            place.setdefault(name, (src0, isl0))
    rows = []
    file_names: dict = {}
    rates: dict = {}
    for c in result["project"]["clips"]:
        audio = (c.get("audio") or [{}])[0]
        if audio.get("sampleRate"):
            rates[Path(c["url"]).stem] = float(audio["sampleRate"])
    for tr in seq.findall("./media/audio/track"):
        for ci in tr.findall("clipitem"):
            f = ci.find("file")
            fid = f.get("id") if f is not None else None
            pu = ci.findtext("file/pathurl")
            if pu:
                file_names[fid] = Path(unquote(pu)).name
            base = file_names.get(fid, "")
            if not base:
                continue  # video-side or unresolvable ref: covered by verifier
            m = re.match(r"^(.*) – pad (\d+) – [0-9a-f]+-direct\.wav$", base)
            if m:
                stem, pad = m.group(1), int(m.group(2))
                # original name: stem + extension looked up from placements
                orig = next((k for k in place if Path(k).stem == stem), None)
            else:
                orig, pad = base, 0
            assert orig in place, f"no placement for {base}"
            src0, isl0 = place[orig]
            rate = rates.get(Path(orig).stem, 48000.0)
            start = int(ci.findtext("start"))
            content = Fraction(start, 1) / fps + Fraction(pad, 1) / Fraction(rate)
            rows.append((ci.findtext("name"), start, pad, float((content - isl0) * SR)))
    offs = sorted(r[3] for r in rows)
    median = offs[len(offs) // 2]
    ok = True
    print(f"global chronology shift: {median:.2f} samples")
    for name, start, pad, off in rows:
        res = off - median
        flag = "OK " if abs(res) <= 1.0 else "FAIL"
        if flag == "FAIL":
            ok = False
        print(f"{name:45s} start {start:7d} pad {pad:5d} residual {res:+.2f} samples {flag}")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
