use crate::{PAGE_ROWS, PreviewPage, PreviewRequest};
use anyhow::{Context as _, Result, bail, ensure};
use std::{
    fs::File,
    io::{BufRead, BufReader, Read, Seek, SeekFrom},
    path::Path,
    time::{Duration, Instant},
};

type Vertex = [f32; 3];
type Triangle = [Vertex; 3];
const VERTEX_LIMIT: usize = 100_000;
const SCAN_BYTES: u64 = 32 * 1024 * 1024;
const FACE_VERTICES: usize = 256;
const LINE_BYTES: u64 = 16 * 1024;

struct MeshInput {
    input: BufReader<File>,
    remaining: u64,
    deadline: Instant,
}

impl MeshInput {
    fn new(file: File) -> Self {
        Self {
            input: BufReader::new(file),
            remaining: SCAN_BYTES,
            deadline: Instant::now() + Duration::from_secs(2),
        }
    }

    fn check(&self) -> Result<()> {
        ensure!(
            Instant::now() < self.deadline && self.remaining != 0,
            "Mesh scan exceeds its 32 MiB or two second budget; use Content or Bytes"
        );
        Ok(())
    }

    fn line(&mut self) -> Result<String> {
        self.check()?;
        let mut line = String::new();
        let count = self
            .input
            .by_ref()
            .take((LINE_BYTES + 1).min(self.remaining))
            .read_line(&mut line)?;
        self.remaining = self.remaining.saturating_sub(count as u64);
        ensure!(count != 0, "Truncated mesh record");
        ensure!(
            count as u64 <= LINE_BYTES,
            "Mesh line exceeds its 16 KiB budget"
        );
        Ok(line)
    }

    fn exact(&mut self, bytes: &mut [u8]) -> Result<()> {
        self.check()?;
        ensure!(
            bytes.len() as u64 <= self.remaining,
            "Mesh read budget exceeded"
        );
        self.input.read_exact(bytes)?;
        self.remaining -= bytes.len() as u64;
        Ok(())
    }

    fn skip(&mut self, bytes: u64) -> Result<()> {
        self.check()?;
        let position = self.input.stream_position()?;
        let end = position
            .checked_add(bytes)
            .context("Mesh position overflow")?;
        ensure!(
            end <= self.input.get_ref().metadata()?.len(),
            "Truncated mesh element"
        );
        self.input.seek(SeekFrom::Start(end))?;
        Ok(())
    }
}

#[derive(Clone, Copy)]
enum PlyType {
    Signed(usize),
    Unsigned(usize),
    Float(usize),
}

impl PlyType {
    fn parse(value: &str) -> Result<Self> {
        Ok(match value {
            "char" | "int8" => Self::Signed(1),
            "uchar" | "uint8" => Self::Unsigned(1),
            "short" | "int16" => Self::Signed(2),
            "ushort" | "uint16" => Self::Unsigned(2),
            "int" | "int32" => Self::Signed(4),
            "uint" | "uint32" => Self::Unsigned(4),
            "int64" => Self::Signed(8),
            "uint64" => Self::Unsigned(8),
            "float" | "float32" => Self::Float(4),
            "double" | "float64" => Self::Float(8),
            _ => bail!("Unsupported PLY property type: {value}"),
        })
    }

    fn size(self) -> usize {
        match self {
            Self::Signed(bytes) | Self::Unsigned(bytes) | Self::Float(bytes) => bytes,
        }
    }

    fn integer(self) -> bool {
        !matches!(self, Self::Float(_))
    }
}

#[derive(Clone, Copy)]
enum PlyNumber {
    Signed(i64),
    Unsigned(u64),
    Float(f64),
}

impl PlyNumber {
    fn index(self) -> Result<u64> {
        match self {
            Self::Signed(value) => u64::try_from(value).context("Negative PLY index or count"),
            Self::Unsigned(value) => Ok(value),
            Self::Float(_) => bail!("PLY indices and list counts must be integers"),
        }
    }

    fn coordinate(self) -> Result<f32> {
        let value = match self {
            Self::Signed(value) => value as f64,
            Self::Unsigned(value) => value as f64,
            Self::Float(value) => value,
        } as f32;
        ensure!(
            value.is_finite(),
            "Non-finite or overflowing PLY coordinate"
        );
        Ok(value)
    }
}

enum PlyProperty {
    Scalar {
        name: String,
        kind: PlyType,
    },
    List {
        name: String,
        count: PlyType,
        kind: PlyType,
    },
}

struct PlyElement {
    name: String,
    count: u64,
    properties: Vec<PlyProperty>,
}

impl PlyElement {
    fn stride(&self) -> Option<u64> {
        self.properties
            .iter()
            .try_fold(0u64, |stride, property| match property {
                PlyProperty::Scalar { kind, .. } => stride.checked_add(kind.size() as u64),
                PlyProperty::List { .. } => None,
            })
    }
}

#[derive(Clone, Copy)]
enum PlyEncoding {
    Ascii,
    LittleEndian,
    BigEndian,
}

struct PlyHeader {
    encoding: PlyEncoding,
    elements: Vec<PlyElement>,
}

