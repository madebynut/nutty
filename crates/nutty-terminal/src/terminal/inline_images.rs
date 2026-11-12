//! Sixel raster decoding and iTerm inline-image metadata, bounded to 4096² pixels.

use anyhow::{Context as _, Result, bail, ensure};
use std::collections::HashMap;

const LIMIT: u32 = 4096;
/// Pixel writes per image: sixteen full-size overdraws, far above what encoders emit.
const MAX_PAINTED: u64 = 16 * LIMIT as u64 * LIMIT as u64;

fn number(bytes: &[u8], index: &mut usize) -> Result<u32> {
    let start = *index;
    let mut value = 0_u32;
    while bytes.get(*index).is_some_and(u8::is_ascii_digit) {
        value = value
            .checked_mul(10)
            .and_then(|n| n.checked_add(u32::from(bytes[*index] - b'0')))
            .context("Sixel number overflow")?;
        *index += 1;
    }
    ensure!(*index > start, "Missing Sixel number");
    Ok(value)
}

fn parameters(bytes: &[u8], index: &mut usize) -> Result<Vec<u32>> {
    let mut values = vec![number(bytes, index)?];
    while bytes.get(*index) == Some(&b';') {
        *index += 1;
        ensure!(values.len() < 5, "Too many Sixel parameters");
        values.push(number(bytes, index)?);
    }
    Ok(values)
}

/// Walks raster commands without allocating a bitmap during terminal parsing.
pub(super) fn sixel_size(bytes: &[u8]) -> Result<(u32, u32)> {
    let mut size = (0, 0);
    walk_sixel(bytes, |command| {
        match command {
            Raster::Size(w, h) => {
                size.0 = size.0.max(w);
                size.1 = size.1.max(h);
            }
            Raster::Paint { x, y, count, bits } => {
                size.0 = size.0.max(x + count);
                size.1 = size.1.max(y + (8 - bits.leading_zeros()).max(1));
            }
            Raster::Color(..) | Raster::Select(..) => {}
        }
        Ok(())
    })?;
    ensure!(
        size.0 > 0 && size.1 > 0 && size.0 <= LIMIT && size.1 <= LIMIT,
        "Sixel dimensions exceed budget"
    );
    Ok(size)
}

enum Raster {
    Size(u32, u32),
    Color(usize, [u8; 4]),
    Select(usize),
    Paint {
        x: u32,
        y: u32,
        count: u32,
        bits: u8,
    },
}

