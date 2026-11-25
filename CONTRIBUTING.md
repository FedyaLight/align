# Contributing to Align

Align synchronizes camera footage with separately recorded audio and sends the
result to Premiere Pro, DaVinci Resolve, and Final Cut Pro. One person runs it,
with a lot of help from coding agents. Pull requests are welcome, small ones
especially. This page is what helps a change go in without a long back-and-forth.

## What we take

Always welcome:

- a file that synchronizes at the wrong offset, or a timeline that opens wrong
  in an editor, fixed with a test that catches it;
- cameras, recorders, containers, and timeline formats Align does not read yet;
- faster analysis that stays just as accurate;
- clearer messages and documentation.

Open an issue first, so we agree on the shape before you spend time:

- a new export format, export option, or desktop screen;
- a change to what an existing export writes. People import Align's timelines
  into projects they are already editing, so exports change on purpose only;
- a large refactor.

Out of scope: sending media, projects, or usage data anywhere. Align works on
the user's machine and only contacts GitHub to check for updates.

## Getting it running

You need Rust 1.100.0 (`rust-toolchain.toml` selects it), FFmpeg and ffprobe for
the test fixtures, and Python 3.11 or later for the acceptance tools. The
[development guide](docs/development.md#prerequisites) lists the platform
packages.

```sh
cargo run -p align-gpui            # the desktop app
cargo run -p align-cli -- --help   # the command line
```

Where things live:

| Path | What |
| --- | --- |
| `crates/align-core` | Data model, fingerprints, matching, time mappings, timeline writers. No platform media APIs. |
| `crates/align-decode` | Media backends, import, audio rendering, export jobs. |
| `crates/align-gpui` | The desktop app. |
| `crates/align-cli` | The command-line interface. |
| `crates/align-aaf` | Native AAF reader, writer, and path repair. |
| `Support/` | Icons, the privacy manifest, and the Python AAF reference. |
| `script/` | Build, fixture, benchmark, and acceptance scripts. |
| `docs/` | User and developer documentation. |

The [architecture section](docs/development.md#architecture) explains how a
synchronization and an export run.

## What a good change looks like

**It is measured.** "Looks in sync" is not a result. Give the offset error in
samples or frames against a known answer: `script/bench-sync.py` generates
cameras with known offsets, or compare a sharp transient in both files. For an
export change, say which editor and version you imported into and what you
checked there. A file that passes a schema is not proof the editor reads it the
way you meant.

**It keeps exports stable.** When a change alters what an export writes, say so
in the pull request and cover it with a test. When rendered audio changes, bump
`AUDIO_SIDECAR_REVISION` in `crates/align-decode/src/export.rs` so old prepared
files are not reused.

**It is simple.** Prefer the change that removes code. Keep algorithms in
`align-core`, media I/O in `align-decode`, and screen state in `align-gpui`, so
the command line and the desktop app behave the same. Read the surrounding code
first and match its naming and style.

## Testing

Reproduce the bug before you fix it, ideally with a test that fails, then watch
the same test pass. Tests run the production code, not a copy of it.

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Without FFmpeg, tests that need it are skipped with a notice. CI runs the same
checks on macOS, Windows, and Linux, so a small fix does not need every platform
on your machine. The [verification section](docs/development.md#verification)
has the full list, including the end-to-end acceptance run for export changes.

Never commit private recordings, editor projects, or absolute paths from your
machine. Generate fixtures with FFmpeg the way the existing tests do.

## Reporting a bug

**Help › Report a Problem** in Align opens the issue form with the version and
operating system filled in. Add the steps, the error message, and, for timeline
problems, the editor and its version. Issues are public, so don't attach
private recordings. If a specific file is needed, describe it instead: the
camera or recorder, and the output of `ffprobe` on it.

## Pull requests

- Link the issue if there is one (`Closes #12`). A small fix doesn't need one.
- One change per pull request. Describe it the way a user would notice it,
  then how you checked it.
- Commit messages start with a short subject saying what changed, then a plain
  paragraph with the reason. `git log` shows the style.
- `main` moves fast and the owner commits straight to it. Rebase if your branch
  conflicts.
- For a first-time contributor, GitHub waits for a maintainer to approve the CI
  run. That is a standard safety setting, not a judgment of your change.
- Sometimes the owner lands your work in a different shape or takes part of it
  into another commit. You stay credited with a `Co-authored-by:` trailer.
- Releases, version numbers, and release notes are the owner's job. Don't bump
  the version in a pull request.

Documentation and user-facing text are in English. Describe what Align does
now and its limits, not the history of the change.

## Working with a coding agent

Using one is fine; much of Align was written that way. Point it at this file
and `docs/development.md`. For risky logic, such as synchronization math, file
writes, or concurrency, get a second review and reproduce each finding before
acting on it. Read the agent's diff yourself before you open the pull request:
you're the one answering for it.

## Security

Report vulnerabilities privately, as described in [SECURITY.md](SECURITY.md),
not in a public issue.

## License

Align is licensed under GPL-3.0-only. By opening a pull request you agree that
your contribution is licensed under the same terms. You keep the copyright in
your work, and there is no contributor agreement to sign.
