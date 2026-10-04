//! Bounded, read-only PDB structure previews. PDB streams are read on demand; names in a
//! PDB are displayed as data and never opened as files. Native parsing uses pdb 0.8.
use super::binary::BinarySummary;
use pdb::{FallibleIterator, Source, SourceSlice, SourceView};
use std::cell::Cell;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;
use std::rc::Rc;
use std::time::{Duration, Instant};

const MSF7: &[u8; 32] = b"Microsoft C/C++ MSF 7.00\r\n\x1aDS\0\0\0";
const MAX_DIRECTORY: usize = 8 * 1024 * 1024;
const MAX_VIEW: usize = 16 * 1024 * 1024;
const MAX_READ: usize = 64 * 1024 * 1024;
const MAX_STREAMS: usize = 65_536;
const MAX_RECORDS: usize = 100_000;
const MAX_ITEMS: usize = 10_000;
const MAX_TEXT: usize = 512;
const LIMIT_NOTE: &str = "Some PDB details were omitted because they exceed preview limits.";
const CORRUPT_NOTE: &str = "Some PDB records could not be parsed.";

#[derive(Debug)]
struct Budget {
    start: Instant,
    read: Cell<usize>,
    records: Cell<usize>,
    limited: Cell<bool>,
}
impl Budget {
    fn new() -> Rc<Self> {
        Rc::new(Self {
            start: Instant::now(),
            read: Cell::new(0),
            records: Cell::new(0),
            limited: Cell::new(false),
        })
    }
    fn expired(&self) -> bool {
        self.start.elapsed() > Duration::from_secs(2)
    }
    fn consume(&self, size: usize) -> io::Result<()> {
        let total = self
            .read
            .get()
            .checked_add(size)
            .ok_or_else(|| invalid(LIMIT_NOTE))?;
        if size > MAX_VIEW || total > MAX_READ || self.expired() {
            self.limited.set(true);
            return Err(io::Error::new(io::ErrorKind::InvalidData, LIMIT_NOTE));
        }
        self.read.set(total);
        Ok(())
    }
    fn record(&self) -> bool {
        if self.records.get() >= MAX_RECORDS || self.expired() {
            self.limited.set(true);
            return false;
        }
        self.records.set(self.records.get() + 1);
        true
    }
}

#[derive(Debug)]
struct BoundedSource {
    file: File,
    length: u64,
    budget: Rc<Budget>,
}
#[derive(Debug)]
struct OwnedView(Vec<u8>);
impl SourceView<'_> for OwnedView {
    fn as_slice(&self) -> &[u8] {
        &self.0
    }
}
impl<'s> Source<'s> for BoundedSource {
    fn view(&mut self, slices: &[SourceSlice]) -> io::Result<Box<dyn SourceView<'s>>> {
        if slices.len() > MAX_STREAMS {
            self.budget.limited.set(true);
            return Err(invalid(LIMIT_NOTE));
        }
        let mut length = 0usize;
        for slice in slices {
            length = length
                .checked_add(slice.size)
                .ok_or_else(|| invalid("PDB stream size overflow"))?;
            if slice
                .offset
                .checked_add(slice.size as u64)
                .map_or(true, |end| end > self.length)
            {
                return Err(invalid("PDB page is outside the file"));
            }
        }
        self.budget.consume(length)?;
        let mut data = vec![0; length];
        let mut at = 0;
        for slice in slices {
            if self.budget.expired() {
                self.budget.limited.set(true);
                return Err(invalid(LIMIT_NOTE));
            }
            self.file.seek(SeekFrom::Start(slice.offset))?;
            self.file.read_exact(&mut data[at..at + slice.size])?;
            at += slice.size;
        }
        Ok(Box::new(OwnedView(data)))
    }
}
fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
fn u16_at(data: &[u8], at: usize) -> io::Result<u16> {
    Ok(u16::from_le_bytes(fixed_at(data, at)?))
}
fn u32_at(data: &[u8], at: usize) -> io::Result<u32> {
    Ok(u32::from_le_bytes(fixed_at(data, at)?))
}
fn u64_at(data: &[u8], at: usize) -> io::Result<u64> {
    Ok(u64::from_le_bytes(fixed_at(data, at)?))
}
fn fixed_at<const SIZE: usize>(data: &[u8], at: usize) -> io::Result<[u8; SIZE]> {
    let end = at
        .checked_add(SIZE)
        .ok_or_else(|| invalid("PDB metadata offset overflow"))?;
    data.get(at..end)
        .ok_or_else(|| invalid("Truncated PDB metadata"))?
        .try_into()
        .map_err(|_| invalid("Truncated PDB metadata"))
}
fn read_at(
    file: &mut File,
    length: u64,
    budget: &Budget,
    offset: u64,
    size: usize,
) -> io::Result<Vec<u8>> {
    if offset
        .checked_add(size as u64)
        .map_or(true, |end| end > length)
    {
        return Err(invalid("PDB metadata is outside the file"));
    }
    budget.consume(size)?;
    let mut out = vec![0; size];
    file.seek(SeekFrom::Start(offset))?;
    file.read_exact(&mut out)?;
    Ok(out)
}
fn text(bytes: &[u8]) -> String {
    let end = bytes[..bytes.len().min(MAX_TEXT)]
        .iter()
        .position(|b| *b == 0)
        .unwrap_or(bytes.len().min(MAX_TEXT));
    let mut value: String = String::from_utf8_lossy(&bytes[..end])
        .chars()
        .flat_map(|c| {
            if c.is_control() {
                c.escape_default().collect::<Vec<_>>()
            } else {
                vec![c]
            }
        })
        .collect();
    if end == MAX_TEXT && bytes.len() > MAX_TEXT {
        value.push('…');
    }
    value
}
fn prop(out: &mut BinarySummary, label: &str, value: impl ToString) {
    out.prop(label, value);
}
fn section(out: &mut BinarySummary, title: &str, items: Vec<String>) {
    out.reserved_section(title, items);
}
fn push_item(
    out: &mut BinarySummary,
    budget: &Budget,
    items: &mut Vec<String>,
    value: String,
) -> bool {
    if out.item(items, value) {
        true
    } else {
        budget.limited.set(true);
        false
    }
}
fn architecture(machine: u16) -> String {
    match machine {
        0x014c => "x86".into(),
        0x8664 => "x86_64".into(),
        0xaa64 => "aarch64".into(),
        0x01c0 | 0x01c2 | 0x01c4 => "arm".into(),
        0x0200 => "ia64".into(),
        0 => "Unknown".into(),
        other => format!("0x{other:04X}"),
    }
}

