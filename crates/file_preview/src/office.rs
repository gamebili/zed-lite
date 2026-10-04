use crate::{CELL_BYTES, PAGE_BYTES, PAGE_ROWS, PreviewPage, PreviewRequest};
use anyhow::{Context, Result, ensure};
use quick_xml::{Reader, events::Event};
use std::{
    fs::{self, File},
    io::{self, BufRead, BufReader, Read, Seek, SeekFrom},
    path::Path,
    time::{Duration, Instant, SystemTime},
};
use zip::ZipArchive;

const DIRECTORY_BYTES: u64 = 8 * 1024 * 1024;
const ARCHIVE_ENTRIES: u64 = 4096;
const XML_BYTES: u64 = 16 * 1024 * 1024;
const READ_BYTES: u64 = 64 * 1024 * 1024;
const SHARED_STRING_BYTES: usize = 2 * 1024 * 1024;
const COMPOUND_BYTES: u64 = 32 * 1024 * 1024;
const TIME_LIMIT: Duration = Duration::from_secs(5);
const OLE_SIGNATURE: &[u8; 8] = b"\xd0\xcf\x11\xe0\xa1\xb1\x1a\xe1";

fn sheet_section(name: &str) -> String {
    format!("Sheet: {name}")
}

struct BoundedReader<R> {
    inner: R,
    remaining: u64,
    deadline: Instant,
}

impl<R> BoundedReader<R> {
    fn new(inner: R, remaining: u64, deadline: Instant) -> Self {
        Self {
            inner,
            remaining,
            deadline,
        }
    }
}

impl<R: Read> Read for BoundedReader<R> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if output.is_empty() {
            return Ok(0);
        }
        if Instant::now() >= self.deadline {
            return Err(io::Error::other(
                "Office preview reached its execution time limit",
            ));
        }
        if self.remaining == 0 {
            return Err(io::Error::other(
                "Office preview reached its bounded read limit",
            ));
        }
        let length = output.len().min(self.remaining as usize);
        let count = self.inner.read(&mut output[..length])?;
        self.remaining -= count as u64;
        Ok(count)
    }
}

impl<R: Seek> Seek for BoundedReader<R> {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        if Instant::now() >= self.deadline {
            return Err(io::Error::other(
                "Office preview reached its execution time limit",
            ));
        }
        self.inner.seek(position)
    }
}

type Archive = ZipArchive<BoundedReader<File>>;

fn unsigned16(bytes: &[u8], offset: usize) -> Result<u16> {
    let value = bytes
        .get(offset..offset + 2)
        .context("Truncated Office container")?;
    Ok(u16::from_le_bytes(value.try_into()?))
}

fn unsigned32(bytes: &[u8], offset: usize) -> Result<u32> {
    let value = bytes
        .get(offset..offset + 4)
        .context("Truncated Office container")?;
    Ok(u32::from_le_bytes(value.try_into()?))
}

fn unsigned64(bytes: &[u8], offset: usize) -> Result<u64> {
    let value = bytes
        .get(offset..offset + 8)
        .context("Truncated Office container")?;
    Ok(u64::from_le_bytes(value.try_into()?))
}

fn number(bytes: &[u8], offset: usize) -> Result<String> {
    let value = bytes
        .get(offset..offset + 8)
        .context("Truncated spreadsheet number")?;
    Ok(f64::from_le_bytes(value.try_into()?).to_string())
}

fn rk_number(value: u32) -> String {
    let number = if value & 2 != 0 {
        f64::from((value as i32) >> 2)
    } else {
        f64::from_bits(u64::from(value & !3) << 32)
    };
    (if value & 1 != 0 {
        number / 100.0
    } else {
        number
    })
    .to_string()
}

fn wide_string(bytes: &[u8], offset: usize) -> Result<(String, usize)> {
    let count = unsigned32(bytes, offset)? as usize;
    ensure!(
        count <= CELL_BYTES / 2,
        "Spreadsheet text exceeds the bounded preview limit"
    );
    let end = offset
        .checked_add(4)
        .and_then(|offset| offset.checked_add(count * 2))
        .context("Spreadsheet string size overflow")?;
    let bytes = bytes
        .get(offset + 4..end)
        .context("Truncated spreadsheet Unicode string")?;
    let units = bytes
        .chunks_exact(2)
        .map(|value| u16::from_le_bytes([value[0], value[1]]))
        .collect::<Vec<_>>();
    Ok((
        String::from_utf16(&units).context("Invalid spreadsheet Unicode string")?,
        end,
    ))
}

fn binary_record(reader: &mut impl Read) -> Result<Option<(u16, Vec<u8>)>> {
    let mut first = [0u8; 1];
    if reader.read(&mut first)? == 0 {
        return Ok(None);
    }
    let kind = if first[0] & 0x80 == 0 {
        u16::from(first[0])
    } else {
        let mut second = [0u8; 1];
        reader.read_exact(&mut second)?;
        ensure!(second[0] & 0x80 == 0, "Invalid XLSB record type");
        u16::from(first[0] & 0x7f) | (u16::from(second[0]) << 7)
    };
    let mut length = 0usize;
    for position in 0..4 {
        let mut byte = [0u8; 1];
        reader.read_exact(&mut byte)?;
        length |= usize::from(byte[0] & 0x7f) << (position * 7);
        if byte[0] & 0x80 == 0 {
            break;
        }
        ensure!(position != 3, "Invalid XLSB record size");
    }
    ensure!(
        length <= 1024 * 1024,
        "XLSB record exceeds the 1 MiB preview limit"
    );
    let mut data = vec![0; length];
    reader.read_exact(&mut data)?;
    Ok(Some((kind, data)))
}

fn binary_shared_strings(archive: &mut Archive, deadline: Instant) -> Result<Vec<String>> {
    if archive.index_for_name("xl/sharedStrings.bin").is_none() {
        return Ok(Vec::new());
    }
    let mut reader = BoundedReader::new(
        archive.by_name("xl/sharedStrings.bin")?,
        XML_BYTES,
        deadline,
    );
    let mut strings = Vec::new();
    let mut bytes = 0usize;
    while let Some((kind, data)) = binary_record(&mut reader)? {
        if kind == 0x0013 {
            let (value, _) = wide_string(&data, 1)?;
            bytes = bytes.saturating_add(value.len());
            ensure!(
                bytes <= SHARED_STRING_BYTES && strings.len() < 100_000,
                "XLSB shared strings exceed the bounded preview limit"
            );
            strings.push(value);
        }
    }
    Ok(strings)
}

