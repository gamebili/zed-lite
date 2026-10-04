use anyhow::{Context as _, Result};
use file_preview::{PreviewRequest, read};
use std::path::PathBuf;

fn main() -> Result<()> {
    file_preview::run_decoder_worker_if_invoked();
    let mut arguments = std::env::args_os().skip(1);
    let path = PathBuf::from(
        arguments
            .next()
            .context("Usage: preview_probe PATH [SECTION] [OFFSET]")?,
    );
    let section = arguments
        .next()
        .map(|value| value.to_string_lossy().into_owned());
    let offset = arguments
        .next()
        .map(|value| value.to_string_lossy().parse::<u64>())
        .transpose()?
        .unwrap_or_default();
    let request = PreviewRequest { section, offset };
    let page = if request.section.as_deref() == Some("Bytes") {
        file_preview::read_bytes(&path, &request)?
    } else {
        read(&path, &request)?
    };
    println!(
        "rows={} columns={} sections={} text_bytes={} image_bytes={} next={:?}",
        page.rows.len(),
        page.columns.len(),
        page.sections.len(),
        page.rows.iter().flatten().map(String::len).sum::<usize>(),
        page.image.as_ref().map_or(0, Vec::len),
        page.next_offset
    );
    if let Some(note) = page.note {
        println!("{note}");
    }
    Ok(())
}
