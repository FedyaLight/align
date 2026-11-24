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
    // Where C4 plays after the group's first clip: the place C3 left is
    // closed with the common gaps, so only the group's own time counts.
    let c4_start = |options: ExportAssemblyOptions| {
        let timeline = timeline(&result, options);
        let start = |id: &ClipId| {
            timeline.islands[0]
                .clips
                .iter()
                .find(|item| &item.clip.id == id)
                .unwrap()
                .start
        };
        start(&c4.id) - start(&ClipId::new("C1"))
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

/// Camera B's B0 ends at 55.050 s and the estimate puts C3 at 55.055 s, in
/// the same folder: apart in seconds, but on the 25 fps grid B0 runs to
/// frame 1377 and C3 starts on frame 1376.
fn rounding_seam() -> SyncResult {
    let clip = |name: &str, at: i64, seconds: f64| {
        camera_clip(
            name,
            Some(crate::camera_clock::tests::CAMERA),
            T0 + at,
            seconds,
        )
    };
    let b0 = camera_clip("B0", Some("device:Sony:ILCE-7M4:9001"), T0 + 45, 10.025);
    let r = recorder("R", 200.0);
    let mut result = crate::camera_clock::tests::result(
        vec![vec![
            (r, 0.0),
            (clip("C1", 10, 10.0), 10.0),
            (clip("C2", 30, 10.0), 30.055),
            (b0, 45.025),
            (clip("C4", 80, 10.0), 80.055),
        ]],
        vec![clip("C3", 55, 12.0)],
        Default::default(),
    );
    result.matches.retain(|m| m.right.0 != "B0");
    result
}

#[test]
fn the_written_grid_decides_whether_an_estimate_fits_its_angle_or_lane() {
    let result = rounding_seam();
    let estimates = camera_clock(&result, &[]).estimates;
    assert!((estimates[0].start - 55.055).abs() < 1e-6, "{estimates:?}");
    let estimated = timeline(&result, applied(&result));

    // The multicam: no angle plays two clips over each other.
    let xml = super::fcpxml::write(&estimated, FcpxmlOptions::default());
    let seconds = |text: &str| {
        let text = text.trim_end_matches('s');
        text.split_once('/').map_or_else(
            || text.parse::<f64>().unwrap(),
            |(n, d)| n.parse::<f64>().unwrap() / d.parse::<f64>().unwrap(),
        )
    };
    let attribute = |line: &str, name: &str| {
        let after = line.split_once(&format!(" {name}=\"")).unwrap().1;
        seconds(after.split('"').next().unwrap())
    };
    let mut angles: Vec<Vec<(String, f64, f64)>> = Vec::new();
    let mut inside = false;
    for line in xml.lines().map(str::trim) {
        if line.starts_with("<mc-angle") {
            angles.push(Vec::new());
            inside = true;
        } else if line.starts_with("</mc-angle") {
            inside = false;
        } else if line.starts_with("<asset-clip") && inside {
            let start = attribute(line, "offset");
            let name = line
                .split("name=\"")
                .nth(1)
                .unwrap()
                .split('"')
                .next()
                .unwrap();
            angles.last_mut().unwrap().push((
                name.to_string(),
                start,
                start + attribute(line, "duration"),
            ));
        }
    }
    assert!(
        angles
            .iter()
            .flatten()
            .any(|(name, ..)| name.contains("C3"))
    );
    for angle in &angles {
        for (i, a) in angle.iter().enumerate() {
            for b in &angle[i + 1..] {
                assert!(
                    a.2 <= b.1 + 1e-6 || b.2 <= a.1 + 1e-6,
                    "overlap in an angle: {a:?} {b:?}"
                );
            }
        }
    }

    // The synchronized timeline: no track plays two clips over each other.
    for format in [
        TimelineExportFormat::FinalCutProXML,
        TimelineExportFormat::PremiereXML,
    ] {
        let (text, extension) = if format == TimelineExportFormat::PremiereXML {
            (
                super::premiere::write(&estimated, format, PremiereOptions::default()),
                "xml",
            )
        } else {
            (
                super::fcpxml::write(&estimated, FcpxmlOptions::TIMELINE),
                "fcpxml",
            )
        };
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(format!("timeline.{extension}"));
        std::fs::write(&path, text).unwrap();
        let draft = crate::xml::read_timeline(&path, None).unwrap();
        let mut tracks: BTreeMap<(String, usize), Vec<(f64, f64)>> = BTreeMap::new();
        for edit in &draft.edits {
            tracks
                .entry((format!("{:?}", edit.media_type), edit.track_index))
                .or_default()
                .push((edit.timeline_start, edit.timeline_end));
        }
        for (track, spans) in &mut tracks {
            spans.sort_by(|a, b| a.0.total_cmp(&b.0));
            for pair in spans.windows(2) {
                assert!(
                    pair[0].1 <= pair[1].0 + 1e-6,
                    "{format:?} {track:?}: {pair:?}"
                );
            }
        }
    }
}

/// `slot_first()` with C3 shortened to ten seconds and groups of another
/// camera, each a recorder and one clip of it at `(recorded, start in group,
/// recorder seconds)`.
fn short_slot(others: &[(i64, f64, f64)]) -> SyncResult {
    let mut result = slot_first();
    for clip in result.project.clips.iter_mut().filter(|c| c.id.0 == "C3") {
        clip.duration = crate::MediaTime::seconds(10.0);
    }
    for (index, (recorded, start, length)) in others.iter().enumerate() {
        let id = index + 1;
        let r = recorder(&format!("R{id}"), *length);
        let b = camera_clip(
            &format!("B{id}"),
            Some("device:Sony:ILCE-7M4:9001"),
            T0 + recorded,
            120.0f64.min(length - start),
        );
        result.matches.push(sound(&r, &b, MatchEvidence::Waveform));
        result.islands.push(SyncIsland {
            id,
            placements: vec![placement(&r, 0.0), placement(&b, *start)],
        });
        result.project.clips.extend([r, b]);
    }
    result
}

/// With Remove common gaps the place an estimated clip left is empty time
/// and closes, at the start or between groups; where other material plays
/// at the same time it stays. Without Cut & Remove the place is kept.
#[test]
fn common_gap_removal_closes_the_place_an_estimate_left() {
    let cut = ExportAssemblyOptions {
        cut_remove: CutRemoveOptions {
            common_gaps: true,
            ..Default::default()
        },
        ..Default::default()
    };
    let starts = |result: &SyncResult, options: ExportAssemblyOptions| -> BTreeMap<String, f64> {
        timeline(result, options).islands[0]
            .clips
            .iter()
            .map(|item| (item.clip.id.0.clone(), item.start))
            .collect()
    };
    // Whether the place closes: another camera's group recorded before and
    // after C3, or one recording the whole time C3 would play.
    for (name, result, closes) in [
        ("first", short_slot(&[]), true),
        (
            "between",
            short_slot(&[(100, 20.0, 300.0), (2000, 50.0, 300.0)]),
            true,
        ),
        ("alongside", short_slot(&[(595, 5.0, 300.0)]), false),
    ] {
        let estimates = camera_clock(&result, &[]).estimates;
        assert_eq!(estimates.len(), 1, "{name}");
        let applied = |options: &ExportAssemblyOptions| ExportAssemblyOptions {
            camera_clock: estimates.clone(),
            ..options.clone()
        };
        let ordinary = starts(&result, cut.clone());
        let estimated = starts(&result, applied(&cut));
        let slot = ordinary["C3"];
        for (clip, start) in estimated.iter().filter(|(clip, _)| *clip != "C3") {
            // Whatever plays after the place moves up by it and its gap.
            let later = closes && ordinary[clip] >= slot + 10.0;
            let expected = ordinary[clip] - if later { 11.0 } else { 0.0 };
            assert!(
                (start - expected).abs() < 1e-6,
                "{name}: {clip} at {start}, expected {expected}"
            );
        }
        if name == "first" {
            // The sequence a user imports opens with the group, not an
            // empty ten seconds.
            let opening = written(
                &timeline(&result, applied(&cut)),
                TimelineExportFormat::PremiereXML,
            )
            .values()
            .flatten()
            .map(|(_, start)| *start)
            .min();
            assert_eq!(opening, Some(0));
        }
        // The place stays when nothing asks to close it.
        let ordinary = starts(&result, Default::default());
        let kept = starts(&result, applied(&Default::default()));
        for clip in kept.keys().filter(|c| *c != "C3") {
            assert_eq!(kept[clip], ordinary[clip], "{name}: {clip}");
        }
    }
}

/// The audio tracks of the replaced sequence as written: whether each is a
/// channel of a stereo track, and how many clips it holds.
fn replaced_audio_tracks(timeline: &ExportTimeline) -> Vec<(bool, usize)> {
    let xml = super::premiere::write(
        timeline,
        TimelineExportFormat::PremiereXML,
        PremiereOptions {
            replaced_sequence: true,
            ..Default::default()
        },
    );
    let audio = xml
        .split("id=\"sequence-2\"")
        .nth(1)
        .and_then(|sequence| sequence.split("<audio>").nth(1))
        .expect("replaced sequence");
    audio
        .split("\n            <track")
        .skip(1)
        .map(|track| {
            (
                track.starts_with(" premiereTrackType=\"Stereo\""),
                track.matches("<clipitem ").count(),
            )
        })
        .collect()
}

/// Under C3's ordinary place a stereo recorder plays in the replaced
/// sequence. Once C3 plays its own sound, mono or none, the pair it left
/// is not two empty mono tracks.
#[test]
fn the_replaced_sequence_does_not_leave_mono_tracks_where_an_estimate_left_a_stereo_pair() {
    let result = short_slot(&[(595, 5.0, 300.0)]);
    let ordinary = replaced_audio_tracks(&timeline(&result, Default::default()));
    assert!(ordinary.iter().all(|&(stereo, _)| stereo), "{ordinary:?}");
    let own_sound = |channels: Option<usize>| {
        let mut result = result.clone();
        for clip in result.project.clips.iter_mut().filter(|c| c.id.0 == "C3") {
            match channels {
                Some(channels) => clip.audio[0].channels = channels,
                None => clip.audio.clear(),
            }
        }
        replaced_audio_tracks(&timeline(&result, applied(&result)))
    };

    // The other tracks keep their numbers: the vacated pair stays a pair,
    // and C3's mono sound is a track of its own after it.
    let mono = own_sound(Some(1));
    let mut expected = ordinary.clone();
    let vacated = expected.len() - 2;
    expected[vacated] = (true, 0);
    expected[vacated + 1] = (true, 0);
    expected.push((false, 1));
    assert_eq!(mono, expected);

    // Without sound the pair at the end only shifts nothing, and goes.
    assert_eq!(own_sound(None), ordinary[..vacated]);
}
