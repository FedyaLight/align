//! AAF object property streams and strong references in a CFB container.
use anyhow::{Context, Result, bail, ensure};
use cfb::CompoundFile;
use std::{
    collections::BTreeMap,
    io::{Read, Seek, Write},
    path::{Path, PathBuf},
};
use uuid::Uuid;

pub const DATA: u16 = 0x82;
pub const STREAM: u16 = 0x42;
pub const REF: u16 = 0x22;
pub const VECTOR: u16 = 0x32;
pub const SET: u16 = 0x3a;
pub const WEAK: u16 = 0x02;

pub const LENGTH: u16 = 0x0202;
pub const KIND: u16 = 0x0201;
pub const MOB_ID: u16 = 0x4401;
pub const MOB_NAME: u16 = 0x4402;
pub const MOB_SLOTS: u16 = 0x4403;
pub const USAGE: u16 = 0x4408;
pub const DESCRIPTOR: u16 = 0x4701;
pub const SLOT_ID: u16 = 0x4801;
pub const SLOT_NAME: u16 = 0x4802;
pub const SEGMENT: u16 = 0x4803;
pub const PHYSICAL: u16 = 0x4804;
pub const EDIT_RATE: u16 = 0x4b01;
pub const LOCATORS: u16 = 0x2f01;
pub const SAMPLE_RATE: u16 = 0x3001;
pub const FILE_LENGTH: u16 = 0x3002;
pub const URL: u16 = 0x4001;
pub const CHANNELS: u16 = 0x3d07;
pub const BITS: u16 = 0x3d01;

pub const FILLER: u8 = 0x09;
pub const NESTED_SCOPE: u8 = 0x0b;
pub const SCOPE_REF: u8 = 0x0d;
pub const SELECTOR: u8 = 0x0e;
pub const SEQUENCE: u8 = 0x0f;
pub const SOURCE_CLIP: u8 = 0x11;
pub const TIMECODE: u8 = 0x14;
pub const COMPOSITION: u8 = 0x35;
pub const MASTER_MOB: u8 = 0x36;
pub const SOURCE_MOB: u8 = 0x37;
pub const PCM: u8 = 0x48;
pub const TOP_LEVEL: Uuid = uuid::uuid!("0d010102-0101-0700-060e-2b3404010101");

pub fn class_id(kind: u8) -> Uuid {
    Uuid::from_u128(0x0d010101_0101_0000_060e_2b3402060101 | (u128::from(kind) << 72))
}

pub fn class<F: Read + Seek>(file: &CompoundFile<F>, path: &Path) -> Result<u8> {
    let id = *file.entry(path)?.clsid();
    let kind = (id.as_u128() >> 72) as u8;
    Ok(if id == class_id(kind) { kind } else { 0 })
}

pub fn utf16(value: &str) -> Vec<u8> {
    value
        .encode_utf16()
        .chain([0])
        .flat_map(u16::to_le_bytes)
        .collect()
}

pub fn decode_string(data: &[u8]) -> Result<String> {
    ensure!(data.len().is_multiple_of(2), "Invalid AAF string");
    Ok(String::from_utf16(
        &data
            .as_chunks::<2>()
            .0
            .iter()
            .map(|b| u16::from_le_bytes([b[0], b[1]]))
            .take_while(|unit| *unit != 0)
            .collect::<Vec<_>>(),
    )?)
}

fn child_name(name: &str) -> Result<&str> {
    ensure!(
        !name.is_empty() && name != "." && name != ".." && !name.contains(['/', '\\', ':', '\0']),
        "Invalid AAF storage name"
    );
    Ok(name)
}

pub fn bytes<const N: usize>(data: &[u8]) -> Result<[u8; N]> {
    data.try_into().context("Invalid AAF property size")
}

/// Bounds metadata work across the whole document, including repeated visits
/// to the same source. Embedded audio is read as a stream outside this budget.
pub struct MetadataReader<F> {
    pub inner: CompoundFile<F>,
    remaining: u64,
}

