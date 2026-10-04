use super::*;
use std::io::Write;
use zip::{ZipWriter, write::SimpleFileOptions};

fn zip_document(path: &Path, parts: &[(&str, &str)]) -> Result<()> {
    let mut writer = ZipWriter::new(File::create(path)?);
    for (name, text) in parts {
        writer.start_file(*name, SimpleFileOptions::default())?;
        writer.write_all(text.as_bytes())?;
    }
    writer.finish()?;
    Ok(())
}

#[test]
fn spreadsheet_pages_cells_formulas_and_shared_strings_without_source_changes() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("book.xlsx");
    let mut sheet = String::from("<worksheet><sheetData><row r=\"1\">");
    for column in 0..203 {
        sheet.push_str(&format!(
            "<c r=\"A{}\" t=\"s\"><f>SUM(A1:A2)</f><v>0</v></c>",
            column + 1
        ));
    }
    sheet.push_str("</row></sheetData></worksheet>");
    zip_document(
        &path,
        &[
            (
                "xl/workbook.xml",
                "<workbook xmlns:r=\"relations\"><sheets><sheet name=\"Data\" r:id=\"r1\"/></sheets></workbook>",
            ),
            (
                "xl/_rels/workbook.xml.rels",
                "<Relationships><Relationship Id=\"r1\" Target=\"worksheets/sheet1.xml\"/></Relationships>",
            ),
            (
                "xl/sharedStrings.xml",
                "<sst><si><t>中 &amp; 文</t></si></sst>",
            ),
            ("xl/worksheets/sheet1.xml", &sheet),
        ],
    )?;
    let before = fs::read(&path)?;
    let first = read(&path, &PreviewRequest::default())?;
    assert_eq!(first.sections, ["Sheet: Data"]);
    assert_eq!(first.rows.len(), PAGE_ROWS);
    assert_eq!(
        first.rows.first().context("first cell")?,
        &["A1", "s", "中 & 文", "SUM(A1:A2)"]
    );
    assert_eq!(first.next_offset, Some(200));
    let second = read(
        &path,
        &PreviewRequest {
            section: Some("Sheet: Data".into()),
            offset: 200,
        },
    )?;
    assert_eq!(second.rows.len(), 3);
    assert_eq!(
        second
            .rows
            .first()
            .and_then(|row| row.first())
            .context("next cell")?,
        "A201"
    );
    assert_eq!(second.next_offset, None);
    assert_eq!(fs::read(&path)?, before);
    Ok(())
}

#[test]
fn word_body_notes_and_entities_are_content() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("text.docx");
    zip_document(
        &path,
        &[
            (
                "word/document.xml",
                "<w:document xmlns:w=\"word\"><w:body><w:p><w:r><w:t>A &lt; B</w:t><w:tab/><w:t>中文</w:t></w:r></w:p><w:tbl><w:tr><w:tc><w:p><w:r><w:t>Table cell</w:t></w:r></w:p></w:tc></w:tr></w:tbl></w:body></w:document>",
            ),
            (
                "word/footnotes.xml",
                "<w:footnotes xmlns:w=\"word\"><w:footnote><w:p><w:r><w:t>Actual footnote</w:t></w:r></w:p></w:footnote></w:footnotes>",
            ),
        ],
    )?;
    let body = read(&path, &PreviewRequest::default())?;
    assert_eq!(
        body.rows.first().context("paragraph")?,
        &["1", "A < B\t中文"]
    );
    assert_eq!(
        body.rows.get(1).context("table paragraph")?,
        &["2", "Table cell"]
    );
    let notes = read(
        &path,
        &PreviewRequest {
            section: Some("Footnotes".into()),
            offset: 0,
        },
    )?;
    assert_eq!(
        notes.rows.first().context("footnote")?,
        &["1", "Actual footnote"]
    );
    Ok(())
}