fn binary_spreadsheet(
    archive: &mut Archive,
    request: &PreviewRequest,
    deadline: Instant,
) -> Result<PreviewPage> {
    let mut sheets = archive
        .file_names()
        .filter(|name| name.starts_with("xl/worksheets/") && name.ends_with(".bin"))
        .map(str::to_string)
        .collect::<Vec<_>>();
    sheets.sort();
    let sections = sheets
        .iter()
        .map(|name| sheet_section(name))
        .collect::<Vec<_>>();
    let selected = request
        .section
        .as_deref()
        .or_else(|| sections.first().map(String::as_str))
        .context("XLSB workbook contains no worksheets")?;
    ensure!(
        sections.iter().any(|name| name == selected),
        "Requested XLSB worksheet is missing"
    );
    let strings = binary_shared_strings(archive, deadline)?;
    let part = selected
        .strip_prefix("Sheet: ")
        .context("Invalid XLSB worksheet section")?;
    let mut reader = BoundedReader::new(archive.by_name(part)?, XML_BYTES, deadline);
    let mut builder = PageBuilder::new(
        selected,
        &["Row", "Column", "Type", "Value", "Formula tokens"],
        request,
    );
    builder.page.sections = sections;
    let mut row = 0u32;
    while let Some((kind, data)) = binary_record(&mut reader)? {
        if kind == 0 {
            row = unsigned32(&data, 0)?;
            continue;
        }
        if kind == 0x0092 {
            break;
        }
        let (cell_type, value, formula_start) = match kind {
            0x0002 => ("number", rk_number(unsigned32(&data, 8)?), None),
            0x0003 | 0x000b => (
                "error",
                format!(
                    "Excel error {}",
                    data.get(8).context("Truncated XLSB error")?
                ),
                (kind == 0x000b).then_some(11),
            ),
            0x0004 | 0x000a => (
                "boolean",
                (data.get(8).context("Truncated XLSB boolean")? != &0).to_string(),
                (kind == 0x000a).then_some(11),
            ),
            0x0005 | 0x0009 => ("number", number(&data, 8)?, (kind == 0x0009).then_some(18)),
            0x0006 | 0x0008 => {
                let (value, end) = wide_string(&data, 8)?;
                ("text", value, (kind == 0x0008).then_some(end + 2))
            }
            0x0007 => (
                "text",
                strings
                    .get(unsigned32(&data, 8)? as usize)
                    .context("XLSB shared string is missing")?
                    .clone(),
                None,
            ),
            _ => continue,
        };
        let column = unsigned32(&data, 0)?;
        let formula = if let Some(start) = formula_start {
            let length = unsigned32(&data, start)? as usize;
            let tokens = data
                .get(start + 4..start + 4 + length)
                .context("Truncated XLSB formula")?;
            tokens
                .iter()
                .take(CELL_BYTES / 3)
                .map(|value| format!("{value:02x}"))
                .collect::<Vec<_>>()
                .join(" ")
        } else {
            String::new()
        };
        if !builder.push(vec![
            (u64::from(row) + 1).to_string(),
            (u64::from(column) + 1).to_string(),
            cell_type.into(),
            value,
            formula,
        ]) {
            break;
        }
    }
    builder.page.note = Some("XLSB cell values and cached formula results are streamed from binary worksheet records. Formula tokens are displayed without execution; date and number formatting remains the stored numeric value.".into());
    Ok(builder.page)
}

fn biff_record(reader: &mut impl Read) -> Result<Option<(u16, Vec<u8>)>> {
    let mut header = [0u8; 4];
    if reader.read(&mut header[..1])? == 0 {
        return Ok(None);
    }
    reader.read_exact(&mut header[1..])?;
    let kind = unsigned16(&header, 0)?;
    let mut data = vec![0; usize::from(unsigned16(&header, 2)?)];
    reader.read_exact(&mut data)?;
    Ok(Some((kind, data)))
}

struct BiffStrings {
    chunks: Vec<Vec<u8>>,
    chunk: usize,
    offset: usize,
}

impl BiffStrings {
    fn byte(&mut self) -> Result<u8> {
        while self
            .chunks
            .get(self.chunk)
            .is_some_and(|chunk| self.offset == chunk.len())
        {
            self.chunk += 1;
            self.offset = 0;
        }
        let value = self
            .chunks
            .get(self.chunk)
            .and_then(|chunk| chunk.get(self.offset))
            .copied()
            .context("Truncated XLS shared string table")?;
        self.offset += 1;
        Ok(value)
    }

    fn short(&mut self) -> Result<u16> {
        Ok(u16::from_le_bytes([self.byte()?, self.byte()?]))
    }

    fn integer(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes([
            self.byte()?,
            self.byte()?,
            self.byte()?,
            self.byte()?,
        ]))
    }

    fn unicode(&mut self) -> Result<String> {
        let count = usize::from(self.short()?);
        ensure!(
            count <= CELL_BYTES / 2,
            "XLS shared string exceeds the bounded preview limit"
        );
        let mut flags = self.byte()?;
        let runs = if flags & 8 != 0 {
            usize::from(self.short()?)
        } else {
            0
        };
        let extension = if flags & 4 != 0 {
            self.integer()? as usize
        } else {
            0
        };
        ensure!(
            extension <= SHARED_STRING_BYTES,
            "XLS string extension exceeds the preview limit"
        );
        let mut units = Vec::with_capacity(count);
        for _ in 0..count {
            if self
                .chunks
                .get(self.chunk)
                .is_some_and(|chunk| self.offset == chunk.len())
            {
                self.chunk += 1;
                self.offset = 0;
                flags = self.byte()?;
            }
            let value = if flags & 1 != 0 {
                self.short()?
            } else {
                u16::from(self.byte()?)
            };
            units.push(value);
        }
        for _ in 0..runs.saturating_mul(4).saturating_add(extension) {
            self.byte()?;
        }
        let value = String::from_utf16(&units).context("Invalid XLS Unicode string")?;
        ensure!(
            value.len() <= CELL_BYTES,
            "XLS shared string exceeds the 4096 byte preview limit"
        );
        Ok(value)
    }
}

fn biff_text(bytes: &[u8], offset: usize) -> Result<String> {
    let mut cursor = BiffStrings {
        chunks: vec![bytes.get(offset..).context("Truncated XLS text")?.to_vec()],
        chunk: 0,
        offset: 0,
    };
    cursor.unicode()
}

