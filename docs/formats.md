# Formats and limitations

## Media inputs

Align discovers WAV, AIF/AIFF, M4A, MP3, M4V, MOV, MP4, MTS, MXF, and R3D
paths. An accepted extension does not guarantee that the installed decoder
supports the file's codec. Unsupported or unreadable media produces a warning or
an error; inspect the result before exporting.

On macOS, the default backend uses AVFoundation. The portable backend uses
Symphonia for supported audio and FFmpeg for other containers. Set
`ALIGN_BACKEND=portable` to select it on macOS. Video metadata is inspected for
timing; synchronization does not decode video frames.

## Timeline interchange

| Format | Import | Export | Notes |
| --- | --- | --- | --- |
| FCP 7 XML | Yes | Yes | Separate Premiere and Resolve writers; selected editing metadata can be preserved. |
| FCPXML | Yes | Yes | Timeline and multicam projects; optional track storylines and audio roles. |
| AAF | Yes | Yes | External picture/audio and embedded PCM audio import; linked media export. Requires `align-aaf`. |
| OpenTimelineIO | No | Yes | Resolve timeline output. |
| Resolve Python script | No | Yes | Import helper generated alongside Resolve exports; requires the editor's scripting interface. |
| Native Premiere/Resolve project | No | No | Use a supported interchange format. |

Default exports include Premiere XML, Resolve XML, Resolve OTIO, the Resolve
import script, and FCPXML. A batch combines sequences into one Premiere XML,
one FCPXML, or one AAF where applicable; Resolve artifacts are written per
sequence. AAF is selected explicitly with `--aaf`.

## Editing support

Selected imported tracks can anchor an existing edit: source ranges, repeated
clip instances, order, positions, and gaps remain fixed while other tracks align.
This does not imply lossless interchange of an arbitrary edited project.

- FCP 7 XML supports basic trims, links, enabled/locked states, and constant-speed
  forward or reverse retiming. Some nested structures are flattened.
- Premiere XML can retain imported non-retime filters and transition data.
  Other writers do not translate every effect or audio gain curve.
- Supported video dissolves can be represented in OTIO. The precision import
  script does not reproduce all transition semantics.
- Variable-speed retiming and complex nested edits are not generally preserved.
  Read import warnings and verify the exported result in the target editor.

## AAF details

AAF import follows supported SourceClip, Sequence, Filler, and active Selector
structures. It handles external media and embedded mono or multichannel PCM
with channel assignments. Edit rates are converted using rational arithmetic;
source sample boundaries and composition frame rates are kept distinct.

Embedded video, compressed embedded audio, AAF effects, transitions, and retiming
are unsupported. Unsupported structures fail explicitly. AAF export writes
linked media rather than embedding essence. Picture metadata may require
ffprobe even when the native Apple backend is selected.

## Synchronization and drift

Matching requires usable shared audio or supported timing/structural evidence.
Silence, repeated takes, limited overlap, or unrelated recordings can leave clips
unmatched. A higher search setting increases the candidate search; it does not
make ambiguous evidence reliable.

The solver keeps disconnected groups separate. Drift rendering uses validated
affine or piecewise time mappings, preserves discrete audio channels, and writes
new WAV files. It does not alter source recordings. This is clock correction,
not a general-purpose creative time-stretch tool.

## Media ownership

| Data | Location | Lifetime |
| --- | --- | --- |
| Original recordings | Their original paths | Managed by the user; never replaced by synchronization. |
| Analysis cache | OS cache directory under `Align/Fingerprints` | Regenerable; controlled by cache cleanup and retention. |
| Embedded AAF audio referenced by saved inspect/sync JSON | OS local data directory under `Align/Saved AAF Media` | Keep while the saved results are needed. |
| Temporary AAF extraction | A private temporary directory | Removed at normal process exit. |
| Retained temporary sources for direct export | `Source Media` under the export directory | Keep with the exported timeline. |
| Corrected audio, precision stems, and replacement media | Export directory | Keep with the exported timeline. |

Saved results and exports may also reference external recordings at their
original paths. Export is not a general media-collection/archive command. Moving
an export folder alone does not necessarily make it portable; retain its media
and relink paths in the editor when moving between machines.

## Verification scope

Automated checks cover known offsets, selected edit preservation, channel and
sample boundaries, AAF round trips, and media references after process exit.
They do not replace an import/playback check in the editor version used for a
production project. See [development](development.md#verification) for the
reproducible checks and optional editor readback tools.
