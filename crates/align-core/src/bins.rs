//! Media bins: the camera or recorder each source file came from, named for
//! an editor's media browser.
//!
//! A bin is the folder a file was copied into, with the folder structure a
//! camera or recorder writes on its card left out: clips from
//! `Wedding/CamA/PRIVATE/M4ROOT/CLIP` belong to `CamA`. A folder that holds
//! several cameras, told apart by their device metadata, or both video and
//! audio-only files, is split into one bin per camera and one for audio.
//!
//! Exports organize the media they reference by these bins: Premiere bins,
//! Final Cut Pro keyword collections, and DaVinci Resolve media pool folders.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Component, Path, PathBuf};

use crate::model::{Clip, ClipId};

/// The bin holding audio that Align rendered for the timelines: drift
/// corrections and precision stems.
pub const GENERATED_AUDIO: &str = "Align generated audio";

/// Joins a folder name to the parent folders that tell it apart. Bins are
/// flat, so the separator must not read as nesting.
const PARENT_SEPARATOR: &str = " · ";

/// Separates a folder name from the camera or media type it is split by.
const PART_SEPARATOR: &str = " – ";

#[derive(Debug)]
pub struct MediaBin<'a> {
    pub name: String,
    /// Source clips in path order.
    pub clips: Vec<&'a Clip>,
}

impl MediaBin<'_> {
    fn has_video(&self) -> bool {
        self.clips.iter().any(|clip| clip.video.is_some())
    }
}

/// Every distinct source clip, grouped into uniquely named bins: camera
/// bins first, then audio, each in name order.
#[derive(Debug, Default)]
pub struct MediaBins<'a> {
    bins: Vec<MediaBin<'a>>,
    by_clip: HashMap<&'a ClipId, usize>,
}

impl<'a> MediaBins<'a> {
    pub fn new(clips: impl IntoIterator<Item = &'a Clip>) -> Self {
        let mut seen = HashSet::new();
        let mut by_folder: BTreeMap<PathBuf, Vec<&'a Clip>> = BTreeMap::new();
        for clip in clips {
            if seen.insert(&clip.id) {
                let folder = clip.url.parent().unwrap_or(Path::new(""));
                by_folder.entry(card_root(folder)).or_default().push(clip);
            }
        }
        let folders: Vec<&Path> = by_folder.keys().map(PathBuf::as_path).collect();
        let folder_names = folder_names(&folders);

        let mut bins = Vec::new();
        for ((_, clips), folder_name) in by_folder.into_iter().zip(folder_names) {
            let parts = sources(clips);
            let split = parts.len() > 1;
            for (part, mut clips) in parts {
                clips.sort_by(|a, b| a.url.cmp(&b.url));
                bins.push(MediaBin {
                    name: if split {
                        format!("{folder_name}{PART_SEPARATOR}{part}")
                    } else {
                        folder_name.clone()
                    },
                    clips,
                });
            }
        }
        bins.sort_by(|a, b| {
            b.has_video()
                .cmp(&a.has_video())
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
                .then_with(|| a.name.cmp(&b.name))
        });
        let mut taken: HashSet<String> = HashSet::from([GENERATED_AUDIO.to_lowercase()]);
        for bin in &mut bins {
            bin.name = unique(&bin.name, &mut taken);
        }
        let by_clip = bins
            .iter()
            .enumerate()
            .flat_map(|(index, bin)| bin.clips.iter().map(move |clip| (&clip.id, index)))
            .collect();
        Self { bins, by_clip }
    }

    pub fn bins(&self) -> &[MediaBin<'a>] {
        &self.bins
    }

    pub fn is_empty(&self) -> bool {
        self.bins.is_empty()
    }

    /// The name of the bin holding `clip`.
    pub fn bin_name(&self, clip: &ClipId) -> Option<&str> {
        self.by_clip
            .get(clip)
            .map(|&index| self.bins[index].name.as_str())
    }
}

/// `name`, or `name 2`, `name 3`… when an earlier bin took it. Editors
/// compare bin names without regard to case.
fn unique(name: &str, taken: &mut HashSet<String>) -> String {
    let mut candidate = name.to_string();
    let mut number = 2;
    while !taken.insert(candidate.to_lowercase()) {
        candidate = format!("{name} {number}");
        number += 1;
    }
    candidate
}