#[test]
fn slides_and_speaker_notes_are_selectable() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("slides.pptx");
    zip_document(
        &path,
        &[
            ("ppt/presentation.xml", "<presentation/>"),
            (
                "ppt/slides/slide2.xml",
                "<slide><p><t>Second slide</t></p></slide>",
            ),
            (
                "ppt/slides/slide1.xml",
                "<slide><p><t>First slide</t></p></slide>",
            ),
            (
                "ppt/notesSlides/notesSlide1.xml",
                "<notes><p><t>Speaker notes</t></p></notes>",
            ),
        ],
    )?;
    let page = read(&path, &PreviewRequest::default())?;
    assert_eq!(page.sections, ["Slide 1", "Notes 1", "Slide 2"]);
    assert_eq!(
        page.rows.first().context("slide paragraph")?,
        &["1", "First slide"]
    );
    let notes = read(
        &path,
        &PreviewRequest {
            section: Some("Notes 1".into()),
            offset: 0,
        },
    )?;
    assert_eq!(
        notes.rows.first().context("notes paragraph")?,
        &["1", "Speaker notes"]
    );
    Ok(())
}

#[test]
fn ods_cells_include_empty_element_values_and_compact_repetition() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("data.ods");
    zip_document(
        &path,
        &[(
            "content.xml",
            "<document xmlns:table=\"table\" xmlns:office=\"office\" xmlns:text=\"text\"><table:table table:name=\"Data\"><table:table-row table:number-rows-repeated=\"1000000\"><table:table-cell table:number-columns-repeated=\"1000000\"/><table:table-cell office:value-type=\"float\" office:value=\"42\" table:formula=\"of:=SUM(A1:A2)\"/><table:table-cell office:value-type=\"string\"><text:p>中文 &amp; value</text:p></table:table-cell></table:table-row></table:table></document>",
        )],
    )?;
    let page = read(&path, &PreviewRequest::default())?;
    assert_eq!(page.rows.len(), 2);
    assert_eq!(
        page.rows.first().context("ODS numeric cell")?,
        &[
            "1",
            "1000001",
            "float",
            "42",
            "of:=SUM(A1:A2)",
            "1",
            "1000000"
        ]
    );
    assert_eq!(
        page.rows
            .get(1)
            .and_then(|row| row.get(3))
            .context("ODS string cell")?,
        "中文 & value"
    );
    Ok(())
}

#[test]
fn oversized_zip_directory_is_rejected_before_parsing_sparse_source() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("huge.xlsx");
    let mut file = File::create(&path)?;
    let length = 128 * 1024 * 1024 * 1024u64;
    file.set_len(length)?;
    file.seek(SeekFrom::Start(length - 22))?;
    let mut end = [0u8; 22];
    end[..4].copy_from_slice(b"PK\x05\x06");
    end[12..16].copy_from_slice(&((DIRECTORY_BYTES + 1) as u32).to_le_bytes());
    file.write_all(&end)?;
    let error = zip_preflight(&mut File::open(&path)?)
        .err()
        .context("expected directory limit")?;
    assert!(error.to_string().contains("8 MiB"));
    assert_eq!(fs::metadata(&path)?.len(), length);
    Ok(())
}

#[test]
fn zip64_entry_counts_are_checked_before_allocation() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("zip64.xlsx");
    let mut file = File::create(&path)?;
    let mut record = [0u8; 56];
    record[..4].copy_from_slice(b"PK\x06\x06");
    record[4..12].copy_from_slice(&44u64.to_le_bytes());
    record[24..32].copy_from_slice(&(ARCHIVE_ENTRIES + 1).to_le_bytes());
    record[32..40].copy_from_slice(&(ARCHIVE_ENTRIES + 1).to_le_bytes());
    file.write_all(&record)?;
    let mut locator = [0u8; 20];
    locator[..4].copy_from_slice(b"PK\x06\x07");
    locator[16..20].copy_from_slice(&1u32.to_le_bytes());
    file.write_all(&locator)?;
    let mut end = [0u8; 22];
    end[..4].copy_from_slice(b"PK\x05\x06");
    end[8..12].fill(0xff);
    file.write_all(&end)?;
    let error = zip_preflight(&mut File::open(&path)?)
        .err()
        .context("expected entry limit")?;
    assert!(error.to_string().contains("4096"));
    Ok(())
}

