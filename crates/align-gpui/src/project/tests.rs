use std::fs::FileTimes;

use align_core::{
    SearchAccuracy, SyncConstraint,
    export::{ExportTimeline, otio},
};
use align_decode::{
    BackendKind,
    pipeline::{Phase, Pipeline, PipelineOptions},
};

use super::*;
use crate::{
    lane::CorrectionOption,
    state::{AppData, SettingsScope},
};

fn save(writer: &Writer, path: &Destination, data: &AppData, placements: Option<Arc<Placements>>) {
    futures::executor::block_on(writer.submit(
        path.clone(),
        data.project_session(),
        placements,
        SaveMode::Replace,
    ))
    .unwrap()
    .unwrap();
}

fn restore(path: &Path) -> AppData {
    let opened = open(path).unwrap();
    let mut data = AppData::default();
    data.restore_project(
        opened.session,
        opened.results,
        opened.changed,
        opened.embedded_unavailable,
    );
    data
}

fn fixture() -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir().unwrap();
    let root = align_core::model::canonical_path(temp.path()).unwrap();
    (temp, root)
}

fn write_takes(root: &Path) -> (PathBuf, PathBuf) {
    let first = root.join("first.wav");
    let second = root.join("second.wav");
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: 44_100,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut a = hound::WavWriter::create(&first, spec).unwrap();
    let mut b = hound::WavWriter::create(&second, spec).unwrap();
    for _ in 0..44_100 {
        b.write_sample(0_i16).unwrap();
    }
    let mut seed = 97_u64;
    for _ in 0..24 * 44_100 {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
        let sample = (seed >> 48) as i16 / 2;
        a.write_sample(sample).unwrap();
        b.write_sample(sample).unwrap();
    }
    a.finalize().unwrap();
    b.finalize().unwrap();
    (first, second)
}

#[test]
fn saved_sync_reopens_for_export_without_its_cache_and_relinks_moved_media() {
    let (_temp, root) = fixture();
    let (first, second) = write_takes(&root);
    let cache = root.join("analysis-cache");
    let destination = Destination::autosave(&root.join("projects"));
    let writer = Writer::default();

    let mut data = AppData::default();
    data.inputs = vec![first.clone(), second.clone()];
    save(&writer, &destination, &data, None);
    let initial = restore(&destination.path);
    assert!(initial.can_synchronize());
    assert!(!initial.can_export());

    let revisions = Mutex::new(Revisions::new());
    let result = Pipeline::new_in(BackendKind::Portable, Some(cache.clone()))
        .synchronize(
            &initial.pipeline_input_sets()[0],
            &[],
            &PipelineOptions::default(),
            Some(&|event| {
                if event.phase == Phase::Inspect
                    && let Some(path) = event.current
                {
                    record_revision(&mut revisions.lock().unwrap(), &path);
                }
            }),
            &AtomicBool::new(false),
        )
        .unwrap();
    assert_eq!(result.matches.len(), 1);
    assert!((result.matches[0].offset.as_seconds().abs() - 1.0).abs() < 0.005);
    let placements = Arc::new(Placements::from_run(
        std::slice::from_ref(&result),
        revisions.into_inner().unwrap(),
    ));
    data.apply_result(result);
    save(&writer, &destination, &data, Some(placements.clone()));
    fs::remove_dir_all(cache).unwrap();
    let mut reopened = restore(&destination.path);
    assert!(reopened.can_export());
    assert!(!reopened.lanes.is_empty());
    let result = reopened.result.as_ref().unwrap();
    assert!((result.matches[0].offset.as_seconds().abs() - 1.0).abs() < 0.005);
    let timeline = ExportTimeline::from_result(result, true).unwrap();
    let exported = String::from_utf8(otio::data(&timeline, false).unwrap()).unwrap();
    assert!(exported.contains("first.wav") && exported.contains("second.wav"));

    let target = CorrectionOption {
        id: "pair".to_owned(),
        left: result.matches[0].left.clone(),
        right: result.matches[0].right.clone(),
        other_name: "other".into(),
        offset: result.matches[0].offset.as_seconds(),
    };
    let lane = reopened.lanes[0].id.clone();
    assert!(reopened.set_lane_search_accuracy(Some(SearchAccuracy::Deep), &lane));
    assert!(reopened.reject_pair(&target));
    assert!(reopened.reject_alignment(&target));
    reopened.mark_sync_dirty();
    save(&writer, &destination, &reopened, Some(placements));

    let dirty = restore(&destination.path);
    assert!(dirty.has_result() && dirty.can_synchronize() && !dirty.can_export());
    assert!(!dirty.can_relink_media());
    let new_revision = FileTimes::new().set_modified(SystemTime::now() - Duration::from_secs(60));
    fs::OpenOptions::new()
        .write(true)
        .open(&second)
        .unwrap()
        .set_times(new_revision)
        .unwrap();
    assert!(restore(&destination.path).can_relink_media());

    let moved = root.join("moved").join("second.wav");
    fs::create_dir(moved.parent().unwrap()).unwrap();
    fs::rename(&second, &moved).unwrap();
    let mut stale = restore(&destination.path);
    assert!(!stale.can_export());
    assert!(stale.can_relink_media());
    assert!(stale.can_synchronize());
    assert_eq!(
        stale.lane_search_accuracy(&lane),
        Some(SearchAccuracy::Deep)
    );
    assert_eq!(stale.add_manual_relinks(vec![moved.clone()]), 1);
    assert_eq!(stale.inputs, [first, moved.clone()]);
    assert_eq!(
        stale.pipeline_options_for_sequence(0).manual_relinks,
        vec![("second.wav".into(), moved.clone())]
    );
    let clips = &stale.result.as_ref().unwrap().project.clips;
    let old_clip = clips.iter().find(|clip| clip.url == second).unwrap();
    let replaced =
        align_decode::pipeline::clip_id_for(&moved.to_string_lossy(), old_clip.duration.value);
    let (left, right) = if old_clip.id == target.left {
        (&replaced, &target.right)
    } else {
        (&target.left, &replaced)
    };
    let constraints = stale.constraints_for_sequence(0);
    assert!(SyncConstraint::rejects_pair(&constraints, left, right));
    assert!(SyncConstraint::rejects_alignment(
        &constraints,
        left,
        right,
        target.offset
    ));
    let archived = open(&destination.path).unwrap().placements;
    save(&writer, &destination, &stale, archived);
    let relinked = restore(&destination.path);
    assert_eq!(
        relinked.pipeline_options_for_sequence(0).manual_relinks,
        vec![("second.wav".into(), moved)]
    );
    assert!(!relinked.can_export());
}

