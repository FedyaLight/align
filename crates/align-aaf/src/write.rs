use crate::{Clock, Rate, absolute, file_url, integer, read::Kind, staged, storage::*};
use anyhow::{Context, Result, ensure};
use cfb::CompoundFile;
use flate2::read::GzDecoder;
use num_traits::{ToPrimitive, Zero};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    fs::File,
    io::{self, BufReader, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};
use uuid::Uuid;

type MobId = [u8; 32];

#[derive(Deserialize)]
struct Document {
    version: u32,
    name: String,
    tracks: Vec<Track<AudioClip>>,
    #[serde(default)]
    picture_tracks: Vec<Track<PictureClip>>,
    composition_edit_rate: Option<Rate>,
    #[serde(default)]
    composition_drop_frame: bool,
}

#[derive(Deserialize)]
struct Track<T> {
    name: String,
    edit_rate: Option<Rate>,
    sample_rate: Option<i32>,
    clips: Vec<T>,
}

#[derive(Deserialize)]
struct Range {
    path: PathBuf,
    start: i64,
    source_in: i64,
    length: i64,
}

impl Range {
    fn check(&self, cursor: i64, frames: i64) -> Result<i64> {
        ensure!(
            self.start >= cursor
                && self.source_in >= 0
                && self.length > 0
                && self
                    .source_in
                    .checked_add(self.length)
                    .is_some_and(|end| end <= frames),
            "Overlapping or out-of-bounds AAF clip"
        );
        self.end()
    }

    fn end(&self) -> Result<i64> {
        self.start
            .checked_add(self.length)
            .context("AAF range overflow")
    }
}

#[derive(Deserialize)]
struct AudioClip {
    #[serde(flatten)]
    range: Range,
    source_frames: i64,
    channels: u16,
    channel: Option<u32>,
    metadata: Option<Metadata>,
}

#[derive(Deserialize)]
struct PictureClip {
    #[serde(flatten)]
    range: Range,
    metadata: Metadata,
    timecode: Option<Timecode>,
}

#[derive(Deserialize)]
struct Timecode {
    start: i64,
    fps: u16,
    drop: bool,
}

#[derive(Deserialize)]
struct Metadata {
    format: Option<MediaFormat>,
    #[serde(default)]
    streams: Vec<MediaStream>,
}

#[derive(Deserialize)]
struct MediaFormat {
    format_long_name: String,
}

fn number_or_string<'de, D: serde::Deserializer<'de>>(input: D) -> Result<i64, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Numeric {
        Number(i64),
        String(String),
    }
    match Numeric::deserialize(input)? {
        Numeric::Number(value) => Ok(value),
        Numeric::String(value) => value.parse().map_err(serde::de::Error::custom),
    }
}

#[derive(Deserialize)]
#[serde(tag = "codec_type")]
enum MediaStream {
    #[serde(rename = "audio")]
    Sound {
        sample_rate: String,
        #[serde(deserialize_with = "number_or_string")]
        duration_ts: i64,
        channels: u32,
        bit_rate: String,
        sample_fmt: String,
    },
    #[serde(rename = "video")]
    Picture {
        avg_frame_rate: String,
        #[serde(deserialize_with = "number_or_string")]
        nb_frames: i64,
        width: u32,
        height: u32,
        codec_name: String,
        profile: Option<String>,
        pix_fmt: String,
    },
    #[serde(other)]
    Other,
}

struct Object {
    class: u8,
    props: Props,
    children: Vec<(String, Object)>,
    streams: Vec<(String, Vec<u8>)>,
}

impl Object {
    fn new(class: u8) -> Self {
        Self {
            class,
            props: Props::default(),
            children: Vec::new(),
            streams: Vec::new(),
        }
    }

    fn object(&mut self, pid: u16, name: &str, child: Object) {
        let name = format!("{name}-{pid:x}");
        self.props.set(pid, REF, utf16(&name));
        self.children.push((name, child));
    }

    fn vector(&mut self, pid: u16, name: &str, children: Vec<Object>) -> Result<()> {
        // Editors expect AAF's property-name and local-property-ID convention.
        let name = format!("{name}-{pid:x}");
        self.props.set(pid, VECTOR, utf16(&name));
        let size = u32::try_from(children.len())?;
        let mut index = index_header(size);
        for (i, child) in children.into_iter().enumerate() {
            index.extend((i as u32).to_le_bytes());
            self.children.push((format!("{name}{{{i:x}}}"), child));
        }
        self.streams.push((format!("{name} index"), index));
        Ok(())
    }