fn ply_header(input: &mut MeshInput) -> Result<PlyHeader> {
    ensure!(input.line()?.trim() == "ply", "Invalid PLY signature");
    let mut encoding = None;
    let mut elements = Vec::<PlyElement>::new();
    let mut bytes = 4u64;
    loop {
        let line = input.line()?;
        bytes += line.len() as u64;
        ensure!(bytes <= 64 * 1024, "PLY header exceeds its 64 KiB budget");
        let mut parts = line.split_whitespace();
        match parts.next() {
            Some("format") => {
                ensure!(encoding.is_none(), "Repeated PLY encoding declaration");
                encoding = Some(match parts.next() {
                    Some("ascii") => PlyEncoding::Ascii,
                    Some("binary_little_endian") => PlyEncoding::LittleEndian,
                    Some("binary_big_endian") => PlyEncoding::BigEndian,
                    _ => bail!("Unsupported PLY encoding"),
                });
                ensure!(parts.next() == Some("1.0"), "Unsupported PLY version");
            }
            Some("element") => {
                ensure!(
                    elements.len() < 256,
                    "PLY has too many element declarations"
                );
                let name = parts.next().context("Missing PLY element name")?.to_owned();
                ensure!(name.len() <= 256, "PLY element name exceeds budget");
                ensure!(
                    !elements.iter().any(|element| element.name == name),
                    "Repeated PLY element"
                );
                elements.push(PlyElement {
                    name,
                    count: parts.next().context("Missing PLY element count")?.parse()?,
                    properties: Vec::new(),
                });
            }
            Some("property") => {
                let element = elements
                    .last_mut()
                    .context("PLY property precedes element")?;
                ensure!(
                    element.properties.len() < 64,
                    "PLY has too many properties per element"
                );
                let kind = parts.next().context("Missing PLY property type")?;
                let property = if kind == "list" {
                    let count =
                        PlyType::parse(parts.next().context("Missing PLY list count type")?)?;
                    let kind = PlyType::parse(parts.next().context("Missing PLY list item type")?)?;
                    ensure!(count.integer(), "PLY list count type must be integer");
                    PlyProperty::List {
                        name: parts.next().context("Missing PLY property name")?.into(),
                        count,
                        kind,
                    }
                } else {
                    PlyProperty::Scalar {
                        name: parts.next().context("Missing PLY property name")?.into(),
                        kind: PlyType::parse(kind)?,
                    }
                };
                element.properties.push(property);
            }
            Some("end_header") => break,
            Some("comment" | "obj_info") | None => {}
            _ => bail!("Invalid PLY header declaration"),
        }
    }
    let header = PlyHeader {
        encoding: encoding.context("Missing PLY encoding")?,
        elements,
    };
    let vertices = header
        .elements
        .iter()
        .find(|element| element.name == "vertex")
        .context("PLY has no vertex element")?;
    for axis in ["x", "y", "z"] {
        ensure!(
            vertices
                .properties
                .iter()
                .filter(
                    |property| matches!(property, PlyProperty::Scalar {name, ..} if name == axis)
                )
                .count()
                == 1,
            "PLY vertex requires one scalar {axis} property"
        );
    }
    Ok(header)
}

fn ply_number(
    input: &mut MeshInput,
    encoding: PlyEncoding,
    kind: PlyType,
    tokens: &mut dyn Iterator<Item = &str>,
) -> Result<PlyNumber> {
    if matches!(encoding, PlyEncoding::Ascii) {
        let token = tokens.next().context("Truncated PLY record")?;
        return Ok(match kind {
            PlyType::Signed(size) => {
                let value: i64 = token.parse()?;
                let bits = size * 8;
                ensure!(
                    bits == 64 || value >= -(1i64 << (bits - 1)) && value < (1i64 << (bits - 1)),
                    "PLY integer exceeds its declared type"
                );
                PlyNumber::Signed(value)
            }
            PlyType::Unsigned(size) => {
                let value: u64 = token.parse()?;
                ensure!(
                    size == 8 || value < (1u64 << (size * 8)),
                    "PLY integer exceeds its declared type"
                );
                PlyNumber::Unsigned(value)
            }
            PlyType::Float(_) => PlyNumber::Float(token.parse()?),
        });
    }
    let mut data = [0u8; 8];
    let size = kind.size();
    input.exact(&mut data[..size])?;
    let little = matches!(encoding, PlyEncoding::LittleEndian);
    let unsigned = if little {
        u64::from_le_bytes(data)
    } else {
        let mut aligned = [0u8; 8];
        aligned[8 - size..].copy_from_slice(&data[..size]);
        u64::from_be_bytes(aligned)
    };
    Ok(match kind {
        PlyType::Unsigned(_) => PlyNumber::Unsigned(unsigned),
        PlyType::Signed(_) => {
            let shift = 64 - size * 8;
            PlyNumber::Signed(((unsigned << shift) as i64) >> shift)
        }
        PlyType::Float(4) => PlyNumber::Float(f64::from(f32::from_bits(unsigned as u32))),
        PlyType::Float(8) => PlyNumber::Float(f64::from_bits(unsigned)),
        PlyType::Float(_) => bail!("Invalid PLY floating point width"),
    })
}

#[derive(Default)]
struct PlyRecord {
    vertex: Vertex,
    indices: Vec<u64>,
}