#[test]
fn repeated_checkpoints_keep_the_latest_sequence_settings_after_reopen() {
    let (_temp, root) = fixture();
    let writer = Writer::default();
    let destination = Destination::autosave(&root);
    let source = root.join("edits.xml");
    fs::write(
        &source,
        "<xmeml><sequence><name>Morning</name></sequence><sequence><name>Evening</name></sequence></xmeml>",
    ).unwrap();
    let mut data = AppData::default();
    let token = data.import_cancel.clone();
    let paths = data.begin_import(vec![source.clone()]);
    let inputs = paths
        .into_iter()
        .map(|path| crate::state::InputPath::inspect(path, &token))
        .collect();
    data.finish_import(&token, inputs);
    data.choose_all_sequences();
    save(&writer, &destination, &data, None);
    for accuracy in [
        SearchAccuracy::Fast,
        SearchAccuracy::Deep,
        SearchAccuracy::Exhaustive,
    ] {
        data.settings_scope = SettingsScope::CurrentSequence;
        assert!(data.set_scoped_search_accuracy(Some(accuracy)));
        drop(writer.submit(
            destination.clone(),
            data.project_session(),
            None,
            SaveMode::Coalesced,
        ));
    }
    writer.flush();

    let reopened = restore(&destination.path);
    assert_eq!(&reopened.inputs, std::slice::from_ref(&source));
    assert_eq!(reopened.sequence_key_for_position(0), 0);
    assert_eq!(reopened.sequence_key_for_position(1), 1);
    assert_eq!(reopened.pipeline_input_sets().len(), 2);
    assert_eq!(
        reopened.effective_settings_for(0).search_accuracy,
        SearchAccuracy::Exhaustive
    );
    assert_eq!(
        reopened.effective_settings_for(1).search_accuracy,
        SearchAccuracy::Balanced
    );
    assert_eq!(managed_projects(&root, SystemTime::now()).len(), 1);
}

