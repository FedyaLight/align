# Align Review Workspace Design

## Goal

Add an in-app review workspace that lets editors preview source video, inspect and correct sync placement, and navigate/play the assembled timeline without altering waveform-matched placements unless the editor explicitly moves them. This work is a separate PR from same-device relative-timestamp placement.

## Context and scope

The timestamp-placement PR handles unmatched clips: only waveform-matched clips from the same device may anchor a relative recording-timestamp estimate; waveform matches are never moved, and estimated placements remain visibly unconfirmed. This design does not change that algorithm.

The review-workspace PR covers: a resizable source-video monitor; a distinct NLE-style timeline playhead with seek, pan, zoom, and playback; visible audio waveforms; manual clip placement with snapping, frame/millisecond nudges, undo/redo; and session restoration. Clip-preview mode remains separate from timeline playback and is active only for a selected video clip. Playback starts paused. If only one audio track is present at the playhead, that track is audible regardless of solo/mute settings.

## Design choices

Keep the upstream change cohesive in one review-workspace PR, rather than mixing it with timestamp placement or filename ordering. Reuse the existing result/project model and playback/export abstractions; avoid introducing a second source of truth for clip positions. The preview fits its available panel while preserving source aspect ratio. The timeline has its own transport state and cursor, independent of the selected-clip source monitor.

## User-visible behavior

1. Selecting a video clip loads that source into the monitor. Resizing the monitor scales the displayed frame to fit without distortion. Clip preview does not take over the timeline playhead.
2. The timeline playhead can be clicked or dragged to any point, and transport controls play/pause from that position. Timeline audio playback is paused at app startup. When only one audio track overlaps the playhead, it is audible even if track solo/mute state would otherwise silence it; normal solo/mute behavior applies when multiple tracks overlap.
3. Waveforms identify embedded camera audio and external recorder tracks. Editors can drag clips with snapping, nudge by frame or milliseconds, and undo/redo edits. The same effective placements drive review playback and NLE timeline exports.
4. Manual placements are represented in the review result/session, not only in transient UI state. A sync rerun must not silently discard a manual placement; clips that are no longer unmatched are not timestamp-estimated or moved by the estimate pass. Exports use the effective manually placed positions. Tentative timestamp estimates stay labeled as estimates in result and export metadata.
5. On reopening, restore selected inputs, latest sync result, manual edits, and relevant review state. Failed-to-decode media remains visible with an actionable warning rather than disappearing.

## Architecture and data flow

The sync result remains the authoritative base. A review/session layer stores user-authored placement overrides and navigation state separately from automatic matches. The timeline renderer and transport read the effective placement view (base result plus valid manual overrides); the source monitor reads only the selected video source. Save/restore serializes the base result reference and overrides together, and export consumes the same effective placement view used by review. Re-running sync refreshes automatic results while retaining compatible manual overrides and re-evaluating stale ones explicitly; it never converts manual or waveform-confirmed positions into timestamp estimates.

Playback transport starts in paused state. Timeline seek updates the timeline cursor and audio source positions; selected-clip preview remains a separate mode and does not auto-start on application launch. Track audibility follows the single-overlapping-track rule above, with established solo/mute controls retained for multi-track overlaps.

## Failure handling

Missing media, unsupported or undecodable media, and stale session references remain visible as warnings. A failed preview load must not move the playhead or start playback. Invalid manual overrides are surfaced for review and are not silently applied to exports. Undo/redo applies only to review edits and does not mutate the underlying source media.

## Validation and acceptance criteria

- Resizing the source monitor preserves aspect ratio and does not distort frames.
- Source clip selection changes clip preview only; timeline seek/play changes the independent timeline cursor/transport only.
- Startup is paused; seeking to any timeline position begins playback there when requested.
- A sole overlapping audio track is audible regardless of solo/mute state; multi-track behavior remains covered by existing controls.
- Drag, snapping, nudge, undo, and redo produce stable effective placements shared by review and exports.
- Session save/restore retains manual edits and review state; a subsequent sync does not silently erase a compatible manual placement.
- Unmatched clips without a waveform anchor are not forced to the end; same-device relative timestamp estimates remain visibly unconfirmed and do not move waveform matches.
- Decoder failures remain visible and do not crash the workspace.
- Run the checks required by upstream CONTRIBUTING.md, including relevant Rust tests and formatting.

## Explicitly out of scope

Natural filename ordering remains a separate small PR. This PR does not add media transcoding, change the timestamp-estimation rules, compact gaps differently, or alter existing stream-copy export behavior.
