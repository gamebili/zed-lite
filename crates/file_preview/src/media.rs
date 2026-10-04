use crate::{CELL_BYTES, FileKind, PAGE_BYTES, PAGE_ROWS, PreviewPage, PreviewRequest};
use anyhow::{Context as _, Result, bail};
use image::{DynamicImage, ImageFormat, ImageReader, Limits};
use std::{
    fs::File,
    io::{BufReader, Cursor, Read, Write},
    path::Path,
    process::{Command, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

const IMAGE_BYTES: usize = 8 * 1024 * 1024;
const DECODE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_PIXELS: u64 = 16 * 1024 * 1024;
const IMAGE_EDGE: u32 = 1024;

pub(crate) fn png(image: DynamicImage) -> Result<Vec<u8>> {
    let image = if image.width() > IMAGE_EDGE || image.height() > IMAGE_EDGE {
        image.thumbnail(IMAGE_EDGE, IMAGE_EDGE)
    } else {
        image
    };
    let mut output = Cursor::new(Vec::new());
    image.write_to(&mut output, ImageFormat::Png)?;
    let bytes = output.into_inner();
    if bytes.len() > IMAGE_BYTES {
        bail!("Encoded image exceeds the preview budget");
    }
    Ok(bytes)
}

fn svg(path: &Path) -> Result<Vec<u8>> {
    const NODE_LIMIT: usize = 10_000;
    const GRADIENT_STOP_COPIES: usize = 128 * 1024;
    const STYLE_VALUE_COPIES: usize = 4 * 1024 * 1024;
    const PIXMAP_BYTES: u64 = 16 * 1024 * 1024;

    fn check_tag(
        tag: &quick_xml::events::BytesStart<'_>,
        node_count: &mut usize,
        gradient_stop_count: &mut usize,
        style_value_bytes: &mut usize,
    ) -> Result<()> {
        *node_count += 1;
        anyhow::ensure!(
            *node_count <= NODE_LIMIT,
            "SVG exceeds the preview complexity budget; use Bytes"
        );
        let name = tag.local_name();
        anyhow::ensure!(
            !matches!(
                name.as_ref(),
                b"clipPath" | b"filter" | b"mask" | b"use" | b"pattern" | b"marker"
            ),
            "SVG reference/filter allocation exceeds the preview budget; use Bytes"
        );
        *gradient_stop_count += usize::from(name.as_ref() == b"stop");
        for attribute in tag.attributes() {
            let attribute = attribute?;
            if matches!(
                attribute.key.local_name().as_ref(),
                b"style" | b"stroke-dasharray" | b"font-family"
            ) {
                *style_value_bytes += attribute.value.len();
            }
        }
        Ok(())
    }

    fn check_groups(
        group: &resvg::usvg::Group,
        scale: f32,
        canvas_width: u32,
        canvas_height: u32,
        allocated_bytes: &mut u64,
    ) -> Result<()> {
        if group.should_isolate() {
            let bounds = group.abs_layer_bounding_box();
            let width = f64::from(bounds.width()) * f64::from(scale);
            let height = f64::from(bounds.height()) * f64::from(scale);
            anyhow::ensure!(
                width.is_finite() && height.is_finite(),
                "SVG isolated-group bounds exceed the preview budget; use Bytes"
            );
            // resvg expands unfiltered layer bounds by four pixels and clips them
            // to a region five times the canvas in each dimension. Ignore position
            // to overestimate offscreen layers instead of undercounting nested ones.
            let width = (width.ceil() + 4.0).min(f64::from(canvas_width) * 5.0) as u64;
            let height = (height.ceil() + 4.0).min(f64::from(canvas_height) * 5.0) as u64;
            *allocated_bytes = allocated_bytes
                .checked_add(width * height * 4)
                .context("SVG pixmap size overflow")?;
            anyhow::ensure!(
                *allocated_bytes <= PIXMAP_BYTES,
                "SVG isolated groups exceed the 16 MiB preview pixel-memory budget; use Bytes"
            );
        }
        for node in group.children() {
            match node {
                resvg::usvg::Node::Group(group) => {
                    check_groups(group, scale, canvas_width, canvas_height, allocated_bytes)?;
                }
                resvg::usvg::Node::Text(text) => {
                    check_groups(
                        text.flattened(),
                        scale,
                        canvas_width,
                        canvas_height,
                        allocated_bytes,
                    )?;
                }
                _ => {}
            }
        }
        Ok(())
    }

    let mut source = Vec::new();
    File::open(path)?
        .take(PAGE_BYTES as u64 + 1)
        .read_to_end(&mut source)?;
    if source.len() > PAGE_BYTES {
        bail!("SVG exceeds the 2 MiB parsing budget; use Bytes");
    }
    let mut xml = quick_xml::Reader::from_reader(Cursor::new(&source));
    let mut buffer = Vec::new();
    let mut depth = 0usize;
    let mut node_count = 0usize;
    let mut gradient_stop_count = 0usize;
    let mut style_value_bytes = 0usize;
    let mut style_depth = None;
    loop {
        match xml.read_event_into(&mut buffer)? {
            quick_xml::events::Event::Start(tag) => {
                depth += 1;
                anyhow::ensure!(
                    depth <= 32,
                    "SVG exceeds the preview depth budget; use Bytes"
                );
                check_tag(
                    &tag,
                    &mut node_count,
                    &mut gradient_stop_count,
                    &mut style_value_bytes,
                )?;
                if tag.local_name().as_ref() == b"style" {
                    style_depth = Some(depth);
                }
            }
            quick_xml::events::Event::Empty(tag) => {
                check_tag(
                    &tag,
                    &mut node_count,
                    &mut gradient_stop_count,
                    &mut style_value_bytes,
                )?;
            }
            quick_xml::events::Event::End(_) => {
                if style_depth == Some(depth) {
                    style_depth = None;
                }
                depth = depth.saturating_sub(1);
            }
            quick_xml::events::Event::Text(text) if style_depth.is_some() => {
                style_value_bytes += text.len();
            }
            quick_xml::events::Event::CData(text) if style_depth.is_some() => {
                style_value_bytes += text.len();
            }
            quick_xml::events::Event::DocType(_) => {
                bail!("SVG DTD is not accepted in bounded previews")
            }
            quick_xml::events::Event::Eof => break,
            _ => {}
        }
        buffer.clear();
    }
    // objectBoundingBox gradients duplicate their stops for every painted object.
    // Include both paints and cached gradient definitions before building the tree.
    anyhow::ensure!(
        node_count
            .saturating_mul(gradient_stop_count)
            .saturating_mul(3)
            <= GRADIENT_STOP_COPIES,
        "SVG gradient expansion exceeds the preview memory budget; use Bytes"
    );
    // CSS values are copied per matching node; inherited dash arrays are parsed
    // into a separate number vector for every painted path.
    anyhow::ensure!(
        node_count.saturating_mul(style_value_bytes) <= STYLE_VALUE_COPIES,
        "SVG style expansion exceeds the preview memory budget; use Bytes"
    );
    let mut options = resvg::usvg::Options::default();
    options.image_href_resolver = resvg::usvg::ImageHrefResolver {
        resolve_string: Box::new(|_, _| None),
        resolve_data: Box::new(|_, _, _| None),
    };
    let tree = resvg::usvg::Tree::from_data(&source, &options)?;
    let size = tree.size();
    let scale = (IMAGE_EDGE as f32 / size.width().max(size.height())).min(1.0);
    let width = (size.width() * scale).ceil().max(1.0) as u32;
    let height = (size.height() * scale).ceil().max(1.0) as u32;
    let mut allocated_bytes = u64::from(width) * u64::from(height) * 4;
    check_groups(tree.root(), scale, width, height, &mut allocated_bytes)?;
    let mut pixmap =
        resvg::tiny_skia::Pixmap::new(width, height).context("Unable to allocate SVG preview")?;
    resvg::render(
        &tree,
        resvg::tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );
    Ok(pixmap.encode_png()?)
}

struct ImageInput {
    file: File,
    remaining: u64,
    deadline: Instant,
}

pub(crate) fn run_svg_worker_if_invoked() {
    let mut arguments = std::env::args_os().skip(1);
    if arguments.next().as_deref() != Some(std::ffi::OsStr::new("--zed-file-preview-svg-worker")) {
        return;
    }
    let result = (|| -> Result<()> {
        let path = arguments
            .next()
            .context("SVG worker requires a source path")?;
        anyhow::ensure!(arguments.next().is_none(), "Unexpected SVG worker argument");
        let bytes = svg(Path::new(&path))?;
        std::io::stdout().lock().write_all(&bytes)?;
        Ok(())
    })();
    match result {
        Ok(()) => std::process::exit(0),
        Err(error) => {
            eprintln!("SVG preview worker failed: {error:#}");
            std::process::exit(1);
        }
    }
}

impl ImageInput {
    fn new(path: &Path) -> Result<Self> {
        Ok(Self {
            file: File::open(path)?,
            remaining: 8 * 1024 * 1024,
            deadline: Instant::now() + Duration::from_secs(2),
        })
    }
}

impl Read for ImageInput {
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        if bytes.is_empty() {
            return Ok(0);
        }
        if self.remaining == 0 || Instant::now() > self.deadline {
            return Err(std::io::Error::other(
                "Image input exceeds the preview read/time budget",
            ));
        }
        let length = bytes.len().min(self.remaining as usize);
        let count = self.file.read(&mut bytes[..length])?;
        self.remaining -= count as u64;
        Ok(count)
    }
}

impl std::io::Seek for ImageInput {
    fn seek(&mut self, position: std::io::SeekFrom) -> std::io::Result<u64> {
        self.file.seek(position)
    }
}

fn native_image(path: &Path) -> Result<(Vec<u8>, u32, u32)> {
    let mut dimensions_reader =
        ImageReader::new(BufReader::new(ImageInput::new(path)?)).with_guessed_format()?;
    let mut probe_limits = Limits::default();
    probe_limits.max_alloc = Some(DECODE_BYTES);
    dimensions_reader.limits(probe_limits);
    let dimensions = dimensions_reader.into_dimensions()?;
    if u64::from(dimensions.0) * u64::from(dimensions.1) > MAX_PIXELS {
        bail!("Image dimensions exceed the 64 MiB decode budget; use Bytes to inspect the file");
    }
    let mut reader =
        ImageReader::new(BufReader::new(ImageInput::new(path)?)).with_guessed_format()?;
    let mut limits = Limits::default();
    limits.max_alloc = Some(DECODE_BYTES);
    limits.max_image_width = Some(32768);
    limits.max_image_height = Some(32768);
    reader.limits(limits);
    let decoded = reader.decode()?;
    Ok((png(decoded)?, dimensions.0, dimensions.1))
}

// This synchronous process is owned and polled on GPUI's background executor.
#[allow(clippy::disallowed_methods)]
pub(crate) fn capture(command: &mut Command, limit: usize) -> Result<Vec<u8>> {
    capture_with_timeout(command, limit, Duration::from_secs(10))
}

// Pipe readers may outlive the main decoder when it has spawned a descendant.
#[allow(clippy::disallowed_methods)]
fn capture_with_timeout(command: &mut Command, limit: usize, timeout: Duration) -> Result<Vec<u8>> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    crate::decoder_budget::configure(command);
    let mut child = command
        .spawn()
        .context("Preview decoder is unavailable; install the named tool or use Bytes")?;
    let mut containment = Some(
        match crate::decoder_budget::ProcessContainment::new(&child) {
            Ok(containment) => containment,
            Err(error) => {
                crate::decoder_budget::terminate(&mut child)?;
                return Err(error).context("Cannot contain the preview decoder");
            }
        },
    );
    let stdout = child.stdout.take().context("No decoder output pipe")?;
    let stderr = child.stderr.take().context("No decoder error pipe")?;
    let (sender, receiver) = mpsc::channel();
    let output_sender = sender.clone();
    let output_thread = thread::spawn(move || {
        let mut output = Vec::new();
        let result = stdout
            .take(limit as u64 + 1)
            .read_to_end(&mut output)
            .map(|_| output);
        if output_sender.send((true, result)).is_err() {
            return;
        }
    });
    let error_thread = thread::spawn(move || {
        let mut output = Vec::new();
        let result = stderr.take(65537).read_to_end(&mut output).map(|_| output);
        if sender.send((false, result)).is_err() {
            return;
        }
    });
    let deadline = Instant::now() + timeout;
    let mut output = None;
    let mut error = None;
    let mut failed = None;
    let status = loop {
        while let Ok((is_output, result)) = receiver.try_recv() {
            match result {
                Ok(bytes) => {
                    let cap = if is_output { limit } else { 65536 };
                    if bytes.len() > cap {
                        failed = Some("Decoder output exceeds the preview budget".to_owned());
                    }
                    if is_output {
                        output = Some(bytes);
                    } else {
                        error = Some(bytes);
                    }
                }
                Err(problem) => failed = Some(problem.to_string()),
            }
        }
        let process_status = child.try_wait()?;
        if process_status.is_none() {
            match crate::decoder_budget::memory_exceeded(&child) {
                Ok(true) => {
                    failed = Some("Decoder exceeded the 256 MiB resident-memory budget".into())
                }
                Ok(false) => {}
                Err(problem) => failed = Some(format!("Cannot monitor decoder memory: {problem}")),
            }
        }
        if Instant::now() >= deadline {
            failed = Some("Decoder exceeded the 10 second preview deadline".into());
        }
        if failed.is_some() {
            drop(containment.take());
            break crate::decoder_budget::terminate(&mut child)?;
        }
        if let Some(status) = process_status {
            if output.is_some() && error.is_some() {
                break status;
            }
        }
        thread::sleep(Duration::from_millis(10));
    };
    drop(containment);
    output_thread
        .join()
        .map_err(|_| anyhow::anyhow!("Decoder output reader failed"))?;
    error_thread
        .join()
        .map_err(|_| anyhow::anyhow!("Decoder error reader failed"))?;
    while let Ok((is_output, result)) = receiver.try_recv() {
        let bytes = result?;
        if bytes.len() > if is_output { limit } else { 65536 } {
            bail!("Decoder output exceeds the preview budget");
        }
        if is_output {
            output = Some(bytes);
        } else {
            error = Some(bytes);
        }
    }
    if let Some(problem) = failed {
        bail!(problem);
    }
    if !status.success() {
        let error = error.unwrap_or_default();
        let message = String::from_utf8_lossy(&error);
        bail!(
            "Preview decoder failed: {}",
            message.chars().take(1024).collect::<String>()
        );
    }
    Ok(output.unwrap_or_default())
}