#[test]
fn startup_expiry_keeps_manual_documents_copies_and_non_project_files() {
    let (_temp, root) = fixture();
    let project_dir = root.join("projects");
    fs::create_dir(&project_dir).unwrap();
    let data = AppData::default();
    let writer = Writer::default();
    let expired = Destination::autosave(&project_dir);
    let refreshed = Destination::autosave(&project_dir);
    let converted = Destination::autosave(&project_dir);
    for dest in [&expired, &refreshed, &converted] {
        save(&writer, dest, &data, None);
    }
    let converted = Destination {
        managed: false,
        ..converted
    };
    save(&writer, &converted, &data, None);
    let foreign = project_dir.join("autosave-12-34-56.align");
    fs::write(&foreign, b"Unrelated file").unwrap();
    let failed = writer.submit(
        Destination {
            path: foreign.clone(),
            managed: false,
        },
        data.project_session(),
        None,
        SaveMode::Replace,
    );
    assert!(futures::executor::block_on(failed).unwrap().is_err());
    let copy = root.join(expired.path.file_name().unwrap());
    fs::copy(&expired.path, &copy).unwrap();
    let old =
        FileTimes::new().set_modified(SystemTime::now() - RETENTION - Duration::from_secs(60));
    for path in [
        &expired.path,
        &refreshed.path,
        &converted.path,
        &foreign,
        &copy,
    ] {
        fs::OpenOptions::new()
            .write(true)
            .open(path)
            .unwrap()
            .set_times(old)
            .unwrap();
    }
    save(&writer, &refreshed, &data, None);
    #[cfg(unix)]
    let (link, target) = {
        let alias = root.join("alias");
        std::os::unix::fs::symlink(&project_dir, &alias).unwrap();
        assert!(managed_projects(&alias, SystemTime::now()).is_empty());
        assert!(expired.path.is_file());
        // Make a real owned file reachable only through a symlink at its
        // original path. Expiry has enough other evidence to delete it.
        let link = Destination::autosave(&project_dir);
        save(&writer, &link, &data, None);
        let target = root.join("target");
        fs::rename(&link.path, &target).unwrap();
        fs::OpenOptions::new()
            .write(true)
            .open(&target)
            .unwrap()
            .set_times(old)
            .unwrap();
        std::os::unix::fs::symlink(&target, &link.path).unwrap();
        (link.path, target)
    };
    let settings = root.join("settings.json");
    let mut configured = Preferences {
        disabled: true,
        folder: Some(project_dir.clone()),
        ..Default::default()
    };
    configured.set_folder(root.join("new autosave location"));
    configured.remember(Recent::new(converted.clone(), "Manual".into()));
    configured.save(&settings).unwrap();
    let restarted = Preferences::load(&settings);
    assert!(restarted.disabled);
    assert_eq!(restarted.autosave_folder(), configured.autosave_folder());
    assert_eq!(
        restarted
            .recent
            .iter()
            .map(|item| &item.path)
            .collect::<Vec<_>>(),
        [&converted.path, &refreshed.path]
    );
    assert!(!expired.path.exists());
    #[cfg(unix)]
    assert!(link.is_symlink() && target.is_file());
    assert!(refreshed.path.is_file());
    assert!(converted.path.is_file() && copy.is_file());
    assert!(managed_projects(&root, SystemTime::now()).is_empty());
    assert_eq!(fs::read(&foreign).unwrap(), b"Unrelated file");
    assert!(!open(&converted.path).unwrap().destination.managed);
    assert!(!open(&copy).unwrap().destination.managed);
}

