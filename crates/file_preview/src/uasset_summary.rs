//! Read-only package tables, using the version guards in PG2's PackageFileSummary/ObjectResource.
//! Does not load UObject payloads, execute Blueprint code, or follow paths stored in the package.
use super::binary::BinarySummary;
use std::{
    cell::Cell,
    collections::{BTreeSet, HashSet},
    fs::File,
    io::{BufReader, Read, Seek, SeekFrom},
    path::Path,
    rc::Rc,
    time::{Duration, Instant},
};

const READ_LIMIT: u64 = 32 * 1024 * 1024;
const BLOCK_LIMIT: usize = 8 * 1024 * 1024;
const SCAN_LIMIT: usize = 100_000;
const FILTER_EDITOR: u32 = 0x8000_0000;
const NOTE_LIMIT: &str = "The Unreal summary is limited to keep large packages responsive.";
const NOTE_DAMAGE: &str =
    "Some Unreal package structures are damaged or unavailable; the readable metadata is shown.";
const NOTE_VERSION: &str =
    "This Unreal package version is not supported; only its readable header is shown.";
const NOTE_UNVERSIONED: &str = "Unversioned Unreal packages require their exact engine schema; object tables were not guessed.";
const NOTE_FIELDS: &str = "Graph, function and legacy property objects are listed from export types. Modern inline properties and Blueprint bytecode are not decoded.";

#[cfg(test)]
#[path = "uasset_summary_tests.rs"]
mod tests;