fn ffmpeg(path: &Path, kind: FileKind, seconds: f64) -> Result<Vec<u8>> {
    let mut command = Command::new("ffmpeg");
    command.args([
        "-v",
        "error",
        "-nostdin",
        "-threads",
        "1",
        "-filter_threads",
        "1",
        "-filter_complex_threads",
        "1",
        "-max_alloc",
        "67108864",
        "-protocol_whitelist",
        "file,pipe,data",
    ]);
    if matches!(kind, FileKind::Video | FileKind::Audio) {
        command.args(["-ss", &seconds.to_string()]);
    }
    if kind == FileKind::Audio {
        // showwavespic emits only at EOF, so an output -t would still ingest the whole file.
        command.args(["-t", "10"]);
    }
    command.arg("-i").arg(path);
    if kind == FileKind::Audio {
        command.args([
            "-filter_complex",
            "[0:a:0]aresample=44100,aformat=sample_fmts=flt:channel_layouts=stereo,showwavespic=s=1024x200:colors=5599ff",
            "-frames:v",
            "1",
        ]);
    } else {
        command.args([
            "-vf",
            "scale=w='min(1024,iw)':h='min(1024,ih)':force_original_aspect_ratio=decrease",
            "-frames:v",
            "1",
        ]);
    }
    command.args([
        "-threads",
        "1",
        "-f",
        "image2pipe",
        "-vcodec",
        "png",
        "pipe:1",
    ]);
    let bytes = capture(&mut command, IMAGE_BYTES)?;
    validate_preview_image(&bytes)?;
    Ok(bytes)
}