    fn write<F: Read + Write + Seek>(self, file: &mut CompoundFile<F>, path: &Path) -> Result<()> {
        file.create_storage(path)?;
        file.set_storage_clsid(path, class_id(self.class))?;
        self.props.save(file, path)?;
        for (name, data) in self.streams {
            let mut stream = file.create_stream(path.join(name))?;
            stream.write_all(&data)?;
            stream.flush()?;
        }
        for (name, child) in self.children {
            child.write(file, &path.join(name))?;
        }
        Ok(())
    }
}

fn index_header(size: u32) -> Vec<u8> {
    [size, size, u32::MAX]
        .into_iter()
        .flat_map(u32::to_le_bytes)
        .collect()
}

fn weak(index: u16, id: Uuid) -> Vec<u8> {
    let mut value = Vec::new();
    value.extend(index.to_le_bytes());
    value.extend(0x1b01u16.to_le_bytes());
    value.push(16);
    value.extend(id.to_bytes_le());
    value
}

fn rate_bytes(rate: &Clock) -> Result<Vec<u8>> {
    let Rate {
        numerator,
        denominator,
    } = Rate::from_clock(rate)?;
    Ok([numerator, denominator]
        .into_iter()
        .flat_map(i32::to_le_bytes)
        .collect())
}

fn component(class: u8, kind: Kind, length: i64) -> Object {
    let mut object = Object::new(class);
    object.props.set(KIND, WEAK, weak(2, kind.id()));
    object.props.data(LENGTH, length.to_le_bytes());
    object
}

fn clip(kind: Kind, length: i64, id: MobId, slot: u32, source_in: i64) -> Object {
    let mut object = component(SOURCE_CLIP, kind, length);
    object.props.data(0x1101, id);
    object.props.data(0x1102, slot.to_le_bytes());
    object.props.data(0x1201, source_in.to_le_bytes());
    object
}

fn sequence(kind: Kind, length: i64, parts: Vec<Object>) -> Result<Object> {
    let mut object = component(SEQUENCE, kind, length);
    object.vector(0x1001, "Components", parts)?;
    Ok(object)
}

fn slot(
    id: u32,
    name: &str,
    rate: &Clock,
    physical: Option<u32>,
    segment: Object,
) -> Result<Object> {
    let mut slot = Object::new(0x3b);
    slot.props.data(SLOT_ID, id.to_le_bytes());
    slot.props.string(SLOT_NAME, name);
    slot.props.data(EDIT_RATE, rate_bytes(rate)?);
    slot.props.data(0x4b02, 0i64.to_le_bytes());
    if let Some(number) = physical {
        slot.props.data(PHYSICAL, number.to_le_bytes());
    }
    slot.object(SEGMENT, "Segment", segment);
    Ok(slot)
}

fn timecode(length: i64, code: &Timecode) -> Result<Object> {
    ensure!(code.start >= 0 && code.fps > 0, "Invalid AAF timecode");
    let mut object = component(TIMECODE, Kind::Timecode, length);
    object.props.data(0x1501, code.start.to_le_bytes());
    object.props.data(0x1502, code.fps.to_le_bytes());
    object.props.data(0x1503, [u8::from(code.drop)]);
    Ok(object)
}

fn new_id() -> MobId {
    let mut id = [0; 32];
    // Basic UMID with random material number and instance zero.
    id[..16].copy_from_slice(&[6, 10, 43, 52, 1, 1, 1, 5, 1, 1, 15, 32, 19, 0, 0, 0]);
    id[16..].copy_from_slice(&Uuid::new_v4().to_bytes_le());
    id
}

fn mob(class: u8, id: MobId, name: &str, timestamp: &[u8], slots: Vec<Object>) -> Result<Object> {
    let mut object = Object::new(class);
    object.props.data(MOB_ID, id);
    object
        .props
        .string(MOB_NAME, if name.is_empty() { "Mob" } else { name });
    object.props.data(0x4404, timestamp);
    object.props.data(0x4405, timestamp);
    object.vector(MOB_SLOTS, "Slots", slots)?;
    Ok(object)
}

fn descriptor(class: u8, rate: &Clock, length: i64, url: &str) -> Result<Object> {
    let mut object = Object::new(class);
    object.props.data(SAMPLE_RATE, rate_bytes(rate)?);
    object.props.data(FILE_LENGTH, length.to_le_bytes());
    let mut locator = Object::new(0x32);
    locator.props.string(URL, url);
    object.vector(LOCATORS, "Locator", vec![locator])?;
    Ok(object)
}

