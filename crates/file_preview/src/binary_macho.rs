//! Mach-O metadata from bounded raw commands, never dyld loading or eager symbol vectors.
use super::{BinarySummary, DISPLAY_LIMIT, Data, SCAN_LIMIT, count, text};
use object::{
    endian::Endian,
    macho as m,
    read::{
        ReadRef, StringTable,
        macho::{LoadCommandVariant, MachHeader, Nlist, Section, Segment},
    },
};
#[path = "binary_macho_bind.rs"]
mod bind;
#[path = "binary_macho_dyld.rs"]
mod dyld;

fn fixed_at<const SIZE: usize>(bytes: &[u8], at: usize) -> Result<[u8; SIZE], String> {
    let end = at
        .checked_add(SIZE)
        .ok_or("Mach-O metadata offset overflow")?;
    bytes
        .get(at..end)
        .ok_or("Truncated Mach-O metadata")?
        .try_into()
        .map_err(|_| "Truncated Mach-O metadata".into())
}

fn damaged(out: &mut BinarySummary, error: impl std::fmt::Display) {
    out.note = Some(format!("Some binary structures could not be read: {error}"));
}
fn problem(data: Data<'_>, out: &mut BinarySummary, error: impl std::fmt::Display) {
    if data.budget.limited.get() {
        out.limited();
    } else {
        damaged(out, error);
    }
}
fn version(v: u32) -> String {
    format!("{}.{}.{}", v >> 16, (v >> 8) & 255, v & 255)
}
fn platform(v: u32) -> String {
    match v {
        1 => "macOS",
        2 => "iOS",
        3 => "tvOS",
        4 => "watchOS",
        5 => "bridgeOS",
        6 => "Mac Catalyst",
        7 => "iOS Simulator",
        8 => "tvOS Simulator",
        9 => "watchOS Simulator",
        10 => "DriverKit",
        11 => "visionOS",
        12 => "visionOS Simulator",
        _ => return format!("Platform {v}"),
    }
    .into()
}
fn architecture(cpu: u32) -> &'static str {
    match cpu {
        7 => "I386",
        0x01000007 => "X86_64",
        12 => "Arm",
        0x0100000c => "Aarch64",
        0x0200000c => "Arm64_32",
        18 => "PowerPc",
        0x01000012 => "PowerPc64",
        _ => "Unknown",
    }
}
fn filetype(v: u32) -> &'static str {
    match v {
        1 => "Relocatable",
        2 => "Executable",
        3 => "Fixed VM shared library",
        4 => "Core",
        5 => "Preload",
        6 => "Dynamic",
        7 => "Dynamic linker",
        8 => "Bundle",
        9 => "Dynamic library stub",
        10 => "Debug symbols",
        11 => "Kernel extension",
        12 => "Fileset",
        _ => "Unknown",
    }
}
fn command_name(cmd: u32) -> String {
    let name = match cmd {
        m::LC_SEGMENT => "LC_SEGMENT",
        m::LC_SEGMENT_64 => "LC_SEGMENT_64",
        m::LC_SYMTAB => "LC_SYMTAB",
        m::LC_DYSYMTAB => "LC_DYSYMTAB",
        m::LC_UUID => "LC_UUID",
        m::LC_ID_DYLIB => "LC_ID_DYLIB",
        m::LC_LOAD_DYLIB => "LC_LOAD_DYLIB",
        m::LC_LOAD_WEAK_DYLIB => "LC_LOAD_WEAK_DYLIB",
        m::LC_REEXPORT_DYLIB => "LC_REEXPORT_DYLIB",
        m::LC_LOAD_UPWARD_DYLIB => "LC_LOAD_UPWARD_DYLIB",
        m::LC_LAZY_LOAD_DYLIB => "LC_LAZY_LOAD_DYLIB",
        m::LC_RPATH => "LC_RPATH",
        m::LC_BUILD_VERSION => "LC_BUILD_VERSION",
        m::LC_VERSION_MIN_MACOSX => "LC_VERSION_MIN_MACOSX",
        m::LC_VERSION_MIN_IPHONEOS => "LC_VERSION_MIN_IPHONEOS",
        m::LC_VERSION_MIN_TVOS => "LC_VERSION_MIN_TVOS",
        m::LC_VERSION_MIN_WATCHOS => "LC_VERSION_MIN_WATCHOS",
        m::LC_CODE_SIGNATURE => "LC_CODE_SIGNATURE",
        m::LC_FUNCTION_STARTS => "LC_FUNCTION_STARTS",
        m::LC_DATA_IN_CODE => "LC_DATA_IN_CODE",
        m::LC_DYLD_EXPORTS_TRIE => "LC_DYLD_EXPORTS_TRIE",
        m::LC_DYLD_CHAINED_FIXUPS => "LC_DYLD_CHAINED_FIXUPS",
        m::LC_DYLD_INFO => "LC_DYLD_INFO",
        m::LC_DYLD_INFO_ONLY => "LC_DYLD_INFO_ONLY",
        m::LC_MAIN => "LC_MAIN",
        m::LC_ENCRYPTION_INFO => "LC_ENCRYPTION_INFO",
        m::LC_ENCRYPTION_INFO_64 => "LC_ENCRYPTION_INFO_64",
        m::LC_SOURCE_VERSION => "LC_SOURCE_VERSION",
        _ => return format!("LC_0x{cmd:X}"),
    };
    name.into()
}
fn flags(v: u32) -> String {
    let mut names = Vec::new();
    for (flag, name) in [
        (1, "NOUNDEFS"),
        (4, "DYLDLINK"),
        (0x20, "SPLIT_SEGS"),
        (0x80, "TWOLEVEL"),
        (0x100, "FORCE_FLAT"),
        (0x200, "NOMULTIDEFS"),
        (0x8000, "WEAK_DEFINES"),
        (0x10000, "BINDS_TO_WEAK"),
        (0x20000, "ALLOW_STACK_EXECUTION"),
        (0x100000, "NO_REEXPORTED_DYLIBS"),
        (0x200000, "PIE"),
        (0x400000, "DEAD_STRIPPABLE_DYLIB"),
        (0x800000, "HAS_TLV_DESCRIPTORS"),
        (0x1000000, "NO_HEAP_EXECUTION"),
        (0x2000000, "APP_EXTENSION_SAFE"),
    ] {
        if v & flag != 0 {
            names.push(name);
        }
    }
    format!(
        "0x{v:08X}{}",
        if names.is_empty() {
            String::new()
        } else {
            format!(" — {}", names.join(", "))
        }
    )
}

