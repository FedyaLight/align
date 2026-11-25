---
name: align-cli
description: Synchronize local camera and recorder media with Align, inspect timeline projects, and export saved synchronization results for editing.
---

Use the executable path from Align → Use with AI Agents… or `align-cli` on PATH.
Quote paths containing spaces. Inspect media or a project with `align-cli PATH...`.

Run `align-cli sync --output-json RESULT.json --summary PATH...` with a fresh
result filename. Read the summary on stdout for islands, clip names, warnings,
and maximum absolute drift. Progress and errors go to stderr. Use
`--all-sequences` to process each sequence in a project.

When asked to export, run `align-cli export-json RESULT.json OUTPUT_FOLDER`
and report the returned artifacts.

A `cameraClock` entry in the summary lists unmatched clips that can be placed
approximately by their camera's clock, and why others cannot. Report these to
the user; add `--camera-clock` (with `--camera-clock-except CLIP` for clips to
leave out) to the export only when they ask for it. Use `align-cli sync --help` or
`align-cli export-json --help` for sequence, relinking, and format options.
