//! Preview decoder for Photoshop PSD/PSB files (from ab2). Reads only the merged
//! ("composite") image at the end of the file, sampling rows and columns so a
//! huge document never has to be decoded at full size. Falls back to the
//! embedded JPEG thumbnail when the composite cannot be read.

use image::{DynamicImage, RgbaImage};
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::Path;

const ROW_BYTES: usize = 8 * 1024 * 1024;

fn block_end(start: u64, length: u64, limit: u64) -> Result<u64, String> {
    start
        .checked_add(length)
        .filter(|end| *end <= limit)
        .ok_or_else(|| "Photoshop block extends beyond its declared section or source file.".into())
}

fn skip<R: Seek>(reader: &mut R, length: u64, limit: u64) -> Result<(), String> {
    let start = reader.stream_position().map_err(io)?;
    let end = block_end(start, length, limit)?;
    reader.seek(SeekFrom::Start(end)).map_err(io)?;
    Ok(())
}

pub struct Decoded {
    pub image: DynamicImage,
    pub width: u32,
    pub height: u32,
    #[allow(dead_code)] // P4J's preview does not show it; ab2's does.
    pub bits_per_pixel: u16,
    /// "composite" or "thumbnail"
    pub source: &'static str,
}

struct Header {
    psb: bool,
    channels: usize,
    height: usize,
    width: usize,
    depth: usize,
    mode: u16,
}

fn u16be<R: Read>(r: &mut R) -> std::io::Result<u16> {
    let mut b = [0u8; 2];
    r.read_exact(&mut b)?;
    Ok(u16::from_be_bytes(b))
}

fn u32be<R: Read>(r: &mut R) -> std::io::Result<u32> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b)?;
    Ok(u32::from_be_bytes(b))
}

fn u64be<R: Read>(r: &mut R) -> std::io::Result<u64> {
    let mut b = [0u8; 8];
    r.read_exact(&mut b)?;
    Ok(u64::from_be_bytes(b))
}

fn io(e: std::io::Error) -> String {
    format!("Unable to read the PSD file: {e}")
}

/// PackBits (Apple RLE) as used by Photoshop.
fn unpack_bits(src: &[u8], out: &mut Vec<u8>, want: usize) {
    out.clear();
    let mut i = 0;
    while i < src.len() && out.len() < want {
        let n = src[i] as i8;
        i += 1;
        if n >= 0 {
            let len = n as usize + 1;
            let end = (i + len).min(src.len());
            out.extend_from_slice(&src[i..end]);
            i = end;
        } else if n != -128 {
            let len = (1 - n as isize) as usize;
            if i < src.len() {
                out.resize(out.len() + len, src[i]);
            }
            i += 1;
        }
    }
    out.resize(want, 0);
}