pub(super) fn summarize<'a, H: MachHeader>(
    data: Data<'a>,
    out: &mut BinarySummary,
) -> Result<(), String> {
    let header = H::parse(data, 0).map_err(|e| e.to_string())?;
    let endian = header.endian().map_err(|e| e.to_string())?;
    out.prop("Architecture", architecture(header.cputype(endian)));
    out.prop("CPU subtype", format!("0x{:X}", header.cpusubtype(endian)));
    out.prop("Binary type", filetype(header.filetype(endian)));
    out.prop(
        "Bitness",
        if header.is_type_64() {
            "64-bit"
        } else {
            "32-bit"
        },
    );
    out.prop(
        "Endianness",
        if header.is_little_endian() {
            "Little endian"
        } else {
            "Big endian"
        },
    );
    out.prop("Header flags", flags(header.flags(endian)));
    out.prop("Load commands", header.ncmds(endian));
    out.prop("Load command bytes", header.sizeofcmds(endian));
    let mut commands = header
        .load_commands(endian, data, 0)
        .map_err(|e| e.to_string())?;
    let (mut command_rows, mut dependencies, mut rpaths, mut regions, mut segments, mut sections) = (
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
    );
    let mut libraries: Vec<&[u8]> = Vec::new();
    let (
        mut symtab,
        mut dysymtab,
        mut export_region,
        mut chains,
        mut function_starts,
        mut signature,
    ) = (None, None, None, None, None, None);
    let mut command_offset = std::mem::size_of::<H>() as u64;
    let mut parsed = 0;
    let mut section_count = 0;
    let mut metadata_complete = true;
    let mut binding_regions = Vec::new();
    loop {
        if !data.live(parsed) {
            out.limited();
            metadata_complete &= parsed == header.ncmds(endian) as usize;
            break;
        }
        let command = match commands.next() {
            Ok(Some(c)) => c,
            Ok(None) => break,
            Err(e) => {
                problem(data, out, e);
                metadata_complete = false;
                break;
            }
        };
        if parsed < DISPLAY_LIMIT {
            out.item(
                &mut command_rows,
                format!(
                    "{} — offset 0x{command_offset:X}, {} bytes",
                    command_name(command.cmd()),
                    command.cmdsize()
                ),
            );
        }
        command_offset = command_offset
            .checked_add(command.cmdsize() as u64)
            .ok_or("Invalid Mach-O command offset")?;
        parsed += 1;
        if matches!(
            command.cmd(),
            m::LC_LOAD_DYLIB
                | m::LC_LOAD_WEAK_DYLIB
                | m::LC_REEXPORT_DYLIB
                | m::LC_LAZY_LOAD_DYLIB
                | m::LC_LOAD_UPWARD_DYLIB
        ) {
            // Preserve ordinals even if the typed command itself is malformed.
            libraries.push(b"<unreadable dylib name>");
        }
        let result: Result<(), String> = (|| {
            match command.variant().map_err(|e| e.to_string())? {
                LoadCommandVariant::Dylib(lib) | LoadCommandVariant::IdDylib(lib) => {
                    let name = command
                        .string(endian, lib.dylib.name)
                        .map_err(|e| e.to_string())?;
                    if command.cmd() == m::LC_ID_DYLIB {
                        out.prop("Dylib ID", text(name));
                        out.prop(
                            "Current version",
                            version(lib.dylib.current_version.get(endian)),
                        );
                        out.prop(
                            "Compatibility version",
                            version(lib.dylib.compatibility_version.get(endian)),
                        );
                    } else {
                        *libraries.last_mut().ok_or("Missing Mach-O dylib ordinal")? = name;
                        out.item(
                            &mut dependencies,
                            format!(
                                "#{} {} — {}, current {}, compatibility {}",
                                libraries.len(),
                                text(name),
                                command_name(command.cmd()),
                                version(lib.dylib.current_version.get(endian)),
                                version(lib.dylib.compatibility_version.get(endian))
                            ),
                        );
                    }
                }
                LoadCommandVariant::Rpath(path) => {
                    out.item(
                        &mut rpaths,
                        text(
                            command
                                .string(endian, path.path)
                                .map_err(|e| e.to_string())?,
                        ),
                    );
                }
                LoadCommandVariant::Uuid(uuid) => {
                    let b = uuid.uuid;
                    out.prop(
                        "UUID",
                        format!(
                            "{:02X}{:02X}{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}-{}",
                            b[0],
                            b[1],
                            b[2],
                            b[3],
                            b[4],
                            b[5],
                            b[6],
                            b[7],
                            b[8],
                            b[9],
                            b[10..]
                                .iter()
                                .map(|v| format!("{v:02X}"))
                                .collect::<String>()
                        ),
                    );
                }
                LoadCommandVariant::BuildVersion(build) => {
                    out.prop("Platform", platform(build.platform.get(endian)));
                    out.prop("Minimum OS", version(build.minos.get(endian)));
                    out.prop("SDK", version(build.sdk.get(endian)));
                    let raw = command.raw_data();
                    let count = build.ntools.get(endian) as usize;
                    if count
                        .checked_mul(8)
                        .and_then(|n| n.checked_add(24))
                        .filter(|&n| n <= raw.len())
                        .is_none()
                    {
                        return Err("Invalid Mach-O build tools table".into());
                    }
                    for (i, tool) in raw[24..24 + count * 8].chunks_exact(8).enumerate() {
                        if !data.live(i) {
                            out.limited();
                            break;
                        }
                        out.item(
                            &mut regions,
                            format!(
                                "Build tool {} — {}",
                                endian.read_u32_bytes(fixed_at(tool, 0)?),
                                version(endian.read_u32_bytes(fixed_at(tool, 4)?))
                            ),
                        );
                    }
                }
                LoadCommandVariant::VersionMin(build) => {
                    let p = match command.cmd() {
                        m::LC_VERSION_MIN_IPHONEOS => 2,
                        m::LC_VERSION_MIN_TVOS => 3,
                        m::LC_VERSION_MIN_WATCHOS => 4,
                        _ => 1,
                    };
                    out.prop("Platform", platform(p));
                    out.prop("Minimum OS", version(build.version.get(endian)));
                    out.prop("SDK", version(build.sdk.get(endian)));
                }
                LoadCommandVariant::Symtab(table) => {
                    symtab = Some(table);
                    out.prop("Declared symbols", table.nsyms.get(endian));
                }
                LoadCommandVariant::Dysymtab(table) => {
                    dysymtab = Some(table);
                    out.prop("External definitions", table.nextdefsym.get(endian));
                    out.prop("Undefined symbols", table.nundefsym.get(endian));
                }
                LoadCommandVariant::Segment32(segment, bytes) => segment_details(
                    data,
                    out,
                    segment,
                    endian,
                    bytes,
                    &mut segments,
                    &mut sections,
                    &mut section_count,
                )?,
                LoadCommandVariant::Segment64(segment, bytes) => segment_details(
                    data,
                    out,
                    segment,
                    endian,
                    bytes,
                    &mut segments,
                    &mut sections,
                    &mut section_count,
                )?,
                LoadCommandVariant::LinkeditData(region) => {
                    let offset = region.dataoff.get(endian) as u64;
                    let size = region.datasize.get(endian) as u64;
                    let valid = data.range(offset, size).is_ok();
                    out.item(
                        &mut regions,
                        format!(
                            "{} — offset 0x{offset:X}, {size} bytes{}",
                            command_name(command.cmd()),
                            if valid { "" } else { " — invalid file range" }
                        ),
                    );
                    if !valid {
                        return Err("Invalid Mach-O linkedit range".into());
                    }
                    match command.cmd() {
                        m::LC_DYLD_EXPORTS_TRIE => export_region = Some((offset, size)),
                        m::LC_DYLD_CHAINED_FIXUPS => chains = Some((offset, size)),
                        m::LC_FUNCTION_STARTS => function_starts = Some((offset, size)),
                        m::LC_CODE_SIGNATURE => signature = Some((offset, size)),
                        _ => {}
                    }
                }
                LoadCommandVariant::DyldInfo(info) => {
                    for (title, off, size, lazy) in [
                        (
                            "Bindings",
                            info.bind_off.get(endian),
                            info.bind_size.get(endian),
                            false,
                        ),
                        (
                            "Weak bindings",
                            info.weak_bind_off.get(endian),
                            info.weak_bind_size.get(endian),
                            false,
                        ),
                        (
                            "Lazy bindings",
                            info.lazy_bind_off.get(endian),
                            info.lazy_bind_size.get(endian),
                            true,
                        ),
                    ] {
                        if size != 0 && binding_regions.len() < 3 {
                            binding_regions.push((title, off as u64, size as u64, lazy));
                        }
                    }
                    if export_region.is_none() {
                        export_region = Some((
                            info.export_off.get(endian) as u64,
                            info.export_size.get(endian) as u64,
                        ));
                    }
                    for (label, offset, size) in [
                        (
                            "Rebase opcodes",
                            info.rebase_off.get(endian),
                            info.rebase_size.get(endian),
                        ),
                        (
                            "Bind opcodes",
                            info.bind_off.get(endian),
                            info.bind_size.get(endian),
                        ),
                        (
                            "Weak bind opcodes",
                            info.weak_bind_off.get(endian),
                            info.weak_bind_size.get(endian),
                        ),
                        (
                            "Lazy bind opcodes",
                            info.lazy_bind_off.get(endian),
                            info.lazy_bind_size.get(endian),
                        ),
                    ] {
                        if size != 0 {
                            out.item(
                                &mut regions,
                                format!("{label} — offset 0x{offset:X}, {size} bytes"),
                            );
                            if data.range(offset as u64, size as u64).is_err() {
                                return Err("Invalid Mach-O dyld info range".into());
                            }
                        }
                    }
                }
                LoadCommandVariant::EncryptionInfo32(e) => {
                    out.prop("Encryption ID", e.cryptid.get(endian));
                    out.item(
                        &mut regions,
                        format!(
                            "Encrypted data — offset 0x{:X}, {} bytes",
                            e.cryptoff.get(endian),
                            e.cryptsize.get(endian)
                        ),
                    );
                }
                LoadCommandVariant::EncryptionInfo64(e) => {
                    out.prop("Encryption ID", e.cryptid.get(endian));
                    out.item(
                        &mut regions,
                        format!(
                            "Encrypted data — offset 0x{:X}, {} bytes",
                            e.cryptoff.get(endian),
                            e.cryptsize.get(endian)
                        ),
                    );
                }
                LoadCommandVariant::EntryPoint(entry) => {
                    out.prop(
                        "Entry point",
                        format!("file offset 0x{:X}", entry.entryoff.get(endian)),
                    );
                    out.prop("Stack size", entry.stacksize.get(endian));
                }
                LoadCommandVariant::SourceVersion(v) => {
                    let n = v.version.get(endian);
                    out.prop(
                        "Source version",
                        format!(
                            "{}.{}.{}.{}.{}",
                            n >> 40,
                            (n >> 30) & 1023,
                            (n >> 20) & 1023,
                            (n >> 10) & 1023,
                            n & 1023
                        ),
                    );
                }
                _ => {}
            }
            Ok(())
        })();
        if let Err(error) = result {
            problem(data, out, error);
            metadata_complete = false;
        }
    }
    out.prop("Parsed load commands", parsed);
    out.prop(
        "Sections",
        count(section_count, metadata_complete && data.live(section_count)),
    );
    out.prop("Dependencies", count(libraries.len(), metadata_complete));
    out.reserved_section("Load commands", command_rows);
    out.reserved_section("Dependencies", dependencies);
    out.reserved_section("Rpaths", rpaths);
    out.reserved_section("Linkedit regions", regions);
    out.reserved_section("Segments", segments);
    out.reserved_section("Sections", sections);
    if let Some((offset, size)) = chains {
        if let Err(e) = chained_imports(data, offset, size, &libraries, out) {
            problem(data, out, e);
        }
    }
    if let Some((offset, size)) = export_region {
        if let Err(e) = dyld::exports(data, offset, size, &libraries, out) {
            problem(data, out, e);
        }
    }
    if let Some((offset, size)) = signature {
        if let Err(e) = code_signature(data, offset, size, out) {
            problem(data, out, e);
        }
    }
    if let Some((offset, size)) = function_starts {
        if let Err(e) = functions(data, offset, size, out) {
            problem(data, out, e);
        }
    }
    for (title, offset, size, lazy) in binding_regions {
        if let Err(error) = bind::summarize(
            data,
            offset,
            size,
            if header.is_type_64() { 8 } else { 4 },
            &libraries,
            title,
            lazy,
            out,
        ) {
            problem(data, out, error);
        }
    }
    if let Some(table) = symtab {
        if let Err(e) = symbols::<H>(
            data,
            endian,
            table,
            dysymtab,
            header.flags(endian),
            header.filetype(endian),
            &libraries,
            chains.is_some(),
            out,
        ) {
            problem(data, out, e);
        }
    }
    Ok(())
}

