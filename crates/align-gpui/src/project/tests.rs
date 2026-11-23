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
