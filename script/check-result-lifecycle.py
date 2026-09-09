#!/usr/bin/env python3
"""Verify that inspect/sync/export results remain usable after the CLI exits."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import random
import shutil
import struct
import subprocess
from urllib.parse import unquote, urlparse
import wave
import xml.etree.ElementTree as ET

import aaf2


def local_path(value):
    if value.startswith('file:'):
        value = unquote(urlparse(value).path)
        if os.name == 'nt' and len(value) > 2 and value[0] == '/' and value[2] == ':':
            value = value[1:]
    return Path(value)


def check_references(artifacts):
    references = set()
    for artifact in artifacts:
        path = Path(artifact['url'])
        assert path.is_file(), path
        if path.suffix in ('.xml', '.fcpxml'):
            root = ET.parse(path).getroot()
            references.update(node.text for node in root.iter('pathurl') if node.text)
            references.update(node.attrib['src'] for node in root.iter() if 'src' in node.attrib)
        elif path.suffix == '.otio':
            def walk(value):
                if isinstance(value, dict):
                    if 'target_url' in value:
                        references.add(value['target_url'])
                    for child in value.values():
                        walk(child)
                elif isinstance(value, list):
                    for child in value:
                        walk(child)
            walk(json.loads(path.read_text(encoding='utf-8')))
        elif path.suffix == '.aaf':
            with aaf2.open(str(path)) as container:
                for mob in container.content.mobs:
                    if isinstance(mob, aaf2.mobs.SourceMob) and mob.descriptor is not None:
                        for locator in mob.descriptor['Locator'].value or []:
                            references.add(locator['URLString'].value)
        elif path.suffix == '.py':
            compile(path.read_text(encoding='utf-8'), str(path), 'exec')
    assert references, 'No media references checked'
    missing = [value for value in references if not local_path(value).is_file()]
    assert not missing, f'Export references missing media: {missing}'
    return len(references)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('cli', type=Path)
    parser.add_argument('output', type=Path)
    args = parser.parse_args()
    root = args.output.resolve()
    root.mkdir(parents=True, exist_ok=False)
    rng = random.Random(444)
    pcm = b''.join(struct.pack('<h', rng.randrange(-18000, 18001)) for _ in range(8 * 48000))
    source = root / 'source.wav'
    with wave.open(str(source), 'wb') as writer:
        writer.setparams((1, 2, 48000, 0, 'NONE', 'PCM'))
        writer.writeframes(pcm)
    project = root / 'embedded.aaf'
    with aaf2.open(str(project), 'w') as container:
        mob = container.create.SourceMob('Embedded mono')
        container.content.mobs.append(mob)
        slot = mob.import_audio_essence(str(source))
        for name in ['First', 'Second']:
            composition = container.create.CompositionMob(name)
            composition.usage = 'Usage_TopLevel'
            container.content.mobs.append(composition)
            track = composition.create_sound_slot(48000)
            track.segment.components.append(mob.create_source_clip(
                slot_id=slot.slot_id, start=0, length=8 * 48000, media_kind='sound'))
            track.segment.length = 8 * 48000
    source.unlink()
    original_hash = hashlib.sha256(project.read_bytes()).hexdigest()
    env = dict(os.environ, ALIGN_BACKEND='portable')

    def run(name, arguments, success=True):
        process = subprocess.run([str(args.cli.resolve()), *map(str, arguments)],
            env=env, capture_output=True, text=True, encoding='utf-8', timeout=180)
        (root / f'{name}.stderr').write_text(process.stderr, encoding='utf-8')
        (root / f'{name}.json').write_text(process.stdout, encoding='utf-8')
        if not success:
            assert process.returncode != 0, f'{name} unexpectedly succeeded'
            return None
        assert process.returncode == 0, f'{name}: {process.stderr}'
        return json.loads(process.stdout)

    # A full-span mono clip needs neither correction nor precision stems.
    # Every writer must nevertheless retain its source beyond process exit.
    direct = run('direct', ['export', '--sequence', '1', root / 'direct', project])
    reference_count = check_references(direct)
    single = root / 'single.aaf'
    shutil.copyfile(project, single)
    with aaf2.open(str(single), 'rw') as container:
        container.content.mobs.pop(list(container.content.toplevel())[1].mob_id)
    inspected = run('inspect', [single])
    assert all(Path(clip['url']).is_file() for clip in inspected['clips'])
    results = run('batch', ['sync', '--all-sequences', project])
    assert len(results) == 2
    for result in results:
        for clip in result['project']['clips']:
            with wave.open(clip['url'], 'rb') as reader:
                assert reader.readframes(reader.getnframes()) == pcm
    batch_artifacts = run('batch-export', ['export-json', root / 'batch.json', root / 'batch-export'])
    assert len(batch_artifacts) == 8  # Premiere and FCPXML each combine both sequences.
    reference_count += check_references(batch_artifacts)
    aaf_artifacts = run('batch-aaf', ['export-json', '--aaf', root / 'batch.json', root / 'batch-aaf'])
    assert len(aaf_artifacts) == 1
    with aaf2.open(aaf_artifacts[0]['url']) as container:
        assert len(list(container.content.toplevel())) == 2
    reference_count += check_references(aaf_artifacts)
    (root / 'empty.json').write_text('[]', encoding='utf-8')
    run('empty-export', ['export-json', root / 'empty.json', root / 'empty-output'], success=False)
    assert not (root / 'empty-output').exists()
    assert hashlib.sha256(project.read_bytes()).hexdigest() == original_hash
    report = dict(passed=True, sequences=2, checked_references=reference_count,
        direct_export_survives_exit=True, inspect_survives_exit=True,
        saved_batch_exports=True, empty_batch_rejected=True, original_unchanged=True)
    (root / 'verification.json').write_text(json.dumps(report, indent=2) + '\n', encoding='utf-8')
    print(json.dumps(report, indent=2))


if __name__ == '__main__':
    main()