#[test]
fn truncated_xml_and_excessive_nesting_fail_visibly() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("bad.docx");
    zip_document(
        &path,
        &[("word/document.xml", "<document><p><t>unfinished")],
    )?;
    let error = read(&path, &PreviewRequest::default())
        .err()
        .context("expected malformed XML")?;
    assert!(error.to_string().contains("truncated"));
    let nested = format!("{}{}", "<node>".repeat(129), "</node>".repeat(129));
    zip_document(&path, &[("word/document.xml", &nested)])?;
    let error = read(&path, &PreviewRequest::default())
        .err()
        .context("expected XML depth limit")?;
    assert!(error.to_string().contains("nesting"));
    Ok(())
}

#[test]
fn a_single_huge_xml_event_cannot_exceed_the_read_budget() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("large.docx");
    let mut writer = ZipWriter::new(File::create(&path)?);
    writer.start_file("word/document.xml", SimpleFileOptions::default())?;
    writer.write_all(b"<document><p><t>")?;
    let chunk = [b'x'; 64 * 1024];
    for _ in 0..257 {
        writer.write_all(&chunk)?;
    }
    writer.write_all(b"</t></p></document>")?;
    writer.finish()?;
    let error = read(&path, &PreviewRequest::default())
        .err()
        .context("expected XML byte limit")?;
    assert!(format!("{error:#}").contains("bounded read limit"));
    Ok(())
}

#[test]
fn legacy_compound_streams_show_paged_actual_data_and_preserve_source() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("legacy.xls");
    {
        let mut compound = cfb::create(&path)?;
        let mut stream = compound.create_stream("/Workbook")?;
        for _ in 0..400 {
            stream.write_all(b"Actual spreadsheet record bytes!")?;
        }
    }
    let before = fs::read(&path)?;
    let page = read(&path, &PreviewRequest::default())?;
    assert!(!page.is_hex);
    assert_eq!(page.sections, ["/Workbook"]);
    assert_eq!(page.rows.len(), PAGE_ROWS);
    assert_eq!(page.next_offset, Some((PAGE_ROWS * 32) as u64));
    assert!(
        page.rows
            .first()
            .and_then(|row| row.get(2))
            .context("stream text")?
            .contains("Actual spreadsheet")
    );
    let next = read(
        &path,
        &PreviewRequest {
            section: Some("/Workbook".into()),
            offset: page.next_offset.context("next offset")?,
        },
    )?;
    assert_eq!(
        next.rows
            .first()
            .and_then(|row| row.first())
            .context("paged offset")?,
        "6400"
    );
    assert_eq!(fs::read(&path)?, before);
    Ok(())
}

#[test]
fn huge_legacy_source_opens_with_bounded_pages() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("huge.xls");
    let mut file = File::create(&path)?;
    file.write_all(OLE_SIGNATURE)?;
    let size = 128 * 1024 * 1024 * 1024u64;
    file.set_len(size)?;
    let page = crate::read(
        &path,
        &PreviewRequest {
            section: Some("Sheet: Missing".into()),
            offset: 200,
        },
    )?;
    assert!(page.is_hex);
    assert_eq!(page.title, "Hex");
    assert!(page.sections.is_empty());
    assert_eq!(page.rows.len(), PAGE_ROWS);
    assert_eq!(page.next_offset, Some((PAGE_ROWS * 16) as u64));
    assert_eq!(
        page.rows.first().and_then(|row| row.first()),
        Some(&"0000000000000000".into())
    );
    assert!(
        page.metadata
            .contains(&("File bytes".into(), size.to_string()))
    );
    assert!(
        page.note
            .context("structural limit note")?
            .contains("structural parsing limit")
    );
    let next = crate::read_bytes(
        &path,
        &PreviewRequest {
            offset: page.next_offset.context("Hex next offset")?,
            ..Default::default()
        },
    )?;
    assert!(next.is_hex);
    assert_eq!(
        next.rows.first().and_then(|row| row.first()),
        Some(&"0000000000000C80".into())
    );
    assert_eq!(file.metadata()?.len(), size);
    Ok(())
}

