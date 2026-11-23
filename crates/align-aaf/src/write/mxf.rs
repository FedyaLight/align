//! Read the header metadata of a single, clip-wrapped VC-3 OP-Atom picture.
//! ST 377-1 defines the KLV and local-set encoding; ST 390 defines OP-Atom.
use super::{MobId, Timecode};
use crate::{Clock, Rate, storage::Props};
use anyhow::{Context, Result, ensure};
use std::{
    collections::HashMap,
    fs::File,
    io::{BufReader, Read, Seek, SeekFrom},
    path::Path,
};
use uuid::Uuid;

const HEADER: [u8; 16] = pack_key(0x0204);
const PRIMER: [u8; 16] = pack_key(0x0501);
const SET_KEY: [u8; 16] = [6, 14, 43, 52, 2, 0x53, 1, 1, 13, 1, 1, 1, 1, 1, 0, 0];
const PICTURE: [u8; 7] = [1, 3, 2, 2, 1, 0, 0];
const TIMECODE: [u8; 7] = [1, 3, 2, 1, 1, 0, 0];
const MAX_HEADER: u64 = 16 * 1024 * 1024;

const fn pack_key(kind: u16) -> [u8; 16] {
    let [high, low] = kind.to_be_bytes();
    [6, 14, 43, 52, 2, 5, 1, 1, 13, 1, 2, 1, 1, high, low, 0]
}

pub(super) struct Picture {
    pub material: MobId,
    pub file: MobId,
    pub rate: Clock,
    pub length: i64,
    pub timecode: Option<Timecode>,
    pub cdci: Props,
}

fn take<const N: usize>(data: &mut &[u8]) -> Result<[u8; N]> {
    let (head, tail) = data.split_at_checked(N).context("Truncated MXF metadata")?;
    *data = tail;
    Ok(head.try_into()?)
}

fn klv(input: &mut impl Read) -> Result<([u8; 16], u64)> {
    let mut key = [0; 16];
    input.read_exact(&mut key)?;
    let mut lead = [0];
    input.read_exact(&mut lead)?;
    if lead[0] < 0x80 {
        return Ok((key, lead[0].into()));
    }
    let count = usize::from(lead[0] & 0x7f);
    ensure!((1..=8).contains(&count), "Invalid MXF KLV length");
    let mut length = [0; 8];
    input.read_exact(&mut length[8 - count..])?;
    Ok((key, u64::from_be_bytes(length)))
}

struct Set<'a> {
    class: u8,
    tags: HashMap<u16, &'a [u8]>,
}

impl<'a> Set<'a> {
    fn read(class: u8, mut data: &'a [u8]) -> Result<Self> {
        let mut tags = HashMap::new();
        while !data.is_empty() {
            let tag = u16::from_be_bytes(take(&mut data)?);
            let size = usize::from(u16::from_be_bytes(take(&mut data)?));
            let (value, rest) = data
                .split_at_checked(size)
                .context("Truncated MXF local set")?;
            ensure!(tags.insert(tag, value).is_none(), "Duplicate MXF local tag");
            data = rest;
        }
        Ok(Self { class, tags })
    }

    fn data(&self, tag: u16) -> Result<&'a [u8]> {
        self.tags
            .get(&tag)
            .copied()
            .with_context(|| format!("Missing MXF tag {tag:04x}"))
    }

    fn bytes<const N: usize>(&self, tag: u16) -> Result<[u8; N]> {
        self.data(tag)?
            .try_into()
            .with_context(|| format!("Invalid MXF tag {tag:04x}"))
    }

    fn uint(&self, tag: u16) -> Result<u32> {
        Ok(u32::from_be_bytes(self.bytes(tag)?))
    }

    fn int(&self, tag: u16) -> Result<i64> {
        Ok(i64::from_be_bytes(self.bytes(tag)?))
    }

    fn rate(&self, tag: u16) -> Result<Clock> {
        let mut data = self.data(tag)?;
        let numerator = i32::from_be_bytes(take(&mut data)?);
        let denominator = i32::from_be_bytes(take(&mut data)?);
        ensure!(data.is_empty(), "Invalid MXF rational");
        Rate {
            numerator,
            denominator,
        }
        .clock()
    }

    fn refs(&self, tag: u16) -> Result<impl Iterator<Item = [u8; 16]> + 'a> {
        let mut data = self.data(tag)?;
        let count = u32::from_be_bytes(take(&mut data)?) as usize;
        let size = u32::from_be_bytes(take(&mut data)?);
        let chunks = data.as_chunks::<16>();
        ensure!(
            size == 16 && chunks.1.is_empty() && chunks.0.len() == count,
            "Invalid MXF strong reference batch"
        );
        Ok(chunks.0.iter().copied())
    }

    fn is_kind(&self, kind: [u8; 7]) -> Result<bool> {
        let label: [u8; 16] = self.bytes(0x0201)?;
        Ok(label[8..15] == kind)
    }
}