#[derive(Clone, Copy, Debug, PartialEq)]
enum Failure {
    Damaged,
    Limited,
    Unsupported,
    Unversioned,
}
type Result<T> = std::result::Result<T, Failure>;
struct Reader<R> {
    input: R,
    length: u64,
    position: u64,
    bytes: u64,
    work: usize,
    deadline: Instant,
}
impl<R: Read + Seek> Reader<R> {
    fn new(mut input: R) -> Result<Self> {
        let length = input.seek(SeekFrom::End(0)).map_err(|_| Failure::Damaged)?;
        input
            .seek(SeekFrom::Start(0))
            .map_err(|_| Failure::Damaged)?;
        Ok(Self {
            input,
            length,
            position: 0,
            bytes: 0,
            work: 0,
            deadline: Instant::now() + Duration::from_secs(2),
        })
    }
    fn live(&mut self) -> Result<()> {
        self.work += 1;
        if self.work > SCAN_LIMIT * 16 || Instant::now() >= self.deadline {
            Err(Failure::Limited)
        } else {
            Ok(())
        }
    }
    fn seek(&mut self, at: u64) -> Result<()> {
        self.live()?;
        if at > self.length {
            return Err(Failure::Damaged);
        }
        self.input
            .seek(SeekFrom::Start(at))
            .map_err(|_| Failure::Damaged)?;
        self.position = at;
        Ok(())
    }
    fn skip(&mut self, size: u64) -> Result<()> {
        // Consume small padding/version fields without discarding BufReader's read-ahead.
        // Seeking at every field would otherwise perform a fresh 64 KiB disk read per record.
        let mut remaining = size;
        let mut buffer = [0; 4096];
        while remaining > 0 {
            let n = remaining.min(buffer.len() as u64) as usize;
            self.raw(&mut buffer[..n])?;
            remaining -= n as u64;
        }
        Ok(())
    }
    fn raw(&mut self, bytes: &mut [u8]) -> Result<()> {
        self.live()?;
        if bytes.len() > BLOCK_LIMIT || self.bytes.saturating_add(bytes.len() as u64) > READ_LIMIT {
            return Err(Failure::Limited);
        }
        let end = self
            .position
            .checked_add(bytes.len() as u64)
            .ok_or(Failure::Damaged)?;
        if end > self.length {
            return Err(Failure::Damaged);
        }
        self.bytes += bytes.len() as u64;
        self.input.read_exact(bytes).map_err(|_| Failure::Damaged)?;
        self.position = end;
        Ok(())
    }
    fn fixed<const N: usize>(&mut self) -> Result<[u8; N]> {
        let mut b = [0; N];
        self.raw(&mut b)?;
        Ok(b)
    }
    fn i32(&mut self) -> Result<i32> {
        Ok(i32::from_le_bytes(self.fixed()?))
    }
    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.fixed()?))
    }
    fn i64(&mut self) -> Result<i64> {
        Ok(i64::from_le_bytes(self.fixed()?))
    }
    fn string(&mut self, max: usize) -> Result<String> {
        let n = self.i32()?;
        let count = n.checked_abs().ok_or(Failure::Damaged)? as usize;
        if count == 0 {
            return Ok(String::new());
        }
        if count > max {
            return Err(Failure::Damaged);
        }
        let mut b = vec![0; count * if n < 0 { 2 } else { 1 }];
        self.raw(&mut b)?;
        if n < 0 {
            let mut units: Vec<_> = b
                .chunks_exact(2)
                .map(|b| u16::from_le_bytes([b[0], b[1]]))
                .collect();
            if units.pop() != Some(0) {
                return Err(Failure::Damaged);
            }
            String::from_utf16(&units).map_err(|_| Failure::Damaged)
        } else {
            if b.pop() != Some(0) {
                return Err(Failure::Damaged);
            }
            // FString ANSI storage is not UTF-8 in older packages; lossless ASCII is common,
            // and non-ASCII legacy bytes are rendered with their byte values, not dropped.
            Ok(match String::from_utf8(b) {
                Ok(s) => s,
                Err(e) => e.into_bytes().into_iter().map(char::from).collect(),
            })
        }
    }
    fn fname(&mut self) -> Result<Name> {
        let index = self.i32()?;
        let number = self.i32()?;
        if index < 0 || number < 0 {
            return Err(Failure::Damaged);
        }
        Ok(Name {
            index: index as usize,
            number: number as u32,
        })
    }
    fn opaque_string(&mut self) -> Result<u64> {
        // FiBData packs a binary Blueprint-search archive inside FString storage.
        // Its UTF-16 code units are not human text (and may contain unpaired surrogates).
        let length = self.i32()?;
        let units = length.checked_abs().ok_or(Failure::Damaged)? as u64;
        if units == 0 {
            return Ok(0);
        }
        let width = if length < 0 { 2 } else { 1 };
        let bytes = units.checked_mul(width).ok_or(Failure::Damaged)?;
        let end = self.position.checked_add(bytes).ok_or(Failure::Damaged)?;
        if end > self.length {
            return Err(Failure::Damaged);
        }
        self.seek(end - width)?;
        if width == 2 {
            if self.fixed::<2>()? != [0, 0] {
                return Err(Failure::Damaged);
            }
        } else if self.fixed::<1>()? != [0] {
            return Err(Failure::Damaged);
        }
        Ok(bytes)
    }
    fn count(&mut self, minimum: u64) -> Result<usize> {
        let n = self.i32()?;
        if n < 0
            || (n as u64)
                .checked_mul(minimum)
                .and_then(|v| self.position.checked_add(v))
                .map_or(true, |v| v > self.length)
        {
            return Err(Failure::Damaged);
        }
        Ok(n as usize)
    }
    fn table(&mut self, count: usize, offset: i32, minimum: u64) -> Result<()> {
        if count == 0 {
            return Ok(());
        }
        if offset <= 0
            || (offset as u64)
                .checked_add(
                    (count as u64)
                        .checked_mul(minimum)
                        .ok_or(Failure::Damaged)?,
                )
                .map_or(true, |end| end > self.length)
        {
            return Err(Failure::Damaged);
        }
        self.seek(offset as u64)
    }
}
#[derive(Clone, Copy)]
struct Name {
    index: usize,
    number: u32,
}
impl Name {
    fn text(self, names: &[String]) -> Result<String> {
        let s = names.get(self.index).ok_or(Failure::Damaged)?;
        Ok(if self.number == 0 {
            s.clone()
        } else {
            format!("{s}_{}", self.number - 1)
        })
    }
}
#[derive(Default)]
struct Header {
    legacy: i32,
    ue4: i32,
    ue5: i32,
    flags: u32,
    old_jb: bool,
    names: usize,
    name_offset: i32,
    imports: usize,
    import_offset: i32,
    exports: usize,
    export_offset: i32,
    depends: i32,
    soft: usize,
    soft_offset: i32,
    registry: i32,
}
#[derive(Clone)]
struct Import {
    class_package: Name,
    class_name: Name,
    outer: i32,
    name: Name,
}
#[derive(Clone)]
struct Export {
    class: i32,
    super_index: i32,
    outer: i32,
    name: Name,
    flags: u32,
    size: i64,
    offset: i64,
    asset: bool,
}
pub(crate) struct Inspection {
    pub summary: BinarySummary,
    pub is_blueprint: bool,
}

