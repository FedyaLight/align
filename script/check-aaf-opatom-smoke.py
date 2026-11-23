#!/usr/bin/env python3
"""Check linked MXF OP-Atom export through Align CLI and independent AAF readback."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
from urllib.parse import unquote, urlsplit

import aaf2


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('cli', type=Path)
    parser.add_argument('output', type=Path)
    args = parser.parse_args()
    root = args.output.resolve()
    root.mkdir(parents=True, exist_ok=True)
    env = dict(os.environ, ALIGN_BACKEND='portable')

    def run(name, *arguments):
        process = subprocess.run([str(args.cli.absolute()), *map(str, arguments)],
            env=env, capture_output=True, text=True, encoding='utf-8', timeout=180)
        (root / f'{name}.stderr').write_text(process.stderr, encoding='utf-8')
        (root / f'{name}.json').write_text(process.stdout, encoding='utf-8')
        return process

    for container in ('mxf_opatom', 'mxf'):
        atom = container == 'mxf_opatom'
        media = root / ('picture # atom.mxf' if atom else 'picture OP1a.mxf')
        subprocess.run(['ffmpeg', '-v', 'error', '-y', '-f', 'lavfi', '-i',
            'testsrc2=size=1280x720:rate=25', '-frames:v', '50', '-timecode', '01:02:03:04',
            '-c:v', 'dnxhd', '-profile:v', 'dnxhr_lb', '-pix_fmt', 'yuv422p',
            '-f', container, str(media)], check=True, timeout=30)
        initial_hash = hashlib.sha256(media.read_bytes()).hexdigest()
        probe = json.loads(subprocess.check_output(['ffprobe', '-v', 'error',
            '-show_format', '-show_streams', '-of', 'json', str(media)], timeout=30))
        assert [stream['codec_type'] for stream in probe['streams']] == ['video']
        source = root / f'{container}.xml'
        source.write_text(f'''<?xml version="1.0" encoding="UTF-8"?>
<xmeml version="4"><sequence><name>{container}</name>
<duration>29</duration><rate><timebase>25</timebase><ntsc>FALSE</ntsc></rate>
<media><video><track>
<clipitem id="edit"><name>Trimmed picture</name><start>12</start><end>29</end>
<in>7</in><out>24</out>
<file id="picture"><name>Picture</name><pathurl>{media.as_uri()}</pathurl>
<rate><timebase>25</timebase><ntsc>FALSE</ntsc></rate><duration>50</duration>
</file></clipitem>
</track></video></media></sequence></xmeml>
''', encoding='utf-8')
        sync = run(f'{container}-sync', 'sync', '--preserve-basic-editing', 'V1', source)
        assert sync.returncode == 0, sync.stderr
        project = json.loads(sync.stdout)['project']
        assert not project['warnings'] and len(project['importedTimeline']['edits']) == 1
        name = f'{container}-export'
        export = run(name, 'export-json', '--aaf', '--no-drift', root / f'{container}-sync.json', root / name)
        assert hashlib.sha256(media.read_bytes()).hexdigest() == initial_hash
        if not atom:
            assert export.returncode != 0 and 'Unsupported MXF operational pattern' in export.stderr, export.stderr
            assert not list((root / name).rglob('*.aaf'))
            continue

        assert export.returncode == 0, export.stderr
        output = Path(json.loads(export.stdout)[0]['url'])
        assert list(output.parent.iterdir()) == [output]
        with aaf2.open(str(output)) as aaf:
            assert not list(aaf.content.essencedata)
            timeline = next(aaf.content.toplevel())
            slot = next(slot for slot in timeline.slots if slot.media_kind == 'Picture')
            assert str(slot.edit_rate) == '25'
            gap, edit = list(slot.segment.components)
            assert isinstance(gap, aaf2.components.Filler) and gap.length == 12
            assert (edit.start, edit.length) == (7, 17)
            master = edit.mob
            assert urn_hex(master) == probe['format']['tags']['material_package_umid'][2:].lower()
            assert str(edit.slot.edit_rate) == '25' and edit.slot.segment.length == 50
            source_clip = edit.slot.segment
            file = source_clip.mob
            assert urn_hex(file) == probe['streams'][0]['tags']['file_package_umid'][2:].lower()
            assert str(source_clip.slot.edit_rate) == '25' and source_clip.slot.segment.length == 50
            desc = file.descriptor
            assert desc.classdef.class_name == 'CDCIDescriptor'
            assert desc['ContainerFormat'].value.name == 'ContainerDef_AAFKLV'
            assert desc.length == 50 and str(desc['SampleRate'].value) == '25'
            assert str(desc['Compression'].value) == '04010202-7128-0000-060e-2b340401010a'
            assert (desc['StoredWidth'].value, desc['StoredHeight'].value) == (1280, 720)
            assert (desc['ComponentWidth'].value, desc['HorizontalSubsampling'].value) == (8, 2)
            [locator] = desc['Locator'].value
            # Normalize Windows drive-URI spelling the same way urllib writes it.
            assert unquote(urlsplit(locator['URLString'].value).path) == unquote(urlsplit(media.as_uri()).path)
            tape = source_clip.slot.segment.mob
            code = next(slot for slot in tape.slots if slot.media_kind == 'Timecode')
            assert str(code.edit_rate) == '25'
            assert (code.segment.start, code.segment.fps, code.segment.drop, code.segment.length) == (93079, 25, False, 50)
        print(f'Linked DNxHR LB OP-Atom structure passed: {output}')
    print('MXF OP1a rejected with an operational-pattern diagnostic.')


def urn_hex(mob):
    return str(mob.mob_id).removeprefix('urn:smpte:umid:').replace('.', '')


if __name__ == '__main__':
    main()