#[test]
fn embedded_aaf_project_reextracts_same_media_or_keeps_a_safe_stale_result() {
    let (_temp, root) = fixture();
    // The bridge is an optional packaged executable. Its real fixture can
    // also be supplied for a re-extraction run outside the normal Rust suite.
    let supplied = std::env::var_os("ALIGN_TEST_EMBEDDED_AAF");
    let source = root.join("edit.aaf");
    if let Some(path) = supplied.as_ref() {
        fs::copy(path, &source).unwrap();
    } else {
        fs::write(&source, b"AAF unavailable").unwrap();
    }
    let mut data = AppData::default();
    data.inputs.push(source.clone());
    data.timeline_inputs.insert(source.clone());
    let mut revisions = Revisions::new();
    record_revision(&mut revisions, &source);
    let pipe = Pipeline::new_in(BackendKind::Portable, Some(root.join("cache")));
    let cancel = AtomicBool::new(false);
    let route = if supplied.is_some() {
        data.pipeline_input_sets().pop().unwrap()
    } else {
        let extracted = align_decode::media_assets::extraction_directory().unwrap();
        let temporary = extracted.join(format!("{:064x}.wav", 42));
        let (first, _) = write_takes(&root);
        fs::copy(first, &temporary).unwrap();
        vec![align_decode::pipeline::PipelineInput::Media(temporary)]
    };
    let result = pipe
        .synchronize(&route, &[], &PipelineOptions::default(), None, &cancel)
        .unwrap();
    let old = result.project.clips[0].url.clone();
    assert!(embedded_aaf_name(&old).is_some());
    record_revision(&mut revisions, &old);
    let placements = Arc::new(Placements::from_run(
        std::slice::from_ref(&result),
        revisions,
    ));
    let id = result.project.clips[0].id.clone();
    data.apply_result(result);
    let path = Destination {
        path: root.join("saved.align"),
        managed: false,
    };
    save(&Writer::default(), &path, &data, Some(placements));
    assert!(
        !fs::read_to_string(&path.path)
            .unwrap()
            .contains(old.to_str().unwrap())
    );
    align_decode::media_assets::cleanup();
    assert!(!old.exists());

    if supplied.is_some() {
        let restored = restore(&path.path);
        assert!(restored.can_export());
        let result = restored.result.unwrap();
        let clip = &result.project.clips[0];
        assert_eq!(clip.id, id);
        assert_ne!(clip.url, old);
        assert!(clip.url.is_file());
        let rerun = pipe
            .synchronize(&route, &[], &PipelineOptions::default(), None, &cancel)
            .unwrap();
        assert_eq!(rerun.project.clips[0].id, id);
        use align_core::export::TimelineExportFormat;
        use align_decode::export::{ExportOptions, ExportRequest, export_prepared};
        let output = root.join("export");
        let timeline = ExportTimeline::from_result(&result, true).unwrap();
        let export = export_prepared(
            ExportRequest {
                backend: pipe.backend(),
                timeline: &timeline,
                directory: &output,
                options: &ExportOptions {
                    formats: vec![TimelineExportFormat::ResolveOTIO],
                    ..Default::default()
                },
                cancel: &cancel,
            },
            None,
        )
        .unwrap();
        align_decode::media_assets::cleanup();
        let otio = fs::read_to_string(&export[0].url).unwrap();
        let retained = output
            .join("Source Media")
            .join(clip.url.file_name().unwrap());
        assert!(retained.is_file());
        assert!(otio.contains("Source%20Media") || otio.contains("Source Media"));
        fs::rename(&source, root.join("moved.aaf")).unwrap();
    }
    let stale = restore(&path.path);
    assert!(!stale.can_export());
    assert!(stale.can_synchronize());
    assert_eq!(stale.result.unwrap().project.clips[0].id, id);
    align_decode::media_assets::cleanup();
}

#[test]
fn dirty_folder_project_relinks_an_individually_moved_recording() {
    let (_temp, root) = fixture();
    let folder = root.join("shoot");
    fs::create_dir(&folder).unwrap();
    let (first, second) = write_takes(&folder);
    let writer = Writer::default();
    let destination = Destination::autosave(&root.join("projects"));

    let mut data = AppData::default();
    let pending = data.begin_import(vec![folder.clone()]);
    let token = data.import_cancel.clone();
    data.finish_import(
        &token,
        pending
            .into_iter()
            .map(|path| crate::state::InputPath::inspect(path, &token))
            .collect(),
    );
    let pipeline = Pipeline::new_in(BackendKind::Portable, Some(root.join("cache")));
    let cancel = AtomicBool::new(false);
    let result = pipeline
        .synchronize(
            &data.pipeline_input_sets()[0],
            &[],
            &PipelineOptions::default(),
            None,
            &cancel,
        )
        .unwrap();
    assert_eq!(result.matches.len(), 1);
    let mut revisions = Revisions::new();
    for path in [&first, &second] {
        record_revision(&mut revisions, path);
    }
    let placements = Placements::from_run(std::slice::from_ref(&result), revisions);
    data.apply_result(result);
    data.mark_sync_dirty();
    save(&writer, &destination, &data, Some(Arc::new(placements)));

    let moved = root.join("second.wav");
    fs::rename(second, &moved).unwrap();
    let mut restored = restore(&destination.path);
    let adds_folder_row = restored.clips.iter().any(|row| row.url == folder);
    assert!(restored.can_relink_media());
    assert_eq!(restored.add_manual_relinks(vec![moved.clone()]), 1);
    let result = pipeline
        .synchronize(
            &restored.pipeline_input_sets()[0],
            &[],
            &restored.pipeline_options_for_sequence(0),
            None,
            &cancel,
        )
        .unwrap();
    let has_moved_clip = result.project.clips.iter().any(|clip| clip.url == moved);
    assert!(
        !adds_folder_row && has_moved_clip,
        "folder added as a clip: {adds_folder_row}; relocated recording found: {has_moved_clip}"
    );
    assert_eq!(result.matches.len(), 1);
}

