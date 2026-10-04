use crate::{CELL_BYTES, FileKind, PAGE_ROWS, PreviewPage, PreviewRequest};
use anyhow::{Context as _, Result, bail, ensure};
use std::{
    fs::File,
    io::{self, BufRead, BufReader, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

struct CompoundReader {
    file: File,
    remaining: u64,
    deadline: Instant,
}

impl Read for CompoundReader {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if output.is_empty() {
            return Ok(0);
        }
        if self.remaining == 0 || Instant::now() >= self.deadline {
            return Err(io::Error::other(
                "Compound-file metadata exceeds its read or execution budget",
            ));
        }
        let length = output.len().min(self.remaining as usize);
        let count = self.file.read(&mut output[..length])?;
        self.remaining -= count as u64;
        Ok(count)
    }
}

impl Seek for CompoundReader {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        if Instant::now() >= self.deadline {
            return Err(io::Error::other(
                "Compound-file indexing exceeded its execution budget",
            ));
        }
        self.file.seek(position)
    }
}

fn compound_reader(mut file: File) -> Result<CompoundReader> {
    let mut header = [0; 512];
    file.read_exact(&mut header)
        .context("Truncated compound-file header")?;
    ensure!(
        &header[..8] == b"\xd0\xcf\x11\xe0\xa1\xb1\x1a\xe1",
        "Invalid compound-file signature"
    );
    let sector_shift = u16::from_le_bytes(header[30..32].try_into()?);
    ensure!(
        matches!(sector_shift, 9 | 12),
        "Invalid compound-file sector size"
    );
    let sector_bytes = 1u64 << sector_shift;
    for offset in [40, 44, 64, 72] {
        let count = u64::from(u32::from_le_bytes(header[offset..offset + 4].try_into()?));
        ensure!(
            count.saturating_mul(sector_bytes) <= 2 * 1024 * 1024,
            "Compound-file allocation tables exceed the 2 MiB header budget"
        );
    }
    file.rewind()?;
    Ok(CompoundReader {
        file,
        remaining: 16 * 1024 * 1024,
        deadline: Instant::now() + Duration::from_secs(5),
    })
}

pub(crate) fn bytes(path: &Path, request: &PreviewRequest) -> Result<PreviewPage> {
    let mut file = File::open(path)?;
    let size = file.metadata()?.len();
    let offset = request.offset.min(size);
    file.seek(SeekFrom::Start(offset))?;
    let mut data = vec![0; PAGE_ROWS * 16];
    let length = file.read(&mut data)?;
    data.truncate(length);
    let mut page = hex_page(&data, offset, size, "Hex");
    page.is_hex = true;
    Ok(page)
}

fn hex_page(data: &[u8], offset: u64, size: u64, title: &str) -> PreviewPage {
    let rows = data
        .chunks(16)
        .enumerate()
        .map(|(index, chunk)| {
            vec![
                format!("{:016X}", offset + index as u64 * 16),
                chunk.iter().map(|byte| format!("{byte:02X} ")).collect(),
                chunk
                    .iter()
                    .map(|byte| {
                        if byte.is_ascii_graphic() || *byte == b' ' {
                            char::from(*byte)
                        } else {
                            '.'
                        }
                    })
                    .collect(),
            ]
        })
        .collect();
    let end = offset + data.len() as u64;
    PreviewPage {
        title: title.into(),
        metadata: vec![("Size".into(), format!("{size} bytes"))],
        columns: vec!["Offset".into(), "Hexadecimal".into(), "ASCII".into()],
        rows,
        next_offset: (end < size && !data.is_empty()).then_some(end),
        ..Default::default()
    }
}