#[derive(Debug)]
struct StreamMeta {
    size: usize,
    pages: Vec<u32>,
}
#[derive(Debug)]
struct MsfIndex {
    page_size: usize,
    streams: Vec<Option<StreamMeta>>,
}
impl MsfIndex {
    fn read(
        &self,
        file: &mut File,
        length: u64,
        budget: &Budget,
        stream: usize,
        offset: usize,
        size: usize,
    ) -> io::Result<Vec<u8>> {
        let meta = self
            .streams
            .get(stream)
            .and_then(Option::as_ref)
            .ok_or_else(|| invalid("PDB stream is absent"))?;
        if offset.checked_add(size).map_or(true, |end| end > meta.size) {
            return Err(invalid("Truncated PDB stream"));
        }
        budget.consume(size)?;
        let mut out = vec![0; size];
        let mut at = 0;
        let mut position = offset;
        while at < size {
            if budget.expired() {
                budget.limited.set(true);
                return Err(invalid(LIMIT_NOTE));
            }
            let page = *meta
                .pages
                .get(position / self.page_size)
                .ok_or_else(|| invalid("Truncated PDB page list"))?;
            let inside = position % self.page_size;
            let count = (self.page_size - inside).min(size - at);
            let start = page as u64 * self.page_size as u64 + inside as u64;
            if start + count as u64 > length {
                return Err(invalid("PDB page is outside the file"));
            }
            file.seek(SeekFrom::Start(start))?;
            file.read_exact(&mut out[at..at + count])?;
            at += count;
            position += count;
        }
        Ok(out)
    }
}

fn readable(index: &MsfIndex, stream: usize, budget: &Budget) -> bool {
    if let Some(meta) = index.streams.get(stream).and_then(Option::as_ref) {
        if meta.size > MAX_VIEW || meta.pages.len() > MAX_STREAMS {
            budget.limited.set(true);
            return false;
        }
        true
    } else {
        false
    }
}