fn segment_details<S: Segment>(
    data: Data<'_>,
    out: &mut BinarySummary,
    segment: &S,
    endian: S::Endian,
    bytes: &[u8],
    segments: &mut Vec<String>,
    sections: &mut Vec<String>,
    count: &mut usize,
) -> Result<(), String> {
    let (offset, size) = segment.file_range(endian);
    out.item(segments,format!("{} — address 0x{:X}, memory {} bytes, file 0x{offset:X} + {size} bytes, protections 0x{:X}/0x{:X}, flags 0x{:X}",text(segment.name()),segment.vmaddr(endian).into(),segment.vmsize(endian).into(),segment.initprot(endian),segment.maxprot(endian),segment.flags(endian)));
    if size != 0 && data.range(offset, size).is_err() {
        return Err("Invalid Mach-O segment range".into());
    }
    for section in segment.sections(endian, bytes).map_err(|e| e.to_string())? {
        if !data.live(*count) {
            out.limited();
            break;
        }
        out.item(sections,format!("{} / {} — address 0x{:X}, {} bytes, file offset 0x{:X}, flags 0x{:X}, relocations {}",text(section.segment_name()),text(section.name()),section.addr(endian).into(),section.size(endian).into(),section.offset(endian),section.flags(endian),section.nreloc(endian)));
        *count += 1;
    }
    Ok(())
}