fn note(out: &mut BinarySummary, failure: Failure) {
    let message = match failure {
        Failure::Damaged => NOTE_DAMAGE,
        Failure::Limited => NOTE_LIMIT,
        Failure::Unsupported => NOTE_VERSION,
        Failure::Unversioned => NOTE_UNVERSIONED,
    };
    if out.note.as_deref() != Some(message) {
        match out.note.as_mut() {
            Some(n) => {
                if !n.contains(message) {
                    n.push('\n');
                    n.push_str(message);
                }
            }
            None => out.note = Some(message.into()),
        }
    }
}
fn header<R: Read + Seek>(r: &mut Reader<R>, out: &mut BinarySummary) -> Result<Header> {
    if r.u32()? != 0x9e2a83c1 {
        return Err(Failure::Damaged);
    }
    let mut h = Header::default();
    h.legacy = r.i32()?;
    out.prop("Package format", h.legacy);
    if !(-9..=-6).contains(&h.legacy) {
        return Err(Failure::Unsupported);
    }
    r.skip(4)?;
    h.ue4 = r.i32()?;
    h.ue5 = if h.legacy <= -8 { r.i32()? } else { 0 };
    out.prop("UE4 object version", h.ue4);
    out.prop("UE5 object version", h.ue5);
    out.prop("Licensee version", r.i32()?);
    if h.ue4 == 0 {
        return Err(Failure::Unversioned);
    }
    if !(214..=522).contains(&h.ue4) || !(0..=1017).contains(&h.ue5) {
        return Err(Failure::Unsupported);
    }
    let mut total = 0;
    if h.ue5 >= 1016 {
        let hash = r.fixed::<20>()?;
        out.prop("Package saved hash", hex(&hash));
        total = r.i32()?;
    }
    let versions = r.count(0)?;
    if versions > 4096 {
        return Err(Failure::Damaged);
    }
    let mut rows = Vec::new();
    let custom_result = (|| {
        for _ in 0..versions {
            let guid = r.fixed::<16>()?;
            let v = r.i32()?;
            out.item(&mut rows, format!("{} = {v}", guid_text(&guid)));
        }
        Ok(())
    })();
    out.reserved_section("Custom versions", rows);
    custom_result?;
    if h.ue5 < 1016 {
        total = r.i32()?;
        if total < 0 {
            h.old_jb = total != -265535;
            out.prop("JBInfo marker", total);
            total = r.i32()?;
        }
    }
    if total <= 0 || total as u64 > r.length {
        return Err(Failure::Damaged);
    }
    out.prop("Total header size", total);
    let folder = r.string(32768)?;
    if !folder.is_empty() {
        out.prop("Package folder", folder);
    }
    h.flags = r.u32()?;
    out.prop("Package flags", format!("0x{:08X}", h.flags));
    let mut flag_names = Vec::new();
    for (mask, name) in [
        (0x40, "EditorOnly"),
        (0x200, "Cooked"),
        (0x2000, "UnversionedProperties"),
        (0x20000, "ContainsMap"),
        (0x200000, "ContainsScript"),
        (FILTER_EDITOR, "FilterEditorOnly"),
    ] {
        if h.flags & mask != 0 {
            flag_names.push(name)
        }
    }
    if !flag_names.is_empty() {
        out.prop("Package flag names", flag_names.join(", "));
    }
    h.names = r.count(0)?;
    h.name_offset = r.i32()?;
    if h.ue5 >= 1008 {
        r.skip(8)?;
    }
    if h.flags & FILTER_EDITOR == 0 && h.ue4 >= 516 {
        let id = r.string(32768)?;
        if !id.is_empty() {
            out.prop("Localization ID", id);
        }
    }
    if h.ue4 >= 459 {
        r.skip(8)?;
    }
    h.exports = r.count(0)?;
    h.export_offset = r.i32()?;
    h.imports = r.count(0)?;
    h.import_offset = r.i32()?;
    if h.ue5 >= 1015 {
        r.skip(16)?;
    }
    if h.ue5 >= 1014 {
        r.skip(4)?;
    }
    h.depends = r.i32()?;
    if h.ue4 >= 384 {
        h.soft = r.count(0)?;
        h.soft_offset = r.i32()?;
    }
    if h.ue4 >= 510 {
        r.skip(4)?;
    }
    let thumbnail = r.i32()?;
    out.prop("Thumbnail table offset", thumbnail);
    out.prop("Names", h.names);
    out.prop("Imports", h.imports);
    out.prop("Exports", h.exports);
    out.prop("Soft package references", h.soft);
    // These table counts are declarations, not claims that every record is readable.
    match tail(r, &h, out) {
        Ok(registry) => h.registry = registry,
        Err(Failure::Unsupported) => return Err(Failure::Unsupported),
        Err(e) => note(out, e),
    }
    Ok(h)
}
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
fn guid_text(bytes: &[u8; 16]) -> String {
    bytes
        .chunks_exact(4)
        .map(|b| format!("{:08X}", u32::from_le_bytes([b[0], b[1], b[2], b[3]])))
        .collect::<Vec<_>>()
        .join("-")
}
fn engine<R: Read + Seek>(r: &mut Reader<R>) -> Result<String> {
    let major = u16::from_le_bytes(r.fixed()?);
    let minor = u16::from_le_bytes(r.fixed()?);
    let patch = u16::from_le_bytes(r.fixed()?);
    let cl = r.u32()?;
    let branch = r.string(32768)?;
    Ok(format!(
        "{major}.{minor}.{patch} · CL {}{}",
        cl & 0x7fff_ffff,
        if branch.is_empty() {
            String::new()
        } else {
            format!(" · {branch}")
        }
    ))
}
fn tail<R: Read + Seek>(r: &mut Reader<R>, h: &Header, out: &mut BinarySummary) -> Result<i32> {
    if h.ue5 < 1016 {
        out.prop("Package GUID", guid_text(&r.fixed()?));
    }
    if h.flags & FILTER_EDITOR == 0 && h.ue4 >= 518 && !h.old_jb {
        r.skip(16)?;
        if h.ue4 < 520 {
            r.skip(16)?;
        }
    }
    let generations = r.count(8)?;
    if generations > 4096 {
        return Err(Failure::Damaged);
    };
    r.skip(generations as u64 * 8)?;
    if h.ue4 >= 336 {
        out.prop("Saved by engine", engine(r)?);
    } else {
        out.prop("Engine changelist", r.i32()?);
    }
    if h.ue4 >= 444 {
        out.prop("Compatible engine", engine(r)?);
    }
    let compression = r.u32()?;
    out.prop("Compression flags", format!("0x{compression:08X}"));
    if r.count(16)? != 0 {
        return Err(Failure::Unsupported);
    }
    r.skip(4)?;
    let additional = r.count(4)?;
    if additional > 4096 {
        return Err(Failure::Damaged);
    }
    for _ in 0..additional {
        r.string(32768)?;
    }
    if h.legacy > -7 && r.i32()? != 0 {
        return Err(Failure::Unsupported);
    }
    let registry = r.i32()?;
    out.prop("Asset registry offset", registry);
    out.prop("Bulk data offset", r.i64()?);
    Ok(registry)
}
fn names<R: Read + Seek>(r: &mut Reader<R>, h: &Header) -> (Vec<String>, Option<Failure>) {
    let mut rows = Vec::new();
    let mut decoded = 0usize;
    let result = (|| {
        r.table(h.names, h.name_offset, 4)?;
        for _ in 0..h.names {
            if rows.len() >= SCAN_LIMIT {
                return Err(Failure::Limited);
            }
            let name = r.string(1024)?;
            decoded = decoded.saturating_add(name.len());
            if decoded > 16 * 1024 * 1024 {
                return Err(Failure::Limited);
            }
            if h.ue4 >= 504 {
                r.skip(4)?;
            }
            rows.push(name);
        }
        Ok(())
    })();
    (rows, result.err())
}
fn imports<R: Read + Seek>(r: &mut Reader<R>, h: &Header) -> (Vec<Import>, Option<Failure>) {
    let mut rows = Vec::new();
    let result = (|| {
        r.table(h.imports, h.import_offset, 28)?;
        for _ in 0..h.imports {
            if rows.len() >= SCAN_LIMIT {
                return Err(Failure::Limited);
            }
            let row = Import {
                class_package: r.fname()?,
                class_name: r.fname()?,
                outer: r.i32()?,
                name: r.fname()?,
            };
            if h.ue4 >= 520 && h.flags & FILTER_EDITOR == 0 {
                r.fname()?;
            }
            if h.ue5 >= 1003 {
                r.skip(4)?;
            }
            rows.push(row);
        }
        Ok(())
    })();
    (rows, result.err())
}
fn exports<R: Read + Seek>(r: &mut Reader<R>, h: &Header) -> (Vec<Export>, Option<Failure>) {
    let mut rows = Vec::new();
    let result = (|| {
        r.table(h.exports, h.export_offset, 44)?;
        for _ in 0..h.exports {
            if rows.len() >= SCAN_LIMIT {
                return Err(Failure::Limited);
            }
            let class = r.i32()?;
            let super_index = r.i32()?;
            if h.ue4 >= 508 {
                r.skip(4)?;
            }
            let outer = r.i32()?;
            let name = r.fname()?;
            let flags = r.u32()?;
            let size = if h.ue4 >= 511 {
                r.i64()?
            } else {
                r.i32()? as i64
            };
            let offset = if h.ue4 >= 511 {
                r.i64()?
            } else {
                r.i32()? as i64
            };
            if size < 0 || offset < 0 {
                return Err(Failure::Damaged);
            }
            r.skip(12)?;
            if h.ue5 < 1005 {
                r.skip(16)?;
            }
            if h.ue5 >= 1006 {
                r.skip(4)?;
            }
            r.skip(4)?;
            if h.ue4 >= 365 {
                r.skip(4)?;
            }
            let asset = if h.ue4 >= 485 { r.i32()? != 0 } else { false };
            if h.ue5 >= 1003 {
                r.skip(4)?;
            }
            if h.ue4 >= 507 {
                r.skip(20)?;
            }
            if h.ue5 >= 1010 && h.flags & 0x2000 == 0 {
                r.skip(16)?;
            }
            rows.push(Export {
                class,
                super_index,
                outer,
                name,
                flags,
                size,
                offset,
                asset,
            });
        }
        Ok(())
    })();
    (rows, result.err())
}
struct Objects<'a> {
    names: &'a [String],
    imports: &'a [Import],
    exports: &'a [Export],
}
impl Objects<'_> {
    fn object(&self, index: i32) -> Result<(Name, i32)> {
        if index == 0 {
            return Err(Failure::Damaged);
        }
        if index > 0 {
            let v = self
                .exports
                .get(index as usize - 1)
                .ok_or(Failure::Damaged)?;
            Ok((v.name, v.outer))
        } else {
            let i = index.checked_neg().ok_or(Failure::Damaged)? as usize;
            let v = self.imports.get(i - 1).ok_or(Failure::Damaged)?;
            Ok((v.name, v.outer))
        }
    }
    fn path(&self, mut index: i32) -> Result<String> {
        if index == 0 {
            return Ok("None".into());
        }
        let mut seen = HashSet::new();
        let mut parts = Vec::new();
        let mut bytes = 0;
        while index != 0 {
            if parts.len() >= 32 || !seen.insert(index) {
                return Err(Failure::Damaged);
            }
            let (name, outer) = self.object(index)?;
            let text = name.text(self.names)?;
            bytes += text.len();
            if bytes > 4096 {
                return Err(Failure::Damaged);
            }
            parts.push(text);
            index = outer;
        }
        parts.reverse();
        Ok(parts.join("."))
    }
    fn class(&self, e: &Export) -> Result<String> {
        if e.class == 0 {
            Ok("/Script/CoreUObject.Class".into())
        } else {
            self.path(e.class)
        }
    }
    fn belongs_to_blueprint(&self, mut index: i32) -> bool {
        let mut seen = HashSet::new();
        for _ in 0..32 {
            if index <= 0 || !seen.insert(index) {
                return false;
            }
            let Some(e) = self.exports.get(index as usize - 1) else {
                return false;
            };
            if self.class(e).ok().is_some_and(|v| blueprint(&v)) {
                return true;
            }
            index = e.outer;
        }
        false
    }
}
fn short(class: &str) -> &str {
    class.rsplit(['.', '/']).next().unwrap_or(class)
}
fn blueprint(class: &str) -> bool {
    matches!(
        short(class),
        "Blueprint"
            | "BlueprintGeneratedClass"
            | "WidgetBlueprint"
            | "WidgetBlueprintGeneratedClass"
            | "AnimBlueprint"
            | "AnimBlueprintGeneratedClass"
            | "LevelScriptBlueprint"
            | "LevelScriptBlueprintGeneratedClass"
    )
}
fn registry<R: Read + Seek>(
    r: &mut Reader<R>,
    h: &Header,
    out: &mut BinarySummary,
    details: bool,
    is_blueprint: &mut bool,
) -> Result<()> {
    if h.registry == 0 {
        return Ok(());
    }
    r.table(1, h.registry, 4)?;
    if h.ue4 >= 521 && h.flags & FILTER_EDITOR == 0 {
        r.i64()?;
    }
    let count = r.count(12)?;
    let mut assets = Vec::new();
    let mut tags = Vec::new();
    let mut work = 0usize;
    let result = (|| {
        for _ in 0..count {
            if work >= SCAN_LIMIT {
                return Err(Failure::Limited);
            }
            work += 1;
            let object = r.string(32768)?;
            let class = r.string(32768)?;
            let tag_count = r.count(8)?;
            if blueprint(&class) {
                *is_blueprint = true;
            }
            if details {
                out.item(&mut assets, format!("{object} · {class}"));
            }
            if blueprint(&class) {
                out.prop("Asset class", &class);
            }
            for _ in 0..tag_count {
                if work >= SCAN_LIMIT {
                    return Err(Failure::Limited);
                }
                work += 1;
                let key = r.string(32768)?;
                let value = if matches!(key.as_str(), "FiBData" | "FiB" | "UnversionedFiBData") {
                    let bytes = r.opaque_string()?;
                    format!("{bytes} bytes (opaque Blueprint search metadata)")
                } else {
                    let start = r.position;
                    let size = r.i32()?.checked_abs().ok_or(Failure::Damaged)? as usize;
                    r.seek(start)?;
                    if size > BLOCK_LIMIT / 2 {
                        let bytes = r.opaque_string()?;
                        note(out, Failure::Limited);
                        format!("{bytes} bytes (value omitted by read limit)")
                    } else {
                        r.string(BLOCK_LIMIT / 2)?
                    }
                };
                if blueprint(&class)
                    && matches!(
                        key.as_str(),
                        "ParentClass"
                            | "NativeParentClass"
                            | "GeneratedClass"
                            | "BlueprintType"
                            | "IsDataOnly"
                    )
                {
                    out.prop(&key, &value);
                }
                if details {
                    out.item(&mut tags, format!("{object}: {key} = {value}"));
                }
            }
            if !details && *is_blueprint {
                break;
            }
        }
        Ok(())
    })();
    out.reserved_section("Asset registry assets", assets);
    out.reserved_section("Asset registry tags", tags);
    result
}