fn summary_page(summary: crate::binary::BinarySummary, request: &PreviewRequest) -> PreviewPage {
    let selected = request.section.as_deref().unwrap_or("Overview");
    let mut page = PreviewPage {
        title: selected.into(),
        sections: std::iter::once("Overview".into())
            .chain(summary.sections.iter().map(|section| section.title.clone()))
            .collect(),
        metadata: summary
            .props
            .iter()
            .map(|prop| (prop.label.clone(), prop.value.clone()))
            .collect(),
        columns: vec!["Content".into()],
        note: summary.note,
        ..Default::default()
    };
    let offset = usize::try_from(request.offset).unwrap_or(usize::MAX);
    if selected == "Overview" {
        page.columns = vec!["Property".into(), "Value".into()];
        page.rows = page
            .metadata
            .iter()
            .skip(offset)
            .take(PAGE_ROWS)
            .map(|(label, value)| vec![label.clone(), value.clone()])
            .collect();
        let next = offset.saturating_add(page.rows.len());
        page.next_offset = (next < page.metadata.len()).then_some(next as u64);
    } else if let Some(section) = summary
        .sections
        .iter()
        .find(|section| section.title == selected)
    {
        page.rows = section
            .items
            .iter()
            .skip(offset)
            .take(PAGE_ROWS)
            .map(|value| vec![bounded(value)])
            .collect();
        page.next_offset = (offset.saturating_add(page.rows.len()) < section.items.len())
            .then_some(offset.saturating_add(page.rows.len()) as u64);
    }
    page
}

fn bounded(value: &str) -> String {
    if value.len() <= CELL_BYTES {
        return value.into();
    }
    let mut end = CELL_BYTES;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &value[..end])
}

pub(crate) fn compound_streams<F>(compound: &cfb::CompoundFile<F>) -> Result<Vec<(String, u64)>> {
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut pending = vec![(PathBuf::from("/"), 0usize)];
    let mut streams = Vec::new();
    let mut entries = 0usize;
    let mut path_bytes = 0usize;
    while let Some((parent, depth)) = pending.pop() {
        // CFB clones the parent path for its sibling iterator before yielding
        // any entry. Bound that path before constructing each iterator.
        ensure!(
            parent.as_os_str().len() <= 256 && depth <= 8,
            "Compound directory exceeds the 256-byte path or 8-level nesting budget"
        );
        ensure!(
            Instant::now() < deadline,
            "Compound directory exceeds its execution budget"
        );
        for entry in compound.read_storage(&parent)? {
            entries += 1;
            ensure!(
                entries <= 4096,
                "Compound directory exceeds the 4096-entry storage and stream budget"
            );
            ensure!(
                Instant::now() < deadline,
                "Compound directory exceeds its execution budget"
            );
            ensure!(
                depth < 8,
                "Compound directory exceeds the 8-level nesting budget"
            );
            let path = entry.path();
            ensure!(
                path.as_os_str().len() <= 256,
                "Compound directory exceeds the 256-byte path budget"
            );
            let name = path.to_string_lossy();
            path_bytes += name.len();
            ensure!(
                path_bytes <= 1024 * 1024,
                "Compound directory exceeds the 1 MiB cumulative path budget"
            );
            if entry.is_stream() {
                streams.push((name.into_owned(), entry.len()));
            } else if entry.is_storage() {
                pending.push((path.to_path_buf(), depth + 1));
            }
        }
    }
    streams.sort_unstable_by(|left, right| left.0.cmp(&right.0));
    Ok(streams)
}

