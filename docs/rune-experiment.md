# Rune desktop experiment

`align-rune` is an experimental frontend alongside the production GPUI app.
It uses the same `align-core`, `align-decode`, allocator, release optimization
settings, application state, lane allocation and export writers. It does not
replace the normal desktop executable or installers.

Rune is pinned to
[`781e2b6`](https://github.com/anomalyco/rune/tree/781e2b6d87cd5f32d6f4365956fad4baaf24ee50).
It is a pre-release framework; upgrading the revision can require source changes.
The native Metal backend, accessibility, embedded fonts and native dialogs are
enabled. Rune's SVG and additional audio codecs are disabled: Align's existing
backend handles media. A process-wide mimalloc allocator is used by both apps.

## Run

```sh
cargo build --locked --release -p align-gpui -p align-rune -p align-cli
cargo run --locked --release -p align-rune
```

On macOS, a local app bundle with a separate bundle identifier can be built with:

```sh
python3 script/package-rune-macos.py --no-build target/Align-Rune.app
open target/Align-Rune.app
```

`--no-build` packages the executable produced by the paired build above,
preserving the measured compiled code (bundle signing changes the executable
signature/hash). Omit it for a standalone Rune build; Cargo feature
unification can then change the binary size.

This is a local ad-hoc signed experiment, not a release installer. The bundle
includes the frontend, icon, privacy manifest and Rune/font notices. It does
not bundle the optional AAF helper, CLI, MCP or portable FFmpeg sidecars. WAV,
MOV and MP4 use Align's usual native media backend on macOS. AAF workflows
still require the separately built AAF helper described in [development](development.md).

## Implemented scope

- Add a file or folder through native dialogs, or drop multiple files/folders.
- Import supported timelines and select one or all sequences.
- Run the existing synchronization pipeline on a worker, display progress,
  waveforms, warnings, matched groups and unmatched clips, and cancel a run.
- Browse/select media, remove selected inputs, clear the session, switch the
  displayed result of a multi-sequence import, zoom and scroll the timeline.
- Change common search accuracy, timing mode, threshold and basic audio-channel
  settings. Toggle light/dark appearance for the experimental window.
- Export through the existing Resolve OTIO/XML/script, Premiere XML, FCPXML and
  AAF writers, with optional drift correction and output-folder selection.
- Native keyboard commands: Cmd/Ctrl-O, R, E and Q.

The full GPUI interface has more functionality. The Rune window does not expose
per-track overrides, manual match corrections, stage history, the Path Fixer,
advanced export labeling/trimming/media options, persistent appearance choices,
the updater or the full application menu. The shared model retains that state,
but these controls have not been ported. Basic native file-dialog selection is
single-file; use folder import or multi-file drag-and-drop for batches.

The experiment is suitable for measuring this frontend and trying the common
sync/export workflow. It is not feature parity or a production migration.
The source is cross-platform, but local validation and measurements cover macOS
Apple Silicon only.

## Shared state

`align-ui` owns desktop session state, settings, cancellation/generation rules,
result handling, appearance persistence and timeline lane allocation. Its
`ScrollPort` abstracts offsets from GPUI's `ScrollHandle`. GPUI adapts its native
handle; Rune's scrolling belongs to its own view tree. Existing state/lane tests
run at this common layer. Two Rune runtime tests exercise real worker event
completion, import/sync/export controls and cancellation with late events.

Rune workers send bounded events and wake the window when data arrives. The
normal idle window has no periodic polling timer. The continuous timer described
below is enabled only for the animation benchmark.

## Reproduce the comparison

Use the same machine and release profile for both binaries, keep both windows
at 1120 × 700 points and use the same display scale. Do not run builds or screen
recordings during timed measurements. The repository release profile uses
`opt-level=3`, thin LTO, one codegen unit, abort-on-panic and stripped binaries.
The upstream Rune workspace profile is not used for this application comparison.

Create a deterministic media fixture using the existing benchmark generator,
then save its result for both interfaces:

```sh
python3 script/bench-sync.py target/release/align-cli target/desktop-fixture \
  --cameras 6 --duration 180 --repeat 1
./target/release/align-cli sync target/desktop-fixture/media \
  > target/desktop-fixture/result.json
python3 script/bench-desktop.py \
  --result target/desktop-fixture/result.json \
  --media target/desktop-fixture/media \
  --out target/desktop-comparison --repeat 3 --seconds 12 --warmup 4
```

The sampler runs paired, alternating processes for empty idle, loaded idle,
continuous zoom, and synchronization. It records every repeat and raw sample in
`measurements.json`, executable sizes and SHA-256 hashes. CPU is the delta of
libproc process CPU time, converted through the Mach timebase; 100% means one
CPU core. RAM is physical footprint in MiB, with resident and peak footprint
reported separately. Idle excludes warmup. Synchronization includes process
startup and UI updates, uses a separate initially empty fingerprint cache for
each run, and saves its result; OS file caches remain warm. It is not an
isolated DSP benchmark. A short run may be dominated by initialization.

The zoom benchmark varies the same model zoom level on a four-second sinusoidal
cycle. GPUI requests display animation frames; Rune requests a 60 Hz deadline.
Use a 60 Hz display for this comparison. This measures each application's
relayout/render behavior; the layouts and amount of visible text differ. It
does not establish equal frame throughput or prove frame latency equivalence.

For separate GPU traces (Xcode/Instruments required):

```sh
python3 script/bench-desktop-gpu.py \
  --result target/desktop-fixture/result.json \
  --out target/desktop-gpu --repeat 3 --seconds 10
```

This records Metal System Trace after warmup, exports `metal-gpu-intervals`,
filters the application's PID and active depth-zero intervals, and unions
concurrent channel intervals. It excludes the first/last second and WindowServer.
The result is **milliseconds of GPU activity per second**, not GPU-core
utilization or power consumption. Instruments has its own overhead, so these
runs must be separate from CPU/RAM sampling. Raw traces contain machine/system
metadata; keep all generated results and traces out of version control.

`ALIGN_NO_UPDATE_CHECK=1` disables GPUI's normal network check. Both frontends
accept opt-in harness environment variables: `ALIGN_EXPERIMENT_RESULT` loads a
saved result; `INPUT`, `SYNC`, `SAVE`, and `CACHE` (with the same prefix) drive a
real synchronization; `ANIMATE` continuously varies zoom. Normal launches leave
all these unset. Rune also supports its upstream headless tools:

```sh
./target/release/align-rune --snap target/rune-empty.png
ALIGN_EXPERIMENT_RESULT=target/desktop-fixture/result.json \
  ./target/release/align-rune --scene export --snap target/rune-export.png \
  --tree target/rune-export-tree.json
```

Compare relative placements and matched groups in the saved outputs before
interpreting timing. Binary size compares standalone stripped executables,
not full installers with optional media helpers. Physical footprint and RSS are
different metrics; neither should be labeled as dedicated VRAM on Apple Silicon.
A smaller partial interface can use less memory independently of its framework.