// Keep third-party iteration bounded even when a next() call skips many padding records.
fn symbol_prefix(data: &[u8], budget: &Budget) -> io::Result<(usize, bool)> {
    let mut cursor = 0;
    let mut count = 0;
    while cursor < data.len() {
        if !budget.record() {
            return Ok((count, false));
        }
        let size = u16_at(data, cursor)? as usize;
        if size < 2 || cursor + 2 + size > data.len() {
            return Err(invalid("Truncated PDB symbol record"));
        }
        let kind = u16_at(data, cursor + 2)?;
        if kind != 0x0402 && kind != 0x0007 {
            count += 1;
        } // S_ALIGN / S_SKIP
        cursor += 2 + size;
    }
    Ok((count, true))
}
fn named_symbol(kind: u16) -> bool {
    // Named variants parsed without count-based allocation; MANYREG and compile flags have
    // no name in this API and remain excluded. Constants additionally require a numeric guard.
    matches!(kind,
        0x0009 | 0x0206..=0x0207 | 0x0209 | 0x0400..=0x0401 | 0x0403 |
        0x1002..=0x1004 | 0x1007..=0x100b | 0x100d..=0x100f | 0x1020..=0x1021 | 0x1029 |
        0x1101..=0x1103 | 0x1105 | 0x1107..=0x1109 | 0x110c..=0x1113 | 0x111c..=0x111d |
        0x1124..=0x1128 | 0x112d | 0x1138 | 0x113e | 0x1146..=0x1147 | 0x1155..=0x1156)
}
fn symbol_numeric_safe(symbol: &pdb::Symbol<'_>) -> bool {
    if matches!(symbol.raw_kind(), 0x1002 | 0x1107 | 0x112d) {
        // pdb 0.8's Variant parser panics in debug builds for an unknown numeric prefix.
        u16_at(symbol.raw_bytes(), 6)
            .is_ok_and(|n| n < 0x8000 || matches!(n, 0x8000..=0x8004 | 0x8009..=0x800a))
    } else {
        true
    }
}
fn module_prefix(data: &[u8], budget: &Budget) -> io::Result<(usize, bool)> {
    let mut cursor = 0usize;
    let mut count = 0;
    while cursor < data.len() {
        if !budget.record() {
            return Ok((count, false));
        }
        if cursor + 64 > data.len() {
            return Err(invalid("Truncated PDB module record"));
        }
        cursor += 64;
        for _ in 0..2 {
            let rest = &data[cursor..];
            let Some(end) = rest[..rest.len().min(2048)].iter().position(|b| *b == 0) else {
                if rest.len() >= 2048 {
                    budget.limited.set(true);
                    return Ok((count, false));
                }
                return Err(invalid("Truncated PDB module name"));
            };
            cursor += end + 1;
        }
        cursor = cursor.next_multiple_of(4);
        if cursor > data.len() {
            return Err(invalid("Truncated PDB module padding"));
        }
        count += 1;
    }
    Ok((count, true))
}
fn type_prefix(data: &[u8], declared: u32, budget: &Budget) -> io::Result<Vec<bool>> {
    let mut cursor = u32_at(data, 4)? as usize;
    if !(56..=1024).contains(&cursor) || cursor > data.len() {
        return Err(invalid("Invalid PDB type header"));
    }
    let mut safe = Vec::new();
    for _ in 0..declared {
        if cursor == data.len() {
            break;
        }
        if !budget.record() {
            break;
        }
        let size = u16_at(data, cursor)? as usize;
        if size < 2 || cursor + 2 + size > data.len() {
            return Err(invalid("Truncated PDB type record"));
        }
        let record = &data[cursor + 2..cursor + 2 + size];
        let kind = u16_at(record, 0)?;
        let at = match kind {
            0x1504..=0x1505 | 0x1004..=0x1005 => Some(18),
            0x1506 | 0x1006 => Some(10),
            _ => None,
        };
        let named = matches!(kind, 0x1504..=0x1507 | 0x1004..=0x1007);
        let numeric = at.map_or(true, |at| {
            u16_at(record, at)
                .is_ok_and(|n| n < 0x8000 || matches!(n, 0x8000 | 0x8002 | 0x8004 | 0x800a))
        });
        if named && !numeric {
            return Err(invalid("Invalid PDB numeric type value"));
        }
        safe.push(named && numeric);
        cursor += 2 + size;
    }
    Ok(safe)
}

