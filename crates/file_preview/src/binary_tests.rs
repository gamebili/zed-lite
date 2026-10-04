use super::*;
use anyhow::Result;
use std::{fs, io::Write as _};

const MSF7: &[u8; 32] = b"Microsoft C/C++ MSF 7.00\r\n\x1aDS\0\0\0";

fn property<'a>(summary: &'a BinarySummary, label: &str) -> Option<&'a str> {
    summary
        .props
        .iter()
        .find(|property| property.label == label)
        .map(|property| property.value.as_str())
}

fn entries<'a>(summary: &'a BinarySummary, title: &str) -> &'a [String] {
    summary
        .sections
        .iter()
        .find(|section| section.title == title)
        .map(|section| section.items.as_slice())
        .unwrap_or(&[])
}

fn inspect(path: &Path) -> Result<BinarySummary> {
    summarize(path).map_err(anyhow::Error::msg)
}

fn put16(bytes: &mut [u8], offset: usize, value: u16) {
    bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}
fn put32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}
fn put64(bytes: &mut [u8], offset: usize, value: u64) {
    bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}
fn pe() -> Vec<u8> {
    let mut bytes = vec![0; 0x600];
    bytes[..2].copy_from_slice(b"MZ");
    put32(&mut bytes, 0x3c, 0x80);
    bytes[0x80..0x84].copy_from_slice(b"PE\0\0");
    put16(&mut bytes, 0x84, 0x8664);
    put16(&mut bytes, 0x86, 1);
    put32(&mut bytes, 0x88, 0x65000000);
    put16(&mut bytes, 0x94, 0xf0);
    put16(&mut bytes, 0x96, 0x2022);
    let opt = 0x98;
    put16(&mut bytes, opt, 0x20b);
    put32(&mut bytes, opt + 16, 0x1000);
    put64(&mut bytes, opt + 24, 0x140000000);
    put32(&mut bytes, opt + 32, 0x1000);
    put32(&mut bytes, opt + 36, 0x200);
    put32(&mut bytes, opt + 56, 0x2000);
    put32(&mut bytes, opt + 60, 0x200);
    put16(&mut bytes, opt + 68, 3);
    put32(&mut bytes, opt + 108, 16);
    for (index, rva, size) in [(0, 0x1120, 0x80), (1, 0x1000, 40), (6, 0x1100, 28)] {
        put32(&mut bytes, opt + 112 + index * 8, rva);
        put32(&mut bytes, opt + 116 + index * 8, size);
    }
    let sec = 0x188;
    bytes[sec..sec + 6].copy_from_slice(b".rdata");
    put32(&mut bytes, sec + 8, 0x400);
    put32(&mut bytes, sec + 12, 0x1000);
    put32(&mut bytes, sec + 16, 0x400);
    put32(&mut bytes, sec + 20, 0x200);
    put32(&mut bytes, sec + 36, 0x40000040);
    put32(&mut bytes, 0x200, 0x1060);
    put32(&mut bytes, 0x20c, 0x1040);
    put32(&mut bytes, 0x210, 0x1060);
    bytes[0x240..0x24d].copy_from_slice(b"KERNEL32.dll\0");
    put64(&mut bytes, 0x260, 0x1080);
    put64(&mut bytes, 0x268, (1 << 63) | 7);
    bytes[0x282..0x288].copy_from_slice(b"Hello\0");
    put32(&mut bytes, 0x30c, 2);
    put32(&mut bytes, 0x310, 42);
    put32(&mut bytes, 0x314, 0x1180);
    put32(&mut bytes, 0x318, 0x380);
    put32(&mut bytes, 0x330, 1);
    put32(&mut bytes, 0x334, 2);
    put32(&mut bytes, 0x338, 1);
    put32(&mut bytes, 0x33c, 0x1150);
    put32(&mut bytes, 0x340, 0x1158);
    put32(&mut bytes, 0x344, 0x115c);
    put32(&mut bytes, 0x350, 0x1000);
    put32(&mut bytes, 0x354, 0x1170);
    put32(&mut bytes, 0x358, 0x1160);
    bytes[0x360..0x368].copy_from_slice(b"DoThing\0");
    bytes[0x370..0x37b].copy_from_slice(b"OTHER.Func\0");
    bytes[0x380..0x384].copy_from_slice(b"RSDS");
    for i in 0..16 {
        bytes[0x384 + i] = i as u8 + 1;
    }
    put32(&mut bytes, 0x394, 3);
    bytes[0x398..0x3aa].copy_from_slice(b"C:\\build\\demo.pdb\0");
    bytes
}
fn record(kind: u16, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&((body.len() + 2) as u16).to_le_bytes());
    out.extend_from_slice(&kind.to_le_bytes());
    out.extend_from_slice(body);
    out
}
fn module(name: &str, object: &str) -> Vec<u8> {
    let mut out = vec![0; 64];
    put16(&mut out, 34, u16::MAX);
    put16(&mut out, 48, 1);
    out.extend_from_slice(name.as_bytes());
    out.push(0);
    out.extend_from_slice(object.as_bytes());
    out.push(0);
    out.resize(out.len().next_multiple_of(4), 0);
    out
}