#[test]
fn unrecognized_office_variants_open_as_file_hex_instead_of_pseudo_text() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let source = vec![0xf7; PAGE_ROWS * 16 + 19];
    for extension in ["docx", "docm", "wps"] {
        let path = directory.path().join(format!("unknown.{extension}"));
        fs::write(&path, &source)?;
        let page = crate::read(
            &path,
            &PreviewRequest {
                section: Some("Unavailable section".into()),
                offset: 200,
            },
        )?;
        assert!(page.is_hex);
        assert_eq!(page.title, "Hex");
        assert!(page.sections.is_empty());
        assert_eq!(page.columns, ["Offset", "Hexadecimal", "ASCII"]);
        assert_eq!(page.rows.len(), PAGE_ROWS);
        assert_eq!(page.next_offset, Some((PAGE_ROWS * 16) as u64));
        assert_eq!(
            page.rows.first().and_then(|row| row.first()),
            Some(&"0000000000000000".into())
        );
        assert_eq!(
            page.rows.first().and_then(|row| row.get(1)),
            Some(&"F7 ".repeat(16))
        );
        assert!(
            page.note
                .context("unrecognized Office format note")?
                .contains("no recognized XML or compound container")
        );
        assert!(
            page.metadata
                .contains(&("File bytes".into(), source.len().to_string()))
        );
        assert_eq!(fs::read(&path)?, source);
    }
    Ok(())
}

#[test]
fn recognized_legacy_word_and_wps_streams_keep_content_sections() -> Result<()> {
    let directory = tempfile::tempdir()?;
    for extension in ["doc", "wps"] {
        let path = directory.path().join(format!("legacy.{extension}"));
        {
            let mut compound = cfb::create(&path)?;
            compound
                .create_stream("/WordDocument")?
                .write_all(b"Actual Word or WPS document text!")?;
        }
        let before = fs::read(&path)?;
        let page = read(&path, &PreviewRequest::default())?;
        assert!(!page.is_hex);
        assert_eq!(page.sections, ["/WordDocument"]);
        assert!(
            page.rows
                .first()
                .and_then(|row| row.get(2))
                .context("legacy document text")?
                .contains("Actual Word or WPS")
        );
        assert_eq!(fs::read(&path)?, before);
    }
    let path = directory.path().join("legacy.wps");
    zip_document(&path, &[("Document/body.dat", "Actual WPS stream text")])?;
    let before = fs::read(&path)?;
    let page = crate::read(&path, &PreviewRequest::default())?;
    assert!(!page.is_hex);
    assert_eq!(page.sections, ["Document/body.dat"]);
    assert!(
        page.rows
            .first()
            .and_then(|row| row.get(2))
            .context("WPS archive document text")?
            .contains("Actual WPS stream text")
    );
    assert_eq!(fs::read(&path)?, before);
    Ok(())
}

#[test]
fn compound_parent_path_budgets_fall_back_to_hex_before_directory_walks() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let deep_path = (0..12)
        .map(|depth| format!("level{depth}"))
        .collect::<Vec<_>>()
        .join("/");
    let name = "中".repeat(31);
    let long_path = [name.as_str(); 3].join("/");
    for (index, storage_path) in [deep_path, long_path].iter().enumerate() {
        let path = directory.path().join(format!("directory{index}.wps"));
        {
            let mut compound = cfb::create(&path)?;
            let storage_path = format!("/{storage_path}");
            compound.create_storage_all(&storage_path)?;
            compound
                .create_stream(format!("{storage_path}/WordDocument"))?
                .write_all(b"Actual document stream text")?;
        }
        let before = fs::read(&path)?;
        assert!(read(&path, &PreviewRequest::default()).is_err());
        let page = crate::read(
            &path,
            &PreviewRequest {
                section: Some("Unavailable section".into()),
                offset: 200,
            },
        )?;
        assert!(page.is_hex);
        assert!(page.sections.is_empty());
        assert_eq!(
            page.rows.first().and_then(|row| row.first()),
            Some(&"0000000000000000".into())
        );
        assert!(
            page.note
                .context("compound directory budget note")?
                .contains("Structured preview failed:")
        );
        assert_eq!(fs::read(&path)?, before);
    }
    Ok(())
}

