//! Worker events wake the UI only when there is data; idle has no polling timer.
use crate::State;
use align_core::SyncResult;
use align_decode::pipeline::{Phase, PipelineProgress};
use align_ui::state::Operation;
use std::path::PathBuf;

pub enum Message {
    Sequence(usize, usize),
    Progress(Box<PipelineProgress>, usize, usize),
    Synced(Result<Vec<SyncResult>, String>),
    Exported(Result<Vec<PathBuf>, String>),
}
pub struct Event {
    generation: u64,
    message: Message,
}

impl State {
    pub fn synchronize(&mut self) {
        if !self.data.begin_sync_run() {
            return;
        }
        let generation = self.data.generation;
        let cancel = self.data.cancel.clone();
        let inputs = self.data.pipeline_input_sets();
        let settings: Vec<_> = (0..inputs.len())
            .map(|position| {
                (
                    self.data
                        .constraints_for_sequence(self.data.sequence_key_for_position(position)),
                    self.data.pipeline_options_for_run(position),
                    self.data.quality_retry_targets_for_position(position),
                    self.data.quality_retry_urls_for_position(position),
                )
            })
            .collect();
        let sender = self.sender.clone();
        let wake = self.wake.clone();
        std::thread::spawn(move || {
            let send = |message| {
                let ok = sender
                    .send(Event {
                        generation,
                        message,
                    })
                    .is_ok();
                wake();
                ok
            };
            let pipeline = align_ui::experiment::pipeline();
            let count = inputs.len();
            let mut results = Vec::new();
            for (index, (inputs, (constraints, options, targets, urls))) in
                inputs.into_iter().zip(settings).enumerate()
            {
                if !send(Message::Sequence(index, count)) {
                    return;
                }
                let progress = |mut p: PipelineProgress| {
                    if targets.is_some() {
                        p.discovered = None;
                        if matches!(p.phase, Phase::Inspect | Phase::Fingerprint)
                            && p.current.as_ref().is_some_and(|p| !urls.contains(p))
                        {
                            p.current = None;
                        }
                        p.preview = None;
                    }
                    send(Message::Progress(Box::new(p), index, count));
                };
                match pipeline.synchronize(
                    &inputs,
                    &constraints,
                    &options,
                    Some(&progress),
                    &cancel,
                ) {
                    Ok(result) => results.push(result),
                    Err(error) => {
                        send(Message::Synced(Err(error.to_string())));
                        return;
                    }
                }
            }
            send(Message::Synced(Ok(results)));
        });
    }

    pub fn export(&mut self) {
        use align_core::export_model::{ExportAssemblyOptions, ExportTimeline};
        if !self.data.can_begin_export() {
            return;
        }
        let Some(directory) = self.data.export_dir.clone() else {
            return;
        };
        let results = if self.data.sequence_results.is_empty() {
            self.data.result.iter().collect::<Vec<_>>()
        } else {
            self.data.sequence_results.iter().collect()
        };
        let timelines = results
            .into_iter()
            .map(|result| {
                ExportTimeline::from_result_with_options(
                    result,
                    ExportAssemblyOptions {
                        unmatched: self.data.export_unmatched,
                        prevent_group_overlaps: self.data.export_prevent_overlaps,
                        disable_unmatched: self.data.export_disable_unmatched,
                        ..Default::default()
                    },
                )
            })
            .collect::<Result<Vec<_>, _>>();
        let timelines = match timelines {
            Ok(t) => t,
            Err(e) => {
                self.data.error = Some(e.to_string());
                return;
            }
        };
        let formats = self.data.export_targets();
        let correct_drift = self.data.export_drift;
        let cancel = self.data.cancel.clone();
        let sender = self.sender.clone();
        let wake = self.wake.clone();
        let generation = self.data.generation;
        self.data.operation = Operation::Exporting;
        self.data.status = "Writing timelines…".into();
        std::thread::spawn(move || {
            let pipeline = align_ui::experiment::pipeline();
            let result = align_decode::export::export_prepared_many(
                align_decode::export::ExportBatchRequest {
                    backend: pipeline.backend(),
                    timelines: &timelines,
                    directory: &directory,
                    formats: &formats,
                    correct_drift,
                    include_replaced_sequence: false,
                    include_media_files: false,
                    aaf_frame_duration: None,
                    include_fcpxml_timeline: true,
                    include_fcpxml_multicam: true,
                    group_fcpxml_storylines: false,
                    cancel: &cancel,
                },
                None,
            )
            .map(|artifacts| artifacts.into_iter().map(|a| a.url).collect())
            .map_err(|e| e.to_string());
            let _ = sender.send(Event {
                generation,
                message: Message::Exported(result),
            });
            wake();
        });
    }

    pub fn apply(&mut self, event: Event) -> bool {
        if event.generation != self.data.generation {
            return false;
        }
        match event.message {
            Message::Sequence(index, count) => {
                self.data.begin_sequence_progress();
                self.data.status = format!("Sequence {}/{} · Reading metadata…", index + 1, count);
            }
            Message::Progress(mut p, index, count) => {
                if let Some((id, waveform)) = p.waveform.take() {
                    self.data.waveform_previews.insert(id, waveform);
                }
                let phase = match p.phase {
                    Phase::Inspect => "inspect",
                    Phase::Fingerprint => "fingerprint",
                    Phase::Match => "match",
                    Phase::Refine => "refine",
                    Phase::Solve => "solve",
                };
                let name = p
                    .current
                    .as_ref()
                    .and_then(|p| p.file_name())
                    .map(|p| p.to_string_lossy().into_owned());
                self.data.apply_progress(
                    phase,
                    p.completed,
                    p.total,
                    name.as_deref(),
                    p.discovered.as_ref(),
                    p.preview.as_ref(),
                );
                if count > 1 {
                    self.data.status =
                        format!("Sequence {}/{} · {}", index + 1, count, self.data.status);
                    self.data.progress = (index as f32 + self.data.progress) / count as f32;
                }
            }
            Message::Synced(Ok(results)) => {
                if let Some(result) = results.first() {
                    align_ui::experiment::save(result);
                }
                self.data.apply_results(results);
                eprintln!("ALIGN_EXPERIMENT_SYNC_DONE");
            }
            Message::Synced(Err(e)) => {
                if self.data.cancel.load(std::sync::atomic::Ordering::Relaxed) {
                    self.data.restore_after_sync_cancel();
                } else {
                    self.data.operation = Operation::Idle;
                    self.data.error = Some(e);
                }
            }
            Message::Exported(result) => match result {
                Ok(paths) => {
                    self.data.exported_files = paths;
                    self.data.operation = Operation::Exported;
                    self.data.status = "Export complete.".into();
                    self.data.progress = 1.0;
                }
                Err(e) => {
                    self.data.operation = Operation::Ready;
                    self.data.error = Some(e);
                }
            },
        }
        true
    }
}