fn biff_spreadsheet(
    stream: &mut (impl Read + Seek),
    request: &PreviewRequest,
) -> Result<PreviewPage> {
    let mut sheets = Vec::new();
    let mut strings = Vec::new();
    let mut pending = None;
    loop {
        let record = if pending.is_some() {
            pending.take()
        } else {
            biff_record(stream)?
        };
        let Some((kind, data)) = record else { break };
        match kind {
            0x002f => anyhow::bail!("This XLS workbook is password-protected"),
            0x0085 => {
                let offset = u64::from(unsigned32(&data, 0)?);
                let count = usize::from(*data.get(6).context("Truncated XLS sheet name")?);
                let flags = *data.get(7).context("Truncated XLS sheet name")?;
                let character_bytes = if flags & 1 != 0 { 2 } else { 1 };
                let name = data
                    .get(8..8 + count * character_bytes)
                    .context("Truncated XLS sheet name")?;
                let units = if character_bytes == 2 {
                    name.chunks_exact(2)
                        .map(|value| u16::from_le_bytes([value[0], value[1]]))
                        .collect::<Vec<_>>()
                } else {
                    name.iter().map(|value| u16::from(*value)).collect()
                };
                ensure!(
                    sheets.len() < ARCHIVE_ENTRIES as usize,
                    "XLS worksheet list exceeds the preview limit"
                );
                sheets.push((
                    sheet_section(&String::from_utf16(&units).context("Invalid XLS sheet name")?),
                    offset,
                ));
            }
            0x00fc => {
                let count = unsigned32(&data, 4)? as usize;
                ensure!(
                    count <= 100_000,
                    "XLS shared string count exceeds the preview limit"
                );
                let mut bytes = data.len();
                let mut chunks = vec![data];
                loop {
                    let next = biff_record(stream)?;
                    match next {
                        Some((0x003c, continuation)) => {
                            bytes = bytes.saturating_add(continuation.len());
                            ensure!(
                                bytes <= SHARED_STRING_BYTES,
                                "XLS shared strings exceed the 2 MiB preview limit"
                            );
                            chunks.push(continuation);
                        }
                        record => {
                            pending = record;
                            break;
                        }
                    }
                }
                let mut cursor = BiffStrings {
                    chunks,
                    chunk: 0,
                    offset: 8,
                };
                let mut decoded_bytes = 0usize;
                for _ in 0..count {
                    let value = cursor.unicode()?;
                    decoded_bytes = decoded_bytes.saturating_add(value.len());
                    ensure!(
                        decoded_bytes <= SHARED_STRING_BYTES,
                        "XLS shared strings exceed the 2 MiB preview limit"
                    );
                    strings.push(value);
                }
            }
            0x000a => break,
            _ => {}
        }
    }
    ensure!(
        !sheets.is_empty(),
        "XLS workbook contains no readable BIFF8 worksheets"
    );
    let selected = request.section.as_deref().unwrap_or(&sheets[0].0);
    let (_, offset) = sheets
        .iter()
        .find(|(name, _)| name == selected)
        .context("Requested XLS worksheet is missing")?;
    stream.seek(SeekFrom::Start(*offset))?;
    let mut builder = PageBuilder::new(
        selected,
        &["Row", "Column", "Type", "Value", "Formula tokens"],
        request,
    );
    builder.page.sections = sheets.iter().map(|(name, _)| name.clone()).collect();
    while let Some((kind, data)) = biff_record(stream)? {
        if kind == 0x000a {
            break;
        }
        if kind == 0x00bd {
            ensure!(
                data.len() >= 6 && (data.len() - 6) % 6 == 0,
                "Invalid XLS MulRK cell record"
            );
            let row = u64::from(unsigned16(&data, 0)?) + 1;
            let first_column = u64::from(unsigned16(&data, 2)?);
            let mut complete = false;
            for (index, cell) in data[4..data.len() - 2].chunks_exact(6).enumerate() {
                if !builder.push(vec![
                    row.to_string(),
                    (first_column + index as u64 + 1).to_string(),
                    "number".into(),
                    rk_number(unsigned32(cell, 2)?),
                    String::new(),
                ]) {
                    complete = true;
                    break;
                }
            }
            if complete {
                break;
            }
            continue;
        }
        let (cell_type, value, formula) = match kind {
            0x0203 => ("number", number(&data, 6)?, String::new()),
            0x027e => ("number", rk_number(unsigned32(&data, 6)?), String::new()),
            0x0204 | 0x00d6 => ("text", biff_text(&data, 6)?, String::new()),
            0x00fd => (
                "text",
                strings
                    .get(unsigned32(&data, 6)? as usize)
                    .context("XLS shared string is missing")?
                    .clone(),
                String::new(),
            ),
            0x0205 => {
                let value = *data.get(6).context("Truncated XLS boolean or error")?;
                if data.get(7) == Some(&0) {
                    ("boolean", (value != 0).to_string(), String::new())
                } else {
                    ("error", format!("Excel error {value}"), String::new())
                }
            }
            0x0006 => {
                let value = if unsigned16(&data, 12)? == u16::MAX {
                    match data.get(6) {
                        Some(0) => "String result stored in the following STRING record".into(),
                        Some(1) => (*data.get(8).context("Truncated XLS formula boolean")? != 0)
                            .to_string(),
                        Some(2) => format!(
                            "Excel error {}",
                            data.get(8).context("Truncated XLS formula error")?
                        ),
                        _ => String::new(),
                    }
                } else {
                    number(&data, 6)?
                };
                let length = usize::from(unsigned16(&data, 20)?);
                let tokens = data.get(22..22 + length).context("Truncated XLS formula")?;
                let formula = tokens
                    .iter()
                    .take(CELL_BYTES / 3)
                    .map(|value| format!("{value:02x}"))
                    .collect::<Vec<_>>()
                    .join(" ");
                ("formula result", value, formula)
            }
            _ => continue,
        };
        if !builder.push(vec![
            (u64::from(unsigned16(&data, 0)?) + 1).to_string(),
            (u64::from(unsigned16(&data, 2)?) + 1).to_string(),
            cell_type.into(),
            value,
            formula,
        ]) {
            break;
        }
    }
    builder.page.note = Some("BIFF8 XLS cells and cached formula results are read directly from the Workbook stream. Formula tokens are displayed without execution; date and number formatting remains the stored numeric value. Other legacy record types remain available in their container bytes.".into());
    Ok(builder.page)
}

#[derive(Default)]
struct LegacySlides {
    sections: Vec<String>,
    text: Vec<(String, u64, u32, u16)>,
    slides: usize,
    notes: usize,
    records: usize,
}

impl LegacySlides {
    fn inspect(
        &mut self,
        stream: &mut (impl Read + Seek),
        end: u64,
        depth: usize,
        section: &str,
    ) -> Result<()> {
        ensure!(
            depth <= 32,
            "Legacy PowerPoint nesting exceeds the preview limit"
        );
        while stream.stream_position()? < end {
            self.records += 1;
            ensure!(
                self.records <= 100_000,
                "Legacy PowerPoint record count exceeds the preview limit"
            );
            let mut header = [0u8; 8];
            stream.read_exact(&mut header)?;
            let version = unsigned16(&header, 0)? & 15;
            let kind = unsigned16(&header, 2)?;
            let length = unsigned32(&header, 4)?;
            let start = stream.stream_position()?;
            let record_end = start
                .checked_add(u64::from(length))
                .context("Legacy PowerPoint record size overflow")?;
            ensure!(
                record_end <= end,
                "Legacy PowerPoint record extends beyond its container"
            );
            if version == 15 {
                let child_section = match kind {
                    1006 => {
                        self.slides += 1;
                        Some(format!("Slide {}", self.slides))
                    }
                    1008 => {
                        self.notes += 1;
                        Some(format!("Notes {}", self.notes))
                    }
                    _ => None,
                };
                if let Some(label) = &child_section {
                    ensure!(
                        self.sections.len() < ARCHIVE_ENTRIES as usize,
                        "Legacy PowerPoint slide count exceeds the preview limit"
                    );
                    self.sections.push(label.clone());
                }
                self.inspect(
                    stream,
                    record_end,
                    depth + 1,
                    child_section.as_deref().unwrap_or(section),
                )?;
            } else if matches!(kind, 4000 | 4008 | 4026) {
                ensure!(
                    self.text.len() < 10_000,
                    "Legacy PowerPoint text record count exceeds the preview limit"
                );
                self.text.push((section.into(), start, length, kind));
            }
            stream.seek(SeekFrom::Start(record_end))?;
        }
        Ok(())
    }
}