fn validate_preview_image(bytes: &[u8]) -> Result<()> {
    let (width, height) = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()?
        .into_dimensions()?;
    if width > IMAGE_EDGE || height > IMAGE_EDGE {
        bail!("Decoder output exceeds preview pixel limits");
    }
    Ok(())
}

fn magick(path: &Path) -> Result<(Vec<u8>, u32, u32)> {
    let directory = tempfile::tempdir()?;
    // LibRaw or another native coder must decode in the monitored process;
    // delegate subprocess allocations would escape the resident-memory budget.
    std::fs::write(
        directory.path().join("policy.xml"),
        r#"<policymap>
<policy domain="delegate" rights="none" pattern="*"/>
<policy domain="filter" rights="none" pattern="*"/>
<policy domain="path" rights="none" pattern="@*"/>
<policy domain="coder" rights="none" pattern="{HTTP,HTTPS,URL,FTP,FTPS,EPHEMERAL,MSL,MVG}"/>
<policy domain="resource" name="memory" value="64MiB"/>
<policy domain="resource" name="map" value="0"/>
<policy domain="resource" name="disk" value="0"/>
<policy domain="resource" name="thread" value="1"/>
<policy domain="resource" name="area" value="16MP"/>
<policy domain="resource" name="width" value="32768"/>
<policy domain="resource" name="height" value="32768"/>
<policy domain="resource" name="list-length" value="16"/>
<policy domain="resource" name="time" value="10"/>
</policymap>"#,
    )?;
    let mut source = path.canonicalize()?.into_os_string();
    source.push("[0]");
    let configure = |command: &mut Command| {
        command
            .env("MAGICK_CONFIGURE_PATH", directory.path())
            .env("MAGICK_TEMPORARY_PATH", directory.path())
            .args([
                "-limit",
                "memory",
                "64MiB",
                "-limit",
                "map",
                "0",
                "-limit",
                "disk",
                "0",
                "-limit",
                "thread",
                "1",
                "-limit",
                "area",
                "16MP",
                "-limit",
                "width",
                "32768",
                "-limit",
                "height",
                "32768",
                "-limit",
                "list-length",
                "16",
                "-limit",
                "time",
                "10",
            ]);
    };
    let mut identify = Command::new("magick");
    identify.arg("identify");
    configure(&mut identify);
    identify.args(["-ping", "-format", "%w %h\n"]).arg(&source);
    let dimensions = capture(&mut identify, 1024)
        .context("ImageMagick cannot inspect this codec without an external delegate; use Bytes")?;
    let dimensions = String::from_utf8(dimensions)?;
    let mut dimensions = dimensions.split_whitespace();
    let width = dimensions
        .next()
        .context("ImageMagick returned no width")?
        .parse::<u32>()?;
    let height = dimensions
        .next()
        .context("ImageMagick returned no height")?
        .parse::<u32>()?;
    anyhow::ensure!(
        dimensions.next().is_none(),
        "ImageMagick returned multiple frames"
    );
    anyhow::ensure!(
        width > 0 && height > 0 && u64::from(width) * u64::from(height) <= MAX_PIXELS,
        "Image dimensions exceed the 64 MiB decode budget; use Bytes"
    );
    let mut command = Command::new("magick");
    configure(&mut command);
    command
        .arg(&source)
        .args(["-thumbnail", "1024x1024>", "-strip", "PNG:-"]);
    let bytes = capture(&mut command, IMAGE_BYTES)
        .context("ImageMagick cannot decode this codec within the memory budget without an external delegate; use Bytes")?;
    validate_preview_image(&bytes)?;
    Ok((bytes, width, height))
}

