# Align

Align synchronizes video and separately recorded audio by matching their
waveforms. It exports aligned timelines for Premiere Pro, DaVinci Resolve,
Final Cut Pro, and AAF-compatible editors, leaving the original media unchanged.

Use the desktop app for interactive work, the CLI for batch processing, or the
stdio MCP server for integrations.

## Features

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
media decoding. Platform-specific build dependencies are
listed in the [development guide](docs/development.md).

```sh
cargo build --release -p align-cli -p align-mcp -p align-gpui
cargo run --release -p align-gpui
```

The desktop executable is `align`; the command-line executable is `align-cli`.
Desktop builds installed through a Velopack package support updates from the
**Align** menu. Updates require accessible release packages; see
[packaging](docs/development.md#packaging).
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
- [Drift correction](docs/drift.md) — clock drift, rendered audio, and export reuse.

## Platform support

Align is under active development. CI checks the Rust workspace and CLI workflows
on macOS, Windows, and Linux. Those checks do not establish compatibility with
every editor version or verify desktop interaction on every platform. The macOS
packaging script targets Apple Silicon and macOS 15 or later.

## Contributing

For bug reports, include the Align version, operating system, steps to reproduce,
and the error message. For timeline problems, name the interchange format and
editor version. Do not attach private recordings or project files to public
issues. Build and verification commands are in the [development guide](docs/development.md).

## License

Copyright (c) 2026 Align contributors.

Align is licensed under the GNU General Public License, version 3 only
(`GPL-3.0-only`). You may use, modify, and redistribute it under those terms.
It is provided without warranty. See [LICENSE](LICENSE).

Third-party components retain their own licenses. See
[third-party notices](THIRD_PARTY_NOTICES.txt) and the
[distribution notes](docs/distribution.md).
