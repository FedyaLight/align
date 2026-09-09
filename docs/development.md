# Development

## Prerequisites

- A current stable Rust toolchain with rustfmt and Clippy.
- FFmpeg and ffprobe on PATH for portable decoding and test fixture generation.
  Use a full FFmpeg build for fixtures; the packaged minimal build omits the
  synthetic video/audio sources used by tests.
- Python 3.11 or later for AAF tooling and acceptance scripts.
- macOS: Xcode command-line tools. The packaged application targets macOS 15+
  on Apple Silicon.
- Windows: the MSVC Rust target and Visual Studio C++ build tools.
- Linux: the desktop development libraries installed by
  [CI](../.github/workflows/align-rs.yml), including XKB, Wayland, ALSA, udev,
  and Vulkan headers.

The workflow is the reference for platform-specific package installation.
Run commands from the repository root.

## Build

```sh
cargo build --release -p align-cli -p align-mcp -p align-gpui
cargo run -p align-gpui
cargo run -p align-cli -- --help
```

Binaries are written to `target/release`: `align`, `align-cli`, and `align-mcp`
(with `.exe` on Windows). Build only `align-cli` when desktop support is not
needed. Backend selection is automatic; `ALIGN_BACKEND=portable` selects the
portable implementation on macOS.

### AAF module

```sh
python3 -m venv .venv
. .venv/bin/activate
python -m pip install -r Support/aaf/build-requirements.txt
python script/build-aaf-sidecar.py target/release
```

On Windows, activate `.venv\Scripts\Activate.ps1` in PowerShell instead.
The builder creates `align-aaf` and `AAF-Licenses` beside the CLI. It freezes
Python and pyaaf2 with PyInstaller, so end users do not need Python installed.
Build the module separately for each target OS.

## Architecture

| Crate | Responsibility |
| --- | --- |
| `align-core` | Data model, fingerprints, matching, graph solving, time mappings, cache, and timeline writers. No platform media APIs. |
| `align-decode` | Media backends, import orchestration, AAF bridge, audio rendering, export jobs, and media ownership. |
| `align-cli` | Argument parsing, synchronization, saved result loading, and common single/batch export settings. |
| `align-gpui` | Desktop state, views, timeline interaction, and background job coordination. |
| `align-mcp` | Stdio protocol adapter around the adjacent CLI. |

`Pipeline` expands and inspects inputs, obtains cached or newly decoded
fingerprints, matches candidates, refines them with audio windows, and solves
synchronization groups. `MediaBackend` separates native Apple media access from
portable decoding. Both feed the same core algorithms.

Export assembles a writer-neutral timeline, prepares required audio, retains
temporary sources, and invokes format writers. `media_assets` owns extracted AAF
audio across the temporary-to-saved boundary. Desktop workers report indexed
results and use cancellation flags; generation checks reject stale UI updates.

Keep algorithms in `align-core`, media I/O in `align-decode`, and presentation
state in the frontend. Preserve existing serialized field names when changing
internal names. Test behavior at the shared layer used by CLI and desktop.

`Support/` contains the application icon, macOS privacy manifest, and AAF bridge
with its pinned Python requirements. `script/` contains build, fixture, and
verification entry points. Generated files belong in `target/`; `cargo clean`
removes them, including local packages and acceptance output. Preserve any
results you need outside that directory before cleaning.

## Verification

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo doc --workspace --no-deps
cargo test --workspace
python -m unittest discover -s Support/aaf -v
cargo build --release -p align-cli
python script/check-end-to-end.py target/release/align-cli target/acceptance
```

Run the Python commands in the AAF virtual environment. The acceptance output
directory must not already exist. On Windows, pass `target/release/align-cli.exe`.
The CLI must have the AAF module beside it; FFmpeg and ffprobe must be on PATH.
CI also builds API documentation with `RUSTDOCFLAGS="-D warnings"` to catch
broken Rust documentation links.

The acceptance runner executes eight checks: general release behavior, basic
editing preservation, linked AAF, picture AAF, embedded mono, embedded stereo,
mixed edit rates, and saved-result/media lifetime. It retains per-check logs and
a `verification.json` containing status and the tested CLI's SHA-256. It does not
open desktop or editor applications.

Tests that require external media tools need those tools installed even when
only the native backend is being developed. CI runs the same acceptance command
on macOS, Windows, and Linux. Release tags matching `align-rs-v*` also build
platform bundles.

### Editor readback tools

The remaining scripts support checks that require an editor installation or a
prepared corpus. Their module docstrings and `--help` describe inputs.

| Scripts | Purpose |
| --- | --- |
| `generate-premiere-frac-fixture.py`, `build-frac-result.py` | Generate known fractional-placement media and saved results. |
| `verify-premiere-frac.py`, `verify-premiere-project.py` | Read saved Premiere projects and compare placements. |
| `check-premiere-render.py` | Compare rendered audio markers with independent ground truth. |
| `verify-resolve-precision.py` | Read a named Resolve timeline through its scripting interface. |
| `check-corpus-offset.py` | Compare exported XML placement with a saved synchronization result. |
| `generate-search-accuracy-fixture.py` | Generate shared-band audio and an unrelated recording. |

These checks have narrower supported structures than a full editor project.
A project-file readback and an acoustic render test prove different things;
record which one was performed. Keep local recordings, project files, screenshots,
and benchmark output out of version control.

Resolve readback requires its `Scripting/Modules` directory on `PYTHONPATH`
and a running editor with scripting enabled. It checks the named project and
timeline before reading them. It is not part of the unattended CLI test suite.

## Packaging

```sh
./script/package-macos.sh /path/to/new/Align-macOS
```

The script builds the application, CLI, MCP server, and AAF module; bundles
FFmpeg/ffprobe by default; includes the privacy manifest and license files;
signs ad hoc; and runs a brief launch check. The output directory must not exist.
It produces a local macOS bundle, not a notarized installer.

Set `ALIGN_SKIP_LAUNCH_CHECK=1` to package without opening the desktop app.
Bundle validation and the CLI/MCP checks still run.

To reuse a previously built minimal FFmpeg directory, set `FFMPEG_DIR`.
`BUNDLE_FFMPEG=0` omits those binaries; portable container decoding and some AAF
picture operations then require system FFmpeg/ffprobe. To build the sidecars:

```sh
./script/build-ffmpeg-minimal.sh target/ffmpeg-minimal
```

That build requires a C toolchain, make, curl, tar, xz, and NASM where applicable.
For a tag named `align-rs-vX.Y.Z` matching the workspace version, the release
workflow also builds Velopack installers/update packages for macOS, Windows,
and Linux and publishes them in a GitHub Release. The desktop app uses those
checksum-verified packages for **Align → Check for Updates…**. Distribution
code signing and Apple notarization still require release credentials and are
not configured in this repository.

## Repository conventions

Documentation and user-facing text are in English. Describe the current behavior
and its limitations, not migration history or local development sessions. Keep
reusable fixtures and checks with the code; write generated artifacts under
`target/` or an ignored local directory. Avoid committing absolute machine paths,
editor projects, or dated acceptance reports.

The workspace declares MIT licensing. Preserve third-party notices when changing
or distributing bundled components.