fn compound(path: &Path, request: &PreviewRequest) -> Result<PreviewPage> {
    // CFB eagerly builds FAT and directory indexes; cap input before opening it.
    if path.metadata()?.len() > 32 * 1024 * 1024 {
        let mut page = bytes(path, request)?;
        page.note = Some("Compound-file indexing exceeds the 32 MiB budget. File bytes remain available without loading the file.".into());
        return Ok(page);
    }
    // A size cap alone does not constrain corrupt files that reuse FAT sectors;
    // aggregate reads and the declared table sizes also need independent caps.
    let reader = compound_reader(File::open(path)?)?;
    let mut compound = cfb::CompoundFile::open(BufReader::with_capacity(32 * 1024, reader))?;
    let streams = compound_streams(&compound)?;
    let mut page = PreviewPage {
        title: "Compound document".into(),
        sections: std::iter::once("Streams".into())
            .chain(streams.iter().map(|(name, _)| name.clone()))
            .collect(),
        note: Some(
            "Stream content is shown without running the application, macros or scene scripts."
                .into(),
        ),
        ..Default::default()
    };
    let selected = request.section.as_deref().unwrap_or("Streams");
    if selected == "Streams" {
        page.columns = vec!["Stream".into(), "Bytes".into()];
        let offset = usize::try_from(request.offset).unwrap_or(usize::MAX);
        page.rows = streams
            .iter()
            .skip(offset)
            .take(PAGE_ROWS)
            .map(|(name, size)| vec![name.clone(), size.to_string()])
            .collect();
        let end = offset.saturating_add(page.rows.len());
        page.next_offset = (end < streams.len()).then_some(end as u64);
    } else {
        let (_, size) = streams
            .iter()
            .find(|(name, _)| name == selected)
            .context("Unknown compound-file stream")?;
        let mut stream = compound.open_stream(selected)?;
        let offset = request.offset.min(*size);
        stream.seek(SeekFrom::Start(offset))?;
        let mut content = vec![0; PAGE_ROWS * 16];
        let length = stream.read(&mut content)?;
        content.truncate(length);
        let mut content_page = hex_page(&content, offset, *size, selected);
        content_page.sections = page.sections;
        content_page.note = page.note;
        return Ok(content_page);
    }
    Ok(page)
}

fn text(path: &Path, request: &PreviewRequest) -> Result<PreviewPage> {
    let mut file = File::open(path)?;
    let size = file.metadata()?.len();
    file.seek(SeekFrom::Start(request.offset.min(size)))?;
    let mut input = BufReader::with_capacity(8192, file);
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut page = PreviewPage {
        title: "Model content".into(),
        sections: vec!["Content".into()],
        columns: vec!["Byte offset".into(), "Content".into()],
        ..Default::default()
    };
    let mut offset = request.offset.min(size);
    for _ in 0..PAGE_ROWS {
        if Instant::now() >= deadline {
            break;
        }
        let mut line = Vec::with_capacity(CELL_BYTES);
        let mut ended = false;
        while line.len() < CELL_BYTES {
            let available = input.fill_buf()?;
            if available.is_empty() {
                ended = true;
                break;
            }
            let count = available
                .iter()
                .position(|byte| *byte == b'\n')
                .map_or(available.len(), |index| index + 1)
                .min(CELL_BYTES - line.len());
            let newline = available.get(count.saturating_sub(1)) == Some(&b'\n');
            line.extend_from_slice(&available[..count]);
            input.consume(count);
            if newline {
                break;
            }
        }
        if line.is_empty() && ended {
            break;
        }
        if let Err(error) = std::str::from_utf8(&line) {
            if error.error_len().is_none()
                && error.valid_up_to() != 0
                && !input.fill_buf()?.is_empty()
            {
                let remaining = line.len() - error.valid_up_to();
                input.seek_relative(-i64::try_from(remaining)?)?;
                line.truncate(error.valid_up_to());
            } else {
                let mut page = bytes(path, request)?;
                page.note = Some("This model content is not valid UTF-8 at the requested position. Actual bytes are shown without replacing or discarding source data.".into());
                return Ok(page);
            }
        }
        if line.iter().any(|byte| {
            *byte == 0 || byte.is_ascii_control() && !matches!(*byte, b'\n' | b'\r' | b'\t')
        }) {
            let mut page = bytes(path, request)?;
            page.note =
                Some("Binary model content is shown as actual bytes instead of text.".into());
            return Ok(page);
        }
        let value = String::from_utf8(line.clone()).context("Invalid UTF-8 model segment")?;
        page.rows.push(vec![format!("{offset}"), value]);
        offset += line.len() as u64;
    }
    page.next_offset = (offset < size).then_some(offset);
    page.metadata.push(("Size".into(), format!("{size} bytes")));
    page.note=Some("Long lines are split into bounded segments. External textures and linked files are not loaded.".into());
    Ok(page)
}

