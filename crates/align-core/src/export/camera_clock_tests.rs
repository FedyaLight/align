//! Camera-clock estimates through timeline assembly and every writer.

use std::collections::BTreeMap;

use super::fcpxml::FcpxmlOptions;
use super::model::{
    CutRemoveOptions, ExportAssemblyOptions, ExportTimeline, TimelineExportFormat,
    UnmatchedPlacement,
};
use super::premiere::PremiereOptions;
use crate::camera_clock::tests::{T0, camera_clip, placement, recorder, shoot, sound};
use crate::camera_clock::{CAMERA_CLOCK_LABEL, camera_clock};
use crate::model::{ClipId, MatchEvidence, SyncIsland, SyncResult, file_name};

/// `shoot()` with a 1000 s recorder, so C3's unmatched place runs past its
/// group's end, and a second group recorded later by another camera and
/// recorder. With `crossing`, camera B also films during C3's gap, on the
/// video track camera A's clips use.
fn two_groups(crossing: bool) -> SyncResult {
    let mut result = shoot();
    let recorder_id = ClipId::new("R");
    for clip in result
        .project
        .clips
        .iter_mut()
        .filter(|c| c.id == recorder_id)
    {
        clip.duration = crate::MediaTime::seconds(1000.0);
    }
    let short = recorder("R", 1000.0);
    result.islands[0].placements[0] = placement(&short, 0.0);
    let camera_b = "device:Sony:ILCE-7M4:9001";
    let r2 = recorder("R2", 300.0);
    let b1 = camera_clip("B1", Some(camera_b), T0 + 2000, 120.0);
    result
        .matches
        .push(sound(&r2, &b1, MatchEvidence::Waveform));
    result.islands.push(SyncIsland {
        id: 1,
        placements: vec![placement(&r2, 0.0), placement(&b1, 50.0)],
    });
    result.project.clips.extend([r2, b1]);
    if crossing {
        let b0 = camera_clip("B0", Some(camera_b), T0 + 600, 60.0);
        let r = result.project.clips[0].clone();
        result.matches.push(sound(&r, &b0, MatchEvidence::Waveform));
        result.islands[0].placements.push(placement(&b0, 700.0));
        result.project.clips.push(b0);
    }
    result
}

/// The timeline the writers get: export combines it to plan prepared
/// audio, then again to write.
fn timeline(result: &SyncResult, options: ExportAssemblyOptions) -> ExportTimeline {
    ExportTimeline::from_result_with_options(result, options)
        .expect("timeline")
        .combined()
        .combined()
}

/// `shoot()` with the recorder dated by its file, far from the camera: the
/// group has no consistent clock, so C3's unmatched place opens the timeline.
fn slot_first() -> SyncResult {
    let mut result = shoot();
    let recorder = &mut result.project.clips[0];
    recorder.recorded_at = Some(T0 + 86_400 * 30);
    recorder.recorded_at_source = Some(crate::model::RecordingTimestampSource::FileSystem);
    result
}

fn applied(result: &SyncResult) -> ExportAssemblyOptions {
    ExportAssemblyOptions {
        camera_clock: camera_clock(result, &[]).estimates,
        ..Default::default()
    }
}

type Placements = BTreeMap<(String, String), Vec<(usize, i64)>>;

