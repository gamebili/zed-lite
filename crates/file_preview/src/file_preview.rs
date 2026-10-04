use anyhow::{Context as _, Result};
use std::path::Path;

mod binary;
mod binary_pdb;
pub mod decoder_budget;
mod formats;
mod inspect;
mod media;
mod mesh;
mod office;
mod psd;
mod sqlite;
mod texture;
mod uasset_summary;

pub use formats::{FileKind, classify};
pub fn run_decoder_worker_if_invoked() {
    media::run_svg_worker_if_invoked();
    texture::run_texture_worker_if_invoked();
}

pub const PAGE_ROWS: usize = 200;
pub const PAGE_BYTES: usize = 2 * 1024 * 1024;
pub const CELL_BYTES: usize = 4096;

#[derive(Clone, Debug, Default)]
pub struct PreviewRequest {
    pub section: Option<String>,
    pub offset: u64,
}

#[derive(Clone, Debug, Default)]
pub struct PreviewPage {
    pub title: String,
    pub metadata: Vec<(String, String)>,
    pub sections: Vec<String>,
    pub columns: Vec<String>,
    pub rows: Vec<Vec<String>>,
    pub image: Option<Vec<u8>>,
    pub next_offset: Option<u64>,
    pub note: Option<String>,
    pub is_hex: bool,
}

pub fn read(path: &Path, request: &PreviewRequest) -> Result<PreviewPage> {
    anyhow::ensure!(
        path.metadata()?.is_file(),
        "Preview requires a regular file"
    );
    let preview = classify(path).map(|kind| match kind {
        FileKind::Sqlite => sqlite::read(path, request),
        FileKind::Sheet | FileKind::Word | FileKind::Presentation | FileKind::Wps => {
            office::read(path, request)
        }
        FileKind::Image | FileKind::Audio | FileKind::Video | FileKind::Pdf => {
            media::read(path, kind, request)
        }
        FileKind::Model | FileKind::Unreal | FileKind::Max | FileKind::Binary => {
            inspect::read(path, kind, request)
        }
    });
    let mut page = match preview {
        Some(Ok(mut page)) if page.is_hex && request.offset != 0 => {
            // Structured offsets can index rows, streams or frames. Start raw
            // navigation at zero; subsequent Hex pages use read_bytes().
            let raw = inspect::bytes(path, &PreviewRequest::default())
                .context("Cannot read the first raw file byte page")?;
            page.title = raw.title;
            page.columns = raw.columns;
            page.rows = raw.rows;
            page.image = raw.image;
            page.next_offset = raw.next_offset;
            page
        }
        Some(Ok(page)) => page,
        Some(Err(error)) => {
            let mut page = inspect::bytes(path, &PreviewRequest::default()).with_context(|| {
                format!("Cannot read file bytes after structured preview failed: {error:#}")
            })?;
            page.note = Some(format!(
                "Structured preview failed: {error:#}. Showing raw file bytes from offset 0."
            ));
            page
        }
        None => {
            let mut page = inspect::bytes(path, request)
                .context("Cannot read file bytes for the unsupported preview format")?;
            page.note = Some("Unsupported preview format. Showing raw file bytes as Hex.".into());
            page
        }
    };
    let mut remaining = PAGE_BYTES;
    let mut limited = false;
    for cell in page.rows.iter_mut().flatten() {
        let mut length = cell.len().min(CELL_BYTES).min(remaining);
        while !cell.is_char_boundary(length) {
            length -= 1;
        }
        if length < cell.len() {
            cell.truncate(length);
            limited = true;
        }
        remaining = remaining.saturating_sub(cell.len());
    }
    if limited {
        let note = page.note.get_or_insert_with(String::new);
        if !note.is_empty() {
            note.push('\n');
        }
        note.push_str("Some cell prefixes are limited by the page byte budget.");
    }
    Ok(page)
}

pub fn read_bytes(path: &Path, request: &PreviewRequest) -> Result<PreviewPage> {
    anyhow::ensure!(
        path.metadata()?.is_file(),
        "Preview requires a regular file"
    );
    inspect::bytes(path, request)
}

#[derive(Default, Debug, serde::Serialize)]
pub(crate) struct Prop {
    pub label: String,
    pub value: String,
    pub time: Option<i64>,
}

