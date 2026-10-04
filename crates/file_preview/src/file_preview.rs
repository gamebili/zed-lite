use anyhow::Result;
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
}

pub fn read(path: &Path, request: &PreviewRequest) -> Result<PreviewPage> {
    anyhow::ensure!(
        path.metadata()?.is_file(),
        "Preview requires a regular file"
    );
    let kind = classify(path).ok_or_else(|| anyhow::anyhow!("Unsupported preview format"))?;
    let mut page = match kind {
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
    }?;
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
