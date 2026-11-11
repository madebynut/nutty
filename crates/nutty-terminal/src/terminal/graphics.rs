//! Bounded Kitty direct-data images, chunking, placements and deletion.

use anyhow::{Context as _, Result, bail, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use std::{
    collections::HashMap,
    io::{Cursor, Read as _},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};
use uuid::Uuid;

const MAX_ENCODED: usize = 24 * 1024 * 1024;
const MAX_PIXELS: usize = 4096 * 4096;
const MAX_MEMORY: usize = 128 * 1024 * 1024;
const MAX_TOTAL_MEMORY: usize = 512 * 1024 * 1024;
#[derive(Clone, Copy)]
enum InlineFormat {
    Sixel,
    Image,
}
struct InlineImage {
    format: InlineFormat,
    encoded: Vec<u8>,
    dimensions: (u32, u32),
    display: (f32, f32),
}

fn decode_inline(bytes: &[u8]) -> Result<image::RgbaImage> {
    let mut reader = image::ImageReader::new(Cursor::new(bytes)).with_guessed_format()?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(4096);
    limits.max_image_height = Some(4096);
    limits.max_alloc = Some((MAX_PIXELS * 8) as u64);
    reader.limits(limits);
    Ok(reader.decode()?.into_rgba8())
}
static TOTAL_MEMORY: AtomicUsize = AtomicUsize::new(0);

#[derive(Debug)]
struct ImageMemory {
    bytes: AtomicUsize,
}
impl ImageMemory {
    fn reserve(bytes: usize) -> Result<Arc<Self>> {
        TOTAL_MEMORY
            .try_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                used.checked_add(bytes)
                    .filter(|total| *total <= MAX_TOTAL_MEMORY)
            })
            .map_err(|_| anyhow::anyhow!("application graphics memory budget exhausted"))?;
        Ok(Arc::new(Self {
            bytes: AtomicUsize::new(bytes),
        }))
    }
    fn grow(&self, bytes: usize) -> Result<()> {
        let previous = self.bytes.load(Ordering::Acquire);
        if bytes > previous {
            TOTAL_MEMORY
                .try_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                    used.checked_add(bytes - previous)
                        .filter(|total| *total <= MAX_TOTAL_MEMORY)
                })
                .map_err(|_| anyhow::anyhow!("application graphics memory budget exhausted"))?;
            self.bytes.store(bytes, Ordering::Release);
        }
        Ok(())
    }
    fn shrink(&self, bytes: usize) {
        let previous = self.bytes.load(Ordering::Acquire);
        if bytes < previous {
            self.bytes.store(bytes, Ordering::Release);
            TOTAL_MEMORY.fetch_sub(previous - bytes, Ordering::AcqRel);
        }
    }
}
impl Drop for ImageMemory {
    fn drop(&mut self) {
        TOTAL_MEMORY.fetch_sub(self.bytes.load(Ordering::Acquire), Ordering::AcqRel);
    }
}