/// Validate the MSF hierarchy before pdb allocates any page/stream lists from untrusted counts.
fn msf_index(file: &mut File, length: u64, budget: &Budget, header: &[u8]) -> io::Result<MsfIndex> {
    if header.get(..32) != Some(MSF7.as_slice()) {
        return Err(invalid("Unsupported PDB format"));
    }
    let page_size = u32_at(header, 32)? as usize;
    let page_count = u32_at(header, 40)? as usize;
    let directory_size = u32_at(header, 44)? as usize;
    if !page_size.is_power_of_two() || !(256..=65_536).contains(&page_size) {
        return Err(invalid("Invalid PDB block size"));
    }
    if page_count == 0 || page_count as u64 * page_size as u64 > length {
        return Err(invalid("PDB block count exceeds file size"));
    }
    if !(4..=MAX_DIRECTORY).contains(&directory_size) {
        return Err(invalid("PDB stream directory exceeds preview limits"));
    }
    let directory_pages = directory_size.div_ceil(page_size);
    let index_size = directory_pages
        .checked_mul(4)
        .ok_or_else(|| invalid("PDB directory size overflow"))?;
    let index_pages = index_size.div_ceil(page_size);
    let pointers = read_at(file, length, budget, 52, index_pages * 4)?;
    let mut page_list = Vec::with_capacity(index_size);
    for n in 0..index_pages {
        let page = u32_at(&pointers, n * 4)? as usize;
        if page >= page_count {
            return Err(invalid("PDB directory index page is outside the file"));
        }
        let size = (index_size - page_list.len()).min(page_size);
        page_list.extend(read_at(
            file,
            length,
            budget,
            page as u64 * page_size as u64,
            size,
        )?);
    }
    let mut directory = Vec::with_capacity(directory_size);
    for n in 0..directory_pages {
        let page = u32_at(&page_list, n * 4)? as usize;
        if page >= page_count {
            return Err(invalid("PDB directory page is outside the file"));
        }
        let size = (directory_size - directory.len()).min(page_size);
        directory.extend(read_at(
            file,
            length,
            budget,
            page as u64 * page_size as u64,
            size,
        )?);
    }
    let count = u32_at(&directory, 0)? as usize;
    if count > MAX_STREAMS || 4 + count * 4 > directory.len() {
        return Err(invalid("Invalid PDB stream count"));
    }
    let mut cursor = 4 + count * 4;
    let mut streams = Vec::with_capacity(count);
    for n in 0..count {
        if n % 1024 == 0 && budget.expired() {
            budget.limited.set(true);
            return Err(invalid(LIMIT_NOTE));
        }
        let size = u32_at(&directory, 4 + n * 4)?;
        if size == u32::MAX {
            streams.push(None);
            continue;
        }
        let size = size as usize;
        let pages = size.div_ceil(page_size);
        let end = cursor
            .checked_add(
                pages
                    .checked_mul(4)
                    .ok_or_else(|| invalid("PDB page list overflow"))?,
            )
            .ok_or_else(|| invalid("PDB page list overflow"))?;
        if end > directory.len() {
            return Err(invalid("Truncated PDB stream page list"));
        }
        let mut list = Vec::with_capacity(pages);
        while cursor < end {
            let page = u32_at(&directory, cursor)?;
            if page as usize >= page_count {
                return Err(invalid("PDB stream page is outside the file"));
            }
            list.push(page);
            cursor += 4;
        }
        streams.push(Some(StreamMeta { size, pages: list }));
    }
    Ok(MsfIndex { page_size, streams })
}

