use crate::{Clock, Rate, absolute, file_url, integer, local_path, normalized, staged, storage::*};
use anyhow::{Context, Result, bail, ensure};
use cfb::CompoundFile;
use num_integer::Integer;
use num_traits::{ToPrimitive, Zero};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet},
    fs::{self, File},
    io::{self, Seek, Write},
    path::{Path, PathBuf},
};
use uuid::Uuid;

type MobId = [u8; 32];
type SlotRef = (MobId, u32);

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Sound,
    Picture,
    Timecode,
}

impl Kind {
    pub fn id(self) -> Uuid {
        match self {
            Self::Sound => uuid::uuid!("01030202-0200-0000-060e-2b3404010101"),
            Self::Picture => uuid::uuid!("01030202-0100-0000-060e-2b3404010101"),
            Self::Timecode => uuid::uuid!("01030201-0100-0000-060e-2b3404010101"),
        }
    }

    fn from_props(props: &Props) -> Result<Self> {
        let data = props.require(KIND)?;
        let id = Uuid::from_bytes_le(bytes(data.get(5..).context("Invalid AAF data kind")?)?);
        [Self::Sound, Self::Picture, Self::Timecode]
            .into_iter()
            .find(|kind| id == kind.id())
            .with_context(|| format!("Unsupported AAF track kind {id}"))
    }

    fn name(self) -> &'static str {
        match self {
            Self::Sound => "sound",
            Self::Picture => "picture",
            Self::Timecode => "timecode",
        }
    }
}

struct Part {
    class: u8,
    props: Props,
}

impl Part {
    fn span(&self, rate: &Clock) -> Result<Clock> {
        let length = self.props.int(LENGTH)?;
        ensure!(length >= 0, "Negative AAF component length");
        Ok(integer(length) / rate)
    }

    fn is_gap(&self) -> bool {
        matches!(self.class, FILLER | SCOPE_REF)
    }
}

struct Clip {
    media: Option<(String, Option<u32>)>,
    source_in: Clock,
    span: Clock,
}

struct Reader<'a> {
    file: CompoundFile<File>,
    mobs: HashMap<MobId, PathBuf>,
    essence: HashMap<MobId, PathBuf>,
    slots: HashMap<SlotRef, PathBuf>,
    extracted: HashMap<MobId, String>,
    extract_dir: Option<&'a Path>,
}

