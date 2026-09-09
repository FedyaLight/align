//! Embedded metadata: Broadcast Wave `bext`, linked file sets, iXML SPEED,
//! Sony NonRealTimeMeta XML, and timecode labels.
//!
//! BWF dates resolve in UTC so recording timestamps do not depend on the
//! machine's time zone. Container timecode tracks are read by `align-decode`.

use std::collections::HashMap;
use std::path::Path;

use quick_xml::Reader;
use quick_xml::events::Event;
use sha2::{Digest, Sha256};

use crate::model::{MediaSpan, MediaTime, SourceTimecode, file_name};

#[derive(Clone, Debug, Default, PartialEq)]
pub struct BwfMetadata {
    /// Unix seconds (UTC — see module docs).
    pub recording_date: Option<i64>,
    pub media_span: Option<MediaSpan>,
    pub time_reference: Option<u64>,
    /// `fmt` sample rate; carried so the timecode below (and
    /// [`BwfMetadata::audio_timecode`]) need no second file pass.
    pub sample_rate: Option<f64>,
    /// Bare bext timecode (25 NDF display) or OriginationTime label.
    pub timecode: Option<SourceTimecode>,
    /// Parsed `iXML` SPEED block from the same chunk walk (`None` when
    /// absent or corrupt — readers must not assume a rate).
    pub ixml: Option<IxmlSpeed>,
}

impl BwfMetadata {
    /// Audio timecode with explicit deterministic priority, identical on
    /// both backends:
    /// 1. bext TimeReference + fmt rate (elapsed; EBU data wins over iXML
    ///    copies), display from iXML RATE/FLAG when present, else 25 NDF;
    /// 2. iXML TIMESTAMP copy when bext is absent;
    /// 3. bare bext OriginationTime label.
    pub fn audio_timecode(&self) -> Option<SourceTimecode> {
        if let (Some(tref), Some(rate)) = (self.time_reference, self.sample_rate) {
            let (duration, drop) = self
                .ixml
                .as_ref()
                .map(|i| (i.frame_duration, i.drop_frame))
                .unwrap_or((MediaTime::new(1, 25), false));
            return SourceTimecode::from_samples(tref, rate, duration, drop);
        }
        if let Some(ix) = self.ixml.as_ref() {
            if let Some(t) = ix.timestamp.as_ref() {
                if let Some(tc) = SourceTimecode::from_samples(
                    t.samples,
                    t.rate,
                    ix.frame_duration,
                    ix.drop_frame,
                ) {
                    return Some(tc);
                }
            }
        }
        self.timecode.clone()
    }
}

/// iXML `BWFXML/SPEED` display parameters (Gallery iXML spec). All fields
/// optional per spec; `frame_duration`/`drop_frame` only describe how to
/// *display* the authoritative bext count, never the elapsed instant.
/// `timestamp` is the redundant SPEED copy of the bext count — used only
/// when bext itself is absent (EBU data takes precedence when present).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct IxmlTimestamp {
    pub samples: u64,
    pub rate: f64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct IxmlSpeed {
    pub frame_duration: MediaTime,
    pub drop_frame: bool,
    pub timestamp: Option<IxmlTimestamp>,
}

// ------------------------------------------------------------ BWF

/// Parse RIFF/RF64/BW64 headers + `ds64`/`bext`/`fmt `/`link`/`iXML`
/// chunks in a single pass. Returns `None` for non-WAVE files (12-byte
/// signature check).
pub fn read_bwf(path: &Path) -> Option<BwfMetadata> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(path).ok()?;
    let mut header = [0u8; 12];
    file.read_exact(&mut header).ok()?;
    if !matches!(&header[0..4], b"RIFF" | b"RF64" | b"BW64") || &header[8..12] != b"WAVE" {
        return None;
    }

    let mut offset: u64 = 12;
    let mut sample_rate: Option<f64> = None;
    let mut date_text: Option<String> = None;
    let mut time_text: Option<String> = None;
    let mut time_reference: Option<u64> = None;
    let mut media_span: Option<MediaSpan> = None;
    let mut data_size_64: Option<u64> = None;
    let mut ixml: Option<IxmlSpeed> = None;

    loop {
        file.seek(SeekFrom::Start(offset)).ok()?;
        let mut chunk_header = [0u8; 8];
        if file.read_exact(&mut chunk_header).is_err() {
            break;
        }
        let id = &chunk_header[0..4];
        let size32 = u32::from_le_bytes(chunk_header[4..8].try_into().ok()?);
        let size = if id == b"data" && size32 == u32::MAX {
            data_size_64.unwrap_or(u64::from(size32))
        } else {
            u64::from(size32)
        };
        let payload = offset.checked_add(8)?;

        if id == b"ds64" && size >= 16 {
            file.seek(SeekFrom::Start(payload)).ok()?;
            let mut buf = [0u8; 16];
            if file.read_exact(&mut buf).is_ok() {
                data_size_64 = Some(u64::from_le_bytes(buf[8..16].try_into().unwrap()));
            }
        } else if id == b"bext" && size >= 346 {
            file.seek(SeekFrom::Start(payload)).ok()?;
            let n = size.min(346) as usize;
            let mut buf = vec![0u8; n];
            if file.read_exact(&mut buf).is_ok() && buf.len() >= 346 {
                date_text = ascii(&buf, 320, 10);
                time_text = ascii(&buf, 330, 8);
                time_reference = Some(
                    u64::from(u32::from_le_bytes(buf[338..342].try_into().unwrap()))
                        | (u64::from(u32::from_le_bytes(buf[342..346].try_into().unwrap())) << 32),
                );
            }
        } else if id == b"fmt " && size >= 8 {
            file.seek(SeekFrom::Start(payload)).ok()?;
            let n = size.min(16) as usize;
            let mut buf = vec![0u8; n];
            if file.read_exact(&mut buf).is_ok() && buf.len() >= 8 {
                sample_rate = Some(f64::from(u32::from_le_bytes(buf[4..8].try_into().unwrap())));
            }
        } else if id == b"link" && size > 0 && size <= 1_048_576 {
            file.seek(SeekFrom::Start(payload)).ok()?;
            let mut buf = vec![0u8; size as usize];
            if file.read_exact(&mut buf).is_ok() {
                media_span = parse_media_span(&buf, &file_name(path));
            }
        } else if id == b"iXML" && size > 0 && size <= 1_048_576 {
            file.seek(SeekFrom::Start(payload)).ok()?;
            let mut buf = vec![0u8; size as usize];
            if file.read_exact(&mut buf).is_ok() {
                ixml = ixml_payload_text(&buf).and_then(parse_ixml_speed);
            }
        }

        let padded = size.checked_add(size & 1)?;
        offset = payload.checked_add(padded)?;
    }

    Some(BwfMetadata {
        recording_date: make_recording_date(
            date_text.as_deref(),
            time_text.as_deref(),
            time_reference,
            sample_rate,
        ),
        media_span,
        time_reference,
        sample_rate,
        timecode: make_bwf_timecode(time_reference, sample_rate, time_text.as_deref()),
        ixml,
    })
}

