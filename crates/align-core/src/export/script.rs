//! Resolve import script: places recordings between frames, on the sample
//! nearest their place, each one clip on a track of its channel format, and
//! makes render-compatible audio-only timelines. When the OTIO lists media bins,
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


def audio_entries():
    """Each OTIO audio clip with its track, place and range."""
    entries = []
    for track_index, track in enumerate(audio_tracks, 1):
        cursor = 0.0
        for child in track['children']:
            duration = seconds(child['source_range']['duration'])
            if child['OTIO_SCHEMA'] == 'Clip.2':
                reference = child['media_references'][child['active_media_reference_key']]
                source_in = seconds(child['source_range']['start_time']) - seconds(
                    reference['available_range']['start_time']
                )
                available_duration = seconds(reference['available_range']['duration'])
                entries.append({
                    'clip': child,
                    'align': (child.get('metadata') or {}).get('Align') or {},
                    'linked': (child.get('metadata') or {}).get('Resolve_OTIO', {}).get('Link Group ID') is not None,
                    'track': track_index,
                    'cursor': cursor,
                    'path': reference['target_url'],
                    'source_in': source_in,
                    'duration': duration,
                    'full_source': abs(source_in) < 0.000001 and abs(duration - available_duration) < 0.00001,
                    'name': child['name'],
                })
            cursor += duration
    return entries


def recordings(entries):
    """The recordings the script places: every channel of a recording played
    whole as one clip on a track of its format, a mono recording or one
    selected channel on a mono track."""
    groups = {}
    for entry in entries:
        groups.setdefault(entry['align'].get('Item') or id(entry['clip']), []).append(entry)
    result = []
    for group in groups.values():
        group.sort(key=lambda entry: entry['align'].get('Channel', 1))
        first = group[0]
        channels = int(first['align'].get('Channels', 1))
        if channels > 1 and not first['align'].get('Selected Channel') and len(group) == channels:
            result.append(dict(
                first,
                entries=group,
                format='stereo' if channels == 2 else f'adaptive{channels}',
                channel=None,
            ))
            continue
        for entry in group:
            result.append(dict(
                entry,
                entries=[entry],
                format='mono',
                channel=entry['align'].get('Channel') if channels > 1 else None,
            ))
    result.sort(key=lambda recording: (recording['track'], recording['cursor']))
    return result


typed_tracks = {}


def track_for(recording):
    """A mono recording keeps its track; the channels of a recording played
    whole share a track of their format, one per track they came from."""
    if recording['format'] == 'mono':
        return recording['track']
    key = (recording['track'], recording['format'])
    if key not in typed_tracks:
        if not timeline.AddTrack('audio', recording['format']):
            raise RuntimeError(f"Resolve could not add a {recording['format']} audio track")
        typed_tracks[key] = timeline.GetTrackCount('audio')
    return typed_tracks[key]


def place(recording, media):
    source_rate = float(media.GetClipProperty('FPS') or rate)
    sample_rate = float(media.GetClipProperty('Sample Rate') or 48000)
    record = timeline_start + recording['cursor'] * rate
    clip_info = {
        'mediaPoolItem': media,
        'mediaType': 2,
        'trackIndex': track_for(recording),
    }
    if not recording['full_source']:
        # Resolve starts a recording only on a frame of it, so the clip
        # moves as far as its start does and its sound keeps its place.
        exact = recording['source_in'] * source_rate
        start = round(exact)
        if record + (start - exact) * rate / source_rate < timeline_start:
            start = math.ceil(exact)
        record += (start - exact) * rate / source_rate
        clip_info['startFrame'] = start
        clip_info['endFrame'] = start + max(1, round(recording['duration'] * source_rate))
    # Resolve's audio renderer rounds fractional sample starts upward. Place
    # on the nearest sample first; a tiny inward epsilon avoids float noise
    # promoting an exact sample to its successor.
    relative_samples = (record - timeline_start) / rate * sample_rate
    record = timeline_start + (math.floor(relative_samples + 0.5) - 0.0001) / sample_rate * rate
    clip_info['recordFrame'] = max(timeline_start, record)
    result = pool.AppendToTimeline([clip_info]) or []
    if len(result) != 1:
        raise RuntimeError(f"Resolve could not place {recording['name']}")
    item = result[0]
    actual = float(item.GetStart(True))
    if abs(actual - clip_info['recordFrame']) > rate / sample_rate:
        raise RuntimeError(f"Resolve quantized {recording['name']} to {actual}")
    if recording['channel'] is not None:
        mapping = {'track_mapping': {'1': {
            'channel_idx': [recording['channel']], 'mute': False, 'type': 'mono',
        }}}
        if not item.SetSourceAudioChannelMapping(json.dumps(mapping)):
            raise RuntimeError(f"Resolve could not play channel {recording['channel']} of {recording['name']}")
    apply_clip(item, recording['clip'])