fn ply_record(
    input: &mut MeshInput,
    encoding: PlyEncoding,
    element: &PlyElement,
) -> Result<PlyRecord> {
    input.check()?;
    let line = if matches!(encoding, PlyEncoding::Ascii) {
        input.line()?
    } else {
        String::new()
    };
    let mut tokens = line.split_whitespace();
    let mut record = PlyRecord::default();
    for property in &element.properties {
        match property {
            PlyProperty::Scalar { name, kind } => {
                let value = ply_number(input, encoding, *kind, &mut tokens)?;
                if element.name == "vertex" {
                    match name.as_str() {
                        "x" => record.vertex[0] = value.coordinate()?,
                        "y" => record.vertex[1] = value.coordinate()?,
                        "z" => record.vertex[2] = value.coordinate()?,
                        _ => {}
                    }
                }
            }
            PlyProperty::List { name, count, kind } => {
                let count = ply_number(input, encoding, *count, &mut tokens)?.index()?;
                ensure!(
                    count <= FACE_VERTICES as u64,
                    "PLY property list exceeds its 256 item budget"
                );
                let face_indices = element.name == "face"
                    && matches!(name.as_str(), "vertex_indices" | "vertex_index");
                if face_indices {
                    ensure!(record.indices.is_empty(), "Repeated PLY face index list");
                }
                for _ in 0..count {
                    let value = ply_number(input, encoding, *kind, &mut tokens)?;
                    if face_indices {
                        record.indices.push(value.index()?);
                    }
                }
            }
        }
    }
    ensure!(
        !matches!(encoding, PlyEncoding::Ascii) || tokens.next().is_none(),
        "Unexpected values after PLY record"
    );
    Ok(record)
}

fn ply_skip(
    input: &mut MeshInput,
    encoding: PlyEncoding,
    element: &PlyElement,
    count: u64,
) -> Result<()> {
    if !matches!(encoding, PlyEncoding::Ascii) {
        if let Some(stride) = element.stride() {
            return input.skip(
                stride
                    .checked_mul(count)
                    .context("PLY element size overflow")?,
            );
        }
    }
    for _ in 0..count {
        ply_record(input, encoding, element)?;
    }
    Ok(())
}

fn triangulate(
    indices: &[u64],
    vertices: &[Vertex],
    offset: u64,
    number: &mut u64,
    triangles: &mut Vec<Triangle>,
) -> Result<bool> {
    ensure!(
        (3..=FACE_VERTICES).contains(&indices.len()),
        "Mesh faces require 3 to 256 vertices"
    );
    for index in indices {
        ensure!(
            *index < vertices.len() as u64,
            "Face references an unavailable vertex"
        );
    }
    let origin = *vertices
        .get(usize::try_from(
            *indices.first().context("Empty mesh face")?,
        )?)
        .context("Invalid mesh vertex")?;
    for indices in indices.windows(2).skip(1) {
        if *number >= offset {
            if triangles.len() == PAGE_ROWS {
                return Ok(true);
            }
            triangles.push([
                origin,
                *vertices
                    .get(usize::try_from(indices[0])?)
                    .context("Invalid mesh vertex")?,
                *vertices
                    .get(usize::try_from(indices[1])?)
                    .context("Invalid mesh vertex")?,
            ]);
        }
        *number = number
            .checked_add(1)
            .context("Mesh triangle count overflow")?;
    }
    Ok(false)
}

fn ply(path: &Path, request: &PreviewRequest) -> Result<PreviewPage> {
    let mut input = MeshInput::new(File::open(path)?);
    let header = ply_header(&mut input)?;
    let selected = request.section.as_deref().unwrap_or("Mesh XY");
    let vertex_count = header
        .elements
        .iter()
        .find(|element| element.name == "vertex")
        .context("Missing PLY vertices")?
        .count;
    let mut vertices = Vec::new();
    let mut triangles = Vec::new();
    let mut triangle_number = 0u64;
    let mut page = PreviewPage {
        title: selected.into(),
        sections: polygon_sections(),
        metadata: header
            .elements
            .iter()
            .map(|element| (element.name.clone(), element.count.to_string()))
            .collect(),
        ..Default::default()
    };
    let tabular = matches!(selected, "Vertices" | "Faces");
    if !tabular && vertex_count > VERTEX_LIMIT as u64 {
        page.note = Some("Wireframe requires at most 100000 indexed vertices. Choose Vertices or Faces to inspect bounded pages of geometry.".into());
        return Ok(page);
    }
    for element in &header.elements {
        if tabular
            && element.name
                == if selected == "Vertices" {
                    "vertex"
                } else {
                    "face"
                }
        {
            let offset = request.offset.min(element.count);
            ply_skip(&mut input, header.encoding, element, offset)?;
            let end = offset.saturating_add(PAGE_ROWS as u64).min(element.count);
            page.columns = if selected == "Vertices" {
                ["Vertex", "X", "Y", "Z"]
            } else {
                ["Face", "Vertex count", "Vertex indices", "Validation"]
            }
            .map(String::from)
            .into();
            for index in offset..end {
                let record = ply_record(&mut input, header.encoding, element)?;
                if selected == "Vertices" {
                    page.rows.push(vec![
                        index.to_string(),
                        record.vertex[0].to_string(),
                        record.vertex[1].to_string(),
                        record.vertex[2].to_string(),
                    ]);
                } else {
                    ensure!(
                        (3..=FACE_VERTICES).contains(&record.indices.len()),
                        "PLY face has no valid vertex index list"
                    );
                    ensure!(
                        record.indices.iter().all(|index| *index < vertex_count),
                        "PLY face references an invalid vertex"
                    );
                    page.rows.push(vec![
                        index.to_string(),
                        record.indices.len().to_string(),
                        format!("{:?}", record.indices),
                        "Valid".into(),
                    ]);
                }
            }
            page.next_offset = (end < element.count).then_some(end);
            return Ok(page);
        }
        if tabular {
            ply_skip(&mut input, header.encoding, element, element.count)?;
            continue;
        }
        match element.name.as_str() {
            "vertex" => {
                ensure!(
                    element.count <= VERTEX_LIMIT as u64,
                    "Wireframe requires at most 100000 vertices; choose Vertices or Faces for bounded pages"
                );
                vertices.reserve(element.count as usize);
                for _ in 0..element.count {
                    vertices.push(ply_record(&mut input, header.encoding, element)?.vertex);
                }
            }
            "face" => {
                ensure!(
                    !vertices.is_empty() || element.count == 0,
                    "PLY faces precede vertices; choose Faces for content"
                );
                for _ in 0..element.count {
                    let record = ply_record(&mut input, header.encoding, element)?;
                    if triangulate(
                        &record.indices,
                        &vertices,
                        request.offset,
                        &mut triangle_number,
                        &mut triangles,
                    )? {
                        return triangle_page(
                            triangles,
                            true,
                            request,
                            page.sections,
                            page.metadata,
                        );
                    }
                }
            }
            _ => ply_skip(&mut input, header.encoding, element, element.count)?,
        }
    }
    if tabular {
        bail!("PLY has no {selected} element");
    }
    triangle_page(triangles, false, request, page.sections, page.metadata)
}