/// Every clip of the written timeline: (file, media) → (track, start in µs),
/// read back the way an editor's import sees them.
fn written(timeline: &ExportTimeline, format: TimelineExportFormat) -> Placements {
    let mut out = Placements::new();
    let mut add = |file: String, media: &str, track: usize, start: f64| {
        out.entry((file, media.to_string()))
            .or_default()
            .push((track, (start * 1e6).round() as i64));
    };
    if format == TimelineExportFormat::ResolveOTIO {
        let otio: serde_json::Value =
            serde_json::from_slice(&super::otio::data(timeline, false).unwrap()).unwrap();
        let mut numbers: BTreeMap<String, usize> = BTreeMap::new();
        for track in otio["tracks"]["children"].as_array().unwrap() {
            // Editors number video and audio tracks separately.
            let kind = track["kind"].as_str().unwrap().to_string();
            let number = numbers.entry(kind.clone()).or_default();
            let index = *number;
            *number += 1;
            let mut at = 0.0;
            for child in track["children"].as_array().unwrap() {
                let range = &child["source_range"]["duration"];
                let duration = range["value"].as_f64().unwrap() / range["rate"].as_f64().unwrap();
                if child["OTIO_SCHEMA"] == "Clip.2" {
                    let file = child["media_references"]["DEFAULT_MEDIA"]["name"]
                        .as_str()
                        .unwrap()
                        .to_string();
                    add(file, &kind, index, at);
                }
                at += duration;
            }
        }
        return out;
    }
    let (text, extension) = match format {
        TimelineExportFormat::PremiereXML => (
            super::premiere::write(timeline, format, PremiereOptions::default()),
            "xml",
        ),
        _ => (
            super::fcpxml::write(timeline, FcpxmlOptions::TIMELINE),
            "fcpxml",
        ),
    };
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join(format!("timeline.{extension}"));
    std::fs::write(&path, text).unwrap();
    let draft = crate::xml::read_timeline(&path, None).expect("written timeline reads back");
    for edit in draft.edits {
        let media = format!("{:?}", edit.media_type);
        add(
            file_name(&edit.url),
            &media,
            edit.track_index,
            edit.timeline_start,
        );
    }
    out
}

const FORMATS: [TimelineExportFormat; 3] = [
    TimelineExportFormat::PremiereXML,
    TimelineExportFormat::FinalCutProXML,
    TimelineExportFormat::ResolveOTIO,
];

#[test]
fn an_applied_estimate_moves_only_its_clip() {
    for (scenario, crossing) in [
        (two_groups(false), false),
        (two_groups(true), true),
        (slot_first(), false),
    ] {
        let result = scenario;
        let ordinary = timeline(&result, ExportAssemblyOptions::default());
        let estimated = timeline(&result, applied(&result));
        let start_of = |timeline: &ExportTimeline, name: &str| {
            timeline.islands[0]
                .clips
                .iter()
                .find(|item| item.clip.id.0 == name)
                .map(|item| item.start)
                .unwrap()
        };
        // C3 leaves an empty place: after its group, running past the
        // group's end, or opening the timeline. The rest stays put.
        let shift = start_of(&ordinary, "C1") - 100.0;
        assert!((start_of(&estimated, "C3") - 699.76 - shift).abs() < 1e-6);
        assert_eq!(start_of(&ordinary, "C1"), start_of(&estimated, "C1"));
        for format in FORMATS {
            let mut before = written(&ordinary, format);
            let mut after = written(&estimated, format);
            let c3: Vec<_> = after
                .keys()
                .filter(|(f, _)| f == "C3.MP4")
                .cloned()
                .collect();
            assert!(!c3.is_empty(), "{format:?}");
            let moved: Vec<_> = c3.iter().map(|key| after.remove(key).unwrap()).collect();
            before.retain(|(file, _), _| file != "C3.MP4");
            assert_eq!(before, after, "{format:?}, crossing: {crossing}");
            // Free on camera A's track, C3 joins it; with camera B there,
            // it takes a new track rather than moving B.
            let video = |placements: &Placements| -> Vec<usize> {
                placements
                    .iter()
                    .filter(|((_, media), _)| media == "Video")
                    .flat_map(|(_, p)| p.iter().map(|(track, _)| *track))
                    .collect()
            };
            let c1_track = after[&("C1.MP4".to_string(), "Video".to_string())][0].0;
            let c3_track = moved[c3.iter().position(|(_, m)| m == "Video").unwrap()][0].0;
            if crossing {
                let last = video(&after).into_iter().max().unwrap();
                assert!(c3_track > last, "{format:?}: C3 on {c3_track}");
            } else {
                assert_eq!(c3_track, c1_track, "{format:?}");
            }
        }
    }
}