fn gltf(path: &Path, request: &PreviewRequest) -> Result<PreviewPage> {
    let file = File::open(path)?;
    let file_size = file.metadata()?.len();
    let mut input = BufReader::new(file);
    let mut header = [0; 12];
    let header_bytes = file_size.min(header.len() as u64) as usize;
    input.read_exact(&mut header[..header_bytes])?;
    let json = if header_bytes >= 4 && header.get(..4) == Some(b"glTF") {
        ensure!(header_bytes == header.len(), "Truncated GLB header");
        ensure!(
            u32::from_le_bytes(header[4..8].try_into()?) == 2,
            "Only GLB version 2 is supported"
        );
        ensure!(
            u64::from(u32::from_le_bytes(header[8..12].try_into()?)) == file_size,
            "GLB declared length does not match the file length"
        );
        let mut chunk = [0; 8];
        input.read_exact(&mut chunk)?;
        if chunk.get(4..) != Some(b"JSON") {
            bail!("GLB first chunk is not JSON");
        }
        let length = u32::from_le_bytes(chunk[..4].try_into()?) as u64;
        ensure!(
            length != 0 && length.is_multiple_of(4),
            "GLB JSON chunk length is invalid"
        );
        ensure!(
            length.checked_add(20).is_some_and(|end| end <= file_size),
            "GLB JSON chunk is truncated"
        );
        if length > crate::PAGE_BYTES as u64 {
            bail!("Model JSON exceeds the 2 MiB structure budget; use Bytes for paged content");
        }
        let mut json = vec![0; length as usize];
        input
            .read_exact(&mut json)
            .context("Truncated GLB JSON chunk")?;
        let mut position = 20 + length;
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut chunks = 0usize;
        while position < file_size {
            chunks += 1;
            ensure!(
                chunks <= 4096 && Instant::now() < deadline,
                "GLB chunk index exceeds its preview budget"
            );
            ensure!(
                position.checked_add(8).is_some_and(|end| end <= file_size),
                "Truncated GLB chunk header"
            );
            input.seek(SeekFrom::Start(position))?;
            input.read_exact(&mut chunk)?;
            let length = u64::from(u32::from_le_bytes(chunk[..4].try_into()?));
            ensure!(length.is_multiple_of(4), "GLB chunk length is not aligned");
            ensure!(
                chunk.get(4..) != Some(b"JSON"),
                "GLB contains more than one JSON chunk"
            );
            position = position
                .checked_add(8)
                .and_then(|start| start.checked_add(length))
                .context("GLB chunk size overflow")?;
            ensure!(position <= file_size, "GLB binary chunk is truncated");
        }
        json
    } else {
        input.seek(SeekFrom::Start(0))?;
        let mut json = Vec::new();
        input
            .take(crate::PAGE_BYTES as u64 + 1)
            .read_to_end(&mut json)?;
        if json.len() > crate::PAGE_BYTES {
            bail!("Model JSON exceeds the 2 MiB structure budget; use Content or Bytes");
        }
        json
    };
    let document: serde_json::Value = serde_json::from_slice(&json)?;
    let object = document
        .as_object()
        .context("Model root is not an object")?;
    let selected = request.section.as_deref().unwrap_or("Scene");
    let mut page = PreviewPage {
        title: selected.into(),
        sections: std::iter::once("Scene".into())
            .chain(object.keys().cloned())
            .collect(),
        columns: vec!["Index / property".into(), "Value".into()],
        ..Default::default()
    };
    let offset = usize::try_from(request.offset).unwrap_or(usize::MAX);
    let values: Box<dyn Iterator<Item = (String, &serde_json::Value)> + '_> = if selected == "Scene"
    {
        Box::new(object.iter().map(|(name, value)| (name.clone(), value)))
    } else {
        let value = object.get(selected).context("Unknown model section")?;
        if let Some(array) = value.as_array() {
            Box::new(
                array
                    .iter()
                    .enumerate()
                    .map(|(index, value)| (index.to_string(), value)),
            )
        } else {
            Box::new(std::iter::once((selected.into(), value)))
        }
    };
    let mut values = values.skip(offset);
    for (name, value) in values.by_ref().take(PAGE_ROWS) {
        page.rows.push(vec![name, bounded(&value.to_string())]);
    }
    page.next_offset = values
        .next()
        .map(|_| offset.saturating_add(page.rows.len()) as u64);
    page.note=Some("Scene graph, meshes, materials, accessors and animation channels are inspectable. Buffer content is available in Bytes; linked resources are not read.".into());
    Ok(page)
}

