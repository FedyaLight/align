# Usage

## macOS installation

The release app is not Developer ID signed or notarized. If macOS says the app
is damaged, move `Align.app` to `/Applications`, then run these commands in
Terminal. Only do this for a copy downloaded from this project's official release.

```sh
sudo xattr -dr com.apple.quarantine "/Applications/Align.app"
open "/Applications/Align.app"
```

## Desktop workflow

1. Add recordings, a folder, or a timeline project. Select a sequence when the
   project contains more than one.
2. Review missing-media warnings and source settings. Use the path fixer if
   recordings have moved.
3. Synchronize. Review the resulting groups, unmatched clips, and warnings.
4. Choose an export destination and the options appropriate for your editor.

While Align analyzes, clips on the timeline are striped: their place is not
confirmed yet. Afterwards a synchronized clip has a link in its top-right
corner and an unmatched clip a warning sign. A clip too narrow for the mark
shows its state by color only.

The export sheet starts from Align's defaults. **Save as Default** keeps the
current export options for new sessions and later launches; the destination
and sequence name are not saved. **Revert to Saved** and **Use Align Defaults**
load either set back into the sheet. The command line does not
read these saved defaults.

Search accuracy controls how much analysis work is performed. Start with
Balanced; higher levels take more time and memory. Match threshold controls the
confidence required to accept a match. These settings serve different purposes.

For an existing edit, enable Preserve basic editing on the tracks that should
remain fixed. This preserves their trims, duplicates, positions, and gaps while
other tracks align to them. See [format limits](formats.md) for unsupported edits.

### Placing unmatched clips by camera clock

Some unmatched clips can be placed approximately from their camera's own
clock: the camera recorded its serial number and when each recording started,
and two of its clips recorded before and after this one are synchronized by
sound in the same group. When the result has such clips, a bar above the
timeline offers them. **Review…** lists each clip with the two clips it is
placed between and the expected accuracy, and the clips that cannot be placed
with the reason. Place clips one at a time, from the clip's right-click menu,
or with **Place All**; **Undo** or **Return to Unmatched** takes a clip back,
and with it a kept choice it was holding back, so another clip is not placed
unasked.