fn probe(path: &Path) -> Result<serde_json::Value> {
    let mut command = Command::new("ffprobe");
    command
        .args([
            "-v",
            "error",
            "-max_alloc",
            "67108864",
            "-protocol_whitelist",
            "file,pipe,data",
            "-show_format",
            "-show_streams",
            "-of",
            "json",
        ])
        .arg(path);
    let output = capture(&mut command, PAGE_BYTES)?;
    Ok(serde_json::from_slice(&output)?)
}

fn check_stream_pixels(metadata: &serde_json::Value) -> Result<bool> {
    let mut total_pixels = 0u64;
    let mut found_dimensions = false;
    if let Some(streams) = metadata
        .get("streams")
        .and_then(serde_json::Value::as_array)
    {
        for stream in streams {
            let video =
                stream.get("codec_type").and_then(serde_json::Value::as_str) == Some("video");
            if !video && stream.get("width").is_none() && stream.get("height").is_none() {
                continue;
            }
            let width = stream
                .get("width")
                .and_then(serde_json::Value::as_u64)
                .filter(|width| *width > 0)
                .context("Video/image width is unavailable or invalid; use Bytes")?;
            let height = stream
                .get("height")
                .and_then(serde_json::Value::as_u64)
                .filter(|height| *height > 0)
                .context("Video/image height is unavailable or invalid; use Bytes")?;
            let pixels = width
                .checked_mul(height)
                .context("Video/image pixel dimensions exceed the decode budget; use Bytes")?;
            total_pixels = total_pixels
                .checked_add(pixels)
                .context("Stream pixels exceed the 64 MiB decode budget; use Bytes")?;
            anyhow::ensure!(
                pixels <= MAX_PIXELS && total_pixels <= MAX_PIXELS,
                "Video/image streams exceed the 64 MiB decode pixel budget; use Bytes"
            );
            found_dimensions = true;
        }
    }
    Ok(found_dimensions)
}

fn check_audio_layout(metadata: &serde_json::Value) -> Result<()> {
    if let Some(streams) = metadata
        .get("streams")
        .and_then(serde_json::Value::as_array)
    {
        for stream in streams {
            if stream.get("codec_type").and_then(serde_json::Value::as_str) != Some("audio") {
                continue;
            }
            let sample_rate = stream
                .get("sample_rate")
                .and_then(|value| value.as_u64().or_else(|| value.as_str()?.parse().ok()));
            let channels = stream.get("channels").and_then(serde_json::Value::as_u64);
            anyhow::ensure!(
                sample_rate.is_some_and(|rate| (1..=384_000).contains(&rate))
                    && channels.is_some_and(|channels| (1..=32).contains(&channels)),
                "Audio stream exceeds the sample-rate/channel decode budget, or its layout is unavailable; use Bytes"
            );
        }
    }
    Ok(())
}

