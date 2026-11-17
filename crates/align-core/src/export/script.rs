//! Resolve precision importer script with sample-grid placement and
//! render-compatible audio-only timelines. When the OTIO lists media bins,
//! the script imports into a new media pool folder and files each source
//! into a subfolder per camera and recorder. Resolve imports neither clip
//! names nor colors, so the script applies them and the enabled state.

pub fn write() -> &'static str {
    r#"#!/usr/bin/env python3
import json
import math
import os
import sys

sys.path.append('/Library/Application Support/Blackmagic Design/DaVinci Resolve/Developer/Scripting/Modules')
import DaVinciResolveScript as dvr


def seconds(value):
    return float(value['value']) / float(value['rate'])


directory = os.path.dirname(os.path.abspath(__file__))
otio_path = os.path.join(directory, 'Align – DaVinci Resolve.otio')
xml_path = os.path.join(directory, 'Align – DaVinci Resolve.xml')
if not os.path.isfile(otio_path):
    raise RuntimeError('Keep this script beside Align – DaVinci Resolve.otio')

resolve = dvr.scriptapp('Resolve')
if not resolve:
    raise RuntimeError('Open DaVinci Resolve and enable local external scripting first')
project = resolve.GetProjectManager().GetCurrentProject()
if not project:
    raise RuntimeError('Open a DaVinci Resolve project first')
pool = project.GetMediaPool()
document = json.load(open(otio_path, encoding='utf-8'))
warnings = []


def collect_media(folder):
    result = list(folder.GetClipList() or [])
    for child in folder.GetSubFolderList() or []:
        result.extend(collect_media(child))
    return result


def media_paths(items):
    paths = {}
    for item in items:
        path = item.GetClipProperty('File Path')
        if path:
            paths[os.path.realpath(str(path))] = item
    return paths


def unique_name(names, base):
    name = base
    suffix = 2
    while name in names:
        name = f'{base} {suffix}'
        suffix += 1
    return name


bins = (document.get('metadata') or {}).get('Align') or {}
media_bins = bins.get('mediaBins') or []
export_folder = None
if media_bins:
    # Resolve imports a timeline's media anew on every import, so each run
    # gets a complete folder of its own.
    root = pool.GetRootFolder()
    export_folder = pool.AddSubFolder(root, unique_name(
        {folder.GetName() for folder in root.GetSubFolderList() or []},
        document.get('name') or 'Align',
    ))
    if not export_folder or not pool.SetCurrentFolder(export_folder):
        raise RuntimeError('Resolve could not create the media pool folder')


def organize_media():
    """File this import's clips into one folder per camera and recorder."""
    if export_folder is None:
        return
    imported = media_paths(export_folder.GetClipList() or [])
    for media_bin in media_bins:
        folder = pool.AddSubFolder(export_folder, media_bin['name'])
        if not folder:
            raise RuntimeError(f"Resolve could not create the {media_bin['name']} folder")
        clips = []
        missing = []
        for path in media_bin['media']:
            real = os.path.realpath(path)
            if real in imported:
                clips.append(imported.pop(real))
            else:
                missing.append(path)
        if clips and not pool.MoveClips(clips, folder):
            raise RuntimeError(f"Resolve could not move clips into {media_bin['name']}")
        if missing:
            pool.SetCurrentFolder(folder)
            if len(pool.ImportMedia(missing) or []) < len(missing):
                print(f"Warning: Resolve did not import every file of {media_bin['name']}", file=sys.stderr)
    generated_audio = bins['generatedAudio']
    generated = [
        imported.pop(real)
        for real in dict.fromkeys(os.path.realpath(path) for path in generated_audio['media'])
        if real in imported
    ]
    if generated:
        folder = pool.AddSubFolder(export_folder, generated_audio['name'])
        if not folder or not pool.MoveClips(generated, folder):
            raise RuntimeError(f"Resolve could not move clips into {generated_audio['name']}")
    pool.SetCurrentFolder(export_folder)


name = unique_name(
    {
        project.GetTimelineByIndex(index).GetName()
        for index in range(1, project.GetTimelineCount() + 1)
    },
    (document.get('name') or 'Align') + ' – Sample Accurate',
)
rate = float(document['global_start_time']['rate'])
audio_tracks = [
    track for track in document['tracks']['children']
    if track['kind'] == 'Audio'
]
video_clips = [
    child
    for track in document['tracks']['children'] if track['kind'] == 'Video'
    for child in track['children'] if child['OTIO_SCHEMA'] == 'Clip.2'
]