impl Reader<'_> {
    fn selected(&mut self, mut path: PathBuf) -> Result<(PathBuf, Part)> {
        let mut parent = None;
        for _ in 0..96 {
            let class = class(&self.file, &path)?;
            let props = Props::load(&mut self.file, &path)?;
            if let Some((kind, length)) = parent {
                ensure!(
                    kind == Kind::from_props(&props)? && length == props.int(LENGTH)?,
                    "Invalid AAF selected segment"
                );
            }
            if class != SELECTOR {
                return Ok((path, Part { class, props }));
            }
            parent = Some((Kind::from_props(&props)?, props.int(LENGTH)?));
            path = props.child(&path, 0x0f01, REF)?;
        }
        bail!("AAF selector nesting exceeds supported depth")
    }

    fn parts(&mut self, root: PathBuf) -> Result<Vec<Part>> {
        let mut pending = vec![(root, 0)];
        let mut output = Vec::new();
        while let Some((path, depth)) = pending.pop() {
            ensure!(depth < 96, "AAF sequence nesting exceeds supported depth");
            let (path, part) = self.selected(path)?;
            if part.class == SEQUENCE {
                let children = part.props.vector(&mut self.file, &path, 0x1001)?;
                pending.extend(children.into_iter().rev().map(|path| (path, depth + 1)));
            } else {
                output.push(part);
            }
        }
        Ok(output)
    }

    fn lookup_slot(&mut self, key: SlotRef, mob: &Path) -> Result<PathBuf> {
        if let Some(path) = self.slots.get(&key) {
            return Ok(path.clone());
        }
        for path in Props::load(&mut self.file, mob)?.vector(&mut self.file, mob, MOB_SLOTS)? {
            let id = Props::load(&mut self.file, &path)?.uint(SLOT_ID)?;
            self.slots.entry((key.0, id)).or_insert(path);
        }
        self.slots
            .get(&key)
            .cloned()
            .context("AAF source slot is missing")
    }

    fn resolve(
        &mut self,
        part: &Part,
        rate: &Clock,
        visited: &mut HashSet<SlotRef>,
        offset: Clock,
        span: Clock,
    ) -> Result<Vec<Clip>> {
        let first = part.props.int(0x1201)?;
        ensure!(first >= 0, "Negative AAF source range");
        let id = bytes(part.props.require(0x1101)?)?;
        let key = (id, part.props.uint(0x1102)?);
        ensure!(
            visited.len() < 96,
            "AAF source reference exceeds supported depth"
        );
        ensure!(visited.insert(key), "Cyclic AAF source reference");
        let mob = self
            .mobs
            .get(&id)
            .cloned()
            .context("AAF source reference is unresolved")?;
        let slot_path = self.lookup_slot(key, &mob)?;
        let slot = Props::load(&mut self.file, &slot_path)?;
        let segment_path = slot.child(&slot_path, SEGMENT, REF)?;
        let segment = Props::load(&mut self.file, &segment_path)?;
        let kind = Kind::from_props(&segment)?;
        ensure!(
            kind == Kind::from_props(&part.props)?,
            "AAF source media kind mismatch"
        );
        let source_rate = Rate::decode(slot.require(EDIT_RATE)?)?;
        // Resolve counts starts in the referencing track's edit units.
        // Recognize source-unit starts when the other interpretation is out of range.
        let mut start = integer(first) / rate + &offset;
        if &source_rate != rate && segment.get(LENGTH).is_some() {
            let available = integer(segment.int(LENGTH)?) / &source_rate;
            let native = integer(first) / &source_rate + offset;
            if &start + &span > available && &native + &span <= available {
                start = native;
            }
        }
        let media = if class(&self.file, &mob)? == SOURCE_MOB {
            self.source(id, &mob, &slot, kind)?
        } else {
            None
        };
        let result = if media.is_some() {
            vec![Clip {
                media,
                source_in: start,
                span,
            }]
        } else {
            let mut output = Vec::new();
            let mut cursor = Clock::zero();
            let mut covered = Clock::zero();
            let limit = &start + &span;
            for part in self.parts(segment_path)? {
                let next = &cursor + part.span(&source_rate)?;
                let begin = (&start).max(&cursor);
                let end = (&limit).min(&next);
                if begin < end {
                    let length = end - begin;
                    if part.class == SOURCE_CLIP {
                        output.extend(self.resolve(
                            &part,
                            &source_rate,
                            visited,
                            begin - &cursor,
                            length.clone(),
                        )?);
                    } else if part.is_gap() {
                        output.push(Clip {
                            media: None,
                            source_in: Clock::zero(),
                            span: length.clone(),
                        });
                    } else {
                        bail!(
                            "Unsupported AAF source component (class 0x{:02x})",
                            part.class
                        );
                    }
                    covered += length;
                }
                cursor = next;
            }
            ensure!(
                covered == span,
                "AAF source reference exceeds its source sequence"
            );
            output
        };
        visited.remove(&key);
        Ok(result)
    }

    fn source(
        &mut self,
        id: MobId,
        mob: &Path,
        slot: &Props,
        kind: Kind,
    ) -> Result<Option<(String, Option<u32>)>> {
        let mob_props = Props::load(&mut self.file, mob)?;
        let descriptor = mob_props.child(mob, DESCRIPTOR, REF)?;
        let props = Props::load(&mut self.file, &descriptor)?;
        let physical = slot
            .get(PHYSICAL)
            .map(|_| slot.uint(PHYSICAL))
            .transpose()?;
        if kind == Kind::Sound
            && let Some(essence) = self.essence.get(&id).cloned()
        {
            ensure!(
                class(&self.file, &descriptor)? == PCM,
                "Unsupported embedded AAF audio encoding"
            );
            let channels = props.uint(CHANNELS)?;
            let physical = physical.or((channels == 1).then_some(1)).unwrap_or(0);
            ensure!(
                (1..=channels).contains(&physical),
                "Ambiguous or invalid embedded AAF physical channel"
            );
            if !self.extracted.contains_key(&id) {
                let path = self.extract(&props, &essence, channels)?;
                self.extracted.insert(id, path);
            }
            return Ok(Some((self.extracted[&id].clone(), Some(physical - 1))));
        }
        if props.get(LOCATORS).is_none() {
            return Ok(None);
        }
        let mut urls = Vec::new();
        for path in props.vector(&mut self.file, &descriptor, LOCATORS)? {
            if let Some(url) = Props::load(&mut self.file, &path)?.text(URL)? {
                urls.push(url);
            }
        }
        ensure!(urls.len() == 1, "AAF source requires one media locator");
        let path = local_path(&urls[0])?;
        if kind == Kind::Picture {
            return Ok(Some((path, None)));
        }
        let physical = if let Some(physical) = physical {
            physical
        } else {
            let mut count = 0;
            for path in mob_props.vector(&mut self.file, mob, MOB_SLOTS)? {
                let slot = Props::load(&mut self.file, &path)?;
                let segment = slot.child(&path, SEGMENT, REF)?;
                let props = Props::load(&mut self.file, &segment)?;
                if Kind::from_props(&props).ok() == Some(Kind::Sound) {
                    count += 1;
                }
            }
            ensure!(count == 1, "Ambiguous AAF source channel");
            1
        };
        Ok(Some((
            path,
            Some(
                physical
                    .checked_sub(1)
                    .context("Invalid AAF physical channel")?,
            ),
        )))
    }

    fn extract(&mut self, props: &Props, essence: &Path, channels: u32) -> Result<String> {
        let dir = self
            .extract_dir
            .context("Embedded AAF audio requires an extraction directory")?;
        let sample_rate = Rate::decode(props.require(SAMPLE_RATE)?)?;
        let bits = props.uint(BITS)?;
        ensure!(
            sample_rate.is_integer()
                && (1..=64).contains(&channels)
                && matches!(bits, 16 | 24 | 32),
            "Unsupported embedded AAF channel layout, sample rate or sample width"
        );
        let rate = sample_rate
            .to_u32()
            .context("Invalid embedded sample rate")?;
        let alignment = channels * (bits / 8);
        let bytes_per_second = rate
            .checked_mul(alignment)
            .context("Invalid WAV byte rate")?;
        let length = u32::try_from(props.int(FILE_LENGTH)?)?
            .checked_mul(alignment)
            .context("Embedded AAF audio exceeds WAV size")?;
        let riff_size = length
            .checked_add(36)
            .context("Embedded AAF audio exceeds WAV size")?;
        let stream_path = Props::load(&mut self.file, essence)?.child(essence, 0x2702, STREAM)?;
        let mut stream = self.file.open_stream(stream_path)?;
        ensure!(
            stream.len() == u64::from(length),
            "WAV frame count differs from AAF descriptor"
        );
        fs::create_dir_all(dir)?;
        let mut file = tempfile::Builder::new()
            .prefix(".aaf-audio-")
            .tempfile_in(dir)?;
        // Match the original module's PCM WAV header and content-addressed names.
        file.write_all(b"RIFF")?;
        file.write_all(&riff_size.to_le_bytes())?;
        file.write_all(b"WAVEfmt ")?;
        file.write_all(&16u32.to_le_bytes())?;
        file.write_all(&1u16.to_le_bytes())?;
        file.write_all(&(channels as u16).to_le_bytes())?;
        file.write_all(&rate.to_le_bytes())?;
        file.write_all(&bytes_per_second.to_le_bytes())?;
        file.write_all(&(alignment as u16).to_le_bytes())?;
        file.write_all(&(bits as u16).to_le_bytes())?;
        file.write_all(b"data")?;
        file.write_all(&length.to_le_bytes())?;
        io::copy(&mut stream, &mut file)?;
        file.rewind()?;
        let mut digest = Sha256::new();
        io::copy(&mut file, &mut digest)?;
        let destination = dir.join(format!("{:x}.wav", digest.finalize()));
        file.persist(&destination)?;
        absolute(&destination)?
            .into_os_string()
            .into_string()
            .map_err(|_| anyhow::anyhow!("Embedded audio path is not Unicode"))
    }

    fn track(&mut self, name: String, kind: Kind, rate: &Clock, path: PathBuf) -> Result<Value> {
        let mut edits = Vec::new();
        let mut cursor = Clock::zero();
        for part in self.parts(path)? {
            let span = part.span(rate)?;
            if part.class == SOURCE_CLIP {
                let mut position = cursor.clone();
                for clip in self.resolve(
                    &part,
                    rate,
                    &mut HashSet::new(),
                    Clock::zero(),
                    span.clone(),
                )? {
                    if let Some((media, channel)) = clip.media
                        && !clip.span.is_zero()
                    {
                        edits.push((
                            media,
                            channel,
                            [position.clone(), clip.source_in, clip.span.clone()],
                        ));
                    }
                    position += clip.span;
                }
            } else if !part.is_gap() {
                bail!(
                    "Unsupported AAF timeline component (class 0x{:02x})",
                    part.class
                );
            }
            cursor += span;
        }

        let times = || edits.iter().flat_map(|(_, _, times)| times);
        let clock = if times().all(|time| (time * rate).is_integer()) {
            rate.clone()
        } else {
            Clock::from_integer(
                times().fold(rate.numer().clone(), |clock, time| clock.lcm(time.denom())),
            )
        };
        let time_rate = Rate::from_clock(&clock)?;
        let mut clips = Vec::new();
        for (path, channel, times) in edits {
            let ticks = |time: &Clock| (time * &clock).to_u64().context("AAF time is out of range");
            clips.push(json!({
                "path": path, "channel": channel,
                "start": ticks(&times[0])?, "source_in": ticks(&times[1])?, "length": ticks(&times[2])?,
            }));
        }
        Ok(json!({
            "name": name, "media_kind": kind.name(), "edit_rate": Rate::from_clock(rate)?,
            "time_rate": time_rate, "clips": clips,
        }))
    }

    fn composition(&mut self, path: &Path, props: &Props) -> Result<Value> {
        let mut tracks = Vec::new();
        for slot in props.vector(&mut self.file, path, MOB_SLOTS)? {
            let props = Props::load(&mut self.file, &slot)?;
            let segment = props.child(&slot, SEGMENT, REF)?;
            let kind = Kind::from_props(&Props::load(&mut self.file, &segment)?)?;
            if kind == Kind::Timecode && class(&self.file, &segment)? == TIMECODE {
                continue;
            }
            ensure!(kind != Kind::Timecode, "Unsupported AAF timecode component");
            let rate = Rate::decode(props.require(EDIT_RATE)?)?;
            let (segment, part) = self.selected(segment)?;
            let layers = if part.class == NESTED_SCOPE {
                part.props.vector(&mut self.file, &segment, 0x0c01)?
            } else {
                vec![segment]
            };
            let name = props.text(SLOT_NAME)?.unwrap_or_default();
            let stacked = layers.len() > 1;
            for (index, layer) in layers.into_iter().enumerate() {
                let name = if stacked {
                    format!("{name} {}", index + 1).trim().to_owned()
                } else {
                    name.clone()
                };
                tracks.push(self.track(name, kind, &rate, layer)?);
            }
        }
        Ok(json!({"name": props.text(MOB_NAME)?, "tracks": tracks}))
    }
}