fn pdf(path: &Path, request: &PreviewRequest) -> Result<PreviewPage> {
    let mut info = Command::new("pdfinfo");
    info.arg(path);
    let information = capture(&mut info, 65536)?;
    let information = String::from_utf8(information)?;
    let count = information
        .lines()
        .find_map(|line| {
            line.strip_prefix("Pages:")
                .and_then(|value| value.trim().parse::<u64>().ok())
        })
        .context("PDF has no readable page count")?;
    let index = request.offset.min(count.saturating_sub(1));
    let directory = tempfile::tempdir()?;
    let prefix = directory.path().join("page");
    let page_number = (index + 1).to_string();
    let mut command = Command::new("pdftoppm");
    command
        .args([
            "-f",
            &page_number,
            "-l",
            &page_number,
            "-scale-to",
            "1024",
            "-singlefile",
            "-png",
        ])
        .arg(path)
        .arg(&prefix);
    capture(&mut command, 65536)?;
    let mut image = Vec::new();
    File::open(prefix.with_extension("png"))?
        .take(IMAGE_BYTES as u64 + 1)
        .read_to_end(&mut image)?;
    if image.len() > IMAGE_BYTES {
        bail!("PDF page exceeds the image budget");
    }
    validate_preview_image(&image)?;
    Ok(PreviewPage {
        title: format!("Page {} of {count}", index + 1),
        metadata: information
            .lines()
            .take(PAGE_ROWS)
            .filter_map(|line| {
                line.split_once(':')
                    .map(|(name, value)| (name.into(), value.trim().into()))
            })
            .collect(),
        image: Some(image),
        next_offset: (index + 1 < count).then_some(index + 1),
        ..Default::default()
    })
}