# Resolve refuses clip names with file-name punctuation; these lookalikes
# keep a label such as "clip.wav ?" readable.
NAME_LOOKALIKES = str.maketrans({
    '?': '\uff1f', '/': '\u2044', ':': '\u2236', '*': '\u2217', '"': '\uff02',
    '<': '\u2039', '>': '\u203a', '|': '\u01c0', '\\': '\u29f5',
})


def apply_clip(item, clip):
    """Give a timeline item its OTIO name, color, and enabled state. Resolve
    imports neither clip names nor colors, and a re-placed clip starts out
    with its media's name, enabled."""
    name = clip.get('name')
    if (
        name
        and item.GetName() != name
        and not item.SetName(name)
        and not item.SetName(name.translate(NAME_LOOKALIKES))
    ):
        warnings.append(f'Could not name {name}')
    color = ((clip.get('metadata') or {}).get('Align') or {}).get('Resolve Clip Color')
    if color and not item.SetClipColor(color):
        warnings.append(f'Could not color {name} {color}')
    if not clip.get('enabled', True) and not item.SetClipEnabled(False):
        warnings.append(f'Could not disable {name}')


def otio_clips(track):
    return [child for child in track['children'] if child['OTIO_SCHEMA'] == 'Clip.2']


def append_audio(entry):
    source_rate = float(entry['media'].GetClipProperty('FPS') or rate)
    start = entry['source_in'] * source_rate
    sample_rate = float(entry['media'].GetClipProperty('Sample Rate') or 48000)
    # Resolve's audio renderer rounds fractional sample starts upward. Place
    # on the nearest sample first; a tiny inward epsilon avoids float noise
    # promoting an exact sample to its successor.
    relative_samples = (entry['record'] - timeline_start) / rate * sample_rate
    record = timeline_start + (math.floor(relative_samples + 0.5) - 0.0001) / sample_rate * rate
    record = max(timeline_start, record)
    clip_info = {
        'mediaPoolItem': entry['media'],
        'recordFrame': record,
        'mediaType': 2,
        'trackIndex': entry['track'],
    }
    if not entry['full_source']:
        clip_info['startFrame'] = start
        clip_info['endFrame'] = start + entry['duration'] * source_rate
    result = pool.AppendToTimeline([clip_info]) or []
    if len(result) != 1:
        raise RuntimeError(f"Resolve could not place {entry['name']}")
    actual = float(result[0].GetStart(True))
    if abs(actual - entry['record']) > rate / sample_rate:
        raise RuntimeError(f"Resolve quantized {entry['name']} to {actual}")
    apply_clip(result[0], entry['clip'])


if not video_clips:
    if not os.path.isfile(xml_path):
        raise RuntimeError('Keep this script beside Align – DaVinci Resolve.xml')
    timeline = pool.ImportTimelineFromFile(
        xml_path,
        {'timelineName': name, 'importSourceClips': True},
    )
    if not timeline:
        raise RuntimeError('Resolve could not create the audio timeline from XML')
    # Resolve 21 refuses even audio-only Deliver jobs without a video track.
    # An empty track is sufficient; no generated picture or extra media needed.
    if timeline.GetTrackCount('video') == 0 and not timeline.AddTrack('video'):
        raise RuntimeError('Resolve could not add the empty render-compatible video track')
    imported_items = [
        item
        for track_index in range(1, timeline.GetTrackCount('audio') + 1)
        for item in (timeline.GetItemListInTrack('audio', track_index) or [])
    ]
    if imported_items and not timeline.DeleteClips(imported_items, False):
        raise RuntimeError('Resolve could not clear the XML bootstrap clips')
    while timeline.GetTrackCount('audio') < len(audio_tracks):
        if not timeline.AddTrack('audio', 'mono'):
            raise RuntimeError('Resolve could not add an audio track')
    timeline_start = float(timeline.GetStartFrame())
    paths = []
    for track in audio_tracks:
        for child in track['children']:
            if child['OTIO_SCHEMA'] == 'Clip.2':
                reference = child['media_references'][child['active_media_reference_key']]
                path = reference['target_url']
                if path not in paths:
                    paths.append(path)
    # With media bins, use this run's own media like the timeline import does.
    media_by_path = media_paths(collect_media(export_folder or pool.GetRootFolder()))
    missing = [path for path in paths if os.path.realpath(path) not in media_by_path]
    imported = pool.ImportMedia(missing) or []
    media_by_path.update({
        os.path.realpath(str(item.GetClipProperty('File Path'))): item
        for item in imported
    })
    if any(os.path.realpath(path) not in media_by_path for path in paths):
        raise RuntimeError('Resolve could not import every audio source')
    placed = 0
    for track_index, track in enumerate(audio_tracks, 1):
        cursor = 0.0
        for child in track['children']:
            duration = seconds(child['source_range']['duration'])
            if child['OTIO_SCHEMA'] == 'Clip.2':
                reference = child['media_references'][child['active_media_reference_key']]
                path = reference['target_url']
                source_in = seconds(child['source_range']['start_time']) - seconds(
                    reference['available_range']['start_time']
                )
                available_duration = seconds(reference['available_range']['duration'])
                append_audio({
                    'media': media_by_path[os.path.realpath(path)],
                    'track': track_index,
                    'record': timeline_start + cursor * rate,
                    'source_in': source_in,
                    'duration': duration,
                    'full_source': abs(source_in) < 0.000001 and abs(duration - available_duration) < 0.00001,
                    'name': child['name'],
                    'clip': child,
                })
                placed += 1
            cursor += duration
    organize_media()
    project.SetCurrentTimeline(timeline)
    print(f'Created {timeline.GetName()} with {placed} sample-accurate audio clips')
    for warning in warnings:
        print('Warning:', warning, file=sys.stderr)
    sys.exit(0)