/// Minimal native streams with real CodeView records. Referenced source paths deliberately
/// do not exist: their presence in the result must never cause filesystem access.
fn native_streams() -> Vec<Vec<u8>> {
    let mut info = vec![0; 32];
    put32(&mut info, 0, 20000404);
    put32(&mut info, 4, 0x12345678);
    put32(&mut info, 8, 7);
    info[12..28].copy_from_slice(&[
        0x78, 0x56, 0x34, 0x12, 0xbc, 0x9a, 0xf0, 0xde, 1, 2, 3, 4, 5, 6, 7, 8,
    ]);
    let mut class = vec![0; 18];
    put16(&mut class, 16, 4);
    class.extend_from_slice(b"Widget\0");
    let class = record(0x1505, &class);
    let mut types = vec![0; 56];
    put32(&mut types, 0, 20040203);
    put32(&mut types, 4, 56);
    put32(&mut types, 8, 0x1000);
    put32(&mut types, 12, 0x1001);
    put32(&mut types, 16, class.len() as u32);
    put16(&mut types, 20, u16::MAX);
    put16(&mut types, 22, u16::MAX);
    types.extend(class);
    let mut modules = module("first", "first.obj");
    modules.extend(module("second", "second.obj"));
    let first = b"Q:\\nonexistent-preview-test\\first.cpp\0";
    let second = b"\\\\nonexistent-p4j-server\\Private\\second.cpp\0";
    let mut sources = vec![0; 20];
    put16(&mut sources, 0, 2);
    put16(&mut sources, 2, 2);
    put16(&mut sources, 6, 1);
    put16(&mut sources, 8, 1);
    put16(&mut sources, 10, 1);
    put32(&mut sources, 16, first.len() as u32);
    sources.extend_from_slice(first);
    sources.extend_from_slice(second);
    let mut dbi = vec![0; 64];
    put32(&mut dbi, 0, u32::MAX);
    put32(&mut dbi, 4, 19990903);
    put32(&mut dbi, 8, 7);
    put16(&mut dbi, 12, u16::MAX);
    put16(&mut dbi, 16, u16::MAX);
    put16(&mut dbi, 20, 5);
    put32(&mut dbi, 24, modules.len() as u32);
    put32(&mut dbi, 36, sources.len() as u32);
    put32(&mut dbi, 48, 22);
    put16(&mut dbi, 58, 0x8664);
    dbi.extend(modules);
    dbi.extend(sources);
    let mut extra = vec![0xff; 22];
    put16(&mut extra, 10, 6);
    dbi.extend(extra);
    let mut public = vec![0; 10];
    put32(&mut public, 0, 3);
    put32(&mut public, 4, 0x20);
    put16(&mut public, 8, 1);
    public.extend_from_slice(b"demo_function\0");
    let mut symbols = record(0x110e, &public);
    let mut udt = 0x1000u32.to_le_bytes().to_vec();
    udt.extend_from_slice(b"Widget\0");
    symbols.extend(record(0x1108, &udt));
    let mut section = vec![0; 40];
    section[..5].copy_from_slice(b".text");
    put32(&mut section, 8, 0x1000);
    put32(&mut section, 12, 0x1000);
    put32(&mut section, 16, 512);
    put32(&mut section, 36, 0x60000020);
    vec![vec![], info, types, dbi, vec![], symbols, section, vec![]]
}