fn push_biff(output: &mut Vec<u8>, kind: u16, data: &[u8]) -> Result<()> {
    output.extend_from_slice(&kind.to_le_bytes());
    output.extend_from_slice(&u16::try_from(data.len())?.to_le_bytes());
    output.extend_from_slice(data);
    Ok(())
}

#[test]
fn biff8_cells_and_continued_unicode_strings_are_paged() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("real.xls");
    let mut globals = Vec::new();
    push_biff(&mut globals, 0x0809, &[0, 6, 5, 0])?;
    let boundsheet_offset = globals.len() + 4;
    let mut sheet_name = vec![0, 0, 0, 0, 0, 0, 4, 0];
    sheet_name.extend_from_slice(b"Data");
    push_biff(&mut globals, 0x0085, &sheet_name)?;
    let mut shared = Vec::new();
    shared.extend_from_slice(&1u32.to_le_bytes());
    shared.extend_from_slice(&1u32.to_le_bytes());
    shared.extend_from_slice(&3u16.to_le_bytes());
    shared.push(1);
    shared.extend_from_slice(&u16::from(b'A').to_le_bytes());
    shared.extend_from_slice(&u16::try_from('中' as u32)?.to_le_bytes());
    push_biff(&mut globals, 0x00fc, &shared)?;
    push_biff(&mut globals, 0x003c, &[0, b'B'])?;
    push_biff(&mut globals, 0x000a, &[])?;
    let start = u32::try_from(globals.len())?;
    globals
        .get_mut(boundsheet_offset..boundsheet_offset + 4)
        .context("boundsheet offset")?
        .copy_from_slice(&start.to_le_bytes());
    let mut cells = Vec::new();
    push_biff(&mut cells, 0x0809, &[0, 6, 0x10, 0])?;
    push_biff(&mut cells, 0x00fd, &[0, 0, 0, 0, 0, 0, 0, 0, 0, 0])?;
    for row in 1..202u16 {
        let mut data = Vec::new();
        data.extend_from_slice(&row.to_le_bytes());
        data.extend_from_slice(&0u16.to_le_bytes());
        data.extend_from_slice(&0u16.to_le_bytes());
        data.extend_from_slice(&f64::from(row).to_le_bytes());
        push_biff(&mut cells, 0x0203, &data)?;
    }
    let mut multiple = Vec::new();
    multiple.extend_from_slice(&202u16.to_le_bytes());
    multiple.extend_from_slice(&0u16.to_le_bytes());
    multiple.extend_from_slice(&0u16.to_le_bytes());
    multiple.extend_from_slice(&((42u32 << 2) | 2).to_le_bytes());
    multiple.extend_from_slice(&0u16.to_le_bytes());
    push_biff(&mut cells, 0x00bd, &multiple)?;
    push_biff(&mut cells, 0x000a, &[])?;
    {
        let mut compound = cfb::create(&path)?;
        let mut stream = compound.create_stream("/Workbook")?;
        stream.write_all(&globals)?;
        stream.write_all(&cells)?;
    }
    let before = fs::read(&path)?;
    let first = read(&path, &PreviewRequest::default())?;
    assert_eq!(first.sections, ["Sheet: Data"]);
    assert_eq!(first.rows.len(), PAGE_ROWS);
    assert_eq!(
        first.rows.first().context("XLS text cell")?,
        &["1", "1", "text", "A中B", ""]
    );
    assert_eq!(first.next_offset, Some(200));
    let second = read(
        &path,
        &PreviewRequest {
            section: Some("Sheet: Data".into()),
            offset: 200,
        },
    )?;
    assert_eq!(second.rows.len(), 3);
    assert_eq!(
        second.rows.first().context("next XLS cell")?,
        &["201", "1", "number", "200", ""]
    );
    assert_eq!(fs::read(&path)?, before);
    let wps_path = directory.path().join("workbook.et");
    fs::copy(&path, &wps_path)?;
    let wps = read(&wps_path, &PreviewRequest::default())?;
    assert_eq!(
        wps.rows.first().context("WPS stored BIFF8 value")?,
        &["1", "1", "text", "A中B", ""]
    );
    Ok(())
}