timeline = pool.ImportTimelineFromFile(
    otio_path,
    {'timelineName': name, 'importSourceClips': True},
)
if not timeline:
    raise RuntimeError('Resolve could not import the adjacent OTIO timeline')

timeline_start = float(timeline.GetStartFrame())
replacements = []

video_tracks = [track for track in document['tracks']['children'] if track['kind'] == 'Video']
for track_index, track in enumerate(video_tracks, 1):
    clips = otio_clips(track)
    resolve_items = timeline.GetItemListInTrack('video', track_index) or []
    if len(clips) != len(resolve_items):
        warnings.append(f'Video track {track_index} did not conform one-to-one; kept its clip names')
        continue
    for clip, item in zip(clips, resolve_items):
        apply_clip(item, clip)

for track_index, track in enumerate(audio_tracks, 1):
    resolve_items = timeline.GetItemListInTrack('audio', track_index) or []
    if len(otio_clips(track)) != len(resolve_items):
        raise RuntimeError(f'Audio track {track_index} did not conform one-to-one')
    cursor = 0.0
    clip_index = 0
    for child in track['children']:
        duration = seconds(child['source_range']['duration'])
        if child['OTIO_SCHEMA'] == 'Clip.2':
            item = resolve_items[clip_index]
            clip_index += 1
            metadata = child.get('metadata', {}).get('Resolve_OTIO', {})
            if metadata.get('Link Group ID') is not None:
                apply_clip(item, child)
            else:
                media = item.GetMediaPoolItem()
                if str(media.GetClipProperty('Audio Ch')) != '1':
                    warnings.append(f"Kept frame-quantized multichannel clip: {child['name']}")
                    apply_clip(item, child)
                else:
                    reference = child['media_references'][child['active_media_reference_key']]
                    source_in = seconds(child['source_range']['start_time']) - seconds(
                        reference['available_range']['start_time']
                    )
                    available_duration = seconds(reference['available_range']['duration'])
                    replacements.append({
                        'item': item,
                        'media': media,
                        'track': track_index,
                        'record': timeline_start + cursor * rate,
                        'source_in': source_in,
                        'duration': duration,
                        'full_source': abs(source_in) < 0.000001 and abs(duration - available_duration) < 0.00001,
                        'name': child['name'],
                        'clip': child,
                    })
        cursor += duration

if replacements and not timeline.DeleteClips([entry['item'] for entry in replacements], False):
    raise RuntimeError('Resolve could not replace the frame-quantized recorder clips')

for entry in replacements:
    append_audio(entry)

organize_media()
project.SetCurrentTimeline(timeline)
print(f'Created {timeline.GetName()} with {len(replacements)} sample-accurate recorder clips')
for warning in warnings:
    print('Warning:', warning, file=sys.stderr)
"#
}