struct Header<'a>(HashMap<[u8; 16], Set<'a>>);

impl<'a> Header<'a> {
    fn parse(data: &'a [u8]) -> Result<Self> {
        let mut sets = HashMap::new();
        let mut cursor = std::io::Cursor::new(data);
        while cursor.position() < data.len() as u64 {
            let (key, len) = klv(&mut cursor)?;
            let start = cursor.position();
            let end = start
                .checked_add(len)
                .filter(|end| *end <= data.len() as u64)
                .context("Truncated MXF header metadata")?;
            cursor.set_position(end);
            if key[..14] == SET_KEY[..14]
                && matches!(
                    key[14],
                    0x2f | 0x18 | 0x23 | 0x36 | 0x37 | 0x3b | 0x0f | 0x11 | 0x14 | 0x28
                )
            {
                ensure!(sets.len() < 4096, "Too many MXF header sets");
                let set = Set::read(key[14], &data[start as usize..end as usize])?;
                ensure!(
                    sets.insert(set.bytes(0x3c0a)?, set).is_none(),
                    "Duplicate MXF instance UID"
                );
            }
        }
        Ok(Self(sets))
    }

    fn get(&self, id: [u8; 16]) -> Result<&Set<'a>> {
        self.0.get(&id).context("Missing MXF header reference")
    }

    fn tracks(&self, package: &Set<'a>) -> Result<Vec<(&Set<'a>, &Set<'a>)>> {
        let mut tracks = Vec::new();
        for id in package.refs(0x4403)? {
            let track = self.get(id)?;
            ensure!(track.class == 0x3b, "Unsupported MXF package track");
            tracks.push((track, self.get(track.bytes(0x4803)?)?));
        }
        Ok(tracks)
    }

    fn component<'s>(&'s self, segment: &'s Set<'a>, class: u8) -> Result<&'s Set<'a>> {
        if segment.class == class {
            return Ok(segment);
        }
        ensure!(segment.class == 0x0f, "Unsupported MXF track segment");
        let mut refs = segment.refs(0x1001)?;
        let part = self.get(refs.next().context("Empty MXF sequence")?)?;
        ensure!(
            refs.next().is_none()
                && part.class == class
                && part.int(0x0202)? == segment.int(0x0202)?,
            "Unsupported MXF track sequence"
        );
        Ok(part)
    }

    fn timecode(&self, tracks: &[(&Set<'a>, &Set<'a>)], rate: &Clock) -> Result<Option<Timecode>> {
        for (track, segment) in tracks {
            if segment.is_kind(TIMECODE)? {
                ensure!(
                    track.rate(0x4b01)? == *rate && track.int(0x4b02)? == 0,
                    "Unsupported MXF timecode track clock"
                );
                let part = self.component(segment, 0x14)?;
                return Ok(Some(Timecode {
                    start: part.int(0x1501)?,
                    fps: u16::from_be_bytes(part.bytes(0x1502)?),
                    drop: part.bytes::<1>(0x1503)? == [1],
                }));
            }
        }
        Ok(None)
    }
}

fn picture_track<'s, 'a>(
    tracks: &[(&'s Set<'a>, &'s Set<'a>)],
) -> Result<(&'s Set<'a>, &'s Set<'a>)> {
    let mut picture = None;
    for &(track, segment) in tracks {
        if segment.is_kind(PICTURE)? {
            ensure!(
                picture.replace((track, segment)).is_none(),
                "MXF has multiple picture tracks"
            );
        }
    }
    picture.context("MXF OP-Atom has no picture track")
}

