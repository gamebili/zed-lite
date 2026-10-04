use super::*;
use std::io::Write;

fn word(output: &mut [u8], offset: usize, value: u32, little_endian: bool) {
    output[offset..offset + 4].copy_from_slice(&if little_endian {
        value.to_le_bytes()
    } else {
        value.to_be_bytes()
    });
}

fn ktx1_fixture(
    width: u32,
    height: u32,
    format: u32,
    kind: u32,
    channels: u32,
    little_endian: bool,
    levels: &[&[u8]],
) -> Vec<u8> {
    let mut output = vec![0; 64];
    output[..12].copy_from_slice(KTX1);
    for (offset, value) in [
        (12, 0x04030201),
        (16, kind),
        (20, 1),
        (24, channels),
        (28, format),
        (36, width),
        (40, height),
        (52, 1),
        (56, levels.len() as u32),
    ] {
        word(&mut output, offset, value, little_endian);
    }
    for level in levels {
        let size = level.len() as u32;
        output.extend_from_slice(&if little_endian {
            size.to_le_bytes()
        } else {
            size.to_be_bytes()
        });
        output.extend_from_slice(level);
        output.resize(output.len().div_ceil(4) * 4, 0);
    }
    output
}

fn ktx2_fixture(
    width: u32,
    height: u32,
    format: u32,
    compression: u32,
    levels: &[&[u8]],
) -> Vec<u8> {
    let mut output = vec![0; 80 + levels.len() * 24];
    output[..12].copy_from_slice(KTX2);
    for (offset, value) in [
        (12, format),
        (16, 1),
        (20, width),
        (24, height),
        (36, 1),
        (40, levels.len() as u32),
        (44, compression),
    ] {
        word(&mut output, offset, value, true);
    }
    for (index, level) in levels.iter().enumerate() {
        let start = 80 + index * 24;
        let offset = output.len() as u64;
        output[start..start + 8].copy_from_slice(&offset.to_le_bytes());
        output[start + 8..start + 16].copy_from_slice(&(level.len() as u64).to_le_bytes());
        output[start + 16..start + 24].copy_from_slice(&(level.len() as u64).to_le_bytes());
        output.extend_from_slice(level);
    }
    output
}

fn pvr_fixture(width: u32, height: u32, format: u64, count: u32, data: &[u8]) -> Vec<u8> {
    let mut output = vec![0; 52];
    output[8..16].copy_from_slice(&format.to_le_bytes());
    for (offset, value) in [
        (0, 0x03525650),
        (24, height),
        (28, width),
        (32, 1),
        (36, 1),
        (40, 1),
        (44, count),
    ] {
        word(&mut output, offset, value, true);
    }
    output.extend_from_slice(data);
    output
}

fn mip_request(level: u32) -> PreviewRequest {
    PreviewRequest {
        section: Some(format!("Mip {level}")),
        ..Default::default()
    }
}

fn decoded(page: PreviewPage) -> Result<RgbaImage> {
    let bytes = page.image.context("selected mip image")?;
    Ok(image::load_from_memory(&bytes)?.to_rgba8())
}

#[test]
fn ktx_overview_is_bounded_and_selected_rgba_mip_preserves_source() -> Result<()> {
    let file = tempfile::NamedTempFile::new()?;
    let source = ktx1_fixture(1, 1, 0x8058, 0x1401, 0x1908, true, &[&[10, 20, 30, 40]]);
    std::fs::write(file.path(), &source)?;
    let overview = read(file.path(), &PreviewRequest::default())?;
    assert_eq!(overview.title, "Overview");
    assert_eq!(overview.sections, ["Overview", "Mip 0"]);
    assert!(overview.image.is_none());
    assert_eq!(overview.rows.len(), 1);
    let image = decoded(read(file.path(), &mip_request(0))?)?;
    assert_eq!(image.get_pixel(0, 0).0, [10, 20, 30, 40]);
    assert_eq!(std::fs::read(file.path())?, source);
    Ok(())
}

#[test]
fn ktx_rgb_rows_honor_four_byte_padding() -> Result<()> {
    let file = tempfile::NamedTempFile::new()?;
    let data = [1, 2, 3, 4, 5, 6, 99, 99, 7, 8, 9, 10, 11, 12, 99, 99];
    std::fs::write(
        file.path(),
        ktx1_fixture(2, 2, 0x8051, 0x1401, 0x1907, true, &[&data]),
    )?;
    let image = decoded(read(file.path(), &mip_request(0))?)?;
    assert_eq!(image.get_pixel(1, 0).0, [4, 5, 6, 255]);
    assert_eq!(image.get_pixel(0, 1).0, [7, 8, 9, 255]);
    Ok(())
}

