//! align-cli: batch synchronization and timeline export.
//! `sync | export | export-json`, progress on stderr, JSON on stdout,
//! exit 2 on usage errors, 1 on failures. mimalloc for low fragmentation.

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use std::io::Write;
use std::path::PathBuf;

use align_decode::pipeline::{Phase, Pipeline, PipelineError, PipelineInput, PipelineOptions};
use clap::{Args, Parser, Subcommand};

#[derive(Parser)]
#[command(name = "align-cli", version)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
    /// Media/folder paths for bare `open` (no subcommand).
    #[arg(global = true)]
    paths: Vec<PathBuf>,
}

#[derive(Subcommand)]
enum Command {
    /// Synchronize media, print SyncResult JSON.
    Sync {
        #[arg(long, value_name = "N")]
        sequence: Option<usize>,
        #[command(flatten)]
        settings: SyncSettings,
        #[command(flatten)]
        relink: RelinkArgs,
        paths: Vec<PathBuf>,
    },
    /// Synchronize + drift-corrected export, print artifacts JSON.
    Export {
        #[arg(long, value_name = "N")]
        sequence: Option<usize>,
        #[arg(long)]
        no_drift: bool,
        #[arg(long)]
        replaced_audio: bool,
        /// Also create complete camera files with external audio replacing scratch audio.
        #[arg(long)]
        export_media: bool,
        #[arg(long, value_enum, default_value_t = Unmatched::OrderTime)]
        unmatched: Unmatched,
        #[arg(long)]
        prevent_group_overlaps: bool,
        #[arg(long)]
        disable_unmatched: bool,
        #[arg(long)]
        label_unmatched: bool,
        #[command(flatten)]
        cut_remove: CutRemoveArgs,
        #[command(flatten)]
        assign: AssignArgs,
        #[command(flatten)]
        relink: RelinkArgs,
        #[command(flatten)]
        settings: SyncSettings,
        output: PathBuf,
        paths: Vec<PathBuf>,
    },
    /// Export a saved result JSON.
    ExportJson {
        /// Select a completed synchronization stage (1-based).
        #[arg(long, value_name = "N")]
        stage: Option<usize>,
        #[arg(long)]
        no_drift: bool,
        #[arg(long)]
        replaced_audio: bool,
        /// Also create complete camera files with external audio replacing scratch audio.
        #[arg(long)]
        export_media: bool,
        #[arg(long, value_enum, default_value_t = Unmatched::OrderTime)]
        unmatched: Unmatched,
        #[arg(long)]
        prevent_group_overlaps: bool,
        #[arg(long)]
        disable_unmatched: bool,
        #[arg(long)]
        label_unmatched: bool,
        #[command(flatten)]
        cut_remove: CutRemoveArgs,
        #[command(flatten)]
        assign: AssignArgs,
        result: PathBuf,
        output: PathBuf,
    },
    /// Clear the persistent fingerprint cache, print freed stats JSON.
    ClearCache {
        /// Only clear entries older than this many days (default: all).
        #[arg(long, value_name = "N")]
        older_than_days: Option<u64>,
    },
}

/// Missing-media relink (Syncaila Path Fixer): saved redirections apply
/// on every run, manual picks apply once.
#[derive(Args, Clone, Debug, Default)]
struct RelinkArgs {
    /// Saved old-prefix=new-location mapping (persisted for future runs).
    #[arg(long, value_name = "OLD=NEW")]
    redirect: Vec<String>,
    /// Exact manual filename=path pick for this run.
    #[arg(long, value_name = "NAME=PATH")]
    relink: Vec<String>,
    /// Forget all saved redirections before applying `--redirect`.
    #[arg(long)]
    clear_redirects: bool,
    /// Timeline-referenced extensions skipped silently (comma-separated).
    #[arg(long, value_name = "EXTS")]
    omit_extensions: Option<String>,
}

/// Resolved `--redirect` / `--relink` / `--omit-extensions`: saved
/// redirections, one-shot manual picks, silently skipped extensions.
type RelinkOptions = (
    Vec<align_core::redirect::PathRedirection>,
    Vec<(String, PathBuf)>,
    Vec<String>,
);