fn mob_id(mut umid: MobId) -> Result<MobId> {
    ensure!(
        umid.starts_with(&[6, 10, 43, 52]) && umid[12] == 0x13,
        "Invalid MXF basic UMID"
    );
    // The UMID label is bytes; its material number is an AAF AUID record.
    let material = Uuid::from_slice(&umid[16..])?.to_bytes_le();
    umid[16..].copy_from_slice(&material);
    Ok(umid)
}

fn read_header(path: &Path) -> Result<Vec<u8>> {
    let file = File::open(path)?;
    let file_len = file.metadata()?.len();
    let mut input = BufReader::new(file);
    let (key, len) = klv(&mut input)?;
    ensure!(
        key == HEADER && len >= 88,
        "Expected a closed, complete MXF header partition"
    );
    let mut pack = [0; 88];
    input.read_exact(&mut pack)?;
    let op = &pack[64..80];
    ensure!(
        op[..7] == [6, 14, 43, 52, 4, 1, 1] && op[8..13] == [13, 1, 2, 1, 0x10],
        "Unsupported MXF operational pattern: expected OP-Atom"
    );
    let size = u64::from_be_bytes(pack[32..40].try_into()?);
    ensure!(
        (1..=MAX_HEADER).contains(&size),
        "Invalid MXF header byte count"
    );
    let mut position = input
        .stream_position()?
        .checked_add(len - 88)
        .filter(|end| *end <= file_len)
        .context("Truncated MXF header partition")?;
    loop {
        input.seek(SeekFrom::Start(position))?;
        let (key, len) = klv(&mut input)?;
        if key == PRIMER {
            break;
        }
        // ST 377-1 excludes any leading KLV Fill from HeaderByteCount.
        ensure!(
            key[..7] == [6, 14, 43, 52, 1, 1, 1] && key[8..] == [3, 1, 2, 16, 1, 0, 0, 0],
            "MXF header has no primer pack"
        );
        position = input
            .stream_position()?
            .checked_add(len)
            .filter(|end| *end <= file_len && *end <= MAX_HEADER)
            .context("Invalid MXF header fill")?;
    }
    ensure!(size <= file_len - position, "Truncated MXF header");
    input.seek(SeekFrom::Start(position))?;
    let mut data = vec![0; size as usize];
    input.read_exact(&mut data)?;
    Ok(data)
}