def remove_empty_tracks(count):
    """Drop the first `count` audio tracks that the recordings left empty."""
    for index in range(count, 0, -1):
        if timeline.GetItemListInTrack('audio', index) or timeline.GetTrackCount('audio') <= 1:
            continue
        if not timeline.DeleteTrack('audio', index):
            warnings.append(f'Could not remove empty audio track {index}')


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
    original_tracks = timeline.GetTrackCount('audio')
    timeline_start = float(timeline.GetStartFrame())
    entries = audio_entries()
    paths = list(dict.fromkeys(entry['path'] for entry in entries))
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
    placed = recordings(entries)
    for recording in placed:
        place(recording, media_by_path[os.path.realpath(recording['path'])])
    remove_empty_tracks(original_tracks)
    organize_media()
    project.SetCurrentTimeline(timeline)
    print(f'Created {timeline.GetName()} with {len(placed)} sample-accurate audio clips')
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

video_tracks = [track for track in document['tracks']['children'] if track['kind'] == 'Video']
for track_index, track in enumerate(video_tracks, 1):
    clips = otio_clips(track)
    resolve_items = timeline.GetItemListInTrack('video', track_index) or []
    if len(clips) != len(resolve_items):
        warnings.append(f'Video track {track_index} did not conform one-to-one; kept its clip names')
        continue
    for clip, item in zip(clips, resolve_items):
        apply_clip(item, clip)

entries = audio_entries()
for track_index, track in enumerate(audio_tracks, 1):
    resolve_items = timeline.GetItemListInTrack('audio', track_index) or []
    track_entries = [entry for entry in entries if entry['track'] == track_index]
    if len(track_entries) != len(resolve_items):
        raise RuntimeError(f'Audio track {track_index} did not conform one-to-one')
    for entry, item in zip(track_entries, resolve_items):
        entry['item'] = item

# Camera sound stays linked to its picture. A selected channel of a recording
# keeps the clip the timeline import gave it, which plays that channel.
for entry in entries:
    if entry['linked'] or entry['align'].get('Selected Channel'):
        apply_clip(entry['item'], entry['clip'])
replaced = recordings([
    entry for entry in entries
    if not entry['linked'] and not entry['align'].get('Selected Channel')
])
replaced_items = [entry['item'] for recording in replaced for entry in recording['entries']]
if replaced_items and not timeline.DeleteClips(replaced_items, False):
    raise RuntimeError('Resolve could not replace the frame-quantized recorder clips')
original_tracks = timeline.GetTrackCount('audio')
for recording in replaced:
    place(recording, recording['item'].GetMediaPoolItem())
remove_empty_tracks(original_tracks)

organize_media()
project.SetCurrentTimeline(timeline)
print(f'Created {timeline.GetName()} with {len(replaced)} sample-accurate recorder clips')
for warning in warnings:
    print('Warning:', warning, file=sys.stderr)
"#
}