fn ascii(buf: &[u8], at: usize, count: usize) -> Option<String> {
    let slice = buf.get(at..at.checked_add(count)?)?;
    // Trim control characters, including NUL padding.
    let text: String = slice
        .iter()
        .take_while(|b| **b != 0)
        .map(|&b| b as char)
        .collect::<String>()
        .trim_matches(|c: char| c.is_control())
        .to_string();
    if text.is_empty() {
        return None;
    }
    // Reject non-ASCII bytes in this fixed-format field.
    if text.bytes().any(|b| b >= 0x80) {
        return None;
    }
    Some(text)
}

fn numeric_parts(text: &str) -> Vec<i64> {
    text.split(|c: char| !c.is_ascii_digit())
        .filter(|s| !s.is_empty())
        .filter_map(|s| s.parse().ok())
        .collect()
}

/// Gregorian date → days since Unix epoch (Hinnant's algorithm, no deps).
fn days_from_civil(year: i64, month: i64, day: i64) -> Option<i64> {
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some(era * 146097 + doe - 719468)
}

fn make_recording_date(
    date_text: Option<&str>,
    time_text: Option<&str>,
    time_reference: Option<u64>,
    sample_rate: Option<f64>,
) -> Option<i64> {
    let date = numeric_parts(date_text?);
    if date.len() != 3 {
        return None;
    }
    let midnight = days_from_civil(date[0], date[1], date[2])? * 86_400;
    if let (Some(tref), Some(rate)) = (time_reference, sample_rate) {
        if rate > 0.0 {
            let seconds = tref as f64 / rate;
            // EBU Tech 3285: TimeReference counts samples since midnight, so
            // only [0, 24 h) belongs to OriginationDate's day. Anything past
            // midnight is the next (unprovable) day — no calendar date.
            if (0.0..86_400.0).contains(&seconds) {
                return Some(midnight + seconds as i64);
            }
            return None;
        }
    }
    let time = numeric_parts(time_text?);
    if time.len() != 3 {
        return None;
    }
    Some(midnight + time[0] * 3600 + time[1] * 60 + time[2])
}

/// BWF timecode: sample-accurate elapsed `(TimeReference, sample_rate)`
/// with a 25 fps NDF display assumption (BWF carries no fps — the label is
/// a convention, the elapsed instant is exact). Callers with iXML display
/// parameters re-derive the label via [`SourceTimecode::from_samples`];
/// elapsed never changes. Without a count, a strict `hh:mm:ss`
/// OriginationTime degrades to a bare time-of-day label (out-of-range is
/// corrupt metadata → `None`, never wrapped into range).
fn make_bwf_timecode(
    time_reference: Option<u64>,
    sample_rate: Option<f64>,
    time_text: Option<&str>,
) -> Option<SourceTimecode> {
    const DISPLAY_25: MediaTime = MediaTime {
        value: 1,
        timescale: 25,
    };
    if let (Some(tref), Some(rate)) = (time_reference, sample_rate) {
        return SourceTimecode::from_samples(tref, rate, DISPLAY_25, false);
    }
    let parts = numeric_parts(time_text?);
    if parts.len() != 3 {
        return None;
    }
    let (h, m, s) = (parts[0], parts[1], parts[2]);
    SourceTimecode::from_components(h, m, s, 0, DISPLAY_25, false)
}

/// EBU Tech 3285 Supplement 4 `link` chunk: file-set identity + this file's
/// part number. Requires ≥2 files, dense 1..=n numbering, and exactly
/// one `actual` matching this filename (case-insensitive), else None.
fn parse_media_span(data: &[u8], actual_filename: &str) -> Option<MediaSpan> {
    let end = data.iter().position(|&b| b == 0).unwrap_or(data.len());
    let xml = std::str::from_utf8(&data[..end]).ok()?;
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    #[derive(Default)]
    struct FileEntry {
        number: Option<i64>,
        name: Option<String>,
        is_actual: bool,
    }
    let mut root_name: Option<String> = None;
    let mut stack: Vec<String> = Vec::new();
    let mut files: Vec<FileEntry> = Vec::new();
    let mut current_text = String::new();
    let mut set_id: Option<String> = None;
    let mut buf = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let name = String::from_utf8_lossy(e.local_name().as_ref()).to_uppercase();
                if root_name.is_none() {
                    root_name = Some(name.clone());
                    if name != "LINK" {
                        return None;
                    }
                }
                if name == "FILE" {
                    let mut entry = FileEntry::default();
                    for attr in e.attributes().flatten() {
                        if !attr.key.as_ref().eq_ignore_ascii_case(b"type") {
                            continue;
                        }
                        if let Ok(v) = attr.decode_and_unescape_value(reader.decoder()) {
                            entry.is_actual = v.eq_ignore_ascii_case("actual");
                        }
                    }
                    files.push(entry);
                }
                stack.push(name);
                current_text.clear();
            }
            Ok(Event::Empty(e)) => {
                let name = String::from_utf8_lossy(e.local_name().as_ref()).to_uppercase();
                // A self-closed root cannot hold FILEs.
                root_name.as_ref()?;
                if name == "FILE" {
                    let mut entry = FileEntry::default();
                    for attr in e.attributes().flatten() {
                        if !attr.key.as_ref().eq_ignore_ascii_case(b"type") {
                            continue;
                        }
                        if let Ok(v) = attr.decode_and_unescape_value(reader.decoder()) {
                            entry.is_actual = v.eq_ignore_ascii_case("actual");
                        }
                    }
                    files.push(entry);
                }
            }
            Ok(Event::Text(e)) => {
                if let Ok(t) = e.unescape() {
                    current_text.push_str(&t);
                }
            }
            Ok(Event::End(e)) => {
                let name = String::from_utf8_lossy(e.local_name().as_ref()).to_uppercase();
                let parent = stack
                    .len()
                    .checked_sub(2)
                    .and_then(|i| stack.get(i).cloned());
                if name == "FILENUMBER" && parent.as_deref() == Some("FILE") {
                    if let Some(last) = files.last_mut() {
                        last.number = current_text.trim().parse().ok().filter(|&n| n > 0);
                    }
                } else if name == "FILENAME" && parent.as_deref() == Some("FILE") {
                    if let Some(last) = files.last_mut() {
                        let v = current_text.trim().to_string();
                        last.name = if v.is_empty() { None } else { Some(v) };
                    }
                } else if name == "ID" && parent.as_deref() == Some("LINK") {
                    let v = current_text.trim().to_string();
                    set_id = if v.is_empty() { None } else { Some(v) };
                }
                stack.pop();
                current_text.clear();
            }
            Ok(Event::Eof) => break,
            Err(_) => return None,
            _ => {}
        }
        buf.clear();
    }

    if files.len() < 2 {
        return None;
    }
    // Drop entries without a positive number or filename.
    files.retain(|f| {
        f.number.is_some_and(|n| n > 0) && f.name.as_deref().is_some_and(|n| !n.is_empty())
    });
    if files.len() < 2 {
        return None;
    }
    let numbers: Vec<i64> = files.iter().filter_map(|f| f.number).collect();
    if numbers.len() != files.len() {
        return None;
    }
    let mut sorted = numbers.clone();
    sorted.sort_unstable();
    if sorted != (1..=files.len() as i64).collect::<Vec<_>>() {
        return None;
    }
    let actuals: Vec<&FileEntry> = files.iter().filter(|f| f.is_actual).collect();
    if actuals.len() != 1 {
        return None;
    }
    let actual = actuals[0];
    let actual_name = actual.name.as_deref()?;
    if !actual_name.eq_ignore_ascii_case(actual_filename) {
        return None;
    }
    let mut ordered: Vec<&FileEntry> = files.iter().collect();
    ordered.sort_by_key(|f| f.number.unwrap_or(0));
    let mut identity = String::new();
    if let Some(id) = set_id {
        identity.push_str(&id);
    }
    identity.push('\0');
    identity.push_str(
        &ordered
            .iter()
            .map(|f| f.name.as_deref().unwrap_or(""))
            .collect::<Vec<_>>()
            .join("\0"),
    );
    let digest = Sha256::digest(identity.as_bytes());
    Some(MediaSpan {
        identifier: format!(
            "bwf:{}",
            digest[..16]
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        ),
        part_number: actual.number.unwrap_or(0) as usize,
        part_count: files.len(),
    })
}