#[test]
fn startup_skips_expiry_while_another_writer_holds_the_lock() {
    let (_temp, root) = fixture();
    let destination = Destination::autosave(&root);
    save(&Writer::default(), &destination, &AppData::default(), None);
    let lock = ManagedDirectory::open(&root).unwrap().lock(true).unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        let future = SystemTime::now() + RETENTION + Duration::from_secs(60);
        let _ = tx.send(managed_projects(&root, future));
    });
    let result = rx.recv_timeout(Duration::from_secs(2));
    drop(lock);
    worker.join().unwrap();
    assert!(
        result
            .expect("startup must not wait for a writer")
            .is_empty()
    );
    assert!(destination.path.is_file());
}

#[test]
fn manually_saved_project_can_be_resaved_after_json_reformatting() {
    let (_temp, root) = fixture();
    let destination = Destination {
        path: root.join("Wedding.align"),
        managed: false,
    };
    let writer = Writer::default();
    let mut data = AppData::default();
    save(&writer, &destination, &data, None);
    let value: serde_json::Value =
        serde_json::from_slice(&fs::read(&destination.path).unwrap()).unwrap();
    fs::write(
        &destination.path,
        serde_json::to_vec_pretty(&value).unwrap(),
    )
    .unwrap();

    data.common_settings.search_accuracy = SearchAccuracy::Deep;
    save(&writer, &destination, &data, None);
    assert_eq!(
        restore(&destination.path).common_settings.search_accuracy,
        SearchAccuracy::Deep
    );
}

/// A recorder and camera clips C1, C2 and C4 synchronized by sound. C3 is
/// unmatched in the waveform stage; the timecode stage, selected, places it.
fn camera_clock_shoot(root: &Path) -> SyncResult {
    use align_core::{
        AudioSummary, Clip, ClipId, ClipPlacement, MappingPoint, MatchEvidence, MatchSummary,
        MediaKind, MediaTime, RecordingTimestampSource, SyncIsland, SyncProject, SyncStage,
        SyncStageKind, TimeMap, VideoSummary,
    };
    const T0: i64 = 1_741_082_400;
    let camera = |name: &str, offset: i64, duration: f64| Clip {
        id: ClipId::new(name),
        url: root.join(format!("{name}.MP4")),
        kind: MediaKind::Video,
        duration: MediaTime::seconds(duration),
        audio: vec![AudioSummary {
            sample_rate: 48_000.0,
            channels: 2,
            bit_depth: Some(16),
            is_float: Some(false),
            source_timecode: None,
        }],
        video: Some(VideoSummary {
            width: 1920,
            height: 1080,
            frame_duration: Some(MediaTime::new(1, 25)),
            source_timecode: None,
            frame_rate_mode: None,
        }),
        recorded_at: Some(T0 + offset),
        recorded_at_source: Some(RecordingTimestampSource::EmbeddedMetadata),
        source_identifier: Some("device:Sony:ILME-FX3:4711".into()),
        media_span: None,
    };
    let recorder = Clip {
        id: ClipId::new("R"),
        url: root.join("R.WAV"),
        kind: MediaKind::Audio,
        video: None,
        recorded_at: None,
        recorded_at_source: None,
        source_identifier: None,
        ..camera("R", 0, 1200.0)
    };
    let clips = vec![
        recorder,
        camera("C1", 0, 60.0),
        camera("C2", 300, 60.0),
        camera("C3", 600, 90.0),
        camera("C4", 800, 60.0),
    ];
    let place = |name: &str, start: f64| {
        let duration = clips.iter().find(|c| c.id.0 == name).unwrap().duration;
        ClipPlacement {
            clip_id: ClipId::new(name),
            mapping: TimeMap {
                points: vec![
                    MappingPoint {
                        source: MediaTime::seconds(0.0),
                        island: MediaTime::seconds(start),
                    },
                    MappingPoint {
                        source: duration,
                        island: MediaTime::seconds(start + duration.as_seconds()),
                    },
                ],
            },
            confidence: 0.9,
        }
    };
    let matched = |name: &str, evidence| MatchSummary {
        left: ClipId::new("R"),
        right: ClipId::new(name),
        drift_ppm: 0.0,
        offset: MediaTime::seconds(0.0),
        confidence: 0.9,
        anchors: 3,
        covered: MediaTime::seconds(60.0),
        residual: MediaTime::seconds(0.0),
        evidence: Some(evidence),
    };
    let sound: Vec<_> = ["C1", "C2", "C4"]
        .into_iter()
        .map(|name| matched(name, MatchEvidence::Waveform))
        .collect();
    let mut placements = vec![
        place("R", 0.0),
        place("C1", 100.0),
        place("C2", 400.0),
        place("C4", 899.6),
    ];
    let waveform = SyncStage {
        kind: SyncStageKind::Waveform,
        islands: vec![SyncIsland {
            id: 0,
            placements: placements.clone(),
        }],
        unmatched: vec![ClipId::new("C3")],
        matches: sound.clone(),
    };
    placements.push(place("C3", 700.0));
    let mut with_timecode = sound;
    with_timecode.push(matched("C3", MatchEvidence::Timecode));
    let timecode = SyncStage {
        kind: SyncStageKind::Timecode,
        islands: vec![SyncIsland { id: 0, placements }],
        unmatched: Vec::new(),
        matches: with_timecode,
    };
    let mut result = SyncResult {
        search_overrides: HashMap::new(),
        stopped: false,
        stages: vec![waveform, timecode],
        selected_stage: None,
        search_accuracy: Default::default(),
        preserve_editing_tracks: Default::default(),
        project: SyncProject {
            clips,
            warnings: Vec::new(),
            imported_timeline: None,
        },
        islands: Vec::new(),
        unmatched: Vec::new(),
        matches: Vec::new(),
        temporal_policy: Default::default(),
    };
    assert!(result.select_stage(1));
    result
}