fn legacy_presentation(
    stream: &mut (impl Read + Seek),
    request: &PreviewRequest,
    length: u64,
) -> Result<PreviewPage> {
    let mut content = LegacySlides::default();
    content.sections.push("Document".into());
    content.inspect(stream, length, 0, "Document")?;
    let selected = request
        .section
        .as_deref()
        .or_else(|| {
            content
                .sections
                .iter()
                .find(|name| name.starts_with("Slide "))
                .map(String::as_str)
        })
        .unwrap_or("Document");
    ensure!(
        content.sections.iter().any(|name| name == selected),
        "Requested legacy presentation slide is missing"
    );
    let mut builder = PageBuilder::new(selected, &["Text record", "Content"], request);
    builder.page.sections = content.sections.clone();
    for (section, offset, length, kind) in content.text {
        if section != selected {
            continue;
        }
        stream.seek(SeekFrom::Start(offset))?;
        let read_length = (length as usize).min(CELL_BYTES);
        let mut bytes = vec![0; read_length];
        stream.read_exact(&mut bytes)?;
        let text = if kind == 4008 {
            bytes.iter().map(|value| char::from(*value)).collect()
        } else {
            let units = bytes
                .chunks_exact(2)
                .map(|value| u16::from_le_bytes([value[0], value[1]]))
                .collect::<Vec<_>>();
            String::from_utf16_lossy(&units)
        };
        if !builder.push(vec![(builder.record + 1).to_string(), text]) {
            break;
        }
    }
    builder.page.note = Some("Legacy PowerPoint slide and notes text is decoded directly from document records. Layout, pictures, and animations require a presentation converter; individual text records are limited to 4096 bytes.".into());
    Ok(builder.page)
}

// ZIP parsers allocate their central directory before opening any entry. Check
// both ordinary and ZIP64 directories first, including sparse oversized inputs.
fn zip_preflight(file: &mut File) -> Result<()> {
    let length = file.metadata()?.len();
    ensure!(length >= 22, "Office file has no ZIP directory");
    let tail_length = length.min(65_557) as usize;
    file.seek(SeekFrom::Start(length - tail_length as u64))?;
    let mut tail = vec![0; tail_length];
    file.read_exact(&mut tail)?;
    let end = (0..=tail_length.saturating_sub(22))
        .rev()
        .find(|position| {
            tail.get(*position..*position + 4) == Some(b"PK\x05\x06")
                && unsigned16(&tail, *position + 20)
                    .is_ok_and(|comment| *position + 22 + usize::from(comment) == tail_length)
        })
        .context("Office file has an invalid ZIP directory")?;
    ensure!(
        unsigned16(&tail, end + 4)? == 0 && unsigned16(&tail, end + 6)? == 0,
        "Multipart Office archives cannot be previewed"
    );
    let mut count = u64::from(unsigned16(&tail, end + 10)?);
    let mut directory_size = u64::from(unsigned32(&tail, end + 12)?);
    let mut directory_offset = u64::from(unsigned32(&tail, end + 16)?);
    let end_offset = length - tail_length as u64 + end as u64;
    if count == u64::from(u16::MAX)
        || directory_size == u64::from(u32::MAX)
        || directory_offset == u64::from(u32::MAX)
    {
        ensure!(end_offset >= 20, "Office ZIP64 locator is missing");
        file.seek(SeekFrom::Start(end_offset - 20))?;
        let mut locator = [0; 20];
        file.read_exact(&mut locator)?;
        ensure!(
            &locator[..4] == b"PK\x06\x07"
                && unsigned32(&locator, 4)? == 0
                && unsigned32(&locator, 16)? == 1,
            "Invalid Office ZIP64 locator"
        );
        let record_offset = unsigned64(&locator, 8)?;
        ensure!(
            record_offset
                .checked_add(56)
                .is_some_and(|position| position <= end_offset - 20),
            "Invalid Office ZIP64 directory offset"
        );
        file.seek(SeekFrom::Start(record_offset))?;
        let mut record = [0; 56];
        file.read_exact(&mut record)?;
        ensure!(
            &record[..4] == b"PK\x06\x06",
            "Invalid Office ZIP64 directory"
        );
        ensure!(
            unsigned64(&record, 4)? <= DIRECTORY_BYTES
                && unsigned32(&record, 16)? == 0
                && unsigned32(&record, 20)? == 0,
            "Office ZIP64 directory is too large or multipart"
        );
        count = unsigned64(&record, 32)?;
        ensure!(
            unsigned64(&record, 24)? == count,
            "Inconsistent Office ZIP64 entry counts"
        );
        directory_size = unsigned64(&record, 40)?;
        directory_offset = unsigned64(&record, 48)?;
    } else {
        ensure!(
            u64::from(unsigned16(&tail, end + 8)?) == count,
            "Inconsistent Office ZIP entry counts"
        );
    }
    ensure!(
        count <= ARCHIVE_ENTRIES,
        "Office archive exceeds the 4096 entry preview limit"
    );
    ensure!(
        directory_size <= DIRECTORY_BYTES,
        "Office ZIP directory exceeds the 8 MiB preview limit"
    );
    ensure!(
        directory_offset
            .checked_add(directory_size)
            .is_some_and(|position| position <= end_offset),
        "Office ZIP directory lies outside the file"
    );
    file.rewind()?;
    Ok(())
}

fn open_zip(mut file: File, deadline: Instant) -> Result<Archive> {
    zip_preflight(&mut file)?;
    let archive = ZipArchive::new(BoundedReader::new(file, READ_BYTES, deadline))
        .context("Unable to open Office ZIP archive")?;
    ensure!(
        archive.len() <= ARCHIVE_ENTRIES as usize,
        "Office archive has too many entries"
    );
    let mut names_bytes = 0usize;
    for name in archive.file_names() {
        names_bytes = names_bytes.saturating_add(name.len());
        ensure!(
            name.len() <= CELL_BYTES && names_bytes <= PAGE_BYTES,
            "Office archive part names exceed the bounded preview limit"
        );
    }
    Ok(archive)
}

struct XmlReader<R> {
    reader: Reader<R>,
    depth: usize,
}