fn wave_summary(path: &Path, rate: i32, frames: i64) -> Result<Vec<u8>> {
    let mut stream = BufReader::new(File::open(path)?);
    let mut header = [0; 12];
    stream.read_exact(&mut header)?;
    ensure!(
        &header[..4] == b"RIFF" && &header[8..] == b"WAVE",
        "AAF stem must be a RIFF WAVE file"
    );
    let limit = u64::from(u32::from_le_bytes(bytes(&header[4..8])?)) + 8;
    ensure!(
        limit <= stream.get_ref().metadata()?.len(),
        "Truncated WAV container"
    );
    let mut format = None;
    let mut data_size = None;
    while stream.stream_position()? + 8 <= limit {
        let mut chunk = [0; 8];
        stream.read_exact(&mut chunk)?;
        let length = u32::from_le_bytes(bytes(&chunk[4..])?);
        let end = stream.stream_position()? + u64::from(length);
        ensure!(end <= limit, "Truncated WAV chunk");
        match &chunk[..4] {
            b"fmt " => {
                ensure!(
                    format.is_none() && (16..=65_507).contains(&length),
                    "Invalid WAV format chunk"
                );
                let mut payload = vec![0; length as usize];
                stream.read_exact(&mut payload)?;
                format = Some((chunk, payload));
            }
            b"data" => {
                ensure!(
                    data_size.replace(length).is_none(),
                    "Multiple WAV data chunks"
                );
            }
            _ => {}
        }
        stream.seek(SeekFrom::Start(end + u64::from(length % 2)))?;
    }
    let (chunk, format) = format.context("WAV is missing format")?;
    let data = data_size.context("WAV is missing audio data")?;
    let encoding = u16::from_le_bytes(bytes(&format[..2])?);
    let channels = u16::from_le_bytes(bytes(&format[2..4])?);
    let actual = u32::from_le_bytes(bytes(&format[4..8])?);
    let byte_rate = u32::from_le_bytes(bytes(&format[8..12])?);
    let align = u16::from_le_bytes(bytes(&format[12..14])?);
    let bits = u16::from_le_bytes(bytes(&format[14..16])?);
    ensure!(
        (encoding == 1 || encoding == 3) && channels == 1 && actual == rate as u32,
        "WAV encoding/channels/rate differ from AAF manifest"
    );
    ensure!(
        matches!(bits, 16 | 24 | 32) && (encoding != 3 || bits == 32),
        "Unsupported WAV sample format"
    );
    ensure!(
        align == bits / 8 && actual.checked_mul(u32::from(align)) == Some(byte_rate),
        "Invalid WAV block alignment"
    );
    ensure!(
        data.is_multiple_of(u32::from(align)) && i64::from(data / u32::from(align)) == frames,
        "WAV frame count differs from AAF manifest"
    );
    Ok([header.to_vec(), chunk.to_vec(), format].concat())
}

fn parse_rate(input: &str) -> Result<Clock> {
    let (numerator, denominator) = input.split_once('/').unwrap_or((input, "1"));
    Rate {
        numerator: numerator.parse()?,
        denominator: denominator.parse()?,
    }
    .clock()
}

fn compression(codec: &str, profile: Option<&str>) -> Uuid {
    match (codec, profile) {
        ("mjpeg", _) => uuid::uuid!("0e040201-0205-0100-060e-2b3404010101"),
        ("h264", Some("Baseline")) => uuid::uuid!("04010202-0131-1001-060e-2b340401010d"),
        ("h264", Some("Constrained Baseline")) => {
            uuid::uuid!("04010202-0131-1101-060e-2b340401010d")
        }
        ("h264", Some("Main")) => uuid::uuid!("04010202-0131-2001-060e-2b340401010d"),
        ("h264", Some("Extended")) => uuid::uuid!("04010202-0131-3001-060e-2b340401010d"),
        ("h264", Some("High")) => uuid::uuid!("04010202-0131-4001-060e-2b340401010d"),
        ("h264", Some("High 10")) => uuid::uuid!("04010202-0131-5001-060e-2b340401010d"),
        ("h264", Some("High 4:2:2")) => uuid::uuid!("04010202-0131-6001-060e-2b340401010d"),
        ("h264", Some("High 10 Intra")) => uuid::uuid!("04010202-0132-2001-060e-2b340401010a"),
        ("h264", Some("High 4:2:2 Intra")) => uuid::uuid!("04010202-0132-3001-060e-2b340401010a"),
        ("h264", Some("High 4:4:4 Predictive")) => {
            uuid::uuid!("04010202-0131-7001-060e-2b340401010d")
        }
        _ => uuid::uuid!("04010202-0000-0000-060e-2b3404010101"),
    }
}

