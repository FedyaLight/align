# Formats and limitations

## Media inputs

Align discovers WAV, AIF/AIFF, M4A, MP3, M4V, MOV, MP4, MTS, MXF, and R3D
paths. An accepted extension does not guarantee that the installed decoder
supports the file's codec. When the built-in decoder fails on a file's audio
during analysis, FFmpeg decodes it again where FFmpeg is available. A file is
named in a warning and left out of synchronization only when its media or
audio still cannot be read; the other files are still synchronized. Inspect
the warnings before exporting.

On macOS, the default backend uses AVFoundation, and reads MXF, which
AVFoundation cannot open, with the bundled FFmpeg. The portable backend reads
MOV/MP4/M4V headers natively, decodes WAV, AIFF, MP3, AAC, ALAC, and PCM with
Symphonia, and uses FFmpeg for other containers (MTS, MXF, R3D) and for
streams Symphonia decodes incorrectly, such as HE-AAC. When Symphonia decodes,
a damaged audio packet becomes silence of the same length, so the rest of the
file keeps its timing.
Set `ALIGN_BACKEND=portable` to select it on macOS. Video metadata is inspected
for timing; synchronization does not decode video frames.

Camera-file export copies QuickTime/MP4 video without re-encoding on every
platform; other containers need FFmpeg.

## Timeline interchange

An XML file is imported as a timeline only when it is an FCP 7 XML or FCPXML
document. The XML metadata that cameras write beside each clip is skipped,
like other files that are not media.

| Format | Import | Export | Notes |
| --- | --- | --- | --- |
| FCP 7 XML | Yes | Yes | Separate Premiere and Resolve writers; selected editing metadata can be preserved. |
| FCPXML | Yes | Yes | Timeline and multicam projects; optional track storylines and audio roles. Imports `.fcpxml` files and `.fcpxmld` bundles. |
| AAF | Yes | Yes | External picture/audio and embedded PCM audio import; linked media export. Requires `align-aaf`. |
| OpenTimelineIO | No | Yes | Resolve timeline output. |
| Resolve Python script | No | Yes | Import helper generated alongside Resolve exports; requires the editor's scripting interface. |
| Native Premiere/Resolve project | No | No | Use a supported interchange format. |

Default exports include Premiere XML, Resolve XML, Resolve OTIO, the Resolve
import script, and FCPXML. A batch combines sequences into one Premiere XML,
one FCPXML, or one AAF where applicable; Resolve artifacts are written per
sequence. AAF is selected explicitly with `--aaf`.

Final Cut Pro counts source times from the start timecode it reads from each
file. Timecode that Align reads from LTC audio or from a camera's XML is not
in the file, so in FCPXML such a file starts at zero, as Final Cut Pro shows it.

## Organized source clips

**Organize imported source clips** in the export sheet, or `--media-bins`,
groups the source clips an export includes, so the editor's browser shows one
group per source. It is off by default and changes only the browser: the
synchronized timelines are the same with or without it.

| Editor | What the import creates |
| --- | --- |
| Premiere Pro | A bin per source group holding its master clips, an `Align generated audio` bin, then the sequences. |
| Final Cut Pro | A browser clip for every file in the event, tagged with its source group or `Align generated audio`, and a keyword collection per tag. |
| DaVinci Resolve | The import script creates a media pool folder named after the timeline, with a folder per source group and one for generated audio. |

A group is named after the folder the files were copied into. The folder
structure that cameras and recorders write on their cards is skipped, so files
in `Wedding/CamA/PRIVATE/M4ROOT/CLIP` belong to `CamA`. Folders with the same
name are told apart by their parent folder, such as `Day 1 · CamA`. A folder
that holds both video and audio-only files is split into its video and its
audio, and Sony cameras sharing a folder are told apart by their metadata.
Other cameras and audio recorders are grouped by folder only: cards of one
camera copied into two folders form two groups, and several recorders copied
into one folder form one.

Source clips keep their original audio; use the exported timeline for
synchronized playback. `Align generated audio` holds the files Align rendered
for the timelines: drift corrections of recorders whose clock drifts, and
for AAF a mono copy of each channel of a recording. The timelines play
these files, so keep them with the export.

Organizing never renames or moves files on disk. Each run of the Resolve import
script creates its own folder, because Resolve imports a timeline's media again
on every import. Importing the Resolve XML or OTIO without the script, and AAF
exports, do not create groups.

## Editing support

Selected imported tracks can anchor an existing edit: source ranges, repeated
clip instances, order, positions, and gaps remain fixed while other tracks align.
This does not imply lossless interchange of an arbitrary edited project.

- FCP 7 XML supports basic trims, links, enabled/locked states, and constant-speed
  forward or reverse retiming. Some nested structures are flattened.
- Premiere XML can retain imported non-retime filters and transition data.
  Other writers do not translate every effect or audio gain curve.
- Supported video dissolves and audio crossfades can be represented in OTIO,
  and the Resolve import script keeps them. FCPXML does not carry transitions
  or speed changes: such clips play there as cuts at normal speed.
- Variable-speed retiming and complex nested edits are not generally preserved.
  Read import warnings and verify the exported result in the target editor.

## AAF details

AAF import follows supported SourceClip, Sequence, Filler, and active Selector
structures. It handles external media and embedded mono or multichannel PCM
with channel assignments. Edit rates are converted using rational arithmetic;
source sample boundaries and composition frame rates are kept distinct.