fn native_source_files(
    index: &MsfIndex,
    file: &mut File,
    length: u64,
    budget: &Budget,
    header: &[u8],
    out: &mut BinarySummary,
) -> io::Result<()> {
    let offset = 64usize
        + u32_at(header, 24)? as usize
        + u32_at(header, 28)? as usize
        + u32_at(header, 32)? as usize;
    let size = u32_at(header, 36)? as usize;
    if size == 0 {
        return Ok(());
    }
    // DBI FileInfo has per-module counts and a file-name offset table, not external file data.
    let head = index.read(file, length, budget, 3, offset, 4)?;
    let modules = u16_at(&head, 0)? as usize;
    if 4 + modules * 4 > size {
        return Err(invalid("Truncated PDB source-file module counts"));
    }
    let counts = index.read(file, length, budget, 3, offset + 4, modules * 4)?;
    let mut count = 0usize;
    for n in 0..modules {
        count = count
            .checked_add(u16_at(&counts, modules * 2 + n * 2)? as usize)
            .ok_or_else(|| invalid("PDB source-file count overflow"))?;
    }
    let names_at = 4
        + modules * 4
        + count
            .checked_mul(4)
            .ok_or_else(|| invalid("PDB source-file table overflow"))?;
    if names_at > size {
        return Err(invalid("Truncated PDB source-file table"));
    }
    prop(out, "Source files", count);
    let offsets = index.read(
        file,
        length,
        budget,
        3,
        offset + 4 + modules * 4,
        count.min(MAX_ITEMS) * 4,
    )?;
    let mut items = Vec::new();
    let mut error = None;
    for n in 0..count.min(MAX_ITEMS) {
        let relative = u32_at(&offsets, n * 4)? as usize;
        if relative >= size - names_at {
            error = Some(invalid("PDB source-file name is outside its stream"));
            break;
        }
        let bytes = match index.read(
            file,
            length,
            budget,
            3,
            offset + names_at + relative,
            (size - names_at - relative).min(MAX_TEXT),
        ) {
            Ok(bytes) => bytes,
            Err(err) => {
                error = Some(err);
                break;
            }
        };
        if !push_item(out, budget, &mut items, text(&bytes)) {
            break;
        }
    }
    if count > MAX_ITEMS {
        budget.limited.set(true);
    }
    section(out, "Source files", items);
    match error {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

fn native(
    file: &mut File,
    length: u64,
    budget: Rc<Budget>,
    header: &[u8],
) -> io::Result<BinarySummary> {
    let index = msf_index(file, length, &budget, header)?;
    let mut out = BinarySummary::default();
    prop(&mut out, "Format", "PDB 7.0");
    prop(
        &mut out,
        "Streams",
        index.streams.iter().filter(|s| s.is_some()).count(),
    );
    prop(&mut out, "Block size", index.page_size);
    let mut streams = Vec::new();
    let stream_count = index.streams.iter().filter(|s| s.is_some()).count();
    for (n, meta) in index
        .streams
        .iter()
        .enumerate()
        .filter_map(|(n, meta)| meta.as_ref().map(|m| (n, m)))
    {
        if streams.len() >= MAX_ITEMS
            || !push_item(
                &mut out,
                &budget,
                &mut streams,
                format!("#{n}: {} bytes", meta.size),
            )
        {
            budget.limited.set(true);
            break;
        }
    }
    section(&mut out, "Streams", streams);
    if stream_count > MAX_ITEMS {
        budget.limited.set(true);
    }
    let source = BoundedSource {
        file: file.try_clone()?,
        length,
        budget: budget.clone(),
    };
    let mut pdb = pdb::PDB::open(source).map_err(|e| invalid(&e.to_string()))?;
    let mut corrupt = false;
    if readable(&index, 1, &budget) {
        match pdb.pdb_information() {
            Ok(info) => {
                if let Ok(bytes) = index.read(file, length, &budget, 1, 0, 4) {
                    prop(&mut out, "Version", u32_at(&bytes, 0)?);
                }
                prop(&mut out, "GUID", info.guid);
                prop(&mut out, "Age", info.age);
                prop(&mut out, "Signature", format!("0x{:08X}", info.signature));
            }
            Err(_) => corrupt = true,
        }
    } else if !budget.limited.get() {
        corrupt = true;
    }
    let dbi_header = index.read(file, length, &budget, 3, 0, 64).ok();
    let mut dbi_valid = false;
    if let Some(head) = &dbi_header {
        let size = index
            .streams
            .get(3)
            .and_then(Option::as_ref)
            .ok_or_else(|| invalid("Missing PDB DBI stream"))?
            .size;
        let parts = [24, 28, 32, 36, 40, 48, 52]
            .iter()
            .try_fold(64u64, |sum, at| {
                Ok::<_, io::Error>(sum + u32_at(head, *at)? as u64)
            });
        dbi_valid = parts.is_ok_and(|total| total <= size as u64 && total <= u32::MAX as u64);
        if dbi_valid {
            prop(&mut out, "Architecture", architecture(u16_at(head, 58)?));
            prop(&mut out, "DBI version", u32_at(head, 4)?);
            prop(&mut out, "DBI age", u32_at(head, 8)?);
            if native_source_files(&index, file, length, &budget, head, &mut out).is_err() {
                corrupt = true;
            }
        } else {
            corrupt = true;
        }
    }
    if dbi_valid {
        let head = dbi_header
            .as_ref()
            .ok_or_else(|| invalid("Missing PDB DBI header"))?;
        if readable(&index, 3, &budget) {
            let modules_size = u32_at(head, 24)? as usize;
            let prefix = index
                .read(file, length, &budget, 3, 64, modules_size)
                .and_then(|data| module_prefix(&data, &budget));
            match prefix {
                Ok((count, complete)) => {
                    prop(
                        &mut out,
                        "Modules",
                        if complete {
                            count.to_string()
                        } else {
                            format!("≥{count}")
                        },
                    );
                    if let Ok(dbi) = pdb.debug_information() {
                        match dbi.modules() {
                            Ok(mut modules) => {
                                let mut items = Vec::new();
                                for _ in 0..count.min(MAX_ITEMS) {
                                    if budget.expired() {
                                        budget.limited.set(true);
                                        break;
                                    }
                                    match modules.next() {
                                        Ok(Some(module)) => {
                                            if !push_item(
                                                &mut out,
                                                &budget,
                                                &mut items,
                                                format!(
                                                    "{} — {}",
                                                    text(module.module_name().as_bytes()),
                                                    text(module.object_file_name().as_bytes())
                                                ),
                                            ) {
                                                break;
                                            }
                                        }
                                        _ => {
                                            corrupt = true;
                                            break;
                                        }
                                    }
                                }
                                if count > MAX_ITEMS {
                                    budget.limited.set(true);
                                }
                                section(&mut out, "Modules", items);
                            }
                            Err(_) => corrupt = true,
                        }
                    } else {
                        corrupt = true;
                    }
                }
                Err(_) => corrupt = true,
            }
        }
        // Read a bounded header prefix; never allocate following an unchecked declared count.
        let extra = [24, 28, 32, 36, 40, 52]
            .iter()
            .try_fold(64usize, |sum, at| {
                sum.checked_add(u32_at(head, *at)? as usize)
                    .ok_or_else(|| invalid("PDB DBI size overflow"))
            })?;
        if u32_at(head, 48)? >= 12 {
            if let Ok(bytes) = index.read(file, length, &budget, 3, extra, 12) {
                let stream = u16_at(&bytes, 10)? as usize;
                if let Some(meta) = index.streams.get(stream).and_then(Option::as_ref) {
                    let count = meta.size / 40;
                    prop(&mut out, "Sections", count);
                    if let Ok(bytes) =
                        index.read(file, length, &budget, stream, 0, count.min(MAX_ITEMS) * 40)
                    {
                        let mut items = Vec::new();
                        for s in bytes.chunks_exact(40) {
                            if !budget.record() {
                                break;
                            }
                            if !push_item(
                                &mut out,
                                &budget,
                                &mut items,
                                format!(
                                    "{}: RVA 0x{:08X}, {} bytes",
                                    text(&s[..8]),
                                    u32_at(s, 12)?,
                                    u32_at(s, 8)?
                                ),
                            ) {
                                break;
                            }
                        }
                        if count > MAX_ITEMS {
                            budget.limited.set(true);
                        }
                        section(&mut out, "Sections", items);
                    }
                }
            }
        }
        let stream = u16_at(head, 20)? as usize;
        // global_symbols() may read the DBI stream itself when its header is not cached.
        if readable(&index, 3, &budget) && readable(&index, stream, &budget) {
            let meta = index
                .streams
                .get(stream)
                .and_then(Option::as_ref)
                .ok_or_else(|| invalid("Missing PDB symbol stream"))?;
            let prefix = index
                .read(file, length, &budget, stream, 0, meta.size)
                .and_then(|bytes| symbol_prefix(&bytes, &budget));
            match prefix {
                Ok((count, complete)) => {
                    prop(
                        &mut out,
                        "Symbols",
                        if complete {
                            count.to_string()
                        } else {
                            format!("≥{count}")
                        },
                    );
                    if let Ok(table) = pdb.global_symbols() {
                        let mut iter = table.iter();
                        let mut items = Vec::new();
                        for _ in 0..count {
                            if items.len() >= MAX_ITEMS {
                                budget.limited.set(true);
                                break;
                            }
                            if budget.expired() {
                                budget.limited.set(true);
                                break;
                            }
                            match iter.next() {
                                Ok(Some(symbol)) => {
                                    if items.len() < MAX_ITEMS && named_symbol(symbol.raw_kind()) {
                                        if !symbol_numeric_safe(&symbol) {
                                            corrupt = true;
                                            continue;
                                        }
                                        match symbol.parse() {
                                            Ok(data) => {
                                                if let Some(name) = data.name() {
                                                    if !push_item(
                                                        &mut out,
                                                        &budget,
                                                        &mut items,
                                                        format!(
                                                            "0x{:04X}: {}",
                                                            symbol.raw_kind(),
                                                            text(name.as_bytes())
                                                        ),
                                                    ) {
                                                        break;
                                                    }
                                                }
                                            }
                                            Err(_) => corrupt = true,
                                        }
                                    }
                                }
                                _ => {
                                    corrupt = true;
                                    break;
                                }
                            }
                        }
                        if count > MAX_ITEMS {
                            budget.limited.set(true);
                        }
                        section(&mut out, "Symbols", items);
                    } else {
                        corrupt = true;
                    }
                }
                Err(_) => corrupt = true,
            }
        }
    }
    // Read the TPI header independently: the count remains useful when a large type stream is
    // skipped. Never construct TypeFinder, whose allocation follows the declared type count.
    if let Some(meta) = index.streams.get(2).and_then(Option::as_ref) {
        if meta.size >= 56 {
            if let Ok(head) = index.read(file, length, &budget, 2, 0, 56) {
                let min = u32_at(&head, 8)?;
                let max = u32_at(&head, 12)?;
                if min >= 4096 && max >= min {
                    prop(&mut out, "Types", max - min);
                    if readable(&index, 2, &budget) {
                        let safe = index
                            .read(file, length, &budget, 2, 0, meta.size)
                            .and_then(|bytes| type_prefix(&bytes, max - min, &budget));
                        match safe {
                            Ok(safe) => {
                                if let Ok(types) = pdb.type_information() {
                                    let mut iter = types.iter();
                                    let mut items = Vec::new();
                                    for parse in safe {
                                        if items.len() >= MAX_ITEMS {
                                            budget.limited.set(true);
                                            break;
                                        }
                                        if budget.expired() {
                                            budget.limited.set(true);
                                            break;
                                        }
                                        match iter.next() {
                                            Ok(Some(typ)) if parse => match typ.parse() {
                                                Ok(data) => {
                                                    if let Some(name) = data.name() {
                                                        if !push_item(
                                                            &mut out,
                                                            &budget,
                                                            &mut items,
                                                            format!(
                                                                "{}: {}",
                                                                typ.index(),
                                                                text(name.as_bytes())
                                                            ),
                                                        ) {
                                                            break;
                                                        }
                                                    }
                                                }
                                                Err(_) => corrupt = true,
                                            },
                                            Ok(Some(_)) => (),
                                            _ => {
                                                corrupt = true;
                                                break;
                                            }
                                        }
                                    }
                                    if max - min > MAX_ITEMS as u32 {
                                        budget.limited.set(true);
                                    }
                                    section(&mut out, "Types", items);
                                } else {
                                    corrupt = true;
                                }
                            }
                            Err(_) => corrupt = true,
                        }
                    }
                } else {
                    corrupt = true;
                }
            } else {
                corrupt = true;
            }
        } else if meta.size != 0 {
            corrupt = true;
        }
    }
    out.note = if budget.limited.get() || out.note.is_some() {
        Some(LIMIT_NOTE.into())
    } else if corrupt {
        Some(CORRUPT_NOTE.into())
    } else {
        None
    };
    Ok(out)
}

fn guid(bytes: &[u8]) -> io::Result<String> {
    if bytes.len() < 16 {
        return Err(invalid("Truncated PDB identifier"));
    }
    Ok(format!(
        "{:08x}-{:04x}-{:04x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        u32_at(bytes, 0)?,
        u16_at(bytes, 4)?,
        u16_at(bytes, 6)?,
        bytes[8],
        bytes[9],
        bytes[10],
        bytes[11],
        bytes[12],
        bytes[13],
        bytes[14],
        bytes[15]
    ))
}

/// Portable PDB is ECMA-335 metadata, not MSF. Inspect only the metadata root, #Pdb ID and
/// #~ table row counts; embedded source blobs and referenced source paths are never opened.
fn portable(
    file: &mut File,
    length: u64,
    budget: &Budget,
    header: &[u8],
) -> io::Result<BinarySummary> {
    let version_len = u32_at(header, 12)? as usize;
    if version_len > 4096 {
        return Err(invalid(
            "Portable PDB version string exceeds preview limits",
        ));
    }
    let version = read_at(file, length, budget, 16, version_len)?;
    let tail_at = (16 + version_len).next_multiple_of(4);
    let tail = read_at(file, length, budget, tail_at as u64, 4)?;
    let count = u16_at(&tail, 2)? as usize;
    if count > 32 {
        return Err(invalid("Portable PDB stream count exceeds preview limits"));
    }
    let mut out = BinarySummary::default();
    prop(&mut out, "Format", "Portable PDB");
    prop(
        &mut out,
        "Version",
        format!("{}.{}", u16_at(header, 4)?, u16_at(header, 6)?),
    );
    prop(&mut out, "Metadata version", text(&version));
    prop(&mut out, "Streams", count);
    let mut cursor = tail_at + 4;
    let mut entries = Vec::new();
    for _ in 0..count {
        let data = read_at(file, length, budget, cursor as u64, 8)?;
        let offset = u32_at(&data, 0)? as u64;
        let size = u32_at(&data, 4)? as usize;
        if offset
            .checked_add(size as u64)
            .map_or(true, |end| end > length)
        {
            return Err(invalid("Portable PDB stream is outside the file"));
        }
        let mut name = Vec::new();
        let mut terminated = false;
        for n in 0..32 {
            let byte = read_at(file, length, budget, (cursor + 8 + n) as u64, 1)?[0];
            if byte == 0 {
                terminated = true;
                break;
            }
            name.push(byte);
        }
        if !terminated {
            return Err(invalid("Portable PDB stream name exceeds preview limits"));
        }
        cursor += 8 + (name.len() + 1).next_multiple_of(4);
        entries.push((offset, size, text(&name)));
    }
    let mut streams = Vec::new();
    let mut found_pdb = false;
    for (offset, size, name) in entries {
        if offset < cursor as u64 {
            return Err(invalid("Portable PDB stream overlaps its metadata header"));
        }
        push_item(
            &mut out,
            budget,
            &mut streams,
            format!("{name}: {size} bytes"),
        );
        if name == "#Pdb" {
            if size < 32 {
                return Err(invalid("Truncated Portable PDB identifier"));
            }
            let id = read_at(file, length, budget, offset, 32)?;
            let referenced_bytes = u64_at(&id, 24)?.count_ones() as usize * 4;
            if referenced_bytes > size - 32 {
                return Err(invalid("Truncated Portable PDB referenced table counts"));
            }
            found_pdb = true;
            prop(&mut out, "GUID", guid(&id[..16])?);
            prop(&mut out, "Signature", format!("0x{:08X}", u32_at(&id, 16)?));
            prop(
                &mut out,
                "Entry point",
                format!("0x{:08X}", u32_at(&id, 20)?),
            );
        }
        if name == "#~" || name == "#-" {
            if size < 24 {
                return Err(invalid("Truncated Portable PDB table header"));
            }
            let head = read_at(file, length, budget, offset, 24)?;
            let mask = u64_at(&head, 8)?;
            let row_bytes = mask.count_ones() as usize * 4;
            if row_bytes > size - 24 {
                return Err(invalid("Truncated Portable PDB table counts"));
            }
            let rows = read_at(file, length, budget, offset + 24, row_bytes)?;
            let mut at = 0;
            let mut tables = Vec::new();
            for n in 0..64 {
                if mask & (1 << n) == 0 {
                    continue;
                }
                let count = u32_at(&rows, at)?;
                at += 4;
                let name = match n {
                    0x30 => "Documents",
                    0x31 => "Methods",
                    0x32 => "Local scopes",
                    0x33 => "Local variables",
                    0x34 => "Local constants",
                    0x35 => "Import scopes",
                    0x36 => "State-machine methods",
                    0x37 => "Custom debug information",
                    _ => "Metadata table",
                };
                if !push_item(
                    &mut out,
                    budget,
                    &mut tables,
                    format!("0x{n:02X} {name}: {count}"),
                ) {
                    break;
                }
                if n == 0x30 {
                    prop(&mut out, "Source files", count);
                }
                if n == 0x31 {
                    prop(&mut out, "Methods", count);
                }
            }
            section(&mut out, "Metadata tables", tables);
        }
    }
    if !found_pdb {
        return Err(invalid("This metadata file is not a Portable PDB"));
    }
    section(&mut out, "Streams", streams);
    if budget.limited.get() || out.note.is_some() {
        out.note = Some(LIMIT_NOTE.into());
    }
    Ok(out)
}

pub(crate) fn summarize(path: &Path) -> Result<BinarySummary, String> {
    let mut file = File::open(path).map_err(|e| e.to_string())?;
    let length = file.metadata().map_err(|e| e.to_string())?.len();
    let budget = Budget::new();
    let head = read_at(&mut file, length, &budget, 0, 16).map_err(|e| e.to_string())?;
    let result = if head.starts_with(b"BSJB") {
        portable(&mut file, length, &budget, &head)
    } else {
        let header = read_at(&mut file, length, &budget, 0, 56).map_err(|e| e.to_string())?;
        native(&mut file, length, budget.clone(), &header)
    };
    result.map_err(|e| e.to_string())
}
