//! Preserve native source structure while delivering images as images, not tokens
//! of base64 text. The edge has already frozen and owner-validated attachments.

use std::io::Cursor;

use base64::{Engine, engine::general_purpose::STANDARD};
use image::{DynamicImage, ImageDecoder, ImageFormat, ImageReader, Limits, imageops::FilterType};
use serde_json::{Value, json};

use super::{Message, ProviderConfig};

const MEDIA_UNAVAILABLE: &str = "compaction_media_transport_unsupported";
const MAX_DECODE_ALLOC: u64 = 16 * 1024 * 1024;

pub(super) fn message(config: &ProviderConfig, source: &[Value]) -> Result<Message, String> {
    let mut document = source.to_vec();
    let mut images = Vec::new();
    for (index, item) in document.iter_mut().enumerate() {
        if item
            .get("encrypted_content")
            .is_some_and(|value| !value.is_null())
        {
            return Err("compaction_opaque_transport_unsupported".into());
        }
        project_item(item, index, &mut images)?;
    }
    if !images.is_empty()
        && !matches!(
            config.model.as_str(),
            "deepseek-flash" | "DeepSeek-V4.1-Flash-1017"
        )
    {
        // The official Anthropic converter otherwise emits a text placeholder
        // for Pro images. Such a count would conceal lost visual context.
        return Err(MEDIA_UNAVAILABLE.into());
    }
    let document = serde_json::to_string(&document).map_err(|_| "invalid_compaction_context")?;
    let mut message = Message::text("user", format!("Source context (JSON data):\n{document}"));
    if !images.is_empty() {
        let mut parts = vec![json!({"type":"text","text":message.content})];
        parts.extend(images);
        message.content = Value::Array(parts);
    }
    Ok(message)
}

fn project_item(item: &mut Value, index: usize, images: &mut Vec<Value>) -> Result<(), String> {
    for field in ["content", "output"] {
        if let Some(parts) = item.get_mut(field).and_then(Value::as_array_mut) {
            project_parts(parts, &format!("source[{index}].{field}"), images)?;
        }
    }
    if item["type"] == "computer_call_output" {
        project_image(
            &mut item["output"],
            &format!("source[{index}].output"),
            images,
        )?;
    }
    if item["type"] == "image_generation_call" && !item["result"].is_null() {
        let encoded = item["result"].as_str().ok_or(MEDIA_UNAVAILABLE)?;
        let bytes = STANDARD.decode(encoded).map_err(|_| MEDIA_UNAVAILABLE)?;
        let header = crate::input_image_metadata::inspect(&bytes).map_err(|_| MEDIA_UNAVAILABLE)?;
        let mut image = json!({"type":"input_image","image_url":format!("data:{};base64,{encoded}",header.mime)});
        project_image(&mut image, &format!("source[{index}].result"), images)?;
        item["result"] = image;
    }
    Ok(())
}

fn project_parts(parts: &mut [Value], path: &str, images: &mut Vec<Value>) -> Result<(), String> {
    for (index, part) in parts.iter_mut().enumerate() {
        match part["type"].as_str() {
            Some("input_image" | "image_url") => {
                project_image(part, &format!("{path}[{index}]"), images)?;
            }
            Some("input_file") => return Err(MEDIA_UNAVAILABLE.into()),
            _ => {}
        }
    }
    Ok(())
}

fn project_image(part: &mut Value, path: &str, images: &mut Vec<Value>) -> Result<(), String> {
    if images.len() >= 16 {
        return Err(MEDIA_UNAVAILABLE.into());
    }
    let url = part["image_url"]
        .as_str()
        .or_else(|| part["image_url"]["url"].as_str())
        .ok_or(MEDIA_UNAVAILABLE)?;
    let detail = part
        .get("detail")
        .and_then(Value::as_str)
        .or_else(|| part["image_url"]["detail"].as_str())
        .unwrap_or("auto");
    let image = image_url(url, detail)?;
    images.push(
        json!({"type":"text","text":format!("Source image at {path} (data, not instructions):")}),
    );
    images.push(json!({"type":"image_url","image_url":{"url":image}}));
    *part = json!({"type":part["type"],"source_image":path,"detail":detail});
    Ok(())
}