fn walk_sixel(bytes: &[u8], mut visit: impl FnMut(Raster) -> Result<()>) -> Result<()> {
    let q = bytes
        .iter()
        .position(|byte| *byte == b'q')
        .context("Missing Sixel header")?;
    ensure!(
        bytes[..q]
            .iter()
            .all(|byte| byte.is_ascii_digit() || *byte == b';'),
        "Invalid Sixel header"
    );
    let (mut index, mut x, mut y) = (q + 1, 0_u32, 0_u32);
    // Each run is bounded, but `$` rewinds to column zero, so a hostile frame can
    // repaint the same band indefinitely. Cap the total work, not only the extent.
    let mut painted = 0_u64;
    while index < bytes.len() {
        let byte = bytes[index];
        index += 1;
        match byte {
            b'"' => {
                let values = parameters(bytes, &mut index)?;
                ensure!(
                    values.len() == 4 && values[2] <= LIMIT && values[3] <= LIMIT,
                    "Invalid Sixel raster attributes"
                );
                visit(Raster::Size(values[2], values[3]))?;
            }
            b'#' => {
                let values = parameters(bytes, &mut index)?;
                ensure!(values[0] < 256, "Sixel palette exceeds 256 colors");
                let color = values[0] as usize;
                if values.len() == 5 {
                    let rgba = match values[1] {
                        2 => {
                            ensure!(
                                values[2..].iter().all(|v| *v <= 100),
                                "Invalid RGB percentage"
                            );
                            [
                                ((values[2] * 255 + 50) / 100) as u8,
                                ((values[3] * 255 + 50) / 100) as u8,
                                ((values[4] * 255 + 50) / 100) as u8,
                                255,
                            ]
                        }
                        1 => {
                            ensure!(
                                values[2] <= 360 && values[3] <= 100 && values[4] <= 100,
                                "Invalid HLS color"
                            );
                            hls(values[2], values[3], values[4])
                        }
                        _ => bail!("Unsupported Sixel color space"),
                    };
                    visit(Raster::Color(color, rgba))?;
                } else {
                    ensure!(values.len() == 1, "Invalid Sixel palette command");
                }
                visit(Raster::Select(color))?;
            }
            b'$' => x = 0,
            b'-' => {
                x = 0;
                y = y.checked_add(6).context("Sixel height overflow")?;
                ensure!(y < LIMIT, "Sixel height exceeds budget");
            }
            b'!' | b'?'..=b'~' => {
                let (count, bits) = if byte == b'!' {
                    let count = number(bytes, &mut index)?;
                    let byte = *bytes.get(index).context("Missing Sixel repeat data")?;
                    index += 1;
                    ensure!((b'?'..=b'~').contains(&byte), "Invalid Sixel repeat data");
                    (count, byte - b'?')
                } else {
                    (1, byte - b'?')
                };
                ensure!(
                    count > 0
                        && count <= LIMIT
                        && x + count <= LIMIT
                        && y + (8 - bits.leading_zeros()).max(1) <= LIMIT,
                    "Sixel repeat exceeds budget"
                );
                painted += u64::from(count) * u64::from(bits.count_ones());
                ensure!(painted <= MAX_PAINTED, "Sixel image exceeds drawing budget");
                visit(Raster::Paint { x, y, count, bits })?;
                x += count;
            }
            b'\r' | b'\n' | b'\t' => {}
            _ => bail!("Invalid Sixel raster byte"),
        }
    }
    Ok(())
}

fn hls(hue: u32, light: u32, saturation: u32) -> [u8; 4] {
    let h = ((hue + 240) % 360) as f32 / 60.;
    let l = light as f32 / 100.;
    let s = saturation as f32 / 100.;
    let c = (1. - (2. * l - 1.).abs()) * s;
    let x = c * (1. - (h % 2. - 1.).abs());
    let m = l - c / 2.;
    let rgb = match h as u8 {
        0 => [c, x, 0.],
        1 => [x, c, 0.],
        2 => [0., c, x],
        3 => [0., x, c],
        4 => [x, 0., c],
        _ => [c, 0., x],
    };
    [
        ((rgb[0] + m) * 255.).round() as u8,
        ((rgb[1] + m) * 255.).round() as u8,
        ((rgb[2] + m) * 255.).round() as u8,
        255,
    ]
}

pub(super) fn sixel_pixels(bytes: &[u8]) -> Result<image::RgbaImage> {
    let (width, height) = sixel_size(bytes)?;
    let transparent = bytes
        .split(|byte| *byte == b'q')
        .next()
        .unwrap_or_default()
        .split(|byte| *byte == b';')
        .nth(1)
        == Some(b"1");
    let mut image = image::RgbaImage::from_pixel(
        width,
        height,
        image::Rgba(if transparent { [0; 4] } else { [0, 0, 0, 255] }),
    );
    let mut palette = [[255, 255, 255, 255]; 256];
    let defaults = [
        [0, 0, 0],
        [51, 51, 204],
        [204, 33, 33],
        [51, 204, 51],
        [204, 51, 204],
        [51, 204, 204],
        [204, 204, 51],
        [135, 135, 135],
        [66, 66, 66],
        [84, 84, 255],
        [255, 84, 84],
        [84, 255, 84],
        [255, 84, 255],
        [84, 255, 255],
        [255, 255, 84],
        [255, 255, 255],
    ];
    for (target, rgb) in palette.iter_mut().zip(defaults) {
        *target = [rgb[0], rgb[1], rgb[2], 255];
    }
    let mut selected = 0;
    walk_sixel(bytes, |command| {
        match command {
            Raster::Color(index, value) => palette[index] = value,
            Raster::Select(index) => selected = index,
            Raster::Paint { x, y, count, bits } => {
                for dy in 0..6 {
                    if bits & (1 << dy) != 0 && y + dy < height {
                        for dx in 0..count {
                            image.put_pixel(x + dx, y + dy, image::Rgba(palette[selected]));
                        }
                    }
                }
            }
            Raster::Size(..) => {}
        }
        Ok(())
    })?;
    Ok(image)
}

