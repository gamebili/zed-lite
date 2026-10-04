//! Bounded, read-only structure inspection. Never maps or executes a selected binary.
use super::{Prop, Section};
use object::{
    Object, ObjectSection, ObjectSegment, ObjectSymbol,
    endian::LittleEndian as LE,
    read::{ReadCache, ReadRef},
};
use std::{
    cell::{Cell, RefCell},
    collections::{BTreeSet, HashSet},
    fs::File,
    ops::Range,
    path::Path,
    time::{Duration, Instant},
};

const BLOCK_LIMIT: u64 = 8 * 1024 * 1024;
const TOTAL_LIMIT: u64 = 32 * 1024 * 1024;
const SCAN_LIMIT: usize = 100_000;
pub(super) const DISPLAY_LIMIT: usize = 10_000;
pub(super) const REPORT_LIMIT: usize = 2 * 1024 * 1024;
const ITEM_REPORT_LIMIT: usize = REPORT_LIMIT - 32 * 1024;
#[path = "binary_macho.rs"]
mod macho;
#[cfg(test)]
#[path = "binary_tests.rs"]
mod tests;
const NOTE_LIMITED: &str = "The summary is limited to keep large binaries responsive.";
#[derive(Default, Debug, serde::Serialize)]
pub(crate) struct BinarySummary {
    pub props: Vec<Prop>,
    pub sections: Vec<Section>,
    pub note: Option<String>,
    #[serde(skip)]
    pub(super) report_bytes: usize,
}
impl BinarySummary {
    fn reserve_report(&mut self, bytes: usize) -> bool {
        if self.report_bytes.saturating_add(bytes) > REPORT_LIMIT {
            self.limited();
            return false;
        }
        self.report_bytes += bytes;
        true
    }
    pub(super) fn prop(&mut self, label: &str, value: impl ToString) {
        let value = value.to_string();
        if !self.reserve_report(label.len().saturating_add(value.len())) {
            return;
        }
        self.props.push(Prop {
            label: label.into(),
            value,
            time: None,
        });
    }
    pub(super) fn item(&mut self, items: &mut Vec<String>, value: String) -> bool {
        if items.len() >= DISPLAY_LIMIT
            || self.report_bytes.saturating_add(value.len()) > ITEM_REPORT_LIMIT
            || !self.reserve_report(value.len())
        {
            self.limited();
            return false;
        }
        items.push(value);
        true
    }
    fn overview_item(&mut self, items: &mut Vec<String>, value: String) -> bool {
        if items.len() >= DISPLAY_LIMIT || !self.reserve_report(value.len()) {
            self.limited();
            return false;
        }
        items.push(value);
        true
    }
    /// For callers with an existing vector; new parsers should budget each item first.
    pub(super) fn section(&mut self, title: &str, items: Vec<String>) {
        let mut bounded = Vec::new();
        for item in items {
            if !self.item(&mut bounded, item) {
                break;
            }
        }
        self.reserved_section(title, bounded);
    }
    /// Items were already charged by item(), including items for concurrent sections.
    pub(super) fn reserved_section(&mut self, title: &str, mut items: Vec<String>) {
        while !items.is_empty() && self.report_bytes.saturating_add(title.len()) > REPORT_LIMIT {
            self.limited();
            if let Some(removed) = items.pop() {
                self.report_bytes = self.report_bytes.saturating_sub(removed.len());
            }
        }
        if !items.is_empty() && self.reserve_report(title.len()) {
            self.sections.push(Section {
                title: title.into(),
                items,
            });
        }
    }
    pub(super) fn limited(&mut self) {
        if self.note.is_none() {
            self.note = Some(NOTE_LIMITED.into());
        }
    }
}
struct Budget {
    cache: ReadCache<File>,
    size: u64,
    bytes: Cell<u64>,
    calls: Cell<usize>,
    ranges: RefCell<HashSet<(u64, u64, u16)>>,
    limited: Cell<bool>,
    deadline: Instant,
}
#[derive(Clone, Copy)]
struct Data<'a> {
    budget: &'a Budget,
    offset: u64,
    size: u64,
}
impl Budget {
    fn new(path: &Path) -> Result<Self, String> {
        let file = File::open(path).map_err(|e| e.to_string())?;
        let metadata = file.metadata().map_err(|e| e.to_string())?;
        if !metadata.is_file() {
            return Err("The selected file is not a regular binary file.".into());
        }
        Ok(Self {
            cache: ReadCache::new(file),
            size: metadata.len(),
            bytes: Cell::new(0),
            calls: Cell::new(0),
            ranges: RefCell::new(HashSet::new()),
            limited: Cell::new(false),
            deadline: Instant::now() + Duration::from_secs(2),
        })
    }
    fn reserve(&self, offset: u64, size: u64, tag: u16) -> Result<(), ()> {
        let calls = self.calls.get().saturating_add(1);
        self.calls.set(calls);
        if calls > SCAN_LIMIT * 4 || size > BLOCK_LIMIT || Instant::now() >= self.deadline {
            self.limited.set(true);
            return Err(());
        }
        if !self.ranges.borrow().contains(&(offset, size, tag)) {
            let total = self.bytes.get().checked_add(size).ok_or(())?;
            if total > TOTAL_LIMIT {
                self.limited.set(true);
                return Err(());
            }
            self.bytes.set(total);
            self.ranges.borrow_mut().insert((offset, size, tag));
        }
        Ok(())
    }
    fn data(&self) -> Data<'_> {
        Data {
            budget: self,
            offset: 0,
            size: self.size,
        }
    }
}
impl<'a> Data<'a> {
    fn range(self, offset: u64, size: u64) -> Result<Self, ()> {
        if offset.checked_add(size).ok_or(())? > self.size {
            return Err(());
        }
        Ok(Self {
            budget: self.budget,
            offset: self.offset.checked_add(offset).ok_or(())?,
            size,
        })
    }
    fn live(self, count: usize) -> bool {
        count < SCAN_LIMIT && Instant::now() < self.budget.deadline
    }
}
impl<'a> ReadRef<'a> for Data<'a> {
    fn len(self) -> Result<u64, ()> {
        Ok(self.size)
    }
    fn read_bytes_at(self, offset: u64, size: u64) -> Result<&'a [u8], ()> {
        if offset.checked_add(size).ok_or(())? > self.size {
            return Err(());
        }
        let offset = self.offset.checked_add(offset).ok_or(())?;
        self.budget.reserve(offset, size, 0)?;
        (&self.budget.cache).read_bytes_at(offset, size)
    }
    fn read_bytes_at_until(self, range: Range<u64>, delimiter: u8) -> Result<&'a [u8], ()> {
        if range.start > range.end || range.end > self.size {
            return Err(());
        }
        let size = (range.end - range.start).min(4096);
        let start = self.offset.checked_add(range.start).ok_or(())?;
        self.budget.reserve(start, size, delimiter as u16 + 1)?;
        let bytes = (&self.budget.cache)
            .read_bytes_at_until(start..start.checked_add(size).ok_or(())?, delimiter)?;
        if bytes.len() as u64 >= size {
            return Err(());
        }
        Ok(bytes)
    }
}
fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(&bytes[..bytes.len().min(4096)])
        .chars()
        .take(1024)
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .collect()
}
fn count(number: usize, complete: bool) -> String {
    if complete {
        number.to_string()
    } else {
        format!("≥ {number}")
    }
}

