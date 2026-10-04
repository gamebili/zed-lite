use crate::{PreviewPage, PreviewRequest};
use anyhow::{Context, Result, bail, ensure};
use image::{DynamicImage, RgbaImage};
use std::{
    fs::File,
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

const KTX1: &[u8; 12] = b"\xabKTX 11\xbb\r\n\x1a\n";
const KTX2: &[u8; 12] = b"\xabKTX 20\xbb\r\n\x1a\n";
const MAX_PIXELS: u64 = 4 * 1024 * 1024;
const MAX_DATA: u64 = 16 * 1024 * 1024;
const WORKER_ARGUMENT: &str = "--zed-file-preview-texture-worker";
const ASTC_BLOCKS: [(u32, u32); 14] = [
    (4, 4),
    (5, 4),
    (5, 5),
    (6, 5),
    (6, 6),
    (8, 5),
    (8, 6),
    (8, 8),
    (10, 5),
    (10, 6),
    (10, 8),
    (10, 10),
    (12, 10),
    (12, 12),
];

#[derive(Clone, Copy, Debug)]
enum Encoding {
    Rgb,
    Rgba,
    Bgra,
    Red,
    Rg,
    Bc1,
    Bc1Alpha,
    Bc2,
    Bc3,
    Bc4,
    Bc5,
    Bc6(bool),
    Bc7,
    Etc1,
    Etc2,
    Etc2Alpha1,
    Etc2Alpha8,
    EacRed,
    EacRg,
    Pvrtc2,
    Pvrtc4,
    Astc(u32, u32),
}

impl Encoding {
    fn channels(self) -> Option<u64> {
        match self {
            Self::Rgb => Some(3),
            Self::Rgba | Self::Bgra => Some(4),
            Self::Red => Some(1),
            Self::Rg => Some(2),
            _ => None,
        }
    }

    fn effective_size(self, width: u32, height: u32) -> (u32, u32) {
        match self {
            Self::Pvrtc2 => (width.max(16), height.max(8)),
            Self::Pvrtc4 => (width.max(8), height.max(8)),
            _ => (width, height),
        }
    }

    fn bytes(self, width: u32, height: u32, alignment: u64) -> Result<u64> {
        let (width, height) = self.effective_size(width, height);
        let width = u64::from(width);
        let height = u64::from(height);
        if let Some(channels) = self.channels() {
            let row = width
                .checked_mul(channels)
                .context("Texture row size overflow")?;
            return row
                .checked_add(alignment - 1)
                .map(|row| row / alignment * alignment)
                .and_then(|row| row.checked_mul(height))
                .context("Texture size overflow");
        }
        match self {
            Self::Astc(block_width, block_height) => width
                .div_ceil(u64::from(block_width))
                .checked_mul(height.div_ceil(u64::from(block_height)))
                .and_then(|blocks| blocks.checked_mul(16)),
            Self::Pvrtc2 => width
                .div_ceil(8)
                .checked_mul(height.div_ceil(4))
                .and_then(|blocks| blocks.checked_mul(8)),
            Self::Pvrtc4 => width
                .div_ceil(4)
                .checked_mul(height.div_ceil(4))
                .and_then(|blocks| blocks.checked_mul(8)),
            _ => {
                let block = if matches!(
                    self,
                    Self::Bc1
                        | Self::Bc1Alpha
                        | Self::Bc4
                        | Self::Etc1
                        | Self::Etc2
                        | Self::Etc2Alpha1
                        | Self::EacRed
                ) {
                    8
                } else {
                    16
                };
                width
                    .div_ceil(4)
                    .checked_mul(height.div_ceil(4))
                    .and_then(|blocks| blocks.checked_mul(block))
            }
        }
        .context("Texture size overflow")
    }
}

struct Mip {
    offset: u64,
    length: u64,
    width: u32,
    height: u32,
}

struct Texture {
    container: &'static str,
    format: String,
    encoding: Option<Encoding>,
    alignment: u64,
    width: u32,
    height: u32,
    faces: u32,
    layers: u32,
    depth: u32,
    declared_levels: u32,
    levels: Vec<Mip>,
    note: Option<String>,
}

fn integer(bytes: &[u8], offset: usize, little_endian: bool) -> Result<u32> {
    let bytes: [u8; 4] = bytes
        .get(offset..offset + 4)
        .context("Truncated texture header")?
        .try_into()?;
    Ok(if little_endian {
        u32::from_le_bytes(bytes)
    } else {
        u32::from_be_bytes(bytes)
    })
}

fn large_integer(bytes: &[u8], offset: usize) -> Result<u64> {
    let bytes: [u8; 8] = bytes
        .get(offset..offset + 8)
        .context("Truncated texture index")?
        .try_into()?;
    Ok(u64::from_le_bytes(bytes))
}

fn read_at(file: &mut File, offset: u64, output: &mut [u8]) -> Result<()> {
    let size = file.metadata()?.len();
    ensure!(
        offset
            .checked_add(output.len() as u64)
            .is_some_and(|end| end <= size),
        "Texture data extends beyond the source file"
    );
    file.seek(SeekFrom::Start(offset))?;
    file.read_exact(output)?;
    Ok(())
}

fn gl_encoding(format: u32, kind: u32, channels: u32) -> Option<Encoding> {
    if kind == 0x1401 {
        return match channels {
            0x1907 => Some(Encoding::Rgb),
            0x1908 => Some(Encoding::Rgba),
            0x80e1 => Some(Encoding::Bgra),
            0x1903 | 0x1909 => Some(Encoding::Red),
            0x8227 => Some(Encoding::Rg),
            _ => None,
        };
    }
    if kind != 0 {
        return None;
    }
    if let Some(index) = format
        .checked_sub(0x93b0)
        .filter(|index| *index < 14)
        .or_else(|| format.checked_sub(0x93d0).filter(|index| *index < 14))
    {
        return ASTC_BLOCKS
            .get(index as usize)
            .map(|&(width, height)| Encoding::Astc(width, height));
    }
    match format {
        0x83f0 | 0x8c4c => Some(Encoding::Bc1),
        0x83f1 | 0x8c4d => Some(Encoding::Bc1Alpha),
        0x83f2 | 0x8c4e => Some(Encoding::Bc2),
        0x83f3 | 0x8c4f => Some(Encoding::Bc3),
        0x8dbb => Some(Encoding::Bc4),
        0x8dbd => Some(Encoding::Bc5),
        0x8e8f => Some(Encoding::Bc6(false)),
        0x8e8e => Some(Encoding::Bc6(true)),
        0x8e8c | 0x8e8d => Some(Encoding::Bc7),
        0x8d64 => Some(Encoding::Etc1),
        0x9274 | 0x9275 => Some(Encoding::Etc2),
        0x9276 | 0x9277 => Some(Encoding::Etc2Alpha1),
        0x9278 | 0x9279 => Some(Encoding::Etc2Alpha8),
        0x9270 => Some(Encoding::EacRed),
        0x9272 => Some(Encoding::EacRg),
        0x8c01 | 0x8c03 => Some(Encoding::Pvrtc2),
        0x8c00 | 0x8c02 => Some(Encoding::Pvrtc4),
        _ => None,
    }
}

fn vk_encoding(format: u32) -> Option<Encoding> {
    if let Some(index) = format
        .checked_sub(157)
        .filter(|index| *index < 28)
        .map(|index| index / 2)
        .or_else(|| {
            format
                .checked_sub(1_000_066_000)
                .filter(|index| *index < 14)
        })
    {
        return ASTC_BLOCKS
            .get(index as usize)
            .map(|&(width, height)| Encoding::Astc(width, height));
    }
    match format {
        9 | 15 => Some(Encoding::Red),
        16 | 22 => Some(Encoding::Rg),
        23 | 29 => Some(Encoding::Rgb),
        37 | 43 => Some(Encoding::Rgba),
        44 | 50 => Some(Encoding::Bgra),
        131 | 132 => Some(Encoding::Bc1),
        133 | 134 => Some(Encoding::Bc1Alpha),
        135 | 136 => Some(Encoding::Bc2),
        137 | 138 => Some(Encoding::Bc3),
        139 => Some(Encoding::Bc4),
        141 => Some(Encoding::Bc5),
        143 => Some(Encoding::Bc6(false)),
        144 => Some(Encoding::Bc6(true)),
        145 | 146 => Some(Encoding::Bc7),
        147 | 148 => Some(Encoding::Etc2),
        149 | 150 => Some(Encoding::Etc2Alpha1),
        151 | 152 => Some(Encoding::Etc2Alpha8),
        153 => Some(Encoding::EacRed),
        155 => Some(Encoding::EacRg),
        1_000_054_000 | 1_000_054_004 => Some(Encoding::Pvrtc2),
        1_000_054_001 | 1_000_054_005 => Some(Encoding::Pvrtc4),
        _ => None,
    }
}

fn extent(file_size: u64, offset: u64, length: u64) -> Result<()> {
    ensure!(
        offset
            .checked_add(length)
            .is_some_and(|end| end <= file_size),
        "Texture mip data is truncated"
    );
    Ok(())
}

fn ktx1(file: &mut File) -> Result<Texture> {
    let mut header = [0; 64];
    read_at(file, 0, &mut header)?;
    let marker = integer(&header, 12, true)?;
    ensure!(
        matches!(marker, 0x04030201 | 0x01020304),
        "Invalid KTX byte order"
    );
    let little_endian = marker == 0x04030201;
    let value = |offset| integer(&header, offset, little_endian);
    let width = value(36)?;
    let height = value(40)?;
    let faces = value(52)?;
    let layers = value(48)?;
    let count = value(56)?.max(1);
    ensure!(
        width > 0 && height > 0 && count <= 32 && matches!(faces, 1 | 6),
        "Invalid KTX dimensions, mip count, or faces"
    );
    let format = value(28)?;
    let kind = value(16)?;
    let encoding = if value(20)? == 1 {
        gl_encoding(format, kind, value(24)?)
    } else {
        None
    };
    let mut offset = 64u64
        .checked_add(u64::from(value(60)?))
        .context("KTX metadata size overflow")?;
    let size = file.metadata()?.len();
    let mut levels = Vec::new();
    for level in 0..count {
        let mut bytes = [0; 4];
        read_at(file, offset, &mut bytes)?;
        let length = u64::from(integer(&bytes, 0, little_endian)?);
        let start = offset.checked_add(4).context("KTX level offset overflow")?;
        extent(size, start, length)?;
        let padded = length.checked_add(3).context("KTX level size overflow")? / 4 * 4;
        let copies = if faces == 6 && layers == 0 { 6 } else { 1 };
        offset = start
            .checked_add(
                padded
                    .checked_mul(copies)
                    .context("KTX face size overflow")?,
            )
            .context("KTX level offset overflow")?;
        ensure!(offset <= size, "KTX face data is truncated");
        levels.push(Mip {
            offset: start,
            length,
            width: (width >> level).max(1),
            height: (height >> level).max(1),
        });
    }
    Ok(Texture {
        container: "KTX 1",
        format: format!("OpenGL 0x{format:04x}"),
        encoding,
        alignment: 4,
        width,
        height,
        faces,
        layers: layers.max(1),
        depth: value(44)?.max(1),
        declared_levels: count,
        levels,
        note: None,
    })
}

fn ktx2(file: &mut File) -> Result<Texture> {
    let mut header = [0; 80];
    read_at(file, 0, &mut header)?;
    let value = |offset| integer(&header, offset, true);
    let width = value(20)?;
    let height = value(24)?;
    let faces = value(36)?;
    let count = value(40)?.max(1);
    ensure!(
        width > 0 && height > 0 && count <= 32 && matches!(faces, 1 | 6),
        "Invalid KTX2 dimensions, mip count, or faces"
    );
    let format = value(12)?;
    let compression = value(44)?;
    let size = file.metadata()?.len();
    let index_end = 80 + u64::from(count) * 24;
    let mut levels = Vec::new();
    for level in 0..count {
        let mut index = [0; 24];
        read_at(file, 80 + u64::from(level) * 24, &mut index)?;
        let offset = large_integer(&index, 0)?;
        let length = large_integer(&index, 8)?;
        ensure!(offset >= index_end, "KTX2 mip overlaps its header index");
        extent(size, offset, length)?;
        levels.push(Mip {
            offset,
            length,
            width: (width >> level).max(1),
            height: (height >> level).max(1),
        });
    }
    let note = (compression != 0).then(|| format!("KTX2 supercompression scheme {compression} requires a transcoder. Mip metadata and actual source bytes remain available."));
    Ok(Texture {
        container: "KTX 2",
        format: format!("Vulkan format {format}"),
        encoding: (compression == 0).then(|| vk_encoding(format)).flatten(),
        alignment: 1,
        width,
        height,
        faces,
        layers: value(32)?.max(1),
        depth: value(28)?.max(1),
        declared_levels: count,
        levels,
        note,
    })
}

fn pvr(file: &mut File) -> Result<Texture> {
    let mut header = [0; 52];
    read_at(file, 0, &mut header)?;
    let value = |offset| integer(&header, offset, true);
    let (width, height, faces, layers, depth, count, offset, encoding, format) =
        if value(0)? == 0x03525650 {
            let pixel_format = large_integer(&header, 8)?;
            let encoding = match pixel_format {
                0 | 1 => Some(Encoding::Pvrtc2),
                2 | 3 => Some(Encoding::Pvrtc4),
                6 => Some(Encoding::Etc1),
                7 => Some(Encoding::Bc1Alpha),
                9 => Some(Encoding::Bc2),
                11 => Some(Encoding::Bc3),
                12 => Some(Encoding::Bc4),
                13 => Some(Encoding::Bc5),
                14 => Some(Encoding::Bc6(value(20)? == 12)),
                15 => Some(Encoding::Bc7),
                22 => Some(Encoding::Etc2),
                23 => Some(Encoding::Etc2Alpha8),
                24 => Some(Encoding::Etc2Alpha1),
                25 => Some(Encoding::EacRed),
                26 => Some(Encoding::EacRg),
                27..=40 => ASTC_BLOCKS
                    .get((pixel_format - 27) as usize)
                    .map(|&(width, height)| Encoding::Astc(width, height)),
                0x08080808_61626772 => Some(Encoding::Rgba),
                0x08080808_61726762 => Some(Encoding::Bgra),
                0x00080808_00626772 => Some(Encoding::Rgb),
                _ => None,
            };
            (
                value(28)?,
                value(24)?,
                value(40)?,
                value(36)?.max(1),
                value(32)?.max(1),
                value(44)?,
                52 + u64::from(value(48)?),
                encoding,
                format!("PVR pixel format 0x{pixel_format:016x}"),
            )
        } else {
            ensure!(value(44)? == 0x21525650, "Invalid PVR texture signature");
            let flags = value(16)? & 255;
            let encoding = match flags {
                24 => Some(Encoding::Pvrtc2),
                25 => Some(Encoding::Pvrtc4),
                _ => None,
            };
            (
                value(8)?,
                value(4)?,
                value(48)?.max(1),
                1,
                1,
                value(12)?
                    .checked_add(1)
                    .context("PVR mip count overflow")?,
                u64::from(value(0)?),
                encoding,
                format!("PVR version 2 format {flags}"),
            )
        };
    ensure!(
        width > 0 && height > 0 && count > 0 && count <= 32 && matches!(faces, 1 | 6),
        "Invalid PVR dimensions, mip count, or faces"
    );
    ensure!(
        offset >= 52 && offset <= file.metadata()?.len(),
        "Invalid PVR metadata extent"
    );
    let mut levels = Vec::new();
    let mut offset = offset;
    if let Some(encoding) = encoding {
        for level in 0..count {
            let width = (width >> level).max(1);
            let height = (height >> level).max(1);
            let length = encoding.bytes(width, height, 1)?;
            extent(file.metadata()?.len(), offset, length)?;
            levels.push(Mip {
                offset,
                length,
                width,
                height,
            });
            let surfaces = u64::from(faces)
                .checked_mul(u64::from(layers))
                .and_then(|count| count.checked_mul(u64::from((depth >> level).max(1))))
                .context("PVR surface count overflow")?;
            offset = offset
                .checked_add(
                    length
                        .checked_mul(surfaces)
                        .context("PVR mip size overflow")?,
                )
                .context("PVR mip offset overflow")?;
            ensure!(
                offset <= file.metadata()?.len(),
                "PVR surface data is truncated"
            );
        }
    }
    Ok(Texture {
        container: "PVR",
        format,
        encoding,
        alignment: 1,
        width,
        height,
        faces,
        layers,
        depth,
        declared_levels: count,
        levels,
        note: None,
    })
}

fn decode(file: &mut File, mip: &Mip, encoding: Encoding, alignment: u64) -> Result<Vec<u8>> {
    let (width, height) = encoding.effective_size(mip.width, mip.height);
    if matches!(encoding, Encoding::Pvrtc2 | Encoding::Pvrtc4) {
        ensure!(
            width.is_power_of_two() && height.is_power_of_two(),
            "PVRTC decoding requires power-of-two dimensions"
        );
    }
    ensure!(
        u64::from(width) * u64::from(height) <= MAX_PIXELS,
        "This mip exceeds the 4 million pixel decode limit; select a smaller mip"
    );
    let length = encoding.bytes(mip.width, mip.height, alignment)?;
    ensure!(
        length <= MAX_DATA && length <= mip.length,
        "Texture mip exceeds the 16 MiB read limit or is truncated"
    );
    let mut source = vec![0; length as usize];
    read_at(file, mip.offset, &mut source)?;
    let width = width as usize;
    let height = height as usize;
    let mut image = vec![0u32; width * height];
    if let Some(channels) = encoding.channels() {
        let stride = (width * channels as usize).div_ceil(alignment as usize) * alignment as usize;
        for (row, pixels) in image.chunks_exact_mut(width).enumerate() {
            for (column, pixel) in pixels.iter_mut().enumerate() {
                let start = row * stride + column * channels as usize;
                let bytes = source
                    .get(start..start + channels as usize)
                    .context("Truncated texture pixel")?;
                let color = match encoding {
                    Encoding::Rgb => [bytes[2], bytes[1], bytes[0], 255],
                    Encoding::Rgba => [bytes[2], bytes[1], bytes[0], bytes[3]],
                    Encoding::Bgra => [bytes[0], bytes[1], bytes[2], bytes[3]],
                    Encoding::Red => [bytes[0], bytes[0], bytes[0], 255],
                    Encoding::Rg => [0, bytes[1], bytes[0], 255],
                    _ => bail!("Compressed texture was assigned an uncompressed channel layout"),
                };
                *pixel = u32::from_le_bytes(color);
            }
        }
    } else {
        let result = match encoding {
            Encoding::Bc1 => texture2ddecoder::decode_bc1(&source, width, height, &mut image),
            Encoding::Bc1Alpha => texture2ddecoder::decode_bc1a(&source, width, height, &mut image),
            Encoding::Bc2 => texture2ddecoder::decode_bc2(&source, width, height, &mut image),
            Encoding::Bc3 => texture2ddecoder::decode_bc3(&source, width, height, &mut image),
            Encoding::Bc4 => texture2ddecoder::decode_bc4(&source, width, height, &mut image),
            Encoding::Bc5 => texture2ddecoder::decode_bc5(&source, width, height, &mut image),
            Encoding::Bc6(signed) => {
                texture2ddecoder::decode_bc6(&source, width, height, &mut image, signed)
            }
            Encoding::Bc7 => texture2ddecoder::decode_bc7(&source, width, height, &mut image),
            Encoding::Etc1 => texture2ddecoder::decode_etc1(&source, width, height, &mut image),
            Encoding::Etc2 => texture2ddecoder::decode_etc2_rgb(&source, width, height, &mut image),
            Encoding::Etc2Alpha1 => {
                texture2ddecoder::decode_etc2_rgba1(&source, width, height, &mut image)
            }
            Encoding::Etc2Alpha8 => {
                texture2ddecoder::decode_etc2_rgba8(&source, width, height, &mut image)
            }
            Encoding::EacRed => texture2ddecoder::decode_eacr(&source, width, height, &mut image),
            Encoding::EacRg => texture2ddecoder::decode_eacrg(&source, width, height, &mut image),
            Encoding::Pvrtc2 => {
                texture2ddecoder::decode_pvrtc_2bpp(&source, width, height, &mut image)
            }
            Encoding::Pvrtc4 => {
                texture2ddecoder::decode_pvrtc_4bpp(&source, width, height, &mut image)
            }
            Encoding::Astc(block_width, block_height) => texture2ddecoder::decode_astc(
                &source,
                width,
                height,
                block_width as usize,
                block_height as usize,
                &mut image,
            ),
            _ => bail!("Uncompressed texture was assigned a block decoder"),
        };
        result.map_err(anyhow::Error::msg)?;
    }
    drop(source);
    let mut rgba = Vec::with_capacity(mip.width as usize * mip.height as usize * 4);
    for row in image.chunks_exact(width).take(mip.height as usize) {
        for pixel in row.iter().take(mip.width as usize) {
            let [blue, green, red, alpha] = pixel.to_le_bytes();
            rgba.extend_from_slice(&[red, green, blue, alpha]);
        }
    }
    drop(image);
    let image = RgbaImage::from_raw(mip.width, mip.height, rgba)
        .context("Invalid decoded texture dimensions")?;
    crate::media::png(DynamicImage::ImageRgba8(image))
}

fn open_texture(path: &Path) -> Result<(File, Texture)> {
    let mut file = File::open(path)?;
    ensure!(
        file.metadata()?.is_file(),
        "Texture preview requires a regular file"
    );
    let mut signature = [0; 12];
    read_at(&mut file, 0, &mut signature)?;
    let texture = if &signature == KTX1 {
        ktx1(&mut file)?
    } else if &signature == KTX2 {
        ktx2(&mut file)?
    } else {
        pvr(&mut file)?
    };
    Ok((file, texture))
}

fn worker_image(path: &Path, index: usize) -> Result<Vec<u8>> {
    let (mut file, texture) = open_texture(path)?;
    ensure!(texture.depth == 1, "Volume texture decoding is unavailable");
    let mip = texture
        .levels
        .get(index)
        .context("Texture mip is missing")?;
    let encoding = texture
        .encoding
        .context("This texture requires a transcoder")?;
    decode(&mut file, mip, encoding, texture.alignment)
}

pub fn run_texture_worker_if_invoked() {
    let mut arguments = std::env::args_os().skip(1);
    if arguments.next().as_deref() != Some(std::ffi::OsStr::new(WORKER_ARGUMENT)) {
        return;
    }
    let result = (|| -> Result<()> {
        let path = PathBuf::from(
            arguments
                .next()
                .context("Texture worker requires a source path")?,
        );
        let index = arguments
            .next()
            .context("Texture worker requires a mip index")?
            .into_string()
            .map_err(|_| anyhow::anyhow!("Invalid texture mip index"))?
            .parse::<usize>()?;
        ensure!(
            arguments.next().is_none(),
            "Unexpected texture worker argument"
        );
        let png = worker_image(&path, index)?;
        std::io::stdout().lock().write_all(&png)?;
        Ok(())
    })();
    match result {
        Ok(()) => std::process::exit(0),
        Err(error) => {
            eprintln!("Texture preview worker failed: {error:#}");
            std::process::exit(1);
        }
    }
}

pub(crate) fn read(path: &Path, request: &PreviewRequest) -> Result<PreviewPage> {
    let (mut file, texture) = open_texture(path)?;
    let size = file.metadata()?.len();
    let selected = request.section.as_deref().unwrap_or("Overview");
    let mut page = PreviewPage {
        title: selected.into(),
        sections: std::iter::once("Overview".into())
            .chain(
                texture
                    .levels
                    .iter()
                    .enumerate()
                    .map(|(level, _)| format!("Mip {level}")),
            )
            .collect(),
        metadata: vec![
            ("Container".into(), texture.container.into()),
            ("Format".into(), texture.format.clone()),
            (
                "Dimensions".into(),
                format!("{} × {}", texture.width, texture.height),
            ),
            ("Faces".into(), texture.faces.to_string()),
            ("Layers".into(), texture.layers.to_string()),
            ("Depth".into(), texture.depth.to_string()),
            ("Mip levels".into(), texture.declared_levels.to_string()),
            ("File bytes".into(), size.to_string()),
        ],
        columns: vec![
            "Mip".into(),
            "Dimensions".into(),
            "Offset".into(),
            "Stored bytes".into(),
        ],
        note: texture.note,
        ..Default::default()
    };
    if texture.encoding.is_none() {
        let mut hex = crate::read_bytes(path, &PreviewRequest::default())?;
        hex.metadata.extend(page.metadata);
        hex.note = Some(format!(
            "Unsupported texture encoding {}. {} Showing raw file bytes from offset 0.",
            texture.format,
            page.note
                .as_deref()
                .unwrap_or("This pixel encoding requires a texture transcoder.")
        ));
        return Ok(hex);
    }
    if selected == "Overview" {
        page.rows = texture
            .levels
            .iter()
            .enumerate()
            .map(|(level, mip)| {
                vec![
                    level.to_string(),
                    format!("{} × {}", mip.width, mip.height),
                    mip.offset.to_string(),
                    mip.length.to_string(),
                ]
            })
            .collect();
    } else {
        let index = selected
            .strip_prefix("Mip ")
            .context("Unknown texture section")?
            .parse::<usize>()?;
        let mip = texture
            .levels
            .get(index)
            .context("Texture mip is missing")?;
        page.rows = vec![vec![
            index.to_string(),
            format!("{} × {}", mip.width, mip.height),
            mip.offset.to_string(),
            mip.length.to_string(),
        ]];
        if texture.depth > 1 {
            page.note = Some("Volume textures expose their mip index and source bytes; 3D slice decoding is unavailable.".into());
        } else if let Some(encoding) = texture.encoding {
            if u64::from(mip.width) * u64::from(mip.height) > MAX_PIXELS {
                page.note = Some("This mip exceeds the 4 million pixel decode limit. Select a smaller mip, or inspect Bytes.".into());
            } else if encoding.channels().is_some() {
                page.image = Some(decode(&mut file, mip, encoding, texture.alignment)?);
            } else {
                // Malformed block codecs can panic; Zed's panic hook aborts the GUI process.
                #[cfg(not(test))]
                {
                    let mut command = std::process::Command::new(std::env::current_exe()?);
                    command
                        .arg(WORKER_ARGUMENT)
                        .arg(path)
                        .arg(index.to_string());
                    page.image = Some(crate::media::capture(&mut command, 8 * 1024 * 1024)?);
                }
                #[cfg(test)]
                {
                    page.image = Some(worker_image(path, index)?);
                }
            }
        }
    }
    if (texture.faces > 1 || texture.layers > 1) && page.image.is_some() {
        page.note = Some("The first face and first layer of the selected mip are displayed. Other surface data remains available in Bytes.".into());
    }
    Ok(page)
}

#[cfg(test)]
#[path = "texture_tests.rs"]
mod tests;