/// `camera_clock_shoot` with the recorder dated by its file, which places
/// the ordinary C3 slot first.
fn file_dated_shoot(root: &Path) -> SyncResult {
    let mut shoot = camera_clock_shoot(root);
    shoot.project.clips[0].recorded_at = Some(1_800_000_000);
    shoot.project.clips[0].recorded_at_source =
        Some(align_core::RecordingTimestampSource::FileSystem);
    shoot
}

/// Every bar of the timeline preview: name, start, duration.
fn bar_layout(data: &AppData) -> Vec<(String, f64, f64)> {
    data.lanes
        .iter()
        .flat_map(|lane| &lane.clips)
        .map(|bar| (bar.name.clone(), bar.start, bar.duration))
        .collect()
}

#[test]
fn camera_clock_choices_follow_stage_changes_and_the_project() {
    use crate::lane::BarMatchState;
    let (_temp, root) = fixture();
    let c3 = align_core::ClipId::new("C3");
    let state = |data: &AppData| {
        data.lanes
            .iter()
            .flat_map(|lane| &lane.clips)
            .find(|bar| bar.clip_id == c3)
            .map(|bar| (bar.match_state, bar.start))
    };
    let mut data = AppData::default();
    data.apply_result(file_dated_shoot(&root));
    assert_eq!(state(&data).unwrap().0, BarMatchState::Matched);
    assert!(!data.set_camera_clock(std::slice::from_ref(&c3), true));

    // The waveform stage offers an estimate; nothing applies it unasked.
    assert!(data.select_sync_stage(0));
    assert_eq!(state(&data).unwrap().0, BarMatchState::Unmatched);
    assert!(data.camera_clock_for_export(0).is_empty());
    assert!(data.set_camera_clock(std::slice::from_ref(&c3), true));
    let (placed, start) = state(&data).unwrap();
    assert_eq!(placed, BarMatchState::CameraClock);
    // Export applies the estimate the preview shows.
    let exported = data.camera_clock_for_export(0);
    assert_eq!(exported.len(), 1);
    assert!((exported[0].start + 91.0 - start).abs() < 1e-6);

    // Remove unmatched leaves no estimate to apply, so the preview is the
    // ordinary one, as for a project that never chose the placement.
    let mut settings = data.export.clone();
    settings.unmatched = align_core::export_model::UnmatchedPlacement::Remove;
    data.apply_export_settings(&settings);
    assert!(data.camera_clock_for_export(0).is_empty());
    let mut never_chosen = AppData::default();
    never_chosen.apply_result(file_dated_shoot(&root));
    never_chosen.select_sync_stage(0);
    never_chosen.apply_export_settings(&settings);
    assert_eq!(state(&never_chosen).unwrap().0, BarMatchState::Unmatched);
    assert_eq!(bar_layout(&data), bar_layout(&never_chosen));
    settings.unmatched = align_core::export_model::UnmatchedPlacement::ByOrderOnly;
    data.apply_export_settings(&settings);
    assert_eq!(state(&data), Some((placed, exported[0].start)));

    // Another stage synchronizes C3; returning restores the placement.
    assert!(data.select_sync_stage(1));
    assert_eq!(state(&data).unwrap().0, BarMatchState::Matched);
    assert!(data.camera_clock_for_export(0).is_empty());
    assert!(data.select_sync_stage(0));
    assert_eq!(state(&data).unwrap().0, BarMatchState::CameraClock);

    let writer = Writer::default();
    let destination = Destination::autosave(&root.join("projects"));
    let placements = Arc::new(Placements::from_run(
        std::slice::from_ref(data.result.as_ref().unwrap()),
        Revisions::new(),
    ));
    save(&writer, &destination, &data, Some(placements));
    let reopened = restore(&destination.path);
    assert_eq!(state(&reopened).unwrap().0, BarMatchState::CameraClock);

    // A project saved before camera-clock placement opens without it.
    let mut session = serde_json::to_value(data.project_session()).unwrap();
    assert!(
        session
            .as_object_mut()
            .unwrap()
            .remove("cameraClock")
            .is_some()
    );
    let mut older = AppData::default();
    older.restore_project(
        serde_json::from_value(session).unwrap(),
        vec![data.result.clone().unwrap()],
        Vec::new(),
        false,
    );
    assert_eq!(state(&older).unwrap().0, BarMatchState::Unmatched);
}