impl<R: BufRead> XmlReader<R> {
    fn read_event_into<'a>(&mut self, buffer: &'a mut Vec<u8>) -> Result<Event<'a>> {
        let event = self.reader.read_event_into(buffer)?;
        match &event {
            Event::Start(_) => {
                self.depth += 1;
                ensure!(
                    self.depth <= 128,
                    "Office XML exceeds the 128 level nesting limit"
                );
            }
            Event::End(_) => {
                self.depth = self
                    .depth
                    .checked_sub(1)
                    .context("Unmatched Office XML end tag")?
            }
            Event::Eof => ensure!(self.depth == 0, "Office XML is truncated"),
            Event::DocType(_) => {
                anyhow::bail!("Office XML document type declarations are unsupported")
            }
            _ => {}
        }
        Ok(event)
    }
}

fn xml_reader<'a>(
    archive: &'a mut Archive,
    part: &str,
    deadline: Instant,
) -> Result<XmlReader<BufReader<BoundedReader<zip::read::ZipFile<'a, BoundedReader<File>>>>>> {
    let entry = archive
        .by_name(part)
        .with_context(|| format!("Office part is missing: {part}"))?;
    let mut reader = Reader::from_reader(BufReader::with_capacity(
        32 * 1024,
        BoundedReader::new(entry, XML_BYTES, deadline),
    ));
    reader.config_mut().expand_empty_elements = true;
    Ok(XmlReader { reader, depth: 0 })
}

fn text(event: &quick_xml::events::BytesText<'_>) -> Result<String> {
    let decoded = event.decode()?;
    Ok(quick_xml::escape::unescape(&decoded)?.into_owned())
}

fn reference(event: &quick_xml::events::BytesRef<'_>) -> Result<String> {
    let name = event.decode()?;
    Ok(quick_xml::escape::unescape(&format!("&{name};"))?.into_owned())
}

fn attribute(start: &quick_xml::events::BytesStart<'_>, name: &[u8]) -> Result<Option<String>> {
    for value in start.attributes() {
        let value = value?;
        if value.key.local_name().as_ref() == name {
            return Ok(Some(
                value
                    .normalized_value(quick_xml::XmlVersion::Implicit1_0)?
                    .into_owned(),
            ));
        }
    }
    Ok(None)
}

fn append(target: &mut String, value: &str) {
    if target.len() >= CELL_BYTES {
        return;
    }
    let mut length = (CELL_BYTES - target.len()).min(value.len());
    while !value.is_char_boundary(length) {
        length = length.saturating_sub(1);
    }
    target.push_str(&value[..length]);
}

struct PageBuilder {
    page: PreviewPage,
    offset: u64,
    record: u64,
    bytes: usize,
}

impl PageBuilder {
    fn new(title: &str, columns: &[&str], request: &PreviewRequest) -> Self {
        Self {
            page: PreviewPage {
                title: title.into(),
                columns: columns.iter().map(|column| (*column).into()).collect(),
                ..Default::default()
            },
            offset: request.offset,
            record: 0,
            bytes: 0,
        }
    }

    fn push(&mut self, mut row: Vec<String>) -> bool {
        let record = self.record;
        self.record = self.record.saturating_add(1);
        if record < self.offset {
            return true;
        }
        for value in &mut row {
            let mut limited = String::new();
            append(&mut limited, value);
            *value = limited;
        }
        let bytes = row.iter().map(String::len).sum::<usize>();
        if self.page.rows.len() >= PAGE_ROWS || self.bytes.saturating_add(bytes) > PAGE_BYTES {
            self.page.next_offset = Some(record);
            return false;
        }
        self.bytes += bytes;
        self.page.rows.push(row);
        true
    }
}

fn shared_strings(archive: &mut Archive, deadline: Instant) -> Result<Vec<String>> {
    if archive.index_for_name("xl/sharedStrings.xml").is_none() {
        return Ok(Vec::new());
    }
    let mut reader = xml_reader(archive, "xl/sharedStrings.xml", deadline)?;
    let mut buffer = Vec::new();
    let mut strings = Vec::new();
    let mut value = String::new();
    let mut in_text = false;
    let mut total = 0usize;
    loop {
        match reader.read_event_into(&mut buffer)? {
            Event::Start(start) if start.local_name().as_ref() == b"si" => value.clear(),
            Event::Start(start) if start.local_name().as_ref() == b"t" => in_text = true,
            Event::End(end) if end.local_name().as_ref() == b"t" => in_text = false,
            Event::Text(event) if in_text => {
                let decoded = text(&event)?;
                ensure!(
                    value.len().saturating_add(decoded.len()) <= CELL_BYTES,
                    "Spreadsheet shared string exceeds the 4096 byte preview limit"
                );
                value.push_str(&decoded);
            }
            Event::GeneralRef(event) if in_text => {
                let decoded = reference(&event)?;
                ensure!(
                    value.len().saturating_add(decoded.len()) <= CELL_BYTES,
                    "Spreadsheet shared string exceeds the 4096 byte preview limit"
                );
                value.push_str(&decoded);
            }
            Event::CData(event) if in_text => {
                let decoded = event.decode()?;
                ensure!(
                    value.len().saturating_add(decoded.len()) <= CELL_BYTES,
                    "Spreadsheet shared string exceeds the 4096 byte preview limit"
                );
                value.push_str(&decoded);
            }
            Event::End(end) if end.local_name().as_ref() == b"si" => {
                total = total.saturating_add(value.len());
                ensure!(
                    total <= SHARED_STRING_BYTES && strings.len() < 100_000,
                    "Spreadsheet shared strings exceed the bounded preview limit"
                );
                strings.push(std::mem::take(&mut value));
            }
            Event::Eof => break,
            _ => {}
        }
        buffer.clear();
    }
    Ok(strings)
}