fn library(ordinal: i64, libraries: &[&[u8]]) -> String {
    match ordinal {
        0 => "self".into(),
        -1 => "main executable".into(),
        -2 => "flat lookup".into(),
        -3 => "weak lookup".into(),
        n if n > 0 => libraries
            .get(n as usize - 1)
            .map(|v| text(v))
            .unwrap_or_else(|| format!("library ordinal {n}")),
        n => format!("library ordinal {n}"),
    }
}
fn symbols<'a, H: MachHeader>(
    data: Data<'a>,
    endian: H::Endian,
    table: &m::SymtabCommand<H::Endian>,
    dynamic: Option<&m::DysymtabCommand<H::Endian>>,
    flags: u32,
    kind: u32,
    libraries: &[&[u8]],
    has_chains: bool,
    out: &mut BinarySummary,
) -> Result<(), String> {
    let total = table.nsyms.get(endian) as usize;
    let offset = table.symoff.get(endian) as u64;
    let stride = std::mem::size_of::<H::Nlist>();
    let bytes = (total as u64)
        .checked_mul(stride as u64)
        .ok_or("Invalid Mach-O symbol count")?;
    data.range(offset, bytes)
        .map_err(|_| "Invalid Mach-O symbol range")?;
    let start = table.stroff.get(endian) as u64;
    let end = start
        .checked_add(table.strsize.get(endian) as u64)
        .ok_or("Invalid Mach-O string table")?;
    data.range(start, end - start)
        .map_err(|_| "Invalid Mach-O string table")?;
    let strings = StringTable::new(data, start, end);
    if let Some(d) = dynamic {
        for (first, number) in [
            (d.iextdefsym.get(endian), d.nextdefsym.get(endian)),
            (d.iundefsym.get(endian), d.nundefsym.get(endian)),
        ] {
            if first
                .checked_add(number)
                .filter(|&n| n as usize <= total)
                .is_none()
            {
                return Err("Invalid Mach-O dynamic symbol range".into());
            }
        }
    }
    let (mut imports, mut definitions, mut aliases) = (Vec::new(), Vec::new(), Vec::new());
    let (mut scanned, mut imported, mut defined) = (0, 0, 0);
    let result: Result<(), String> = (|| {
        let mut index = 0;
        while index < total {
            if !data.live(scanned) {
                out.limited();
                break;
            }
            let number = (total - index).min(1024).min(SCAN_LIMIT - scanned);
            let block = data
                .read_slice_at::<H::Nlist>(offset + index as u64 * stride as u64, number)
                .map_err(|_| "Invalid Mach-O symbol entries")?;
            for symbol in block {
                if !data.live(scanned) {
                    out.limited();
                    break;
                }
                let symbol_index = index;
                index += 1;
                scanned += 1;
                if symbol.is_stab() {
                    continue;
                }
                let value: u64 = symbol.n_value(endian).into();
                let typ = symbol.n_type();
                let basic = typ & m::N_TYPE;
                let is_import = typ & m::N_EXT != 0 && basic == m::N_UNDF && value == 0;
                let is_external = typ & m::N_EXT != 0
                    && typ & m::N_PEXT == 0
                    && matches!(basic, m::N_SECT | m::N_ABS);
                let in_range = |start: u32, count: u32| -> bool {
                    let start = start as usize;
                    count
                        .checked_add(start as u32)
                        .map(|end| symbol_index >= start && symbol_index < end as usize)
                        .unwrap_or(false)
                };
                let dynamic_import = is_import
                    && dynamic
                        .map(|d| in_range(d.iundefsym.get(endian), d.nundefsym.get(endian)))
                        .unwrap_or(true)
                    && matches!(kind, 2 | 6 | 8 | 9);
                let dynamic_definition = is_external
                    && dynamic
                        .map(|d| in_range(d.iextdefsym.get(endian), d.nextdefsym.get(endian)))
                        .unwrap_or(true);
                if dynamic_import {
                    imported += 1;
                }
                if dynamic_definition {
                    defined += 1;
                }
                if !(dynamic_import && imports.len() < DISPLAY_LIMIT)
                    && !(dynamic_definition && definitions.len() < DISPLAY_LIMIT)
                    && !(basic == m::N_INDR && aliases.len() < DISPLAY_LIMIT)
                {
                    continue;
                }
                let name = text(symbol.name(endian, strings).map_err(|e| e.to_string())?);
                if dynamic_import && !has_chains {
                    let ordinal = (symbol.n_desc(endian) >> 8) as i64;
                    let ordinal = if ordinal == 254 && libraries.len() < 254 {
                        -2
                    } else if ordinal == 255 {
                        -1
                    } else {
                        ordinal
                    };
                    out.item(
                        &mut imports,
                        format!(
                            "{}!{name}{}",
                            if flags & m::MH_TWOLEVEL != 0 {
                                library(ordinal, libraries)
                            } else {
                                "flat lookup".into()
                            },
                            if symbol.n_desc(endian) & m::N_WEAK_REF != 0 {
                                " — weak"
                            } else {
                                ""
                            }
                        ),
                    );
                }
                if dynamic_definition {
                    out.item(
                        &mut definitions,
                        format!(
                            "{name} — {} 0x{value:X}",
                            if basic == m::N_ABS {
                                "absolute"
                            } else {
                                "address"
                            }
                        ),
                    );
                }
                if typ & m::N_EXT != 0 && typ & m::N_PEXT == 0 && basic == m::N_INDR {
                    let target =
                        u32::try_from(value).map_err(|_| "Invalid Mach-O alias string offset")?;
                    out.item(
                        &mut aliases,
                        format!(
                            "{name} -> {}",
                            text(
                                strings
                                    .get(target)
                                    .map_err(|_| "Invalid Mach-O alias string")?
                            )
                        ),
                    );
                }
            }
        }
        Ok(())
    })();
    let complete = scanned == total && result.is_ok();
    out.prop("Scanned symbols", count(scanned, complete));
    out.prop("Symbols", count(scanned, complete));
    out.prop("External defined symbols", count(defined, complete));
    out.reserved_section("External defined symbols", definitions);
    out.reserved_section("Symbol aliases", aliases);
    if !has_chains && matches!(kind, 2 | 6 | 8 | 9) {
        out.prop("Imports", count(imported, complete));
        out.prop("Import source", "Symbol table");
        out.reserved_section("Imports", imports);
    }
    if !complete {
        out.limited();
    }
    // Render ordinary rows after the loader-specific information has claimed its
    // share of the output budget. Re-reading a table reuses ReadCache blocks.
    let mut rows = Vec::new();
    let mut scanned_rows = 0;
    let rows_result: Result<(), String> = (|| {
        while scanned_rows < scanned && rows.len() < DISPLAY_LIMIT {
            if !data.live(scanned_rows) {
                out.limited();
                break;
            }
            let number = (scanned - scanned_rows).min(1024);
            let block = data
                .read_slice_at::<H::Nlist>(offset + scanned_rows as u64 * stride as u64, number)
                .map_err(|_| "Invalid Mach-O symbol entries")?;
            for symbol in block {
                if !data.live(scanned_rows) {
                    out.limited();
                    break;
                }
                scanned_rows += 1;
                if symbol.is_stab() {
                    continue;
                }
                let value: u64 = symbol.n_value(endian).into();
                if !out.item(
                    &mut rows,
                    format!(
                        "{} — value 0x{value:X}, type 0x{:02X}, section {}, description 0x{:04X}",
                        text(symbol.name(endian, strings).map_err(|e| e.to_string())?),
                        symbol.n_type(),
                        symbol.n_sect(),
                        symbol.n_desc(endian)
                    ),
                ) {
                    return Ok(());
                }
            }
        }
        Ok(())
    })();
    out.reserved_section("Symbols", rows);
    if scanned_rows < scanned {
        out.limited();
    }
    result.and(rows_result)
}