impl<F: Read + Seek> MetadataReader<F> {
    pub fn new(file: CompoundFile<F>) -> Self {
        Self {
            inner: file,
            remaining: 256 << 20,
        }
    }

    fn charge(&mut self, cost: u64) -> Result<()> {
        self.remaining = self
            .remaining
            .checked_sub(cost)
            .context("AAF document exceeds the metadata processing limit")?;
        Ok(())
    }

    fn read_stream(&mut self, path: &Path, limit: u64) -> Result<Vec<u8>> {
        let mut stream = self
            .inner
            .open_stream(path)
            .with_context(|| format!("AAF stream {}", path.display()))?;
        let length = stream.len();
        ensure!(length <= limit, "AAF metadata stream is too large");
        // Even tiny streams incur object lookup and allocation work.
        self.charge(length + 256)?;
        let mut data = Vec::new();
        stream.read_to_end(&mut data)?;
        Ok(data)
    }
}

#[derive(Default)]
pub struct Props(BTreeMap<u16, (u16, Vec<u8>)>);

impl Props {
    pub fn extend(&mut self, other: Self) {
        self.0.extend(other.0);
    }

    pub fn load<F: Read + Seek>(file: &mut MetadataReader<F>, path: &Path) -> Result<Self> {
        let data = file.read_stream(&path.join("properties"), 16 << 20)?;
        let [order, _, low, high, ..] = data.as_slice() else {
            bail!("Truncated AAF property stream");
        };
        ensure!(*order == 0x4c, "Unsupported AAF property byte order");
        let count = usize::from(u16::from_le_bytes([*low, *high]));
        let (table, mut values) = data[4..]
            .split_at_checked(count * 6)
            .context("Truncated AAF property table")?;
        file.charge(count as u64 * 32)?;
        let mut props = Self::default();
        for entry in table.as_chunks::<6>().0 {
            let pid = u16::from_le_bytes(bytes(&entry[..2])?);
            let form = u16::from_le_bytes(bytes(&entry[2..4])?);
            let size = usize::from(u16::from_le_bytes(bytes(&entry[4..])?));
            let (value, rest) = values
                .split_at_checked(size)
                .context("Truncated AAF property value")?;
            props.set(pid, form, value.to_vec());
            values = rest;
        }
        Ok(props)
    }

    pub fn save<F: Read + Write + Seek>(
        &self,
        file: &mut CompoundFile<F>,
        path: &Path,
    ) -> Result<()> {
        let mut stream = file.create_stream(path.join("properties"))?;
        stream.write_all(&[0x4c, 32])?;
        stream.write_all(&u16::try_from(self.0.len())?.to_le_bytes())?;
        for (&pid, (form, value)) in &self.0 {
            stream.write_all(&pid.to_le_bytes())?;
            stream.write_all(&form.to_le_bytes())?;
            stream.write_all(
                &u16::try_from(value.len())
                    .context("AAF property is too long")?
                    .to_le_bytes(),
            )?;
        }
        for (_, value) in self.0.values() {
            stream.write_all(value)?;
        }
        stream.flush()?;
        Ok(())
    }

    pub fn set(&mut self, pid: u16, form: u16, value: impl Into<Vec<u8>>) {
        self.0.insert(pid, (form, value.into()));
    }

    pub fn data(&mut self, pid: u16, value: impl Into<Vec<u8>>) {
        self.set(pid, DATA, value);
    }

    pub fn string(&mut self, pid: u16, text: &str) {
        self.data(pid, utf16(text));
    }

    pub fn get(&self, pid: u16) -> Option<&[u8]> {
        self.0.get(&pid).map(|(_, data)| data.as_slice())
    }

    pub fn require(&self, pid: u16) -> Result<&[u8]> {
        self.get(pid)
            .with_context(|| format!("Missing AAF property {pid:04x}"))
    }

    pub fn text(&self, pid: u16) -> Result<Option<String>> {
        self.get(pid).map(decode_string).transpose()
    }

    pub fn int(&self, pid: u16) -> Result<i64> {
        Ok(i64::from_le_bytes(bytes(self.require(pid)?)?))
    }