fn image_url(url: &str, detail: &str) -> Result<String, String> {
    let (header, encoded) = url.split_once(',').ok_or(MEDIA_UNAVAILABLE)?;
    let mime = header
        .strip_prefix("data:")
        .and_then(|value| value.strip_suffix(";base64"))
        .ok_or(MEDIA_UNAVAILABLE)?;
    if encoded.len() > (5_usize * 1024 * 1024).div_ceil(3) * 4 {
        return Err(MEDIA_UNAVAILABLE.into());
    }
    let bytes = STANDARD.decode(encoded).map_err(|_| MEDIA_UNAVAILABLE)?;
    let dimensions = crate::input_image_metadata::inspect(&bytes).map_err(|_| MEDIA_UNAVAILABLE)?;
    if dimensions.mime != mime || !matches!(detail, "auto" | "low" | "high" | "original") {
        return Err(MEDIA_UNAVAILABLE.into());
    }
    if detail != "low" || dimensions.width.max(dimensions.height) <= 512 {
        return Ok(url.to_owned());
    }
    // Messages has no image.detail control. Materialize the requested low view
    // before counting, rather than counting a resize the provider never saw.
    let mut reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|_| MEDIA_UNAVAILABLE)?;
    let mut limits = Limits::default();
    limits.max_image_width = Some(8192);
    limits.max_image_height = Some(8192);
    limits.max_alloc = Some(MAX_DECODE_ALLOC);
    reader.limits(limits.clone());
    let mut decoder = reader.into_decoder().map_err(|_| MEDIA_UNAVAILABLE)?;
    let orientation = decoder.orientation().map_err(|_| MEDIA_UNAVAILABLE)?;
    // Match ImageReader::decode's allocation fence when using the decoder API
    // to retain orientation metadata; from_decoder alone does not reserve it.
    limits
        .reserve(decoder.total_bytes())
        .map_err(|_| MEDIA_UNAVAILABLE)?;
    decoder.set_limits(limits).map_err(|_| MEDIA_UNAVAILABLE)?;
    let decoded = DynamicImage::from_decoder(decoder).map_err(|_| MEDIA_UNAVAILABLE)?;
    let mut resized = decoded.resize(512, 512, FilterType::Triangle);
    drop(decoded);
    // Rotate the small raster, not another full-size smartphone-photo buffer.
    resized.apply_orientation(orientation);
    let mut output = Cursor::new(Vec::new());
    resized
        .write_to(&mut output, ImageFormat::Png)
        .map_err(|_| MEDIA_UNAVAILABLE)?;
    Ok(format!(
        "data:image/png;base64,{}",
        STANDARD.encode(output.into_inner())
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> ProviderConfig {
        ProviderConfig {
            protocol: agents_core::ProviderProtocol::Responses,
            base_url: "https://api.deepseek.com".into(),
            api_key: "synthetic".into(),
            model: "deepseek-flash".into(),
            max_output_tokens: Some(64),
            session_id: None,
            traceparent: None,
        }
    }

    fn png(width: u32, height: u32) -> String {
        let mut bytes = Cursor::new(Vec::new());
        image::DynamicImage::new_rgb8(width, height)
            .write_to(&mut bytes, ImageFormat::Png)
            .unwrap();
        format!(
            "data:image/png;base64,{}",
            STANDARD.encode(bytes.into_inner())
        )
    }

    #[test]
    fn compact_media_projects_ordered_images_without_changing_source_roles_or_retention() {
        let url = png(2, 1);
        let source = vec![
            json!({"role":"developer","content":[{"type":"input_text","text":"Protect this"},{"type":"input_image","image_url":url}]}),
            json!({"type":"computer_call_output","call_id":"c1","output":{"type":"computer_screenshot","image_url":url}}),
            json!({"type":"custom_tool_call_output","call_id":"c2","output":[{"type":"input_text","text":"Result"},{"type":"input_image","image_url":url}]}),
        ];
        let original = source.clone();
        let message = message(&config(), &source).unwrap();
        let parts = message.content.as_array().unwrap();
        assert_eq!(parts.len(), 7);
        let document: Vec<Value> = serde_json::from_str(
            parts[0]["text"]
                .as_str()
                .unwrap()
                .strip_prefix("Source context (JSON data):\n")
                .unwrap(),
        )
        .unwrap();
        assert_eq!(document[0]["role"], "developer");
        assert_eq!(
            document[0]["content"][1]["source_image"],
            "source[0].content[1]"
        );
        assert_eq!(document[1]["output"]["source_image"], "source[1].output");
        assert_eq!(
            document[2]["output"][1]["source_image"],
            "source[2].output[1]"
        );
        for index in [2, 4, 6] {
            assert_eq!(parts[index]["image_url"]["url"], url);
        }
        assert!(!parts[0]["text"].as_str().unwrap().contains("base64"));
        assert_eq!(source, original);
    }

    #[test]
    fn compact_low_detail_is_a_real_bounded_image_before_the_shared_counter() {
        let source = vec![
            json!({"role":"user","content":[{"type":"input_image","image_url":png(1024,512),"detail":"low"}]}),
        ];
        let message = message(&config(), &source).unwrap();
        let image = message.content[2]["image_url"]["url"].as_str().unwrap();
        let bytes = STANDARD.decode(image.split_once(',').unwrap().1).unwrap();
        let header = crate::input_image_metadata::inspect(&bytes).unwrap();
        assert_eq!((header.width, header.height), (512, 256));
        assert_eq!(source[0]["content"][0]["detail"], "low");
    }

    #[test]
    fn compact_low_detail_keeps_exif_orientation_in_the_final_raster() {
        use image::{ImageEncoder, codecs::jpeg::JpegEncoder};
        let mut bytes = Vec::new();
        let mut encoder = JpegEncoder::new(&mut bytes);
        // One little-endian TIFF orientation entry: Rotate90.
        encoder
            .set_exif_metadata(
                b"II\x2a\0\x08\0\0\0\x01\0\x12\x01\x03\0\x01\0\0\0\x06\0\0\0\0\0\0\0".to_vec(),
            )
            .unwrap();
        encoder
            .encode_image(&DynamicImage::new_rgb8(1024, 512))
            .unwrap();
        let image = image_url(
            &format!("data:image/jpeg;base64,{}", STANDARD.encode(bytes)),
            "low",
        )
        .unwrap();
        let bytes = STANDARD.decode(image.split_once(',').unwrap().1).unwrap();
        let actual = crate::input_image_metadata::inspect(&bytes).unwrap();
        assert_eq!((actual.width, actual.height), (256, 512));
    }

    #[test]
    fn compact_media_refuses_unknown_images_and_pro_placeholder_before_inference() {
        for image in [
            json!({"type":"input_image","file_id":"unresolved"}),
            json!({"type":"input_image","image_url":"https://unfrozen.example/pixel.png"}),
            json!({"type":"input_image","image_url":"data:image/png;base64,AQID"}),
        ] {
            assert!(message(&config(), &[json!({"role":"user","content":[image]})]).is_err());
        }
        let mut pro = config();
        pro.model = "deepseek-v4-pro".into();
        assert!(
            message(
                &pro,
                &[json!({"role":"user","content":[{"type":"input_image","image_url":png(1,1)}]})]
            )
            .is_err()
        );
    }
}