fn chained_imports(
    data: Data<'_>,
    offset: u64,
    size: u64,
    libraries: &[&[u8]],
    out: &mut BinarySummary,
) -> Result<(), String> {
    let range = data
        .range(offset, size)
        .map_err(|_| "Invalid chained fixups range")?;
    let header = range
        .read_bytes_at(0, 28)
        .map_err(|_| "Invalid chained fixups header")?;
    let u32_at = |at: usize| fixed_at(header, at).map(u32::from_le_bytes);
    let version = u32_at(0)?;
    let imports_at = u32_at(8)? as u64;
    let strings_at = u32_at(12)? as u64;
    let number = u32_at(16)? as usize;
    let format = u32_at(20)?;
    let compression = u32_at(24)?;
    out.prop("Chained fixups version", version);
    out.prop("Chained imports", number);
    out.prop("Chained import format", format);
    out.prop("Chained symbol format", compression);
    if version != 0 {
        return Err("Unsupported chained fixups version".into());
    }
    if compression != 0 {
        return Err("Compressed chained fixup symbol names are not supported".into());
    }
    let stride = match format {
        1 => 4,
        2 => 8,
        3 => 16,
        _ => return Err("Unsupported chained import format".into()),
    };
    range
        .range(
            imports_at,
            (number as u64)
                .checked_mul(stride)
                .ok_or("Invalid chained import count")?,
        )
        .map_err(|_| "Invalid chained import table")?;
    if strings_at > size {
        return Err("Invalid chained import string table".into());
    }
    let strings = StringTable::new(range, strings_at, size);
    let mut items = Vec::new();
    let mut read = 0;
    let result: Result<(), String> = (|| {
        while read < number {
            if !data.live(read) {
                out.limited();
                break;
            }
            let bytes = range
                .read_bytes_at(imports_at + read as u64 * stride, stride)
                .map_err(|_| "Invalid chained import entry")?;
            let (ordinal, weak, name, addend) = if format == 3 {
                let word = u64::from_le_bytes(fixed_at(bytes, 0)?);
                let ord = word & 65535;
                let ordinal = if ord > 0xfff0 {
                    ord as i64 - 65536
                } else {
                    ord as i64
                };
                (
                    ordinal,
                    word & (1 << 16) != 0,
                    (word >> 32) as u32,
                    i64::from_le_bytes(fixed_at(bytes, 8)?),
                )
            } else {
                let word = u32::from_le_bytes(fixed_at(bytes, 0)?);
                let ord = word & 255;
                let ordinal = if ord > 0xf0 {
                    ord as i64 - 256
                } else {
                    ord as i64
                };
                (
                    ordinal,
                    word & 256 != 0,
                    word >> 9,
                    if format == 2 {
                        i32::from_le_bytes(fixed_at(bytes, 4)?) as i64
                    } else {
                        0
                    },
                )
            };
            if read < DISPLAY_LIMIT {
                out.item(
                    &mut items,
                    format!(
                        "{}!{}{}{}",
                        library(ordinal, libraries),
                        text(
                            strings
                                .get(name)
                                .map_err(|_| "Invalid chained import name")?
                        ),
                        if weak { " — weak" } else { "" },
                        if addend != 0 {
                            format!(" — addend {addend}")
                        } else {
                            String::new()
                        }
                    ),
                );
            }
            read += 1;
        }
        Ok(())
    })();
    out.prop("Imports", count(read, read == number && result.is_ok()));
    out.prop("Import source", "Chained fixups");
    out.reserved_section("Imports", items);
    if read < number || number > DISPLAY_LIMIT {
        out.limited();
    }
    result
}
fn functions(
    data: Data<'_>,
    offset: u64,
    size: u64,
    out: &mut BinarySummary,
) -> Result<(), String> {
    let range = data
        .range(offset, size)
        .map_err(|_| "Invalid function starts range")?;
    let mut cursor = 0;
    let mut value = 0u64;
    let mut count = 0usize;
    let mut rows = Vec::new();
    let mut complete = false;
    let result: Result<(), String> = (|| {
        while cursor < size {
            if !data.live(count) {
                out.limited();
                break;
            }
            let mut delta = 0u64;
            let mut shift = 0;
            loop {
                let byte = *range
                    .read_bytes_at(cursor, 1)
                    .map_err(|_| "Invalid function starts ULEB")?
                    .first()
                    .ok_or("Truncated function starts ULEB")?;
                cursor += 1;
                if shift == 63 && byte & 0x7e != 0 {
                    return Err("Function starts ULEB overflow".into());
                }
                delta |= ((byte & 127) as u64) << shift;
                if byte & 128 == 0 {
                    break;
                }
                shift += 7;
                if shift > 63 {
                    return Err("Function starts ULEB overflow".into());
                }
            }
            if delta == 0 {
                complete = true;
                break;
            }
            value = value
                .checked_add(delta)
                .ok_or("Function starts address overflow")?;
            out.item(&mut rows, format!("image offset 0x{value:X}"));
            count += 1;
        }
        Ok(())
    })();
    out.prop(
        "Function starts",
        super::count(count, complete && result.is_ok()),
    );
    out.reserved_section("Function starts", rows);
    if !complete {
        out.limited();
    }
    result
}

