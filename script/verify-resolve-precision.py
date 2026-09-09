#!/usr/bin/env python3
"""Read-only acceptance of recorder placement in an imported Align OTIO script timeline."""
import argparse
import json
import os

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("otio")
parser.add_argument("--project", required=True)
parser.add_argument("--timeline", required=True)
args = parser.parse_args()
try:
    import DaVinciResolveScript as dvr
except ModuleNotFoundError as error:
    raise SystemExit('Add the Resolve Scripting/Modules directory to PYTHONPATH before running this check.') from error

resolve = dvr.scriptapp('Resolve')
if not resolve:
    raise SystemExit('Resolve is unavailable')
project = resolve.GetProjectManager().GetCurrentProject()
if not project or project.GetName() != args.project:
    raise SystemExit('Current project does not match --project; nothing changed')
timeline = project.GetCurrentTimeline()
if not timeline or timeline.GetName() != args.timeline:
    raise SystemExit('Current timeline does not match --timeline; nothing changed')
with open(args.otio, encoding='utf-8') as source:
    document = json.load(source)
rate = float(document['global_start_time']['rate'])
base = timeline.GetStartFrame()
results = []
failures = []
tracks = [t for t in document['tracks']['children'] if t['kind'] == 'Audio']
for index, track in enumerate(tracks, 1):
    items = timeline.GetItemListInTrack('audio', index) or []
    clips = [c for c in track['children'] if c['OTIO_SCHEMA'] == 'Clip.2']
    if len(items) != len(clips):
        failures.append(f'Track {index}: expected {len(clips)} clips, got {len(items)}')
        continue
    cursor = 0.0
    position = 0
    for child in track['children']:
        if child['OTIO_SCHEMA'] not in ('Clip.2', 'Gap.1'):
            raise SystemExit('Transitions are not supported by this verifier')
        duration = child['source_range']['duration']
        seconds = duration['value'] / duration['rate']
        if child['OTIO_SCHEMA'] == 'Clip.2':
            item = items[position]
            position += 1
            if 'Link Group ID' not in child.get('metadata', {}).get('Resolve_OTIO', {}):
                start_error = (item.GetStart(True) - base) / rate - cursor
                duration_error = (item.GetEnd(True) - item.GetStart(True)) / rate - seconds
                reference = child['media_references'][child['active_media_reference_key']]
                expected_path = os.path.realpath(reference['target_url'])
                media = item.GetMediaPoolItem()
                actual_path = os.path.realpath(str(media.GetClipProperty('File Path')))
                identity_matches = actual_path == expected_path
                ok = identity_matches and max(abs(start_error), abs(duration_error)) <= 1 / 48000
                results.append(dict(name=item.GetName(), track=index, ok=ok,
                                    expected_path=expected_path, actual_path=actual_path,
                                    start_error_seconds=start_error,
                                    duration_error_seconds=duration_error))
                if not ok:
                    failures.append(f'Track {index}: {child["name"]} does not conform')
        cursor += seconds
if not results:
    failures.append('No recorder clips verified')
print(json.dumps(dict(ok=not failures, clips=results, failures=failures), indent=2))
raise SystemExit(bool(failures))