#[derive(Default, Debug, serde::Serialize)]
pub(crate) struct Section {
    pub title: String,
    pub items: Vec<String>,
}

fn extension(path: &str) -> String {
    Path::new(path)
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs::{self, File},
        io::{Seek as _, SeekFrom, Write as _},
    };

    fn hex_bytes(page: &PreviewPage) -> Result<Vec<u8>> {
        page.rows
            .iter()
            .map(|row| row.get(1).context("Missing hexadecimal row"))
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .flat_map(|cell| cell.split_whitespace())
            .map(|value| u8::from_str_radix(value, 16).map_err(anyhow::Error::from))
            .collect()
    }

    #[test]
    fn unsupported_binary_formats_open_as_hex_and_keep_byte_page_offsets() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("unknown.foreign_format");
        let source = (0..PAGE_ROWS * 16 * 2 + 37)
            .map(|index| (index % 251) as u8)
            .collect::<Vec<_>>();
        fs::write(&path, &source)?;
        let mut offset = 11u64;
        let mut contents = Vec::new();
        loop {
            let page = read(
                &path,
                &PreviewRequest {
                    section: Some("Bytes".into()),
                    offset,
                },
            )?;
            assert!(page.is_hex);
            assert_eq!(page.title, "Hex");
            assert_eq!(page.columns, ["Offset", "Hexadecimal", "ASCII"]);
            assert!(page.sections.is_empty());
            assert!(page.rows.len() <= PAGE_ROWS);
            assert_eq!(
                page.rows.first().and_then(|row| row.first()),
                Some(&format!("{offset:016X}"))
            );
            assert!(
                page.note
                    .as_deref()
                    .is_some_and(|note| note.contains("Unsupported preview format"))
            );
            let data = hex_bytes(&page)?;
            assert!(data.len() <= PAGE_ROWS * 16);
            contents.extend_from_slice(&data);
            match page.next_offset {
                Some(next) => {
                    assert_eq!(next, offset + data.len() as u64);
                    offset = next;
                }
                None => break,
            }
        }
        assert_eq!(contents, source.get(11..).context("Source suffix")?);
        assert_eq!(fs::read(&path)?, source);
        Ok(())
    }

    #[test]
    fn malformed_known_formats_fall_back_to_hex_from_the_start_with_the_original_error()
    -> Result<()> {
        let directory = tempfile::tempdir()?;
        for extension in ["sqlite3", "dll", "uasset", "glb", "ktx", "docx"] {
            let mut source = vec![0xf7; PAGE_ROWS * 16 + 19];
            if extension == "docx" {
                source[..2].copy_from_slice(b"PK");
            }
            let path = directory.path().join(format!("broken.{extension}"));
            fs::write(&path, &source)?;
            let page = read(
                &path,
                &PreviewRequest {
                    section: Some("Unavailable section".into()),
                    offset: 200,
                },
            )?;
            assert!(page.is_hex, "Missing Hex fallback for {extension}");
            assert_eq!(page.next_offset, Some((PAGE_ROWS * 16) as u64));
            assert_eq!(
                page.rows
                    .first()
                    .and_then(|row| row.first())
                    .map(String::as_str),
                Some("0000000000000000")
            );
            assert_eq!(hex_bytes(&page)?, source[..PAGE_ROWS * 16]);
            let note = page
                .note
                .as_deref()
                .context("Missing parser failure note")?;
            assert!(note.contains("Structured preview failed:"));
            assert!(note.contains("offset 0"));
            if extension == "sqlite3" {
                assert!(note.contains("Invalid SQLite file header"));
            }
            let next = read_bytes(
                &path,
                &PreviewRequest {
                    offset: page.next_offset.context("Missing Hex continuation")?,
                    ..Default::default()
                },
            )?;
            assert!(next.is_hex);
            assert_eq!(hex_bytes(&next)?, source[PAGE_ROWS * 16..]);
            assert_eq!(next.next_offset, None);
            assert_eq!(fs::read(&path)?, source);
        }
        Ok(())
    }

    #[test]
    fn sparse_huge_files_use_bounded_hex_fallback_and_can_seek_to_the_last_bytes() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("huge.sqlite3");
        let size = 64 * 1024 * 1024 * 1024u64;
        let tail = *b"last sixteen raw";
        let mut file = File::create(&path)?;
        file.write_all(b"invalid SQLite header")?;
        file.set_len(size)?;
        file.seek(SeekFrom::End(-16))?;
        file.write_all(&tail)?;
        drop(file);
        let page = read(
            &path,
            &PreviewRequest {
                offset: 200,
                ..Default::default()
            },
        )?;
        assert!(page.is_hex);
        assert_eq!(page.rows.len(), PAGE_ROWS);
        assert_eq!(hex_bytes(&page)?.len(), PAGE_ROWS * 16);
        assert_eq!(page.next_offset, Some((PAGE_ROWS * 16) as u64));
        assert!(
            page.note
                .as_deref()
                .is_some_and(|note| note.contains("Invalid SQLite file header"))
        );
        let unknown = directory.path().join("huge.foreign_format");
        fs::rename(&path, &unknown)?;
        let last = read(
            &unknown,
            &PreviewRequest {
                offset: size - 16,
                ..Default::default()
            },
        )?;
        assert!(last.is_hex);
        assert_eq!(last.rows.len(), 1);
        assert_eq!(hex_bytes(&last)?, tail);
        assert_eq!(last.next_offset, None);
        assert_eq!(fs::metadata(&unknown)?.len(), size);
        Ok(())
    }

    #[test]
    fn native_budget_fallback_resets_structured_offsets_for_hex_navigation() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("large.max");
        let mut file = File::create(&path)?;
        file.write_all(b"raw compound file")?;
        file.set_len(64 * 1024 * 1024)?;
        drop(file);
        let page = read(
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
        assert!(hex_bytes(&page)?.starts_with(b"raw compound file"));
        assert_eq!(page.next_offset, Some((PAGE_ROWS * 16) as u64));
        assert!(
            page.note
                .as_deref()
                .is_some_and(|note| note.contains("32 MiB budget"))
        );
        Ok(())
    }

    #[test]
    fn a_real_bytes_content_section_remains_structured_content() -> Result<()> {
        let file = tempfile::NamedTempFile::with_suffix(".gltf")?;
        fs::write(
            file.path(),
            br#"{"asset":{"version":"2.0"},"Bytes":["actual scene data"]}"#,
        )?;
        let page = read(
            file.path(),
            &PreviewRequest {
                section: Some("Bytes".into()),
                ..Default::default()
            },
        )?;
        assert!(!page.is_hex);
        assert_eq!(page.title, "Bytes");
        assert!(page.sections.contains(&"Bytes".into()));
        assert!(
            page.rows
                .iter()
                .flatten()
                .any(|cell| cell.contains("actual scene data"))
        );
        Ok(())
    }

    #[test]
    fn missing_sources_and_non_files_remain_read_errors() -> Result<()> {
        let directory = tempfile::tempdir()?;
        for path in [
            directory.path().join("missing.sqlite3"),
            directory.path().join("missing.foreign_format"),
        ] {
            let error = read(&path, &PreviewRequest::default())
                .err()
                .context("Missing source must remain an error")?;
            assert_eq!(
                error
                    .downcast_ref::<std::io::Error>()
                    .map(std::io::Error::kind),
                Some(std::io::ErrorKind::NotFound)
            );
            assert!(read_bytes(&path, &PreviewRequest::default()).is_err());
        }
        assert!(read(directory.path(), &PreviewRequest::default()).is_err());
        assert!(read_bytes(directory.path(), &PreviewRequest::default()).is_err());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn inaccessible_sources_are_not_hidden_by_hex_fallback() -> Result<()> {
        use std::os::unix::fs::PermissionsExt as _;
        if unsafe { libc::geteuid() } == 0 {
            return Ok(());
        }
        let directory = tempfile::tempdir()?;
        for extension in ["sqlite3", "foreign_format"] {
            let path = directory.path().join(format!("unreadable.{extension}"));
            fs::write(&path, b"source data")?;
            let permissions = fs::metadata(&path)?.permissions();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o0))?;
            let result = read(&path, &PreviewRequest::default());
            fs::set_permissions(&path, permissions)?;
            let error = result
                .err()
                .context("Unreadable source must remain an error")?;
            assert_eq!(
                error
                    .downcast_ref::<std::io::Error>()
                    .map(std::io::Error::kind),
                Some(std::io::ErrorKind::PermissionDenied)
            );
        }
        Ok(())
    }
}