fn push_binary_record(output: &mut Vec<u8>, kind: u16, data: &[u8]) -> Result<()> {
    if kind < 128 {
        output.push(u8::try_from(kind)?);
    } else {
        output.push((kind as u8 & 0x7f) | 0x80);
        output.push(u8::try_from(kind >> 7)?);
    }
    let mut length = data.len();
    loop {
        let byte = (length & 0x7f) as u8;
        length >>= 7;
        output.push(if length == 0 { byte } else { byte | 0x80 });
        if length == 0 {
            break;
        }
    }
    output.extend_from_slice(data);
    Ok(())
}

#[test]
fn xlsb_binary_cell_records_show_values_and_formula_tokens() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("binary.xlsb");
    let mut strings = Vec::new();
    let mut string_record = vec![0];
    let units = "中文 shared".encode_utf16().collect::<Vec<_>>();
    string_record.extend_from_slice(&u32::try_from(units.len())?.to_le_bytes());
    for unit in units {
        string_record.extend_from_slice(&unit.to_le_bytes());
    }
    push_binary_record(&mut strings, 0x0013, &string_record)?;
    let mut cells = Vec::new();
    push_binary_record(&mut cells, 0x0000, &0u32.to_le_bytes())?;
    push_binary_record(&mut cells, 0x0007, &[0; 12])?;
    let mut numeric = vec![1, 0, 0, 0, 0, 0, 0, 0];
    numeric.extend_from_slice(&42f64.to_le_bytes());
    push_binary_record(&mut cells, 0x0005, &numeric)?;
    let mut formula = numeric;
    formula.extend_from_slice(&[0, 0]);
    formula.extend_from_slice(&1u32.to_le_bytes());
    formula.push(0x1e);
    push_binary_record(&mut cells, 0x0009, &formula)?;
    push_binary_record(&mut cells, 0x0092, &[])?;
    let mut writer = ZipWriter::new(File::create(&path)?);
    for (name, bytes) in [
        ("xl/workbook.bin", &[][..]),
        ("xl/sharedStrings.bin", strings.as_slice()),
        ("xl/worksheets/sheet1.bin", cells.as_slice()),
    ] {
        writer.start_file(name, SimpleFileOptions::default())?;
        writer.write_all(bytes)?;
    }
    writer.finish()?;
    let page = read(&path, &PreviewRequest::default())?;
    assert_eq!(page.sections, ["Sheet: xl/worksheets/sheet1.bin"]);
    assert_eq!(
        page.rows.first().context("XLSB text cell")?,
        &["1", "1", "text", "中文 shared", ""]
    );
    assert_eq!(
        page.rows.get(1).context("XLSB number cell")?,
        &["1", "2", "number", "42", ""]
    );
    assert_eq!(
        page.rows.get(2).context("XLSB formula cell")?,
        &["1", "2", "number", "42", "1e"]
    );
    Ok(())
}