fn worksheets(archive: &mut Archive, deadline: Instant) -> Result<Vec<(String, String)>> {
    let mut relationships = Vec::new();
    {
        let mut reader = xml_reader(archive, "xl/_rels/workbook.xml.rels", deadline)?;
        let mut buffer = Vec::new();
        loop {
            match reader.read_event_into(&mut buffer)? {
                Event::Start(start) | Event::Empty(start)
                    if start.local_name().as_ref() == b"Relationship" =>
                {
                    if attribute(&start, b"TargetMode")?.as_deref() != Some("External")
                        && let (Some(identifier), Some(target)) =
                            (attribute(&start, b"Id")?, attribute(&start, b"Target")?)
                    {
                        ensure!(
                            relationships.len() < ARCHIVE_ENTRIES as usize
                                && target.len() <= CELL_BYTES,
                            "Spreadsheet relationships exceed the preview limit"
                        );
                        let target = if target.starts_with('/') {
                            target.trim_start_matches('/').to_string()
                        } else {
                            format!("xl/{target}")
                        };
                        relationships.push((identifier, target));
                    }
                }
                Event::Eof => break,
                _ => {}
            }
            buffer.clear();
        }
    }
    let mut sheets = Vec::new();
    let mut reader = xml_reader(archive, "xl/workbook.xml", deadline)?;
    let mut buffer = Vec::new();
    loop {
        match reader.read_event_into(&mut buffer)? {
            Event::Start(start) | Event::Empty(start)
                if start.local_name().as_ref() == b"sheet" =>
            {
                if let (Some(name), Some(identifier)) =
                    (attribute(&start, b"name")?, attribute(&start, b"id")?)
                    && let Some((_, target)) =
                        relationships.iter().find(|(key, _)| *key == identifier)
                {
                    ensure!(
                        sheets.len() < ARCHIVE_ENTRIES as usize && name.len() <= CELL_BYTES,
                        "Spreadsheet worksheet list exceeds the preview limit"
                    );
                    sheets.push((sheet_section(&name), target.clone()));
                }
            }
            Event::Eof => break,
            _ => {}
        }
        buffer.clear();
    }
    ensure!(
        !sheets.is_empty(),
        "Spreadsheet contains no readable worksheets"
    );
    Ok(sheets)
}

fn spreadsheet(
    archive: &mut Archive,
    request: &PreviewRequest,
    deadline: Instant,
) -> Result<PreviewPage> {
    let sheets = worksheets(archive, deadline)?;
    let selected = request.section.as_deref().unwrap_or(&sheets[0].0);
    let (_, part) = sheets
        .iter()
        .find(|(name, _)| name == selected)
        .context("The requested worksheet no longer exists")?;
    let strings = shared_strings(archive, deadline)?;
    let mut builder = PageBuilder::new(selected, &["Cell", "Type", "Value", "Formula"], request);
    builder.page.sections = sheets.iter().map(|(name, _)| name.clone()).collect();
    builder.page.note = Some("Cells are streamed from the worksheet. Values retain their stored type; formulas are displayed without execution. Individual values are limited to 4096 bytes.".into());
    let mut reader = xml_reader(archive, part, deadline)?;
    let mut buffer = Vec::new();
    let mut coordinate = String::new();
    let mut kind = String::new();
    let mut value = String::new();
    let mut formula = String::new();
    let mut capture = 0;
    loop {
        match reader.read_event_into(&mut buffer)? {
            Event::Start(start) if start.local_name().as_ref() == b"c" => {
                coordinate = attribute(&start, b"r")?
                    .unwrap_or_else(|| format!("Cell {}", builder.record + 1));
                kind = attribute(&start, b"t")?.unwrap_or_else(|| "number".into());
                value.clear();
                formula.clear();
            }
            Event::Start(start) if matches!(start.local_name().as_ref(), b"v" | b"t") => {
                capture = 1
            }
            Event::Start(start) if start.local_name().as_ref() == b"f" => capture = 2,
            Event::Text(event) if capture == 1 => append(&mut value, &text(&event)?),
            Event::Text(event) if capture == 2 => append(&mut formula, &text(&event)?),
            Event::GeneralRef(event) if capture == 1 => append(&mut value, &reference(&event)?),
            Event::GeneralRef(event) if capture == 2 => append(&mut formula, &reference(&event)?),
            Event::CData(event) if capture == 1 => append(&mut value, &event.decode()?),
            Event::CData(event) if capture == 2 => append(&mut formula, &event.decode()?),
            Event::End(end) if matches!(end.local_name().as_ref(), b"v" | b"t" | b"f") => {
                capture = 0
            }
            Event::End(end) if end.local_name().as_ref() == b"c" => {
                if kind == "s" {
                    let index = value
                        .parse::<usize>()
                        .context("Invalid spreadsheet shared string index")?;
                    value = strings
                        .get(index)
                        .context("Spreadsheet shared string is missing")?
                        .clone();
                }
                if !builder.push(vec![
                    coordinate.clone(),
                    kind.clone(),
                    value.clone(),
                    formula.clone(),
                ]) {
                    break;
                }
            }
            Event::Eof => break,
            _ => {}
        }
        buffer.clear();
    }
    Ok(builder.page)
}

fn document_parts(archive: &Archive) -> Vec<(String, String)> {
    let mut parts = archive
        .file_names()
        .filter_map(|name| {
            let label = if name == "word/document.xml" {
                Some("Document".into())
            } else if name == "word/footnotes.xml" {
                Some("Footnotes".into())
            } else if name == "word/endnotes.xml" {
                Some("Endnotes".into())
            } else if name == "word/comments.xml" {
                Some("Comments".into())
            } else if name.starts_with("word/header") && name.ends_with(".xml") {
                Some(format!("Header: {}", name.trim_start_matches("word/")))
            } else if name.starts_with("word/footer") && name.ends_with(".xml") {
                Some(format!("Footer: {}", name.trim_start_matches("word/")))
            } else {
                None
            };
            label.map(|label| (label, name.to_string()))
        })
        .collect::<Vec<_>>();
    parts.sort_by(|left, right| {
        (left.0 != "Document", &left.0).cmp(&(right.0 != "Document", &right.0))
    });
    parts
}

