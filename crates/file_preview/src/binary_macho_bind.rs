//! Bounded classic dyld bind streams. Weak binding and lazy binding stay distinct.
use super::super::{DISPLAY_LIMIT, NOTE_LIMITED, count};
use super::{BinarySummary, Data, library, text};
use object::read::ReadRef;

struct Cursor<'a> {
    data: Data<'a>,
    at: u64,
    window: Option<(u64, &'a [u8])>,
    work: usize,
}
impl<'a> Cursor<'a> {
    fn byte(&mut self) -> Result<u8, String> {
        if !self.data.live(0) || self.work >= 4 * 1024 * 1024 {
            return Err(NOTE_LIMITED.into());
        }
        if self.at >= self.data.size {
            return Err("Truncated Mach-O bind opcode stream".into());
        }
        let start = self.at / 4096 * 4096;
        if self.window.map(|(at, _)| at) != Some(start) {
            self.window = Some((
                start,
                self.data
                    .read_bytes_at(start, (self.data.size - start).min(4096))
                    .map_err(|_| "Invalid Mach-O bind opcode range")?,
            ));
        }
        let value = *self
            .window
            .and_then(|(_, bytes)| bytes.get((self.at - start) as usize))
            .ok_or("Truncated Mach-O bind opcode window")?;
        self.at += 1;
        self.work += 1;
        Ok(value)
    }
    fn uleb(&mut self) -> Result<u64, String> {
        let mut v = 0;
        for shift in (0..=63).step_by(7) {
            let b = self.byte()?;
            if shift == 63 && b & 0x7e != 0 {
                return Err("Mach-O bind ULEB overflow".into());
            }
            v |= ((b & 127) as u64) << shift;
            if b & 128 == 0 {
                return Ok(v);
            }
        }
        Err("Mach-O bind ULEB overflow".into())
    }
    fn sleb(&mut self) -> Result<i64, String> {
        let mut v = 0u64;
        for shift in (0..=63).step_by(7) {
            let b = self.byte()?;
            if shift == 63 && b & 127 != 0 && b & 127 != 127 {
                return Err("Mach-O bind SLEB overflow".into());
            }
            v |= ((b & 127) as u64) << shift;
            if b & 128 == 0 {
                let used = shift + 7;
                if used < 64 && b & 64 != 0 {
                    v |= u64::MAX << used;
                }
                return Ok(v as i64);
            }
        }
        Err("Mach-O bind SLEB overflow".into())
    }
    fn name(&mut self) -> Result<Vec<u8>, String> {
        let mut bytes = Vec::new();
        loop {
            let b = self.byte()?;
            if b == 0 {
                return Ok(bytes);
            }
            if bytes.len() == 4096 {
                return Err("Mach-O bind symbol name exceeds preview limits".into());
            }
            bytes.push(b);
        }
    }
}
#[derive(Default)]
struct State {
    ordinal: i64,
    ordinal_set: bool,
    name: Vec<u8>,
    flags: u8,
    kind: u8,
    addend: i64,
    segment: Option<u8>,
    offset: u64,
}
fn emit(
    data: Data<'_>,
    out: &mut BinarySummary,
    rows: &mut Vec<String>,
    total: &mut usize,
    state: &State,
    libraries: &[&[u8]],
) -> Result<(), String> {
    if !data.live(*total) {
        return Err(NOTE_LIMITED.into());
    }
    if state.name.is_empty() || state.segment.is_none() || !state.ordinal_set {
        return Err("Incomplete Mach-O bind target".into());
    }
    let segment = state.segment.ok_or("Incomplete Mach-O bind target")?;
    if state.ordinal < -3 || state.ordinal > libraries.len() as i64 {
        return Err("Invalid Mach-O binding library ordinal".into());
    }
    if !matches!(state.kind, 1..=3) {
        return Err("Unsupported Mach-O binding type".into());
    }
    if *total < DISPLAY_LIMIT {
        out.item(
            rows,
            format!(
                "{}!{} — segment {}, offset 0x{:X}, type {}{}{}",
                library(state.ordinal, libraries),
                text(&state.name),
                segment,
                state.offset,
                state.kind,
                if state.flags & 1 != 0 {
                    " — weak import"
                } else if state.flags & 8 != 0 {
                    " — non-weak definition"
                } else {
                    ""
                },
                if state.addend != 0 {
                    format!(" — addend {}", state.addend)
                } else {
                    String::new()
                }
            ),
        );
    }
    *total += 1;
    Ok(())
}
pub(super) fn summarize(
    data: Data<'_>,
    offset: u64,
    size: u64,
    pointer_size: u64,
    libraries: &[&[u8]],
    title: &str,
    lazy: bool,
    out: &mut BinarySummary,
) -> Result<(), String> {
    let range = data
        .range(offset, size)
        .map_err(|_| "Invalid Mach-O binding range")?;
    let mut cursor = Cursor {
        data: range,
        at: 0,
        window: None,
        work: 0,
    };
    let weak = title == "Weak bindings";
    let mut state = State {
        kind: 1,
        ordinal: if weak { -3 } else { 0 },
        ordinal_set: weak,
        ..Default::default()
    };
    let mut rows = Vec::new();
    let mut total = 0;
    let mut operations = 0;
    let mut complete = size == 0;
    let result: Result<(), String> = (|| {
        while cursor.at < size {
            if !data.live(operations) {
                return Err(NOTE_LIMITED.into());
            }
            operations += 1;
            let byte = cursor.byte()?;
            let op = byte & 0xf0;
            let imm = byte & 15;
            match op {
                0 => {
                    if lazy {
                        state = State {
                            kind: 1,
                            ..Default::default()
                        };
                        complete = cursor.at == size;
                        continue;
                    }
                    complete = true;
                    break;
                }
                0x10 => {
                    if weak {
                        return Err("Invalid weak binding library opcode".into());
                    }
                    state.ordinal = imm as i64;
                    state.ordinal_set = true;
                }
                0x20 => {
                    if weak {
                        return Err("Invalid weak binding library opcode".into());
                    }
                    state.ordinal = i64::try_from(cursor.uleb()?)
                        .map_err(|_| "Mach-O bind ordinal overflow")?;
                    state.ordinal_set = true;
                }
                0x30 => {
                    if weak {
                        return Err("Invalid weak binding library opcode".into());
                    }
                    state.ordinal = if imm == 0 {
                        0
                    } else {
                        (imm | 0xf0) as i8 as i64
                    };
                    state.ordinal_set = true;
                }
                0x40 => {
                    state.flags = imm;
                    state.name = cursor.name()?;
                }
                0x50 => state.kind = imm,
                0x60 => state.addend = cursor.sleb()?,
                0x70 => {
                    state.segment = Some(imm);
                    state.offset = cursor.uleb()?;
                }
                0x80 => {
                    state.offset = state
                        .offset
                        .checked_add(cursor.uleb()?)
                        .ok_or("Mach-O binding address overflow")?
                }
                0x90 => {
                    emit(data, out, &mut rows, &mut total, &state, libraries)?;
                    state.offset = state
                        .offset
                        .checked_add(pointer_size)
                        .ok_or("Mach-O binding address overflow")?;
                }
                0xa0 => {
                    emit(data, out, &mut rows, &mut total, &state, libraries)?;
                    let skip = cursor.uleb()?;
                    state.offset = state
                        .offset
                        .checked_add(pointer_size)
                        .and_then(|v| v.checked_add(skip))
                        .ok_or("Invalid Mach-O binding address")?;
                }
                0xb0 => {
                    emit(data, out, &mut rows, &mut total, &state, libraries)?;
                    state.offset = state
                        .offset
                        .checked_add((imm as u64 + 1) * pointer_size)
                        .ok_or("Mach-O binding address overflow")?;
                }
                0xc0 => {
                    let repeat = cursor.uleb()?;
                    let skip = cursor
                        .uleb()?
                        .checked_add(pointer_size)
                        .ok_or("Mach-O binding skip overflow")?;
                    for _ in 0..repeat {
                        emit(data, out, &mut rows, &mut total, &state, libraries)?;
                        state.offset = state
                            .offset
                            .checked_add(skip)
                            .ok_or("Mach-O binding address overflow")?;
                    }
                }
                0xd0 => return Err("Threaded Mach-O bind opcodes are not supported".into()),
                _ => return Err("Unknown Mach-O binding opcode".into()),
            }
            if lazy {
                complete = false;
            }
        }
        Ok(())
    })();
    let result = match result {
        Err(e) if e == NOTE_LIMITED => {
            out.limited();
            Ok(())
        }
        Ok(()) if !complete => Err("Unterminated Mach-O bind opcode stream".into()),
        r => r,
    };
    out.prop(title, count(total, complete && result.is_ok()));
    out.reserved_section(title, rows);
    if !complete || total > DISPLAY_LIMIT {
        out.limited();
    }
    result
}
