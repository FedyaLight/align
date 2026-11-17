use super::*;

/// FCPXML may locate media relative to the document's folder, with
/// percent-encoding; they must not resolve against the working directory.
#[test]
fn fcpxml_media_may_be_relative_to_the_document() {
    let dir = std::env::temp_dir().join(format!("align-relative-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("cut.fcpxml");
    std::fs::write(
        &path,
        FCPXML.replace("file:///tmp/c.mov", "Media/my%20clip.mov"),
    )
    .unwrap();
    let draft = read_timeline(&path, None).unwrap();
    assert_eq!(
        draft.media_urls(),
        vec![dir.join("Media").join("my clip.mov")]
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Editor exports start with a declaration, a DOCTYPE and sometimes
/// comments; all of them must stay timelines, or projects would be
/// skipped silently.
/// Parse failures surface while importing a file the user chose, so the
/// message must name it.
#[test]
fn unreadable_documents_are_named_in_import_errors() {
    let dir = tempfile::tempdir().unwrap();
    let broken = dir.path().join("broken.xml");
    std::fs::write(&broken, "<xmeml version=\"5\"><sequence></xmeml>").unwrap();
    let empty = dir.path().join("empty.fcpxml");
    std::fs::write(&empty, "<fcpxml version=\"1.10\"><library/></fcpxml>").unwrap();
    let missing = dir.path().join("missing.xml");
    for (path, name) in [
        (&broken, "broken.xml"),
        (&empty, "empty.fcpxml"),
        (&missing, "missing.xml"),
    ] {
        for error in [
            read_timeline(path, None).unwrap_err(),
            timeline_sequence_summaries(path).unwrap_err(),
        ] {
            assert!(error.to_string().contains(name), "{error}");
        }
    }
}

#[test]
fn timeline_documents_are_recognized_by_root_element() {
    let dir = std::env::temp_dir().join(format!("align-timeline-root-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let cases = [
        (
            "premiere.xml",
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE xmeml>\n<xmeml version=\"4\"><sequence/></xmeml>",
            true,
        ),
        (
            "resolve.xml",
            "<?xml version=\"1.0\"?>\n<!-- exported -->\n<XMEML version=\"5\"/>",
            true,
        ),
        (
            "fcp.xml",
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE fcpxml>\n<fcpxml version=\"1.10\"/>",
            true,
        ),
        (
            "A001C001_CANON.XML",
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<Clip><ClipName>A001C001</ClipName></Clip>",
            false,
        ),
        (
            "C0001M01.XML",
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<NonRealTimeMeta xmlns=\"urn:schemas-professionalDisc:nonRealTimeMeta:ver.2.00\"/>",
            false,
        ),
        ("broken.xml", "not xml at all", true),
    ];
    for (name, text, timeline) in cases {
        let path = dir.join(name);
        std::fs::write(&path, text).unwrap();
        assert_eq!(is_timeline_document(&path), timeline, "{name}");
    }
    // UTF-16 with a byte order mark is decoded to find the root; without
    // one, NUL bytes keep the file a timeline.
    let utf16 = |text: &str, bom: [u8; 2], unit: fn(u16) -> [u8; 2]| {
        let mut bytes = bom.to_vec();
        bytes.extend(text.encode_utf16().flat_map(unit));
        bytes
    };
    let project = "<?xml version=\"1.0\" encoding=\"UTF-16\"?>\n<xmeml version=\"5\"/>";
    let camera = "<?xml version=\"1.0\" encoding=\"UTF-16\"?>\n<Clip/>";
    let encoded = [
        (
            "le.xml",
            utf16(project, [0xFF, 0xFE], u16::to_le_bytes),
            true,
        ),
        (
            "be.xml",
            utf16(project, [0xFE, 0xFF], u16::to_be_bytes),
            true,
        ),
        (
            "camera-le.xml",
            utf16(camera, [0xFF, 0xFE], u16::to_le_bytes),
            false,
        ),
        (
            "no-bom.xml",
            project.encode_utf16().flat_map(u16::to_le_bytes).collect(),
            true,
        ),
    ];
    for (name, bytes, timeline) in encoded {
        let path = dir.join(name);
        std::fs::write(&path, bytes).unwrap();
        assert_eq!(is_timeline_document(&path), timeline, "{name}");
    }
    assert!(is_timeline_document(&dir.join("missing.xml")));
    let _ = std::fs::remove_dir_all(&dir);
}

const FCP7: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<xmeml version="4">
<sequence>
<name>Seq</name>
<rate><timebase>25</timebase><ntsc>FALSE</ntsc></rate>
<media>
<video>
<track>
<enabled>TRUE</enabled><locked>FALSE</locked>
<clipitem id="a1">
<name>Shot A</name>
<enabled>TRUE</enabled>
<in>0</in><out>104</out><start>0</start><end>-1</end>
<labels><label2>Mango</label2></labels>
<file id="fa"><name>A</name><pathurl>file:///tmp/a.mov</pathurl><rate><timebase>25</timebase><ntsc>FALSE</ntsc></rate><duration>200</duration></file>
<link><linkclipref>aa1</linkclipref></link>
</clipitem>
<transitionitem>
<start>96</start><end>104</end><alignment>center</alignment>
<effect>
<name>Cross Dissolve</name><effectid>Cross Dissolve</effectid>
<effectcategory>Dissolve</effectcategory>
<effecttype>transition</effecttype><mediatype>video</mediatype>
<startratio>0</startratio><endratio>1</endratio><reverse>FALSE</reverse>
</effect>
</transitionitem>
<clipitem id="b1">
<name>Shot B</name>
<enabled>TRUE</enabled>
<in>46</in><out>150</out><start>-1</start><end>200</end>
<file id="fb"><name>B</name><pathurl>file:///tmp/b.mov</pathurl><rate><timebase>25</timebase><ntsc>FALSE</ntsc></rate><duration>200</duration></file>
<filter>
<effect>
<name>Time Remap</name><effectid>timeremap</effectid>
<parameter authoringApp="PremierePro"><parameterid>graphdict</parameterid>
<keyframe><when>0</when><value>0</value></keyframe>
<keyframe><when>150</when><value>150</value></keyframe>
</parameter>
</effect>
</filter>
</clipitem>
</track>
</video>
<audio>
<track>
<enabled>TRUE</enabled><locked>FALSE</locked>
<clipitem id="aa1">
<name>Shot A Audio</name>
<enabled>TRUE</enabled>
<in>0</in><out>100</out><start>0</start><end>100</end>
<file id="fa2"><name>A</name><pathurl>file:///tmp/a.mov</pathurl><rate><timebase>25</timebase><ntsc>FALSE</ntsc></rate><duration>200</duration></file>
<link><linkclipref>a1</linkclipref></link>
</clipitem>
</track>
</audio>
</media>
</sequence>
</xmeml>"#;

const FCPXML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<fcpxml version="1.10">
<resources>
<format id="r1" frameDuration="1/25s"/>
<asset id="a1" hasVideo="1" hasAudio="1"><media-rep src="file:///tmp/c.mov"/></asset>
</resources>
<library><event><project name="Proj">
<sequence format="r1" name="Cut">
<spine>
<asset-clip ref="a1" name="Clip C" offset="0s" duration="100/25s" start="0s" lane="1" audioRole="dialogue.interview"/>
</spine>
</sequence>
</project></event></library>
</fcpxml>"#;

#[test]
fn fcp7_sequence_summary_and_edits() {
    let dir = std::env::temp_dir().join(format!("align-xml-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let xml = dir.join("edit.xml");
    std::fs::write(&xml, FCP7).unwrap();

    let summaries = timeline_sequence_summaries(&xml).expect("summaries");
    assert_eq!(summaries.len(), 1);
    assert_eq!(summaries[0].name, "Seq");
    assert_eq!(summaries[0].clip_count, 3);

    let draft = read_timeline(&xml, None).expect("read");
    assert_eq!(draft.name, "Seq");
    assert_eq!(draft.frame_duration, MediaTime::new(1, 25));
    // 2 video + 1 audio edits.
    assert_eq!(draft.edits.len(), 3);
    let a = draft.edits.iter().find(|e| e.id == "a1").expect("a1");
    assert_eq!(a.url, PathBuf::from("/tmp/a.mov"));
    assert!((a.source_in - 0.0).abs() < 1e-9);
    assert!((a.source_out - 4.16).abs() < 1e-9);
    assert!((a.timeline_start - 0.0).abs() < 1e-9);
    assert_eq!(
        a.fcp7_labels_xml.as_deref(),
        Some("<labels><label2>Mango</label2></labels>")
    );
    // Retimed clip B keeps its graph: 100% forward.
    let b = draft.edits.iter().find(|e| e.id == "b1").expect("b1");
    assert!((b.playback_rate - 1.0).abs() < 1e-9);
    assert!(!b.plays_backward);
    assert!(
        b.fcp7_time_remap_xml
            .as_ref()
            .unwrap()
            .contains("timeremap")
    );
    assert_eq!(b.fcp7_retime_in, Some(46));
    // Portable cross dissolve detected.
    assert_eq!(draft.transitions.len(), 1);
    assert!(draft.transitions[0].is_otio_portable);
    assert_eq!(
        draft.transitions[0].kind,
        TimelineTransitionKind::CrossDissolve
    );
    assert!(draft.transitions[0].effect_xml.contains("Cross Dissolve"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn fcp7_multi_sequence_requires_choice() {
    let doc = XmlDoc::parse(
        "<xmeml><sequence><name>A</name></sequence><sequence><name>B</name></sequence></xmeml>",
    )
    .unwrap();
    assert_eq!(top_sequences(&doc).len(), 2);
    let dir = std::env::temp_dir().join(format!("align-xml2-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let xml = dir.join("multi.xml");
    std::fs::write(
        &xml,
        "<xmeml><sequence><name>A</name></sequence><sequence><name>B</name></sequence></xmeml>",
    )
    .unwrap();
    assert!(matches!(
        read_timeline(&xml, None),
        Err(ImportError::MultipleSequences(_, 2))
    ));
    assert!(matches!(
        read_timeline(&xml, Some(5)),
        Err(ImportError::InvalidSequence(_, 5))
    ));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn fcpxml_reads_assets_and_links() {
    let dir = std::env::temp_dir().join(format!("align-xml3-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let xml = dir.join("edit.fcpxml");
    std::fs::write(&xml, FCPXML).unwrap();
    let summaries = timeline_sequence_summaries(&xml).expect("summaries");
    assert_eq!(summaries.len(), 1);
    assert_eq!(summaries[0].name, "Proj");
    assert_eq!(summaries[0].clip_count, 1);
    let draft = read_timeline(&xml, None).expect("read");
    assert_eq!(draft.frame_duration, MediaTime::new(1, 25));
    assert_eq!(draft.edits.len(), 2);
    let video = draft
        .edits
        .iter()
        .find(|e| e.media_type == DraftMediaKind::Video)
        .unwrap();
    let audio = draft
        .edits
        .iter()
        .find(|e| e.media_type == DraftMediaKind::Audio)
        .unwrap();
    assert_eq!(video.url, PathBuf::from("/tmp/c.mov"));
    assert!(video.linked_edit_ids.contains(&audio.id));
    assert!(audio.linked_edit_ids.contains(&video.id));
    assert_eq!(
        audio.fcpxml_audio_role.as_deref(),
        Some("dialogue.interview")
    );
    assert!((video.timeline_end - 4.0).abs() < 1e-9);
    assert_eq!(video.track_index, 1);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn local_file_paths_decode() {
    assert_eq!(
        local_file_path("file:///tmp/my%20clip.mov"),
        Some(PathBuf::from("/tmp/my clip.mov"))
    );
    assert_eq!(local_file_path("https://x/y.mov"), None);
    assert_eq!(local_file_path("/tmp/plain.mov"), None);
    assert_eq!(percent_decode("a%20b%2Fc"), "a b/c");
    #[cfg(windows)]
    assert_eq!(
        local_file_path("file:///C:/Media/My%20Clip.wav"),
        Some(PathBuf::from("C:/Media/My Clip.wav"))
    );
}

#[test]
fn rewrites_only_matching_fcp7_and_fcpxml_media_paths() {
    let fcp7 = br#"<?xml version="1.0"?><xmeml><sequence><media><video><track>
<clipitem><file><pathurl>file:///old/A%20Clip.mov</pathurl><name>Keep &amp; Me</name></file></clipitem>
<clipitem><file><pathurl><![CDATA[file:///old/rec.wav]]></pathurl></file></clipitem>
</track></video></media></sequence></xmeml>"#;
    let replacements = vec![
        (
            PathBuf::from("/old/A Clip.mov"),
            PathBuf::from("/new/A Clip.mov"),
        ),
        (PathBuf::from("/old/rec.wav"), PathBuf::from("/new/rec.wav")),
    ];
    let fixed = String::from_utf8(
        rewrite_media_paths(fcp7, &replacements, &Relocation::none(Path::new("/old"))).unwrap(),
    )
    .unwrap();
    assert!(fixed.contains("<pathurl>file:///new/A%20Clip.mov</pathurl>"));
    assert!(fixed.contains("<![CDATA[file:///new/rec.wav]]>"));
    assert!(fixed.contains("<name>Keep &amp; Me</name>"));

    let fcpxml = br#"<fcpxml><resources>
<asset id="a" src="file:///old/A%20Clip.mov" name="Keep &amp; Me"/>
<asset id="b"><media-rep kind="original-media" src="file:///old/rec.wav"/></asset>
<asset id="c" src="file:///unchanged.mov"/>
</resources></fcpxml>"#;
    let fixed = String::from_utf8(
        rewrite_media_paths(fcpxml, &replacements, &Relocation::none(Path::new("/old"))).unwrap(),
    )
    .unwrap();
    assert!(fixed.contains("src=\"file:///new/A%20Clip.mov\""));
    assert!(fixed.contains("src=\"file:///new/rec.wav\""));
    assert!(fixed.contains("<asset id=\"c\" src=\"file:///unchanged.mov\"/>"));
    assert!(fixed.contains("name=\"Keep &amp; Me\""));
}

#[test]
fn resolve_links_and_dedupes() {
    use crate::model::{AudioSummary, VideoSummary};
    let dir = std::env::temp_dir().join(format!("align-xml4-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let xml = dir.join("edit.xml");
    std::fs::write(&xml, FCP7).unwrap();
    let draft = read_timeline(&xml, None).expect("read");
    let mk_clip = |id: &str, url: &str, kind: MediaKind| Clip {
        id: crate::model::ClipId::new(id),
        url: PathBuf::from(url),
        kind,
        duration: MediaTime::seconds(10.0),
        audio: vec![AudioSummary {
            sample_rate: 48000.0,
            channels: 2,
            bit_depth: None,
            is_float: None,
            source_timecode: None,
        }],
        video: if kind == MediaKind::Video {
            Some(VideoSummary {
                width: 1920,
                height: 1080,
                frame_duration: Some(MediaTime::new(1, 25)),
                source_timecode: None,
                frame_rate_mode: None,
            })
        } else {
            None
        },
        recorded_at: None,
        recorded_at_source: None,
        source_identifier: None,
        media_span: None,
    };
    // /tmp/a.mov is video+audio, /tmp/b.mov video-only here.
    let clips = vec![
        mk_clip("ca", "/tmp/a.mov", MediaKind::Video),
        mk_clip("cb", "/tmp/b.mov", MediaKind::Video),
    ];
    let timeline = draft.resolve(&clips);
    assert_eq!(timeline.edits.len(), 2);
    let va = timeline.edits.iter().find(|e| e.id == "a1").expect("a1");
    assert_eq!(va.clip_id.0, "ca");
    // Embedded audio linked via mutual linkclipref.
    let linked = va.linked_audio_edit.as_ref().expect("linked");
    assert_eq!(linked.id, "aa1");
    assert_eq!(va.audio_track_index, Some(1));
    // b.mov has no inspected audio: video edit resolves without link.
    let vb = timeline.edits.iter().find(|e| e.id == "b1").expect("b1");
    assert!(vb.linked_audio_edit.is_none());
    // Unresolved media warns once per path.
    let warnings = draft.unresolved_warnings(&clips, &[]);
    assert!(warnings.is_empty());
    let warnings = draft.unresolved_warnings(&clips[..1], &[]);
    assert_eq!(warnings.len(), 1);
    assert!(warnings[0].message.contains("/tmp/b.mov"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn relink_unique_filename() {
    let dir = std::env::temp_dir().join(format!("align-xml5-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let xml = dir.join("edit.xml");
    std::fs::write(&xml, FCP7).unwrap();
    let draft = read_timeline(&xml, None).expect("read");
    // /tmp/*.mov do not exist; provide same-named candidates.
    let media = dir.join("media");
    std::fs::create_dir_all(&media).unwrap();
    let cand = media.join("a.mov");
    std::fs::write(&cand, b"fake").unwrap();
    let (relinked, replacements) =
        draft.relinking_missing_media_with_replacements(std::slice::from_ref(&cand), &[], &[]);
    assert!(relinked.edits.iter().any(|e| e.url == cand));
    assert!(replacements.contains(&(PathBuf::from("/tmp/a.mov"), cand.clone())));
    assert!(
        relinked
            .warnings
            .iter()
            .any(|w| w.message.contains("Relinked a.mov"))
    );
    // Ambiguous candidates warn instead of guessing.
    let other = dir.join("other");
    std::fs::create_dir_all(&other).unwrap();
    let cand2 = other.join("a.mov");
    std::fs::write(&cand2, b"fake").unwrap();
    let relinked = draft.relinking_missing_media(&[cand, cand2], &[], &[]);
    assert!(
        relinked
            .warnings
            .iter()
            .any(|w| w.message.contains("Could not relink a.mov: 2"))
    );

    // An unchanged card/day suffix resolves duplicate filenames after
    // the project root moves; a shorter common suffix must not win.
    let mut deep = draft.clone();
    for edit in &mut deep.edits {
        if edit.url.file_name().and_then(|name| name.to_str()) == Some("a.mov") {
            edit.url = PathBuf::from("/offline/day-1/card-a/a.mov");
        }
    }
    let exact_dir = dir.join("new-root/day-1/card-a");
    let weaker_dir = dir.join("new-root/day-2/card-a");
    std::fs::create_dir_all(&exact_dir).unwrap();
    std::fs::create_dir_all(&weaker_dir).unwrap();
    let exact = exact_dir.join("a.mov");
    let weaker = weaker_dir.join("a.mov");
    std::fs::write(&exact, b"fake").unwrap();
    std::fs::write(&weaker, b"fake").unwrap();
    let relinked = deep.relinking_missing_media(&[weaker, exact.clone()], &[], &[]);
    assert!(relinked.edits.iter().any(|edit| edit.url == exact));
    assert!(
        relinked
            .warnings
            .iter()
            .any(|warning| warning.message.contains("matching 2 parent folders"))
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn relink_saved_redirection_without_pool() {
    use crate::redirect::PathRedirection;

    let dir = std::env::temp_dir().join(format!("align-xml-redir-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let xml = dir.join("edit.xml");
    std::fs::write(&xml, FCP7).unwrap();
    let draft = read_timeline(&xml, None).expect("read");
    let media = dir.join("card");
    std::fs::create_dir_all(&media).unwrap();
    std::fs::write(media.join("a.mov"), b"fake").unwrap();
    std::fs::write(media.join("b.mov"), b"fake").unwrap();
    let redirects = vec![PathRedirection::new("/tmp", media.clone())];
    let relinked = draft.relinking_missing_media(&[], &redirects, &[]);
    assert!(relinked.edits.iter().any(|e| e.url == media.join("a.mov")));
    assert!(relinked.edits.iter().any(|e| e.url == media.join("b.mov")));
    assert!(
        relinked
            .warnings
            .iter()
            .any(|w| w.message.contains("via saved redirection"))
    );
    // A rewrite pointing at nothing falls back to silence, not a guess.
    let dead = vec![PathRedirection::new("/tmp", dir.join("empty"))];
    let relinked = draft.relinking_missing_media(&[], &dead, &[]);
    assert!(
        relinked
            .warnings
            .iter()
            .all(|w| !w.message.contains("via saved redirection"))
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn relink_manual_pick_breaks_ties() {
    let dir = std::env::temp_dir().join(format!("align-xml-manual-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let xml = dir.join("edit.xml");
    std::fs::write(&xml, FCP7).unwrap();
    let draft = read_timeline(&xml, None).expect("read");
    let dup1 = dir.join("dup1");
    let dup2 = dir.join("dup2");
    std::fs::create_dir_all(&dup1).unwrap();
    std::fs::create_dir_all(&dup2).unwrap();
    let pick = dup1.join("a.mov");
    std::fs::write(&pick, b"fake").unwrap();
    std::fs::write(dup2.join("a.mov"), b"fake").unwrap();
    let manual = vec![("a.mov".to_string(), pick.clone())];
    let relinked = draft.relinking_missing_media(&[dup2.join("a.mov"), pick.clone()], &[], &manual);
    assert!(relinked.edits.iter().any(|e| e.url == pick));
    assert!(
        relinked
            .warnings
            .iter()
            .any(|w| w.message.contains("(manual)"))
    );
    // A manual target that does not exist warns instead of relinking.
    let missing = vec![("b.mov".to_string(), dir.join("ghost.mov"))];
    let relinked = draft.relinking_missing_media(&[], &[], &missing);
    assert!(
        relinked
            .warnings
            .iter()
            .any(|w| w.message.contains("does not exist"))
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn unresolved_warnings_skip_omitted_extensions() {
    let dir = std::env::temp_dir().join(format!("align-xml-omit-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let xml = dir.join("edit.xml");
    std::fs::write(&xml, FCP7).unwrap();
    let draft = read_timeline(&xml, None).expect("read");
    assert!(!draft.unresolved_warnings(&[], &[]).is_empty());
    assert!(
        draft
            .unresolved_warnings(&[], &["mov".to_string()])
            .is_empty()
    );
    let _ = std::fs::remove_dir_all(&dir);
}
#[test]
fn reverse_retime_imports_backward_with_negative_otio_scalar() {
    // Live-verified Resolve behavior: negative LinearTimeWarp plays
    // backward from the same source range as forward playback.
    let dir = std::env::temp_dir().join(format!("align-xml-rev-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let xml = dir.join("rev.xml");
    // Reverse -100% with Premiere's +1 constant: values fall 1 per
    // frame from 101 to 1 over the clip (see VERIFICATION.md).
    // (0,101),(100,1): raw slope -1 → backward; shifted (0,100),(100,0).
    std::fs::write(
        &xml,
        r#"<?xml version="1.0"?><xmeml version="4"><sequence><name>R</name>
<rate><timebase>25</timebase><ntsc>FALSE</ntsc></rate>
<media><video><track>
<clipitem id="r1"><name>Rev</name>
<in>0</in><out>100</out><start>0</start><end>100</end>
<file id="f"><pathurl>file:///tmp/r.mov</pathurl><rate><timebase>25</timebase><ntsc>FALSE</ntsc></rate><duration>100</duration></file>
<filter><effect><name>Time Remap</name><effectid>timeremap</effectid>
<parameter><parameterid>graphdict</parameterid>
<keyframe><when>0</when><value>101</value></keyframe>
<keyframe><when>100</when><value>1</value></keyframe>
</parameter></effect></filter>
</clipitem>
</track></video></media>
</sequence></xmeml>"#,
    )
    .unwrap();
    let draft = read_timeline(&xml, None).expect("read");
    assert_eq!(draft.edits.len(), 1);
    let edit = &draft.edits[0];
    assert!(edit.plays_backward);
    assert!((edit.playback_rate - 1.0).abs() < 1e-9);
    // Same source range as forward playback (no phase guessing).
    assert!((edit.source_in - 0.0).abs() < 1e-9);
    assert!((edit.source_out - 4.0).abs() < 1e-9);
    assert!(
        edit.fcp7_time_remap_xml
            .as_ref()
            .unwrap()
            .contains("timeremap")
    );

    // OTIO writer emits the negative scalar.
    let timeline = draft_to_otio_timeline(&draft);
    let bytes = crate::export::otio::data(&timeline).expect("otio");
    let text = String::from_utf8(bytes).expect("utf8");
    assert!(
        text.contains("\"time_scalar\": -1.0"),
        "negative warp missing"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn non_portable_dissolve_warns_and_flattens_in_otio() {
    // Odd duration (not centered-even) → Premiere keeps it, OTIO gets
    // overlap instead of a Transition.
    let dir = std::env::temp_dir().join(format!("align-xml-np-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let xml = dir.join("np.xml");
    std::fs::write(
        &xml,
        r#"<?xml version="1.0"?><xmeml version="4"><sequence><name>N</name>
<rate><timebase>25</timebase><ntsc>FALSE</ntsc></rate>
<media><video><track>
<clipitem id="a1"><in>0</in><out>104</out><start>0</start><end>-1</end>
<file id="fa"><pathurl>file:///tmp/a.mov</pathurl><rate><timebase>25</timebase><ntsc>FALSE</ntsc></rate><duration>200</duration></file>
</clipitem>
<transitionitem><start>96</start><end>103</end><alignment>center</alignment>
<effect><name>Cross Dissolve</name><effectid>Cross Dissolve</effectid>
<effectcategory>Dissolve</effectcategory><effecttype>transition</effecttype><mediatype>video</mediatype>
<startratio>0</startratio><endratio>1</endratio><reverse>FALSE</reverse></effect>
</transitionitem>
<clipitem id="b1"><in>46</in><out>150</out><start>-1</start><end>200</end>
<file id="fb"><pathurl>file:///tmp/b.mov</pathurl><rate><timebase>25</timebase><ntsc>FALSE</ntsc></rate><duration>200</duration></file>
</clipitem>
</track></video></media>
</sequence></xmeml>"#,
    )
    .unwrap();
    let draft = read_timeline(&xml, None).expect("read");
    assert_eq!(draft.transitions.len(), 1);
    assert!(!draft.transitions[0].is_otio_portable);
    assert!(
        draft
            .warnings
            .iter()
            .any(|w| w.message.contains("flattened in Resolve OTIO"))
    );
    // Premiere writer still emits the transition item verbatim.
    let timeline = draft_to_otio_timeline(&draft);
    let premiere = crate::export::premiere::write(
        &timeline,
        crate::export_model::TimelineExportFormat::PremiereXML,
        false,
    );
    assert!(premiere.contains("<transitionitem>"));
    // OTIO has clips + overlap but no Transition node.
    let bytes = crate::export::otio::data(&timeline).expect("otio");
    let text = String::from_utf8(bytes).expect("utf8");
    assert!(!text.contains("Transition.1"));
    assert!(text.contains("Clip.2"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn variable_speed_imports_as_average_rate() {
    // 3-keyframe ramp: constant fit fails, average rate wins with warning.
    let dir = std::env::temp_dir().join(format!("align-xml-var-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let xml = dir.join("var.xml");
    std::fs::write(
        &xml,
        r#"<?xml version="1.0"?><xmeml version="4"><sequence><name>V</name>
<rate><timebase>25</timebase><ntsc>FALSE</ntsc></rate>
<media><video><track>
<clipitem id="v1"><name>Ramp</name>
<in>0</in><out>100</out><start>0</start><end>50</end>
<file id="f"><pathurl>file:///tmp/v.mov</pathurl><rate><timebase>25</timebase><ntsc>FALSE</ntsc></rate><duration>200</duration></file>
<filter><effect><name>Time Remap</name><effectid>timeremap</effectid>
<parameter><parameterid>graphdict</parameterid>
<keyframe><when>0</when><value>0</value></keyframe>
<keyframe><when>50</when><value>25</value></keyframe>
<keyframe><when>100</when><value>100</value></keyframe>
</parameter></effect></filter>
</clipitem>
</track></video></media>
</sequence></xmeml>"#,
    )
    .unwrap();
    let draft = read_timeline(&xml, None).expect("read");
    assert_eq!(draft.edits.len(), 1);
    // Average slope (100-0)/(100-0) = 1 → 100% constant.
    assert!((draft.edits[0].playback_rate - 1.0).abs() < 1e-9);
    assert!(!draft.edits[0].plays_backward);
    assert!(
        draft
            .warnings
            .iter()
            .any(|w| w.message.contains("Variable speed change"))
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn fcp7_nested_sequence_flattens_with_window_mapping() {
    // Parent uses nested seconds 1..5 of an 8 s inner timeline at 0..4 s.
    let dir = std::env::temp_dir().join(format!("align-xml-nest-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let xml = dir.join("nest.xml");
    std::fs::write(
        &xml,
        r#"<?xml version="1.0"?><xmeml version="4"><sequence><name>Main</name>
<rate><timebase>25</timebase><ntsc>FALSE</ntsc></rate>
<media><video><track>
<enabled>TRUE</enabled><locked>FALSE</locked>
<clipitem id="n1"><name>Nested Story</name>
<in>25</in><out>125</out><start>0</start><end>100</end>
<sequence><name>Inner</name>
<rate><timebase>25</timebase><ntsc>FALSE</ntsc></rate>
<media><video><track>
<enabled>TRUE</enabled><locked>FALSE</locked>
<clipitem id="inner-v1"><name>Inner Video</name>
<in>0</in><out>200</out><start>0</start><end>200</end>
<file id="fv"><pathurl>file:///tmp/nv.mov</pathurl><rate><timebase>25</timebase><ntsc>FALSE</ntsc></rate><duration>200</duration></file>
</clipitem>
</track></video>
<audio><track>
<enabled>TRUE</enabled><locked>FALSE</locked>
<clipitem id="inner-a1"><name>Inner Audio</name>
<in>10</in><out>210</out><start>0</start><end>200</end>
<file id="fa"><pathurl>file:///tmp/na.wav</pathurl><rate><timebase>25</timebase><ntsc>FALSE</ntsc></rate><duration>300</duration></file>
</clipitem>
</track></audio></media>
</sequence>
</clipitem>
<clipitem id="d1"><name>Direct</name>
<in>0</in><out>50</out><start>100</start><end>150</end>
<file id="fd"><pathurl>file:///tmp/d.mov</pathurl><rate><timebase>25</timebase><ntsc>FALSE</ntsc></rate><duration>50</duration></file>
</clipitem>
</track></video></media>
</sequence></xmeml>"#,
    )
    .unwrap();
    let draft = read_timeline(&xml, None).expect("read");
    assert_eq!(draft.edits.len(), 3);
    let v = draft
        .edits
        .iter()
        .find(|e| e.id == "n1/inner-v1")
        .expect("nested video");
    assert_eq!(v.url, PathBuf::from("/tmp/nv.mov"));
    assert!((v.source_in - 1.0).abs() < 1e-9);
    assert!((v.source_out - 5.0).abs() < 1e-9);
    assert!((v.timeline_start - 0.0).abs() < 1e-9);
    assert!((v.timeline_end - 4.0).abs() < 1e-9);
    assert!((v.playback_rate - 1.0).abs() < 1e-9);
    assert_eq!(v.track_index, 1);
    let a = draft
        .edits
        .iter()
        .find(|e| e.id == "n1/inner-a1")
        .expect("nested audio");
    assert_eq!(a.url, PathBuf::from("/tmp/na.wav"));
    assert!((a.source_in - 1.4).abs() < 1e-9);
    assert!((a.source_out - 5.4).abs() < 1e-9);
    assert!((a.timeline_start - 0.0).abs() < 1e-9);
    assert!((a.timeline_end - 4.0).abs() < 1e-9);
    assert_eq!(a.track_index, 1);
    let d = draft.edits.iter().find(|e| e.id == "d1").expect("direct");
    assert!((d.timeline_start - 4.0).abs() < 1e-9);
    assert!(
        !draft
            .warnings
            .iter()
            .any(|w| w.message.contains("no readable local file path")),
        "nested container must not raise the missing-file warning"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn fcp7_multiclip_uses_marked_angles_with_fallback_warning() {
    let dir = std::env::temp_dir().join(format!("align-xml-mc-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let xml = dir.join("mc.xml");
    std::fs::write(
        &xml,
        r#"<?xml version="1.0"?><xmeml version="4"><sequence><name>MC</name>
<rate><timebase>25</timebase><ntsc>FALSE</ntsc></rate>
<media><video><track>
<enabled>TRUE</enabled><locked>FALSE</locked>
<clipitem id="m1"><name>Interview MC</name>
<in>0</in><out>100</out><start>0</start><end>100</end>
<multiclip><name>Interview MC</name>
<angle><clip><name>Cam A</name><media>
<video><videotrack><clipitem id="ang-a-v"><name>Cam A Video</name>
<in>0</in><out>200</out><start>0</start><end>200</end>
<file id="fca"><pathurl>file:///tmp/ca.mov</pathurl><rate><timebase>25</timebase><ntsc>FALSE</ntsc></rate><duration>200</duration></file>
</clipitem></videotrack></video>
<audio><audiotrack><clipitem id="ang-a-a"><name>Cam A Audio</name>
<in>0</in><out>200</out><start>0</start><end>200</end>
<file id="fca2"><pathurl>file:///tmp/ca.mov</pathurl><rate><timebase>25</timebase><ntsc>FALSE</ntsc></rate><duration>200</duration></file>
</clipitem></audiotrack></audio>
</media></clip>
<activevideoangle/></angle>
<angle><clip><name>Cam B</name><media>
<video><videotrack><clipitem id="ang-b-v"><name>Cam B Video</name>
<in>0</in><out>200</out><start>0</start><end>200</end>
<file id="fcb"><pathurl>file:///tmp/cb.mov</pathurl><rate><timebase>25</timebase><ntsc>FALSE</ntsc></rate><duration>200</duration></file>
</clipitem></videotrack></video>
<audio><audiotrack><clipitem id="ang-b-a"><name>Cam B Audio</name>
<in>0</in><out>200</out><start>0</start><end>200</end>
<file id="fcb2"><pathurl>file:///tmp/cb.mov</pathurl><rate><timebase>25</timebase><ntsc>FALSE</ntsc></rate><duration>200</duration></file>
</clipitem></audiotrack></audio>
</media></clip>
<activeaudioangle/></angle>
</multiclip>
</clipitem>
</track></video>
<audio><track>
<enabled>TRUE</enabled><locked>FALSE</locked>
<clipitem id="m2"><name>Lone Angle</name>
<in>0</in><out>100</out><start>0</start><end>100</end>
<multiclip><name>Lone</name>
<angle><clip><name>Cam C</name><media>
<video><videotrack><clipitem id="ang-c-v"><name>Cam C Video</name>
<in>0</in><out>200</out><start>0</start><end>200</end>
<file id="fcc"><pathurl>file:///tmp/cc.mov</pathurl><rate><timebase>25</timebase><ntsc>FALSE</ntsc></rate><duration>200</duration></file>
</clipitem></videotrack></video>
<audio><audiotrack><clipitem id="ang-c-a"><name>Cam C Audio</name>
<in>0</in><out>200</out><start>0</start><end>200</end>
<file id="fcc2"><pathurl>file:///tmp/cc.mov</pathurl><rate><timebase>25</timebase><ntsc>FALSE</ntsc></rate><duration>200</duration></file>
</clipitem></audiotrack></audio>
</media></clip>
</angle>
</multiclip>
</clipitem>
</track></audio></media>
</sequence></xmeml>"#,
    )
    .unwrap();
    let draft = read_timeline(&xml, None).expect("read");
    // Marked angles: video from Cam A, audio from Cam B.
    let v = draft
        .edits
        .iter()
        .find(|e| e.media_type == DraftMediaKind::Video && e.url.to_string_lossy() == "/tmp/ca.mov")
        .expect("marked video angle");
    assert!((v.timeline_start - 0.0).abs() < 1e-9);
    assert!((v.timeline_end - 4.0).abs() < 1e-9);
    assert!((v.source_in - 0.0).abs() < 1e-9);
    assert!((v.source_out - 4.0).abs() < 1e-9);
    let a = draft
        .edits
        .iter()
        .find(|e| e.media_type == DraftMediaKind::Audio && e.url.to_string_lossy() == "/tmp/cb.mov")
        .expect("marked audio angle");
    assert!((a.timeline_start - 0.0).abs() < 1e-9);
    assert!((a.timeline_end - 4.0).abs() < 1e-9);
    // Unmarked multiclip falls back to the first angle with a warning.
    assert!(
        draft
            .edits
            .iter()
            .any(|e| e.url.to_string_lossy() == "/tmp/cc.mov"),
        "fallback angle flattened"
    );
    assert!(
        draft
            .warnings
            .iter()
            .any(|w| w.message.contains("first angle")),
        "fallback warns"
    );
    assert!(
        !draft
            .warnings
            .iter()
            .any(|w| w.message.contains("no readable local file path")),
        "multiclip must not raise the missing-file warning"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn fcpxml_prefers_original_and_reads_legacy_asset_sources() {
    for asset in [
        r#"<asset id="a" hasVideo="1" hasAudio="0" src="file:///original.mov"/>"#,
        r#"<asset id="a" hasVideo="1" hasAudio="0"><media-rep kind="proxy-media" src="file:///proxy.mov"/>
            <media-rep kind="original-media" src="file:///original.mov"/></asset>"#,
    ] {
        let text = format!(
            r#"<fcpxml version="1.10"><resources>
            <format id="f" frameDuration="1/25s"/>
            {asset}
            </resources><library><event name="Test"><project name="Test">
            <sequence format="f" duration="1s"><spine>
            <asset-clip ref="a" offset="0s" start="0s" duration="1s"/>
            </spine></sequence></project></event></library></fcpxml>"#
        );
        let doc = XmlDoc::parse(&text).unwrap();
        let draft = read_fcpxml(&doc, Path::new("test.fcpxml"), None, false).unwrap();
        assert_eq!(draft.edits[0].url, Path::new("/original.mov"));
        let preferred = read_fcpxml(&doc, Path::new("test.fcpxml"), None, true).unwrap();
        assert_eq!(
            preferred.edits[0].url,
            Path::new(if asset.contains("proxy-media") {
                "/proxy.mov"
            } else {
                "/original.mov"
            })
        );
    }
}

#[test]
fn fcpxml_compound_and_nested_compound_flatten() {
    // ref-clip C maps compound [0.8s, 5.6s] onto [0.4s, 5.2s];
    // inner ref-clip D maps [0s, 2s] of compound E onto [4s, 6s] of C.
    let dir = std::env::temp_dir().join(format!("align-xml-cmp-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let xml = dir.join("cmp.fcpxml");
    std::fs::write(
        &xml,
        r#"<?xml version="1.0"?><fcpxml version="1.10">
<resources>
<format id="r1" frameDuration="1/25s"/>
<asset id="a1" hasVideo="1" hasAudio="1"><media-rep src="file:///tmp/x.mov"/></asset>
<asset id="a2" hasVideo="0" hasAudio="1"><media-rep src="file:///tmp/y.wav"/></asset>
<media id="e1" name="Inner"><sequence format="r1"><spine>
<asset-clip ref="a2" name="Deep" offset="0s" duration="50/25s" start="0s"/>
</spine></sequence></media>
<media id="c1" name="Compound"><sequence format="r1"><spine>
<asset-clip ref="a1" name="X" offset="0s" duration="100/25s" start="0s"/>
<asset-clip ref="a2" name="Y" offset="100/25s" duration="50/25s" start="0s"/>
<ref-clip ref="e1" name="D" offset="100/25s" duration="50/25s" start="0s"/>
</spine></sequence></media>
</resources>
<library><event><project name="Proj">
<sequence format="r1" name="Cut">
<spine>
<ref-clip ref="c1" name="C" offset="10/25s" duration="120/25s" start="20/25s"/>
</spine>
</sequence>
</project></event></library>
</fcpxml>"#,
    )
    .unwrap();
    let draft = read_timeline(&xml, None).expect("read");
    // X: compound [0.8, 4.0] -> [0.4, 3.6]; Y: [4.0, 5.6] -> [3.6, 5.2].
    let x = draft
        .edits
        .iter()
        .find(|e| e.url.to_string_lossy() == "/tmp/x.mov")
        .expect("x");
    assert!((x.source_in - 0.8).abs() < 1e-9);
    assert!((x.source_out - 4.0).abs() < 1e-9);
    assert!((x.timeline_start - 0.4).abs() < 1e-9);
    assert!((x.timeline_end - 3.6).abs() < 1e-9);
    let y: Vec<_> = draft
        .edits
        .iter()
        .filter(|e| e.url.to_string_lossy() == "/tmp/y.wav")
        .collect();
    assert_eq!(y.len(), 2);
    let y_direct = y
        .iter()
        .find(|e| (e.timeline_start - 3.6).abs() < 1e-9)
        .expect("y");
    assert!((y_direct.source_in - 0.0).abs() < 1e-9);
    assert!((y_direct.source_out - 1.6).abs() < 1e-9);
    // Depth-2: E [0, 2] of C -> C [4, 6] -> parent [3.6, 5.6],
    // capped by the C window [0.4, 5.2].
    let deep = y
        .iter()
        .find(|e| (e.timeline_start - 3.6).abs() < 0.01)
        .expect("deep");
    assert!((deep.timeline_end - 5.2).abs() < 0.01);
    assert!((deep.source_in - 0.0).abs() < 1e-9);
    assert!((deep.source_out - 1.6).abs() < 0.01);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn fcpxml_multicam_segments_and_audition() {
    let dir = std::env::temp_dir().join(format!("align-xml-mc-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let xml = dir.join("mc.fcpxml");
    std::fs::write(
        &xml,
        r#"<?xml version="1.0"?><fcpxml version="1.10">
<resources>
<format id="r1" frameDuration="1/25s"/>
<asset id="a1" hasVideo="1" hasAudio="1"><media-rep src="file:///tmp/va.mov"/></asset>
<asset id="a2" hasVideo="0" hasAudio="1"><media-rep src="file:///tmp/au.wav"/></asset>
<media id="m1" name="MC"><multicam format="r1">
<mc-angle name="A" angleID="ang1">
<asset-clip ref="a1" offset="0s" duration="200/25s" start="0s"/>
</mc-angle>
<mc-angle name="B" angleID="ang2">
<asset-clip ref="a2" offset="0s" duration="200/25s" start="0s"/>
</mc-angle>
</multicam></media>
</resources>
<library><event><project name="Proj">
<sequence format="r1" name="Cut">
<spine>
<mc-clip ref="m1" name="M1" offset="0s" duration="100/25s" start="0s">
<mc-source angleID="ang1" srcEnable="all"/>
</mc-clip>
<mc-clip ref="m1" name="M2" offset="100/25s" duration="100/25s" start="100/25s">
<mc-source angleID="ang1" srcEnable="video" offset="100/25s" duration="50/25s"/>
<mc-source angleID="ang2" srcEnable="audio" offset="150/25s" duration="50/25s"/>
</mc-clip>
<audition>
<asset-clip ref="a1" name="Pick" offset="200/25s" duration="25/25s" start="0s"/>
<asset-clip ref="a2" name="Alt" offset="200/25s" duration="25/25s" start="0s"/>
</audition>
</spine>
</sequence>
</project></event></library>
</fcpxml>"#,
    )
    .unwrap();
    let draft = read_timeline(&xml, None).expect("read");
    // M1: whole-span angle A (video+audio of va.mov).
    let m1: Vec<_> = draft
        .edits
        .iter()
        .filter(|e| e.url.to_string_lossy() == "/tmp/va.mov" && e.timeline_start < 4.0)
        .collect();
    assert_eq!(m1.len(), 2);
    // M2: video from angle A [4, 6], audio from angle B [6, 8].
    let m2v = draft
        .edits
        .iter()
        .find(|e| {
            e.url.to_string_lossy() == "/tmp/va.mov"
                && e.media_type == DraftMediaKind::Video
                && (e.timeline_start - 4.0).abs() < 1e-9
        })
        .expect("m2 video");
    assert!((m2v.timeline_end - 6.0).abs() < 1e-9);
    assert!((m2v.source_in - 4.0).abs() < 1e-9);
    let m2a = draft
        .edits
        .iter()
        .find(|e| e.url.to_string_lossy() == "/tmp/au.wav" && (e.timeline_start - 6.0).abs() < 1e-9)
        .expect("m2 audio");
    assert!((m2a.timeline_end - 8.0).abs() < 1e-9);
    assert!((m2a.source_in - 6.0).abs() < 1e-9);
    assert!((m2a.source_out - 8.0).abs() < 1e-9);
    // Audition: active pick only, with a warning.
    let picks: Vec<_> = draft
        .edits
        .iter()
        .filter(|e| (e.timeline_start - 8.0).abs() < 1e-9)
        .collect();
    assert_eq!(picks.len(), 2);
    assert!(
        picks
            .iter()
            .all(|e| e.url.to_string_lossy() == "/tmp/va.mov")
    );
    assert!(
        draft
            .warnings
            .iter()
            .any(|w| w.message.contains("Audition alternatives")),
        "audition warns"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn clip_edit_to_window_maps_forward_and_backward() {
    let base = || DraftEdit {
        id: "e".to_string(),
        name: None,
        url: PathBuf::from("/tmp/m.mov"),
        media_type: DraftMediaKind::Video,
        source_in: 10.0,
        source_out: 20.0,
        timeline_start: 0.0,
        timeline_end: 10.0,
        playback_rate: 1.0,
        plays_backward: false,
        fcp7_time_remap_xml: None,
        fcp7_filter_xmls: Vec::new(),
        fcp7_retime_in: None,
        fcp7_retime_out: None,
        fcp7_retime_duration: None,
        fcp7_labels_xml: None,
        time_scale: 1_000_000,
        include_embedded_audio: true,
        audio_source_channel: None,
        fcpxml_audio_role: None,
        track_index: 1,
        enabled: true,
        track_enabled: true,
        track_locked: false,
        linked_edit_ids: HashSet::new(),
    };
    let mut fwd = base();
    assert!(clip_edit_to_window(&mut fwd, 2.0, 5.0));
    assert!((fwd.source_in - 12.0).abs() < 1e-9);
    assert!((fwd.source_out - 15.0).abs() < 1e-9);
    assert!((fwd.timeline_start - 2.0).abs() < 1e-9);
    assert!((fwd.timeline_end - 5.0).abs() < 1e-9);
    let mut outside = base();
    assert!(!clip_edit_to_window(&mut outside, 20.0, 30.0));
    let mut bwd = base();
    bwd.plays_backward = true;
    assert!(clip_edit_to_window(&mut bwd, 2.0, 5.0));
    // Reversed: timeline 2..5 maps to source 18..15.
    assert!((bwd.source_in - 15.0).abs() < 1e-9);
    assert!((bwd.source_out - 18.0).abs() < 1e-9);
    assert!((bwd.timeline_start - 2.0).abs() < 1e-9);
    assert!((bwd.timeline_end - 5.0).abs() < 1e-9);
}

#[test]
fn fcpxml_cyclic_compound_is_skipped_without_hanging() {
    let dir = std::env::temp_dir().join(format!("align-xml-cyc-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let xml = dir.join("cyc.fcpxml");
    std::fs::write(
        &xml,
        r#"<?xml version="1.0"?><fcpxml version="1.10">
<resources>
<format id="r1" frameDuration="1/25s"/>
<asset id="a1" hasVideo="1" hasAudio="0"><media-rep src="file:///tmp/x.mov"/></asset>
<media id="c1" name="A"><sequence format="r1"><spine>
<ref-clip ref="c2" name="ToB" offset="0s" duration="10/25s" start="0s"/>
</spine></sequence></media>
<media id="c2" name="B"><sequence format="r1"><spine>
<asset-clip ref="a1" name="X" offset="0s" duration="10/25s" start="0s"/>
<ref-clip ref="c1" name="ToA" offset="10/25s" duration="10/25s" start="0s"/>
</spine></sequence></media>
</resources>
<library><event><project name="Proj">
<sequence format="r1" name="Cut">
<spine>
<ref-clip ref="c1" name="C" offset="0s" duration="20/25s" start="0s"/>
</spine>
</sequence>
</project></event></library>
</fcpxml>"#,
    )
    .unwrap();
    let draft = read_timeline(&xml, None).expect("read");
    // B's asset flattens once; the A<->B cycle is cut with a warning.
    assert_eq!(draft.edits.len(), 1);
    assert_eq!(draft.edits[0].url, PathBuf::from("/tmp/x.mov"));
    assert!(
        draft
            .warnings
            .iter()
            .any(|w| w.message.contains("Cyclic compound")),
        "cycle warns"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn fcp7_deep_nesting_hits_the_depth_cap() {
    // Programmatic 12-deep inline nesting; the cap stops expansion.
    let mut inner = String::from(
        r#"<clipitem id="leaf"><name>Leaf</name><in>0</in><out>100</out><start>0</start><end>100</end><file id="f"><pathurl>file:///tmp/leaf.mov</pathurl><rate><timebase>25</timebase><ntsc>FALSE</ntsc></rate><duration>100</duration></file></clipitem>"#,
    );
    for level in 0..12 {
        inner = format!(
            r#"<clipitem id="n{level}"><name>N{level}</name><in>0</in><out>100</out><start>0</start><end>100</end><sequence><name>S{level}</name><rate><timebase>25</timebase><ntsc>FALSE</ntsc></rate><media><video><track><enabled>TRUE</enabled><locked>FALSE</locked>{inner}</track></video></media></sequence></clipitem>"#
        );
    }
    let doc = format!(
        r#"<?xml version="1.0"?><xmeml version="4"><sequence><name>Deep</name><rate><timebase>25</timebase><ntsc>FALSE</ntsc></rate><media><video><track><enabled>TRUE</enabled><locked>FALSE</locked>{inner}<clipitem id="flat"><name>Flat</name><in>0</in><out>100</out><start>0</start><end>100</end><file id="ff"><pathurl>file:///tmp/flat.mov</pathurl><rate><timebase>25</timebase><ntsc>FALSE</ntsc></rate><duration>100</duration></file></clipitem></track></video></media></sequence></xmeml>"#
    );
    let dir = std::env::temp_dir().join(format!("align-xml-deep-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let xml = dir.join("deep.xml");
    std::fs::write(&xml, doc).unwrap();
    let draft = read_timeline(&xml, None).expect("read");
    // The direct clip survives; the over-deep branch is cut with a warning.
    assert_eq!(draft.edits.len(), 1);
    assert_eq!(draft.edits[0].url, PathBuf::from("/tmp/flat.mov"));
    assert!(
        draft
            .warnings
            .iter()
            .any(|w| w.message.contains("too deep")),
        "depth cap warns"
    );
    // Terminates (this assertion is the test): no stack overflow.
    let _ = std::fs::remove_dir_all(&dir);
}

/// Minimal ExportTimeline from a draft for writer-level assertions.
fn draft_to_otio_timeline(draft: &TimelineDraft) -> crate::export_model::ExportTimeline {
    use crate::export_model::{ExportIsland, ExportItem};
    use crate::model::{AudioSummary, Clip, ClipId, MediaKind, VideoSummary};
    let mut clips = Vec::new();
    for edit in &draft.edits {
        let kind = match edit.media_type {
            DraftMediaKind::Video => MediaKind::Video,
            DraftMediaKind::Audio => MediaKind::Audio,
        };
        let clip = Clip {
            id: ClipId::new(format!("clip-{}", edit.id)),
            url: edit.url.clone(),
            kind,
            duration: MediaTime::seconds(edit.source_out.max(8.0)),
            audio: vec![AudioSummary {
                sample_rate: 48000.0,
                channels: 1,
                bit_depth: None,
                is_float: None,
                source_timecode: None,
            }],
            video: (kind == MediaKind::Video).then(|| VideoSummary {
                width: 1920,
                height: 1080,
                frame_duration: Some(MediaTime::new(1, 25)),
                source_timecode: None,
                frame_rate_mode: None,
            }),
            recorded_at: None,
            recorded_at_source: None,
            source_identifier: None,
            media_span: None,
        };
        let mut item = ExportItem::new(clip, edit.timeline_start, 1.0, vec![], 1.0);
        item.instance_id = edit.id.clone();
        item.source_in = edit.source_in;
        item.source_out = edit.source_out;
        item.timeline_duration = edit.timeline_end - edit.timeline_start;
        item.playback_rate = edit.playback_rate;
        item.plays_backward = edit.plays_backward;
        item.fcp7_time_remap_xml = edit.fcp7_time_remap_xml.clone();
        item.transition_after = draft
            .transitions
            .iter()
            .find(|t| t.left_edit_id == edit.id)
            .map(|t| crate::export_model::ExportTransition {
                kind: crate::export_model::ExportTransitionKind::from_model(t.kind),
                right_instance_id: t.right_edit_id.clone(),
                start: t.start,
                end: t.end,
                alignment: t.alignment.clone(),
                fcp7_effect_xml: t.effect_xml.clone(),
                fcp7_transition_xml: Some(t.transition_xml.clone()),
                is_otio_portable: t.is_otio_portable,
            });
        clips.push(item);
    }
    crate::export_model::ExportTimeline::new(
        vec![ExportIsland {
            id: 0,
            clips,
            duration: 12.0,
        }],
        draft.frame_duration,
        &draft.name,
    )
}