pub(super) fn iterm_attributes(header: &[u8]) -> Result<HashMap<String, String>> {
    let text = std::str::from_utf8(header)?;
    let mut attributes = HashMap::new();
    for pair in text.split(';').take(16) {
        if let Some((key, value)) = pair.split_once('=') {
            ensure!(value.len() <= 1024, "iTerm image attribute too long");
            attributes.insert(key.to_string(), value.to_string());
        }
    }
    ensure!(
        attributes.get("inline").is_some_and(|value| value == "1"),
        "Only inline iTerm images are supported"
    );
    Ok(attributes)
}

pub(super) fn extent(value: Option<&String>, cell: f32, viewport: f32) -> Result<Option<f32>> {
    let Some(value) = value.filter(|value| value.as_str() != "auto") else {
        return Ok(None);
    };
    let pixels = if let Some(value) = value.strip_suffix("px") {
        value.parse::<f32>()?
    } else if let Some(value) = value.strip_suffix('%') {
        value.parse::<f32>()? * viewport / 100.
    } else {
        value.parse::<f32>()? * cell
    };
    ensure!(
        pixels.is_finite() && pixels > 0. && pixels <= 32768.,
        "iTerm image dimensions exceed budget"
    );
    Ok(Some(pixels))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn raster_palette_repeats_and_bands_are_bounded() -> Result<()> {
        let image = sixel_pixels(b"0;1;0q\"1;1;3;12#1;2;100;0;0!3~-$#2;1;120;50;100@")?;
        assert_eq!(image.dimensions(), (3, 12));
        assert_eq!(image.get_pixel(2, 5).0, [255, 0, 0, 255]);
        assert_eq!(image.get_pixel(2, 10).0, [0; 4]);
        assert!(sixel_size(b"q!4294967295~").is_err());
        assert!(sixel_size(b"q\"1;1;100000;100000~").is_err());
        Ok(())
    }
}

#[cfg(test)]
mod budget_tests {
    use super::*;

    fn overdraw(passes: usize) -> Vec<u8> {
        let mut frame = b"q\"1;1;4096;6".to_vec();
        for _ in 0..passes {
            frame.extend_from_slice(b"!4096~$");
        }
        frame
    }

    #[test]
    fn repeated_overdraw_is_bounded_without_limiting_real_images() {
        assert_eq!(sixel_size(&overdraw(16)).ok(), Some((4096, 6)));
        let started = std::time::Instant::now();
        // Each pass paints 4096 × 6 pixels; this exceeds 16 full 4096² frames.
        let hostile = overdraw(12_000);
        assert!(sixel_size(&hostile).is_err());
        assert!(sixel_pixels(&hostile).is_err());
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
    }

    #[test]
    fn only_drawn_pixels_count_against_the_budget() {
        // '?' carries no set bits, so it moves the cursor without painting.
        let mut frame = b"q".to_vec();
        for _ in 0..12_000 {
            frame.extend_from_slice(b"!4096?$");
        }
        frame.extend_from_slice(b"~");
        assert!(sixel_size(&frame).is_ok());
    }
}