#[test]
fn rejected_clock_choices_hold_for_place_all_one_at_a_time_and_reopen() {
    use crate::lane::CorrectionOption;
    use align_core::ClipId;
    let (_temp, root) = fixture();
    let shoot = competing_camera_clock(&root, 90.0, 90.0);
    let ids = [ClipId::new("C3"), ClipId::new("C3b")];
    for (pair, offset, count) in [(true, 0.0, 1), (false, 1.0, 1), (false, 3.0, 2)] {
        let mut data = AppData::default();
        data.apply_result(shoot.clone());
        let correction = CorrectionOption {
            id: "pair".into(),
            left: ids[1].clone(),
            right: ids[0].clone(),
            other_name: "C3".into(),
            offset,
        };
        if pair {
            data.reject_pair(&correction);
        } else {
            data.reject_alignment(&correction);
        }
        assert!(data.set_camera_clock(&ids, true));
        assert_eq!(data.camera_clock_for_export(0).len(), count);
        if count == 1 {
            assert!(data.camera_clock.refusal(&ids[1]).is_some());
        }
        data.set_camera_clock(&ids, false);
        assert!(data.set_camera_clock(&ids[1..], true));
        assert_eq!(data.set_camera_clock(&ids[..1], true), count == 2);
        // A project may already contain both choices from before the conflict.
        let mut session = serde_json::to_value(data.project_session()).unwrap();
        session["cameraClock"]["0"] = serde_json::json!(ids);
        data.restore_project(
            serde_json::from_value(session).unwrap(),
            vec![shoot.clone()],
            vec![],
            false,
        );
        assert_eq!(data.camera_clock_for_export(0).len(), count);
        assert!(data.camera_clock_applied(&ids[0]));
        assert_eq!(data.camera_clock_applied(&ids[1]), count == 2);
    }
}

/// `camera_clock_shoot` with a second camera recording the same times against
/// the same recorder, so C3 and C3b are unmatched and cannot both be placed.
/// C3 is `c3` seconds long and C3b `c3b`.
fn competing_camera_clock(root: &Path, c3: f64, c3b: f64) -> SyncResult {
    use align_core::{ClipId, MediaTime};
    let mut shoot = camera_clock_shoot(root);
    assert!(shoot.select_stage(0));
    shoot.stages.clear();
    shoot.selected_stage = None;
    for mut clip in shoot
        .project
        .clone()
        .clips
        .into_iter()
        .filter(|c| c.video.is_some())
    {
        clip.id.0.push('b');
        clip.url = root.join(format!("{}.MP4", clip.id.0));
        clip.source_identifier = Some("device:Sony:ILME-FX3:4712".into());
        shoot.project.clips.push(clip);
    }
    for mut p in shoot.islands[0]
        .placements
        .clone()
        .into_iter()
        .filter(|p| p.clip_id.0 != "R")
    {
        p.clip_id.0.push('b');
        shoot.islands[0].placements.push(p);
    }
    for mut m in shoot.matches.clone() {
        m.right.0.push('b');
        shoot.matches.push(m);
    }
    shoot.unmatched.push(ClipId::new("C3b"));
    for clip in &mut shoot.project.clips {
        match clip.id.0.as_str() {
            "C3" => clip.duration = MediaTime::seconds(c3),
            "C3b" => clip.duration = MediaTime::seconds(c3b),
            _ => {}
        }
    }
    shoot
}