// ------------------------------------------------------------ iXML

/// `iXML` chunk payload → XML text. Strips RIFF word-alignment and writer
/// NUL padding plus trailing whitespace, but rejects anything after
/// `</BWFXML>` and payloads that don't open as XML — padding is
/// convention, trailing garbage is corruption, never metadata.
fn ixml_payload_text(buf: &[u8]) -> Option<&str> {
    let end = buf.iter().rposition(|&b| b != 0)?;
    let mut payload = &buf[..=end];
    while payload.last().is_some_and(|b| b.is_ascii_whitespace()) {
        payload = &payload[..payload.len() - 1];
    }
    if !payload.ends_with(b"</BWFXML>") {
        return None;
    }
    let text = std::str::from_utf8(payload).ok()?;
    let head = text.strip_prefix('\u{FEFF}').unwrap_or(text).trim_start();
    if head.starts_with("<BWFXML") || head.starts_with("<?xml") {
        Some(text)
    } else {
        None
    }
}

/// Parse `BWFXML/SPEED`: display rate + DF flag + redundant timestamp
/// copy. `None` when the SPEED block or its rate is absent/unparseable —
/// readers must not assume a rate. `TIMECODE_RATE` accepts spec ratios
/// (`30000/1001`) and observed decimals (`29.97002997003`); the flag
/// defaults to NDF per spec.
pub fn parse_ixml_speed(xml: &str) -> Option<IxmlSpeed> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut stack: Vec<String> = Vec::new();
    let mut current_text = String::new();
    let mut rate: Option<String> = None;
    let mut flag: Option<String> = None;
    let mut hi: Option<String> = None;
    let mut lo: Option<String> = None;
    let mut ts_rate: Option<String> = None;
    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                stack.push(String::from_utf8_lossy(e.local_name().as_ref()).to_uppercase());
                current_text.clear();
            }
            Ok(Event::Empty(e)) => {
                // Self-closed SPEED children carry no text; nothing to capture.
                let _ = String::from_utf8_lossy(e.local_name().as_ref()).to_uppercase();
            }
            Ok(Event::Text(e)) => {
                if let Ok(t) = e.unescape() {
                    current_text.push_str(&t);
                }
            }
            Ok(Event::End(e)) => {
                let name = String::from_utf8_lossy(e.local_name().as_ref()).to_uppercase();
                if stack.iter().any(|s| s == "SPEED") {
                    match name.as_str() {
                        "TIMECODE_RATE" => rate = Some(current_text.trim().to_string()),
                        "TIMECODE_FLAG" => flag = Some(current_text.trim().to_string()),
                        "TIMESTAMP_SAMPLES_SINCE_MIDNIGHT_HI" => {
                            hi = Some(current_text.trim().to_string())
                        }
                        "TIMESTAMP_SAMPLES_SINCE_MIDNIGHT_LO" => {
                            lo = Some(current_text.trim().to_string())
                        }
                        "TIMESTAMP_SAMPLE_RATE" => ts_rate = Some(current_text.trim().to_string()),
                        _ => {}
                    }
                }
                stack.pop();
                current_text.clear();
            }
            Ok(Event::Eof) => break,
            Err(_) => return None,
            _ => {}
        }
        buf.clear();
    }
    let frame_duration = parse_ixml_rate(rate.as_deref()?)?;
    // The spec allows only DF/NDF (default NDF). Any other non-empty flag
    // is corrupt metadata — the display rate is unprovable, so no
    // timecode rather than a silently wrong label.
    let flag = flag.as_deref().map(str::trim).unwrap_or("");
    let drop_frame = if flag.is_empty() || flag.eq_ignore_ascii_case("NDF") {
        false
    } else if flag.eq_ignore_ascii_case("DF") {
        true
    } else {
        return None;
    };
    let timestamp = (|| {
        let lo: u64 = lo?.parse().ok()?;
        let rate: f64 = ts_rate?.parse().ok()?;
        if !rate.is_finite() || rate <= 0.0 {
            return None;
        }
        let hi: u64 = hi.and_then(|h| h.parse().ok()).unwrap_or(0);
        let samples: u64 = (u128::from(hi) << 32 | u128::from(lo)).try_into().ok()?;
        Some(IxmlTimestamp { samples, rate })
    })();
    Some(IxmlSpeed {
        frame_duration,
        drop_frame,
        timestamp,
    })
}