Embedded video, compressed embedded audio, AAF effects, transitions, and retiming
are unsupported. Unsupported structures fail explicitly. AAF export links
QuickTime/MP4 picture, PCM audio, and single-track, clip-wrapped VC-3
(DNxHD/DNxHR) picture in MXF OP-Atom. The MXF link keeps the original file,
its package identifiers, and source timecode. DaVinci Resolve Studio 21.1
was verified with DNxHD 36 and DNxHR LB/HQX. Premiere Pro 27.0 left the
tested DNxHR LB OP-Atom media offline. MXF OP1a, other MXF essence
mappings, and embedded essence export are unsupported.

## Synchronization and drift

Matching requires usable shared audio or supported timing/structural evidence.
Silence, repeated takes, limited overlap, or unrelated recordings can leave clips
unmatched. A higher search setting increases the candidate search; it does not
make ambiguous evidence reliable.

The solver keeps disconnected groups separate. Drift rendering uses validated
affine or piecewise time mappings, preserves discrete audio channels, and writes
new WAV files. It does not alter source recordings. This is clock correction,
not a general-purpose creative time-stretch tool. See
[drift correction](drift.md) for interpretation and export behavior.

## Camera clock placement

Placing unmatched clips by camera clock is off until it is applied, in the
desktop app for each clip or with `--camera-clock`.

**Which clips.** An unmatched clip qualifies when its file holds Sony
NonRealTimeMeta metadata with the camera's serial number and recording time
(`Device serialNo` and `CreationDate`), as Sony cameras write inside MP4 and
MXF files. The separate `M01.XML` file beside a clip is not read. Some models
write a placeholder serial number (0, 4294967295, or all F); their clips do
not qualify, because two such cameras would look the same. Other cameras do
not record a serial number Align reads, so their clips are not placed. Clips
of the same camera are recognized by manufacturer, model, and serial number
together; folders, file dates, and the model alone never count.

**Where it is placed.** Among the camera's clips synchronized by sound, Align
takes the nearest one recorded before the unmatched clip and the nearest one
recorded after it. Each has an offset between its recording time and its
place in the group; the clip's offset is interpolated between the two, so
steady clock drift between them is accounted for. The clip's time source
decides how its recording time is read: Automatic and REC START take it as
the start, REC STOP as the end. A clip or anchor whose time source is Timecode
is not used, because timecode need not run continuously between clips. Clips
placed by timecode or file links are not anchors, and a placed clip is never
an anchor for another.

**Accuracy.** Recording times are whole seconds, so two of them can be off by
up to a second against each other. The tolerance shown is that second plus
how far the two anchors disagree, and it is an expectation, not a guarantee:
a camera whose start delay varies, or a clock set between recordings, can
put a clip further off. Anchors that disagree by more than two seconds place
nothing between them.

**When a clip is not placed.** A clip stays unmatched, with the reason shown,
when its time source is Timecode, it has no recording time, no clip of the
camera recorded before or after it is synchronized by sound, the nearest
clips before and after it are in different groups or the camera has clips
from another group between them, the anchors disagree, it would not fit
between the anchors or would overlap another synchronized or placed clip of
the camera, or its pairing or alignment with a clip it would play alongside
was rejected.

**In exports.**

- A placed clip is named `[CAMERA CLOCK]` before any unmatched label, and
  keeps the unmatched color, role, and **Disable in timeline** settings.
- With Cut and remove options off, no other clip moves or changes track. The
  clip's former place after its group stays empty. It joins its camera's
  track when that track is free there, else the first free track, else a new
  track after the others.
- **Remove** unmatched clips removes placed clips too. **By order only**
  still places them by the camera clock.
- **Cut gaps empty on all tracks** keeps the time a placed clip covers, so
  the clips after it in its group, and the later groups, end up that much
  later than without it. **Cut recorder audio with no camera** keeps a
  recorder clip that a placed clip overlaps. Trimming and **Remove clips
  shorter than** treat it like any clip.
- The Premiere replaced-audio sequence keeps a placed clip's own camera sound
  instead of the recorder's. Clean-audio camera files are not created for
  placed clips.
- For an imported XML, FCPXML, or AAF edit, edits keep their placement. Align
  compares where each edit of a clip starts with where the camera clock puts
  the same frame, accounting for the edit's source in point, speed, and its
  track's offset, and reports the difference. It does so only when that track
  has synchronized edits from the clip's group that agree within a frame; an
  edit played in reverse is not compared. A clip without an edit stays among
  the sources.

Estimates are not stored in saved results: they are computed again from the
result's selected stage and time-source settings each time.

## Media ownership

| Data | Location | Lifetime |
| --- | --- | --- |
| Original recordings | Their original paths | Managed by the user; never replaced by synchronization. |
| Analysis cache | OS cache directory under `Align/Fingerprints` | Regenerable; controlled by cache cleanup and retention. |
| Embedded AAF audio referenced by saved inspect/sync JSON | OS local data directory under `Align/Saved AAF Media` | Keep while the saved results are needed. |
| Temporary AAF extraction | A private temporary directory | Removed at normal process exit. |
| Retained temporary sources for direct export | `Source Media` under the export directory | Keep with the exported timeline. |
| Corrected audio, AAF channel stems, and replacement media | Export directory | Keep with the exported timeline. |

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