    pub fn uint(&self, pid: u16) -> Result<u32> {
        Ok(u32::from_le_bytes(bytes(self.require(pid)?)?))
    }

    pub fn child(&self, path: &Path, pid: u16, form: u16) -> Result<PathBuf> {
        let (actual, data) = self
            .0
            .get(&pid)
            .with_context(|| format!("Missing AAF reference {pid:04x}"))?;
        ensure!(*actual == form, "Unexpected AAF reference form");
        let name = decode_string(if form == STREAM {
            data.strip_prefix(&[0x55])
                .context("Invalid AAF stream reference")?
        } else {
            data
        })?;
        Ok(path.join(child_name(&name)?))
    }

    pub fn vector<F: Read + Seek>(
        &self,
        file: &mut MetadataReader<F>,
        path: &Path,
        pid: u16,
    ) -> Result<Vec<PathBuf>> {
        if self.get(pid).is_none() {
            return Ok(Vec::new());
        }
        let base = self.child(path, pid, VECTOR)?;
        let name = base
            .file_name()
            .context("Missing AAF reference name")?
            .to_string_lossy();
        let index = index_data(file, path, &name, 12, 4)?;
        index
            .as_chunks::<4>()
            .0
            .iter()
            .map(|entry| Ok(path.join(format!("{name}{{{:x}}}", u32::from_le_bytes(*entry)))))
            .collect()
    }

    pub fn set_members<F: Read + Seek>(
        &self,
        file: &mut MetadataReader<F>,
        path: &Path,
        pid: u16,
    ) -> Result<Vec<([u8; 32], PathBuf)>> {
        if self.get(pid).is_none() {
            return Ok(Vec::new());
        }
        let base = self.child(path, pid, SET)?;
        let name = base
            .file_name()
            .context("Missing AAF reference name")?
            .to_string_lossy();
        let index = index_data(file, path, &name, 15, 40)?;
        index
            .as_chunks::<40>()
            .0
            .iter()
            .map(|entry| {
                let key = u32::from_le_bytes(bytes(&entry[..4])?);
                Ok((bytes(&entry[8..])?, path.join(format!("{name}{{{key:x}}}"))))
            })
            .collect()
    }
}

fn index_data<F: Read + Seek>(
    file: &mut MetadataReader<F>,
    path: &Path,
    name: &str,
    header: usize,
    stride: usize,
) -> Result<Vec<u8>> {
    let data = file.read_stream(&path.join(format!("{name} index")), 32 << 20)?;
    ensure!(data.len() >= header, "Truncated AAF reference index");
    let count = u32::from_le_bytes(bytes(&data[..4])?) as usize;
    ensure!(
        count.checked_mul(stride) == Some(data.len() - header),
        "Invalid AAF reference index"
    );
    // Account for the paths allocated for each reference before building them.
    file.charge(count as u64 * 128)?;
    Ok(data[header..].to_vec())
}

pub fn content_path<F: Read + Seek>(file: &mut MetadataReader<F>) -> Result<PathBuf> {
    let root = Path::new("/");
    let header = Props::load(file, root)?.child(root, 0x0002, REF)?;
    Props::load(file, &header)?.child(&header, 0x3b03, REF)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn property_reads_share_a_document_budget() -> Result<()> {
        let mut file = CompoundFile::create(Cursor::new(Vec::new()))?;
        let first = Path::new("/first");
        let second = Path::new("/second");
        for path in [first, second] {
            file.create_storage(path)?;
            let mut props = Props::default();
            props.string(MOB_NAME, "Take");
            props.save(&mut file, path)?;
        }
        let mut metadata = MetadataReader::new(file);
        metadata.remaining = 400;
        assert_eq!(
            Props::load(&mut metadata, first)?
                .text(MOB_NAME)?
                .as_deref(),
            Some("Take")
        );
        let error = Props::load(&mut metadata, second)
            .err()
            .context("Second read exceeded the document budget")?;
        assert_eq!(
            error.to_string(),
            "AAF document exceeds the metadata processing limit"
        );
        Ok(())
    }
}