/// The cameras and media types of one folder's clips. Several video devices
/// identified by their metadata are separated; video without a device
/// joins the only identified device, and audio-only files form their own part.
fn sources(clips: Vec<&Clip>) -> Vec<(String, Vec<&Clip>)> {
    let device = |clip: &Clip| {
        clip.source_identifier
            .as_deref()
            .and_then(|id| id.strip_prefix("device:"))
            .map(str::to_string)
    };
    let (video, audio): (Vec<&Clip>, Vec<&Clip>) =
        clips.into_iter().partition(|clip| clip.video.is_some());
    let devices: BTreeMap<String, Vec<&Clip>> =
        video.iter().fold(BTreeMap::new(), |mut devices, clip| {
            if let Some(id) = device(clip) {
                devices.entry(id).or_insert_with(Vec::new).push(*clip);
            }
            devices
        });
    let mut parts = Vec::new();
    if devices.len() > 1 {
        let models: Vec<String> = devices.keys().map(|id| device_model(id).into()).collect();
        for (id, clips) in devices {
            let model = device_model(&id);
            let label = if models.iter().filter(|m| *m == model).count() > 1 {
                format!("{model} {}", id.rsplit(':').next().unwrap_or(""))
            } else {
                model.to_string()
            };
            parts.push((label, clips));
        }
        let unidentified: Vec<&Clip> = video.into_iter().filter(|c| device(c).is_none()).collect();
        if !unidentified.is_empty() {
            parts.push(("Video".to_string(), unidentified));
        }
    } else if !video.is_empty() {
        let label = match devices.keys().next() {
            Some(id) if video.iter().all(|clip| device(clip).as_deref() == Some(id)) => {
                device_model(id).to_string()
            }
            _ => "Video".to_string(),
        };
        parts.push((label, video));
    }
    if !audio.is_empty() {
        parts.push(("Audio".to_string(), audio));
    }
    parts
}

/// `manufacturer:model:serial` → `model`.
fn device_model(id: &str) -> &str {
    id.split(':').nth(1).filter(|m| !m.is_empty()).unwrap_or(id)
}

/// Folder names, extended with parent folders until every name is unique:
/// `Day 1 · CamA` and `Day 2 · CamA`.
fn folder_names(folders: &[&Path]) -> Vec<String> {
    let components: Vec<Vec<String>> = folders
        .iter()
        .map(|folder| {
            folder
                .components()
                .filter_map(|c| match c {
                    Component::Normal(name) => Some(name.to_string_lossy().into_owned()),
                    _ => None,
                })
                .collect()
        })
        .collect();
    let mut depth = vec![1usize; folders.len()];
    let name = |index: usize, depth: usize| {
        let parts = &components[index];
        if parts.is_empty() {
            "Media".to_string()
        } else {
            parts[parts.len().saturating_sub(depth)..].join(PARENT_SEPARATOR)
        }
    };
    loop {
        let mut by_name: HashMap<String, Vec<usize>> = HashMap::new();
        for (index, &depth) in depth.iter().enumerate() {
            by_name
                .entry(name(index, depth).to_lowercase())
                .or_default()
                .push(index);
        }
        let mut extended = false;
        for indices in by_name.values().filter(|indices| indices.len() > 1) {
            for &index in indices {
                if depth[index] < components[index].len() {
                    depth[index] += 1;
                    extended = true;
                }
            }
        }
        if !extended {
            return depth
                .iter()
                .enumerate()
                .map(|(index, &depth)| name(index, depth))
                .collect();
        }
    }
}