pub(crate) fn read(path: &Path, kind: FileKind, request: &PreviewRequest) -> Result<PreviewPage> {
    match kind {
        FileKind::Binary => crate::binary::summarize(path)
            .map(|summary| summary_page(summary, request))
            .map_err(anyhow::Error::msg),
        FileKind::Unreal => crate::uasset_summary::inspect(path, true)
            .map(|inspection| {
                let blueprint = inspection.is_blueprint;
                let mut page = summary_page(inspection.summary, request);
                page.metadata
                    .push(("Blueprint".into(), blueprint.to_string()));
                page
            })
            .map_err(anyhow::Error::msg),
        FileKind::Max => compound(path, request),
        FileKind::Model => {
            let extension = crate::extension(&path.to_string_lossy());
            if matches!(extension.as_str(), "obj" | "stl" | "ply" | "off")
                && request.section.as_deref() == Some("Content")
            {
                let mut page = text(path, request)?;
                page.sections = crate::mesh::sections(path);
                return Ok(page);
            }
            if matches!(extension.as_str(), "obj" | "stl" | "ply" | "off")
                && request.section.as_deref().is_none_or(|section| {
                    section.starts_with("Mesh ") || matches!(section, "Vertices" | "Faces")
                })
            {
                crate::mesh::read(path, request)
            } else if matches!(extension.as_str(), "gltf" | "glb") {
                gltf(path, request)
            } else if matches!(
                extension.as_str(),
                "obj"
                    | "ply"
                    | "dae"
                    | "amf"
                    | "usda"
                    | "dxf"
                    | "lws"
                    | "ase"
                    | "ac"
                    | "nff"
                    | "off"
                    | "smd"
                    | "vta"
                    | "x3d"
                    | "wrl"
                    | "vrml"
                    | "ifc"
                    | "irr"
                    | "irrmesh"
                    | "bvh"
                    | "vtk"
                    | "vtp"
                    | "pcd"
                    | "xyz"
                    | "gcode"
                    | "md5mesh"
            ) {
                text(path, request)
            } else {
                let mut page = bytes(path, request)?;
                page.note=Some("This model encoding is displayed as paged binary content. Native mesh rendering is unavailable for this encoding.".into());
                Ok(page)
            }
        }
        _ => bail!("Unknown structured file kind"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn binary_overview_properties_have_a_next_page() {
        let summary = crate::binary::BinarySummary {
            props: (0..203)
                .map(|index| crate::Prop {
                    label: format!("Property {index}"),
                    value: index.to_string(),
                    time: None,
                })
                .collect(),
            ..Default::default()
        };
        let first = summary_page(summary, &PreviewRequest::default());
        assert_eq!(first.rows.len(), PAGE_ROWS);
        assert_eq!(first.next_offset, Some(200));
    }

    #[test]
    fn mesh_content_preserves_projection_navigation() -> Result<()> {
        let file = tempfile::NamedTempFile::with_suffix(".obj")?;
        std::fs::write(file.path(), b"v 0 0 0\nv 1 0 0\nv 0 1 0\nf 1 2 3\n")?;
        let page = read(
            file.path(),
            FileKind::Model,
            &PreviewRequest {
                section: Some("Content".into()),
                ..Default::default()
            },
        )?;
        assert_eq!(page.sections, ["Mesh XY", "Mesh XZ", "Mesh YZ", "Content"]);
        assert!(
            page.rows
                .iter()
                .flatten()
                .any(|value| value.contains("f 1 2 3"))
        );
        Ok(())
    }
    #[test]
    fn sparse_file_read_is_independent_of_file_size() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("huge.max");
        let mut file = File::create(&path)?;
        file.write_all(b"bounded content")?;
        file.set_len(64 * 1024 * 1024 * 1024)?;
        let page = read(&path, FileKind::Max, &PreviewRequest::default())?;
        assert_eq!(page.rows.len(), PAGE_ROWS);
        assert_eq!(page.next_offset, Some((PAGE_ROWS * 16) as u64));
        assert!(page.note.is_some());
        let last = bytes(
            &path,
            &PreviewRequest {
                offset: 64 * 1024 * 1024 * 1024 - 16,
                ..Default::default()
            },
        )?;
        assert_eq!(last.rows.len(), 1);
        assert_eq!(last.next_offset, None);
        Ok(())
    }
    #[test]
    fn long_model_line_is_paged_without_discarding_bytes() -> Result<()> {
        let file = tempfile::NamedTempFile::new()?;
        std::fs::write(file.path(), vec![b'x'; CELL_BYTES * PAGE_ROWS + 100])?;
        let first = text(file.path(), &PreviewRequest::default())?;
        assert_eq!(first.next_offset, Some((CELL_BYTES * PAGE_ROWS) as u64));
        let second = text(
            file.path(),
            &PreviewRequest {
                offset: first.next_offset.context("next")?,
                ..Default::default()
            },
        )?;
        assert_eq!(
            second
                .rows
                .first()
                .context("row")?
                .get(1)
                .context("value")?
                .len(),
            100
        );
        assert_eq!(second.next_offset, None);
        Ok(())
    }
    #[test]
    fn gltf_exposes_actual_mesh_and_material_content() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("scene.gltf");
        std::fs::write(
            &path,
            r#"{"asset":{"version":"2.0"},"meshes":[{"name":"hero","primitives":[{"attributes":{"POSITION":0}}]}],"materials":[{"name":"red"}]}"#,
        )?;
        let page = read(
            &path,
            FileKind::Model,
            &PreviewRequest {
                section: Some("meshes".into()),
                ..Default::default()
            },
        )?;
        assert!(
            page.rows
                .first()
                .context("mesh")?
                .get(1)
                .context("content")?
                .contains("POSITION")
        );
        assert!(page.sections.contains(&"materials".into()));
        Ok(())
    }

    #[test]
    fn multilingual_model_segments_preserve_utf8_and_exact_byte_offsets() -> Result<()> {
        let file = tempfile::NamedTempFile::new()?;
        let source = format!("ab{}", "中文🦀".repeat(100_000));
        std::fs::write(file.path(), &source)?;
        let first = text(file.path(), &PreviewRequest::default())?;
        let mut contents = first
            .rows
            .iter()
            .map(|row| row.get(1).context("text segment"))
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .cloned()
            .collect::<String>();
        let next_offset = first.next_offset.context("next UTF-8 page")?;
        assert_eq!(next_offset as usize, contents.len());
        let second = text(
            file.path(),
            &PreviewRequest {
                offset: next_offset,
                ..Default::default()
            },
        )?;
        for row in &second.rows {
            contents.push_str(row.get(1).context("second text segment")?);
        }
        assert_eq!(contents, source);
        assert_eq!(second.next_offset, None);
        assert!(!contents.contains('\u{fffd}'));
        Ok(())
    }

    #[test]
    fn invalid_utf8_and_binary_model_content_are_actual_byte_pages() -> Result<()> {
        let file = tempfile::NamedTempFile::new()?;
        std::fs::write(file.path(), b"model \xff\xfe")?;
        let page = text(file.path(), &PreviewRequest::default())?;
        assert!(page.is_hex);
        assert_eq!(page.columns, ["Offset", "Hexadecimal", "ASCII"]);
        assert!(
            page.rows
                .first()
                .and_then(|row| row.get(1))
                .context("hexadecimal")?
                .contains("FF FE")
        );
        assert!(page.note.context("UTF-8 note")?.contains("not valid UTF-8"));
        std::fs::write(file.path(), b"model\0binary")?;
        let page = text(file.path(), &PreviewRequest::default())?;
        assert_eq!(page.title, "Hex");
        assert!(page.is_hex);
        assert!(page.note.context("binary note")?.contains("Binary model"));
        Ok(())
    }

    fn glb_bytes(json: &[u8], declared_json_bytes: u32) -> Result<Vec<u8>> {
        let mut bytes = b"glTF".to_vec();
        bytes.extend_from_slice(&2u32.to_le_bytes());
        bytes.extend_from_slice(&u32::try_from(20 + json.len())?.to_le_bytes());
        bytes.extend_from_slice(&declared_json_bytes.to_le_bytes());
        bytes.extend_from_slice(b"JSON");
        bytes.extend_from_slice(json);
        Ok(bytes)
    }

    #[test]
    fn glb_requires_version_total_length_and_complete_chunks() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("model.glb");
        let valid = glb_bytes(b"{}  ", 4)?;
        std::fs::write(&path, &valid)?;
        assert_eq!(gltf(&path, &PreviewRequest::default())?.title, "Scene");
        let mut invalid = valid.clone();
        invalid
            .get_mut(4..8)
            .context("GLB version")?
            .copy_from_slice(&1u32.to_le_bytes());
        std::fs::write(&path, &invalid)?;
        assert!(
            gltf(&path, &PreviewRequest::default())
                .err()
                .context("version error")?
                .to_string()
                .contains("version 2")
        );
        let mut invalid = valid;
        invalid
            .get_mut(8..12)
            .context("GLB total length")?
            .copy_from_slice(&28u32.to_le_bytes());
        std::fs::write(&path, &invalid)?;
        assert!(
            gltf(&path, &PreviewRequest::default())
                .err()
                .context("length error")?
                .to_string()
                .contains("declared length")
        );
        std::fs::write(&path, glb_bytes(b"{}  ", 8)?)?;
        assert!(
            gltf(&path, &PreviewRequest::default())
                .err()
                .context("chunk error")?
                .to_string()
                .contains("truncated")
        );
        Ok(())
    }

    #[test]
    fn tiny_gltf_json_is_readable_and_truncated_binary_chunks_are_rejected() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("tiny.gltf");
        std::fs::write(&path, b"{}")?;
        assert_eq!(gltf(&path, &PreviewRequest::default())?.title, "Scene");
        let mut container = glb_bytes(b"{}  ", 4)?;
        container.extend_from_slice(&8u32.to_le_bytes());
        container.extend_from_slice(b"BIN\0");
        container.extend_from_slice(&[0; 4]);
        let length = u32::try_from(container.len())?;
        container
            .get_mut(8..12)
            .context("total length")?
            .copy_from_slice(&length.to_le_bytes());
        std::fs::write(&path, &container)?;
        assert!(
            gltf(&path, &PreviewRequest::default())
                .err()
                .context("binary chunk error")?
                .to_string()
                .contains("binary chunk is truncated")
        );
        Ok(())
    }

    #[test]
    fn compound_declared_table_sizes_are_checked_before_indexing() -> Result<()> {
        let file = tempfile::NamedTempFile::new()?;
        let mut header = [0; 512];
        header[..8].copy_from_slice(b"\xd0\xcf\x11\xe0\xa1\xb1\x1a\xe1");
        header[30..32].copy_from_slice(&9u16.to_le_bytes());
        header[44..48].copy_from_slice(&u32::MAX.to_le_bytes());
        std::fs::write(file.path(), header)?;
        let error = compound_reader(File::open(file.path())?)
            .err()
            .context("expected table limit")?;
        assert!(error.to_string().contains("header budget"));
        Ok(())
    }

    #[test]
    fn compound_reader_bounds_repeated_reads_and_execution_time() -> Result<()> {
        let file = tempfile::NamedTempFile::new()?;
        std::fs::write(file.path(), b"abcd")?;
        let mut reader = CompoundReader {
            file: File::open(file.path())?,
            remaining: 1,
            deadline: Instant::now() + Duration::from_secs(5),
        };
        let mut output = [0; 4];
        assert_eq!(reader.read(&mut output)?, 1);
        reader.seek(SeekFrom::Start(0))?;
        assert!(reader.read(&mut output).is_err());
        reader.remaining = 4;
        reader.deadline = Instant::now() - Duration::from_secs(1);
        assert!(reader.read(&mut output).is_err());
        assert!(reader.seek(SeekFrom::Start(0)).is_err());
        Ok(())
    }

    #[test]
    fn compound_stream_pages_preserve_source_bytes() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("scene.max");
        {
            let mut compound = cfb::create(&path)?;
            let mut stream = compound.create_stream("/Scene")?;
            stream.write_all(b"actual scene records")?;
        }
        let before = std::fs::read(&path)?;
        let page = compound(
            &path,
            &PreviewRequest {
                section: Some("/Scene".into()),
                ..Default::default()
            },
        )?;
        assert_eq!(page.rows.len(), 2);
        assert!(!page.is_hex);
        assert!(page.sections.contains(&"/Scene".into()));
        assert_eq!(std::fs::read(&path)?, before);
        Ok(())
    }

    #[test]
    fn small_compound_files_cannot_expand_deep_or_long_directory_paths() -> Result<()> {
        let directory = tempfile::tempdir()?;
        for (name, component, levels, reason) in [
            ("deep", "Storage".into(), 40, "nesting budget"),
            ("long", "层".repeat(31), 4, "256-byte path budget"),
        ] {
            let path = directory.path().join(format!("{name}.max"));
            let mut storage = String::new();
            for _ in 0..levels {
                storage.push('/');
                storage.push_str(&component);
            }
            {
                let mut compound = cfb::create(&path)?;
                compound.create_storage_all(&storage)?;
                compound
                    .create_stream(format!("{storage}/Payload"))?
                    .write_all(b"actual scene records")?;
            }
            let before = std::fs::read(&path)?;
            assert!(before.len() < 256 * 1024);
            let error = compound(&path, &PreviewRequest::default())
                .err()
                .context("Expected bounded compound directory error")?;
            assert!(error.to_string().contains(reason), "{error:#}");
            let page = crate::read(
                &path,
                &PreviewRequest {
                    section: Some("Streams".into()),
                    offset: 200,
                },
            )?;
            assert!(page.is_hex);
            assert_eq!(
                page.rows
                    .first()
                    .and_then(|row| row.first())
                    .map(String::as_str),
                Some("0000000000000000")
            );
            assert_eq!(page.next_offset, Some((PAGE_ROWS * 16) as u64));
            assert!(
                page.note
                    .as_deref()
                    .is_some_and(|note| note.contains(reason))
            );
            assert_eq!(std::fs::read(&path)?, before);
        }
        Ok(())
    }

    #[test]
    fn empty_compound_storages_count_towards_the_directory_entry_budget() -> Result<()> {
        let mut compound = cfb::CompoundFile::create(io::Cursor::new(Vec::new()))?;
        let mut pending = vec![(0usize, 4097usize)];
        while let Some((start, end)) = pending.pop() {
            if start == end {
                continue;
            }
            let middle = start + (end - start) / 2;
            compound.create_storage(format!("/Storage{middle:04}"))?;
            pending.push((start, middle));
            pending.push((middle + 1, end));
        }
        let error = compound_streams(&compound)
            .err()
            .context("Expected empty storage directory limit")?;
        assert!(error.to_string().contains("4096-entry"));
        Ok(())
    }
}