#[test]
fn unmatched_options_apply_to_estimated_clips() {
    let result = two_groups(false);
    let c3 = |options: ExportAssemblyOptions| {
        timeline(&result, options).islands[0]
            .clips
            .iter()
            .find(|item| item.clip.id.0 == "C3")
            .cloned()
    };
    let removed = c3(ExportAssemblyOptions {
        unmatched: UnmatchedPlacement::Remove,
        ..applied(&result)
    });
    assert!(removed.is_none(), "Remove unmatched removes estimates too");

    let labelled = c3(ExportAssemblyOptions {
        label_unmatched: true,
        disable_unmatched: true,
        unmatched_color: Some("Mango".into()),
        ..applied(&result)
    })
    .unwrap();
    assert_eq!(
        labelled.display_name.as_deref(),
        Some(format!("{CAMERA_CLOCK_LABEL} [UNSYNCED] C3.MP4").as_str())
    );
    assert!(!labelled.flags.enabled);
    assert!(labelled.fcp7.fcp7_labels_xml.unwrap().contains("Mango"));

    let order_only = c3(ExportAssemblyOptions {
        unmatched: UnmatchedPlacement::ByOrderOnly,
        ..applied(&result)
    })
    .unwrap();
    assert!((order_only.start - 699.76).abs() < 1e-6);
}

/// Remove common gaps keeps the time an estimated clip now covers, so the
/// clips after it stay later by that much.
#[test]
fn common_gap_removal_keeps_the_time_an_estimate_covers() {
    let mut result = shoot();
    // The recorder stops between C2 and C4; a second one records C4.
    let r1 = recorder("R", 500.0);
    let r2 = recorder("R2", 150.0);
    let c4 = result.project.clips[3].clone();
    result.islands[0].placements[0] = placement(&r1, 0.0);
    result.islands[0].placements.push(placement(&r2, 850.0));
    result.project.clips[0] = r1;
    result.matches.retain(|m| m.right != c4.id);
    result
        .matches
        .push(sound(&r2, &c4, MatchEvidence::Waveform));
    result.project.clips.push(r2);
    let cut = CutRemoveOptions {
        common_gaps: true,
        ..Default::default()
    };
    let c4_start = |options: ExportAssemblyOptions| {
        timeline(&result, options).islands[0]
            .clips
            .iter()
            .find(|item| item.clip.id == c4.id)
            .unwrap()
            .start
    };
    let ordinary = c4_start(ExportAssemblyOptions {
        cut_remove: cut,
        ..Default::default()
    });
    let estimated = c4_start(ExportAssemblyOptions {
        cut_remove: cut,
        ..applied(&result)
    });
    assert!((estimated - ordinary - 90.0).abs() < 1e-6);
}

#[test]
fn the_replaced_sequence_keeps_an_estimated_clips_own_sound() {
    let result = shoot();
    let estimated = timeline(&result, applied(&result));
    let xml = super::premiere::write(
        &estimated,
        TimelineExportFormat::PremiereXML,
        PremiereOptions {
            replaced_sequence: true,
            ..Default::default()
        },
    );
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("timeline.xml");
    std::fs::write(&path, xml).unwrap();
    let index = crate::xml::timeline_sequence_summaries(&path)
        .unwrap()
        .into_iter()
        .find(|summary| summary.name.ends_with("– replaced"))
        .expect("replaced sequence")
        .index;
    let replaced = crate::xml::read_timeline(&path, Some(index)).unwrap();
    let sound: Vec<(String, f64, f64)> = replaced
        .edits
        .iter()
        .filter(|edit| edit.media_type == crate::xml::DraftMediaKind::Audio)
        .map(|edit| (file_name(&edit.url), edit.timeline_start, edit.timeline_end))
        .collect();
    let c3 = (699.76, 789.76);
    assert!(sound.iter().any(|(file, ..)| file == "C3.MP4"), "{sound:?}");
    assert!(
        sound
            .iter()
            .filter(|(file, ..)| file == "R.WAV")
            .all(|(_, start, end)| *end <= c3.0 + 0.05 || *start >= c3.1 - 0.05),
        "recorder audio replaced the estimated clip's: {sound:?}"
    );
    assert!(sound.iter().any(|(file, ..)| file == "R.WAV"));
}
