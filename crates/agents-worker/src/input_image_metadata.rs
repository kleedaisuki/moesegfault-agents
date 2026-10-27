//! Bounded image-header inspection shared by preparation and exact counting.
use std::io::Cursor;

use image::{ImageFormat, ImageReader, Limits};

const MAX_INPUT_BYTES: usize = 16 * 1024 * 1024;
const MAX_EDGE: u32 = 16_384;
const MAX_PIXELS: u64 = 64 * 1024 * 1024;
const MAX_HEADER_ALLOC: u64 = 16 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ImageHeader {
    pub mime: &'static str,
    pub width: u32,
    pub height: u32,
}

pub(crate) fn inspect(bytes: &[u8]) -> Result<ImageHeader, &'static str> {
    if bytes.is_empty() || bytes.len() > MAX_INPUT_BYTES {
        return Err("invalid_image_size");
    }
    let mut reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|_| "invalid_image_source")?;
    let mime = match reader.format() {
        Some(ImageFormat::Png) => "image/png",
        Some(ImageFormat::Jpeg) => "image/jpeg",
        Some(ImageFormat::WebP) => "image/webp",
        Some(ImageFormat::Gif) => "image/gif",
        _ => return Err("unsupported_image_type"),
    };
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_EDGE);
    limits.max_image_height = Some(MAX_EDGE);
    limits.max_alloc = Some(MAX_HEADER_ALLOC);
    reader.limits(limits);
    let (width, height) = reader
        .into_dimensions()
        .map_err(|_| "invalid_image_source")?;
    if width == 0
        || height == 0
        || width > MAX_EDGE
        || height > MAX_EDGE
        || u64::from(width) * u64::from(height) > MAX_PIXELS
    {
        return Err("invalid_image_size");
    }
    Ok(ImageHeader {
        mime,
        width,
        height,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::{Engine, engine::general_purpose::STANDARD};

    #[test]
    fn native_image_headers_support_gif_without_full_frame_allocation() {
        let gif = STANDARD
            .decode("R0lGODlhAQABAIAAAAAAAP///ywAAAAAAQABAAACAUwAOw==")
            .unwrap();
        assert_eq!(
            inspect(&gif).unwrap(),
            ImageHeader {
                mime: "image/gif",
                width: 1,
                height: 1
            }
        );
        assert!(inspect(&[1, 2, 3]).is_err());
        let mut excessive = gif.clone();
        excessive[6..10].copy_from_slice(&[0xff, 0xff, 0xff, 0xff]);
        assert!(inspect(&excessive).is_err());
    }
}