fn off_line(input: &mut MeshInput) -> Result<String> {
    loop {
        let line = input.line()?;
        let content = line.split('#').next().unwrap_or_default().trim();
        if !content.is_empty() {
            return Ok(content.into());
        }
    }
}

fn off_face(input: &mut MeshInput, vertex_count: u64) -> Result<Vec<u64>> {
    let line = off_line(input)?;
    let mut parts = line.split_whitespace();
    let count: usize = parts.next().context("Missing OFF face size")?.parse()?;
    ensure!(
        (3..=FACE_VERTICES).contains(&count),
        "OFF faces require 3 to 256 vertices"
    );
    let mut indices = Vec::with_capacity(count);
    for _ in 0..count {
        let index: u64 = parts
            .next()
            .context("Truncated OFF face indices")?
            .parse()?;
        ensure!(
            index < vertex_count,
            "OFF face references an invalid vertex"
        );
        indices.push(index);
    }
    Ok(indices)
}

fn off(path: &Path, request: &PreviewRequest) -> Result<PreviewPage> {
    let mut input = MeshInput::new(File::open(path)?);
    let header = off_line(&mut input)?;
    let mut parts = header.split_whitespace();
    ensure!(
        parts.next() == Some("OFF"),
        "Only plain OFF geometry is supported"
    );
    let counts = if parts.clone().next().is_some() {
        parts.collect::<Vec<_>>().join(" ")
    } else {
        off_line(&mut input)?
    };
    let mut counts = counts.split_whitespace();
    let vertex_count: u64 = counts.next().context("Missing OFF vertex count")?.parse()?;
    let face_count: u64 = counts.next().context("Missing OFF face count")?.parse()?;
    let edge_count: u64 = counts.next().context("Missing OFF edge count")?.parse()?;
    let selected = request.section.as_deref().unwrap_or("Mesh XY");
    let tabular = matches!(selected, "Vertices" | "Faces");
    let mut page = PreviewPage {
        title: selected.into(),
        sections: polygon_sections(),
        metadata: vec![
            ("Vertices".into(), vertex_count.to_string()),
            ("Faces".into(), face_count.to_string()),
            ("Edges".into(), edge_count.to_string()),
        ],
        ..Default::default()
    };
    if !tabular && vertex_count > VERTEX_LIMIT as u64 {
        page.note = Some("Wireframe requires at most 100000 indexed vertices. Choose Vertices or Faces to inspect bounded pages of geometry.".into());
        return Ok(page);
    }
    let mut vertices = Vec::new();
    if !tabular {
        vertices.reserve(vertex_count as usize);
    }
    page.columns = ["Vertex", "X", "Y", "Z"].map(String::from).into();
    let end = request
        .offset
        .saturating_add(PAGE_ROWS as u64)
        .min(vertex_count);
    for index in 0..vertex_count {
        let line = off_line(&mut input)?;
        let point = vertex(&mut line.split_whitespace())?;
        if !tabular {
            vertices.push(point);
        } else if selected == "Vertices" && index >= request.offset {
            page.rows.push(vec![
                index.to_string(),
                point[0].to_string(),
                point[1].to_string(),
                point[2].to_string(),
            ]);
        }
        if selected == "Vertices" && index + 1 == end {
            page.next_offset = (end < vertex_count).then_some(end);
            return Ok(page);
        }
    }
    if selected == "Vertices" {
        return Ok(page);
    }
    let mut triangles = Vec::new();
    let mut triangle_number = 0u64;
    page.columns = ["Face", "Vertex count", "Vertex indices"]
        .map(String::from)
        .into();
    let end = request
        .offset
        .saturating_add(PAGE_ROWS as u64)
        .min(face_count);
    for index in 0..face_count {
        let indices = off_face(&mut input, vertex_count)?;
        if selected == "Faces" {
            if index >= request.offset {
                page.rows.push(vec![
                    index.to_string(),
                    indices.len().to_string(),
                    format!("{indices:?}"),
                ]);
            }
            if index + 1 == end {
                page.next_offset = (end < face_count).then_some(end);
                return Ok(page);
            }
        } else if triangulate(
            &indices,
            &vertices,
            request.offset,
            &mut triangle_number,
            &mut triangles,
        )? {
            return triangle_page(triangles, true, request, page.sections, page.metadata);
        }
    }
    if selected == "Faces" {
        return Ok(page);
    }
    triangle_page(triangles, false, request, page.sections, page.metadata)
}