fn msf(streams: &[Vec<u8>]) -> Vec<u8> {
    let block = 512usize;
    let directory_size = 4
        + streams.len() * 4
        + streams
            .iter()
            .map(|s| s.len().div_ceil(block) * 4)
            .sum::<usize>();
    let directory_pages = directory_size.div_ceil(block);
    let index_pages = (directory_pages * 4).div_ceil(block);
    let stream_start = 3 + index_pages + directory_pages;
    let count = (stream_start
        + streams
            .iter()
            .map(|s| s.len().div_ceil(block))
            .sum::<usize>())
    .max(8);
    let mut out = vec![0; count * block];
    out[..32].copy_from_slice(MSF7);
    put32(&mut out, 32, block as u32);
    put32(&mut out, 36, 1);
    put32(&mut out, 40, count as u32);
    put32(&mut out, 44, directory_size as u32);
    for n in 0..index_pages {
        put32(&mut out, 52 + n * 4, (3 + n) as u32);
    }
    let mut directory = vec![0; directory_size];
    put32(&mut directory, 0, streams.len() as u32);
    let mut cursor = 4 + streams.len() * 4;
    let mut page = stream_start;
    for (n, stream) in streams.iter().enumerate() {
        put32(&mut directory, 4 + n * 4, stream.len() as u32);
        for chunk in stream.chunks(block) {
            put32(&mut directory, cursor, page as u32);
            cursor += 4;
            out[page * block..page * block + chunk.len()].copy_from_slice(chunk);
            page += 1;
        }
    }
    for n in 0..directory_pages {
        put32(&mut out, 3 * block + n * 4, (3 + index_pages + n) as u32);
    }
    for (n, chunk) in directory.chunks(block).enumerate() {
        let start = (3 + index_pages + n) * block;
        out[start..start + chunk.len()].copy_from_slice(chunk);
    }
    out
}

#[test]
fn pe_preview_shows_imports_ordinals_exports_and_pdb_references_without_writing() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("sample.dll");
    let bytes = pe();
    fs::write(&path, &bytes)?;
    let summary = inspect(&path)?;
    assert_eq!(property(&summary, "Format"), Some("PE"));
    assert_eq!(property(&summary, "Architecture"), Some("X86_64"));
    assert_eq!(property(&summary, "Imports"), Some("2"));
    assert_eq!(property(&summary, "Exports"), Some("2"));
    assert!(
        entries(&summary, "Imports")
            .iter()
            .any(|value| value == "KERNEL32.dll!#7")
    );
    assert!(
        entries(&summary, "Exports")
            .iter()
            .any(|value| value.contains("OTHER!Func"))
    );
    assert_eq!(property(&summary, "PDB file"), Some("C:\\build\\demo.pdb"));
    assert_eq!(property(&summary, "PDB age"), Some("3"));
    assert_eq!(fs::read(&path)?, bytes);
    assert_eq!(fs::read_dir(directory.path())?.count(), 1);
    Ok(())
}

#[test]
fn native_pdb_preview_reads_streams_and_keeps_referenced_paths_as_data() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("fixture.pdb");
    let bytes = msf(&native_streams());
    fs::write(&path, &bytes)?;
    let summary = inspect(&path)?;
    assert_eq!(property(&summary, "Format"), Some("PDB 7.0"));
    assert_eq!(property(&summary, "Architecture"), Some("x86_64"));
    assert_eq!(
        property(&summary, "GUID"),
        Some("12345678-9abc-def0-0102-030405060708")
    );
    assert_eq!(property(&summary, "Modules"), Some("2"));
    assert_eq!(property(&summary, "Source files"), Some("2"));
    assert_eq!(property(&summary, "Types"), Some("1"));
    assert_eq!(property(&summary, "Symbols"), Some("2"));
    assert!(
        entries(&summary, "Symbols")
            .iter()
            .any(|value| value.contains("demo_function"))
    );
    assert!(
        entries(&summary, "Source files")
            .iter()
            .any(|value| value.contains("nonexistent-preview-test"))
    );
    assert_eq!(fs::read(&path)?, bytes);
    assert_eq!(fs::read_dir(directory.path())?.count(), 1);
    Ok(())
}