pub(crate) fn read(path: &Path, kind: FileKind, request: &PreviewRequest) -> Result<PreviewPage> {
    if kind == FileKind::Pdf {
        return pdf(path, request);
    }
    let mut page = PreviewPage {
        title: format!("{kind:?}"),
        metadata: vec![(
            "File size".into(),
            format!("{} bytes", path.metadata()?.len()),
        )],
        ..Default::default()
    };
    let extension = crate::extension(&path.to_string_lossy());
    if kind == FileKind::Image {
        if matches!(extension.as_str(), "ktx" | "ktx2" | "pvr") {
            return crate::texture::read(path, request);
        } else if extension == "svg" {
            let mut command = Command::new(std::env::current_exe()?);
            command.arg("--zed-file-preview-svg-worker").arg(path);
            let bytes = capture(&mut command, IMAGE_BYTES)?;
            validate_preview_image(&bytes)?;
            page.image = Some(bytes);
            page.note = Some("Embedded raster images, font text and external resources are omitted. Reference/filter structures that exceed the memory budget remain available in Bytes.".into());
        } else if matches!(extension.as_str(), "psd" | "psb") {
            page.sections = vec!["Composite".into(), "Layers".into()];
            if request.section.as_deref() == Some("Layers") {
                let layers = crate::psd::layers(path).map_err(anyhow::Error::msg)?;
                let offset = usize::try_from(request.offset).unwrap_or(usize::MAX);
                page.columns = vec![
                    "Layer".into(),
                    "Depth".into(),
                    "Visible".into(),
                    "Group".into(),
                ];
                page.rows = layers
                    .iter()
                    .skip(offset)
                    .take(PAGE_ROWS)
                    .map(|layer| {
                        vec![
                            layer.name.clone(),
                            layer.depth.to_string(),
                            (!layer.hidden).to_string(),
                            layer.group.to_string(),
                        ]
                    })
                    .collect();
                page.next_offset = (offset.saturating_add(page.rows.len()) < layers.len())
                    .then_some(offset.saturating_add(page.rows.len()) as u64);
                return Ok(page);
            }
            let decoded = crate::psd::decode(path, IMAGE_EDGE).map_err(anyhow::Error::msg)?;
            page.metadata.extend([
                (
                    "Dimensions".into(),
                    format!("{} × {}", decoded.width, decoded.height),
                ),
                ("Bits per pixel".into(), decoded.bits_per_pixel.to_string()),
                ("Image source".into(), decoded.source.into()),
            ]);
            page.image = Some(png(decoded.image)?);
        } else {
            match native_image(path) {
                Ok((bytes, width, height)) => {
                    page.image = Some(bytes);
                    page.metadata
                        .push(("Dimensions".into(), format!("{width} × {height}")));
                }
                Err(native_error) => {
                    // A recognized image over budget must not be retried using an unconstrained decoder.
                    if native_error.to_string().contains("budget")
                        || native_error.to_string().contains("limit")
                    {
                        return Err(native_error);
                    }
                    let ffmpeg_result = match probe(path) {
                        Ok(metadata) => {
                            if check_stream_pixels(&metadata)? {
                                ffmpeg(path, kind, 0.0)
                            } else {
                                Err(anyhow::anyhow!(
                                    "FFmpeg cannot inspect the source image dimensions"
                                ))
                            }
                        }
                        Err(error) => Err(error),
                    };
                    match ffmpeg_result {
                        Ok(bytes) => {
                            page.image = Some(bytes);
                            page.note = Some(format!("Converted with FFmpeg: {native_error}"));
                        }
                        Err(ffmpeg_error) => {
                            let (bytes, width, height) = magick(path).with_context(|| {
                                format!("Native decoder: {native_error}; FFmpeg: {ffmpeg_error}")
                            })?;
                            page.image = Some(bytes);
                            page.metadata
                                .push(("Dimensions".into(), format!("{width} × {height}")));
                            page.note = Some(
                                "Converted with ImageMagick using a bounded native codec.".into(),
                            );
                        }
                    }
                }
            }
        }
        return Ok(page);
    }
    let metadata = probe(path)?;
    if kind == FileKind::Audio {
        check_audio_layout(&metadata)?;
    }
    let dimensions_available = check_stream_pixels(&metadata)?;
    anyhow::ensure!(
        kind != FileKind::Video || dimensions_available,
        "Video dimensions are unavailable within the decode budget; use Bytes"
    );
    let duration = metadata
        .get("format")
        .and_then(|format| format.get("duration"))
        .and_then(|duration| duration.as_str())
        .and_then(|duration| duration.parse::<f64>().ok())
        .filter(|duration| duration.is_finite() && *duration >= 0.0);
    let seconds = request.offset as f64;
    page.image = Some(ffmpeg(path, kind, seconds)?);
    page.title = if kind == FileKind::Audio {
        format!("Waveform at {seconds:.0}s")
    } else {
        format!("Frame at {seconds:.0}s")
    };
    page.columns = vec!["Stream / property".into(), "Value".into()];
    if let Some(streams) = metadata.get("streams").and_then(|value| value.as_array()) {
        for (index, stream) in streams.iter().take(16).enumerate() {
            if let Some(object) = stream.as_object() {
                for (name, value) in object.iter().take(12) {
                    let value = value.to_string();
                    page.rows.push(vec![
                        format!("Stream {index}: {name}"),
                        value.chars().take(CELL_BYTES).collect(),
                    ]);
                }
            }
        }
    }
    let step = if kind == FileKind::Audio { 10 } else { 1 };
    page.next_offset = duration
        .filter(|duration| seconds + (step as f64) < *duration)
        .map(|_| request.offset.saturating_add(step));
    page.note = Some(
        if kind == FileKind::Audio {
            "Each page decodes a 10 second waveform window."
        } else {
            "Next and Previous seek by one second; frames are decoded on demand."
        }
        .into(),
    );
    Ok(page)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_audio_reads_one_bounded_waveform_window() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("hour.wav");
        let data_bytes = 3600u32 * 44100 * 4;
        let mut file = File::create(&path)?;
        file.write_all(b"RIFF")?;
        file.write_all(&(data_bytes + 36).to_le_bytes())?;
        file.write_all(b"WAVEfmt ")?;
        file.write_all(&16u32.to_le_bytes())?;
        file.write_all(&1u16.to_le_bytes())?;
        file.write_all(&2u16.to_le_bytes())?;
        file.write_all(&44100u32.to_le_bytes())?;
        file.write_all(&176400u32.to_le_bytes())?;
        file.write_all(&4u16.to_le_bytes())?;
        file.write_all(&16u16.to_le_bytes())?;
        file.write_all(b"data")?;
        file.write_all(&data_bytes.to_le_bytes())?;
        file.set_len(44 + u64::from(data_bytes))?;
        let before = file.metadata()?;
        let page = match read(&path, FileKind::Audio, &PreviewRequest::default()) {
            Ok(page) => page,
            Err(error)
                if error.chain().any(|cause| {
                    cause
                        .downcast_ref::<std::io::Error>()
                        .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound)
                }) =>
            {
                return Ok(());
            }
            Err(error) => return Err(error),
        };
        assert_eq!(page.next_offset, Some(10));
        let image = image::load_from_memory(&page.image.context("Waveform image")?)?;
        assert_eq!((image.width(), image.height()), (1024, 200));
        let after = file.metadata()?;
        assert_eq!(before.len(), after.len());
        assert_eq!(before.modified()?, after.modified()?);
        assert!(
            check_audio_layout(&serde_json::json!({"streams":[{
                "codec_type":"audio", "sample_rate":"4294967295", "channels":65535
            }]}))
            .is_err()
        );
        Ok(())
    }

    #[test]
    fn stream_pixel_budget_checks_total_dimensions_and_overflow() -> Result<()> {
        assert!(!check_stream_pixels(
            &serde_json::json!({"streams": [{"codec_type": "audio"}]})
        )?);
        assert!(check_stream_pixels(&serde_json::json!({"streams": [
            {"codec_type": "video", "width": 1024, "height": 1024},
            {"codec_type": "video", "width": 1024, "height": 1024}
        ]}))?);
        for metadata in [
            serde_json::json!({"streams": [{"codec_type":"video", "width": 8192, "height":8192}]}),
            serde_json::json!({"streams": [{"codec_type":"video", "width":4096, "height":4096}, {"codec_type":"video", "width":1, "height":1}]}),
            serde_json::json!({"streams": [{"codec_type":"video", "width":u64::MAX, "height":2}]}),
            serde_json::json!({"streams": [{"codec_type":"video", "width":0, "height":1}]}),
            serde_json::json!({"streams": [{"codec_type":"video", "width":1, "height":-1}]}),
        ] {
            assert!(check_stream_pixels(&metadata).is_err());
        }
        Ok(())
    }

    #[test]
    fn magick_native_xpm_fixture_preserves_pixels_and_source() -> Result<()> {
        let mut version = Command::new("magick");
        version.arg("-version");
        match capture(&mut version, 8192) {
            Ok(_) => {}
            Err(error)
                if error.chain().any(|cause| {
                    cause
                        .downcast_ref::<std::io::Error>()
                        .is_some_and(|problem| problem.kind() == std::io::ErrorKind::NotFound)
                }) =>
            {
                return Ok(());
            }
            Err(error) => return Err(error),
        }
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("native.xpm");
        let source = b"/* XPM */\nstatic char * pixels[] = {\n\"2 1 2 1\",\n\"r c #FF0000\",\n\"b c #0000FF\",\n\"rb\"\n};\n";
        std::fs::write(&path, source)?;
        let (bytes, width, height) = magick(&path)?;
        assert_eq!((width, height), (2, 1));
        let image = image::load_from_memory(&bytes)?.to_rgba8();
        assert_eq!(image.dimensions(), (2, 1));
        assert_eq!(image.get_pixel(0, 0).0, [255, 0, 0, 255]);
        assert_eq!(image.get_pixel(1, 0).0, [0, 0, 255, 255]);
        assert_eq!(std::fs::read(&path)?, source);
        let oversized = directory.path().join("oversized.xpm");
        std::fs::write(&oversized, b"/* XPM */\nstatic char * pixels[] = {\n\"40000 40000 1 1\",\n\"r c #FF0000\",\n\"r\"\n};\n")?;
        assert!(magick(&oversized).is_err());
        let delegated = directory.path().join("delegate.eps");
        std::fs::write(&delegated, b"%!PS-Adobe-3.0 EPSF-3.0\n%%BoundingBox: 0 0 2 1\n0 0 moveto 2 1 lineto stroke\nshowpage\n")?;
        let error = magick(&delegated)
            .err()
            .context("external delegate was denied")?;
        assert!(format!("{error:#}").contains("security policy"));
        Ok(())
    }
    #[test]
    fn image_output_pixel_limit_rejects_oversized_canvas() -> Result<()> {
        let image = DynamicImage::new_rgb8(2048, 1);
        let mut encoded = Cursor::new(Vec::new());
        image.write_to(&mut encoded, ImageFormat::Png)?;
        assert!(validate_preview_image(encoded.get_ref()).is_err());
        let preview = png(image)?;
        validate_preview_image(&preview)?;
        Ok(())
    }
    #[test]
    fn native_image_does_not_load_sparse_trailing_data() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("huge.bmp");
        DynamicImage::new_rgb8(16, 16).save(&path)?;
        File::options()
            .write(true)
            .open(&path)?
            .set_len(64 * 1024 * 1024 * 1024)?;
        let (bytes, width, height) = native_image(&path)?;
        assert_eq!((width, height), (16, 16));
        assert!(bytes.len() < IMAGE_BYTES);
        Ok(())
    }
    #[test]
    fn jpeg_decoder_cannot_read_an_entire_sparse_file_before_limits() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("huge.jpg");
        DynamicImage::new_rgb8(16, 16).save(&path)?;
        File::options()
            .write(true)
            .open(&path)?
            .set_len(64 * 1024 * 1024 * 1024)?;
        let error = native_image(&path).expect_err("Sparse JPEG must be bounded");
        assert!(error.to_string().contains("budget"));
        Ok(())
    }

    #[test]
    fn svg_isolation_and_filter_allocations_are_bounded() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("nested.svg");
        let source = format!(
            "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"1024\" height=\"1024\">{}<rect width=\"1024\" height=\"1024\"/>{}</svg>",
            "<g opacity=\"0.5\">".repeat(16),
            "</g>".repeat(16)
        );
        std::fs::write(&path, source)?;
        assert!(svg(&path).is_err());
        std::fs::write(
            &path,
            "<svg xmlns=\"http://www.w3.org/2000/svg\"><filter id=\"f\"/><rect filter=\"url(#f)\" width=\"100\" height=\"100\"/></svg>",
        )?;
        assert!(svg(&path).is_err());
        std::fs::write(
            &path,
            "<svg xmlns=\"http://www.w3.org/2000/svg\"><defs><g id=\"a\"><rect width=\"10\" height=\"10\"/></g></defs><use href=\"#a\"/></svg>",
        )?;
        assert!(svg(&path).is_err());
        let source = format!(
            "<svg xmlns=\"http://www.w3.org/2000/svg\">{}</svg>",
            "<rect width=\"1\" height=\"1\"/>".repeat(10_001)
        );
        std::fs::write(&path, source)?;
        assert!(svg(&path).is_err());
        Ok(())
    }

    #[test]
    fn svg_offscreen_isolation_is_charged_at_the_renderer_canvas_bound() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("offscreen.svg");
        let source = format!(
            "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"1024\" height=\"1024\">{}<rect x=\"-2048\" y=\"-2048\" width=\"5120\" height=\"5120\"/>{}</svg>",
            "<g opacity=\"0.5\">".repeat(8),
            "</g>".repeat(8)
        );
        std::fs::write(&path, source)?;
        let error = svg(&path)
            .err()
            .context("Oversized isolated layers must fail before rendering")?;
        assert!(error.to_string().contains("16 MiB"));
        std::fs::write(
            &path,
            "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"256\" height=\"256\"><g opacity=\"0.5\"><rect width=\"256\" height=\"256\" fill=\"blue\"/></g></svg>",
        )?;
        validate_preview_image(&svg(&path)?)?;
        Ok(())
    }

    #[test]
    fn svg_gradient_copy_amplification_is_rejected_before_tree_construction() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("gradient.svg");
        let stops = (0..1000)
            .map(|index| {
                format!(
                    "<stop offset=\"{}\" stop-color=\"{}\"/>",
                    index as f64 / 999.0,
                    if index % 2 == 0 { "red" } else { "blue" }
                )
            })
            .collect::<String>();
        let source = format!(
            "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"100\" height=\"100\"><defs><linearGradient id=\"g\">{stops}</linearGradient></defs>{}</svg>",
            "<rect width=\"100\" height=\"100\" fill=\"url(#g)\"/>".repeat(5000)
        );
        std::fs::write(&path, source)?;
        let error = svg(&path)
            .err()
            .context("Gradient cloning must fail before tree construction")?;
        assert!(error.to_string().contains("gradient expansion"));
        std::fs::write(
            &path,
            "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"100\" height=\"100\"><defs><linearGradient id=\"g\"><stop offset=\"0\" stop-color=\"red\"/><stop offset=\"1\" stop-color=\"blue\"/></linearGradient></defs><rect width=\"100\" height=\"100\" fill=\"url(#g)\"/></svg>",
        )?;
        validate_preview_image(&svg(&path)?)?;
        Ok(())
    }

    #[test]
    fn svg_reference_chains_are_rejected_before_expansion_and_pixel_allocation() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("references.svg");
        let mut clips =
            String::from("<clipPath id=\"c0\"><rect width=\"1024\" height=\"1024\"/></clipPath>");
        for index in 1..80 {
            clips.push_str(&format!("<clipPath id=\"c{index}\" clip-path=\"url(#c{})\"><rect width=\"1024\" height=\"1024\"/></clipPath>", index - 1));
        }
        std::fs::write(
            &path,
            format!(
                "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"1024\" height=\"1024\"><defs>{clips}</defs><rect width=\"1024\" height=\"1024\" clip-path=\"url(#c79)\"/></svg>"
            ),
        )?;
        let error = svg(&path)
            .err()
            .context("Clip chains must fail before rendering")?;
        assert!(error.to_string().contains("reference/filter"));
        let mut definitions = String::from("<g id=\"g0\"><rect width=\"1\" height=\"1\"/></g>");
        for index in 1..24 {
            definitions.push_str(&format!(
                "<g id=\"g{index}\"><use href=\"#g{}\"/><use href=\"#g{}\"/></g>",
                index - 1,
                index - 1
            ));
        }
        std::fs::write(
            &path,
            format!(
                "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"100\" height=\"100\"><defs>{definitions}</defs><use href=\"#g23\"/></svg>"
            ),
        )?;
        let error = svg(&path)
            .err()
            .context("Use expansion must fail before tree construction")?;
        assert!(error.to_string().contains("reference/filter"));
        let mut definitions = String::from(
            "<marker id=\"m0\" markerWidth=\"10\" markerHeight=\"10\"><rect width=\"10\" height=\"10\"/></marker>",
        );
        for index in 1..24 {
            definitions.push_str(&format!(
                "<marker id=\"m{index}\" markerWidth=\"10\" markerHeight=\"10\"><path d=\"M0 0L1 1L2 2\" stroke=\"black\" marker-mid=\"url(#m{})\" marker-end=\"url(#m{})\"/></marker>",
                index - 1,
                index - 1
            ));
        }
        std::fs::write(
            &path,
            format!(
                "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"100\" height=\"100\"><defs>{definitions}</defs><path d=\"M0 0L1 1L2 2\" stroke=\"black\" marker-mid=\"url(#m23)\" marker-end=\"url(#m23)\"/></svg>"
            ),
        )?;
        let error = svg(&path)
            .err()
            .context("Marker expansion must fail before tree construction")?;
        assert!(error.to_string().contains("reference/filter"));
        Ok(())
    }

    #[test]
    fn svg_css_and_inherited_dash_array_copying_are_bounded() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("styles.svg");
        let source = format!(
            "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"100\" height=\"100\"><style>rect {{ font-family: '{}'; }}</style>{}</svg>",
            "a".repeat(256 * 1024),
            "<rect width=\"1\" height=\"1\"/>".repeat(1000)
        );
        std::fs::write(&path, source)?;
        let error = svg(&path)
            .err()
            .context("CSS value copying must fail before tree construction")?;
        assert!(error.to_string().contains("style expansion"));
        let source = format!(
            "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"100\" height=\"100\"><g stroke=\"black\" stroke-dasharray=\"{}\">{}</g></svg>",
            "1 1 ".repeat(10_000),
            "<rect width=\"1\" height=\"1\"/>".repeat(1000)
        );
        std::fs::write(&path, source)?;
        let error = svg(&path)
            .err()
            .context("Inherited dash-array copying must fail before tree construction")?;
        assert!(error.to_string().contains("style expansion"));
        std::fs::write(
            &path,
            "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"100\" height=\"100\"><style>rect { fill: blue; }</style><rect width=\"100\" height=\"100\" style=\"stroke: red; stroke-dasharray: 1 1\"/></svg>",
        )?;
        validate_preview_image(&svg(&path)?)?;
        Ok(())
    }

    #[test]
    fn decoder_stdout_is_bounded_and_process_is_reaped() -> Result<()> {
        #[cfg(unix)]
        {
            let mut command = Command::new("sh");
            command.args([
                "-c",
                "while :; do printf 'xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx'; done",
            ]);
            let start = Instant::now();
            assert!(capture(&mut command, 128).is_err());
            assert!(start.elapsed() < Duration::from_secs(12));
        }
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn decoder_descendant_cannot_keep_output_pipes_open_past_deadline() -> Result<()> {
        let mut command = Command::new("sh");
        command.args(["-c", "sleep 30 & exit 0"]);
        let start = Instant::now();
        let result = capture_with_timeout(&mut command, 128, Duration::from_millis(100));
        assert!(result.is_err());
        assert!(start.elapsed() < Duration::from_secs(2));
        Ok(())
    }
}