fn polygon_sections() -> Vec<String> {
    [
        "Mesh XY", "Mesh XZ", "Mesh YZ", "Vertices", "Faces", "Content",
    ]
    .map(String::from)
    .into()
}

fn vertex(parts: &mut dyn Iterator<Item = &str>) -> Result<Vertex> {
    let mut vertex = [0.0f32; 3];
    for value in &mut vertex {
        *value = parts.next().context("Incomplete model vertex")?.parse()?;
        ensure!(value.is_finite(), "Non-finite model coordinate");
    }
    Ok(vertex)
}

fn obj(path: &Path, request: &PreviewRequest) -> Result<(Vec<Triangle>, bool)> {
    let file = File::open(path)?;
    let length = file.metadata()?.len();
    let mut input = BufReader::new(file.take(SCAN_BYTES));
    let mut vertices = Vec::new();
    let mut triangles = Vec::new();
    let mut face_number = 0u64;
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut line = String::new();
    loop {
        ensure!(
            Instant::now() < deadline,
            "Model scan reached its two second budget; use Content or Bytes"
        );
        line.clear();
        input.by_ref().take(16 * 1024 + 1).read_line(&mut line)?;
        if line.is_empty() {
            break;
        }
        ensure!(line.len() <= 16 * 1024, "Model line exceeds preview budget");
        let mut parts = line.split_whitespace();
        match parts.next() {
            Some("v") => {
                ensure!(
                    vertices.len() < VERTEX_LIMIT,
                    "Model has more than 100000 indexed vertices; use Content or Bytes"
                );
                vertices.push(vertex(&mut parts)?);
            }
            Some("f") => {
                let mut points = Vec::new();
                for part in parts.take(257) {
                    let index = part
                        .split('/')
                        .next()
                        .context("Invalid face index")?
                        .parse::<i64>()?;
                    let position = if index > 0 {
                        usize::try_from(index - 1)?
                    } else {
                        usize::try_from(i64::try_from(vertices.len())? + index)?
                    };
                    points.push(
                        *vertices
                            .get(position)
                            .context("Face references an unavailable vertex")?,
                    );
                }
                ensure!(
                    (3..=256).contains(&points.len()),
                    "Model faces require 3 to 256 vertices within the preview budget"
                );
                let origin = points.first().context("Face has no vertices")?;
                for points in points.windows(2).skip(1) {
                    if face_number >= request.offset {
                        if triangles.len() == PAGE_ROWS {
                            return Ok((triangles, true));
                        }
                        triangles.push([
                            *origin,
                            *points.first().context("face")?,
                            *points.get(1).context("face")?,
                        ]);
                    }
                    face_number += 1;
                }
            }
            _ => {}
        }
    }
    ensure!(
        length <= SCAN_BYTES,
        "Model scan exceeds the 32 MiB budget; use Content or Bytes"
    );
    Ok((triangles, false))
}