fn inspect_reader<R: Read + Seek>(
    input: R,
    details: bool,
) -> std::result::Result<Inspection, String> {
    let mut r = Reader::new(input).map_err(|_| "Unable to read Unreal package.".to_string())?;
    let mut out = BinarySummary::default();
    out.prop("File size", r.length);
    let h = match header(&mut r, &mut out) {
        Ok(h) => h,
        Err(e) => {
            if out.props.len() == 1 {
                return Err("Not a readable Unreal package.".into());
            }
            note(&mut out, e);
            return Ok(Inspection {
                summary: out,
                is_blueprint: false,
            });
        }
    };
    let (names, name_error) = names(&mut r, &h);
    if let Some(e) = name_error {
        note(&mut out, e);
    }
    let (imports, import_error) = imports(&mut r, &h);
    if let Some(e) = import_error {
        note(&mut out, e);
    }
    let (exports, export_error) = exports(&mut r, &h);
    if let Some(e) = export_error {
        note(&mut out, e);
    }
    let objects = Objects {
        names: &names,
        imports: &imports,
        exports: &exports,
    };
    let mut is_blueprint = false;
    let mut classes = BTreeSet::new();
    let mut parents = BTreeSet::new();
    for e in &exports {
        if let Err(error) = r.live() {
            note(&mut out, error);
            break;
        }
        if let Ok(class) = objects.class(e) {
            if blueprint(&class) {
                is_blueprint = true;
                if classes.len() < 32 {
                    classes.insert(class.clone());
                }
            }
            if short(&class).ends_with("BlueprintGeneratedClass") && e.super_index != 0 {
                if let Ok(parent) = objects.path(e.super_index) {
                    if parents.len() < 32 {
                        parents.insert(parent);
                    }
                }
            }
            if e.asset && classes.len() < 32 {
                classes.insert(class);
            }
        }
    }
    if !classes.is_empty() {
        out.prop(
            "Asset classes",
            classes.into_iter().take(32).collect::<Vec<_>>().join(", "),
        );
    }
    if !parents.is_empty() {
        out.prop(
            "Generated class parents",
            parents.into_iter().take(32).collect::<Vec<_>>().join(", "),
        );
    }
    if let Err(e) = registry(&mut r, &h, &mut out, details, &mut is_blueprint) {
        note(&mut out, e);
    }
    if details {
        let mut graphs = Vec::new();
        let mut functions = Vec::new();
        let mut properties = Vec::new();
        let mut export_rows = Vec::new();
        let mut import_rows = Vec::new();
        let mut dependencies = Vec::new();
        let mut dep_seen = BTreeSet::new();
        // Proven class/ownership metadata precedes the more verbose table dump.
        for (i, e) in exports.iter().enumerate() {
            if let Err(error) = r.live() {
                note(&mut out, error);
                break;
            }
            let result = (|| {
                let class = objects.class(e)?;
                let name = objects.path(i as i32 + 1)?;
                let row = format!("{name} · {class}");
                if objects.belongs_to_blueprint(e.outer) {
                    match short(&class) {
                        "EdGraph" => {
                            out.item(&mut graphs, row);
                        }
                        "Function" => {
                            out.item(&mut functions, row);
                        }
                        "BoolProperty"
                        | "ByteProperty"
                        | "IntProperty"
                        | "Int64Property"
                        | "FloatProperty"
                        | "DoubleProperty"
                        | "NameProperty"
                        | "StrProperty"
                        | "TextProperty"
                        | "ObjectProperty"
                        | "ClassProperty"
                        | "StructProperty"
                        | "ArrayProperty"
                        | "MapProperty"
                        | "SetProperty"
                        | "EnumProperty"
                        | "DelegateProperty"
                        | "MulticastDelegateProperty" => {
                            out.item(&mut properties, row);
                        }
                        _ => {}
                    }
                }
                Ok(())
            })();
            if let Err(e) = result {
                note(&mut out, e);
            }
        }
        out.reserved_section("Graph objects", graphs);
        out.reserved_section("Function objects", functions);
        out.reserved_section("Legacy property objects", properties);
        for (i, import) in imports.iter().enumerate() {
            if let Err(e) = r.live() {
                note(&mut out, e);
                break;
            }
            let result = (|| {
                let name = objects.path(-(i as i32) - 1)?;
                let class = import.class_name.text(&names)?;
                let package = import.class_package.text(&names)?;
                if class == "Package"
                    && dep_seen.len() < super::binary::DISPLAY_LIMIT
                    && dep_seen.insert(name.clone())
                {
                    out.item(&mut dependencies, name.clone());
                }
                out.item(
                    &mut import_rows,
                    format!(
                        "#{} {name} · {package}.{class} · outer {}",
                        i + 1,
                        import.outer
                    ),
                );
                Ok(())
            })();
            if let Err(e) = result {
                note(&mut out, e);
            }
        }
        out.reserved_section("Package dependencies", dependencies);
        out.reserved_section("Imported objects", import_rows);
        for (i, e) in exports.iter().enumerate() {
            if let Err(error) = r.live() {
                note(&mut out, error);
                break;
            }
            let result = (|| {
                let class = objects.class(e)?;
                let name = objects.path(i as i32 + 1)?;
                out.item(&mut export_rows,format!("#{} {name} · {class} · flags 0x{:08X} · {} bytes @ {} · outer {} · super {}",i+1,e.flags,e.size,e.offset,e.outer,e.super_index));
                Ok(())
            })();
            if let Err(e) = result {
                note(&mut out, e);
            }
        }
        out.reserved_section("Exported objects", export_rows);
        if let Err(e) = soft_dependencies(&mut r, &h, &names, &mut out) {
            note(&mut out, e);
        }
        if let Err(e) = export_dependencies(&mut r, &h, &objects, &mut out) {
            note(&mut out, e);
        }
        let mut rows = Vec::new();
        for (i, name) in names.iter().enumerate() {
            if !out.item(&mut rows, format!("#{i} {name}")) {
                break;
            }
        }
        out.reserved_section("Name map", rows);
        if is_blueprint {
            match out.note.as_mut() {
                Some(n) => {
                    n.push('\n');
                    n.push_str(NOTE_FIELDS);
                }
                None => out.note = Some(NOTE_FIELDS.into()),
            }
        }
    }
    // Translate the shared binary output limit into a format-specific message.
    if let Some(n) = out.note.as_mut() {
        *n = n.replace(
            "The summary is limited to keep large binaries responsive.",
            NOTE_LIMIT,
        );
    }
    Ok(Inspection {
        summary: out,
        is_blueprint,
    })
}
fn soft_dependencies<R: Read + Seek>(
    r: &mut Reader<R>,
    h: &Header,
    names: &[String],
    out: &mut BinarySummary,
) -> Result<()> {
    let mut rows = Vec::new();
    let result = (|| {
        r.table(h.soft, h.soft_offset, 4)?;
        for _ in 0..h.soft {
            if rows.len() >= SCAN_LIMIT {
                return Err(Failure::Limited);
            }
            let name = if h.ue4 >= 514 {
                r.fname()?.text(names)?
            } else {
                r.string(32768)?
            };
            if !out.item(&mut rows, name) {
                break;
            }
        }
        Ok(())
    })();
    out.reserved_section("Soft package dependencies", rows);
    result
}
fn export_dependencies<R: Read + Seek>(
    r: &mut Reader<R>,
    h: &Header,
    objects: &Objects<'_>,
    out: &mut BinarySummary,
) -> Result<()> {
    if h.depends == 0 {
        return Ok(());
    }
    let mut rows = Vec::new();
    let mut work = 0;
    let result = (|| {
        r.table(h.exports, h.depends, 4)?;
        for index in 0..h.exports {
            if work >= SCAN_LIMIT {
                return Err(Failure::Limited);
            }
            let count = r.count(4)?;
            work += 1;
            for _ in 0..count {
                if work >= SCAN_LIMIT {
                    return Err(Failure::Limited);
                }
                work += 1;
                let dependency = r.i32()?;
                let target = objects.path(dependency)?;
                if !out.item(&mut rows, format!("Export #{} → {target}", index + 1)) {
                    return Ok(());
                }
            }
        }
        Ok(())
    })();
    out.reserved_section("Export dependencies", rows);
    result
}
struct DiskBudget {
    file: File,
    bytes: u64,
    deadline: Instant,
    limited: Rc<Cell<bool>>,
}
impl Read for DiskBudget {
    fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
        if output.is_empty() {
            return Ok(0);
        }
        if self.bytes >= READ_LIMIT || Instant::now() >= self.deadline {
            self.limited.set(true);
            return Err(std::io::Error::other("Unreal read budget exhausted"));
        }
        let length = output
            .len()
            .min(BLOCK_LIMIT)
            .min((READ_LIMIT - self.bytes) as usize);
        let n = self.file.read(&mut output[..length])?;
        self.bytes += n as u64;
        Ok(n)
    }
}
impl Seek for DiskBudget {
    fn seek(&mut self, position: SeekFrom) -> std::io::Result<u64> {
        self.file.seek(position)
    }
}
pub(crate) fn inspect(path: &Path, details: bool) -> std::result::Result<Inspection, String> {
    let file = File::open(path).map_err(|e| e.to_string())?;
    if !file.metadata().map_err(|e| e.to_string())?.is_file() {
        return Err("The selected file is not a regular Unreal package.".into());
    }
    let limited = Rc::new(Cell::new(false));
    let input = DiskBudget {
        file,
        bytes: 0,
        deadline: Instant::now() + Duration::from_secs(2),
        limited: limited.clone(),
    };
    let mut result = inspect_reader(BufReader::with_capacity(64 * 1024, input), details)?;
    if limited.get() {
        note(&mut result.summary, Failure::Limited);
    }
    Ok(result)
}