#[derive(Clone)]
struct SourceTrack {
    kind: Kind,
    rate: Clock,
    length: i64,
    channel: Option<u32>,
}

struct Link {
    master: MobId,
    tracks: Vec<SourceTrack>,
    tape: usize,
}

struct Mob {
    id: MobId,
    object: Object,
}

struct Graph {
    mobs: Vec<Mob>,
    timestamp: Vec<u8>,
}

impl Graph {
    fn add(&mut self, id: MobId, object: Object) -> usize {
        let position = self.mobs.len();
        self.mobs.push(Mob { id, object });
        position
    }

    fn link(
        &mut self,
        path: &Path,
        tracks: Vec<SourceTrack>,
        descriptor: Object,
        codec: Option<&str>,
        code: Option<&Timecode>,
    ) -> Result<Link> {
        let name = path
            .file_stem()
            .and_then(|name| name.to_str())
            .context("Invalid AAF media name")?;
        let tape_id = new_id();
        let source_id = new_id();
        let master = new_id();
        let source_slots = |target: MobId, tape: bool| -> Result<Vec<Object>> {
            tracks
                .iter()
                .enumerate()
                .map(|(index, track)| {
                    let id = u32::try_from(index + 1)?;
                    let edit = clip(
                        track.kind,
                        track.length,
                        target,
                        if tape { 0 } else { id },
                        0,
                    );
                    let segment =
                        sequence(track.kind, if tape { track.length } else { 0 }, vec![edit])?;
                    slot(
                        id,
                        "",
                        &track.rate,
                        if tape { None } else { track.channel },
                        segment,
                    )
                })
                .collect()
        };
        let mut tape_slots = source_slots([0; 32], true)?;
        if let Some(code) = code {
            let picture = tracks
                .iter()
                .find(|track| track.kind == Kind::Picture)
                .context("No AAF picture clock")?;
            tape_slots.push(slot(
                u32::try_from(tape_slots.len() + 1)?,
                "Timecode",
                &picture.rate,
                None,
                timecode(picture.length, code)?,
            )?);
        }
        let mut tape = mob(
            SOURCE_MOB,
            tape_id,
            &format!("{name} <TAPE MOB>"),
            &self.timestamp,
            tape_slots,
        )?;
        tape.object(DESCRIPTOR, "EssenceDescription", Object::new(0x4a));
        let mut source = mob(
            SOURCE_MOB,
            source_id,
            &format!("{name} <SOURCE MOB>"),
            &self.timestamp,
            source_slots(tape_id, false)?,
        )?;
        source.object(DESCRIPTOR, "EssenceDescription", descriptor);
        if let Some(codec) = codec {
            let mut tag = Object::new(0x3f);
            tag.props.string(0x5001, "Video");
            let mut value = vec![0x4c];
            value.extend(uuid::uuid!("01100200-0000-0000-060e-2b3401040101").to_bytes_le());
            value.extend(utf16(codec));
            tag.props.data(0x5003, value);
            source.vector(0x4406, "UserComments", vec![tag])?;
        }
        let master_mob = mob(
            MASTER_MOB,
            master,
            name,
            &self.timestamp,
            source_slots(source_id, false)?,
        )?;
        let tape = self.add(tape_id, tape);
        self.add(source_id, source);
        self.add(master, master_mob);
        Ok(Link {
            master,
            tracks,
            tape,
        })
    }

    fn wave(&mut self, path: &Path, rate: i32, frames: i64) -> Result<Link> {
        let summary = wave_summary(path, rate, frames)?;
        let rate = integer(i64::from(rate));
        let mut desc = descriptor(0x2c, &rate, frames, &file_url(path)?)?;
        desc.props.data(0x3801, summary);
        desc.props.set(
            0x3004,
            WEAK,
            weak(3, uuid::uuid!("4313b571-d8ba-11d2-809b-006008143e6f")),
        );
        self.link(
            path,
            vec![SourceTrack {
                kind: Kind::Sound,
                rate,
                length: frames,
                channel: Some(1),
            }],
            desc,
            None,
            None,
        )
    }