#[derive(Clone, Copy)]
enum Level {
    Named(&'static str),
    /// A prefix followed by exactly this many digits: `FOLDER01`, `ZOOM0001`.
    Numbered(&'static str, usize),
    /// A folder name ending in `.EXT`, like RED's `.RDM` and `.RDC`.
    Extension(&'static str),
    Any,
}

impl Level {
    fn matches(self, name: &str) -> bool {
        let upper = name.to_ascii_uppercase();
        match self {
            Self::Named(expected) => upper == expected,
            Self::Numbered(prefix, digits) => upper.strip_prefix(prefix).is_some_and(|rest| {
                rest.len() == digits && rest.bytes().all(|b| b.is_ascii_digit())
            }),
            Self::Extension(extension) => upper
                .strip_suffix(extension)
                .is_some_and(|stem| stem.len() > 1 && stem.ends_with('.')),
            Self::Any => true,
        }
    }
}

/// Folder structures cameras and recorders write below a card's root, from
/// the root down to the folder holding the media files.
const CARD_LAYOUTS: &[&[Level]] = {
    use Level::*;
    &[
        // Sony XAVC S and XAVC HS.
        &[Named("PRIVATE"), Named("M4ROOT"), Named("CLIP")],
        &[Named("M4ROOT"), Named("CLIP")],
        // Sony XDCAM and XAVC cinema cameras.
        &[Named("PRIVATE"), Named("XDROOT"), Named("CLIP")],
        &[Named("XDROOT"), Named("CLIP")],
        &[Named("BPAV"), Named("CLPR"), Any],
        &[Named("MP_ROOT"), Any],
        // AVCHD.
        &[
            Named("PRIVATE"),
            Named("AVCHD"),
            Named("BDMV"),
            Named("STREAM"),
        ],
        &[Named("AVCHD"), Named("BDMV"), Named("STREAM")],
        &[Named("BDMV"), Named("STREAM")],
        // Photo-style cameras: Canon, Panasonic, Nikon, Fujifilm, GoPro, DJI.
        &[Named("DCIM"), Any],
        // Canon XF and Panasonic P2.
        &[Named("CONTENTS"), Numbered("CLIPS", 3), Any],
        &[Named("CONTENTS"), Numbered("CLIPS", 3)],
        &[Named("CONTENTS"), Named("VIDEO")],
        &[Named("CONTENTS"), Named("AUDIO")],
        // RED.
        &[Extension("RDM"), Extension("RDC")],
        &[Extension("RDC")],
        // Zoom recorders.
        &[Named("MULTI"), Numbered("FOLDER", 2), Numbered("ZOOM", 4)],
        &[Named("STEREO"), Numbered("FOLDER", 2)],
        &[Named("4CH"), Numbered("FOLDER", 2)],
        &[Numbered("FOLDER", 2), Numbered("ZOOM", 4)],
        &[Numbered("FOLDER", 2)],
        &[Numbered("ZOOM", 4)],
    ]
};

/// The folder a card's media was copied into: `folder` without the deepest
/// card structure it ends with, if anything remains above that structure.
pub fn card_root(folder: &Path) -> PathBuf {
    let names: Vec<String> = folder
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    let longest = CARD_LAYOUTS
        .iter()
        .filter(|layout| layout.len() < names.len())
        .filter(|layout| {
            names[names.len() - layout.len()..]
                .iter()
                .zip(layout.iter())
                .all(|(name, level)| level.matches(name))
        })
        .map(|layout| layout.len())
        .max()
        .unwrap_or(0);
    let mut root = folder.to_path_buf();
    for _ in 0..longest {
        root.pop();
    }
    root
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{AudioSummary, MediaKind, MediaTime, VideoSummary};

    fn clip(path: &str, video: bool, device: Option<&str>) -> Clip {
        Clip {
            id: ClipId::new(path),
            url: PathBuf::from(path),
            kind: if video {
                MediaKind::Video
            } else {
                MediaKind::Audio
            },
            duration: MediaTime::new(10, 1),
            audio: vec![AudioSummary {
                sample_rate: 48_000.0,
                channels: 2,
                bit_depth: Some(24),
                is_float: None,
                source_timecode: None,
            }],
            video: video.then(|| VideoSummary {
                width: 1920,
                height: 1080,
                frame_duration: Some(MediaTime::new(1, 25)),
                source_timecode: None,
                frame_rate_mode: None,
            }),
            recorded_at: None,
            recorded_at_source: None,
            source_identifier: device.map(|d| format!("device:{d}")),
            media_span: None,
        }
    }

    fn contents(bins: &MediaBins) -> Vec<(String, Vec<String>)> {
        bins.bins()
            .iter()
            .map(|bin| {
                (
                    bin.name.clone(),
                    bin.clips
                        .iter()
                        .map(|c| crate::model::file_name(&c.url))
                        .collect(),
                )
            })
            .collect()
    }

    fn bin_names(clips: &[Clip]) -> Vec<String> {
        MediaBins::new(clips)
            .bins()
            .iter()
            .map(|bin| bin.name.clone())
            .collect()
    }

    #[test]
    fn card_structure_is_not_a_bin() {
        for (folder, root) in [
            ("/Shoot/CamA/PRIVATE/M4ROOT/CLIP", "/Shoot/CamA"),
            ("/Shoot/FX6/XDROOT/Clip", "/Shoot/FX6"),
            ("/Shoot/GH6/DCIM/100_PANA", "/Shoot/GH6"),
            ("/Volumes/EOS_DIGITAL/DCIM/100CANON", "/Volumes/EOS_DIGITAL"),
            ("/Shoot/HC/PRIVATE/AVCHD/BDMV/STREAM", "/Shoot/HC"),
            ("/Shoot/C300/CONTENTS/CLIPS001/AA0001", "/Shoot/C300"),
            (
                "/Shoot/RED/A001_0101XY.RDM/A001_C001_0101XY.RDC",
                "/Shoot/RED",
            ),
            ("/Shoot/Zoom/MULTI/FOLDER01/ZOOM0001", "/Shoot/Zoom"),
            ("/Shoot/H4n/STEREO/FOLDER01", "/Shoot/H4n"),
            // Ordinary folders, including ones named like a single card level.
            ("/Shoot/CamA", "/Shoot/CamA"),
            ("/Users/me/Private", "/Users/me/Private"),
            ("/Shoot/CLIP", "/Shoot/CLIP"),
            // Nothing above the structure: keep the folder.
            ("DCIM/100CANON", "DCIM/100CANON"),
        ] {
            assert_eq!(
                card_root(Path::new(folder)),
                PathBuf::from(root),
                "{folder}"
            );
        }
    }

    #[test]
    fn bins_follow_folders_with_cameras_first() {
        let clips = [
            clip(
                "/Wedding/Zoom/MULTI/FOLDER01/ZOOM0002/ZOOM0002_LR.WAV",
                false,
                None,
            ),
            clip(
                "/Wedding/Zoom/MULTI/FOLDER01/ZOOM0001/ZOOM0001_LR.WAV",
                false,
                None,
            ),
            clip("/Wedding/CamB/DCIM/100CANON/MVI_0001.MP4", true, None),
            clip("/Wedding/CamA/PRIVATE/M4ROOT/CLIP/C0001.MP4", true, None),
            clip("/Wedding/CamA/PRIVATE/M4ROOT/CLIP/C0001.MP4", true, None),
        ];
        let bins = MediaBins::new(&clips);
        assert_eq!(
            contents(&bins),
            [
                ("CamA".into(), vec!["C0001.MP4".into()]),
                ("CamB".into(), vec!["MVI_0001.MP4".into()]),
                (
                    "Zoom".into(),
                    vec!["ZOOM0001_LR.WAV".into(), "ZOOM0002_LR.WAV".into()]
                ),
            ]
        );
        assert_eq!(bins.bin_name(&clips[2].id), Some("CamB"));
    }

    #[test]
    fn folders_with_the_same_name_are_named_by_their_parents() {
        let clips = [
            clip("/Wedding/Day 1/CamA/C0001.MP4", true, None),
            clip("/Wedding/Day 2/CamA/C0001.MP4", true, None),
            clip("/Wedding/Day 2/Audio/T001.WAV", false, None),
        ];
        let names = bin_names(&clips);
        assert_eq!(names, ["Day 1 · CamA", "Day 2 · CamA", "Audio"]);
    }

    #[test]
    fn one_folder_of_several_sources_is_split() {
        let clips = [
            clip("/Dump/C0001.MP4", true, Some("Sony:ILME-FX3:111")),
            clip("/Dump/C0002.MP4", true, Some("Sony:ILME-FX3:222")),
            clip("/Dump/C0003.MP4", true, Some("Sony:ILCE-7SM3:333")),
            clip("/Dump/C0004.MP4", true, None),
            clip("/Dump/T001.WAV", false, None),
        ];
        let names = bin_names(&clips);
        assert_eq!(
            names,
            [
                "Dump – ILCE-7SM3",
                "Dump – ILME-FX3 111",
                "Dump – ILME-FX3 222",
                "Dump – Video",
                "Dump – Audio",
            ]
        );

        // One identified camera keeps the files whose metadata is missing.
        let single = [
            clip("/CamA/C0001.MP4", true, Some("Sony:ILME-FX3:111")),
            clip("/CamA/C0002.MP4", true, None),
            clip("/CamA/A001.WAV", false, None),
        ];
        let bins = MediaBins::new(&single);
        assert_eq!(
            contents(&bins),
            [
                (
                    "CamA – Video".into(),
                    vec!["C0001.MP4".into(), "C0002.MP4".into()]
                ),
                ("CamA – Audio".into(), vec!["A001.WAV".into()]),
            ]
        );
    }

    #[test]
    fn bin_names_stay_unique() {
        let clips = [
            clip("C:/Media/Align generated audio/A.WAV", false, None),
            clip("D:/Media/Align generated audio/B.WAV", false, None),
        ];
        let names = bin_names(&clips);
        assert_eq!(names.len(), 2);
        assert!(
            names
                .iter()
                .all(|n| !n.eq_ignore_ascii_case(GENERATED_AUDIO))
        );
        assert_ne!(names[0].to_lowercase(), names[1].to_lowercase());
    }
}
