//! Static export-trie inspection. Offsets are reported, never resolved or executed.
use super::super::{BinarySummary, DISPLAY_LIMIT, Data, SCAN_LIMIT, count, text};
use object::{macho, read::ReadRef};
use std::collections::BTreeSet;

const WINDOW: u64 = 4096;
const BYTE_WORK_LIMIT: usize = 4 * 1024 * 1024;
const DEPTH_LIMIT: usize = 256;
const PATH_LIMIT: usize = 4096;

#[derive(Clone, Copy, Debug)]
enum ParseError {
    Damaged(&'static str),
    Limited,
}
impl ParseError {
    fn message(self) -> &'static str {
        match self {
            Self::Damaged(message) => message,
            Self::Limited => "The export trie exceeded the inspection budget.",
        }
    }
}
struct Cursor {
    position: u64,
    end: u64,
}
struct Reader<'a> {
    data: Data<'a>,
    steps: usize,
    window_start: u64,
    window: &'a [u8],
}
impl<'a> Reader<'a> {
    fn new(data: Data<'a>) -> Self {
        Self {
            data,
            steps: 0,
            window_start: 0,
            window: &[],
        }
    }
    fn byte(&mut self, cursor: &mut Cursor) -> Result<u8, ParseError> {
        if self.steps >= BYTE_WORK_LIMIT || !self.data.live(0) {
            return Err(ParseError::Limited);
        }
        self.steps += 1;
        if cursor.position >= cursor.end {
            return Err(ParseError::Damaged("Truncated export-trie entry."));
        }
        let relative = cursor.position.checked_sub(self.window_start);
        if relative.map_or(true, |index| index >= self.window.len() as u64) {
            self.window_start = cursor.position / WINDOW * WINDOW;
            let size = (self.data.size - self.window_start).min(WINDOW);
            self.window = self
                .data
                .read_bytes_at(self.window_start, size)
                .map_err(|_| {
                    if self.data.budget.limited.get() {
                        ParseError::Limited
                    } else {
                        ParseError::Damaged("The export-trie bytes could not be read.")
                    }
                })?;
        }
        let byte = *self
            .window
            .get((cursor.position - self.window_start) as usize)
            .ok_or(ParseError::Damaged("Truncated export-trie window."))?;
        cursor.position += 1;
        Ok(byte)
    }
    fn uleb(&mut self, cursor: &mut Cursor) -> Result<u64, ParseError> {
        let mut value = 0u64;
        for index in 0..10 {
            let byte = self.byte(cursor)?;
            // A u64 has only one remaining payload bit in the tenth byte.
            if index == 9 && (byte & 0xfe) != 0 {
                return Err(ParseError::Damaged(
                    "An export-trie ULEB128 value overflowed.",
                ));
            }
            value |= u64::from(byte & 0x7f) << (index * 7);
            if byte & 0x80 == 0 {
                return Ok(value);
            }
        }
        Err(ParseError::Damaged(
            "An export-trie ULEB128 value overflowed.",
        ))
    }
    fn string(&mut self, cursor: &mut Cursor) -> Result<Vec<u8>, ParseError> {
        let mut bytes = Vec::new();
        loop {
            let byte = self.byte(cursor)?;
            if byte == 0 {
                return Ok(bytes);
            }
            if bytes.len() == PATH_LIMIT {
                return Err(ParseError::Limited);
            }
            bytes.push(byte);
        }
    }
}

