//! Image files as the `image` block a tool result carries.

use std::io::Cursor;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use image::codecs::jpeg::JpegEncoder;
use image::imageops::FilterType;
use image::{DynamicImage, ImageError, ImageFormat, Rgb, RgbImage};
use serde_json::json;

use crate::api::ContentBlock;

/// The longest edge an image is sent with.
const MAX_EDGE: u32 = 2000;
/// The most bytes an image is sent with as it is, or made into a PNG.
const MAX_BYTES: usize = 1_000_000;
/// The JPEG qualities tried in turn; the last is used whatever its size.
const JPEG_QUALITIES: [u8; 4] = [80, 60, 40, 20];

/// An image ready to send.
pub struct Prepared {
    pub block: ContentBlock,
    pub width: u32,
    pub height: u32,
    /// What to tell the model when it was resized.
    pub note: Option<String>,
}

/// The format `bytes` start with, among those the API takes.
pub fn sniff(bytes: &[u8]) -> Option<ImageFormat> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some(ImageFormat::Png)
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some(ImageFormat::Jpeg)
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some(ImageFormat::Gif)
    } else if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        Some(ImageFormat::WebP)
    } else {
        None
    }
}

/// `bytes`, of `format`, within the size limits: unchanged when they already
/// are, or decoded, resized and encoded again.
pub fn prepare(bytes: &[u8], format: ImageFormat) -> Result<Prepared, String> {
    let decoded = image::load_from_memory_with_format(bytes, format).map_err(|e| match e {
        ImageError::Limits(_) => {
            "is too large to decode as an image; resize it or export a smaller copy, then read \
             that."
                .to_string()
        }
        other => format!("could not be decoded as an image: {other}"),
    })?;
    let (width, height) = (decoded.width(), decoded.height());
    let (new_width, new_height) = within_edge(width, height);
    let resized = (new_width, new_height) != (width, height);

    if !resized && bytes.len() <= MAX_BYTES {
        return Ok(Prepared {
            block: block(format, bytes),
            width,
            height,
            note: None,
        });
    }
    let image = if resized {
        decoded.resize_exact(new_width, new_height, FilterType::Triangle)
    } else {
        decoded
    };
    let (format, encoded) = encode(&image, format)?;
    Ok(Prepared {
        block: block(format, &encoded),
        width: new_width,
        height: new_height,
        note: resized.then(|| {
            format!(
                "[Image: original {width}x{height}, displayed at {new_width}x{new_height}. \
                 Multiply coordinates by {:.2} to map to original image.]",
                f64::from(width.max(height)) / f64::from(new_width.max(new_height))
            )
        }),
    })
}

fn within_edge(width: u32, height: u32) -> (u32, u32) {
    let long = width.max(height);
    if long <= MAX_EDGE {
        return (width, height);
    }
    let scaled = |edge: u32| {
        let edge = (u64::from(edge) * u64::from(MAX_EDGE) + u64::from(long) / 2) / u64::from(long);
        (edge as u32).max(1)
    };
    (scaled(width), scaled(height))
}

/// A PNG when that fits [`MAX_BYTES`] and the source was not a JPEG, else a
/// JPEG at the first quality that fits.
fn encode(image: &DynamicImage, source: ImageFormat) -> Result<(ImageFormat, Vec<u8>), String> {
    if source != ImageFormat::Jpeg {
        let mut png = Cursor::new(Vec::new());
        image
            .write_to(&mut png, ImageFormat::Png)
            .map_err(|e| format!("could not be encoded as PNG: {e}"))?;
        let png = png.into_inner();
        if png.len() <= MAX_BYTES {
            return Ok((ImageFormat::Png, png));
        }
    }
    let flat = on_white(image);
    let mut jpeg = Vec::new();
    for quality in JPEG_QUALITIES {
        jpeg.clear();
        flat.write_with_encoder(JpegEncoder::new_with_quality(&mut jpeg, quality))
            .map_err(|e| format!("could not be encoded as JPEG: {e}"))?;
        if jpeg.len() <= MAX_BYTES {
            break;
        }
    }
    Ok((ImageFormat::Jpeg, jpeg))
}

/// `image` with its transparency blended onto white.
fn on_white(image: &DynamicImage) -> RgbImage {
    let rgba = image.to_rgba8();
    RgbImage::from_fn(rgba.width(), rgba.height(), |x, y| {
        let [r, g, b, a] = rgba.get_pixel(x, y).0;
        let blend =
            |c: u8| ((u16::from(c) * u16::from(a) + 255 * (255 - u16::from(a))) / 255) as u8;
        Rgb([blend(r), blend(g), blend(b)])
    })
}

fn block(format: ImageFormat, bytes: &[u8]) -> ContentBlock {
    ContentBlock(json!({
        "type": "image",
        "source": {
            "type": "base64",
            "media_type": format.to_mime_type(),
            "data": BASE64.encode(bytes),
        },
    }))
}