fn relink_options(args: &RelinkArgs) -> Result<RelinkOptions, CliError> {
    let store = align_core::redirect::config_file();
    if args.clear_redirects {
        let _ = std::fs::remove_file(&store);
    }
    let mut redirects = align_core::redirect::load_from(&store);
    for flag in &args.redirect {
        let (old, new) =
            align_core::redirect::split_pair("--redirect", flag).map_err(CliError::Failure)?;
        if !new.is_dir() {
            return Err(CliError::Failure(format!(
                "--redirect target is not a directory: {}.",
                new.display()
            )));
        }
        let entry = align_core::redirect::PathRedirection::new(&old, new);
        if !redirects.contains(&entry) {
            redirects.push(entry);
        }
    }
    align_core::redirect::save_to(&store, &redirects);
    let mut manual = Vec::new();
    for flag in &args.relink {
        let (name, path) =
            align_core::redirect::split_pair("--relink", flag).map_err(CliError::Failure)?;
        manual.push((name, path));
    }
    let omit: Vec<String> = args
        .omit_extensions
        .as_deref()
        .unwrap_or("")
        .split(',')
        .filter_map(|extension| {
            let trimmed = extension.trim().trim_start_matches('.');
            (!trimmed.is_empty()).then(|| trimmed.to_string())
        })
        .collect();
    Ok((redirects, manual, omit))
}

/// Post-sync Cut / Remove (Syncaila Extra options). All default to off.
#[derive(Args, Clone, Copy, Debug, Default)]
struct CutRemoveArgs {
    /// Cut ranges empty on every clip and close the timeline.
    #[arg(long)]
    cut_common_gaps: bool,
    /// Drop recorder audio that overlaps no camera clip.
    #[arg(long)]
    cut_lone_recorder: bool,
    /// Drop clips shorter than this many seconds.
    #[arg(long, value_name = "SECS", default_value_t = 0.0)]
    cut_shorter_than: f64,
    /// Trim this many seconds off every clip start.
    #[arg(long, value_name = "SECS", default_value_t = 0.0)]
    trim_starts: f64,
    /// Trim this many seconds off every clip end.
    #[arg(long, value_name = "SECS", default_value_t = 0.0)]
    trim_ends: f64,
}

impl CutRemoveArgs {
    fn core(self) -> Result<align_core::export_model::CutRemoveOptions, CliError> {
        for (name, value) in [
            ("--cut-shorter-than", self.cut_shorter_than),
            ("--trim-starts", self.trim_starts),
            ("--trim-ends", self.trim_ends),
        ] {
            if !value.is_finite() || value < 0.0 {
                return Err(CliError::Failure(format!(
                    "{name} requires a non-negative number of seconds."
                )));
            }
        }
        Ok(align_core::export_model::CutRemoveOptions {
            common_gaps: self.cut_common_gaps,
            lone_recorder: self.cut_lone_recorder,
            shorter_than: self.cut_shorter_than,
            trim_starts: self.trim_starts,
            trim_ends: self.trim_ends,
        })
    }
}

/// Export assignment (Syncaila Export settings): sequence name, symbol,
/// color and role labels for unmatched clips. All default to off.
#[derive(Args, Clone, Debug, Default)]
struct AssignArgs {
    /// Override the exported sequence/project name.
    #[arg(long, value_name = "NAME")]
    sequence_name: Option<String>,
    /// Custom symbol for unmatched names (implies labeling).
    #[arg(long, value_name = "TEXT")]
    unmatched_symbol: Option<String>,
    /// Attach the unmatched symbol as a suffix instead of a prefix.
    #[arg(long)]
    unmatched_symbol_suffix: bool,
    /// FCP 7 color label for unmatched clips (Premiere/Resolve XML).
    #[arg(long, value_name = "COLOR")]
    unmatched_color: Option<String>,
    /// Final Cut audio role for unmatched audio (FCPXML only).
    #[arg(long, value_name = "ROLE")]
    unmatched_role: Option<String>,
}

#[derive(Clone, Copy, Debug, clap::ValueEnum)]
enum SearchAccuracy {
    Fast,
    Balanced,
    Thorough,
    Deep,
    Exhaustive,
}