#[test]
fn returning_a_clock_choice_does_not_place_the_clip_it_held_back() {
    use crate::lane::CorrectionOption;
    use align_core::ClipId;
    let (_temp, root) = fixture();
    let shoot = competing_camera_clock(&root, 90.0, 90.0);
    let ids = [ClipId::new("C3"), ClipId::new("C3b")];
    let mut data = AppData::default();
    data.apply_result(shoot);
    assert!(data.set_camera_clock(&ids, true));
    assert_eq!(data.camera_clock_for_export(0).len(), 2);
    // Rejecting the pair afterwards leaves only C3 placed; C3b is held back.
    data.reject_pair(&CorrectionOption {
        id: "pair".into(),
        left: ids[1].clone(),
        right: ids[0].clone(),
        other_name: "C3".into(),
        offset: 0.0,
    });
    assert!(data.camera_clock_applied(&ids[0]));
    assert!(!data.camera_clock_applied(&ids[1]));

    // Return All to Unmatched, as the panel sends it for the visible clips.
    let visible: Vec<ClipId> = data
        .camera_clock
        .estimates
        .iter()
        .map(|e| e.clip_id.clone())
        .collect();
    assert_eq!(visible, vec![ids[0].clone()]);
    assert!(data.set_camera_clock(&visible, false));
    assert!(data.camera_clock_for_export(0).is_empty());
    assert!(!data.camera_clock_applied(&ids[1]));
    // Both are offered again; the choice is the user's to make anew.
    assert_eq!(data.camera_clock.estimates.len(), 2);
}

#[test]
fn a_clock_choice_the_cleanup_removes_does_not_block_the_clip_it_conflicts_with() {
    use crate::lane::CorrectionOption;
    use align_core::ClipId;
    let (_temp, root) = fixture();
    // C3b is two seconds long, C3 twenty.
    let shoot = competing_camera_clock(&root, 20.0, 2.0);
    let ids = [ClipId::new("C3"), ClipId::new("C3b")];
    let mut data = AppData::default();
    data.apply_result(shoot);
    data.reject_pair(&CorrectionOption {
        id: "pair".into(),
        left: ids[1].clone(),
        right: ids[0].clone(),
        other_name: "C3".into(),
        offset: 0.0,
    });
    assert!(data.set_camera_clock(&ids[1..], true));
    assert!(data.camera_clock_applied(&ids[1]));
    assert!(data.camera_clock.estimate(&ids[0]).is_none());

    // Remove clips shorter than 5 s drops C3b; C3 is offered and places.
    let mut settings = data.export.clone();
    settings.cut_shorter_than = 5.0;
    data.apply_export_settings(&settings);
    assert!(!data.camera_clock_applied(&ids[1]));
    assert!(data.camera_clock.estimate(&ids[0]).is_some());
    assert!(data.set_camera_clock(&ids[..1], true));
    let exported: Vec<_> = data
        .camera_clock_for_export(0)
        .into_iter()
        .map(|e| e.clip_id)
        .collect();
    assert_eq!(exported, vec![ids[0].clone()]);

    // Without the cleanup C3b is not restored over the placement chosen later.
    settings.cut_shorter_than = 0.0;
    data.apply_export_settings(&settings);
    assert!(data.camera_clock_applied(&ids[0]));
    assert!(!data.camera_clock_applied(&ids[1]));
}

/// A choice made at one stage outlives it, but while the stage gives the
/// clip no estimate the preview is the ordinary one, whatever the export
/// settings would do to the timeline.
#[test]
fn a_kept_clock_choice_does_not_change_the_preview_without_an_estimate() {
    let (_temp, root) = fixture();
    let c3 = align_core::ClipId::new("C3");
    let prepare = |chosen: bool| {
        let mut data = AppData::default();
        data.apply_result(file_dated_shoot(&root));
        if chosen {
            assert!(data.select_sync_stage(0));
            assert!(data.set_camera_clock(std::slice::from_ref(&c3), true));
        }
        let mut settings = data.export.clone();
        settings.cut_common_gaps = true;
        settings.trim_starts = 2.0;
        settings.unmatched = align_core::export_model::UnmatchedPlacement::ByOrderOnly;
        data.apply_export_settings(&settings);
        // C3 is synchronized at this stage: nothing is left to estimate.
        assert!(data.select_sync_stage(1));
        assert!(data.camera_clock.estimates.is_empty());
        data
    };
    assert_eq!(bar_layout(&prepare(true)), bar_layout(&prepare(false)));
}