pub fn timeline(path: &Path, extract_dir: Option<&Path>) -> Result<Value> {
    let mut file = cfb::open(path)?;
    let path = content_path(&mut file)?;
    let props = Props::load(&mut file, &path)?;
    let members = props.set_members(&mut file, &path, 0x1901)?;
    let mut reader = Reader {
        essence: props
            .set_members(&mut file, &path, 0x1902)?
            .into_iter()
            .collect(),
        mobs: members.iter().cloned().collect(),
        file,
        slots: HashMap::new(),
        extracted: HashMap::new(),
        extract_dir,
    };
    let mut sequences = Vec::new();
    for (_, path) in members {
        if class(&reader.file, &path)? == COMPOSITION {
            let props = Props::load(&mut reader.file, &path)?;
            if props.get(USAGE) == Some(TOP_LEVEL.to_bytes_le().as_slice()) {
                sequences.push(reader.composition(&path, &props)?);
            }
        }
    }
    ensure!(!sequences.is_empty(), "AAF has no top-level composition");
    Ok(json!({"version": 3, "sequences": sequences}))
}

#[derive(Deserialize)]
struct RepairManifest {
    version: u32,
    replacements: Vec<Replacement>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Replacement {
    from: PathBuf,
    to: PathBuf,
}

pub fn repair(manifest: &Path, source: &Path, destination: &Path) -> Result<Value> {
    let manifest: RepairManifest =
        serde_json::from_reader(io::BufReader::new(File::open(manifest)?))?;
    ensure!(
        manifest.version == 1,
        "Unsupported AAF path repair manifest"
    );
    let source = absolute(source)?;
    ensure!(
        absolute(destination).ok().as_ref() != Some(&source),
        "AAF path repair destination must be a copy"
    );
    let mut mappings = HashMap::new();
    for entry in manifest.replacements {
        let target = absolute(&entry.to)?;
        ensure!(target.is_file(), "AAF replacement target is not a file");
        mappings.insert(normalized(&entry.from), file_url(&target)?);
    }
    let mut changed = 0;
    let path = staged(destination, |temporary| {
        io::copy(&mut File::open(source)?, temporary)?;
        temporary.rewind()?;
        let mut file = CompoundFile::open(temporary)?;
        let content = content_path(&mut file)?;
        let content_props = Props::load(&mut file, &content)?;
        for (_, path) in content_props.set_members(&mut file, &content, 0x1901)? {
            if class(&file, &path)? != SOURCE_MOB {
                continue;
            }
            let mob = Props::load(&mut file, &path)?;
            if mob.get(DESCRIPTOR).is_none() {
                continue;
            }
            let descriptor = mob.child(&path, DESCRIPTOR, REF)?;
            let props = Props::load(&mut file, &descriptor)?;
            for locator in props.vector(&mut file, &descriptor, LOCATORS)? {
                let mut props = Props::load(&mut file, &locator)?;
                if let Some(url) = props.text(URL)?
                    && let Ok(old) = local_path(&url)
                    && let Some(new) = mappings.get(&normalized(Path::new(&old)))
                {
                    props.string(URL, new);
                    props.save(&mut file, &locator)?;
                    changed += 1;
                }
            }
        }
        file.flush()?;
        Ok(())
    })?;
    Ok(json!({"version": 1, "path": path, "replacements": changed}))
}