impl SearchAccuracy {
    fn core(self) -> align_core::SearchAccuracy {
        match self {
            Self::Fast => align_core::SearchAccuracy::Fast,
            Self::Balanced => align_core::SearchAccuracy::Balanced,
            Self::Thorough => align_core::SearchAccuracy::Thorough,
            Self::Deep => align_core::SearchAccuracy::Deep,
            Self::Exhaustive => align_core::SearchAccuracy::Exhaustive,
        }
    }
}

#[derive(Args, Clone, Copy, Debug)]
struct SyncSettings {
    /// Select a completed synchronization stage (1-based); default is most synced clips.
    #[arg(long, value_name = "N")]
    stage: Option<usize>,
    /// Search more spectral bands at the cost of time and memory.
    #[arg(long, value_enum, default_value_t = SearchAccuracy::Balanced)]
    search_accuracy: SearchAccuracy,
    /// Whether clips from one source track may synchronize together.
    #[arg(long, value_enum, default_value_t = TrackContent::Auto)]
    track_content: TrackContent,
    /// Timestamp evidence (Syncaila Time source).
    #[arg(long, value_enum, default_value_t = TimeSource::Auto)]
    time_source: TimeSource,
    /// Required waveform confidence.
    #[arg(long, value_enum, default_value_t = MatchThreshold::Balanced)]
    match_threshold: MatchThreshold,
    /// Preserve chronology when selecting between competing matches.
    #[arg(long, value_enum, default_value_t = ClipOrder::Auto)]
    clip_order: ClipOrder,
}

impl SyncSettings {
    fn pipeline_options(self) -> PipelineOptions {
        PipelineOptions {
            search_accuracy: self.search_accuracy.core(),
            temporal: align_core::TemporalPolicy {
                default: self.time_source.mode(),
                modes: std::collections::HashMap::new(),
            },
            match_policy: align_core::MatchPolicy {
                default: self.match_threshold.core(),
                thresholds: std::collections::HashMap::new(),
            },
            clip_order: align_core::ClipOrderPolicy {
                default: self.clip_order.core(),
                modes: std::collections::HashMap::new(),
            },
            track_content: align_core::TrackContentPolicy {
                default: self.track_content.core(),
                modes: std::collections::HashMap::new(),
            },
            ..PipelineOptions::default()
        }
    }
}

fn main() {
    let code = run();
    std::process::exit(code);
}

fn run() -> i32 {
    match run_inner(Cli::parse()) {
        Ok(()) => 0,
        Err(CliError::Usage) => {
            eprintln!(
                "usage: align-cli sync [OPTIONS] <media-or-folder> [...] | align-cli export [OPTIONS] <output-folder> <media-or-folder> [...] | align-cli export-json [OPTIONS] <result.json> <output-folder>"
            );
            2
        }
        Err(CliError::Failure(message)) => {
            eprintln!("error: {message}");
            1
        }
    }
}

enum CliError {
    Usage,
    Failure(String),
}

impl From<PipelineError> for CliError {
    fn from(error: PipelineError) -> Self {
        Self::Failure(error.to_string())
    }
}

impl From<align_decode::export::ExportError> for CliError {
    fn from(error: align_decode::export::ExportError) -> Self {
        Self::Failure(error.to_string())
    }
}

#[derive(Clone, Copy, Debug, clap::ValueEnum)]
enum TimeSource {
    Auto,
    #[value(name = "rec-start")]
    RecStart,
    #[value(name = "rec-stop")]
    RecStop,
    Timecode,
}

#[derive(Clone, Copy, Debug, Default, clap::ValueEnum)]
enum MatchThreshold {
    Permissive,
    #[default]
    Balanced,
    Conservative,
}

#[derive(Clone, Copy, Debug, Default, clap::ValueEnum)]
enum ClipOrder {
    #[default]
    Auto,
    AsImported,
    ByDateTime,
    ByFileName,
    Ignore,
}

#[derive(Clone, Copy, Debug, Default, clap::ValueEnum)]
enum TrackContent {
    #[default]
    Auto,
    Linear,
    Takes,
}

#[derive(Clone, Copy, Debug, Default, clap::ValueEnum)]
enum Unmatched {
    #[default]
    OrderTime,
    OrderOnly,
    Remove,
}