struct Frame {
    offset: u64,
    path_len: usize,
    children_position: u64,
    remaining: u8,
    edges: BTreeSet<Vec<u8>>,
}
struct Inspection<'a, 'b> {
    reader: Reader<'a>,
    libraries: &'b [&'b [u8]],
    rows: Vec<String>,
    exports: usize,
    nodes: usize,
    incomplete: bool,
    first_error: Option<ParseError>,
}
impl Inspection<'_, '_> {
    fn record(&mut self, error: ParseError, summary: &mut BinarySummary) {
        self.incomplete = true;
        if matches!(error, ParseError::Limited) {
            summary.limited();
        }
        self.first_error.get_or_insert(error);
    }
    fn terminal(&mut self, cursor: &mut Cursor, path: &[u8]) -> Result<String, ParseError> {
        if path.is_empty() {
            return Err(ParseError::Damaged(
                "An export-trie symbol has an empty name.",
            ));
        }
        let flags = self.reader.uleb(cursor)?;
        let mut description = if flags & u64::from(macho::EXPORT_SYMBOL_FLAGS_REEXPORT) != 0 {
            if flags
                & u64::from(
                    macho::EXPORT_SYMBOL_FLAGS_KIND_MASK
                        | macho::EXPORT_SYMBOL_FLAGS_STUB_AND_RESOLVER,
                )
                != 0
            {
                return Err(ParseError::Damaged(
                    "An export-trie re-export has invalid kind or resolver flags.",
                ));
            }
            let ordinal = self.reader.uleb(cursor)?;
            if ordinal == 0 || ordinal > self.libraries.len() as u64 {
                return Err(ParseError::Damaged(
                    "An export-trie re-export library ordinal is out of range.",
                ));
            }
            let alias = self.reader.string(cursor)?;
            format!(
                "re-export from {}!{}",
                text(self.libraries[ordinal as usize - 1]),
                text(if alias.is_empty() { path } else { &alias })
            )
        } else {
            let value = self.reader.uleb(cursor)?;
            let kind = flags & u64::from(macho::EXPORT_SYMBOL_FLAGS_KIND_MASK);
            if flags & u64::from(macho::EXPORT_SYMBOL_FLAGS_STUB_AND_RESOLVER) != 0 {
                if kind != u64::from(macho::EXPORT_SYMBOL_FLAGS_KIND_REGULAR) {
                    return Err(ParseError::Damaged(
                        "An export-trie resolver has an invalid symbol kind.",
                    ));
                }
                let resolver = self.reader.uleb(cursor)?;
                format!("stub image offset 0x{value:X}, resolver image offset 0x{resolver:X}")
            } else {
                match kind as u32 {
                    macho::EXPORT_SYMBOL_FLAGS_KIND_REGULAR => format!("image offset 0x{value:X}"),
                    macho::EXPORT_SYMBOL_FLAGS_KIND_THREAD_LOCAL => {
                        format!("thread-local image offset 0x{value:X}")
                    }
                    macho::EXPORT_SYMBOL_FLAGS_KIND_ABSOLUTE => format!("absolute 0x{value:X}"),
                    _ => {
                        return Err(ParseError::Damaged(
                            "An export-trie symbol kind is invalid.",
                        ));
                    }
                }
            }
        };
        if cursor.position != cursor.end {
            return Err(ParseError::Damaged(
                "An export-trie terminal has unexpected trailing bytes.",
            ));
        }
        if flags & u64::from(macho::EXPORT_SYMBOL_FLAGS_WEAK_DEFINITION) != 0 {
            description.push_str(", weak definition");
        }
        // Preserve unfamiliar future flags without interpreting them as addresses.
        let known = u64::from(
            macho::EXPORT_SYMBOL_FLAGS_KIND_MASK
                | macho::EXPORT_SYMBOL_FLAGS_WEAK_DEFINITION
                | macho::EXPORT_SYMBOL_FLAGS_REEXPORT
                | macho::EXPORT_SYMBOL_FLAGS_STUB_AND_RESOLVER,
        );
        if flags & !known != 0 {
            description.push_str(&format!(", flags 0x{flags:X}"));
        }
        Ok(format!("{} — {description}", text(path)))
    }
    fn node(
        &mut self,
        offset: u64,
        path: &[u8],
        summary: &mut BinarySummary,
    ) -> Result<Frame, ParseError> {
        if self.nodes >= SCAN_LIMIT || !self.reader.data.live(0) {
            return Err(ParseError::Limited);
        }
        self.nodes += 1;
        if offset >= self.reader.data.size {
            return Err(ParseError::Damaged(
                "An export-trie child offset is out of range.",
            ));
        }
        let mut cursor = Cursor {
            position: offset,
            end: self.reader.data.size,
        };
        let terminal_size = self.reader.uleb(&mut cursor)?;
        let terminal_end = cursor
            .position
            .checked_add(terminal_size)
            .filter(|end| *end <= cursor.end)
            .ok_or(ParseError::Damaged(
                "An export-trie terminal exceeds its data range.",
            ))?;
        if terminal_size != 0 {
            let mut terminal = Cursor {
                position: cursor.position,
                end: terminal_end,
            };
            match self.terminal(&mut terminal, path) {
                Ok(row) => {
                    self.exports += 1;
                    if self.exports <= DISPLAY_LIMIT {
                        summary.item(&mut self.rows, row);
                    } else {
                        summary.limited();
                    }
                }
                Err(error) => self.record(error, summary),
            }
        }
        cursor.position = terminal_end;
        let remaining = self.reader.byte(&mut cursor)?;
        Ok(Frame {
            offset,
            path_len: path.len(),
            children_position: cursor.position,
            remaining,
            edges: BTreeSet::new(),
        })
    }
    fn scan(&mut self, summary: &mut BinarySummary) {
        if self.reader.data.size == 0 {
            return;
        }
        let root = match self.node(0, &[], summary) {
            Ok(root) => root,
            Err(error) => {
                self.record(error, summary);
                return;
            }
        };
        let mut stack = vec![root];
        // One symbol path, restored on ascent; frames never copy entire prefixes.
        let mut path = Vec::new();
        while let Some(frame) = stack.last_mut() {
            if self.reader.steps >= BYTE_WORK_LIMIT || !self.reader.data.live(0) {
                self.record(ParseError::Limited, summary);
                break;
            }
            if frame.remaining == 0 {
                stack.pop();
                path.truncate(stack.last().map_or(0, |frame| frame.path_len));
                continue;
            }
            frame.remaining -= 1;
            let mut cursor = Cursor {
                position: frame.children_position,
                end: self.reader.data.size,
            };
            let child = self
                .reader
                .string(&mut cursor)
                .and_then(|edge| self.reader.uleb(&mut cursor).map(|offset| (edge, offset)));
            let (edge, offset) = match child {
                Ok(child) => child,
                Err(error) => {
                    self.record(error, summary);
                    // The next sibling's position is no longer known. Other ancestors
                    // can still be inspected, and their previously read rows survive.
                    stack.pop();
                    path.truncate(stack.last().map_or(0, |frame| frame.path_len));
                    continue;
                }
            };
            frame.children_position = cursor.position;
            if edge.is_empty() || !frame.edges.insert(edge.clone()) {
                self.record(
                    ParseError::Damaged("An export-trie edge is empty or duplicated."),
                    summary,
                );
                continue;
            }
            let prefix_len = frame.path_len;
            if prefix_len.saturating_add(edge.len()) > PATH_LIMIT || stack.len() >= DEPTH_LIMIT {
                self.record(ParseError::Limited, summary);
                continue;
            }
            // A shared node is valid under different prefixes. Only the current
            // ancestor chain is a cycle; a global visited set would lose exports.
            if stack.iter().any(|frame| frame.offset == offset) {
                self.record(
                    ParseError::Damaged("The export trie contains a cycle."),
                    summary,
                );
                continue;
            }
            path.truncate(prefix_len);
            path.extend_from_slice(&edge);
            match self.node(offset, &path, summary) {
                Ok(frame) => stack.push(frame),
                Err(error) => {
                    self.record(error, summary);
                    path.truncate(prefix_len);
                }
            }
        }
    }
}

