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

Search accuracy controls how much analysis work is performed. Start with
Balanced; higher levels take more time and memory. Match threshold controls the
confidence required to accept a match. These settings serve different purposes.

For an existing edit, enable Preserve basic editing on the tracks that should
remain fixed. This preserves their trims, duplicates, positions, and gaps while
other tracks align to them. See [format limits](formats.md) for unsupported edits.

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

## Export options

By default, export writes the standard editor formats. `--aaf` selects AAF
instead. Both `export` and `export-json` accept the same export settings.

```sh
align-cli export-json --aaf --aaf-fps 25 result.json /path/to/aaf-output
align-cli export-json --no-drift result.json /path/to/output
align-cli export-json --replaced-audio result.json /path/to/output
align-cli export-json --no-fcpxml-multicam result.json /path/to/output
align-cli export-json --unmatched order-only --disable-unmatched \
  --prevent-group-overlaps result.json /path/to/output
```

- `--no-drift` disables clock-drift correction during export.
- `--replaced-audio` adds a sequence with external audio replacing camera audio.
- `--export-media` also creates complete camera files with replacement audio;
  this is separate from timeline-only export.
- `--no-fcpxml-multicam` or `--no-fcpxml-timeline` omits the corresponding
  FCPXML project. `--fcpxml-storylines` groups tracks into separate storylines.
- `--aaf-fps` overrides the composition timecode rate. Source picture edit rates
  are retained; this option does not retime the footage.

Labels, roles, sequence names, gap removal, and trimming are listed in
`align-cli export --help`. Check the exported timeline before using it as the
basis for further editing.

## Cache and saved media

```sh
align-cli clear-cache
align-cli clear-cache --older-than-days 30
```

The analysis cache contains fingerprints and portable video-timing inspections.
Clearing it causes later runs to repeat that work. Desktop cache controls also
support clearing the current media and changing retention.

Embedded AAF audio retained for saved JSON is application data, not cache.
Do not delete it while those results are needed. See
[media ownership](formats.md#media-ownership) for file locations and export behavior.

## MCP integration

`align-mcp` is a stdio server. Keep `align-cli` beside it. The server exposes
`align_inspect`, `align_sync`, and `align_export`; it invokes the adjacent CLI and
returns its result. Use absolute media and output paths.

For clients that accept an `mcpServers` configuration:

```json
{
  "mcpServers": {
    "align": {
      "command": "/absolute/path/to/align-mcp",
      "args": []
    }
  }
}
```

The desktop menu **Align → Use with AI Agents…** provides a configuration using
its installed executable path. The MCP tool schema is the reference for supported
arguments; it does not expose every CLI setting.