#[test]
fn ktx_big_endian_header_and_mip_size_are_supported() -> Result<()> {
    let file = tempfile::NamedTempFile::new()?;
    std::fs::write(
        file.path(),
        ktx1_fixture(1, 1, 0x8058, 0x1401, 0x1908, false, &[&[7, 8, 9, 255]]),
    )?;
    let image = decoded(read(file.path(), &mip_request(0))?)?;
    assert_eq!(image.get_pixel(0, 0).0, [7, 8, 9, 255]);
    Ok(())
}

#[test]
fn ktx_bc1_decodes_actual_block_colors() -> Result<()> {
    let file = tempfile::NamedTempFile::new()?;
    let block = [0, 248, 0, 0, 0, 0, 0, 0];
    std::fs::write(
        file.path(),
        ktx1_fixture(4, 4, 0x83f0, 0, 0, true, &[&block]),
    )?;
    let image = decoded(read(file.path(), &mip_request(0))?)?;
    assert!(image.pixels().all(|pixel| pixel.0 == [255, 0, 0, 255]));
    Ok(())
}

#[test]
fn ktx2_bc3_decodes_alpha_and_actual_block_colors() -> Result<()> {
    let file = tempfile::NamedTempFile::new()?;
    let block = [255, 0, 0, 0, 0, 0, 0, 0, 0, 248, 0, 0, 0, 0, 0, 0];
    std::fs::write(file.path(), ktx2_fixture(4, 4, 137, 0, &[&block]))?;
    let image = decoded(read(file.path(), &mip_request(0))?)?;
    assert!(image.pixels().all(|pixel| pixel.0 == [255, 0, 0, 255]));
    Ok(())
}

#[test]
fn ktx2_rgba_has_unpadded_rows_and_independent_mips() -> Result<()> {
    let file = tempfile::NamedTempFile::new()?;
    std::fs::write(
        file.path(),
        ktx2_fixture(
            2,
            1,
            37,
            0,
            &[&[1, 2, 3, 255, 4, 5, 6, 255], &[20, 30, 40, 255]],
        ),
    )?;
    let image = decoded(read(file.path(), &mip_request(1))?)?;
    assert_eq!(image.dimensions(), (1, 1));
    assert_eq!(image.get_pixel(0, 0).0, [20, 30, 40, 255]);
    Ok(())
}

#[test]
fn ktx2_bgra_channel_order_is_preserved() -> Result<()> {
    let file = tempfile::NamedTempFile::new()?;
    std::fs::write(
        file.path(),
        ktx2_fixture(1, 1, 44, 0, &[&[30, 20, 10, 255]]),
    )?;
    assert_eq!(
        decoded(read(file.path(), &mip_request(0))?)?
            .get_pixel(0, 0)
            .0,
        [10, 20, 30, 255]
    );
    Ok(())
}

#[test]
fn basis_supercompression_retains_readable_mip_index() -> Result<()> {
    let file = tempfile::NamedTempFile::new()?;
    std::fs::write(file.path(), ktx2_fixture(8192, 8192, 0, 1, &[&[1, 2, 3]]))?;
    for request in [PreviewRequest::default(), mip_request(0)] {
        let page = read(file.path(), &request)?;
        assert_eq!(page.rows.len(), 1);
        assert!(page.image.is_none());
        assert!(
            page.note
                .context("transcoder note")?
                .contains("supercompression")
        );
    }
    Ok(())
}

#[test]
fn pvr_version_three_rgba_is_native_and_read_only() -> Result<()> {
    let file = tempfile::NamedTempFile::new()?;
    let source = pvr_fixture(1, 1, 0x08080808_61626772, 1, &[4, 3, 2, 255]);
    std::fs::write(file.path(), &source)?;
    assert_eq!(
        decoded(read(file.path(), &mip_request(0))?)?
            .get_pixel(0, 0)
            .0,
        [4, 3, 2, 255]
    );
    assert_eq!(std::fs::read(file.path())?, source);
    Ok(())
}

#[test]
fn pvr_version_two_pvrtc_minimum_blocks_are_decoded() -> Result<()> {
    let file = tempfile::NamedTempFile::new()?;
    let mut source = vec![0; 84];
    for (offset, value) in [(0, 52), (4, 1), (8, 1), (16, 25), (44, 0x21525650), (48, 1)] {
        word(&mut source, offset, value, true);
    }
    std::fs::write(file.path(), source)?;
    let image = decoded(read(file.path(), &mip_request(0))?)?;
    assert_eq!(image.dimensions(), (1, 1));
    assert_eq!(image.get_pixel(0, 0).0, [0, 0, 0, 0]);
    Ok(())
}