/// Immutable decoded pixels; native renderers cache their own GPU representation by ID.
pub struct Raster {
    pub id: Uuid,
    pub pixels: image::RgbaImage,
}
impl std::fmt::Debug for Raster {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Raster")
            .field("id", &self.id)
            .field("dimensions", &self.pixels.dimensions())
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Debug)]
pub struct DisplayImage {
    pub image: Arc<Raster>,
    pub column: f32,
    pub row: f32,
    pub columns: f32,
    pub rows: f32,
    pub z: i32,
    pub crop: [f32; 4],
    _memory: Arc<ImageMemory>,
}
struct Stored {
    version: Uuid,
    data: Option<Arc<Raster>>,
    width: u32,
    height: u32,
    reserved: usize,
    previous: Option<Previous>,
    memory: Arc<ImageMemory>,
}
struct Previous {
    version: Uuid,
    data: Arc<Raster>,
    width: u32,
    height: u32,
    reserved: usize,
    placements: Vec<Placement>,
    memory: Arc<ImageMemory>,
}
#[derive(Clone)]
struct Placement {
    image: u32,
    id: u32,
    column: f32,
    row: f32,
    columns: f32,
    rows: f32,
    z: i32,
    crop: [f32; 4],
    alternate: bool,
}
pub(super) struct Decode {
    pub id: u32,
    version: Uuid,
    params: HashMap<String, String>,
    encoded: Vec<u8>,
    encoded_memory: Arc<ImageMemory>,
    query: bool,
    pub quiet: u8,
    file: bool,
    memory: Option<Arc<ImageMemory>>,
    inline: Option<InlineFormat>,
}
#[derive(Default)]
pub(super) struct Graphics {
    images: HashMap<u32, Stored>,
    placements: Vec<Placement>,
    current: Option<Decode>,
    next: u32,
    pub region: Option<(i32, i32)>,
    pub allow_files: bool,
    pub sixel_display_mode: bool,
    iterm: Option<ItermTransfer>,
}
struct ItermTransfer {
    attributes: HashMap<String, String>,
    encoded: Vec<u8>,
    memory: Arc<ImageMemory>,
}
pub(super) struct Outcome {
    pub decode: Option<Decode>,
    pub response: Option<String>,
    pub move_rows: usize,
    pub move_columns: usize,
}
impl Graphics {
    pub fn sixel(
        &mut self,
        bytes: &[u8],
        cursor: (usize, i32),
        alternate: bool,
        cell: (f32, f32),
    ) -> Result<Outcome> {
        let dimensions = super::inline_images::sixel_size(bytes)?;
        let cursor = if self.sixel_display_mode {
            (0, 0)
        } else {
            cursor
        };
        let mut outcome = self.inline_image(
            InlineImage {
                format: InlineFormat::Sixel,
                encoded: bytes.to_vec(),
                dimensions,
                display: (dimensions.0 as f32, dimensions.1 as f32),
            },
            cursor,
            alternate,
            cell,
        )?;
        if self.sixel_display_mode {
            outcome.move_rows = 0;
        }
        Ok(outcome)
    }
    pub fn iterm(
        &mut self,
        bytes: &[u8],
        cursor: (usize, i32),
        alternate: bool,
        cell: (f32, f32),
        viewport: (usize, usize),
    ) -> Result<Option<Outcome>> {
        let Some(bytes) = bytes.strip_prefix(b"1337;") else {
            return Ok(None);
        };
        if let Some(header) = bytes.strip_prefix(b"MultipartFile=") {
            self.iterm = None;
            self.iterm = Some(ItermTransfer {
                attributes: super::inline_images::iterm_attributes(header)?,
                encoded: Vec::new(),
                memory: ImageMemory::reserve(0)?,
            });
            return Ok(None);
        }
        if let Some(part) = bytes.strip_prefix(b"FilePart=") {
            let ItermTransfer {
                attributes,
                mut encoded,
                memory,
            } = self
                .iterm
                .take()
                .context("Missing multipart iTerm header")?;
            ensure!(
                encoded.len() + part.len() <= MAX_ENCODED,
                "iTerm transfer exceeds budget"
            );
            memory.grow(encoded.len() + part.len())?;
            encoded.extend_from_slice(part);
            memory.grow(encoded.capacity())?;
            self.iterm = Some(ItermTransfer {
                attributes,
                encoded,
                memory,
            });
            return Ok(None);
        }
        let ItermTransfer {
            attributes,
            encoded,
            memory: _memory,
        } = if bytes == b"FileEnd" {
            self.iterm.take().context("Missing multipart iTerm file")?
        } else if let Some(file) = bytes.strip_prefix(b"File=") {
            let split = file
                .iter()
                .position(|byte| *byte == b':')
                .context("Missing iTerm image payload")?;
            ItermTransfer {
                attributes: super::inline_images::iterm_attributes(&file[..split])?,
                encoded: file[split + 1..].to_vec(),
                memory: ImageMemory::reserve(file.len())?,
            }
        } else {
            return Ok(None);
        };
        ensure!(
            encoded.len() <= MAX_ENCODED,
            "iTerm transfer exceeds budget"
        );
        let raw = STANDARD.decode(encoded)?;
        if let Some(size) = attributes.get("size") {
            ensure!(
                size.parse::<usize>()? == raw.len(),
                "iTerm file size mismatch"
            );
        }
        let dimensions = image::ImageReader::new(Cursor::new(&raw))
            .with_guessed_format()?
            .into_dimensions()?;
        let mut width = super::inline_images::extent(
            attributes.get("width"),
            cell.0,
            viewport.0 as f32 * cell.0,
        )?;
        let mut height = super::inline_images::extent(
            attributes.get("height"),
            cell.1,
            viewport.1 as f32 * cell.1,
        )?;
        let aspect = dimensions.0 as f32 / dimensions.1 as f32;
        if !attributes
            .get("preserveAspectRatio")
            .is_some_and(|value| value == "0")
        {
            match (width, height) {
                (Some(w), Some(h)) => {
                    let scale = (w / dimensions.0 as f32).min(h / dimensions.1 as f32);
                    width = Some(dimensions.0 as f32 * scale);
                    height = Some(dimensions.1 as f32 * scale);
                }
                (Some(w), None) => height = Some(w / aspect),
                (None, Some(h)) => width = Some(h * aspect),
                _ => {}
            }
        }
        self.inline_image(
            InlineImage {
                format: InlineFormat::Image,
                encoded: raw,
                dimensions,
                display: (
                    width.unwrap_or(dimensions.0 as f32),
                    height.unwrap_or(dimensions.1 as f32),
                ),
            },
            cursor,
            alternate,
            cell,
        )
        .map(Some)
    }
    fn inline_image(
        &mut self,
        image: InlineImage,
        cursor: (usize, i32),
        alternate: bool,
        cell: (f32, f32),
    ) -> Result<Outcome> {
        let InlineImage {
            format,
            encoded,
            dimensions,
            display,
        } = image;
        ensure!(
            encoded.len() <= MAX_ENCODED
                && dimensions.0 > 0
                && dimensions.1 > 0
                && dimensions.0 <= 4096
                && dimensions.1 <= 4096,
            "Inline image exceeds budget"
        );
        ensure!(
            display.0.is_finite()
                && display.1.is_finite()
                && display.0 > 0.
                && display.1 > 0.
                && display.0 / cell.0 <= 4096.
                && display.1 / cell.1 <= 4096.,
            "Inline placement exceeds budget"
        );
        self.next = self.next.wrapping_add(1).max(1);
        while self.images.contains_key(&self.next) {
            self.next = self.next.wrapping_add(1).max(1);
        }
        let id = self.next;
        let params = HashMap::from([
            ("a".into(), "T".into()),
            ("s".into(), dimensions.0.to_string()),
            ("v".into(), dimensions.1.to_string()),
            ("c".into(), (display.0 / cell.0).ceil().to_string()),
            ("r".into(), (display.1 / cell.1).ceil().to_string()),
        ]);
        let decode = Decode {
            id,
            version: Uuid::new_v4(),
            params,
            encoded_memory: ImageMemory::reserve(encoded.capacity())?,
            encoded,
            memory: None,
            query: false,
            quiet: 2,
            file: false,
            inline: Some(format),
        };
        let mut outcome = self.finish(decode, cursor, alternate, cell)?;
        if let Some(placement) = self
            .placements
            .iter_mut()
            .find(|placement| placement.image == id)
        {
            placement.columns = display.0 / cell.0;
            placement.rows = display.1 / cell.1;
        }
        outcome.move_columns = 0;
        Ok(outcome)
    }
    pub fn has_placements(&self) -> bool {
        !self.placements.is_empty()
    }
    pub fn handle(
        &mut self,
        bytes: &[u8],
        cursor: (usize, i32),
        alternate: bool,
        cell: (f32, f32),
    ) -> Outcome {
        let params_text = bytes.split(|byte| *byte == b';').next().unwrap_or_default();
        let header = std::str::from_utf8(params_text).unwrap_or_default();
        let mut params = HashMap::new();
        for pair in header.split(',') {
            if let Some((key, value)) = pair.split_once('=') {
                params.insert(key.to_string(), value.to_string());
            }
        }
        let quiet = number(&params, "q", 0).min(2) as u8;
        let id = number(&params, "i", 0);
        let result = self.command(
            params,
            bytes.get(params_text.len() + 1..).unwrap_or_default(),
            cursor,
            alternate,
            cell,
        );
        match result {
            Ok(mut outcome) => {
                if outcome.response.is_some() && quiet > 0 {
                    outcome.response = None;
                }
                outcome
            }
            Err(error) => Outcome {
                decode: None,
                response: (quiet < 2).then(|| reply(id, &format!("EINVAL:{error}"))),
                move_rows: 0,
                move_columns: 0,
            },
        }
    }
    fn command(
        &mut self,
        mut params: HashMap<String, String>,
        payload: &[u8],
        cursor: (usize, i32),
        alternate: bool,
        cell: (f32, f32),
    ) -> Result<Outcome> {
        ensure!(
            params.len() <= 32
                && params
                    .iter()
                    .all(|(key, value)| key.len() <= 2 && value.len() <= 32),
            "invalid graphics header"
        );
        let action = params.get("a").map(String::as_str).unwrap_or("t");
        if action == "q"
            && let Some(feature) = params.get("e")
        {
            let id = number(&params, "i", 0);
            let mut keys = String::new();
            if matches!(feature.as_str(), "t" | "T" | "p" | "d" | "q") {
                keys = format!(",e={feature}");
                if number(&params, "C", 0) == 1 {
                    keys.push_str(",C=1");
                }
                if let Some(medium) = params.get("t")
                    && (medium == "d" || (self.allow_files && matches!(medium.as_str(), "f" | "t")))
                {
                    keys.push_str(&format!(",t={medium}"));
                }
            }
            return Ok(Outcome {
                decode: None,
                response: Some(format!("\x1b_Gi={id}{keys};OK\x1b\\")),
                move_rows: 0,
                move_columns: 0,
            });
        }
        if action == "d" {
            let id = number(&params, "i", 0);
            let placement = number(&params, "p", 0);
            let mode = params.get("d").map(String::as_str).unwrap_or("a");
            match mode {
                "a" | "A" => {
                    self.placements.retain(|item| item.alternate != alternate);
                    if mode == "A" {
                        self.images
                            .retain(|id, _| self.placements.iter().any(|item| item.image == *id));
                        self.current = None;
                    }
                }
                "i" | "I" => {
                    self.placements.retain(|item| {
                        item.image != id || (placement != 0 && item.id != placement)
                    });
                    if mode == "I" {
                        self.images.remove(&id);
                    }
                }
                "p" | "P" => {
                    let x = number(&params, "x", 1) as f32 - 1.;
                    let y = number(&params, "y", 1) as f32 - 1.;
                    self.placements.retain(|item| {
                        item.alternate != alternate
                            || x < item.column
                            || x >= item.column + item.columns
                            || y < item.row
                            || y >= item.row + item.rows
                    });
                }
                "z" | "Z" => {
                    let z = signed(&params, "z", 0);
                    self.placements
                        .retain(|item| item.alternate != alternate || item.z != z);
                }
                _ => bail!("unsupported deletion mode"),
            }
            return Ok(Outcome {
                decode: None,
                response: None,
                move_rows: 0,
                move_columns: 0,
            });
        }
        if action == "p" {
            let id = number(&params, "i", 0);
            let (columns, rows) = self.place(id, &params, cursor, alternate, cell)?;
            return Ok(Outcome {
                decode: None,
                response: Some(reply(id, "OK")),
                move_rows: rows,
                move_columns: columns,
            });
        }
        ensure!(
            matches!(action, "t" | "T" | "q"),
            "unsupported graphics action"
        );
        let mut transmission = if let Some(mut current) = self.current.take() {
            ensure!(
                number(&params, "i", current.id) == current.id,
                "interleaved image chunks"
            );
            for key in ["q", "m"] {
                if let Some(value) = params.remove(key) {
                    current.params.insert(key.into(), value);
                }
            }
            current
        } else {
            let medium = params.get("t").map(String::as_str).unwrap_or("d");
            ensure!(
                matches!(medium, "d" | "f" | "t"),
                "unsupported image transfer medium"
            );
            ensure!(
                medium == "d" || self.allow_files,
                "file graphics require permission for this local pane"
            );
            let format = number(&params, "f", 32);
            ensure!(matches!(format, 24 | 32 | 100), "unsupported image format");
            let compression = params.get("o").map(String::as_str).unwrap_or("");
            ensure!(matches!(compression, "" | "z"), "unsupported compression");
            ensure!(
                number(&params, "U", 0) == 0,
                "unicode placeholders are not supported"
            );
            let mut id = number(&params, "i", 0);
            if id == 0 {
                self.next = self.next.wrapping_add(1).max(1);
                while self.images.contains_key(&self.next) {
                    self.next = self.next.wrapping_add(1).max(1);
                }
                id = self.next;
            }
            Decode {
                id,
                version: Uuid::new_v4(),
                quiet: number(&params, "q", 0).min(2) as u8,
                query: action == "q",
                file: medium != "d",
                params,
                encoded: Vec::new(),
                encoded_memory: ImageMemory::reserve(0)?,
                memory: None,
                inline: None,
            }
        };
        ensure!(
            transmission.encoded.len() + payload.len() <= MAX_ENCODED,
            "image transfer exceeds 24 MiB"
        );
        let required = transmission.encoded.len() + payload.len();
        if transmission.encoded.capacity() < required {
            let capacity = required
                .max(transmission.encoded.capacity().saturating_mul(2))
                .min(MAX_ENCODED);
            transmission.encoded_memory.grow(capacity)?;
            transmission
                .encoded
                .try_reserve_exact(capacity - transmission.encoded.len())?;
            transmission
                .encoded_memory
                .grow(transmission.encoded.capacity())?;
        }
        transmission.encoded.extend_from_slice(payload);
        ensure!(
            !transmission.file || transmission.encoded.len() <= 8192,
            "image filename exceeds budget"
        );
        if number(&transmission.params, "m", 0) == 1 {
            self.current = Some(transmission);
            return Ok(Outcome {
                decode: None,
                response: None,
                move_rows: 0,
                move_columns: 0,
            });
        }
        if !transmission.file && number(&transmission.params, "f", 32) == 100 {
            let compressed = transmission
                .params
                .get("o")
                .is_some_and(|value| value == "z");
            let mut length = transmission.encoded.len().min(64) / 4 * 4;
            let header = loop {
                let raw = STANDARD
                    .decode(&transmission.encoded[..length])
                    .context("invalid PNG header encoding")?;
                if !compressed {
                    break raw;
                }
                let mut expanded = Vec::new();
                let result = flate2::read::ZlibDecoder::new(raw.as_slice())
                    .take(24)
                    .read_to_end(&mut expanded);
                if expanded.len() >= 24 {
                    break expanded;
                }
                if length == transmission.encoded.len() {
                    result?;
                    bail!("incomplete compressed PNG header");
                }
                length = (length.max(4) * 4).min(transmission.encoded.len());
            };
            ensure!(
                header.len() >= 24 && header[..8] == [137, 80, 78, 71, 13, 10, 26, 10],
                "invalid PNG header"
            );
            let width = u32::from_be_bytes(header[16..20].try_into()?);
            let height = u32::from_be_bytes(header[20..24].try_into()?);
            ensure!(
                width > 0 && height > 0 && width <= 4096 && height <= 4096,
                "PNG dimensions exceed budget"
            );
            transmission.params.insert("s".into(), width.to_string());
            transmission.params.insert("v".into(), height.to_string());
        }
        self.finish(transmission, cursor, alternate, cell)
    }
    fn finish(
        &mut self,
        mut transmission: Decode,
        cursor: (usize, i32),
        alternate: bool,
        cell: (f32, f32),
    ) -> Result<Outcome> {
        let width = number(&transmission.params, "s", 0);
        let height = number(&transmission.params, "v", 0);
        let reserve = if transmission.file && number(&transmission.params, "f", 32) == 100 {
            MAX_PIXELS * 8
        } else {
            ensure!(
                width > 0 && height > 0 && width <= 4096 && height <= 4096,
                "invalid dimensions"
            );
            width as usize * height as usize * 8
        };
        let used: usize = self
            .images
            .iter()
            .filter(|(id, _)| **id != transmission.id)
            .map(|(_, image)| {
                image.reserved + image.previous.as_ref().map(|old| old.reserved).unwrap_or(0)
            })
            .sum();
        let old_reserve =
            self.images
                .get(&transmission.id)
                .map(|image| {
                    image.previous.as_ref().map(|old| old.reserved).unwrap_or(
                        if image.data.is_some() {
                            image.reserved
                        } else {
                            0
                        },
                    )
                })
                .unwrap_or(0);
        ensure!(
            used + reserve + old_reserve <= MAX_MEMORY
                && (self.images.len() < 128 || self.images.contains_key(&transmission.id)),
            "graphics memory budget exhausted"
        );
        let memory = ImageMemory::reserve(reserve)?;
        transmission.memory = Some(memory.clone());
        if transmission.query && !transmission.file {
            ensure!(
                transmission.encoded.len() <= 16 * 1024 && reserve <= 256 * 256 * 8,
                "query payload exceeds probe budget"
            );
            transmission.pixels()?;
            return Ok(Outcome {
                decode: None,
                response: Some(reply(transmission.id, "OK")),
                move_rows: 0,
                move_columns: 0,
            });
        }
        if transmission.query {
            return Ok(Outcome {
                decode: Some(transmission),
                response: None,
                move_rows: 0,
                move_columns: 0,
            });
        }
        let previous = self.images.remove(&transmission.id).and_then(|image| {
            image.previous.or_else(|| {
                image.data.map(|data| Previous {
                    version: image.version,
                    data,
                    width: image.width,
                    height: image.height,
                    reserved: image.reserved,
                    memory: image.memory,
                    placements: self
                        .placements
                        .iter()
                        .filter(|item| item.image == transmission.id)
                        .cloned()
                        .collect(),
                })
            })
        });
        self.images.insert(
            transmission.id,
            Stored {
                version: transmission.version,
                data: previous.as_ref().map(|old| old.data.clone()),
                width,
                height,
                reserved: reserve,
                previous,
                memory,
            },
        );
        let (columns, rows) = if transmission
            .params
            .get("a")
            .is_some_and(|action| action == "T")
        {
            match self.place(
                transmission.id,
                &transmission.params,
                cursor,
                alternate,
                cell,
            ) {
                Ok(placement) => placement,
                Err(error) => {
                    self.rollback(transmission.id);
                    return Err(error);
                }
            }
        } else {
            (0, 0)
        };
        Ok(Outcome {
            decode: Some(transmission),
            response: None,
            move_rows: rows,
            move_columns: columns,
        })
    }
    fn rollback(&mut self, id: u32) {
        let previous = self.images.remove(&id).and_then(|stored| stored.previous);
        self.placements.retain(|item| item.image != id);
        if let Some(previous) = previous {
            self.placements.extend(previous.placements);
            self.images.insert(
                id,
                Stored {
                    version: previous.version,
                    data: Some(previous.data),
                    width: previous.width,
                    height: previous.height,
                    reserved: previous.reserved,
                    previous: None,
                    memory: previous.memory,
                },
            );
        }
    }
    fn place(
        &mut self,
        id: u32,
        params: &HashMap<String, String>,
        cursor: (usize, i32),
        alternate: bool,
        cell: (f32, f32),
    ) -> Result<(usize, usize)> {
        let placement = number(params, "p", 0);
        ensure!(
            self.placements.len() < 512
                || (placement != 0
                    && self
                        .placements
                        .iter()
                        .any(|item| item.image == id && item.id == placement)),
            "too many image placements"
        );
        let image = self.images.get(&id).context("image id does not exist")?;
        let columns = number(params, "c", 0);
        let rows = number(params, "r", 0);
        let width = number(params, "w", image.width);
        let height = number(params, "h", image.height);
        let column = cursor.0 as f32 + number(params, "X", 0) as f32 / cell.0;
        let row = cursor.1 as f32 + number(params, "Y", 0) as f32 / cell.1;
        let columns = if columns == 0 {
            width as f32 / cell.0
        } else {
            columns as f32
        };
        let rows = if rows == 0 {
            height as f32 / cell.1
        } else {
            rows as f32
        };
        ensure!(
            columns <= 4096. && rows <= 4096.,
            "placement dimensions too large"
        );
        let placement = number(params, "p", 0);
        if placement != 0 {
            self.placements
                .retain(|item| item.image != id || item.id != placement);
        }
        self.placements.push(Placement {
            image: id,
            id: placement,
            column,
            row,
            columns,
            rows,
            z: signed(params, "z", 0),
            crop: [
                number(params, "x", 0) as f32,
                number(params, "y", 0) as f32,
                width as f32,
                height as f32,
            ],
            alternate,
        });
        Ok(if number(params, "C", 0) == 1 {
            (0, 0)
        } else {
            (columns.ceil() as usize, rows.ceil() as usize)
        })
    }
    pub fn install(
        &mut self,
        decode: &Decode,
        result: Result<image::RgbaImage>,
        cell: (f32, f32),
    ) -> Option<String> {
        if decode.query {
            let text = match result {
                Ok(_) => "OK".to_string(),
                Err(error) => format!("EINVAL:{error}"),
            };
            return (decode.quiet == 0 || (decode.quiet == 1 && text != "OK"))
                .then(|| reply(decode.id, &text));
        }
        if self
            .images
            .get(&decode.id)
            .is_none_or(|image| image.version != decode.version)
        {
            return None;
        }
        let text = match result {
            Ok(data) => {
                let (width, height) = data.dimensions();
                if decode.query {
                    self.images.remove(&decode.id);
                } else if let Some(stored) = self.images.get_mut(&decode.id) {
                    stored.width = width;
                    stored.height = height;
                    stored.reserved = data.len() * 2;
                    stored.memory.shrink(data.len() * 2);
                    stored.previous = None;
                    stored.data = Some(Arc::new(Raster {
                        id: decode.version,
                        pixels: data,
                    }));
                    for placement in self
                        .placements
                        .iter_mut()
                        .filter(|placement| placement.image == decode.id)
                    {
                        if placement.crop[2] == 0. {
                            placement.crop[2] = width as f32;
                            placement.columns = width as f32 / cell.0;
                        }
                        if placement.crop[3] == 0. {
                            placement.crop[3] = height as f32;
                            placement.rows = height as f32 / cell.1;
                        }
                    }
                }
                "OK".into()
            }
            Err(error) => {
                self.rollback(decode.id);
                format!("EINVAL:{error}")
            }
        };
        (decode.quiet == 0 || (decode.quiet == 1 && text != "OK")).then(|| reply(decode.id, &text))
    }
    pub fn snapshot(&self, offset: usize, alternate: bool) -> Vec<DisplayImage> {
        let mut images: Vec<_> = self
            .placements
            .iter()
            .filter(|placement| {
                placement.alternate == alternate
                    && placement.row + offset as f32 + placement.rows >= 0.
                    && placement.row + (offset as f32) < 500.
            })
            .filter_map(|placement| {
                let stored = self.images.get(&placement.image)?;
                Some(DisplayImage {
                    image: stored.data.clone()?,
                    column: placement.column,
                    row: placement.row + offset as f32,
                    columns: placement.columns,
                    rows: placement.rows,
                    z: placement.z,
                    crop: placement.crop,
                    _memory: stored
                        .previous
                        .as_ref()
                        .map(|previous| previous.memory.clone())
                        .unwrap_or_else(|| stored.memory.clone()),
                })
            })
            .collect();
        images.sort_by_key(|image| image.z);
        images
    }
    pub fn scroll(
        &mut self,
        top: i32,
        bottom: i32,
        amount: i32,
        alternate: bool,
        history: usize,
        lines: usize,
    ) {
        if amount == 0 || top > bottom {
            return;
        }
        let history_scroll = top == 0 && bottom == lines as i32 - 1 && !alternate && amount > 0;
        self.placements.retain_mut(|placement| {
            if placement.alternate != alternate {
                return true;
            }
            if history_scroll {
                placement.row -= amount as f32;
                placement.row + placement.rows >= -(history as f32)
            } else if placement.row >= top as f32 && placement.row <= bottom as f32 {
                placement.row -= amount as f32;
                placement.row + placement.rows > top as f32 && placement.row <= bottom as f32
            } else {
                true
            }
        });
    }
    pub fn clear_alternate(&mut self) {
        self.placements.retain(|item| !item.alternate);
    }
}
fn number(params: &HashMap<String, String>, key: &str, default: u32) -> u32 {
    params
        .get(key)
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}
fn signed(params: &HashMap<String, String>, key: &str, default: i32) -> i32 {
    params
        .get(key)
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}
fn reply(id: u32, text: &str) -> String {
    format!("\x1b_Gi={id};{text}\x1b\\")
}
impl Decode {
    pub fn reads_file(&self) -> bool {
        self.file
    }
    pub fn pixels(&self) -> Result<image::RgbaImage> {
        if let Some(format) = self.inline {
            return match format {
                InlineFormat::Sixel => super::inline_images::sixel_pixels(&self.encoded),
                InlineFormat::Image => decode_inline(&self.encoded),
            };
        }
        let bytes = STANDARD
            .decode(&self.encoded)
            .context("invalid base64 image data")?;
        let bytes = if self.file {
            let path = std::str::from_utf8(&bytes).context("image filename is not UTF-8")?;
            ensure!(
                path.len() <= 4096 && !path.contains('\0'),
                "invalid image filename"
            );
            let path = std::fs::canonicalize(path)?;
            ensure!(
                std::fs::metadata(&path)?.is_file(),
                "image payload is not a regular file"
            );
            let mut options = std::fs::OpenOptions::new();
            options.read(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt as _;
                options.custom_flags(nix::libc::O_NONBLOCK | nix::libc::O_NOFOLLOW);
            }
            let file = options.open(&path)?;
            let metadata = file.metadata()?;
            ensure!(
                metadata.is_file(),
                "image file exceeds budget or is not regular"
            );
            let mut bytes = Vec::new();
            use std::io::Seek as _;
            let mut file = file;
            file.seek(std::io::SeekFrom::Start(number(&self.params, "O", 0) as u64))?;
            let count = number(&self.params, "S", 0);
            file.take(if count == 0 {
                MAX_ENCODED as u64 + 1
            } else {
                u64::from(count).min(MAX_ENCODED as u64 + 1)
            })
            .read_to_end(&mut bytes)?;
            ensure!(bytes.len() <= MAX_ENCODED, "image file exceeds budget");
            if self.params.get("t").is_some_and(|medium| medium == "t")
                && path.to_string_lossy().contains("tty-graphics-protocol")
                && [
                    std::env::temp_dir(),
                    std::path::PathBuf::from("/tmp"),
                    std::path::PathBuf::from("/dev/shm"),
                ]
                .iter()
                .any(|root| std::fs::canonicalize(root).is_ok_and(|root| path.starts_with(root)))
            {
                std::fs::remove_file(path)?;
            }
            bytes
        } else {
            bytes
        };
        let bytes = if self.params.get("o").is_some_and(|value| value == "z") {
            let mut expanded = Vec::new();
            flate2::read::ZlibDecoder::new(bytes.as_slice())
                .take((MAX_PIXELS * 4 + 1) as u64)
                .read_to_end(&mut expanded)?;
            ensure!(
                expanded.len() <= MAX_PIXELS * 4,
                "decompressed image too large"
            );
            expanded
        } else {
            bytes
        };
        let width = number(&self.params, "s", 0);
        let height = number(&self.params, "v", 0);
        match number(&self.params, "f", 32) {
            100 => {
                let mut reader =
                    image::ImageReader::with_format(Cursor::new(bytes), image::ImageFormat::Png);
                let mut limits = image::Limits::default();
                limits.max_image_width = Some(4096);
                limits.max_image_height = Some(4096);
                limits.max_alloc = Some((MAX_PIXELS * 4) as u64);
                reader.limits(limits);
                Ok(reader.decode()?.into_rgba8())
            }
            24 => {
                ensure!(
                    bytes.len() == width as usize * height as usize * 3,
                    "RGB image length mismatch"
                );
                let pixels =
                    image::RgbImage::from_raw(width, height, bytes).context("invalid RGB image")?;
                Ok(image::DynamicImage::ImageRgb8(pixels).into_rgba8())
            }
            32 => {
                ensure!(
                    bytes.len() == width as usize * height as usize * 4,
                    "RGBA image length mismatch"
                );
                image::RgbaImage::from_raw(width, height, bytes).context("invalid RGBA image")
            }
            _ => bail!("unsupported image format"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn graphics_budget_outlives_deleted_images_until_render_snapshots_are_released() -> Result<()> {
        assert!(ImageMemory::reserve(MAX_TOTAL_MEMORY + 1).is_err());
        let mut graphics = Graphics::default();
        let outcome = graphics.handle(
            b"a=T,i=71,f=32,s=1,v=1,C=1;AQIDBA==",
            (0, 0),
            false,
            (8., 20.),
        );
        let decode = outcome.decode.context("image decode")?;
        let pixel_memory = Arc::downgrade(decode.memory.as_ref().context("pixel budget")?);
        let encoded_memory = Arc::downgrade(&decode.encoded_memory);
        graphics.install(&decode, decode.pixels(), (8., 20.));
        drop(decode);
        assert!(
            encoded_memory.upgrade().is_none(),
            "transmission budget is released after decoding"
        );
        let snapshot = graphics.snapshot(0, false);
        graphics.handle(b"a=d,d=I,i=71", (0, 0), false, (8., 20.));
        assert!(
            pixel_memory.upgrade().is_some(),
            "a drawing frame still owns the pixels"
        );
        drop(snapshot);
        assert!(
            pixel_memory.upgrade().is_none(),
            "the last frame releases its pixel budget"
        );
        Ok(())
    }

    #[test]
    fn failed_replacement_restores_pixels_and_placements_and_probe_is_immediate() -> Result<()> {
        let mut graphics = Graphics::default();
        let original = graphics.handle(
            b"a=T,i=5,f=32,s=1,v=1,c=3,r=2,C=1;AQIDBA==",
            (2, 4),
            false,
            (8., 20.),
        );
        let decode = original.decode.context("initial upload")?;
        graphics.install(&decode, decode.pixels(), (8., 20.));
        let initial = graphics.snapshot(0, false).remove(0);
        let replacement = graphics.handle(
            b"a=T,i=5,f=32,s=1,v=1,c=7,r=8,C=1;AA==",
            (9, 9),
            false,
            (8., 20.),
        );
        let decode = replacement
            .decode
            .context("invalid replacement reaches decoder")?;
        assert!(decode.pixels().is_err());
        assert!(
            graphics
                .install(&decode, decode.pixels(), (8., 20.))
                .is_some()
        );
        let restored = graphics.snapshot(0, false).remove(0);
        assert_eq!(
            (
                restored.column,
                restored.row,
                restored.columns,
                restored.rows
            ),
            (initial.column, initial.row, initial.columns, initial.rows)
        );
        assert_eq!(
            restored.image.pixels.as_raw(),
            initial.image.pixels.as_raw()
        );
        let probe = graphics.handle(b"a=q,i=31,f=24,s=1,v=1;AAAA", (0, 0), false, (8., 20.));
        assert!(
            probe.decode.is_none(),
            "small probes respond before later device-attribute replies"
        );
        assert_eq!(probe.response, Some(reply(31, "OK")));
        let feature = graphics.handle(b"a=q,i=31,e=T,U=1,C=1", (0, 0), false, (8., 20.));
        assert_eq!(
            feature.response,
            Some("\x1b_Gi=31,e=T,C=1;OK\x1b\\".into()),
            "unsupported placeholders must not be advertised"
        );
        Ok(())
    }

    #[test]
    fn remote_file_images_are_denied_and_local_temporary_cleanup_is_scoped() -> Result<()> {
        let directory = tempfile::tempdir()?;
        for temporary in [false, true] {
            let path = directory.path().join(if temporary {
                "tty-graphics-protocol-fixture.png"
            } else {
                "image.png"
            });
            image::RgbaImage::from_pixel(2, 3, image::Rgba([1, 2, 3, 255])).save(&path)?;
            let frame = format!(
                "a=T,i=4,f=100,t={},C=1;{}",
                if temporary { "t" } else { "f" },
                STANDARD.encode(path.to_string_lossy().as_bytes())
            );
            let mut graphics = Graphics::default();
            assert!(
                graphics
                    .handle(frame.as_bytes(), (0, 0), false, (8., 20.))
                    .decode
                    .is_none()
            );
            assert!(
                path.exists(),
                "denied file access never opens or deletes the payload"
            );
            graphics.allow_files = true;
            let decode = graphics
                .handle(frame.as_bytes(), (0, 0), false, (8., 20.))
                .decode
                .context("local file upload")?;
            let pixels = decode.pixels()?;
            assert_eq!(pixels.dimensions(), (2, 3));
            graphics.install(&decode, Ok(pixels), (8., 20.));
            assert_eq!(graphics.snapshot(0, false).len(), 1);
            assert_eq!(path.exists(), !temporary);
        }
        Ok(())
    }
    #[test]
    fn chunked_rgba_images_keep_placements_and_respond_after_decode() -> Result<()> {
        let mut graphics = Graphics::default();
        let first = graphics.handle(
            b"a=T,i=7,p=2,f=32,s=2,v=1,c=4,r=2,C=1,m=1;AQID",
            (3, 4),
            false,
            (8., 20.),
        );
        assert!(first.decode.is_none() && first.response.is_none());
        let last = graphics.handle(b"m=0;BAUGBwg=", (9, 9), false, (8., 20.));
        let decode = last.decode.context("completed transmission")?;
        assert_eq!(decode.pixels()?.as_raw(), &[1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(
            graphics.install(&decode, decode.pixels(), (8., 20.)),
            Some(reply(7, "OK"))
        );
        let images = graphics.snapshot(0, false);
        assert_eq!(images.len(), 1);
        assert_eq!((images[0].column, images[0].row), (9., 9.));
        assert_eq!(images[0].image.pixels.as_raw(), &[1, 2, 3, 4, 5, 6, 7, 8]);
        graphics.scroll(0, 29, 3, false, 1000, 30);
        assert_eq!(graphics.snapshot(0, false)[0].row, 6.);
        graphics.handle(b"a=d,d=I,i=7", (0, 0), false, (8., 20.));
        assert!(graphics.snapshot(0, false).is_empty());
        assert!(graphics.images.is_empty());
        Ok(())
    }
    #[test]
    fn png_and_zlib_are_bounded_and_bad_transfers_cannot_replace_decoded_images() -> Result<()> {
        let mut png = Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
            2,
            3,
            image::Rgba([10, 20, 30, 255]),
        ))
        .write_to(&mut png, image::ImageFormat::Png)?;
        for compressed in [false, true] {
            let data = if compressed {
                use std::io::Write as _;
                let mut encoder =
                    flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
                encoder.write_all(png.get_ref())?;
                encoder.finish()?
            } else {
                png.get_ref().clone()
            };
            let frame = format!(
                "a=T,i=1,f=100,q=1,C=1{};{}",
                if compressed { ",o=z" } else { "" },
                STANDARD.encode(data)
            );
            let mut graphics = Graphics::default();
            let outcome = graphics.handle(frame.as_bytes(), (0, 0), false, (8., 20.));
            let decode = outcome.decode.with_context(|| {
                format!(
                    "PNG transmission compressed={compressed}, response={:?}",
                    outcome.response
                )
            })?;
            assert_eq!(decode.pixels()?.dimensions(), (2, 3));
            assert!(
                graphics
                    .install(&decode, decode.pixels(), (8., 20.))
                    .is_none()
            );
            assert_eq!(graphics.snapshot(0, false).len(), 1);
        }
        let mut graphics = Graphics::default();
        let denied = graphics.handle(
            b"a=T,i=3,f=32,s=99999,v=99999;AA==",
            (0, 0),
            false,
            (8., 20.),
        );
        assert!(denied.decode.is_none() && denied.response.is_some());
        assert!(graphics.images.is_empty());
        let denied = graphics.handle(b"a=T,i=3,t=f;L2V0Yy9wYXNzd2Q=", (0, 0), false, (8., 20.));
        assert!(denied.decode.is_none());
        Ok(())
    }
}