/// iXML `TIMECODE_RATE`: spec `num/den` ratios or observed decimal rates
/// (snapped through the canonical broadcast table).
fn parse_ixml_rate(text: &str) -> Option<MediaTime> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    if let Some((num, den)) = text.split_once('/') {
        let num: i64 = num.trim().parse().ok()?;
        let den: i64 = den.trim().parse().ok()?;
        if num <= 0 || den <= 0 || num > i32::MAX as i64 || den > i32::MAX as i64 {
            return None;
        }
        // Rate num/den → frame duration den/num.
        let duration = MediaTime::new(den, num as i32);
        return SourceTimecode::nominal_fps(duration)
            .filter(|n| (1..=120).contains(n))
            .map(|_| duration);
    }
    let rate: f64 = text.parse().ok()?;
    crate::timing::canonical_frame_duration(rate, None)
}

// ------------------------------------------------------------ Sony tail

/// Extract Sony NonRealTimeMeta XML from the last MiB of an MXF/MP4 file.
pub fn read_sony_tail(path: &Path) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(path).ok()?;
    let end = file.seek(SeekFrom::End(0)).ok()?;
    let count = end.min(1_048_576);
    file.seek(SeekFrom::Start(end - count)).ok()?;
    let mut buf = vec![0u8; count as usize];
    file.read_exact(&mut buf).ok()?;
    let raw = String::from_utf8_lossy(&buf);
    let start = raw.find("<NonRealTimeMeta")?;
    let end = raw[start..].find("</NonRealTimeMeta>")? + "</NonRealTimeMeta>".len();
    Some(raw[start..start + end].to_string())
}

struct SonyElement {
    name: String,
    attrs: HashMap<String, String>,
}

/// Correct streaming scan (Start pushes, Empty records leaf, End pops).
fn sony_elements(xml: &str) -> Vec<(SonyElement, Option<usize>)> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut out: Vec<(SonyElement, Option<usize>)> = Vec::new();
    let mut stack: Vec<usize> = Vec::new();
    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let idx = out.len();
                out.push((element_of(&e, &reader), stack.last().copied()));
                stack.push(idx);
            }
            Ok(Event::Empty(e)) => {
                out.push((element_of(&e, &reader), stack.last().copied()));
            }
            Ok(Event::End(_)) => {
                stack.pop();
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    out
}

fn element_of(e: &quick_xml::events::BytesStart<'_>, reader: &Reader<&[u8]>) -> SonyElement {
    let name = String::from_utf8_lossy(e.local_name().as_ref()).into_owned();
    let mut attrs = HashMap::new();
    for attr in e.attributes().flatten() {
        // Attribute names may carry namespace prefixes — compare raw and
        // local forms at query time instead.
        if let Ok(v) = attr.decode_and_unescape_value(reader.decoder()) {
            attrs.insert(
                String::from_utf8_lossy(attr.key.as_ref()).into_owned(),
                v.into_owned(),
            );
        }
    }
    SonyElement { name, attrs }
}

fn attr_local<'a>(attrs: &'a HashMap<String, String>, local: &str) -> Option<&'a str> {
    attrs.iter().find_map(|(k, v)| {
        let ln = k.rsplit(':').next().unwrap_or(k);
        if ln.eq_ignore_ascii_case(local) {
            Some(v.as_str())
        } else {
            None
        }
    })
}

/// Sony LTC timecode: first `LtcChange frameCount=0` value (8 digits,
/// FFSSMMHH pairs) with the parent table's `tcFps`, else `fallback`.
/// Always NDF (LTC carries no DF flag); ranges validated, never guessed.
pub fn sony_timecode(xml: &str, fallback: MediaTime) -> Option<SourceTimecode> {
    let elements = sony_elements(xml);
    let (idx, _) = elements.iter().enumerate().find(|(_, (e, _))| {
        e.name.eq_ignore_ascii_case("LtcChange") && attr_local(&e.attrs, "frameCount") == Some("0")
    })?;
    let (element, parent) = &elements[idx];
    let encoded = attr_local(&element.attrs, "value")?;
    if encoded.len() != 8 || !encoded.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let pairs: Vec<i64> = (0..8)
        .step_by(2)
        .map(|i| encoded[i..i + 2].parse().unwrap_or(-1))
        .collect();
    let (frames, seconds, minutes, hours) = (pairs[0], pairs[1], pairs[2], pairs[3]);
    let duration = parent
        .and_then(|p| {
            let fps: i64 = attr_local(&elements.get(p)?.0.attrs, "tcFps")?
                .parse()
                .ok()?;
            if (1..=120).contains(&fps) {
                Some(MediaTime::new(1, fps as i32))
            } else {
                None
            }
        })
        .unwrap_or(fallback);
    SourceTimecode::from_components(hours, minutes, seconds, frames, duration, false)
}

/// Sony `CreationDate value` (ISO 8601) → Unix seconds, else None.
pub fn sony_recording_date(xml: &str) -> Option<i64> {
    let elements = sony_elements(xml);
    let (_, (element, _)) = elements
        .iter()
        .enumerate()
        .find(|(_, (e, _))| e.name.eq_ignore_ascii_case("CreationDate"))?;
    parse_iso8601(attr_local(&element.attrs, "value")?)
}

/// `device:manufacturer:model:serial`, rejecting sentinel/blank serials
/// (mirrors `isUsableSerial`: empty, "0", u32::MAX, all-F).
pub fn sony_device_id(xml: &str) -> Option<String> {
    let elements = sony_elements(xml);
    let (_, (element, _)) = elements
        .iter()
        .enumerate()
        .find(|(_, (e, _))| e.name.eq_ignore_ascii_case("Device"))?;
    let manufacturer = attr_local(&element.attrs, "manufacturer")?;
    let model = attr_local(&element.attrs, "modelName")?;
    let serial = attr_local(&element.attrs, "serialNo")?;
    let normalized = serial.trim().to_uppercase();
    if normalized.is_empty()
        || normalized == "0"
        || normalized == "4294967295"
        || normalized.bytes().all(|b| b == b'F')
    {
        return None;
    }
    Some(format!("device:{manufacturer}:{model}:{serial}"))
}