fn macho(command_size: u32) -> Vec<u8> {
    let mut bytes = vec![0; 40];
    put32(&mut bytes, 0, 0xfeedfacf);
    put32(&mut bytes, 4, 0x0100000c);
    put32(&mut bytes, 12, 6);
    put32(&mut bytes, 16, 1);
    put32(&mut bytes, 20, 8);
    put32(
        &mut bytes,
        24,
        object::macho::MH_TWOLEVEL | object::macho::MH_DYLDLINK,
    );
    put32(&mut bytes, 32, object::macho::LC_UUID);
    put32(&mut bytes, 36, command_size);
    bytes
}

#[test]
fn malformed_pe_macho_and_pdb_report_damage_without_panics_or_writes() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let mut broken_pe = pe();
    put32(&mut broken_pe, 0x3c, u32::MAX);
    let broken_macho = macho(u32::MAX);
    let mut broken_pdb = msf(&native_streams());
    put32(&mut broken_pdb, 44, u32::MAX);
    for (name, bytes) in [
        ("broken.exe", broken_pe),
        ("broken.dylib", broken_macho),
        ("broken.pdb", broken_pdb),
    ] {
        let path = directory.path().join(name);
        fs::write(&path, &bytes)?;
        let result = inspect(&path);
        assert!(
            result.is_err() || result.as_ref().is_ok_and(|summary| summary.note.is_some()),
            "Damage must remain visible for {name}"
        );
        assert_eq!(fs::read(&path)?, bytes);
    }
    Ok(())
}

#[test]
fn huge_sparse_command_tables_and_cache_requests_stop_before_allocation() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("huge.dylib");
    let mut bytes = macho(8);
    put32(&mut bytes, 20, u32::try_from(BLOCK_LIMIT + 1)?);
    let mut file = File::create(&path)?;
    file.write_all(&bytes)?;
    file.set_len(8 * 1024 * 1024 * 1024)?;
    drop(file);
    let stamp = fs::metadata(&path)?.modified()?;
    let summary = inspect(&path)?;
    assert_eq!(property(&summary, "Architecture"), Some("Aarch64"));
    assert_eq!(summary.note.as_deref(), Some(NOTE_LIMITED));
    let budget = Budget::new(&path).map_err(anyhow::Error::msg)?;
    assert!(budget.data().read_bytes_at(0, BLOCK_LIMIT + 1).is_err());
    assert_eq!(budget.bytes.get(), 0);
    assert!(budget.data().read_bytes_at(u64::MAX, 100).is_err());
    assert!(budget.data().range(1, u64::MAX).is_err());
    assert_eq!(fs::metadata(&path)?.modified()?, stamp);
    assert_eq!(fs::metadata(&path)?.len(), 8 * 1024 * 1024 * 1024);
    Ok(())
}

#[test]
fn pdb_directory_counts_are_checked_before_allocating() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let original = msf(&native_streams());
    for (position, value) in [
        (32, 513),
        (40, u32::MAX),
        (44, u32::MAX),
        (52, u32::MAX),
        (3 * 512, u32::MAX),
        (4 * 512, u32::MAX),
    ] {
        let path = directory.path().join(format!("broken-{position}.pdb"));
        let mut bytes = original.clone();
        put32(&mut bytes, position, value);
        fs::write(&path, &bytes)?;
        assert!(
            inspect(&path).is_err(),
            "Invalid PDB count at {position} must fail before allocation"
        );
        assert_eq!(fs::read(&path)?, bytes);
    }
    Ok(())
}