impl Unmatched {
    fn core(self) -> align_core::export_model::UnmatchedPlacement {
        use align_core::export_model::UnmatchedPlacement as P;
        match self {
            Self::OrderTime => P::ByOrderAndTime,
            Self::OrderOnly => P::ByOrderOnly,
            Self::Remove => P::Remove,
        }
    }
}

impl ClipOrder {
    fn core(self) -> align_core::ClipOrder {
        match self {
            Self::Auto => align_core::ClipOrder::Auto,
            Self::AsImported => align_core::ClipOrder::AsImported,
            Self::ByDateTime => align_core::ClipOrder::ByDateTime,
            Self::ByFileName => align_core::ClipOrder::ByFileName,
            Self::Ignore => align_core::ClipOrder::Ignore,
        }
    }
}

impl TrackContent {
    fn core(self) -> align_core::TrackContent {
        match self {
            Self::Auto => align_core::TrackContent::Auto,
            Self::Linear => align_core::TrackContent::Linear,
            Self::Takes => align_core::TrackContent::Takes,
        }
    }
}

impl MatchThreshold {
    fn core(self) -> align_core::MatchThreshold {
        match self {
            Self::Permissive => align_core::MatchThreshold::Permissive,
            Self::Balanced => align_core::MatchThreshold::Balanced,
            Self::Conservative => align_core::MatchThreshold::Conservative,
        }
    }
}

impl TimeSource {
    fn mode(self) -> align_core::TemporalMode {
        match self {
            Self::Auto => align_core::TemporalMode::Auto,
            Self::RecStart => align_core::TemporalMode::RecStart,
            Self::RecStop => align_core::TemporalMode::RecStop,
            Self::Timecode => align_core::TemporalMode::Timecode,
        }
    }
}

fn sequence_index(value: Option<usize>) -> Result<Option<usize>, CliError> {
    match value {
        None => Ok(None),
        Some(0) => Err(CliError::Failure(
            "--sequence requires a positive, one-based sequence number.".into(),
        )),
        Some(n) => Ok(Some(n - 1)),
    }
}

fn to_inputs(paths: &[PathBuf], sequence: Option<usize>) -> Vec<PipelineInput> {
    paths
        .iter()
        .map(|path| {
            let is_xml = path
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| e.eq_ignore_ascii_case("xml") || e.eq_ignore_ascii_case("fcpxml"))
                .unwrap_or(false);
            if !is_xml {
                return PipelineInput::Media(path.clone());
            }
            match sequence {
                Some(index) => PipelineInput::TimelineSequence(path.clone(), index),
                None => PipelineInput::Timeline(path.clone()),
            }
        })
        .collect()
}

fn progress_printer() -> impl Fn(align_decode::pipeline::PipelineProgress) + Send + Sync {
    |update| {
        let phase = match update.phase {
            Phase::Inspect => "inspect",
            Phase::Fingerprint => "fingerprint",
            Phase::Match => "match",
            Phase::Refine => "refine",
            Phase::Solve => "solve",
        };
        let name = update
            .current
            .as_ref()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .unwrap_or("");
        eprintln!("{} {}/{} {}", phase, update.completed, update.total, name);
    }
}

fn select_stage(
    result: &mut align_core::SyncResult,
    requested: Option<usize>,
) -> Result<(), CliError> {
    let Some(number) = requested else {
        return Ok(());
    };
    if number
        .checked_sub(1)
        .is_some_and(|index| result.select_stage(index))
    {
        return Ok(());
    }
    Err(CliError::Failure(format!(
        "Stage {number} is unavailable; choose 1 through {}.",
        result.stages.len().max(1)
    )))
}

fn print_json(value: &impl serde::Serialize) -> Result<(), CliError> {
    let mut out = serde_json::to_string_pretty(value)
        .map_err(|e| CliError::Failure(format!("JSON encode: {e}")))?;
    out.push('\n');
    std::io::stdout()
        .write_all(out.as_bytes())
        .map_err(|e| CliError::Failure(format!("stdout: {e}")))?;
    Ok(())
}