/// Minimal ISO 8601 (`YYYY-MM-DDTHH:MM:SS[.frac][Z|±HH[:MM]]`) → Unix.
/// Only what camera sidecars emit; anything else is None, never guessed.
pub fn parse_iso8601(text: &str) -> Option<i64> {
    let (date_part, time_part) = text.split_once(['T', 't'])?;
    let d: Vec<i64> = date_part
        .split('-')
        .filter_map(|s| s.parse().ok())
        .collect();
    if d.len() != 3 {
        return None;
    }
    // Split timezone suffix (cameras always emit one; refuse to assume).
    let (clock, offset_secs) = if let Some(stripped) = time_part.strip_suffix(['Z', 'z']) {
        (stripped, 0i64)
    } else {
        let i = time_part.rfind(['+', '-']).filter(|&i| i > 0)?;
        let (clock, zone) = time_part.split_at(i);
        let sign = if zone.starts_with('+') { -1 } else { 1 };
        let digits: String = zone[1..].chars().filter(|c| *c != ':').collect();
        if digits.len() != 2 && digits.len() != 4 {
            return None;
        }
        let zh: i64 = digits.get(..2)?.parse().ok()?;
        let zm: i64 = digits.get(2..).map(|s| s.parse().unwrap_or(0)).unwrap_or(0);
        if zh > 23 || zm > 59 {
            return None;
        }
        (clock, sign * (zh * 3600 + zm * 60))
    };
    let t: Vec<&str> = clock.split(':').collect();
    if t.len() != 3 {
        return None;
    }
    let (hh, mm): (i64, i64) = (t[0].parse().ok()?, t[1].parse().ok()?);
    let ss: i64 = t[2].split(['.', ',']).next()?.parse().ok()?;
    if hh > 23 || mm > 59 || ss > 60 {
        return None;
    }
    let days = days_from_civil(d[0], d[1], d[2])?;
    Some(days * 86_400 + hh * 3600 + mm * 60 + ss + offset_secs)
}

// ------------------------------------------------------------ timecode text