#[test]
fn worksheet_names_do_not_collide_with_preview_controls() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("controls.xlsx");
    let names = ["Bytes", "Overview", "Content", "Sheet: Bytes"];
    let mut workbook = String::from("<workbook xmlns:r=\"relations\"><sheets>");
    let mut relationships = String::from("<Relationships>");
    let mut parts = Vec::new();
    for (index, name) in names.iter().enumerate() {
        workbook.push_str(&format!("<sheet name=\"{name}\" r:id=\"r{index}\"/>"));
        relationships.push_str(&format!(
            "<Relationship Id=\"r{index}\" Target=\"worksheets/sheet{index}.xml\"/>"
        ));
        parts.push((format!("xl/worksheets/sheet{index}.xml"), format!("<worksheet><sheetData><row><c r=\"A1\" t=\"inlineStr\"><is><t>{name}</t></is></c></row></sheetData></worksheet>")));
    }
    workbook.push_str("</sheets></workbook>");
    relationships.push_str("</Relationships>");
    parts.push(("xl/workbook.xml".into(), workbook));
    parts.push(("xl/_rels/workbook.xml.rels".into(), relationships));
    let borrowed = parts
        .iter()
        .map(|(name, xml)| (name.as_str(), xml.as_str()))
        .collect::<Vec<_>>();
    zip_document(&path, &borrowed)?;
    let before = fs::read(&path)?;
    for name in names {
        let section = sheet_section(name);
        let page = crate::read(
            &path,
            &PreviewRequest {
                section: Some(section.clone()),
                ..Default::default()
            },
        )?;
        assert_eq!(page.title, section);
        assert_eq!(
            page.rows
                .first()
                .and_then(|row| row.get(2))
                .context("named sheet cell")?,
            name
        );
    }
    assert_eq!(
        crate::read_bytes(
            &path,
            &PreviewRequest {
                section: Some("Bytes".into()),
                ..Default::default()
            }
        )?
        .title,
        "Hex"
    );
    assert_eq!(fs::read(&path)?, before);
    let path = directory.path().join("controls.ods");
    zip_document(
        &path,
        &[(
            "content.xml",
            "<document xmlns:table=\"table\" xmlns:office=\"office\" xmlns:text=\"text\"><table:table table:name=\"Bytes\"><table:table-row><table:table-cell office:value-type=\"string\"><text:p>actual content</text:p></table:table-cell></table:table-row></table:table></document>",
        )],
    )?;
    let page = crate::read(
        &path,
        &PreviewRequest {
            section: Some("Sheet: Bytes".into()),
            ..Default::default()
        },
    )?;
    assert_eq!(page.sections, ["Sheet: Bytes"]);
    assert!(
        page.rows
            .iter()
            .flatten()
            .any(|value| value == "actual content")
    );
    Ok(())
}

#[test]
fn oversized_xlsb_record_fails_before_payload_allocation() -> Result<()> {
    let mut record = vec![5];
    let mut length = 2 * 1024 * 1024usize;
    loop {
        let byte = (length & 0x7f) as u8;
        length >>= 7;
        record.push(if length == 0 { byte } else { byte | 0x80 });
        if length == 0 {
            break;
        }
    }
    let error = binary_record(&mut io::Cursor::new(record))
        .err()
        .context("expected XLSB record limit")?;
    assert!(error.to_string().contains("1 MiB"));
    Ok(())
}

fn push_presentation_record(
    output: &mut Vec<u8>,
    kind: u16,
    container: bool,
    data: &[u8],
) -> Result<()> {
    output.extend_from_slice(&(if container { 15u16 } else { 0u16 }).to_le_bytes());
    output.extend_from_slice(&kind.to_le_bytes());
    output.extend_from_slice(&u32::try_from(data.len())?.to_le_bytes());
    output.extend_from_slice(data);
    Ok(())
}

#[test]
fn legacy_powerpoint_decodes_slide_and_notes_text() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("legacy.ppt");
    let mut slide = Vec::new();
    let text = "Actual 中文 slide"
        .encode_utf16()
        .flat_map(u16::to_le_bytes)
        .collect::<Vec<_>>();
    push_presentation_record(&mut slide, 4000, false, &text)?;
    let mut notes = Vec::new();
    push_presentation_record(&mut notes, 4008, false, b"Actual speaker notes")?;
    let mut document = Vec::new();
    push_presentation_record(&mut document, 1006, true, &slide)?;
    push_presentation_record(&mut document, 1008, true, &notes)?;
    let mut records = Vec::new();
    push_presentation_record(&mut records, 1000, true, &document)?;
    {
        let mut compound = cfb::create(&path)?;
        let mut stream = compound.create_stream("/PowerPoint Document")?;
        stream.write_all(&records)?;
    }
    let before = fs::read(&path)?;
    let page = read(&path, &PreviewRequest::default())?;
    assert_eq!(page.sections, ["Document", "Slide 1", "Notes 1"]);
    assert_eq!(
        page.rows.first().context("legacy slide text")?,
        &["1", "Actual 中文 slide"]
    );
    let notes = read(
        &path,
        &PreviewRequest {
            section: Some("Notes 1".into()),
            offset: 0,
        },
    )?;
    assert_eq!(
        notes.rows.first().context("legacy speaker notes")?,
        &["1", "Actual speaker notes"]
    );
    assert_eq!(fs::read(&path)?, before);
    Ok(())
}
