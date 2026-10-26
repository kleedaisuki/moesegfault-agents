//! DeepSeek V4.1 image token adjustment from final prepared image sources.
//! Dimension fitting follows DeepSeek recipe 0.2.0 image/opencv.rs (MIT).
//! Copyright (c) 2026 DeepSeek; license: third_party/deepseek-recipe/LICENSE.
use std::borrow::Cow;

use base64::{Engine, engine::general_purpose::STANDARD};
use deepseek_recipe_core::multimodal::{ImageDetail, ImageSource, ImageTokenSpec};

const MAX_BYTES: usize = 5 * 1024 * 1024;
const MAX_TOTAL_BYTES: usize = 4 * 1024 * 1024;
const WEBP_MAX: usize = 16_383;

pub(crate) fn adjustment(images: &[ImageSource]) -> Result<u64, String> {
    if images.len() > 8 {
        return Err("token_count_image_limit".into());
    }
    let mut total_bytes = 0usize;
    let mut adjustment = 0u64;
    for image in images {
        let (bytes, declared) = match image {
            ImageSource::DataUrl { data_url, .. } => {
                let (bytes, declared) = decode(data_url)?;
                (Cow::Owned(bytes), declared)
            }
            ImageSource::Bytes { data, .. } if data.len() <= MAX_BYTES => {
                (Cow::Borrowed(data.as_slice()), None)
            }
            ImageSource::Bytes { .. } => return Err("invalid_image_size".into()),
            ImageSource::Url { .. } => return Err("token_count_image_not_prepared".into()),
        };
        total_bytes = total_bytes
            .checked_add(bytes.len())
            .ok_or("attachments_too_large")?;
        if total_bytes > MAX_TOTAL_BYTES {
            return Err("attachments_too_large".into());
        }
        let header = crate::input_image_metadata::inspect(&bytes)?;
        if declared.as_deref().is_some_and(|mime| mime != header.mime) {
            return Err("invalid_image_source".into());
        }
        let count = dimension_tokens(header.width, header.height, image.detail())?;
        adjustment = adjustment
            .checked_add(count.checked_sub(1).ok_or("token_count_image_invalid")?)
            .ok_or("token_count_overflow")?;
    }
    Ok(adjustment)
}

fn decode(url: &str) -> Result<(Vec<u8>, Option<String>), String> {
    let (header, encoded) = url.split_once(',').ok_or("invalid_image_source")?;
    let media = header
        .strip_prefix("data:")
        .and_then(|s| s.strip_suffix(";base64"))
        .filter(|mime| {
            matches!(
                *mime,
                "image/png" | "image/jpeg" | "image/gif" | "image/webp"
            )
        })
        .ok_or("invalid_image_source")?;
    if encoded.len() > MAX_BYTES.div_ceil(3) * 4 {
        return Err("invalid_image_size".into());
    }
    let bytes = STANDARD
        .decode(encoded)
        .map_err(|_| "invalid_image_source")?;
    if bytes.is_empty() || bytes.len() > MAX_BYTES {
        return Err("invalid_image_size".into());
    }
    Ok((bytes, Some(media.to_owned())))
}

pub(crate) fn dimension_tokens(
    width: u32,
    height: u32,
    detail: ImageDetail,
) -> Result<u64, String> {
    // The pinned official profile accepts at most 8192 per edge for <=8 images.
    if width == 0 || height == 0 || width > 8192 || height > 8192 {
        return Err("token_count_image_dimensions_unavailable".into());
    }
    let mut width = width as usize;
    let mut height = height as usize;
    let long_side = width.max(height);
    if detail.is_low() && long_side > 512 {
        let ratio = 512.0 / long_side as f64;
        width = ((width as f64 * ratio).round() as usize).max(1);
        height = ((height as f64 * ratio).round() as usize).max(1);
    }
    let spec = ImageTokenSpec::v41();
    for _ in 0..6 {
        let fitted = spec
            .calc_resize(width, height)
            .map_err(|_| "token_count_image_resize_failed")?;
        if fitted.best_width <= WEBP_MAX && fitted.best_height <= WEBP_MAX {
            return fitted
                .token_count
                .try_into()
                .map_err(|_| "token_count_overflow".into());
        }
        let long_side = fitted.best_width.max(fitted.best_height);
        let long = WEBP_MAX / spec.patch_size() * spec.patch_size();
        let scaled = (fitted.best_width.min(fitted.best_height) as f64 * long as f64
            / long_side as f64)
            .round();
        let short = (scaled as usize).max(1).div_ceil(spec.patch_size()) * spec.patch_size();
        (width, height) = if fitted.best_width >= fitted.best_height {
            (long, short)
        } else {
            (short, long)
        };
    }
    Err("token_count_image_resize_failed".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn official_image_dimension_and_detail_corpus() {
        for (width, height, expected) in [
            (1, 1, 184),
            (640, 480, 206),
            (1920, 1080, 968),
            (1080, 1920, 986),
            (8192, 8192, 994),
        ] {
            for detail in [ImageDetail::High, ImageDetail::Original, ImageDetail::Auto] {
                assert_eq!(dimension_tokens(width, height, detail).unwrap(), expected);
            }
        }
        assert_eq!(dimension_tokens(1024, 1024, ImageDetail::Low).unwrap(), 184);
        for (width, height) in [(1, 8192), (8192, 1)] {
            assert!(dimension_tokens(width, height, ImageDetail::Original).unwrap() <= 1024);
        }
        assert!(dimension_tokens(8193, 1, ImageDetail::High).is_err());
    }

    #[test]
    fn image_count_never_fetches_unprepared_urls_or_guesses_corrupt_bytes() {
        assert!(
            adjustment(&[ImageSource::Url {
                url: "https://example.com/image.png".into(),
                detail: ImageDetail::High,
                path: String::new()
            }])
            .is_err()
        );
        assert!(
            adjustment(&[ImageSource::DataUrl {
                data_url: "data:image/png;base64,AQID".into(),
                detail: ImageDetail::Auto,
                path: String::new()
            }])
            .is_err()
        );
    }
}