fn run_inner(cli: Cli) -> Result<(), CliError> {
    match cli.command {
        None if cli.paths.is_empty() => Err(CliError::Usage),
        None => {
            // Bare paths: inspect only (mirrors Swift `open`).
            let pipeline = Pipeline::default_backend();
            let inputs = to_inputs(&cli.paths, None);
            let redirects = align_core::redirect::load_from(&align_core::redirect::config_file());
            let options = PipelineOptions {
                redirects,
                ..Default::default()
            };
            let project = pipeline.open_with(&inputs, &options)?;
            print_json(&project)
        }
        Some(Command::Sync {
            sequence,
            settings,
            relink,
            paths,
        }) => {
            if paths.is_empty() {
                return Err(CliError::Usage);
            }
            let sequence = sequence_index(sequence)?;
            let pipeline = Pipeline::default_backend();
            let inputs = to_inputs(&paths, sequence);
            let progress = progress_printer();
            let cancel = std::sync::atomic::AtomicBool::new(false);
            let mut options = settings.pipeline_options();
            let (redirects, manual, omit) = relink_options(&relink)?;
            options.redirects = redirects;
            options.manual_relinks = manual;
            options.omit_extensions = omit;
            let mut result =
                pipeline.synchronize(&inputs, &[], &options, Some(&progress), &cancel)?;
            select_stage(&mut result, settings.stage)?;
            print_json(&result)
        }
        Some(Command::Export {
            sequence,
            no_drift,
            replaced_audio,
            export_media,
            unmatched,
            prevent_group_overlaps,
            disable_unmatched,
            label_unmatched,
            cut_remove,
            assign,
            relink,
            settings,
            output,
            paths,
        }) => {
            if paths.is_empty() {
                return Err(CliError::Failure(
                    "Export requires an output folder and at least one media path.".into(),
                ));
            }
            let sequence = sequence_index(sequence)?;
            let pipeline = Pipeline::default_backend();
            let inputs = to_inputs(&paths, sequence);
            let progress = progress_printer();
            let cancel = std::sync::atomic::AtomicBool::new(false);
            let mut options = settings.pipeline_options();
            let (redirects, manual, omit) = relink_options(&relink)?;
            options.redirects = redirects;
            options.manual_relinks = manual;
            options.omit_extensions = omit;
            let mut result =
                pipeline.synchronize(&inputs, &[], &options, Some(&progress), &cancel)?;
            select_stage(&mut result, settings.stage)?;
            let timeline = align_core::export_model::ExportTimeline::from_result_with_options(
                &result,
                align_core::export_model::ExportAssemblyOptions {
                    unmatched: unmatched.core(),
                    prevent_group_overlaps,
                    disable_unmatched,
                    label_unmatched,
                    cut_remove: cut_remove.core()?,
                    unmatched_symbol: assign.unmatched_symbol.clone(),
                    unmatched_symbol_suffix: assign.unmatched_symbol_suffix,
                    unmatched_color: assign.unmatched_color.clone(),
                    unmatched_role: assign.unmatched_role.clone(),
                    sequence_name: assign.sequence_name.clone(),
                },
            )
            .map_err(|e| CliError::Failure(e.to_string()))?;
            let cancel = std::sync::atomic::AtomicBool::new(false);
            let artifacts = align_decode::export::export_prepared(
                align_decode::export::ExportRequest {
                    backend: pipeline.backend(),
                    timeline: &timeline,
                    directory: &output,
                    formats: &align_core::export_model::TimelineExportFormat::default_formats(),
                    correct_drift: !no_drift,
                    include_replaced_sequence: replaced_audio,
                    include_media_files: export_media,
                    cancel: &cancel,
                },
                None,
            )?;
            print_json(&artifacts)
        }
        Some(Command::ExportJson {
            stage,
            no_drift,
            replaced_audio,
            export_media,
            unmatched,
            prevent_group_overlaps,
            disable_unmatched,
            label_unmatched,
            cut_remove,
            assign,
            result,
            output,
        }) => {
            let bytes = std::fs::read(&result)
                .map_err(|e| CliError::Failure(format!("read {}: {e}", result.display())))?;
            let mut sync_result: align_core::SyncResult = serde_json::from_slice(&bytes)
                .map_err(|e| CliError::Failure(format!("parse {}: {e}", result.display())))?;
            select_stage(&mut sync_result, stage)?;
            let pipeline = Pipeline::default_backend();
            let timeline = align_core::export_model::ExportTimeline::from_result_with_options(
                &sync_result,
                align_core::export_model::ExportAssemblyOptions {
                    unmatched: unmatched.core(),
                    prevent_group_overlaps,
                    disable_unmatched,
                    label_unmatched,
                    cut_remove: cut_remove.core()?,
                    unmatched_symbol: assign.unmatched_symbol.clone(),
                    unmatched_symbol_suffix: assign.unmatched_symbol_suffix,
                    unmatched_color: assign.unmatched_color.clone(),
                    unmatched_role: assign.unmatched_role.clone(),
                    sequence_name: assign.sequence_name.clone(),
                },
            )
            .map_err(|e| CliError::Failure(e.to_string()))?;
            let cancel = std::sync::atomic::AtomicBool::new(false);
            let artifacts = align_decode::export::export_prepared(
                align_decode::export::ExportRequest {
                    backend: pipeline.backend(),
                    timeline: &timeline,
                    directory: &output,
                    formats: &align_core::export_model::TimelineExportFormat::default_formats(),
                    correct_drift: !no_drift,
                    include_replaced_sequence: replaced_audio,
                    include_media_files: export_media,
                    cancel: &cancel,
                },
                None,
            )?;
            print_json(&artifacts)
        }
        Some(Command::ClearCache { older_than_days }) => {
            use align_core::FingerprintCache;
            let cache = FingerprintCache::new(None);
            let before = cache.statistics();
            if let Some(days) = older_than_days {
                cache.prune_older_than(std::time::Duration::from_secs(days * 24 * 60 * 60));
            } else {
                cache.clear();
            }
            let after = cache.statistics();
            print_json(&serde_json::json!({
                "cleared_files": before.file_count.saturating_sub(after.file_count),
                "cleared_bytes": before.total_bytes.saturating_sub(after.total_bytes),
                "remaining_files": after.file_count,
                "remaining_bytes": after.total_bytes,
            }))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn export_accepts_the_same_sync_settings_as_sync() {
        let cli = Cli::try_parse_from([
            "align-cli",
            "export",
            "--time-source",
            "timecode",
            "--match-threshold",
            "conservative",
            "--clip-order",
            "by-file-name",
            "--track-content",
            "linear",
            "--no-drift",
            "--replaced-audio",
            "--export-media",
            "--label-unmatched",
            "/tmp/out",
            "/tmp/media",
        ])
        .expect("export arguments");
        let Some(Command::Export {
            settings,
            no_drift,
            replaced_audio,
            export_media,
            label_unmatched,
            ..
        }) = cli.command
        else {
            panic!("export command")
        };
        let options = settings.pipeline_options();

        assert_eq!(options.temporal.default, align_core::TemporalMode::Timecode);
        assert_eq!(
            options.match_policy.default,
            align_core::MatchThreshold::Conservative
        );
        assert_eq!(
            options.clip_order.default,
            align_core::ClipOrder::ByFileName
        );
        assert_eq!(
            options.track_content.default,
            align_core::TrackContent::Linear
        );
        assert!(no_drift);
        assert!(replaced_audio);
        assert!(export_media);
        assert!(label_unmatched);
    }

    #[test]
    fn relink_and_cleanup_flags_parse() {
        let cli = Cli::try_parse_from([
            "align-cli",
            "sync",
            "--redirect",
            "/old=/new",
            "--relink",
            "a.mov=/new/a.mov",
            "--clear-redirects",
            "--omit-extensions",
            "jpg,png",
            "/tmp/media",
        ])
        .expect("sync arguments");
        let Some(Command::Sync { relink, .. }) = cli.command else {
            panic!("sync command")
        };
        assert_eq!(relink.redirect, vec!["/old=/new".to_string()]);
        assert_eq!(relink.relink, vec!["a.mov=/new/a.mov".to_string()]);
        assert!(relink.clear_redirects);
        assert_eq!(relink.omit_extensions.as_deref(), Some("jpg,png"));

        let cli = Cli::try_parse_from(["align-cli", "clear-cache", "--older-than-days", "7"])
            .expect("clear-cache arguments");
        assert!(matches!(
            cli.command,
            Some(Command::ClearCache {
                older_than_days: Some(7)
            })
        ));
    }
}