pub(super) fn exports(
    data: Data<'_>,
    offset: u64,
    size: u64,
    libraries: &[&[u8]],
    summary: &mut BinarySummary,
) -> Result<(), String> {
    // Reserve the small count before rows can consume the shared report budget.
    let prop_index = summary.props.len();
    summary.prop("Exports", format!("≥ {SCAN_LIMIT}"));
    let count_reserved = summary.props.len() > prop_index;
    let mut result = Inspection {
        reader: Reader::new(data),
        libraries,
        rows: Vec::new(),
        exports: 0,
        nodes: 0,
        incomplete: false,
        first_error: None,
    };
    match data.range(offset, size) {
        Ok(trie) => {
            result.reader = Reader::new(trie);
            result.scan(summary);
        }
        Err(_) => result.record(
            ParseError::Damaged("The export-trie range is outside the binary."),
            summary,
        ),
    }
    if count_reserved {
        let value = count(result.exports, !result.incomplete);
        let prop = summary
            .props
            .get_mut(prop_index)
            .ok_or("Reserved export property is unavailable")?;
        summary.report_bytes = summary
            .report_bytes
            .saturating_sub(prop.value.len())
            .saturating_add(value.len());
        prop.value = value;
    }
    summary.reserved_section("Exports", result.rows);
    match result.first_error {
        Some(error) => Err(error.message().into()),
        None => Ok(()),
    }
}