pub(crate) fn summarize(path: &Path) -> Result<BinarySummary, String> {
    if super::extension(&path.to_string_lossy()) == "pdb" {
        return super::binary_pdb::summarize(path);
    }
    let budget = Budget::new(path)?;
    let data = budget.data();
    let kind = object::FileKind::parse(data)
        .map_err(|e| format!("Unsupported or damaged binary file: {e}"))?;
    let mut summary = BinarySummary::default();
    summary.prop(
        "Format",
        match kind {
            object::FileKind::Archive => "Unix archive",
            object::FileKind::Pe32 | object::FileKind::Pe64 => "PE",
            object::FileKind::Coff | object::FileKind::CoffBig => "COFF",
            object::FileKind::CoffImport => "COFF import",
            object::FileKind::MachO32
            | object::FileKind::MachO64
            | object::FileKind::MachOFat32
            | object::FileKind::MachOFat64 => "Mach-O",
            object::FileKind::Elf32 | object::FileKind::Elf64 => "ELF",
            _ => "Object file",
        },
    );
    let result = match kind {
        object::FileKind::Archive => archive(data, &mut summary),
        object::FileKind::CoffImport => short_import(data, &mut summary),
        object::FileKind::MachOFat32 => fat::<object::macho::FatArch32>(data, &mut summary),
        object::FileKind::MachOFat64 => fat::<object::macho::FatArch64>(data, &mut summary),
        object::FileKind::MachO32 => {
            macho::summarize::<object::macho::MachHeader32<object::Endianness>>(data, &mut summary)
        }
        object::FileKind::MachO64 => {
            macho::summarize::<object::macho::MachHeader64<object::Endianness>>(data, &mut summary)
        }
        _ => object_file(data, &mut summary),
    };
    if let Err(error) = result {
        summary.note = Some(if budget.limited.get() {
            NOTE_LIMITED.into()
        } else {
            format!("Some binary structures could not be read: {error}")
        });
    }
    if budget.limited.get() {
        summary.limited();
    }
    Ok(summary)
}
fn object_file(data: Data<'_>, summary: &mut BinarySummary) -> Result<(), String> {
    let file = object::File::parse(data).map_err(|e| e.to_string())?;
    summary.prop("Architecture", format!("{:?}", file.architecture()));
    summary.prop("Binary type", format!("{:?}", file.kind()));
    summary.prop("Bitness", if file.is_64() { "64-bit" } else { "32-bit" });
    summary.prop(
        "Endianness",
        if file.is_little_endian() {
            "Little endian"
        } else {
            "Big endian"
        },
    );
    summary.prop("Entry point", format!("0x{:X}", file.entry()));
    let mut sections = Vec::new();
    let mut number = 0;
    for section in file.sections() {
        if !data.live(number) {
            summary.limited();
            break;
        }
        if number < DISPLAY_LIMIT {
            summary.item(
                &mut sections,
                format!(
                    "{} — address 0x{:X}, size {} bytes, {:?}",
                    text(section.name_bytes().unwrap_or(b"?")),
                    section.address(),
                    section.size(),
                    section.flags()
                ),
            );
        }
        number += 1;
    }
    summary.prop("Sections", count(number, data.live(number)));
    summary.reserved_section("Sections", sections);
    if number > DISPLAY_LIMIT {
        summary.limited();
    }
    let mut segments = Vec::new();
    for (number, segment) in file.segments().take(DISPLAY_LIMIT + 1).enumerate() {
        if number == DISPLAY_LIMIT {
            summary.limited();
            break;
        }
        let (offset, size) = segment.file_range();
        summary.item(
            &mut segments,
            format!(
                "{} — address 0x{:X}, memory {} bytes, file 0x{offset:X} + {size} bytes",
                text(segment.name_bytes().ok().flatten().unwrap_or(b"")),
                segment.address(),
                segment.size()
            ),
        );
    }
    summary.reserved_section("Segments", segments);
    let mut symbols = Vec::new();
    let mut number = 0;
    let mut complete = true;
    for symbol in file.symbols() {
        if !data.live(number) {
            complete = false;
            summary.limited();
            break;
        }
        if number < DISPLAY_LIMIT {
            summary.item(
                &mut symbols,
                format!(
                    "{} — 0x{:X}, {} bytes, {:?}",
                    text(symbol.name_bytes().unwrap_or(b"?")),
                    symbol.address(),
                    symbol.size(),
                    symbol.kind()
                ),
            );
        }
        number += 1;
    }
    summary.prop("Symbols", count(number, complete));
    summary.reserved_section("Symbols", symbols);
    if number > DISPLAY_LIMIT {
        summary.limited();
    }
    match &file {
        object::File::Pe32(pe) => pe_details(pe, data, summary)?,
        object::File::Pe64(pe) => pe_details(pe, data, summary)?,
        // The general imports()/exports() APIs eagerly allocate an entry per symbol and
        // cannot be interrupted once their raw tables are cached. Leave these optional
        // fields out for other formats; the bounded Symbols section above is independent
        // of dynamic-loader visibility and must not be presented as an export table.
        _ => {}
    }
    if let Some(pdb) = file.pdb_info().map_err(|e| e.to_string())? {
        summary.prop("PDB file", text(pdb.path()));
        summary.prop("PDB age", pdb.age());
        let g = pdb.guid();
        summary.prop(
            "PDB GUID",
            format!(
                "{:08X}-{:04X}-{:04X}-{:02X}{:02X}-{}",
                u32::from_le_bytes([g[0], g[1], g[2], g[3]]),
                u16::from_le_bytes([g[4], g[5]]),
                u16::from_le_bytes([g[6], g[7]]),
                g[8],
                g[9],
                g[10..]
                    .iter()
                    .map(|b| format!("{b:02X}"))
                    .collect::<String>()
            ),
        );
    }
    Ok(())
}
fn pe_details<'a, Pe: object::read::pe::ImageNtHeaders>(
    file: &object::read::pe::PeFile<'a, Pe, Data<'a>>,
    data: Data<'a>,
    summary: &mut BinarySummary,
) -> Result<(), String> {
    use object::read::pe::{ImageOptionalHeader, ImageThunkData};
    let header = file.nt_headers();
    let optional = header.optional_header();
    summary.prop("Image base", format!("0x{:X}", optional.image_base()));
    let subsystem = optional.subsystem();
    summary.prop(
        "Subsystem",
        match subsystem {
            1 => "Native",
            2 => "Windows GUI",
            3 => "Windows console",
            7 => "POSIX",
            9 => "Windows CE GUI",
            10 => "EFI application",
            11 => "EFI boot service driver",
            12 => "EFI runtime driver",
            14 => "Xbox",
            _ => "Other",
        },
    );
    let timestamp = header.file_header().time_date_stamp.get(LE);
    summary.prop("Timestamp", timestamp);
    if let Some(prop) = summary.props.last_mut().filter(|p| p.label == "Timestamp") {
        prop.time = (timestamp != 0).then_some(timestamp as i64 * 1000);
    }
    let mut import_count = 0;
    let mut libraries = BTreeSet::new();
    let mut imports = Vec::new();
    let mut complete = true;
    if let Some(table) = file.import_table().map_err(|e| e.to_string())? {
        let mut descriptors = table.descriptors().map_err(|e| e.to_string())?;
        let mut descriptor_count = 0;
        'imports: while let Some(descriptor) = descriptors.next().map_err(|e| e.to_string())? {
            if !data.live(descriptor_count) {
                complete = false;
                break;
            }
            descriptor_count += 1;
            // Retain borrowed bytes rather than a truncated, owned display string:
            // malicious overlapping long names must not allocate one copy per DLL,
            // and differences beyond the display limit still affect the true count.
            let library = table
                .name(descriptor.name.get(LE))
                .map_err(|e| e.to_string())?;
            libraries.insert(library);
            let mut thunk_address = descriptor.original_first_thunk.get(LE);
            if thunk_address == 0 {
                thunk_address = descriptor.first_thunk.get(LE);
            }
            let mut thunks = table.thunks(thunk_address).map_err(|e| e.to_string())?;
            while let Some(thunk) = thunks.next::<Pe>().map_err(|e| e.to_string())? {
                if !data.live(import_count) {
                    complete = false;
                    break 'imports;
                }
                if import_count < DISPLAY_LIMIT {
                    let name = if thunk.is_ordinal() {
                        format!("#{}", thunk.ordinal())
                    } else {
                        text(
                            table
                                .hint_name(thunk.address())
                                .map_err(|e| e.to_string())?
                                .1,
                        )
                    };
                    summary.item(&mut imports, format!("{}!{name}", text(library)));
                }
                import_count += 1;
            }
        }
    }
    summary.prop("Imports", count(import_count, complete));
    summary.prop("Imported libraries", count(libraries.len(), complete));
    summary.reserved_section("Imports", imports);
    if !complete || import_count > DISPLAY_LIMIT {
        summary.limited();
    }
    if let Some(table) = file.export_table().map_err(|e| e.to_string())? {
        let mut exported = Vec::new();
        let mut total = 0;
        let mut complete = true;
        for (index, address) in table.addresses().iter().enumerate() {
            if !data.live(index) {
                complete = false;
                summary.limited();
                break;
            }
            if address.get(LE) != 0 {
                if total < DISPLAY_LIMIT {
                    exported.push(index);
                }
                total += 1;
            }
        }
        summary.prop("Exports", count(total, complete));
        let wanted: HashSet<_> = exported.iter().copied().collect();
        let mut names = std::collections::HashMap::new();
        for (number, (pointer, index)) in table.name_iter().enumerate() {
            if !data.live(number) {
                summary.limited();
                break;
            }
            if !wanted.contains(&(index as usize)) {
                continue;
            }
            names.insert(
                index as usize,
                text(
                    table
                        .name_from_pointer(pointer)
                        .map_err(|e| e.to_string())?,
                ),
            );
        }
        let mut exports = Vec::new();
        for index in exported {
            let target = table
                .target_by_index(index as u32)
                .map_err(|e| e.to_string())?;
            let name = names.get(&index).cloned().map(Ok).unwrap_or_else(|| {
                table
                    .ordinal_base()
                    .checked_add(index as u32)
                    .map(|ordinal| format!("#{ordinal}"))
                    .ok_or("Invalid PE export ordinal range")
            })?;
            let target = match target {
                object::read::pe::ExportTarget::Address(address) => format!("RVA 0x{address:X}"),
                object::read::pe::ExportTarget::ForwardByOrdinal(library, ordinal) => {
                    format!("{}!#{ordinal}", text(library))
                }
                object::read::pe::ExportTarget::ForwardByName(library, name) => {
                    format!("{}!{}", text(library), text(name))
                }
            };
            summary.item(&mut exports, format!("{name} — {target}"));
        }
        summary.reserved_section("Exports", exports);
        if total > DISPLAY_LIMIT {
            summary.limited();
        }
    }
    Ok(())
}
fn short_import(data: Data<'_>, summary: &mut BinarySummary) -> Result<(), String> {
    let file = object::read::coff::ImportFile::parse(data).map_err(|e| e.to_string())?;
    summary.prop("Architecture", format!("{:?}", file.architecture()));
    summary.prop("Binary type", "Import library entry");
    summary.section(
        "Imports",
        vec![format!(
            "{}!{} — {:?}",
            text(file.dll()),
            text(file.symbol()),
            file.import_type()
        )],
    );
    Ok(())
}
fn fat<'a, Fat: object::read::macho::FatArch>(
    data: Data<'a>,
    summary: &mut BinarySummary,
) -> Result<(), String> {
    let file = object::read::macho::MachOFatFile::<Fat>::parse(data).map_err(|e| e.to_string())?;
    summary.prop("Architecture", "Universal");
    summary.prop("Architectures", file.arches().len());
    if file.arches().len() > DISPLAY_LIMIT {
        summary.limited();
    }
    let mut architectures = Vec::new();
    let mut slice_props = Vec::new();
    for (index, arch) in file.arches().iter().enumerate() {
        if !data.live(index) {
            summary.limited();
            break;
        }
        let (offset, size) = arch.file_range();
        let name = format!("{:?}", arch.architecture());
        summary.item(
            &mut architectures,
            format!("{name} — offset 0x{offset:X}, {size} bytes"),
        );
        let range = match data.range(offset, size) {
            Ok(range) => range,
            Err(_) => {
                summary.note = Some(
                    "Some binary structures could not be read: Invalid Mach-O slice range".into(),
                );
                continue;
            }
        };
        let mut slice = BinarySummary {
            report_bytes: summary.report_bytes,
            ..Default::default()
        };
        let result = match object::FileKind::parse(range) {
            Ok(object::FileKind::MachO32) => macho::summarize::<
                object::macho::MachHeader32<object::Endianness>,
            >(range, &mut slice),
            Ok(object::FileKind::MachO64) => macho::summarize::<
                object::macho::MachHeader64<object::Endianness>,
            >(range, &mut slice),
            _ => Err("Invalid Mach-O slice header".into()),
        };
        summary.report_bytes = slice.report_bytes;
        if let Some(note) = slice.note {
            summary.note = Some(note);
        }
        if let Err(error) = result {
            summary.note = Some(if data.budget.limited.get() {
                NOTE_LIMITED.into()
            } else {
                format!("Some binary structures could not be read: {error}")
            });
        }
        // Refund each original entry before adding its architecture prefix. All slices
        // share the same report/read/time budgets; no slice payload is copied wholesale.
        for prop in slice.props {
            summary.report_bytes = summary
                .report_bytes
                .saturating_sub(prop.label.len() + prop.value.len());
            summary.overview_item(
                &mut slice_props,
                format!("[{name}] {}: {}", prop.label, prop.value),
            );
        }
        for section in slice.sections {
            summary.report_bytes = summary.report_bytes.saturating_sub(section.title.len());
            let mut prefixed = Vec::new();
            for item in section.items {
                summary.report_bytes = summary.report_bytes.saturating_sub(item.len());
                summary.item(&mut prefixed, format!("[{name}] {item}"));
            }
            summary.reserved_section(&section.title, prefixed);
        }
    }
    summary.reserved_section("Architectures", architectures);
    summary.reserved_section("Architecture slices", slice_props);
    Ok(())
}
fn archive(data: Data<'_>, summary: &mut BinarySummary) -> Result<(), String> {
    let file = object::read::archive::ArchiveFile::parse(data).map_err(|e| e.to_string())?;
    summary.prop(
        "Archive format",
        format!(
            "{:?}{}",
            file.kind(),
            if file.is_thin() { " (thin)" } else { "" }
        ),
    );
    let mut items = Vec::new();
    let mut imports = Vec::new();
    let mut architectures = BTreeSet::new();
    let mut number = 0;
    let mut complete = true;
    for member in file.members() {
        if !data.live(number) {
            complete = false;
            summary.limited();
            break;
        }
        let member = match member {
            Ok(member) => member,
            Err(error) => {
                complete = false;
                summary.note = Some(if data.budget.limited.get() {
                    NOTE_LIMITED.into()
                } else {
                    format!("Some binary structures could not be read: {error}")
                });
                break;
            }
        };
        let (offset, size) = member.file_range();
        if number < DISPLAY_LIMIT {
            let mut detail = String::new();
            if !member.is_thin() && size >= 16 {
                if let Ok(range) = data.range(offset, size) {
                    if let Ok(kind) = object::FileKind::parse(range) {
                        detail = format!(" — {kind:?}");
                        if kind == object::FileKind::CoffImport {
                            if let Ok(import) = object::read::coff::ImportFile::parse(range) {
                                architectures.insert(format!("{:?}", import.architecture()));
                                summary.item(
                                    &mut imports,
                                    format!("{}!{}", text(import.dll()), text(import.symbol())),
                                );
                            }
                        } else if let Ok(object) = object::File::parse(range) {
                            architectures.insert(format!("{:?}", object.architecture()));
                        }
                    }
                }
            }
            summary.item(
                &mut items,
                format!("{} — {} bytes{detail}", text(member.name()), member.size()),
            );
        }
        number += 1;
    }
    summary.prop("Archive members", count(number, complete));
    summary.reserved_section("Archive members", items);
    summary.reserved_section("Imports", imports);
    match file.symbols() {
        Ok(Some(symbols)) => {
            let mut items = Vec::new();
            let mut number = 0;
            let mut complete = true;
            for symbol in symbols {
                if !data.live(number) {
                    complete = false;
                    summary.limited();
                    break;
                }
                match symbol {
                    Ok(symbol) => {
                        if number < DISPLAY_LIMIT {
                            summary.item(
                                &mut items,
                                format!(
                                    "{} — member offset 0x{:X}",
                                    text(symbol.name()),
                                    symbol.offset().0
                                ),
                            );
                        }
                        number += 1;
                    }
                    Err(error) => {
                        summary.note =
                            Some(format!("Some binary structures could not be read: {error}"));
                        complete = false;
                        break;
                    }
                }
            }
            summary.prop("Archive symbols", count(number, complete));
            summary.reserved_section("Archive symbols", items);
            if number > DISPLAY_LIMIT {
                summary.limited();
            }
        }
        Ok(None) => {}
        Err(error) => {
            summary.note = Some(if data.budget.limited.get() {
                NOTE_LIMITED.into()
            } else {
                format!("Some binary structures could not be read: {error}")
            });
        }
    }
    if !architectures.is_empty() {
        summary.prop(
            "Architecture",
            architectures.into_iter().collect::<Vec<_>>().join(", "),
        );
    }
    if number > DISPLAY_LIMIT {
        summary.limited();
    }
    Ok(())
}
