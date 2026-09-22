# Drift correction

## What drift means

Clock drift is a change in the relative timing of shared sound over a recording.
A constant start offset, different file lengths, or different sample rates alone
do not establish drift. Echoes and movement between microphones can also change
measured audio delays.

For a linear mapping `timeline = rate × source + offset`, accumulated drift is
`(rate − 1) × duration`, and parts per million (ppm) is `(rate − 1) × 1,000,000`.
For example, 12 ppm corresponds to 21.6 ms over 30 minutes. Measurements are
relative to another recording; they do not identify an absolutely accurate clock.

Align estimates timing from audio windows across the overlap. Noisy or ambiguous
matches can produce uncertain estimates. Additional diagnostic windows help
inspect a result but do not constitute independent ground truth.

## Export behavior

When correction is needed and enabled, export renders new WAV files using the
validated time mapping. Channels remain separate and original recordings are
not modified. `--no-drift` disables this correction during CLI export.

Writing a timeline description and rendering its required audio are separate
operations. The first export may write substantial audio data for drift
correction, precise placement, channel extraction, or replacement media. Its
speed depends on recording length, decoding, and disk throughput.

Export can reuse prepared audio in the same destination when source identity
and preparation settings match. Reuse does not mean a fresh render would take
the same time. Keep generated audio with the exported timeline.

## Saved results

Exporting a saved result uses its stored time mappings; it does not repeat
synchronization. Run synchronization again to apply changes in the analysis
algorithm. See [usage](usage.md) for saved-result export and
[development](development.md#drift-diagnostics) for diagnostic commands.