#[test]
fn pvrtc_two_and_four_bit_blocks_produce_actual_colors() -> Result<()> {
    let file = tempfile::NamedTempFile::new()?;
    let mut blocks = Vec::new();
    for _ in 0..4 {
        blocks.extend_from_slice(&[0, 0, 0, 0, 0, 252, 0, 252]);
    }
    for (format, width) in [(0, 16), (2, 8)] {
        std::fs::write(file.path(), pvr_fixture(width, 8, format, 1, &blocks))?;
        let image = decoded(read(file.path(), &mip_request(0))?)?;
        assert!(image.pixels().all(|pixel| pixel.0 == [255, 0, 0, 255]));
    }
    Ok(())
}

#[test]
fn sparse_large_texture_refuses_large_mip_but_reads_smaller_mip() -> Result<()> {
    let file = tempfile::NamedTempFile::new()?;
    let size = 128 * 1024 * 1024 * 1024u64;
    let mut source = vec![0; 80 + 14 * 24];
    source[..12].copy_from_slice(KTX2);
    for (offset, value) in [(12, 37), (16, 1), (20, 8192), (24, 8192), (36, 1), (40, 14)] {
        word(&mut source, offset, value, true);
    }
    let mut offset = 4096u64;
    for level in 0..14 {
        let start = 80 + level * 24;
        let length = u64::from((8192u32 >> level).max(1)).pow(2) * 4;
        let current = if level == 13 { size - 4 } else { offset };
        source[start..start + 8].copy_from_slice(&current.to_le_bytes());
        source[start + 8..start + 16].copy_from_slice(&length.to_le_bytes());
        source[start + 16..start + 24].copy_from_slice(&length.to_le_bytes());
        offset += length;
    }
    let mut writer = File::create(file.path())?;
    writer.write_all(&source)?;
    writer.set_len(size)?;
    writer.seek(SeekFrom::Start(size - 4))?;
    writer.write_all(&[13, 14, 15, 255])?;
    let large = read(file.path(), &mip_request(0))?;
    assert!(large.image.is_none());
    assert!(
        large
            .note
            .context("large mip note")?
            .contains("pixel decode limit")
    );
    let small = decoded(read(file.path(), &mip_request(13))?)?;
    assert_eq!(small.get_pixel(0, 0).0, [13, 14, 15, 255]);
    assert_eq!(file.as_file().metadata()?.len(), size);
    Ok(())
}

#[test]
fn malformed_headers_and_excessive_mip_counts_fail_before_allocation() -> Result<()> {
    let file = tempfile::NamedTempFile::new()?;
    let mut source = ktx1_fixture(1, 1, 0x8058, 0x1401, 0x1908, true, &[&[0; 4]]);
    word(&mut source, 56, u32::MAX, true);
    std::fs::write(file.path(), source)?;
    assert!(read(file.path(), &PreviewRequest::default()).is_err());
    std::fs::write(file.path(), KTX2)?;
    assert!(read(file.path(), &PreviewRequest::default()).is_err());
    let mut source = pvr_fixture(1, 1, 0x08080808_61626772, 1, &[0; 4]);
    word(&mut source, 48, u32::MAX, true);
    std::fs::write(file.path(), source)?;
    assert!(read(file.path(), &PreviewRequest::default()).is_err());
    Ok(())
}

#[test]
fn truncated_mip_data_and_header_overlap_are_rejected() -> Result<()> {
    let file = tempfile::NamedTempFile::new()?;
    let mut source = ktx1_fixture(1, 1, 0x8058, 0x1401, 0x1908, true, &[&[0; 4]]);
    word(&mut source, 64, 100, true);
    std::fs::write(file.path(), source)?;
    assert!(read(file.path(), &PreviewRequest::default()).is_err());
    let mut source = ktx2_fixture(1, 1, 37, 0, &[&[0; 4]]);
    source[80..88].copy_from_slice(&0u64.to_le_bytes());
    std::fs::write(file.path(), source)?;
    assert!(read(file.path(), &PreviewRequest::default()).is_err());
    Ok(())
}

#[test]
fn unknown_pvr_encoding_preserves_declared_metadata() -> Result<()> {
    let file = tempfile::NamedTempFile::new()?;
    std::fs::write(file.path(), pvr_fixture(512, 512, 99, 3, &[]))?;
    let page = read(file.path(), &PreviewRequest::default())?;
    assert!(page.image.is_none());
    assert!(page.metadata.contains(&("Mip levels".into(), "3".into())));
    assert!(page.note.context("format note")?.contains("transcoder"));
    Ok(())
}

#[test]
fn unsupported_section_and_non_power_of_two_pvrtc_fail_safely() -> Result<()> {
    let file = tempfile::NamedTempFile::new()?;
    std::fs::write(file.path(), pvr_fixture(9, 8, 2, 1, &[0; 48]))?;
    assert!(
        read(file.path(), &mip_request(0))
            .err()
            .context("PVRTC extent error")?
            .to_string()
            .contains("power-of-two")
    );
    assert!(read(file.path(), &mip_request(99)).is_err());
    Ok(())
}