    fn camera(
        &mut self,
        path: &Path,
        metadata: &Metadata,
        code: Option<&Timecode>,
    ) -> Result<Link> {
        ensure!(
            !path
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("mxf")),
            "MXF AAF export requires the bundled Python AAF module"
        );
        ensure!(
            metadata
                .format
                .as_ref()
                .is_some_and(|format| format.format_long_name == "QuickTime / MOV"),
            "Unsupported AAF camera container"
        );
        let url = file_url(path)?;
        let mut descriptors = Vec::new();
        let mut tracks = Vec::new();
        let mut codec = None;
        for stream in &metadata.streams {
            match stream {
                MediaStream::Picture {
                    avg_frame_rate,
                    nb_frames,
                    width,
                    height,
                    codec_name,
                    profile,
                    pix_fmt,
                } => {
                    let rate = parse_rate(avg_frame_rate)?;
                    ensure!(
                        *nb_frames > 0 && *width > 0 && *height > 0,
                        "Invalid AAF camera picture metadata"
                    );
                    let mut desc = descriptor(0x28, &rate, *nb_frames, &url)?;
                    desc.props.data(
                        0x3201,
                        compression(codec_name, profile.as_deref()).to_bytes_le(),
                    );
                    desc.props.data(0x3202, height.to_le_bytes());
                    desc.props.data(0x3203, width.to_le_bytes());
                    desc.props.data(0x320c, [0]);
                    desc.props.data(0x320d, [0; 8]);
                    desc.props.data(
                        0x320e,
                        rate_bytes(&Clock::new((*width).into(), (*height).into()))?,
                    );
                    let depth = [8u32, 10, 12, 16]
                        .into_iter()
                        .find(|depth| pix_fmt.contains(&format!("p{depth}")))
                        .unwrap_or(8);
                    let (h, v): (u32, u32) = if pix_fmt.contains("422") {
                        (2, 1)
                    } else if pix_fmt.contains("444") {
                        (1, 1)
                    } else {
                        (2, 2)
                    };
                    desc.props.data(0x3301, depth.to_le_bytes());
                    desc.props.data(0x3302, h.to_le_bytes());
                    desc.props.data(0x3308, v.to_le_bytes());
                    descriptors.push(desc);
                    if codec.is_none() {
                        codec = Some(codec_name.as_str());
                        tracks.insert(
                            0,
                            SourceTrack {
                                kind: Kind::Picture,
                                rate,
                                length: *nb_frames,
                                channel: None,
                            },
                        );
                    }
                }
                MediaStream::Sound {
                    sample_rate,
                    duration_ts,
                    channels,
                    bit_rate,
                    sample_fmt,
                } => {
                    let rate = parse_rate(sample_rate)?;
                    ensure!(
                        *duration_ts > 0 && (1..=64).contains(channels),
                        "Invalid AAF camera sound metadata"
                    );
                    let mut desc = descriptor(PCM, &rate, *duration_ts, &url)?;
                    desc.props.data(0x3d03, rate_bytes(&rate)?);
                    desc.props.data(CHANNELS, channels.to_le_bytes());
                    desc.props
                        .data(0x3d09, bit_rate.parse::<u32>()?.to_le_bytes());
                    let (bits, align): (u32, u16) = match sample_fmt.as_str() {
                        "u8p" => (8, 1),
                        "s16" | "s16p" => (16, 2),
                        "s32" | "s32p" => (32, 4),
                        "s64" | "s64p" => (64, 8),
                        "flt" | "fltp" => (32, 4),
                        "dbl" | "dblp" => (64, 8),
                        _ => (0, 0),
                    };
                    desc.props.data(BITS, bits.to_le_bytes());
                    desc.props.data(0x3d0a, align.to_le_bytes());
                    desc.props.data(
                        0x3d06,
                        uuid::uuid!("04020202-0000-0000-060e-2b3404010101").to_bytes_le(),
                    );
                    descriptors.push(desc);
                    if tracks.iter().all(|track| track.kind != Kind::Sound) {
                        for channel in 1..=*channels {
                            tracks.push(SourceTrack {
                                kind: Kind::Sound,
                                rate: rate.clone(),
                                length: *duration_ts,
                                channel: Some(channel),
                            });
                        }
                    }
                }
                MediaStream::Other => {}
            }
        }
        let first = tracks
            .first()
            .context("AAF camera has no picture or sound streams")?;
        let desc = if descriptors.len() == 1 {
            descriptors
                .pop()
                .context("AAF camera descriptor is missing")?
        } else {
            let mut desc = descriptor(0x44, &first.rate, first.length, &url)?;
            desc.props.data(
                0xff8d,
                uuid::uuid!("b22697a2-3442-44e8-bb8f-7a1cd290ebf1").to_bytes_le(),
            );
            desc.vector(0x3f01, "FileDescriptors", descriptors)?;
            desc
        };
        self.link(path, tracks, desc, codec, code)
    }

    fn camera_link<'a>(
        &mut self,
        links: &'a mut HashMap<PathBuf, Link>,
        path: PathBuf,
        metadata: &Metadata,
        code: Option<&Timecode>,
    ) -> Result<&'a Link> {
        use std::collections::hash_map::Entry;
        match links.entry(path) {
            Entry::Vacant(entry) => {
                let link = self.camera(entry.key(), metadata, code)?;
                Ok(entry.insert(link))
            }
            Entry::Occupied(entry) => {
                if let Some(code) = code {
                    // A clip that shares the file may supply its tape timecode
                    // after that file's first appearance.
                    let link = entry.get();
                    let tape = &mut self.mobs[link.tape].object;
                    if tape
                        .children
                        .iter()
                        .filter(|(_, object)| object.class == 0x3b)
                        .count()
                        == link.tracks.len()
                    {
                        let track = link
                            .tracks
                            .iter()
                            .find(|track| track.kind == Kind::Picture)
                            .context("No AAF picture clock")?;
                        let index = u32::try_from(link.tracks.len())?;
                        tape.children.push((
                            format!("Slots-4403{{{index:x}}}"),
                            slot(
                                index + 1,
                                "Timecode",
                                &track.rate,
                                None,
                                timecode(track.length, code)?,
                            )?,
                        ));
                        let mut bytes = index_header(index + 1);
                        for key in 0..=index {
                            bytes.extend(key.to_le_bytes());
                        }
                        tape.streams[0].1 = bytes;
                    }
                }
                Ok(entry.into_mut())
            }
        }
    }
}

