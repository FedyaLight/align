# Align

Align synchronizes camera and recorder audio and exports timelines for video
editors. It finds shared sound, refines offsets, and corrects supported clock
drift without modifying the source recordings.

Use the desktop app for interactive work, the CLI for batch processing, or the
stdio MCP server for integrations.

## What it does

- Synchronizes media files, folders, and imported XML, FCPXML, or AAF timelines.
- Uses waveform matching with timecode and recording metadata as supporting evidence.
- Keeps unrelated recordings in separate synchronization groups.
- Preserves selected tracks' basic edits while aligning other recordings.
- Exports Premiere XML, Final Cut Pro FCPXML, Resolve OTIO/XML and an import
  script, or linked AAF.
- Saves analysis results for later export and reuses cached fingerprints.

Format support has limits, particularly for effects, transitions, and retiming.
See [supported formats](docs/formats.md) before processing an edited project.

## Build and run

Install a current stable Rust toolchain. Install FFmpeg and ffprobe for portable
media decoding and the test fixtures. Platform-specific build dependencies are
listed in the [development guide](docs/development.md).

```sh
cargo build --release -p align-cli -p align-mcp -p align-gpui
cargo run --release -p align-gpui
```

The desktop executable is `align`; the command-line executable is `align-cli`.
Installed desktop releases can check, download, and apply updates from the
**Align** menu on macOS, Windows, and Linux.
AAF support requires an additional [build step](docs/development.md#aaf-module).

```sh
# Synchronize recordings and save the result.
./target/release/align-cli sync /path/to/media > result.json

# Export the saved result.
./target/release/align-cli export-json result.json /path/to/output

# Or synchronize and export in one step.
./target/release/align-cli export /path/to/output /path/to/media
```

The CLI writes JSON to stdout and progress to stderr. Keep exported media folders
with their timelines; corrected audio and precision stems may be required for
playback. See [usage](docs/usage.md) for sequence selection, relinking, and export
options.

## Documentation

- [Usage](docs/usage.md) — desktop workflow, CLI examples, saved results, and MCP.
- [Formats](docs/formats.md) — import/export support, media ownership, and limitations.
- [Development](docs/development.md) — setup, architecture, tests, and packaging.

## Project status

Align is under active development. CI checks the Rust workspace and CLI workflows
on macOS, Windows, and Linux. Those checks do not establish compatibility with
every editor version or verify desktop interaction on every platform. The macOS
packaging script targets Apple Silicon and macOS 15 or later.

## License

Licensed under the MIT license. See [LICENSE](LICENSE) and
[third-party notices](THIRD_PARTY_NOTICES.txt). Packaged AAF and FFmpeg modules
include their own license files.