A placed clip is purple on the timeline, with a clock instead of the link, and
stays unmatched: the place is an estimate, not a match. Export uses exactly
what the timeline shows. The choice is kept per sequence in the project and
applies again after reopening, another stage, or synchronizing again, wherever
the clip still has an estimate. New projects and projects saved by earlier
versions place nothing, and the choice is never part of the saved export
defaults. See [camera clock placement](formats.md#camera-clock-placement) for
which cameras qualify, how accurate the place is, and what each format writes.

### Save and reopen projects

Align creates an autosave when synchronization starts and updates that same
project as you work. **File → Project Autosave** can turn this off or choose a
different folder for new projects. The default is **Align** inside your system
Documents folder. At startup, Align removes its autosaves that have gone seven
days without being opened or updated.

Use **File → Recent Projects** or **Open Project…** to reopen a `.align` file.
The Windows installer registers Align to open these files in Explorer.
**Save As…** creates a project you can name and keep. **Save Project** saves to
that file even when autosave is off.

A project keeps the source paths, sequence choices, synchronization settings,
relinks, corrections, and completed placements. A reopened timeline can be
exported immediately when its result is current and its sources are unchanged.
If sources were moved or changed, use **Relink…** as needed, then synchronize
again. Embedded AAF audio is extracted again from the original AAF on open.
The project does not contain the original media and works independently of the
analysis cache.

### Keyboard shortcuts

| Action | macOS | Windows and Linux |
| --- | --- | --- |
| Add media | Cmd+O | Ctrl+O |
| Open project | Cmd+Shift+O | Ctrl+Shift+O |
| Save project | Cmd+S | Ctrl+S |
| Save project as | Cmd+Shift+S | Ctrl+Shift+S |
| Synchronize again | Cmd+R | Ctrl+R |
| Export | Cmd+E | Ctrl+E |
| Remove selected sources (files stay on disk) | Delete or Fn+Delete | Delete |
| Zoom the timeline in, out, to fit | Cmd+= or Cmd++, Cmd+−, Cmd+0 | Ctrl+= or Ctrl++, Ctrl+−, Ctrl+0 |
| Zoom the timeline at the pointer | Cmd or Option + scroll | Ctrl or Alt + scroll |
| Pan the timeline | Arrow keys (Shift for larger steps), Home, End | same |
| Full screen | Ctrl+Cmd+F | F11 |
| Quit | Cmd+Q | Ctrl+Q |

On Windows and Linux, **Menu** opens a sidebar from the top toolbar or the
empty start screen; pressing and releasing Alt on its own also opens or closes
it. **Path Fixer** appears after you import a timeline.
Export is available from the timeline toolbar after synchronization. On macOS,
use the native menu bar.

Escape closes the open menu, panel, or dialog.

## Reporting a problem

Use **Help → Report a Problem…**, the same button in **About Align**, or the
button in an error dialog. Align also shows a dismissible reminder once per
installed release. The Help command remains available after you close it.

The report opens GitHub's bug form with the Environment field already filled
in: the Align version, the operating system and its version, and the
architecture, exactly as the app shows them. Describe what happened and, if
you can, how to reproduce it. Check existing issues first in case the problem
is already being investigated. GitHub sign-in is required; you review and
submit the issue there. Recordings and logs are not uploaded. Error details can
be copied separately; remove private filenames and paths before posting them
publicly.

## CLI basics

Examples assume `align-cli` is on PATH. When running from a source checkout,
substitute `./target/release/align-cli` (`align-cli.exe` on Windows).

```sh
align-cli --help
align-cli sync --help
align-cli export --help
align-cli export-json --help
```

Inspect without synchronizing by supplying paths without a subcommand:

```sh
align-cli /path/to/media > project.json
```

Put the subcommand first when synchronizing or exporting. `export-json` accepts
exactly a result file and an output directory; `clear-cache` takes no media paths.
Unexpected arguments exit with code 2 before the operation runs.

Inspection returns a project, not a synchronization result. To produce JSON that
`export-json` can consume, use `sync`:

```sh
align-cli sync /path/to/camera /path/to/recorder > result.json
align-cli export-json result.json /path/to/output
```

JSON is written to stdout; diagnostics and progress go to stderr. Exit code 0
means success, 1 means an operation failed, and 2 means invalid usage. Treat saved
results as versioned application data; arbitrary cross-version compatibility is
not guaranteed.

## Synchronization settings

Synchronization combines audio matching, links between recognized consecutive
recording parts, and timecode overlap for otherwise unmatched clips. It is not
limited to waveform matching. Supported source timecodes can come from container
metadata, BWF/iXML, or LTC recorded as audio.

The default `--time-source auto` uses available recording timestamps and source
timecode. `--time-source timecode` selects timecode as the timing metadata source;
it still runs audio matching. Missing metadata does not disable audio matching,
and timecode does not override established audio matches. Devices need compatible
timecodes for overlap-based alignment to be meaningful.

```sh
align-cli sync --search-accuracy thorough --match-threshold conservative \
  /path/to/media > result.json

align-cli sync --time-source timecode --clip-order by-file-name \
  /path/to/media > result.json

align-cli sync --preserve-basic-editing V1 --preserve-basic-editing A1 \
  /path/to/edit.xml > result.json
```

`--track-content linear` prevents waveform/timecode matches within one imported
source track. Use `takes` when that track contains overlapping takes. These
policies do not replace structural links between consecutive recording parts.

`--stage N` selects a completed synchronization stage using a one-based index.
Without an override, Align selects the stage with the most synchronized clips,
using the later stage to break ties. `export-json --stage N` changes the exported
stage without rerunning analysis when the saved result contains stage history.

## Multiple sequences

```sh
align-cli sync --sequence 2 /path/to/project.aaf > result.json
align-cli sync --all-sequences /path/to/project.xml > results.json
align-cli export-json results.json /path/to/output
align-cli export --all-sequences /path/to/output /path/to/project.fcpxml
```

Sequence numbers start at 1. `--sequence` and `--all-sequences` are mutually
exclusive. A multi-sequence import requires an explicit selection. Batch JSON is
an array of results; `export-json` accepts either that array or one result.

## Missing media

```sh
# Remember a directory-prefix replacement for future runs.
align-cli sync --redirect '/old/media=/new/media' /path/to/project.xml > result.json

# Choose one exact file for this run.
align-cli sync --relink 'take.wav=/new/media/take.wav' /path/to/project.aaf > result.json

# Write repaired references to a separate project file.
align-cli sync --write-fixed-project /path/to/project-fixed.xml \
  /path/to/project.xml /path/to/media > result.json
```

`--clear-redirects` removes saved redirections. `--prefer-proxies` selects FCPXML
proxy media when available. `--omit-extensions` excludes selected
comma-separated extensions referenced by the timeline. Repair writes a copy;
it cannot replace the source project.

## Opening the export

| Editor | Open | Sound |
| --- | --- | --- |
| Premiere Pro | `Align – Adobe Premiere Pro.xml` (File → Import) | Starts on the nearest frame. A two-channel recording played whole is one stereo clip; a single channel, or a recording of more channels, has a mono track per channel, as Premiere imports it itself. |
| Final Cut Pro | `Align – Final Cut Pro.fcpxml` (File → Import → XML) | Keeps its exact place. A multichannel recording is one clip with a subrole per channel. In a multicam angle, a recording starts on its file's first whole frame. |
| DaVinci Resolve | With a project open and Preferences → System → General → External scripting using set to Local, run `Align – DaVinci Resolve.py` | Placed on the sample nearest its place; each recording, and a camera's sound linked to its picture, is one clip on a mono, stereo, or adaptive track of its format. The script also applies clip names, the color labels set in export, and with `--media-bins` files media into a folder per source. |
| DaVinci Resolve, without scripting | `Align – DaVinci Resolve.xml` (File → Import → Timeline) | Starts on the nearest frame, each channel on a mono track. Clip names and colors are not imported. Use this file rather than the Premiere XML: Resolve reads some trimmed recordings in the Premiere XML a frame late. |
| Other AAF editors | The AAF export, with its `AAF Audio` folder | Starts on the nearest frame, each channel on a mono track. At 29.97 and 59.94 the timeline is drop-frame when the first source's timecode is. |

The OTIO file is the script's timeline; imported on its own, Resolve places
its sound on frames too. Sound on the nearest frame is within half a frame of
its place, as with other synchronization tools.

## Export options

By default, export writes the standard editor formats. `--aaf` selects AAF
instead. Both `export` and `export-json` accept the same export settings.

```sh
align-cli export-json --aaf --aaf-fps 25 result.json /path/to/aaf-output
align-cli export-json --no-drift result.json /path/to/output
align-cli export-json --replaced-audio result.json /path/to/output
align-cli export-json --no-fcpxml-multicam result.json /path/to/output
align-cli export-json --media-bins result.json /path/to/output
align-cli export-json --unmatched order-only --disable-unmatched \
  --prevent-group-overlaps result.json /path/to/output
```

- `--no-drift` disables clock-drift correction during export.
- `--replaced-audio` adds a sequence with external audio replacing camera audio.
- `--media-bins` organizes the source clips an editor imports into one group per
  source folder; see [organized source clips](formats.md#organized-source-clips).
- `--export-media` also creates complete camera files with replacement audio;
  this is separate from timeline-only export.
- `--no-fcpxml-multicam` or `--no-fcpxml-timeline` omits the corresponding
  FCPXML project. `--fcpxml-storylines` groups tracks into separate storylines.
- `--aaf-fps` sets the AAF composition rate, the timeline's by default. Picture
  is counted in composition frames, as editors read it, and every camera is a
  layer of one picture track; this option does not retime the footage.
- `--camera-clock` places unmatched clips approximately by their camera's
  clock, as described in [camera clock placement](formats.md#camera-clock-placement).
  `--camera-clock-except CLIP` leaves one clip unmatched; give its file name,
  its path when two folders share the name, or its clip ID, and repeat it for
  more clips. Each placement, exception, and reason for leaving a clip
  unmatched is printed on stderr. Without `--camera-clock` nothing is placed.

`sync --summary` lists the clips the camera clock can place, under
`cameraClock`, with the reasons for the others, so you can review them before
exporting. `export` and `export-json` compute the same placements from the
same result and stage.

Labels, roles, sequence names, gap removal, and trimming are listed in
`align-cli export --help`. DaVinci Resolve ignores clip names and color labels
when it imports XML or OTIO; its import script applies them, using the nearest
Resolve clip color, and substitutes lookalike characters for punctuation such as
`?` that Resolve does not allow in names. Check the exported timeline before
using it as the basis for further editing.

## Cache and saved media

```sh
align-cli clear-cache
align-cli clear-cache --older-than-days 30
```

The on-disk analysis cache stores fingerprints, waveform previews, spectral
flux, LTC detections, and portable video timing. By default Align removes an
entry once it has gone 7 days unused. Open **File → Analysis Cache** from the macOS menu bar or the
**Menu** sidebar on Windows and Linux to change retention, clear entries for
the current project, or clear all entries.

Use the on-disk analysis cache control to skip reading and writing the cache
on new desktop and CLI runs. Existing entries stay on disk and can still be
cleared. Turning the cache back on resumes caching with your selected
retention. **Until cleared** sets retention with no automatic expiration.

Embedded AAF audio retained for saved JSON is application data, not cache.
Do not delete it while those results are needed. See
[media ownership](formats.md#media-ownership) for file locations and export behavior.

## AI agents

Open **Align → Use with AI Agents…** to copy the installed CLI path or a prompt.
Inspect media by passing paths directly. Save the complete synchronization result
and print a short JSON summary with:

```sh
align-cli sync --output-json result.json --summary /path/to/media
align-cli export-json result.json /path/to/export
```

The same flags work with `--all-sequences`. Without `--summary`, sync prints
the full result. See the [agent skill](../skills/align-cli/SKILL.md).