#[derive(Clone, Copy)]
enum Round {
    Nearest,
    Up,
    Down,
}

fn units(value: i64, from: &Clock, to: &Clock, round: Round) -> Result<i64> {
    let exact = integer(value) * to / from;
    let ticks = match round {
        Round::Nearest => (exact + Clock::new(1.into(), 2.into())).floor(),
        Round::Up => exact.ceil(),
        Round::Down => exact.floor(),
    };
    int_ticks(&ticks)
}

fn int_ticks(value: &Clock) -> Result<i64> {
    value.to_i64().context("AAF edit unit overflow")
}

struct Edit {
    start: i64,
    end: i64,
    source_in: i64,
    master: MobId,
    slot_id: u32,
}

fn edit_sequence(kind: Kind, edits: Vec<Edit>, length: i64, below: bool) -> Result<Object> {
    let mut parts = Vec::new();
    let mut cursor = 0;
    let gap = |length| {
        let mut part = component(if below { SCOPE_REF } else { FILLER }, kind, length);
        if below {
            part.props.data(0x0e01, 0u32.to_le_bytes());
            part.props.data(0x0e02, 1u32.to_le_bytes());
        }
        part
    };
    for edit in edits {
        if edit.start > cursor {
            parts.push(gap(edit.start - cursor));
        }
        parts.push(clip(
            kind,
            edit.end - edit.start,
            edit.master,
            edit.slot_id,
            edit.source_in,
        ));
        cursor = edit.end;
    }
    if length > cursor {
        parts.push(gap(length - cursor));
    }
    sequence(kind, length, parts)
}