/// Reads one 8-bit sample of column `x` from a decoded row.
fn sample(row: &[u8], x: usize, depth: usize) -> u8 {
    match depth {
        1 => {
            // Bitmap mode: 1 = black.
            if row[x / 8] & (0x80 >> (x % 8)) != 0 {
                0
            } else {
                255
            }
        }
        16 => row[x * 2],
        32 => {
            let v =
                f32::from_be_bytes([row[x * 4], row[x * 4 + 1], row[x * 4 + 2], row[x * 4 + 3]]);
            (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u8
        }
        _ => row[x],
    }
}

pub fn decode(path: &Path, max_px: u32) -> Result<Decoded, String> {
    let file = File::open(path).map_err(io)?;
    let source_size = file.metadata().map_err(io)?.len();
    let mut r = BufReader::new(file);
    let mut sig = [0u8; 4];
    r.read_exact(&mut sig).map_err(io)?;
    if &sig != b"8BPS" {
        return Err("Not a Photoshop file (no 8BPS signature).".into());
    }
    let version = u16be(&mut r).map_err(io)?;
    if version != 1 && version != 2 {
        return Err(format!("Unsupported PSD version {version}."));
    }
    r.seek(SeekFrom::Current(6)).map_err(io)?;
    let h = Header {
        psb: version == 2,
        channels: u16be(&mut r).map_err(io)? as usize,
        height: u32be(&mut r).map_err(io)? as usize,
        width: u32be(&mut r).map_err(io)? as usize,
        depth: u16be(&mut r).map_err(io)? as usize,
        mode: u16be(&mut r).map_err(io)?,
    };
    if h.width > 300_000
        || h.height > 300_000
        || h.channels > 56
        || h.width.saturating_mul(h.depth / 8) > 4 * 1024 * 1024
        || h.channels.saturating_mul(h.height) > 1_000_000
    {
        return Err("Photoshop dimensions exceed the preview memory budget.".into());
    }
    if h.width == 0 || h.height == 0 || h.channels == 0 {
        return Err("The PSD file has no pixels.".into());
    }

    let palette_len = u32be(&mut r).map_err(io)? as usize;
    block_end(
        r.stream_position().map_err(io)?,
        palette_len as u64,
        source_size,
    )?;
    let mut palette = vec![0u8; palette_len.min(1 << 20)];
    r.read_exact(&mut palette).map_err(io)?;
    skip(&mut r, (palette_len - palette.len()) as u64, source_size)?;

    let res_len = u32be(&mut r).map_err(io)? as u64;
    let res_start = r.stream_position().map_err(io)?;
    let resource_end = block_end(res_start, res_len, source_size)?;
    r.seek(SeekFrom::Start(resource_end)).map_err(io)?;

    // Layer and mask section: skipped, except for the sign of the layer count,
    // which says whether the first extra channel is the merged transparency.
    let lm_len = if h.psb {
        u64be(&mut r).map_err(io)?
    } else {
        u32be(&mut r).map_err(io)? as u64
    };
    let lm_start = r.stream_position().map_err(io)?;
    let lm_end = block_end(lm_start, lm_len, source_size)?;
    let mut merged_alpha = false;
    if lm_len >= if h.psb { 10 } else { 6 } {
        let layer_info_len = if h.psb {
            u64be(&mut r).map_err(io)?
        } else {
            u32be(&mut r).map_err(io)? as u64
        };
        block_end(r.stream_position().map_err(io)?, layer_info_len, lm_end)?;
        if layer_info_len >= 2 {
            merged_alpha = (u16be(&mut r).map_err(io)? as i16) < 0;
        }
    }
    r.seek(SeekFrom::Start(lm_end)).map_err(io)?;

    match composite(&mut r, &h, &palette, merged_alpha, max_px) {
        Ok((image, bpp)) => Ok(Decoded {
            image,
            width: h.width as u32,
            height: h.height as u32,
            bits_per_pixel: bpp,
            source: "composite",
        }),
        Err(e) => {
            let image = thumbnail(&mut r, res_start, res_len)
                .map_err(|t| format!("{e}; the embedded thumbnail is unavailable too: {t}"))?;
            Ok(Decoded {
                image,
                width: h.width as u32,
                height: h.height as u32,
                bits_per_pixel: (h.depth * h.channels.min(4)) as u16,
                source: "thumbnail",
            })
        }
    }
}

fn composite<R: Read + Seek>(
    r: &mut R,
    h: &Header,
    palette: &[u8],
    merged_alpha: bool,
    max_px: u32,
) -> Result<(DynamicImage, u16), String> {
    let compression = u16be(r).map_err(|_| {
        "The file stores no composite image (saved without Maximize Compatibility?).".to_string()
    })?;
    let row_bytes = match h.depth {
        1 => h.width.div_ceil(8),
        8 | 16 | 32 => h.width * h.depth / 8,
        d => return Err(format!("{d}-bit color is not supported.")),
    };

    // Which composite channels are needed, per color mode.
    let (color_chans, kind): (usize, &str) = match h.mode {
        3 => (3, "rgb"),
        4 => (4, "cmyk"),
        2 => (1, "indexed"),
        0 => (1, "gray"),
        _ => (1, "gray"), // grayscale, duotone, multichannel, Lab (lightness only)
    };
    if h.channels < color_chans {
        return Err(format!("Too few channels ({}).", h.channels));
    }
    let alpha = merged_alpha && h.channels > color_chans;
    let used = color_chans + usize::from(alpha);

    // Sample every `step`-th row/column so the intermediate stays near 2x the preview size.
    let longest = h.width.max(h.height) as u32;
    let step = longest.div_ceil(2 * max_px.max(1)).max(1) as usize;
    let (ow, oh) = (h.width.div_ceil(step), h.height.div_ceil(step));

    let data_start = r.stream_position().map_err(|e| e.to_string())?;
    let source_size = r.seek(SeekFrom::End(0)).map_err(io)?;
    r.seek(SeekFrom::Start(data_start)).map_err(io)?;
    // Byte offset (from the start of the pixel data) and length of each channel row.
    let row_at: Box<dyn Fn(usize, usize) -> (u64, usize)> = match compression {
        0 => {
            let length = h
                .channels
                .checked_mul(h.height)
                .and_then(|rows| rows.checked_mul(row_bytes))
                .ok_or_else(|| {
                    "Photoshop composite size exceeds the preview budget.".to_string()
                })?;
            block_end(data_start, length as u64, source_size)?;
            Box::new(move |c, y| (((c * h.height + y) * row_bytes) as u64, row_bytes))
        }
        1 => {
            let n = h.channels * h.height;
            let table = (n * if h.psb { 4 } else { 2 }) as u64;
            block_end(data_start, table, source_size)?;
            let mut counts = Vec::with_capacity(n);
            for _ in 0..n {
                let length = if h.psb {
                    u32be(r).map_err(|e| e.to_string())? as usize
                } else {
                    u16be(r).map_err(|e| e.to_string())? as usize
                };
                if length > ROW_BYTES {
                    return Err("Photoshop compressed row exceeds the 8 MiB preview budget.".into());
                }
                counts.push(length);
            }
            let mut offsets = Vec::with_capacity(n + 1);
            let mut acc = table;
            for c in &counts {
                offsets.push(acc);
                acc = block_end(acc, *c as u64, source_size.saturating_sub(data_start))?;
            }
            Box::new(move |c, y| (offsets[c * h.height + y], counts[c * h.height + y]))
        }
        c => {
            return Err(format!(
                "The composite image uses unsupported compression {c}."
            ));
        }
    };

    let mut planes = vec![vec![0u8; ow * oh]; used];
    let mut raw = Vec::new();
    let mut row = Vec::new();
    for (ci, plane) in planes.iter_mut().enumerate() {
        for oy in 0..oh {
            let (off, len) = row_at(ci, oy * step);
            if len > ROW_BYTES {
                return Err("Photoshop compressed row exceeds the 8 MiB preview budget.".into());
            }
            let start = block_end(data_start, off, source_size)?;
            block_end(start, len as u64, source_size)?;
            r.seek(SeekFrom::Start(start)).map_err(io)?;
            raw.resize(len, 0);
            r.read_exact(&mut raw)
                .map_err(|e| format!("The composite image is incomplete: {e}"))?;
            let src: &[u8] = if compression == 1 {
                unpack_bits(&raw, &mut row, row_bytes);
                &row
            } else {
                &raw
            };
            for ox in 0..ow {
                plane[oy * ow + ox] = sample(src, ox * step, h.depth);
            }
        }
    }

    let mut img = RgbaImage::new(ow as u32, oh as u32);
    for (i, px) in img.pixels_mut().enumerate() {
        let a = if alpha { planes[color_chans][i] } else { 255 };
        px.0 = match kind {
            "rgb" => [planes[0][i], planes[1][i], planes[2][i], a],
            "cmyk" => {
                // Photoshop stores CMYK inverted (255 = no ink).
                let k = planes[3][i] as u16;
                let f = |v: u8| ((v as u16 * k) / 255) as u8;
                [f(planes[0][i]), f(planes[1][i]), f(planes[2][i]), a]
            }
            "indexed" if palette.len() >= 768 => {
                let v = planes[0][i] as usize;
                [palette[v], palette[256 + v], palette[512 + v], a]
            }
            _ => [planes[0][i], planes[0][i], planes[0][i], a],
        };
    }
    Ok((DynamicImage::ImageRgba8(img), (h.depth * used) as u16))
}

pub struct Layer {
    pub name: String,
    pub hidden: bool,
    /// Group nesting, 0 = top level.
    pub depth: usize,
    pub group: bool,
}

/// Layer names from the layer records (top to bottom, as the Layers panel shows them).
/// Reads only the records at the start of the layer section, never the pixel data.
pub fn layers(path: &Path) -> Result<Vec<Layer>, String> {
    let file = File::open(path).map_err(io)?;
    let source_size = file.metadata().map_err(io)?.len();
    let mut r = BufReader::new(file);
    let mut sig = [0u8; 4];
    r.read_exact(&mut sig).map_err(io)?;
    if &sig != b"8BPS" {
        return Err("Not a Photoshop file.".into());
    }
    let psb = u16be(&mut r).map_err(io)? == 2;
    r.seek(SeekFrom::Start(26)).map_err(io)?;
    let length = u64::from(u32be(&mut r).map_err(io)?);
    skip(&mut r, length, source_size)?;
    let length = u64::from(u32be(&mut r).map_err(io)?);
    skip(&mut r, length, source_size)?;
    let len = |r: &mut BufReader<File>| -> std::io::Result<u64> {
        if psb {
            u64be(r)
        } else {
            u32be(r).map(u64::from)
        }
    };
    let lm_len = len(&mut r).map_err(io)?;
    let lm_end = block_end(r.stream_position().map_err(io)?, lm_len, source_size)?;
    if lm_len == 0 {
        return Ok(Vec::new());
    }
    let li_len = len(&mut r).map_err(io)?;
    block_end(r.stream_position().map_err(io)?, li_len, lm_end)?;
    if li_len == 0 {
        // 16/32-bit documents keep the layer info in an Lr16/Lr32 block after the global mask.
        let mask = u64::from(u32be(&mut r).map_err(io)?);
        skip(&mut r, mask, lm_end)?;
        let mut found = false;
        while lm_end.saturating_sub(r.stream_position().map_err(io)?) >= 12 {
            let mut head = [0u8; 8];
            r.read_exact(&mut head).map_err(io)?;
            if &head[..4] != b"8BIM" && &head[..4] != b"8B64" {
                break;
            }
            let key = &head[4..];
            let size = if psb
                && matches!(
                    key,
                    b"Lr16"
                        | b"Lr32"
                        | b"Layr"
                        | b"LMsk"
                        | b"Mt16"
                        | b"Mt32"
                        | b"Mtrn"
                        | b"Alph"
                        | b"FMsk"
                        | b"lnk2"
                        | b"FEid"
                        | b"FXid"
                        | b"PxSD"
                ) {
                u64be(&mut r).map_err(io)?
            } else {
                u32be(&mut r).map_err(io)? as u64
            };
            let at = r.stream_position().map_err(io)?;
            let end = block_end(at, size, lm_end)?;
            if matches!(key, b"Lr16" | b"Lr32" | b"Layr") && size >= 2 {
                found = true;
                break;
            }
            let end = block_end(end, size % 2, lm_end)?;
            r.seek(SeekFrom::Start(end)).map_err(io)?;
        }
        if !found {
            return Ok(Vec::new());
        }
    }
    let count = (u16be(&mut r).map_err(io)? as i16).unsigned_abs() as usize;

    // Records, bottom layer first.
    if count > 4096 {
        return Err("Photoshop layers exceed the preview budget.".into());
    }
    let mut recs = Vec::with_capacity(count);
    for _ in 0..count {
        skip(&mut r, 16, lm_end)?;
        let chans = u64::from(u16be(&mut r).map_err(io)?);
        skip(
            &mut r,
            chans * if psb { 10 } else { 6 } + 4 + 4 + 1 + 1,
            lm_end,
        )?;
        let mut fl = [0u8; 2];
        r.read_exact(&mut fl).map_err(io)?; // flags, filler
        let extra = u32be(&mut r).map_err(io)? as u64;
        let extra_start = r.stream_position().map_err(io)?;
        let end = block_end(extra_start, extra, lm_end)?;
        let mask = u64::from(u32be(&mut r).map_err(io)?);
        skip(&mut r, mask, end)?;
        let ranges = u64::from(u32be(&mut r).map_err(io)?);
        skip(&mut r, ranges, end)?;
        let mut n = [0u8; 1];
        r.read_exact(&mut n).map_err(io)?;
        let mut raw = vec![0u8; n[0] as usize];
        r.read_exact(&mut raw).map_err(io)?;
        let pad = (4 - (1 + raw.len()) % 4) % 4;
        skip(&mut r, pad as u64, end)?;
        let mut name = encoding_rs::GBK.decode(&raw).0.into_owned();
        let mut divider = 0u32;
        // Additional layer information: unicode name (luni) and group markers (lsct/lsdk).
        while end.saturating_sub(r.stream_position().map_err(io)?) >= 12 {
            let mut head = [0u8; 8];
            r.read_exact(&mut head).map_err(io)?;
            if &head[..4] != b"8BIM" && &head[..4] != b"8B64" {
                break;
            }
            let key = &head[4..];
            let long = psb
                && matches!(
                    key,
                    b"LMsk"
                        | b"Lr16"
                        | b"Lr32"
                        | b"Layr"
                        | b"Mt16"
                        | b"Mt32"
                        | b"Mtrn"
                        | b"Alph"
                        | b"FMsk"
                        | b"lnk2"
                        | b"FEid"
                        | b"FXid"
                        | b"PxSD"
                );
            let size = if long {
                u64be(&mut r).map_err(io)?
            } else {
                u32be(&mut r).map_err(io)? as u64
            };
            let at = r.stream_position().map_err(io)?;
            let additional_end = block_end(at, size, end)?;
            match key {
                b"luni" if size >= 4 => {
                    let chars = u32be(&mut r).map_err(io)? as usize;
                    if chars > 4096 {
                        return Err("Photoshop layer names exceed the preview budget.".into());
                    }
                    let mut u = vec![0u8; (chars * 2).min(size as usize - 4)];
                    r.read_exact(&mut u).map_err(io)?;
                    name = encoding_rs::UTF_16BE
                        .decode_without_bom_handling(&u)
                        .0
                        .trim_end_matches('\0')
                        .to_string();
                }
                b"lsct" | b"lsdk" if size >= 4 => divider = u32be(&mut r).map_err(io)?,
                _ => {}
            }
            r.seek(SeekFrom::Start(additional_end)).map_err(io)?;
        }
        r.seek(SeekFrom::Start(end)).map_err(io)?;
        recs.push((name, fl[0] & 2 != 0, divider));
    }

    let mut out = Vec::with_capacity(recs.len());
    let mut depth = 0usize;
    for (name, hidden, divider) in recs.into_iter().rev() {
        match divider {
            1 | 2 => {
                out.push(Layer {
                    name,
                    hidden,
                    depth,
                    group: true,
                });
                depth += 1;
            }
            3 => depth = depth.saturating_sub(1),
            _ => out.push(Layer {
                name,
                hidden,
                depth,
                group: false,
            }),
        }
    }
    Ok(out)
}

/// Image resource 1036 (or the older 1033) holds a JPEG thumbnail.
fn thumbnail<R: Read + Seek>(r: &mut R, start: u64, len: u64) -> Result<DynamicImage, String> {
    let source_size = r.seek(SeekFrom::End(0)).map_err(io)?;
    let end = block_end(start, len, source_size)?;
    r.seek(SeekFrom::Start(start)).map_err(|e| e.to_string())?;
    while end.saturating_sub(r.stream_position().map_err(io)?) >= 12 {
        let mut sig = [0u8; 4];
        r.read_exact(&mut sig).map_err(|e| e.to_string())?;
        if &sig != b"8BIM" {
            break;
        }
        let id = u16be(r).map_err(|e| e.to_string())?;
        let mut n = [0u8; 1];
        r.read_exact(&mut n).map_err(|e| e.to_string())?;
        let name_len = n[0] as i64;
        // Pascal string, padded so length byte + text is even.
        skip(
            r,
            (name_len + if name_len % 2 == 0 { 1 } else { 0 }) as u64,
            end,
        )?;
        let size = u32be(r).map_err(|e| e.to_string())? as u64;
        let data_at = r.stream_position().map_err(|e| e.to_string())?;
        let data_end = block_end(data_at, size, end)?;
        if (id == 1036 || id == 1033) && size > 28 {
            let jpeg_start = block_end(data_at, 28, data_end)?;
            r.seek(SeekFrom::Start(jpeg_start)).map_err(io)?;
            if size > 8 * 1024 * 1024 {
                return Err("Photoshop thumbnail exceeds the preview budget.".into());
            }
            let mut jpeg = vec![0u8; (size - 28) as usize];
            r.read_exact(&mut jpeg).map_err(|e| e.to_string())?;
            let mut reader = image::ImageReader::with_format(
                std::io::Cursor::new(&jpeg),
                image::ImageFormat::Jpeg,
            );
            let mut limits = image::Limits::default();
            limits.max_alloc = Some(64 * 1024 * 1024);
            limits.max_image_width = Some(8192);
            limits.max_image_height = Some(8192);
            reader.limits(limits);
            let img = reader.decode().map_err(|e| e.to_string())?;
            // 1033 thumbnails are stored BGR.
            return Ok(if id == 1033 {
                let mut rgb = img.to_rgb8();
                for p in rgb.pixels_mut() {
                    p.0.swap(0, 2);
                }
                DynamicImage::ImageRgb8(rgb)
            } else {
                img
            });
        }
        let padded_end = block_end(data_end, size % 2, end)?;
        r.seek(SeekFrom::Start(padded_end)).map_err(io)?;
    }
    Err("No embedded thumbnail.".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::{Context as _, Result};
    use std::io::Cursor;

    fn psb(resources: &[u8], layer_bytes: u64, row_counts: &[u32], data: &[u8]) -> Vec<u8> {
        let mut source = Vec::from(&b"8BPS"[..]);
        source.extend_from_slice(&2u16.to_be_bytes());
        source.extend_from_slice(&[0; 6]);
        source.extend_from_slice(&3u16.to_be_bytes());
        source.extend_from_slice(&1u32.to_be_bytes());
        source.extend_from_slice(&1u32.to_be_bytes());
        source.extend_from_slice(&8u16.to_be_bytes());
        source.extend_from_slice(&3u16.to_be_bytes());
        source.extend_from_slice(&0u32.to_be_bytes());
        source.extend_from_slice(&(resources.len() as u32).to_be_bytes());
        source.extend_from_slice(resources);
        source.extend_from_slice(&layer_bytes.to_be_bytes());
        source.extend_from_slice(&1u16.to_be_bytes());
        for count in row_counts {
            source.extend_from_slice(&count.to_be_bytes());
        }
        source.extend_from_slice(data);
        source
    }

    #[test]
    fn psb_oversized_compressed_row_fails_before_payload_allocation() -> Result<()> {
        let file = tempfile::NamedTempFile::new()?;
        let source = psb(&[], 0, &[u32::MAX, 0, 0], &[]);
        assert!(source.len() < 100);
        std::fs::write(file.path(), &source)?;
        let error = decode(file.path(), 1024)
            .err()
            .context("oversized PSB row")?;
        assert!(error.contains("compressed row exceeds the 8 MiB"));
        assert_eq!(std::fs::read(file.path())?, source);
        Ok(())
    }

    #[test]
    fn psb_row_extents_and_overflowing_layer_declarations_fail() -> Result<()> {
        let file = tempfile::NamedTempFile::new()?;
        std::fs::write(file.path(), psb(&[], 0, &[100, 0, 0], &[]))?;
        let error = decode(file.path(), 1024)
            .err()
            .context("truncated PSB row")?;
        assert!(error.contains("extends beyond"));
        std::fs::write(file.path(), psb(&[], u64::MAX, &[], &[]))?;
        assert!(
            decode(file.path(), 1024)
                .err()
                .context("layer overflow")?
                .contains("extends beyond")
        );
        assert!(
            layers(file.path())
                .err()
                .context("layer index overflow")?
                .contains("extends beyond")
        );
        Ok(())
    }

    #[test]
    fn psb_bounded_rle_preserves_pixels_and_oversized_row_uses_thumbnail() -> Result<()> {
        let file = tempfile::NamedTempFile::new()?;
        std::fs::write(file.path(), psb(&[], 0, &[2, 2, 2], &[0, 10, 0, 20, 0, 30]))?;
        let decoded = decode(file.path(), 1024).map_err(anyhow::Error::msg)?;
        assert_eq!(decoded.source, "composite");
        assert_eq!(
            decoded.image.to_rgba8().get_pixel(0, 0).0,
            [10, 20, 30, 255]
        );
        let mut jpeg = Cursor::new(Vec::new());
        DynamicImage::new_rgb8(1, 1).write_to(&mut jpeg, image::ImageFormat::Jpeg)?;
        let mut resource = Vec::from(&b"8BIM"[..]);
        resource.extend_from_slice(&1036u16.to_be_bytes());
        resource.extend_from_slice(&[0, 0]);
        resource.extend_from_slice(&(28 + jpeg.get_ref().len() as u32).to_be_bytes());
        resource.extend_from_slice(&[0; 28]);
        resource.extend_from_slice(jpeg.get_ref());
        resource.resize(resource.len().next_multiple_of(2), 0);
        std::fs::write(file.path(), psb(&resource, 0, &[u32::MAX, 0, 0], &[]))?;
        let decoded = decode(file.path(), 1024).map_err(anyhow::Error::msg)?;
        assert_eq!(decoded.source, "thumbnail");
        assert_eq!(decoded.image.width(), 1);
        assert_eq!(decoded.image.height(), 1);
        Ok(())
    }
}