pub(super) fn read(path: &Path) -> Result<Picture> {
    let data = read_header(path)?;
    let header = Header::parse(&data)?;
    let mut prefaces = header.0.values().filter(|set| set.class == 0x2f);
    let preface = prefaces.next().context("Missing MXF Preface")?;
    ensure!(prefaces.next().is_none(), "Multiple MXF Prefaces");
    let storage = header.get(preface.bytes(0x3b03)?)?;
    ensure!(storage.class == 0x18, "Invalid MXF ContentStorage");
    let mut essence_refs = storage.refs(0x1902)?;
    let essence = header.get(
        essence_refs
            .next()
            .context("Missing MXF EssenceContainerData")?,
    )?;
    ensure!(
        essence_refs.next().is_none() && essence.class == 0x23 && essence.uint(0x3f07)? != 0,
        "MXF OP-Atom requires one essence container"
    );
    let file_id: MobId = essence.bytes(0x2701)?;
    let mut material = None;
    let mut file = None;
    for id in storage.refs(0x1901)? {
        let package = header.get(id)?;
        if package.class == 0x36 {
            ensure!(
                material.replace(package).is_none(),
                "Multiple MXF Material Packages"
            );
        } else if package.class == 0x37 && package.bytes::<32>(0x4401)? == file_id {
            ensure!(
                file.replace(package).is_none(),
                "Duplicate MXF File Package UMID"
            );
        }
    }

    let material = material.context("Missing MXF Material Package")?;
    let file = file.context("Missing MXF File Package")?;
    let file_tracks = header.tracks(file)?;
    let (file_track, file_segment) = picture_track(&file_tracks)?;
    let source_slot = file_track.uint(0x4801)?;
    let rate = file_track.rate(0x4b01)?;
    let length = header.component(file_segment, 0x11)?.int(0x0202)?;
    let desc = header
        .0
        .get(&file.bytes(0x4701)?)
        .filter(|set| set.class == 0x28)
        .context("Unsupported MXF picture descriptor: expected CDCI")?;
    let container: [u8; 16] = desc.bytes(0x3004)?;
    let compression: [u8; 16] = desc.bytes(0x3201)?;
    ensure!(
        container[..7] == [6, 14, 43, 52, 4, 1, 1]
            && container[8..] == [13, 1, 3, 1, 2, 0x11, 1, 0]
            && compression[..7] == container[..7]
            && compression[8..13] == [4, 1, 2, 2, 0x71],
        "Unsupported MXF picture essence: expected clip-wrapped VC-3 (DNxHD/DNxHR)"
    );
    ensure!(
        length > 0
            && file_track.int(0x4b02)? == 0
            && source_slot != 0
            && desc.rate(0x3001)? == rate
            && (!desc.tags.contains_key(&0x3006) || desc.uint(0x3006)? == source_slot)
            && (!desc.tags.contains_key(&0x3002) || desc.int(0x3002)? == length),
        "Unsupported MXF File Package clock or extent"
    );
    let material_tracks = header.tracks(material)?;
    let (material_track, material_segment) = picture_track(&material_tracks)?;
    let link = header.component(material_segment, 0x11)?;
    ensure!(
        material_track.rate(0x4b01)? == rate
            && material_track.int(0x4b02)? == 0
            && link.int(0x0202)? == length
            && link.int(0x1201)? == 0
            && link.bytes::<32>(0x1101)? == file_id
            && link.uint(0x1102)? == source_slot,
        "Unsupported MXF Material Package picture edit"
    );

    let timecode = match header.timecode(&material_tracks, &rate)? {
        Some(code) => Some(code),
        None => header.timecode(&file_tracks, &rate)?,
    };
    let mut cdci = Props::default();
    // An SMPTE UL stores the label prefix first. AAF's AUID record
    // puts that half last and serializes the identifier fields little-endian.
    let mut compression = compression;
    compression.rotate_left(8);
    let compression = Uuid::from_bytes(compression);
    cdci.data(0x3201, compression.to_bytes_le());
    cdci.data(0x320c, desc.bytes::<1>(0x320c)?);
    for tag in [0x3202, 0x3203, 0x3301, 0x3302] {
        let value = desc.uint(tag)?;
        ensure!(value != 0, "Invalid MXF picture descriptor");
        cdci.data(tag, value.to_le_bytes());
    }
    // Sampled and display rectangles, alignment, and CDCI sampling levels.
    for tag in [
        0x3204, 0x3205, 0x3206, 0x3207, 0x3208, 0x3209, 0x320a, 0x320b, 0x3211, 0x3304, 0x3305,
        0x3306, 0x3308,
    ] {
        if desc.tags.contains_key(&tag) {
            cdci.data(tag, desc.uint(tag)?.to_le_bytes());
        }
    }
    if desc.tags.contains_key(&0x3303) {
        cdci.data(0x3303, desc.bytes::<1>(0x3303)?);
    }
    if desc.tags.contains_key(&0x3307) {
        cdci.data(
            0x3307,
            u16::from_be_bytes(desc.bytes(0x3307)?).to_le_bytes(),
        );
    }
    cdci.data(0x320e, super::rate_bytes(&desc.rate(0x320e)?)?);
    let mut line_map = desc.data(0x320d)?;
    let count = u32::from_be_bytes(take(&mut line_map)?) as usize;
    let width = u32::from_be_bytes(take(&mut line_map)?);
    ensure!(
        width == 4 && (1..=2).contains(&count) && line_map.len() == count * 4,
        "Invalid MXF VideoLineMap"
    );
    let mut lines = Vec::with_capacity(count * 4);
    for _ in 0..count {
        lines.extend(i32::from_be_bytes(take(&mut line_map)?).to_le_bytes());
    }
    cdci.data(0x320d, lines);

    Ok(Picture {
        material: mob_id(material.bytes(0x4401)?)?,
        file: mob_id(file_id)?,
        rate,
        length,
        timecode,
        cdci,
    })
}