#[test]
fn astc_all_block_footprints_and_container_mappings_decode_real_color() -> Result<()> {
    let file = tempfile::NamedTempFile::new()?;
    let block = [
        0xfc, 0xfd, 255, 255, 255, 255, 255, 255, 17, 17, 34, 34, 51, 51, 255, 255,
    ];
    for (index, &(width, height)) in ASTC_BLOCKS.iter().enumerate() {
        for source in [
            ktx1_fixture(width, height, 0x93b0 + index as u32, 0, 0, true, &[&block]),
            ktx2_fixture(width, height, 157 + index as u32 * 2, 0, &[&block]),
            pvr_fixture(width, height, 27 + index as u64, 1, &block),
        ] {
            std::fs::write(file.path(), source)?;
            let image = decoded(read(file.path(), &mip_request(0))?)?;
            assert_eq!(image.dimensions(), (width, height));
            assert!(image.pixels().all(|pixel| pixel.0 == [17, 34, 51, 255]));
        }
    }
    Ok(())
}

#[test]
fn bc6_signed_and_unsigned_formats_decode_bounded_hdr_blocks() -> Result<()> {
    let file = tempfile::NamedTempFile::new()?;
    for format in [143, 144] {
        std::fs::write(file.path(), ktx2_fixture(4, 4, format, 0, &[&[0; 16]]))?;
        let image = decoded(read(file.path(), &mip_request(0))?)?;
        assert_eq!(image.dimensions(), (4, 4));
        assert!(image.pixels().all(|pixel| pixel.0 == [0, 0, 0, 255]));
    }
    Ok(())
}

#[test]
fn astc_requires_full_block_before_decoder_entry() -> Result<()> {
    let file = tempfile::NamedTempFile::new()?;
    std::fs::write(file.path(), ktx2_fixture(4, 4, 157, 0, &[&[0xfc, 0xfd]]))?;
    let page = read(file.path(), &PreviewRequest::default())?;
    assert_eq!(page.rows.len(), 1);
    assert!(read(file.path(), &mip_request(0)).is_err());
    Ok(())
}

#[test]
fn ktx_cube_array_image_size_includes_all_layers_and_faces() -> Result<()> {
    let file = tempfile::NamedTempFile::new()?;
    let mut source = ktx1_fixture(
        2,
        2,
        0x8058,
        0x1401,
        0x1908,
        true,
        &[&[4; 192], &[8, 9, 10, 255].repeat(12)],
    );
    word(&mut source, 48, 2, true);
    word(&mut source, 52, 6, true);
    std::fs::write(file.path(), source)?;
    let page = read(file.path(), &mip_request(1))?;
    assert_eq!(
        page.metadata
            .iter()
            .find(|(key, _)| key == "Layers")
            .context("layers")?
            .1,
        "2"
    );
    assert!(
        page.note
            .as_deref()
            .context("surface note")?
            .contains("first face")
    );
    assert_eq!(decoded(page)?.get_pixel(0, 0).0, [8, 9, 10, 255]);
    Ok(())
}

#[test]
fn ktx_non_array_cubemap_image_size_is_per_face() -> Result<()> {
    let file = tempfile::NamedTempFile::new()?;
    let mut source = ktx1_fixture(
        2,
        2,
        0x8058,
        0x1401,
        0x1908,
        true,
        &[&[4; 96], &[8, 9, 10, 255].repeat(6)],
    );
    word(&mut source, 52, 6, true);
    word(&mut source, 64, 16, true);
    word(&mut source, 164, 4, true);
    std::fs::write(file.path(), source)?;
    assert_eq!(
        decoded(read(file.path(), &mip_request(1))?)?
            .get_pixel(0, 0)
            .0,
        [8, 9, 10, 255]
    );
    Ok(())
}

#[test]
fn ktx2_cube_array_keeps_all_surface_metadata() -> Result<()> {
    let file = tempfile::NamedTempFile::new()?;
    let mut source = ktx2_fixture(1, 1, 37, 0, &[&[7, 8, 9, 255].repeat(12)]);
    word(&mut source, 32, 2, true);
    word(&mut source, 36, 6, true);
    std::fs::write(file.path(), source)?;
    let page = read(file.path(), &mip_request(0))?;
    assert!(page.metadata.contains(&("Faces".into(), "6".into())));
    assert!(page.metadata.contains(&("Layers".into(), "2".into())));
    assert_eq!(decoded(page)?.get_pixel(0, 0).0, [7, 8, 9, 255]);
    Ok(())
}