impl Graph {
    fn sound(
        &mut self,
        track: Track<AudioClip>,
        clock: Option<&Clock>,
        links: &mut HashMap<PathBuf, Link>,
        id: u32,
    ) -> Result<Object> {
        let rate = track
            .sample_rate
            .filter(|rate| *rate > 0)
            .context("Invalid AAF sample rate")?;
        let native = integer(i64::from(rate));
        let clock = clock.unwrap_or(&native);
        let mut edits = Vec::new();
        let mut cursor = 0;
        let mut previous_end = 0;
        for clip in track.clips {
            let range = clip.range;
            range.check(cursor, clip.source_frames)?;
            let path = absolute(&range.path)?;
            let (master, slot_id, available) = if let Some(channel) = clip.channel {
                let metadata = clip
                    .metadata
                    .as_ref()
                    .context("AAF camera sound requires metadata")?;
                let link = self.camera_link(links, path, metadata, None)?;
                let channel = channel
                    .checked_add(1)
                    .context("Invalid AAF camera channel")?;
                let (index, source) = link
                    .tracks
                    .iter()
                    .enumerate()
                    .find(|(_, source)| {
                        source.kind == Kind::Sound
                            && source.channel == Some(channel)
                            && source.rate == native
                    })
                    .context("AAF camera sound has no such channel")?;
                (link.master, u32::try_from(index + 1)?, source.length)
            } else {
                ensure!(
                    clip.channels == 1,
                    "AAF audio requires one lossless mono stem per channel"
                );
                (
                    self.wave(&path, rate, clip.source_frames)?.master,
                    1,
                    clip.source_frames,
                )
            };
            if range.source_in >= available {
                continue;
            }
            let span = range.length.min(available - range.source_in);
            cursor = range.start + span;
            let start = units(range.start, &native, clock, Round::Nearest)?;
            let source_in = units(range.source_in, &native, clock, Round::Nearest)?;
            let file_end = units(available, &native, clock, Round::Down)?;
            let max_end = start
                .checked_add(file_end - source_in)
                .context("AAF sound edit overflow")?;
            let end = units(cursor, &native, clock, Round::Up)?.min(max_end);
            ensure!(
                start >= previous_end && end > start,
                "AAF sound clip does not fit its frames"
            );
            edits.push(Edit {
                start,
                end,
                source_in,
                master,
                slot_id,
            });
            previous_end = end;
        }
        slot(
            id,
            &track.name,
            clock,
            None,
            edit_sequence(Kind::Sound, edits, previous_end, false)?,
        )
    }

    fn picture(
        &mut self,
        tracks: Vec<Track<PictureClip>>,
        clock: &Clock,
        links: &mut HashMap<PathBuf, Link>,
        id: u32,
    ) -> Result<Object> {
        let mut layers = Vec::new();
        let mut duration = 0;
        for track in tracks {
            let native = track
                .edit_rate
                .context("Missing AAF picture rate")?
                .clock()?;
            let mut edits = Vec::new();
            let mut cursor = 0;
            for clip in track.clips {
                let range = clip.range;
                let path = absolute(&range.path)?;
                let link = self.camera_link(links, path, &clip.metadata, clip.timecode.as_ref())?;
                let (index, source) = link
                    .tracks
                    .iter()
                    .enumerate()
                    .find(|(_, source)| source.kind == Kind::Picture)
                    .context("AAF picture requires one video stream")?;
                ensure!(
                    source.rate == native,
                    "AAF picture rate differs from its media"
                );
                cursor = range.check(cursor, source.length)?;
                let start = units(range.start, &native, clock, Round::Nearest)?;
                let source_in = units(range.source_in, &native, clock, Round::Nearest)?;
                let file_end = units(source.length, &native, clock, Round::Down)?;
                let max_end = start
                    .checked_add(file_end - source_in)
                    .context("AAF picture edit overflow")?;
                let end = units(cursor, &native, clock, Round::Nearest)?.min(max_end);
                ensure!(
                    end > start,
                    "AAF picture clip is shorter than a composition frame"
                );
                edits.push(Edit {
                    start,
                    end,
                    source_in,
                    master: link.master,
                    slot_id: u32::try_from(index + 1)?,
                });
            }
            let end = edits.last().context("Empty AAF picture track")?.end;
            duration = duration.max(end);
            layers.push(edits);
        }
        let kind = Kind::Picture;
        let segment = if layers.len() == 1 {
            edit_sequence(
                kind,
                layers.pop().context("Missing AAF picture track")?,
                duration,
                false,
            )?
        } else {
            let mut stack = component(NESTED_SCOPE, kind, duration);
            let slots = layers
                .into_iter()
                .enumerate()
                .map(|(i, edits)| edit_sequence(kind, edits, duration, i > 0))
                .collect::<Result<_>>()?;
            stack.vector(0x0c01, "Slots", slots)?;
            stack
        };
        slot(id, "V1", clock, Some(1), segment)
    }

