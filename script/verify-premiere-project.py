#!/usr/bin/env python3
"""Read-only, fail-closed comparison of a saved Premiere import with FCP7 XML.

Supports one unretimed sequence without transitions. Premiere's private project
schema is version-dependent; unknown/missing nodes fail instead of being skipped.
A saved project is NLE readback evidence, not an acoustic or playback check.
"""
import argparse
import gzip
import hashlib
import json
from fractions import Fraction
from pathlib import Path
import xml.etree.ElementTree as ET

TICKS = 254016000000


def rate(node):
    base = int(node.findtext('timebase'))
    return Fraction(base * 1000, 1001) if node.findtext('ntsc') == 'TRUE' else Fraction(base)


def verify(xml_path, project_path):
    original = ET.parse(xml_path).getroot()
    sequences = original.findall('.//sequence')
    assert len(sequences) == 1, 'Expected exactly one input sequence'
    seq = sequences[0]
    assert not seq.findall('.//transitionitem'), 'Transitions unsupported'
    assert not seq.findall('.//filter'), 'Filters/retimes unsupported'
    project = ET.fromstring(gzip.decompress(project_path.read_bytes()))
    actual_sequences = project.findall('Sequence')
    assert len(actual_sequences) == 1, 'Use an isolated single-sequence project'
    actual_seq = actual_sequences[0]
    assert actual_seq.findtext('Name') == seq.findtext('name'), 'Wrong sequence'
    objects = {}
    for node in project:
        for key in ('ObjectID', 'ObjectUID'):
            if key in node.attrib:
                assert node.attrib[key] not in objects, 'Duplicate object id'
                objects[node.attrib[key]] = node

    def deref(node):
        assert node is not None, 'Missing reference'
        return objects[node.get('ObjectRef') or node.get('ObjectURef')]

    fps = rate(seq.find('rate'))
    tick_frame = Fraction(TICKS, 1) / fps
    report = {'sequence': seq.findtext('name'), 'fps': str(fps), 'items': [],
              'errors': [], 'limitations': ['Private Premiere project schema',
              'No playback or acoustic correctness check',
              'No unmatched-policy coverage unless present in supplied fixture']}
    groups = [deref(n.find('Second')) for n in actual_seq.findall('./TrackGroups/TrackGroup')]
    id_map = {}
    for kind in ('video', 'audio'):
        group = next(g for g in groups if g.tag == kind.title() + 'TrackGroup')
        if kind == 'video':
            assert int(group.findtext('./TrackGroup/FrameRate')) == tick_frame, 'Wrong FPS'
        expected_tracks = seq.findall(f'./media/{kind}/track')
        actual_tracks = group.findall('./TrackGroup/Tracks/Track')
        assert len(expected_tracks) == len(actual_tracks), f'{kind}: track count'
        for expected_track, track_ref in zip(expected_tracks, actual_tracks):
            track = deref(track_ref)
            track_index = int(track_ref.get('Index')) + 1
            expected_items = expected_track.findall('clipitem')
            actual_items = [deref(n) for n in track.findall('./ClipTrack/ClipItems/TrackItems/TrackItem')]
            assert len(expected_items) == len(actual_items), f'{kind}{track_index}: count'
            for expected, actual in zip(expected_items, actual_items):
                sub = deref(actual.find('./ClipTrackItem/SubClip'))
                assert sub.findtext('Name') == expected.findtext('name'), 'Clip order/name mismatch'
                clip = deref(sub.find('Clip'))
                source_rate = rate(expected.find('rate'))
                source_tick_frame = Fraction(TICKS, 1) / source_rate
                start = int(actual.findtext('./ClipTrackItem/TrackItem/Start', '0'))
                end = int(actual.findtext('./ClipTrackItem/TrackItem/End'))
                subframe = Fraction(int(expected.findtext('subframeoffset', '0')), 80)
                wanted_start = (int(expected.findtext('start')) + subframe) * tick_frame
                wanted_end = (int(expected.findtext('end')) + subframe) * tick_frame
                row = {'type': kind, 'track': track_index, 'name': sub.findtext('Name'),
                       'start_error_seconds': float((start - wanted_start) / TICKS),
                       'end_error_seconds': float((end - wanted_end) / TICKS)}
                for field, wanted in [('InPoint', int(expected.findtext('in'))),
                                      ('OutPoint', int(expected.findtext('out')))]:
                    value = int(clip.findtext('./Clip/' + field, '0' if field == 'InPoint' else None))
                    assert value == wanted * source_tick_frame, f'{row}: {field}'
                if kind == 'audio':
                    channels = [int(deref(n).findtext('ChannelIndex')) for n in
                                clip.findall('./SecondaryContents/SecondaryContentItem')]
                    wanted_channel = int(expected.findtext('./sourcetrack/trackindex')) - 1
                    assert channels == [wanted_channel], f'{row}: channels {channels}'
                    row['source_channels'] = channels
                for field in ('start_error_seconds', 'end_error_seconds'):
                    if abs(row[field]) > 1 / 48000:
                        report['errors'].append(f'{kind}{track_index} {row["name"]}: {field}={row[field]}')
                id_map[expected.get('id')] = actual.get('ObjectID')
                report['items'].append(row)
    assert report['items'], 'Empty coverage'
    actual_links = {frozenset(n.get('ObjectRef') for n in deref(ref).findall('./TrackItemGroup/TrackItems/TrackItem'))
                    for ref in actual_seq.findall('./PersistentGroupContainer/LinkContainer/Links/Link')}
    expected_links = {frozenset(id_map[n.findtext('linkclipref')] for n in item.findall('link'))
                      for item in seq.findall('.//clipitem') if item.findall('link')}
    assert actual_links == expected_links, 'A/V link groups differ'
    report['av_link_groups'] = len(actual_links)
    report['sha256'] = {str(p): hashlib.sha256(p.read_bytes()).hexdigest() for p in (xml_path, project_path)}
    report['passed'] = not report['errors']
    return report


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('xml', type=Path)
    parser.add_argument('project', type=Path)
    args = parser.parse_args()
    result = verify(args.xml, args.project)
    print(json.dumps(result, indent=2, ensure_ascii=False))
    raise SystemExit(0 if result['passed'] else 1)