/// `HH:MM:SS:FF` (`;` = drop-frame) at integer `fps` → [`SourceTimecode`].
/// Used for ffprobe `timecode` tags; the integer rate snaps through the
/// canonical broadcast table (`29.97` is not expressible here — rational
/// callers use [`SourceTimecode::from_label`] directly). Ranges, DF rates
/// and skipped DF labels validated, never guessed.
pub fn parse_timecode_string(text: &str, fps: i64) -> Option<SourceTimecode> {
    if fps <= 0 || fps > 120 {
        return None;
    }
    let duration = crate::timing::canonical_frame_duration(fps as f64, None)
        .unwrap_or_else(|| MediaTime::new(1, fps as i32));
    SourceTimecode::from_label(text, duration)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_bext_wav(dir: &std::path::Path, name: &str) -> std::path::PathBuf {
        // Minimal RIFF/WAVE + fmt + bext(OriginationDate/Time/TimeRef) + link + data.
        let path = dir.join(name);
        let mut wav: Vec<u8> = Vec::new();
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&[0u8; 4]); // size patch later
        wav.extend_from_slice(b"WAVE");
        // fmt : PCM 48 kHz stereo.
        wav.extend_from_slice(b"fmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
        wav.extend_from_slice(&2u16.to_le_bytes());
        wav.extend_from_slice(&48000u32.to_le_bytes());
        wav.extend_from_slice(&(48000u32 * 2 * 2).to_le_bytes());
        wav.extend_from_slice(&4u16.to_le_bytes());
        wav.extend_from_slice(&16u16.to_le_bytes());
        // bext v1: 346 bytes.
        let mut bext = vec![0u8; 346];
        bext[0..10].copy_from_slice(b"TestArtist");
        bext[320..330].copy_from_slice(b"2024-05-06");
        bext[330..338].copy_from_slice(b"12:34:56");
        let tref: u64 = ((12 * 3600 + 34 * 60 + 56) as u64) * 48000;
        bext[338..346].copy_from_slice(&tref.to_le_bytes());
        wav.extend_from_slice(b"bext");
        wav.extend_from_slice(&(346u32).to_le_bytes());
        wav.extend_from_slice(&bext);
        // data: 1 s of silence.
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&(48000u32 * 4).to_le_bytes());
        wav.extend_from_slice(&vec![0u8; 48000 * 4]);
        let riff_size = (wav.len() - 8) as u32;
        wav[4..8].copy_from_slice(&riff_size.to_le_bytes());
        std::fs::write(&path, &wav).unwrap();
        path
    }

    fn tmp(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("align-meta-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn bext_date_timecode_and_rate() {
        let dir = tmp("bext");
        let wav = write_bext_wav(&dir, "take.wav");
        let meta = read_bwf(&wav).expect("bwf");
        // 2024-05-06T12:34:56Z = 1714953600 + 45296.
        assert_eq!(meta.recording_date, Some(1_714_998_896));
        assert_eq!(meta.time_reference, Some(45_296 * 48_000));
        assert_eq!(meta.sample_rate, Some(48000.0));
        let tc = meta.timecode.expect("timecode");
        assert_eq!(tc.text, "12:34:56:00");
        // Sample-accurate elapsed, not a 25 fps quantization.
        assert_eq!(tc.frame_number, 45_296_i64 * 48_000);
        assert_eq!(tc.frame_duration, MediaTime::new(1, 48_000));
        assert!((tc.as_seconds() - 45_296.0).abs() < 1e-9);
        assert!(!tc.drop_frame);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bext_time_reference_is_not_frame_quantized() {
        // One sample past the frame grid: display label unchanged, elapsed
        // moves by exactly one sample (the old code rounded to 25 fps).
        let dir = tmp("bext1");
        let path = dir.join("one.wav");
        let mut wav: Vec<u8> = Vec::new();
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&[0u8; 4]);
        wav.extend_from_slice(b"WAVE");
        wav.extend_from_slice(b"fmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&2u16.to_le_bytes());
        wav.extend_from_slice(&48000u32.to_le_bytes());
        wav.extend_from_slice(&(48000u32 * 2 * 2).to_le_bytes());
        wav.extend_from_slice(&4u16.to_le_bytes());
        wav.extend_from_slice(&16u16.to_le_bytes());
        let mut bext = vec![0u8; 346];
        bext[320..330].copy_from_slice(b"2024-05-06");
        bext[330..338].copy_from_slice(b"12:34:56");
        let tref: u64 = 45_296 * 48_000 + 1;
        bext[338..346].copy_from_slice(&tref.to_le_bytes());
        wav.extend_from_slice(b"bext");
        wav.extend_from_slice(&346u32.to_le_bytes());
        wav.extend_from_slice(&bext);
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&4u32.to_le_bytes());
        wav.extend_from_slice(&[0u8; 4]);
        let size = (wav.len() - 8) as u32;
        wav[4..8].copy_from_slice(&size.to_le_bytes());
        std::fs::write(&path, &wav).unwrap();
        let meta = read_bwf(&path).expect("bwf");
        let tc = meta.timecode.expect("timecode");
        assert_eq!(tc.text, "12:34:56:00");
        assert_eq!(tc.frame_number, tref as i64);
        assert!((tc.as_seconds() - (45_296.0 + 1.0 / 48_000.0)).abs() < 1e-9);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn recording_date_needs_sub_24h_reference() {
        // 2024-05-06 midnight UTC = 1714953600.
        let day = Some("2024-05-06");
        let at = |secs: f64| {
            make_recording_date(day, None, Some((secs * 48_000.0) as u64), Some(48_000.0))
        };
        assert_eq!(at(0.0), Some(1_714_953_600));
        assert_eq!(at(86_399.0), Some(1_714_953_600 + 86_399));
        // Exactly/at 24 h and beyond: next unprovable day, no date.
        // (Display label still wraps, elapsed still exact — see
        // elapsed_never_wraps_display_wraps_at_24h.)
        assert_eq!(at(86_400.0), None);
        assert_eq!(at(86_401.0), None);
        assert_eq!(at(172_800.0), None);
    }

    #[test]
    fn non_wave_files_are_none() {
        let dir = tmp("neg");
        let mp3 = dir.join("a.mp3");
        std::fs::write(&mp3, b"ID3....fake").unwrap();
        assert!(read_bwf(&mp3).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn link_span_parses_and_validates() {
        let xml = br#"<?xml version="1.0"?><LINK><ID>SET9</ID>
            <FILE type="actual"><FILENUMBER>2</FILENUMBER><FILENAME>TAKE_02.WAV</FILENAME></FILE>
            <FILE><FILENUMBER>1</FILENUMBER><FILENAME>TAKE_01.WAV</FILENAME></FILE>
        </LINK>"#;
        let span = parse_media_span(xml, "take_02.wav").expect("span");
        assert_eq!(span.part_number, 2);
        assert_eq!(span.part_count, 2);
        assert!(span.identifier.starts_with("bwf:"));
        // Deterministic identity.
        assert_eq!(parse_media_span(xml, "take_02.wav"), Some(span.clone()));
        // Wrong filename / single file / bad numbering rejected.
        assert!(parse_media_span(xml, "other.wav").is_none());
        let single = br#"<LINK><FILE type="actual"><FILENUMBER>1</FILENUMBER><FILENAME>A.WAV</FILENAME></FILE></LINK>"#;
        assert!(parse_media_span(single, "a.wav").is_none());
        let dup = br#"<LINK>
            <FILE type="actual"><FILENUMBER>1</FILENUMBER><FILENAME>A.WAV</FILENAME></FILE>
            <FILE><FILENUMBER>1</FILENUMBER><FILENAME>B.WAV</FILENAME></FILE></LINK>"#;
        assert!(parse_media_span(dup, "a.wav").is_none());
    }

    const SONY: &str = r#"<NonRealTimeMeta>
        <Device manufacturer="Sony" modelName="PXW-Z280" serialNo="1234567"/>
        <CreationDate value="2024-05-06T12:34:56+09:00"/>
        <Table tcFps="25"><LtcChange frameCount="0" value="10123456"/></Table>
    </NonRealTimeMeta>"#;

    #[test]
    fn sony_tail_fields() {
        // LtcChange value "07123405" → pairs FF=07 SS=12 MM=34 HH=05.
        let xml = SONY.replace("10123456", "07123405");
        let tc = sony_timecode(&xml, MediaTime::new(1, 25)).expect("tc");
        assert_eq!(tc.text, "05:34:12:07");
        assert_eq!(tc.frame_number, 501_307);
        assert_eq!(tc.frame_duration, MediaTime::new(1, 25));
        // 2024-05-06T12:34:56+09:00 = 1714998896 − 32400.
        assert_eq!(sony_recording_date(&xml), Some(1_714_966_496));
        assert_eq!(
            sony_device_id(&xml).as_deref(),
            Some("device:Sony:PXW-Z280:1234567")
        );
        // Sentinel serials rejected.
        for bad in ["0", "4294967295", "FFFFFFFF", "   "] {
            let x = SONY.replace("1234567", bad);
            assert!(sony_device_id(&x).is_none(), "{bad}");
        }
    }

    #[test]
    fn iso8601_zones() {
        assert_eq!(parse_iso8601("2024-05-06T12:34:56Z"), Some(1_714_998_896));
        assert_eq!(
            parse_iso8601("2024-05-06T12:34:56.789+09:00"),
            Some(1_714_966_496)
        );
        assert_eq!(
            parse_iso8601("2024-05-06T00:00:00-0500"),
            Some(1_714_971_600)
        );
        assert!(parse_iso8601("2024-05-06 12:34:56").is_none());
        assert!(parse_iso8601("2024-05-06T12:34:56").is_none());
        assert!(parse_iso8601("garbage").is_none());
    }

    #[test]
    fn timecode_strings() {
        use crate::model::SourceTimecode;
        let tc = parse_timecode_string("01:02:03:12", 25).expect("tc");
        assert_eq!(tc.frame_number, 93_087);
        assert!(!tc.drop_frame);
        let df = parse_timecode_string("01:02:03;12", 30).expect("df");
        assert!(df.drop_frame);
        assert_eq!(df.text, "01:02:03;12");
        // Nominal 111702 minus 2×(62−6) drops.
        assert_eq!(df.frame_number, 111_702 - 112);
        assert!(parse_timecode_string("01:02:03:30", 30).is_none());
        assert!(parse_timecode_string("25:00:00:00", 25).is_none());
        assert!(parse_timecode_string("nope", 25).is_none());
        // Drop-frame is forbidden outside nominal 30/60 (SMPTE ST 12-1).
        assert!(parse_timecode_string("01:02:03;12", 25).is_none());
        assert!(parse_timecode_string("01:02:03;12", 24).is_none());
        assert!(parse_timecode_string("01:02:03;12", 50).is_none());
        // Skipped DF labels at a non-10th minute start are invalid.
        assert!(parse_timecode_string("00:01:00;00", 30).is_none());
        assert!(parse_timecode_string("00:01:00;01", 30).is_none());
        assert!(parse_timecode_string("00:10:00;00", 30).is_some());
        // 29.97 NDF uses the true 1001/30000 rate, not 1/30.
        let ndf =
            SourceTimecode::from_label("01:00:00:00", MediaTime::new(1001, 30_000)).expect("ndf");
        assert_eq!(ndf.frame_number, 108_000);
        assert!((ndf.as_seconds() - 108_000.0 * 1001.0 / 30_000.0).abs() < 1e-9);
    }

    #[test]
    fn drop_frame_29_97_anchors_and_transitions() {
        use crate::model::SourceTimecode;
        const D29: MediaTime = MediaTime {
            value: 1001,
            timescale: 30_000,
        };
        // Reference anchor: 01:00:00;00 DF = elapsed frame 107892.
        let hour = SourceTimecode::from_label("01:00:00;00", D29).expect("hour");
        assert_eq!(hour.frame_number, 107_892);
        assert!(hour.drop_frame);
        assert!((hour.as_seconds() - 107_892.0 * 1001.0 / 30_000.0).abs() < 1e-9);
        // Minute transition skips ;00 and ;01 (non-10th minute).
        let last = SourceTimecode::from_label("00:00:59;29", D29).expect("last");
        assert_eq!(last.frame_number, 1799);
        let next = SourceTimecode::from_frame_number(1800, D29, true).expect("next");
        assert_eq!(next.text, "00:01:00;02");
        // 10th minute carries no skip.
        let before10 = SourceTimecode::from_label("00:09:59;29", D29).expect("before10");
        assert_eq!(before10.frame_number, 17_981);
        let ten = SourceTimecode::from_frame_number(17_982, D29, true).expect("ten");
        assert_eq!(ten.text, "00:10:00;00");
        assert!(SourceTimecode::from_label("00:10:00;00", D29).is_some());
        // Hour transition.
        let before_hour = SourceTimecode::from_label("00:59:59;29", D29).expect("before-hour");
        assert_eq!(before_hour.frame_number, 107_891);
        let hour_label = SourceTimecode::from_frame_number(107_892, D29, true).expect("hour");
        assert_eq!(hour_label.text, "01:00:00;00");
        // Round-trip across two drop boundaries.
        for fn_ in [
            0i64, 1, 1799, 1800, 1801, 17_981, 17_982, 53_946, 107_891, 107_892, 2_589_407,
        ] {
            let tc = SourceTimecode::from_frame_number(fn_, D29, true).expect("tc");
            let back = SourceTimecode::from_label(&tc.text, D29).expect("back");
            assert_eq!(back.frame_number, fn_, "round-trip {}", tc.text);
        }
    }

    #[test]
    fn drop_frame_59_94_anchors() {
        use crate::model::SourceTimecode;
        const D59: MediaTime = MediaTime {
            value: 1001,
            timescale: 60_000,
        };
        // 01:00:00;00 = 216000 nominal minus 4×(60−6) drops.
        let hour = SourceTimecode::from_label("01:00:00;00", D59).expect("hour");
        assert_eq!(hour.frame_number, 216_000 - 216);
        // Four skipped labels at a non-10th minute start.
        for bad in ["00:01:00;00", "00:01:00;01", "00:01:00;02", "00:01:00;03"] {
            assert!(SourceTimecode::from_label(bad, D59).is_none(), "{bad}");
        }
        assert!(SourceTimecode::from_label("00:01:00;04", D59).is_some());
        assert!(SourceTimecode::from_label("00:10:00;00", D59).is_some());
        // Minute transition: 3599 → 00:01:00;04.
        let last = SourceTimecode::from_label("00:00:59;59", D59).expect("last");
        assert_eq!(last.frame_number, 3599);
        let next = SourceTimecode::from_frame_number(3600, D59, true).expect("next");
        assert_eq!(next.text, "00:01:00;04");
        // Round-trip incl. the 24 h wrap edge (day = 144 × 35964).
        for fn_ in [0i64, 3599, 3600, 215_783, 215_784, 5_178_815] {
            let tc = SourceTimecode::from_frame_number(fn_, D59, true).expect("tc");
            let back = SourceTimecode::from_label(&tc.text, D59).expect("back");
            assert_eq!(back.frame_number, fn_, "round-trip {}", tc.text);
        }
    }

    #[test]
    fn drop_frame_full_day_round_trip() {
        use crate::model::SourceTimecode;
        // Every elapsed frame of a 24 h day, both DF rates: label and back.
        for (value, timescale, day) in [
            (1001i64, 30_000i32, 2_589_408i64),
            (1001, 60_000, 5_178_816),
        ] {
            let duration = MediaTime::new(value, timescale);
            for n in 0..day {
                let tc = SourceTimecode::from_frame_number(n, duration, true).expect("label");
                let back = SourceTimecode::from_label(&tc.text, duration).expect("parse");
                assert_eq!(back.frame_number, n, "{} {n}", tc.text);
            }
        }
        // NDF day boundaries: display wraps, elapsed within the day returns.
        for (value, timescale, nominal) in [(1i64, 25i32, 25i64), (1, 30, 30)] {
            let duration = MediaTime::new(value, timescale);
            let day = 24 * 3600 * nominal;
            for n in [
                0,
                1,
                nominal - 1,
                nominal,
                day - 1,
                day,
                day + 1,
                2 * day - 1,
            ] {
                let tc = SourceTimecode::from_frame_number(n, duration, false).expect("label");
                let back = SourceTimecode::from_label(&tc.text, duration).expect("parse");
                assert_eq!(back.frame_number, n % day, "{n}");
            }
        }
    }

    #[test]
    fn elapsed_never_wraps_display_wraps_at_24h() {
        use crate::model::SourceTimecode;
        // 25 fps: one full day plus 5 frames — elapsed kept, label wrapped.
        let tc = SourceTimecode::from_frame_number(25 * 86_400 + 5, MediaTime::new(1, 25), false)
            .expect("tc");
        assert_eq!(tc.frame_number, 25 * 86_400 + 5);
        assert_eq!(tc.text, "00:00:00:05");
        assert!((tc.as_seconds() - (86_400.0 + 0.2)).abs() < 1e-9);
        // BWF past midnight: TimeReference 24 h + 7 samples at 48 kHz.
        let bwf = SourceTimecode::from_samples(
            86_400 * 48_000 + 7,
            48_000.0,
            MediaTime::new(1, 25),
            false,
        )
        .expect("bwf");
        assert_eq!(bwf.text, "00:00:00:00");
        assert!((bwf.as_seconds() - (86_400.0 + 7.0 / 48_000.0)).abs() < 1e-9);
        // Derived DF flag at a non-DF rate sanitizes to NDF (count stands).
        let sane =
            SourceTimecode::from_frame_number(100, MediaTime::new(1, 25), true).expect("sane");
        assert!(!sane.drop_frame);
        assert_eq!(sane.text, "00:00:04:00");
    }

    const IXML_SPEED: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<BWFXML>
  <IXML_VERSION>1.54</IXML_VERSION>
  <PROJECT>Reel</PROJECT>
  <SPEED>
    <MASTER_SPEED>25/1</MASTER_SPEED>
    <TIMECODE_RATE>30000/1001</TIMECODE_RATE>
    <TIMECODE_FLAG>DF</TIMECODE_FLAG>
    <FILE_SAMPLE_RATE>48000</FILE_SAMPLE_RATE>
    <TIMESTAMP_SAMPLES_SINCE_MIDNIGHT_HI>0</TIMESTAMP_SAMPLES_SINCE_MIDNIGHT_HI>
    <TIMESTAMP_SAMPLES_SINCE_MIDNIGHT_LO>172800000</TIMESTAMP_SAMPLES_SINCE_MIDNIGHT_LO>
    <TIMESTAMP_SAMPLE_RATE>48000</TIMESTAMP_SAMPLE_RATE>
  </SPEED>
</BWFXML>"#;

    #[test]
    fn ixml_speed_parses_rate_flag_and_timestamp() {
        let speed = parse_ixml_speed(IXML_SPEED).expect("speed");
        assert_eq!(speed.frame_duration, MediaTime::new(1001, 30_000));
        assert!(speed.drop_frame);
        let ts = speed.timestamp.expect("timestamp");
        assert_eq!(ts.samples, 172_800_000);
        assert_eq!(ts.rate, 48_000.0);
        // Decimal rates (observed from field recorders) snap identically.
        let dec = parse_ixml_speed(IXML_SPEED.replace("30000/1001", "29.97002997003").as_str())
            .expect("decimal");
        assert_eq!(dec.frame_duration, MediaTime::new(1001, 30_000));
        // Flag defaults to NDF; rate is mandatory, not guessed.
        let ndf = parse_ixml_speed(
            IXML_SPEED
                .replace("<TIMECODE_FLAG>DF</TIMECODE_FLAG>", "")
                .as_str(),
        )
        .expect("ndf default");
        assert!(!ndf.drop_frame);
        assert!(parse_ixml_speed("<BWFXML></BWFXML>").is_none());
        assert!(parse_ixml_speed(IXML_SPEED.replace("30000/1001", "bogus").as_str()).is_none());
        assert!(parse_ixml_speed("not xml at all <").is_none());
        // Unknown non-empty flags are corrupt, not silent NDF
        // (surrounding whitespace is still tolerated).
        for bad in ["XX", "DROP", "ND", "0", "D"] {
            let xml = IXML_SPEED.replace(">DF<", &format!(">{bad}<"));
            assert!(parse_ixml_speed(&xml).is_none(), "{bad:?}");
        }
    }

    fn write_ixml_wav(dir: &std::path::Path, name: &str, payload: &[u8]) -> std::path::PathBuf {
        let path = dir.join(name);
        let mut wav: Vec<u8> = Vec::new();
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&[0u8; 4]);
        wav.extend_from_slice(b"WAVE");
        wav.extend_from_slice(b"fmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&2u16.to_le_bytes());
        wav.extend_from_slice(&48000u32.to_le_bytes());
        wav.extend_from_slice(&(48000u32 * 2 * 2).to_le_bytes());
        wav.extend_from_slice(&4u16.to_le_bytes());
        wav.extend_from_slice(&16u16.to_le_bytes());
        wav.extend_from_slice(b"iXML");
        wav.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        wav.extend_from_slice(payload);
        if payload.len() % 2 == 1 {
            wav.push(0);
        }
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&8u32.to_le_bytes());
        wav.extend_from_slice(&[0u8; 8]);
        let size = (wav.len() - 8) as u32;
        wav[4..8].copy_from_slice(&size.to_le_bytes());
        std::fs::write(&path, &wav).unwrap();
        path
    }

    #[test]
    fn ixml_chunk_single_pass_with_padding_rules() {
        let dir = tmp("ixml");
        // Clean payload arrives through the same single walk as bext/fmt.
        let path = write_ixml_wav(&dir, "take.wav", IXML_SPEED.as_bytes());
        let meta = read_bwf(&path).expect("bwf");
        let speed = meta.ixml.expect("ixml");
        assert_eq!(speed.frame_duration, MediaTime::new(1001, 30_000));
        assert!(speed.drop_frame);
        // Trailing NUL padding inside the payload is convention, not data.
        let mut padded = IXML_SPEED.as_bytes().to_vec();
        padded.extend_from_slice(&[0, 0, 0]);
        let path = write_ixml_wav(&dir, "padded.wav", &padded);
        assert!(read_bwf(&path).expect("bwf").ixml.is_some());
        // Garbage after </BWFXML> is corruption, never metadata.
        let mut garbage = IXML_SPEED.as_bytes().to_vec();
        garbage.extend_from_slice(b"TRAILER");
        let path = write_ixml_wav(&dir, "garbage.wav", &garbage);
        assert!(read_bwf(&path).expect("bwf").ixml.is_none());
        // Garbage instead of XML is not metadata either.
        let path = write_ixml_wav(&dir, "junk.wav", b"JUNKJUNKJUNKJUNK");
        assert!(read_bwf(&path).expect("bwf").ixml.is_none());
        assert!(read_bwf(&dir.join("missing.wav")).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn audio_timecode_priority_is_bext_first() {
        // EBU precedence without file I/O: a disagreeing iXML timestamp
        // copy never moves elapsed; iXML only sets the display label.
        let ixml = parse_ixml_speed(IXML_SPEED).expect("speed");
        let meta = BwfMetadata {
            recording_date: None,
            media_span: None,
            time_reference: Some(100 * 48_000),
            sample_rate: Some(48_000.0),
            timecode: None,
            ixml: Some(ixml),
        };
        let tc = meta.audio_timecode().expect("tc");
        assert!((tc.as_seconds() - 100.0).abs() < 1e-9);
        assert!(tc.drop_frame);
        assert_eq!(tc.text, "00:01:39;29");
        // No bext: the iXML timestamp copy (3600 s) is the fallback.
        let bare = BwfMetadata {
            ixml: Some(ixml),
            ..Default::default()
        };
        let tc = bare.audio_timecode().expect("tc");
        assert!((tc.as_seconds() - 3600.0).abs() < 1e-9);
        assert_eq!(tc.text, "01:00:00;00");
        // Nothing at all: None, never guessed.
        assert!(BwfMetadata::default().audio_timecode().is_none());
    }
}