    fn composition(&mut self, mut doc: Document) -> Result<()> {
        ensure!(
            matches!(doc.version, 1 | 2),
            "Unsupported AAF bridge protocol"
        );
        if doc.version == 1 {
            doc.picture_tracks.clear();
        }
        ensure!(
            !doc.tracks.is_empty() || !doc.picture_tracks.is_empty(),
            "No AAF tracks"
        );
        let id = new_id();
        let position = self.mobs.len();
        let mut links = HashMap::new();
        let picture_clock = doc.picture_tracks.first().and_then(|track| track.edit_rate);
        let clock = doc
            .composition_edit_rate
            .or(picture_clock)
            .map(Rate::clock)
            .transpose()?;
        let mut slots = Vec::new();
        if let Some(clock) = &clock {
            let mut duration = Clock::zero();
            for track in &doc.picture_tracks {
                let rate = track
                    .edit_rate
                    .context("Missing AAF picture edit rate")?
                    .clock()?;
                for clip in &track.clips {
                    duration = duration.max(integer(clip.range.end()?) / &rate);
                }
            }
            for track in &doc.tracks {
                let rate = track
                    .sample_rate
                    .filter(|rate| *rate > 0)
                    .context("Invalid AAF sample rate")?;
                for clip in &track.clips {
                    duration = duration.max(integer(clip.range.end()?) / integer(i64::from(rate)));
                }
            }
            let code = Timecode {
                fps: clock
                    .round()
                    .to_u16()
                    .context("Invalid AAF timecode frame rate")?,
                drop: doc.composition_drop_frame,
                start: 0,
            };
            slots.push(slot(
                1,
                "Timecode",
                clock,
                Some(1),
                timecode(int_ticks(&(duration * clock).ceil())?, &code)?,
            )?);
        }
        if !doc.picture_tracks.is_empty() {
            let clock = clock.as_ref().context("Missing AAF composition rate")?;
            slots.push(self.picture(
                doc.picture_tracks,
                clock,
                &mut links,
                u32::try_from(slots.len() + 1)?,
            )?);
        }
        for track in doc.tracks {
            slots.push(self.sound(
                track,
                clock.as_ref(),
                &mut links,
                u32::try_from(slots.len() + 1)?,
            )?);
        }
        let mut composition = mob(COMPOSITION, id, &doc.name, &self.timestamp, slots)?;
        composition.props.data(USAGE, TOP_LEVEL.to_bytes_le());
        self.mobs.insert(
            position,
            Mob {
                id,
                object: composition,
            },
        );
        Ok(())
    }
}

pub fn write(manifest: &Path, destination: &Path) -> Result<Value> {
    let mut input: Value = serde_json::from_reader(BufReader::new(File::open(manifest)?))?;
    let documents = if input["version"] == 3 {
        serde_json::from_value::<Vec<Document>>(input["sequences"].take())?
    } else {
        vec![serde_json::from_value::<Document>(input)?]
    };
    ensure!(!documents.is_empty(), "AAF batch has no sequences");
    let path = staged(destination, |temporary| {
        // The dictionary has no media or timelines. Only the serialized AAF
        // type registry and standard definitions are reused at runtime.
        io::copy(
            &mut GzDecoder::new(include_bytes!("../dictionary.aaf.gz").as_slice()),
            temporary,
        )?;
        temporary.rewind()?;
        let mut file = CompoundFile::open(temporary)?;
        let header_path = Path::new("/Header-2");
        let header = Props::load(&mut file, header_path)?;
        let ident_path = header
            .vector(&mut file, header_path, 0x3b06)?
            .pop()
            .context("Missing AAF identification")?;
        let mut ident = Props::load(&mut file, &ident_path)?;
        ident.string(0x3c04, env!("CARGO_PKG_VERSION"));
        ident.string(0x3c08, &crate::platform());
        ident.data(0x3c09, Uuid::new_v4().to_bytes_le());
        ident.save(&mut file, &ident_path)?;
        let mut graph = Graph {
            mobs: Vec::new(),
            timestamp: header.require(0x3b02)?.to_vec(),
        };
        for document in documents {
            graph.composition(document)?;
        }
        let content = content_path(&mut file)?;
        let mut props = Props::load(&mut file, &content)?;
        let name = "Mobs-1901";
        props.set(0x1901, SET, utf16(name));
        props.save(&mut file, &content)?;
        let mut index = index_header(u32::try_from(graph.mobs.len())?);
        index.extend(MOB_ID.to_le_bytes());
        index.push(32);
        for (i, mob) in graph.mobs.into_iter().enumerate() {
            index.extend((i as u32).to_le_bytes());
            index.extend(1u32.to_le_bytes());
            index.extend(mob.id);
            mob.object
                .write(&mut file, &content.join(format!("{name}{{{i:x}}}")))?;
        }
        let mut stream = file.create_stream(content.join(format!("{name} index")))?;
        stream.write_all(&index)?;
        stream.flush()?;
        drop(stream);
        file.flush()?;
        Ok(())
    })?;
    Ok(json!({"version": 1, "path": path}))
}