fn presentation_parts(archive: &Archive) -> Vec<(String, String)> {
    let mut parts = archive
        .file_names()
        .filter_map(|name| {
            if let Some(number) = name
                .strip_prefix("ppt/slides/slide")
                .and_then(|value| value.strip_suffix(".xml"))
                .and_then(|value| value.parse::<u32>().ok())
            {
                Some((number, false, format!("Slide {number}"), name.to_string()))
            } else if let Some(number) = name
                .strip_prefix("ppt/notesSlides/notesSlide")
                .and_then(|value| value.strip_suffix(".xml"))
                .and_then(|value| value.parse::<u32>().ok())
            {
                Some((number, true, format!("Notes {number}"), name.to_string()))
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    parts.sort_by_key(|(number, notes, _, _)| (*number, *notes));
    parts
        .into_iter()
        .map(|(_, _, label, part)| (label, part))
        .collect()
}

fn paragraphs(
    archive: &mut Archive,
    request: &PreviewRequest,
    deadline: Instant,
    parts: Vec<(String, String)>,
) -> Result<PreviewPage> {
    ensure!(
        !parts.is_empty(),
        "Office document has no readable text parts"
    );
    let selected = request.section.as_deref().unwrap_or(&parts[0].0);
    let (_, part) = parts
        .iter()
        .find(|(label, _)| label == selected)
        .context("The requested Office document section no longer exists")?;
    let mut builder = PageBuilder::new(selected, &["Paragraph", "Text"], request);
    builder.page.sections = parts.iter().map(|(label, _)| label.clone()).collect();
    builder.page.note = Some("Document text, including table paragraphs, is displayed in reading order. Embedded scripts are not executed. Individual paragraphs are limited to 4096 bytes.".into());
    let mut reader = xml_reader(archive, part, deadline)?;
    let mut buffer = Vec::new();
    let mut paragraph = String::new();
    let mut in_text = false;
    let mut in_paragraph = false;
    loop {
        match reader.read_event_into(&mut buffer)? {
            Event::Start(start) if start.local_name().as_ref() == b"p" => {
                paragraph.clear();
                in_paragraph = true;
            }
            Event::Start(start) if start.local_name().as_ref() == b"t" => in_text = true,
            Event::End(end) if end.local_name().as_ref() == b"t" => in_text = false,
            Event::Text(event) if in_text && in_paragraph => append(&mut paragraph, &text(&event)?),
            Event::GeneralRef(event) if in_text && in_paragraph => {
                append(&mut paragraph, &reference(&event)?)
            }
            Event::CData(event) if in_text && in_paragraph => {
                append(&mut paragraph, &event.decode()?)
            }
            Event::Start(start) if in_paragraph && start.local_name().as_ref() == b"tab" => {
                append(&mut paragraph, "\t")
            }
            Event::Start(start) if in_paragraph && start.local_name().as_ref() == b"br" => {
                append(&mut paragraph, "\n")
            }
            Event::End(end) if end.local_name().as_ref() == b"p" => {
                in_paragraph = false;
                if !builder.push(vec![(builder.record + 1).to_string(), paragraph.clone()]) {
                    break;
                }
            }
            Event::Eof => break,
            _ => {}
        }
        buffer.clear();
    }
    Ok(builder.page)
}

fn ods(archive: &mut Archive, request: &PreviewRequest, deadline: Instant) -> Result<PreviewPage> {
    let mut reader = xml_reader(archive, "content.xml", deadline)?;
    let mut buffer = Vec::new();
    let mut builder = PageBuilder::new(
        "Spreadsheet",
        &[
            "Row",
            "Column",
            "Type",
            "Value",
            "Formula",
            "Repeated columns",
            "Repeated rows",
        ],
        request,
    );
    let mut selected = request.section.clone();
    let mut active = false;
    let mut row = 0u64;
    let mut column = 0u64;
    let mut repetitions = 1u64;
    let mut row_repetitions = 1u64;
    let mut kind = String::new();
    let mut value = String::new();
    let mut formula = String::new();
    let mut in_paragraph = false;
    loop {
        match reader.read_event_into(&mut buffer)? {
            Event::Start(start) if start.local_name().as_ref() == b"table" => {
                let name = attribute(&start, b"name")?
                    .unwrap_or_else(|| format!("Sheet {}", builder.page.sections.len() + 1));
                ensure!(
                    builder.page.sections.len() < ARCHIVE_ENTRIES as usize
                        && name.len() <= CELL_BYTES,
                    "ODS sheet list exceeds the preview limit"
                );
                let name = sheet_section(&name);
                builder.page.sections.push(name.clone());
                if selected.is_none() {
                    selected = Some(name.clone());
                }
                active = selected.as_deref() == Some(&name);
                if active {
                    builder.page.title = name;
                    row = 0;
                }
            }
            Event::End(end) if end.local_name().as_ref() == b"table" => active = false,
            Event::Start(start) if active && start.local_name().as_ref() == b"table-row" => {
                row = row.saturating_add(1);
                column = 0;
                row_repetitions = attribute(&start, b"number-rows-repeated")?
                    .and_then(|value| value.parse().ok())
                    .unwrap_or(1)
                    .max(1);
            }
            Event::End(end) if active && end.local_name().as_ref() == b"table-row" => {
                row = row.saturating_add(row_repetitions.saturating_sub(1));
            }
            Event::Start(start)
                if active
                    && matches!(
                        start.local_name().as_ref(),
                        b"table-cell" | b"covered-table-cell"
                    ) =>
            {
                repetitions = attribute(&start, b"number-columns-repeated")?
                    .and_then(|value| value.parse().ok())
                    .unwrap_or(1)
                    .max(1);
                column = column.saturating_add(1);
                kind = attribute(&start, b"value-type")?.unwrap_or_default();
                value = attribute(&start, b"value")?
                    .or(attribute(&start, b"date-value")?)
                    .or(attribute(&start, b"boolean-value")?)
                    .unwrap_or_default();
                formula = attribute(&start, b"formula")?.unwrap_or_default();
            }
            Event::Start(start) if active && start.local_name().as_ref() == b"p" => {
                in_paragraph = true;
                if kind == "string" && !value.is_empty() {
                    append(&mut value, "\n");
                }
            }
            Event::End(end) if active && end.local_name().as_ref() == b"p" => in_paragraph = false,
            Event::Text(event) if active && in_paragraph && kind == "string" => {
                append(&mut value, &text(&event)?)
            }
            Event::GeneralRef(event) if active && in_paragraph && kind == "string" => {
                append(&mut value, &reference(&event)?)
            }
            Event::CData(event) if active && in_paragraph && kind == "string" => {
                append(&mut value, &event.decode()?)
            }
            Event::End(end)
                if active
                    && matches!(
                        end.local_name().as_ref(),
                        b"table-cell" | b"covered-table-cell"
                    ) =>
            {
                if (!value.is_empty() || !formula.is_empty())
                    && !builder.push(vec![
                        row.to_string(),
                        column.to_string(),
                        kind.clone(),
                        value.clone(),
                        formula.clone(),
                        repetitions.to_string(),
                        row_repetitions.to_string(),
                    ])
                {
                    break;
                }
                column = column.saturating_add(repetitions.saturating_sub(1));
            }
            Event::Eof => break,
            _ => {}
        }
        buffer.clear();
    }
    ensure!(
        selected
            .as_ref()
            .is_some_and(|name| builder.page.sections.contains(name)),
        "Requested ODS worksheet is missing"
    );
    builder.page.note = Some("Stored cells and formulas are streamed without executing formulas. Repeated cells and rows remain compact; coordinates preserve their positions. Worksheet names are discovered while reading.".into());
    Ok(builder.page)
}

fn stream_page(
    reader: &mut impl Read,
    request: &PreviewRequest,
    title: &str,
) -> Result<PreviewPage> {
    let mut builder = PageBuilder::new(
        title,
        &["Byte offset", "Hexadecimal", "Text", "UTF-16LE"],
        &PreviewRequest::default(),
    );
    let mut offset = request.offset;
    for _ in 0..PAGE_ROWS {
        let mut bytes = [0u8; 32];
        let mut count = 0;
        while count < bytes.len() {
            let length = reader.read(&mut bytes[count..])?;
            if length == 0 {
                break;
            }
            count += length;
        }
        if count == 0 {
            break;
        }
        let hex = bytes[..count]
            .iter()
            .map(|value| format!("{value:02x}"))
            .collect::<Vec<_>>()
            .join(" ");
        let printable = bytes[..count]
            .iter()
            .map(|value| {
                if value.is_ascii_graphic() || *value == b' ' {
                    char::from(*value)
                } else {
                    '·'
                }
            })
            .collect::<String>();
        let unicode = String::from_utf16_lossy(
            &bytes[..count]
                .chunks_exact(2)
                .map(|value| u16::from_le_bytes([value[0], value[1]]))
                .collect::<Vec<_>>(),
        );
        let unicode = unicode
            .chars()
            .map(|value| if value.is_control() { '·' } else { value })
            .collect();
        builder.push(vec![offset.to_string(), hex, printable, unicode]);
        offset = offset.saturating_add(count as u64);
    }
    let mut extra = [0u8; 1];
    if reader.read(&mut extra)? != 0 {
        builder.page.next_offset = Some(offset);
    }
    Ok(builder.page)
}

fn archive_stream(
    archive: &mut Archive,
    request: &PreviewRequest,
    deadline: Instant,
) -> Result<PreviewPage> {
    let mut sections = archive
        .file_names()
        .filter(|name| !name.ends_with('/'))
        .map(str::to_string)
        .collect::<Vec<_>>();
    sections.sort();
    let selected = request
        .section
        .as_deref()
        .or_else(|| sections.first().map(String::as_str))
        .context("Office archive is empty")?;
    let entry = archive.by_name(selected)?;
    let length = entry.size();
    let mut reader = BoundedReader::new(entry, XML_BYTES, deadline);
    ensure!(
        request.offset <= XML_BYTES.saturating_sub((PAGE_ROWS * 32 + 1) as u64),
        "This compressed stream offset exceeds the bounded preview scan limit"
    );
    io::copy(&mut reader.by_ref().take(request.offset), &mut io::sink())?;
    let mut page = stream_page(&mut reader, request, selected)?;
    page.sections = sections;
    page.metadata
        .push(("Stream bytes".into(), length.to_string()));
    page.note = Some("This legacy Office container is opened read-only. Select a stream to inspect its actual bytes and text. Full spreadsheet or slide layout decoding is unavailable for this format.".into());
    Ok(page)
}

fn compound(
    path: &Path,
    file: File,
    request: &PreviewRequest,
    deadline: Instant,
) -> Result<PreviewPage> {
    if file.metadata()?.len() > COMPOUND_BYTES {
        let mut page = crate::read_bytes(path, &PreviewRequest::default())?;
        page.note = Some("This compound document exceeds the 32 MiB structural parsing limit. Actual file bytes are shown as Hex from offset 0; use an Office converter for worksheet or slide layout. The limit prevents large FAT and MiniFAT allocations.".into());
        return Ok(page);
    }
    // The parser preallocates MiniFAT chains before reading them, so a read
    // budget alone is insufficient. The structural size cap above bounds that
    // allocation; the reader also bounds corrupt or repeated sector reads.
    let mut compound = cfb::CompoundFile::open(BufReader::with_capacity(
        32 * 1024,
        BoundedReader::new(file, XML_BYTES, deadline),
    ))
    .context("Invalid Office compound document")?;
    let mut sections = crate::inspect::compound_streams(&compound)?
        .into_iter()
        .map(|(name, _)| name)
        .collect::<Vec<_>>();
    ensure!(
        !sections
            .iter()
            .any(|name| name.ends_with("/EncryptedPackage")),
        "This Office document is password-protected"
    );
    sections.sort();
    if request
        .section
        .as_deref()
        .is_none_or(|section| !section.starts_with('/'))
        && let Some(document) = sections
            .iter()
            .find(|name| name.eq_ignore_ascii_case("/PowerPoint Document"))
    {
        let length = compound.entry(document)?.len();
        let mut stream = compound.open_stream(document)?;
        return legacy_presentation(&mut stream, request, length);
    }
    if request
        .section
        .as_deref()
        .is_none_or(|section| !section.starts_with('/'))
        && let Some(workbook) = sections.iter().find(|name| {
            name.eq_ignore_ascii_case("/Workbook") || name.eq_ignore_ascii_case("/Book")
        })
    {
        let mut stream = compound.open_stream(workbook)?;
        let mut signature = [0u8; 4];
        if stream.read(&mut signature)? == signature.len() && unsigned16(&signature, 0)? == 0x0809 {
            stream.rewind()?;
            return biff_spreadsheet(&mut stream, request);
        }
    }
    let selected = request
        .section
        .as_deref()
        .or_else(|| sections.first().map(String::as_str))
        .context("Office compound document contains no streams")?;
    let length = compound.entry(selected)?.len();
    let mut stream = compound.open_stream(selected)?;
    stream.seek(SeekFrom::Start(request.offset.min(length)))?;
    let mut page = stream_page(&mut stream, request, selected)?;
    page.sections = sections;
    page.metadata
        .push(("Stream bytes".into(), length.to_string()));
    page.note = Some("Legacy Office streams are paged directly from the source without loading the document into memory. The byte and text view includes document records; full worksheet, slide, or WPS layout decoding is unavailable.".into());
    Ok(page)
}

fn stamp(path: &Path) -> Result<(u64, Option<SystemTime>)> {
    let metadata = fs::metadata(path)?;
    Ok((metadata.len(), metadata.modified().ok()))
}

pub(crate) fn read(path: &Path, request: &PreviewRequest) -> Result<PreviewPage> {
    let before = stamp(path)?;
    let deadline = Instant::now() + TIME_LIMIT;
    let mut file = File::open(path)?;
    let mut header = [0u8; 8];
    let header_length = file.read(&mut header)?;
    file.rewind()?;
    let mut page = if header_length == 8 && &header == OLE_SIGNATURE {
        compound(path, file, request, deadline)?
    } else if header.starts_with(b"PK") {
        let mut archive = open_zip(file, deadline)?;
        if archive.index_for_name("xl/workbook.xml").is_some() {
            spreadsheet(&mut archive, request, deadline)?
        } else if archive.index_for_name("xl/workbook.bin").is_some() {
            binary_spreadsheet(&mut archive, request, deadline)?
        } else if archive.index_for_name("word/document.xml").is_some() {
            let parts = document_parts(&archive);
            paragraphs(&mut archive, request, deadline, parts)?
        } else if archive.index_for_name("ppt/presentation.xml").is_some() {
            let parts = presentation_parts(&archive);
            paragraphs(&mut archive, request, deadline, parts)?
        } else if archive.index_for_name("content.xml").is_some() {
            ods(&mut archive, request, deadline)?
        } else {
            archive_stream(&mut archive, request, deadline)?
        }
    } else {
        let mut page = crate::read_bytes(path, &PreviewRequest::default())?;
        page.note = Some("This Office or WPS variant has no recognized XML or compound container. Actual document bytes are shown as Hex from offset 0; install an appropriate document converter for layout rendering.".into());
        page
    };
    ensure!(
        stamp(path)? == before,
        "The Office document changed during preview; refresh to read it again"
    );
    page.metadata
        .insert(0, ("File bytes".into(), before.0.to_string()));
    Ok(page)
}

#[cfg(test)]
#[path = "office_tests.rs"]
mod tests;