fn stl(path: &Path, request: &PreviewRequest) -> Result<(Vec<Triangle>, bool)> {
    let mut file = File::open(path)?;
    let length = file.metadata()?.len();
    let mut header = [0; 84];
    file.read_exact(&mut header)?;
    let count = u32::from_le_bytes(header[80..84].try_into()?) as u64;
    if count
        .checked_mul(50)
        .and_then(|bytes| bytes.checked_add(84))
        == Some(length)
    {
        let offset = request.offset.min(count);
        file.seek(SeekFrom::Start(84 + offset * 50))?;
        let mut triangles = Vec::new();
        for _ in offset..count.min(offset + PAGE_ROWS as u64) {
            let mut data = [0; 50];
            file.read_exact(&mut data)?;
            let mut triangle = [[0.0; 3]; 3];
            for (vertex_index, point) in triangle.iter_mut().enumerate() {
                for (axis, value) in point.iter_mut().enumerate() {
                    let position = 12 + vertex_index * 12 + axis * 4;
                    *value = f32::from_le_bytes(
                        data.get(position..position + 4)
                            .context("Truncated STL vertex")?
                            .try_into()?,
                    );
                    ensure!(value.is_finite(), "Non-finite STL coordinate");
                }
            }
            triangles.push(triangle);
        }
        return Ok((triangles, offset + (PAGE_ROWS as u64) < count));
    }
    file.seek(SeekFrom::Start(0))?;
    let mut input = BufReader::new(file.take(SCAN_BYTES));
    let mut line = String::new();
    let mut vertices = Vec::with_capacity(3);
    let mut triangles = Vec::new();
    let mut index = 0u64;
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        ensure!(
            Instant::now() < deadline,
            "STL scan exceeded preview deadline"
        );
        line.clear();
        input.by_ref().take(16 * 1024 + 1).read_line(&mut line)?;
        if line.is_empty() {
            break;
        }
        ensure!(line.len() <= 16 * 1024, "STL line exceeds preview budget");
        let mut parts = line.split_whitespace();
        if parts.next() == Some("vertex") {
            vertices.push(vertex(&mut parts)?);
        }
        if vertices.len() == 3 {
            if index >= request.offset {
                if triangles.len() == PAGE_ROWS {
                    return Ok((triangles, true));
                }
                triangles.push([
                    *vertices.first().context("vertex")?,
                    *vertices.get(1).context("vertex")?,
                    *vertices.get(2).context("vertex")?,
                ]);
            }
            vertices.clear();
            index += 1;
        }
    }
    ensure!(
        length <= SCAN_BYTES,
        "STL scan exceeds read budget; use Bytes"
    );
    Ok((triangles, false))
}

fn draw(triangles: &[Triangle], axes: (usize, usize)) -> Result<Vec<u8>> {
    let mut bounds = [
        f64::INFINITY,
        f64::INFINITY,
        f64::NEG_INFINITY,
        f64::NEG_INFINITY,
    ];
    for point in triangles.iter().flatten() {
        bounds[0] = bounds[0].min(f64::from(point[axes.0]));
        bounds[1] = bounds[1].min(f64::from(point[axes.1]));
        bounds[2] = bounds[2].max(f64::from(point[axes.0]));
        bounds[3] = bounds[3].max(f64::from(point[axes.1]));
    }
    let width = 768;
    let height = 768;
    let mut image = image::RgbaImage::from_pixel(width, height, image::Rgba([28, 30, 35, 255]));
    let span = (bounds[2] - bounds[0]).max(bounds[3] - bounds[1]);
    let span = if span == 0.0 { 1.0 } else { span };
    let project = |point: Vertex| {
        (
            ((f64::from(point[axes.0]) - bounds[0]) / span * 704.0 + 32.0).clamp(0.0, 767.0) as i32,
            (736.0 - (f64::from(point[axes.1]) - bounds[1]) / span * 704.0).clamp(0.0, 767.0)
                as i32,
        )
    };
    for triangle in triangles {
        for index in 0..3 {
            let (mut horizontal, mut vertical) = project(triangle[index]);
            let (end_horizontal, end_vertical) = project(triangle[(index + 1) % 3]);
            let horizontal_delta = (end_horizontal - horizontal).abs();
            let vertical_delta = -(end_vertical - vertical).abs();
            let horizontal_step = if horizontal < end_horizontal { 1 } else { -1 };
            let vertical_step = if vertical < end_vertical { 1 } else { -1 };
            let mut error = horizontal_delta + vertical_delta;
            for _ in 0..2048 {
                if horizontal >= 0
                    && vertical >= 0
                    && (horizontal as u32) < width
                    && (vertical as u32) < height
                {
                    image.put_pixel(
                        horizontal as u32,
                        vertical as u32,
                        image::Rgba([111, 180, 250, 255]),
                    );
                }
                if horizontal == end_horizontal && vertical == end_vertical {
                    break;
                }
                let doubled = 2 * error;
                if doubled >= vertical_delta {
                    error += vertical_delta;
                    horizontal += horizontal_step;
                }
                if doubled <= horizontal_delta {
                    error += horizontal_delta;
                    vertical += vertical_step;
                }
            }
        }
    }
    crate::media::png(image::DynamicImage::ImageRgba8(image))
}

pub(crate) fn read(path: &Path, request: &PreviewRequest) -> Result<PreviewPage> {
    let extension = crate::extension(&path.to_string_lossy());
    match extension.as_str() {
        "ply" => return ply(path, request),
        "off" => return off(path, request),
        _ => {}
    }
    let (triangles, more) = match extension.as_str() {
        "obj" => obj(path, request)?,
        "stl" => stl(path, request)?,
        _ => bail!("Unsupported mesh decoder"),
    };
    triangle_page(triangles, more, request, sections(path), Vec::new())
}

pub(crate) fn sections(path: &Path) -> Vec<String> {
    match crate::extension(&path.to_string_lossy()).as_str() {
        "ply" | "off" => polygon_sections(),
        _ => ["Mesh XY", "Mesh XZ", "Mesh YZ", "Content"]
            .map(String::from)
            .into(),
    }
}