fn code_signature(
    data: Data<'_>,
    offset: u64,
    size: u64,
    out: &mut BinarySummary,
) -> Result<(), String> {
    let range = data
        .range(offset, size)
        .map_err(|_| "Invalid code signature range")?;
    let head = range
        .read_bytes_at(0, 12)
        .map_err(|_| "Invalid code signature header")?;
    let be = |slice: &[u8]| fixed_at(slice, 0).map(u32::from_be_bytes);
    let magic = be(&head[..4])?;
    let length = be(&head[4..8])? as u64;
    out.prop("Signature magic", format!("0x{magic:08X}"));
    out.prop("Signature bytes", length);
    if length > size || length < 12 {
        return Err("Invalid code signature length".into());
    }
    if magic != 0xfade0cc0 {
        return Ok(());
    }
    let number = be(&head[8..12])? as usize;
    if (number as u64)
        .checked_mul(8)
        .and_then(|n| n.checked_add(12))
        .filter(|&n| n <= length)
        .is_none()
    {
        return Err("Invalid code signature blob index".into());
    }
    out.prop("Signature blobs", number);
    let mut blobs = Vec::new();
    let result: Result<(), String> = (|| {
        for index in 0..number {
            if !data.live(index) {
                out.limited();
                break;
            }
            let entry = range
                .read_bytes_at(12 + index as u64 * 8, 8)
                .map_err(|_| "Invalid code signature blob index")?;
            let kind = be(&entry[..4])?;
            let at = be(&entry[4..])? as u64;
            let head = range
                .read_bytes_at(at, 8)
                .map_err(|_| "Invalid code signature blob")?;
            let blob_magic = be(&head[..4])?;
            let bytes = be(&head[4..8])? as u64;
            if bytes < 8 || at.checked_add(bytes).filter(|&end| end <= length).is_none() {
                return Err("Invalid code signature blob length".into());
            }
            out.item(
                &mut blobs,
                format!("slot {kind} — magic 0x{blob_magic:08X}, offset 0x{at:X}, {bytes} bytes"),
            );
            if blob_magic == 0xfade0c02 {
                let cd = range
                    .range(at, bytes)
                    .map_err(|_| "Invalid CodeDirectory range")?;
                let h = cd
                    .read_bytes_at(0, 44)
                    .map_err(|_| "Invalid CodeDirectory header")?;
                let v = be(&h[8..12])?;
                out.prop("CodeDirectory version", format!("0x{v:X}"));
                out.prop("Signing flags", format!("0x{:X}", be(&h[12..16])?));
                out.prop("Code slots", be(&h[28..32])?);
                out.prop("Special slots", be(&h[24..28])?);
                out.prop("Code limit", be(&h[32..36])?);
                out.prop("Hash size", h[36]);
                out.prop("Hash type", h[37]);
                out.prop("Signing platform", h[38]);
                out.prop("Page size exponent", h[39]);
                out.prop(
                    "Signing identifier",
                    text(
                        StringTable::new(cd, 0, bytes)
                            .get(be(&h[20..24])?)
                            .map_err(|_| "Invalid signing identifier")?,
                    ),
                );
                // Metadata describes the embedded signature; it does not verify trust.
            }
        }
        Ok(())
    })();
    out.reserved_section("Signature blobs", blobs);
    result
}