fn triangle_page(
    triangles: Vec<Triangle>,
    more: bool,
    request: &PreviewRequest,
    sections: Vec<String>,
    metadata: Vec<(String, String)>,
) -> Result<PreviewPage> {
    let title = request.section.as_deref().unwrap_or("Mesh XY");
    let axes = match title {
        "Mesh XZ" => (0, 2),
        "Mesh YZ" => (1, 2),
        _ => (0, 1),
    };
    let image = (!triangles.is_empty())
        .then(|| draw(&triangles, axes))
        .transpose()?;
    let rows = triangles
        .iter()
        .enumerate()
        .map(|(index, triangle)| {
            vec![
                request.offset.saturating_add(index as u64).to_string(),
                format!("{:?}", triangle[0]),
                format!("{:?}", triangle[1]),
                format!("{:?}", triangle[2]),
            ]
        })
        .collect();
    Ok(PreviewPage {
        title: title.into(),
        sections,
        metadata,
        columns: ["Triangle", "Vertex A", "Vertex B", "Vertex C"].map(String::from).into(),
        rows,
        image,
        next_offset: more.then_some(request.offset.saturating_add(triangles.len() as u64)),
        note: Some("Wireframe and coordinates show up to 200 triangles per page. Choose a projection or inspect raw content.".into()),
        is_hex: false,
        large_file_size: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn section(section: &str, offset: u64) -> PreviewRequest {
        PreviewRequest {
            section: Some(section.into()),
            offset,
        }
    }

    #[test]
    fn ascii_ply_pages_reordered_coordinates_and_polygon_faces() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("model.ply");
        let mut content = "ply\nformat ascii 1.0\ncomment reordered coordinates\nelement vertex 270\nproperty uchar red\nproperty float z\nproperty double x\nproperty float y\nelement face 270\nproperty list uchar int vertex_indices\nend_header\n".to_owned();
        for index in 0..270 {
            content.push_str(&format!("255 0 {index} 1\n"));
        }
        for _ in 0..270 {
            content.push_str("4 0 1 2 3\n");
        }
        std::fs::write(&path, content)?;
        let vertices = crate::read(&path, &section("Vertices", 200))?;
        assert_eq!(vertices.rows.len(), 70);
        assert_eq!(
            vertices.rows.first(),
            Some(&vec![
                "200".to_owned(),
                "200".to_owned(),
                "1".to_owned(),
                "0".to_owned()
            ])
        );
        assert_eq!(vertices.next_offset, None);
        let faces = crate::read(&path, &section("Faces", 0))?;
        assert_eq!(faces.rows.len(), 200);
        assert_eq!(
            faces
                .rows
                .first()
                .and_then(|row| row.get(2))
                .map(String::as_str),
            Some("[0, 1, 2, 3]")
        );
        assert_eq!(faces.next_offset, Some(200));
        let mesh = crate::read(&path, &PreviewRequest::default())?;
        assert_eq!(mesh.rows.len(), 200);
        assert_eq!(mesh.next_offset, Some(200));
        assert!(mesh.image.is_some());
        let last = crate::read(&path, &section("Mesh XZ", 400))?;
        assert_eq!(last.rows.len(), 140);
        assert_eq!(last.next_offset, None);
        Ok(())
    }

    #[test]
    fn binary_ply_decodes_both_byte_orders_and_ignores_extra_properties() -> Result<()> {
        use std::io::Write;
        let directory = tempfile::tempdir()?;
        for little in [true, false] {
            let encoding = if little {
                "binary_little_endian"
            } else {
                "binary_big_endian"
            };
            let path = directory.path().join(format!("{encoding}.ply"));
            let mut file = File::create(&path)?;
            write!(
                file,
                "ply\nformat {encoding} 1.0\nelement vertex 3\nproperty float x\nproperty double y\nproperty short z\nproperty uchar intensity\nelement face 1\nproperty uint material\nproperty list uchar int vertex_indices\nend_header\n"
            )?;
            for (horizontal, vertical) in [(0.0f32, 0.0f64), (1.0, 0.0), (0.0, 1.0)] {
                file.write_all(&if little {
                    horizontal.to_le_bytes()
                } else {
                    horizontal.to_be_bytes()
                })?;
                file.write_all(&if little {
                    vertical.to_le_bytes()
                } else {
                    vertical.to_be_bytes()
                })?;
                file.write_all(&if little {
                    (-2i16).to_le_bytes()
                } else {
                    (-2i16).to_be_bytes()
                })?;
                file.write_all(&[128])?;
            }
            file.write_all(&if little {
                7u32.to_le_bytes()
            } else {
                7u32.to_be_bytes()
            })?;
            file.write_all(&[3])?;
            for index in [0i32, 1, 2] {
                file.write_all(&if little {
                    index.to_le_bytes()
                } else {
                    index.to_be_bytes()
                })?;
            }
            drop(file);
            let vertices = crate::read(&path, &section("Vertices", 1))?;
            assert_eq!(
                vertices.rows.first(),
                Some(&vec![
                    "1".to_owned(),
                    "1".to_owned(),
                    "0".to_owned(),
                    "-2".to_owned()
                ])
            );
            let faces = crate::read(&path, &section("Faces", 0))?;
            assert_eq!(
                faces
                    .rows
                    .first()
                    .and_then(|row| row.get(2))
                    .map(String::as_str),
                Some("[0, 1, 2]")
            );
            let mesh = crate::read(&path, &PreviewRequest::default())?;
            assert_eq!(mesh.rows.len(), 1);
            assert!(mesh.image.is_some());
        }
        Ok(())
    }

    #[test]
    fn huge_binary_ply_seeks_to_vertex_pages_without_building_an_index() -> Result<()> {
        use std::io::Write;
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("huge.ply");
        let mut file = File::create(&path)?;
        file.write_all(b"ply\nformat binary_little_endian 1.0\nelement vertex 10000000\nproperty float x\nproperty float y\nproperty float z\nend_header\n")?;
        let body = file.stream_position()?;
        file.set_len(body + 120_000_000)?;
        file.seek(SeekFrom::Start(body + 119_999_988))?;
        for value in [1.0f32, 2.0, 3.0] {
            file.write_all(&value.to_le_bytes())?;
        }
        drop(file);
        let overview = crate::read(&path, &PreviewRequest::default())?;
        assert!(
            overview
                .sections
                .iter()
                .any(|section| section == "Vertices")
        );
        assert!(overview.image.is_none());
        let last = crate::read(&path, &section("Vertices", 9_999_999))?;
        assert_eq!(
            last.rows,
            vec![vec![
                "9999999".to_owned(),
                "1".to_owned(),
                "2".to_owned(),
                "3".to_owned()
            ]]
        );
        assert_eq!(last.next_offset, None);
        Ok(())
    }

    #[test]
    fn malformed_ply_counts_indices_and_truncation_are_rejected() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("broken.ply");
        let header = "ply\nformat ascii 1.0\nelement vertex 3\nproperty float x\nproperty float y\nproperty float z\nelement face 1\nproperty list uint int vertex_indices\nend_header\n0 0 0\n1 0 0\n0 1 0\n";
        for face in ["4294967295\n", "3 0 1 3\n", "3 0 -1 2\n", "3 0 1\n"] {
            std::fs::write(&path, format!("{header}{face}"))?;
            assert!(read(&path, &section("Faces", 0)).is_err());
        }
        let mut input = MeshInput::new(File::open(&path)?);
        input.deadline = Instant::now() - Duration::from_secs(1);
        assert!(input.line().is_err());
        input.deadline = Instant::now() + Duration::from_secs(1);
        input.remaining = 2;
        assert!(input.exact(&mut [0; 3]).is_err());
        Ok(())
    }

    #[test]
    fn off_pages_faces_and_projects_tiny_and_degenerate_geometry() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("tiny.off");
        std::fs::write(
            &path,
            "# mesh comment\nOFF 4 1 0\n0 0 0\n0.000001 0 0\n0.000001 0.000001 0\n0 0.000001 0\n4 0 1 2 3 # quad\n",
        )?;
        let vertices = crate::read(&path, &section("Vertices", 2))?;
        assert_eq!(vertices.rows.len(), 2);
        assert_eq!(
            vertices
                .rows
                .first()
                .and_then(|row| row.first())
                .map(String::as_str),
            Some("2")
        );
        let faces = crate::read(&path, &section("Faces", 0))?;
        assert_eq!(
            faces.rows,
            vec![vec![
                "0".to_owned(),
                "4".to_owned(),
                "[0, 1, 2, 3]".to_owned()
            ]]
        );
        let mesh = crate::read(&path, &PreviewRequest::default())?;
        assert_eq!(mesh.rows.len(), 2);
        let image =
            image::load_from_memory(mesh.image.as_deref().context("wireframe PNG")?)?.to_rgba8();
        assert!(image.pixels().filter(|pixel| pixel[0] == 111).count() > 1000);
        let image = image::load_from_memory(&draw(&[[[1.0; 3]; 3]], (0, 1))?)?.to_rgba8();
        assert_eq!(image.pixels().filter(|pixel| pixel[0] == 111).count(), 1);
        std::fs::write(&path, "OFF\n3 1 0\n0 0 0\n1 0 0\n0 1 0\n3 0 1 3\n")?;
        assert!(read(&path, &section("Faces", 0)).is_err());
        Ok(())
    }

    #[test]
    fn binary_stl_seeks_into_large_geometry_without_indexing() -> Result<()> {
        use std::io::Write;
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("huge.stl");
        let mut file = File::create(&path)?;
        file.write_all(&[0; 80])?;
        file.write_all(&1_000_000u32.to_le_bytes())?;
        file.set_len(84 + 50_000_000)?;
        let page = read(
            &path,
            &PreviewRequest {
                offset: 999_900,
                ..Default::default()
            },
        )?;
        assert_eq!(page.rows.len(), 100);
        assert!(page.image.is_some());
        assert_eq!(page.next_offset, None);
        Ok(())
    }

    #[test]
    fn obj_triangulates_negative_indices_and_projects_extreme_coordinates() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("extreme.obj");
        std::fs::write(
            &path,
            "v -3e38 -3e38 0\nv 3e38 -3e38 0\nv 3e38 3e38 0\nv -3e38 3e38 0\nf -4 -3 -2 -1\n",
        )?;
        let (triangles, more) = obj(&path, &PreviewRequest::default())?;
        assert_eq!(triangles.len(), 2);
        assert!(!more);
        let image = image::load_from_memory(&draw(&triangles, (0, 1))?)?.to_rgba8();
        assert!(image.pixels().filter(|pixel| pixel[0] == 111).count() > 1000);
        std::fs::write(&path, "v 0 0 0\nv 1 1 1\nf 1 2\n")?;
        assert!(obj(&path, &PreviewRequest::default()).is_err());
        Ok(())
    }
}
