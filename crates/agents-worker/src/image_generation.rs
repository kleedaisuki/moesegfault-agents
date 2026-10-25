//! Image-service boundary. Credentials and paid-operation fencing belong to the
//! caller; an uncertain response never authorizes another generation request.

use std::{
    collections::{BTreeMap, BTreeSet},
    io::Cursor,
};

use base64::{Engine, engine::general_purpose::STANDARD};
use image::{ImageFormat as CodecFormat, ImageReader, Limits};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

#[path = "image_generation_transport.rs"]
mod transport;

pub(crate) use transport::{generate, materialize};

#[cfg(test)]
#[path = "image_generation_tests.rs"]
mod tests;

pub(crate) const MAX_IMAGE_BYTES: usize = 16 * 1024 * 1024;
const MAX_INPUT_BYTES: usize = 16 * 1024 * 1024;
const MAX_EDGE: u32 = 16_384;
const MAX_PIXELS: u64 = 64 * 1024 * 1024;

fn invalid(code: &str) -> worker::Error {
    worker::Error::RustError(code.into())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Format {
    Png,
    Jpeg,
    Webp,
}

impl Format {
    pub(crate) fn mime(self) -> &'static str {
        match self {
            Self::Png => "image/png",
            Self::Jpeg => "image/jpeg",
            Self::Webp => "image/webp",
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Jpeg => "jpeg",
            Self::Webp => "webp",
        }
    }

    fn from_mime(value: &str) -> Option<Self> {
        [Self::Png, Self::Jpeg, Self::Webp]
            .into_iter()
            .find(|format| format.mime().eq_ignore_ascii_case(value))
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Action {
    Generate,
    Edit,
    #[default]
    Auto,
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ImageOptions {
    #[serde(default)]
    pub action: Action,
    #[serde(default)]
    pub background: Option<String>,
    #[serde(default)]
    pub input_fidelity: Option<String>,
    #[serde(default)]
    pub input_image_mask: Option<MaskReference>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub moderation: Option<String>,
    #[serde(default)]
    pub output_compression: Option<u8>,
    #[serde(default)]
    pub output_format: Option<Format>,
    #[serde(default)]
    pub partial_images: Option<u8>,
    #[serde(default)]
    pub quality: Option<String>,
    #[serde(default)]
    pub size: Option<String>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MaskReference {
    #[serde(default)]
    pub file_id: Option<String>,
    #[serde(default)]
    pub image_url: Option<String>,
}

impl ImageOptions {
    pub(crate) fn validate(&self) -> worker::Result<()> {
        option(&self.background, &["auto", "opaque", "transparent"])?;
        option(&self.input_fidelity, &["low", "high"])?;
        option(&self.moderation, &["auto", "low"])?;
        option(
            &self.quality,
            &["auto", "low", "medium", "high", "xhigh", "max"],
        )?;
        if self.output_compression.is_some_and(|value| value > 100)
            || self.partial_images.is_some_and(|value| value > 3)
        {
            return Err(invalid("invalid_image_generation_options"));
        }
        if self.background.as_deref() == Some("transparent")
            && self.output_format == Some(Format::Jpeg)
        {
            return Err(invalid("invalid_image_generation_background"));
        }
        if let Some(model) = &self.model {
            text(model, 256)?;
        }
        if let Some(size) = self.size.as_deref().filter(|value| *value != "auto") {
            dimensions(size)?;
        }
        if let Some(mask) = &self.input_image_mask {
            match (&mask.file_id, &mask.image_url) {
                (Some(id), None) if opaque(id) => {}
                (None, Some(url)) if url.starts_with("data:image/png;base64,") => {
                    if url.len() > MAX_IMAGE_BYTES.div_ceil(3) * 4 + 64 {
                        return Err(invalid("image_generation_input_too_large"));
                    }
                }
                _ => return Err(invalid("invalid_image_generation_mask")),
            }
        }
        Ok(())
    }

    fn fields(&self) -> BTreeMap<&'static str, Value> {
        let mut values = BTreeMap::new();
        values.insert(
            "output_format",
            json!(self.output_format.unwrap_or(Format::Png)),
        );
        for (name, value) in [
            ("background", self.background.as_ref()),
            ("input_fidelity", self.input_fidelity.as_ref()),
            ("moderation", self.moderation.as_ref()),
            ("quality", self.quality.as_ref()),
            ("size", self.size.as_ref()),
        ] {
            if let Some(value) = value {
                // Native auto values select the service default; providers need
                // not advertise a separate quality/background/size control.
                if value != "auto" {
                    values.insert(name, json!(value));
                }
            }
        }
        if let Some(value) = self.output_compression
            && self.output_format.unwrap_or(Format::Png) != Format::Png
        {
            values.insert("output_compression", json!(value));
        }
        values
    }
}

fn option(value: &Option<String>, allowed: &[&str]) -> worker::Result<()> {
    if value
        .as_deref()
        .is_some_and(|value| !allowed.contains(&value))
    {
        return Err(invalid("invalid_image_generation_options"));
    }
    Ok(())
}

fn text(value: &str, limit: usize) -> worker::Result<()> {
    if value.trim().is_empty() || value.len() > limit || value.chars().any(char::is_control) {
        return Err(invalid("invalid_image_generation_input"));
    }
    Ok(())
}

fn opaque(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && value
            .bytes()
            .all(|value| value.is_ascii_alphanumeric() || b"_-".contains(&value))
}

fn dimensions(value: &str) -> worker::Result<(u32, u32)> {
    let (width, height) = value
        .split_once('x')
        .ok_or_else(|| invalid("invalid_image_generation_size"))?;
    if !width
        .bytes()
        .chain(height.bytes())
        .all(|byte| byte.is_ascii_digit())
    {
        return Err(invalid("invalid_image_generation_size"));
    }
    let width = width
        .parse()
        .map_err(|_| invalid("invalid_image_generation_size"))?;
    let height = height
        .parse()
        .map_err(|_| invalid("invalid_image_generation_size"))?;
    checked_dimensions(width, height)?;
    Ok((width, height))
}

fn checked_dimensions(width: u32, height: u32) -> worker::Result<()> {
    if width == 0
        || height == 0
        || width > MAX_EDGE
        || height > MAX_EDGE
        || u64::from(width) * u64::from(height) > MAX_PIXELS
    {
        return Err(invalid("image_generation_pixel_limit"));
    }
    Ok(())
}

/// Validated encoded raster; serialization keeps one base64 copy, not a JSON
/// array of bytes. Restored pending outputs pass through the same validation.
#[derive(Clone)]
pub(crate) struct Raster {
    encoded: String,
    format: Format,
    width: u32,
    height: u32,
    bytes_len: usize,
}

impl Raster {
    pub(crate) fn from_bytes(bytes: &[u8]) -> worker::Result<Self> {
        let (format, width, height) = inspect(bytes)?;
        Ok(Self {
            encoded: STANDARD.encode(bytes),
            format,
            width,
            height,
            bytes_len: bytes.len(),
        })
    }

    fn from_encoded(encoded: String) -> worker::Result<Self> {
        if encoded.len() > MAX_IMAGE_BYTES.div_ceil(3) * 4 {
            return Err(invalid("image_generation_image_too_large"));
        }
        let bytes = STANDARD
            .decode(&encoded)
            .map_err(|_| invalid("invalid_generated_image"))?;
        let (format, width, height) = inspect(&bytes)?;
        Ok(Self {
            encoded,
            format,
            width,
            height,
            bytes_len: bytes.len(),
        })
    }

    pub(crate) fn from_base64(value: &str) -> worker::Result<Self> {
        Self::from_encoded(value.to_owned())
    }

    pub(crate) fn bytes(&self) -> Vec<u8> {
        STANDARD
            .decode(&self.encoded)
            .expect("validated raster encoding")
    }

    pub(crate) fn encoded(&self) -> &str {
        &self.encoded
    }
    pub(crate) fn format(&self) -> Format {
        self.format
    }
    pub(crate) fn dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }
    pub(crate) fn len(&self) -> usize {
        self.bytes_len
    }
    pub(crate) fn sha256(&self) -> String {
        format!("{:x}", Sha256::digest(self.bytes()))
    }
    fn data_url(&self) -> String {
        format!("data:{};base64,{}", self.format.mime(), self.encoded)
    }
}

fn inspect(bytes: &[u8]) -> worker::Result<(Format, u32, u32)> {
    if bytes.is_empty() || bytes.len() > MAX_IMAGE_BYTES {
        return Err(invalid("image_generation_image_too_large"));
    }
    let mut reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|_| invalid("invalid_generated_image"))?;
    let format = match reader.format() {
        Some(CodecFormat::Png) => Format::Png,
        Some(CodecFormat::Jpeg) => Format::Jpeg,
        Some(CodecFormat::WebP) => Format::Webp,
        _ => return Err(invalid("invalid_generated_image_format")),
    };
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_EDGE);
    limits.max_image_height = Some(MAX_EDGE);
    limits.max_alloc = Some(64 * 1024 * 1024);
    reader.limits(limits);
    let (width, height) = reader
        .into_dimensions()
        .map_err(|_| invalid("invalid_generated_image"))?;
    checked_dimensions(width, height)?;
    Ok((format, width, height))
}

impl Serialize for Raster {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.encoded)
    }
}

impl<'de> Deserialize<'de> for Raster {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::from_encoded(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ImageApi {
    Openai,
    Openrouter,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(crate) struct Capabilities {
    pub streaming: bool,
    /// Explicit profile/capability record, never inferred from a model-name prefix.
    pub parameters: BTreeMap<String, Parameter>,
    pub max_references: usize,
    pub mask: bool,
}

impl Capabilities {
    pub(crate) fn from_openrouter_endpoint(endpoint: &Value) -> worker::Result<Self> {
        let mut parameters: BTreeMap<String, Parameter> =
            serde_json::from_value(endpoint["supported_parameters"].clone())
                .map_err(|_| invalid("invalid_image_generation_capabilities"))?;
        let max_references = endpoint
            .pointer("/supported_parameters/input_references/max")
            .and_then(Value::as_u64)
            .unwrap_or(0)
            .min(16) as usize;
        // These are Image API normalization fields rather than model-specific
        // provider knobs. The returned raster is still checked against both.
        parameters.insert(
            "output_format".into(),
            Parameter::Enum {
                values: vec![json!("png"), json!("jpeg"), json!("webp")],
            },
        );
        parameters.insert("size".into(), Parameter::Boolean);
        let passthrough = endpoint["allowed_passthrough_parameters"].as_array();
        for (name, values) in [
            ("moderation", vec![json!("auto"), json!("low")]),
            ("input_fidelity", vec![json!("low"), json!("high")]),
        ] {
            if passthrough.is_some_and(|parameters| {
                parameters.iter().any(|value| value.as_str() == Some(name))
            }) {
                parameters.insert(name.into(), Parameter::Enum { values });
            }
        }
        Ok(Self {
            streaming: endpoint["supports_streaming"].as_bool().unwrap_or(false),
            parameters,
            max_references,
            mask: false,
        })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum Parameter {
    #[serde(rename = "enum")]
    Enum {
        values: Vec<Value>,
    },
    Range {
        min: i64,
        max: i64,
    },
    Boolean,
}

impl Parameter {
    fn accepts(&self, value: &Value) -> bool {
        match self {
            Self::Enum { values } => values.contains(value),
            Self::Range { min, max } => value
                .as_i64()
                .is_some_and(|value| (*min..=*max).contains(&value)),
            Self::Boolean => true,
        }
    }
}

/// Do not derive Debug/Serialize: the resolved service key is invocation-local.
pub(crate) struct ImageConfig {
    pub api: ImageApi,
    pub base_url: String,
    pub model: String,
    pub public_model: Option<String>,
    pub api_key: String,
    pub capabilities: Capabilities,
    pub asset_hosts: Vec<String>,
    pub traceparent: Option<String>,
    pub router_endpoint: Option<RouterEndpoint>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct RouterEndpoint {
    pub provider_slug: String,
    pub provider_tag: String,
    pub passthrough_parameters: BTreeSet<String>,
}

impl RouterEndpoint {
    pub(crate) fn from_catalog(endpoint: &Value) -> worker::Result<Self> {
        let provider_slug = endpoint["provider_slug"]
            .as_str()
            .ok_or_else(|| invalid("invalid_image_generation_capabilities"))?
            .to_owned();
        let provider_tag = endpoint["provider_tag"]
            .as_str()
            .ok_or_else(|| invalid("invalid_image_generation_capabilities"))?
            .to_owned();
        text(&provider_slug, 128)?;
        text(&provider_tag, 128)?;
        let passthrough_parameters =
            serde_json::from_value(endpoint["allowed_passthrough_parameters"].clone())
                .map_err(|_| invalid("invalid_image_generation_capabilities"))?;
        Ok(Self {
            provider_slug,
            provider_tag,
            passthrough_parameters,
        })
    }
}

pub(crate) struct ImageInput {
    pub prompt: String,
    pub images: Vec<Raster>,
    pub mask: Option<Raster>,
    pub options: ImageOptions,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum ImageSource {
    Inline {
        image: Raster,
    },
    Remote {
        url: String,
        media_type: Option<String>,
    },
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct ImageResult {
    pub source: ImageSource,
    pub provider_request_id: Option<String>,
    pub created_at: Option<u64>,
    pub revised_prompt: Option<String>,
    pub output_format: Option<Format>,
    pub background: Option<String>,
    pub quality: Option<String>,
    pub requested_size: Option<String>,
    /// Original arbitrary-precision usage/cost. The existing accounting layer
    /// decides authority and converts decimal USD; absent usage is not free.
    pub usage: Option<Value>,
}

impl ImageResult {
    pub(crate) fn reported_cost(&self) -> Option<&Value> {
        self.usage
            .as_ref()?
            .get("cost")
            .filter(|value| !value.is_null())
    }
}

pub(crate) struct PartialImage {
    pub index: u8,
    pub image: Raster,
}

pub(crate) struct PreparedImageRequest {
    endpoint: String,
    content_type: String,
    body: Vec<u8>,
    stream: bool,
    requested_format: Format,
}

pub(crate) fn prepare(
    config: &ImageConfig,
    input: &ImageInput,
) -> worker::Result<PreparedImageRequest> {
    input.options.validate()?;
    if input.prompt.trim().is_empty()
        || input.prompt.len() > 32 * 1024
        || input.prompt.contains('\0')
    {
        return Err(invalid("invalid_image_generation_prompt"));
    }
    text(&config.model, 256)?;
    if config.api_key.is_empty()
        || config.api_key.len() > 16 * 1024
        || config.api_key.chars().any(char::is_control)
    {
        return Err(invalid("invalid_image_generation_credentials"));
    }
    if input
        .options
        .model
        .as_ref()
        .is_some_and(|model| model != &config.model && Some(model) != config.public_model.as_ref())
    {
        return Err(invalid("image_generation_model_route_mismatch"));
    }
    let base = crate::auth::provider_keys::normalize_base_url(
        agents_core::ProviderProtocol::Openai,
        &config.base_url,
    )?;
    if input.images.len() > config.capabilities.max_references || input.images.len() > 16 {
        return Err(invalid("image_generation_reference_limit"));
    }
    let input_bytes = input
        .images
        .iter()
        .map(Raster::len)
        .sum::<usize>()
        .saturating_add(input.mask.as_ref().map_or(0, Raster::len));
    if input_bytes > MAX_INPUT_BYTES {
        return Err(invalid("image_generation_input_too_large"));
    }
    let edit = match input.options.action {
        Action::Edit => true,
        Action::Generate => false,
        Action::Auto => !input.images.is_empty(),
    };
    if edit && input.images.is_empty() || !edit && !input.images.is_empty() {
        return Err(invalid("image_generation_action_mismatch"));
    }
    if input.mask.is_some() != input.options.input_image_mask.is_some()
        || input.mask.is_some() && (!edit || !config.capabilities.mask)
    {
        return Err(invalid("image_generation_mask_unavailable"));
    }
    if let Some(mask) = &input.mask {
        if mask.format != Format::Png || mask.dimensions() != input.images[0].dimensions() {
            return Err(invalid("invalid_image_generation_mask"));
        }
    }
    let partials = input.options.partial_images.unwrap_or(0);
    if partials > 0 && !config.capabilities.streaming {
        return Err(invalid("image_generation_streaming_unavailable"));
    }
    let fields = input.options.fields();
    validate_capabilities(&config.capabilities, &fields)?;
    match config.api {
        ImageApi::Openrouter => router_request(config, input, &base, fields, partials),
        ImageApi::Openai if edit => edit_request(config, input, &base, fields, partials),
        ImageApi::Openai => {
            let mut body = json!({"model":config.model,"prompt":input.prompt,"n":1});
            for (name, value) in fields {
                body[name] = value;
            }
            if partials > 0 {
                body["stream"] = json!(true);
                body["partial_images"] = json!(partials);
            }
            Ok(PreparedImageRequest {
                endpoint: format!("{base}/images/generations"),
                content_type: "application/json".into(),
                body: serde_json::to_vec(&body)?,
                stream: partials > 0,
                requested_format: input.options.output_format.unwrap_or(Format::Png),
            })
        }
    }
}

fn validate_capabilities(
    capabilities: &Capabilities,
    fields: &BTreeMap<&str, Value>,
) -> worker::Result<()> {
    for (name, value) in fields {
        if !capabilities
            .parameters
            .get(*name)
            .is_some_and(|parameter| parameter.accepts(value))
        {
            return Err(invalid("image_generation_option_unavailable"));
        }
    }
    Ok(())
}

fn router_request(
    config: &ImageConfig,
    input: &ImageInput,
    base: &str,
    fields: BTreeMap<&str, Value>,
    partials: u8,
) -> worker::Result<PreparedImageRequest> {
    let mut body = json!({"model":config.model,"prompt":input.prompt,"n":1});
    if let Some(endpoint) = &config.router_endpoint {
        body["provider"] = json!({"only":[endpoint.provider_tag],"allow_fallbacks":false});
    }
    for (name, value) in fields {
        if let Some(endpoint) = &config.router_endpoint
            && endpoint.passthrough_parameters.contains(name)
        {
            if body["provider"].get("options").is_none() {
                body["provider"]["options"] = json!({});
            }
            if body["provider"]["options"]
                .get(&endpoint.provider_slug)
                .is_none()
            {
                body["provider"]["options"][&endpoint.provider_slug] = json!({});
            }
            body["provider"]["options"][&endpoint.provider_slug][name] = value;
        } else {
            body[name] = value;
        }
    }
    if !input.images.is_empty() {
        body["input_references"] = json!(
            input
                .images
                .iter()
                .map(|image| json!({"type":"image_url","image_url":{"url":image.data_url()}}))
                .collect::<Vec<_>>()
        );
    }
    if input.mask.is_some() {
        return Err(invalid("image_generation_mask_unavailable"));
    }
    if partials > 0 {
        body["stream"] = json!(true);
    }
    Ok(PreparedImageRequest {
        endpoint: format!("{base}/images"),
        content_type: "application/json".into(),
        body: serde_json::to_vec(&body)?,
        stream: partials > 0,
        requested_format: input.options.output_format.unwrap_or(Format::Png),
    })
}

fn edit_request(
    config: &ImageConfig,
    input: &ImageInput,
    base: &str,
    fields: BTreeMap<&str, Value>,
    partials: u8,
) -> worker::Result<PreparedImageRequest> {
    let boundary = format!("magents-{}", uuid::Uuid::new_v4().simple());
    let mut body = Vec::new();
    for (name, value) in [
        ("model", json!(config.model)),
        ("prompt", json!(input.prompt)),
        ("n", json!(1)),
    ]
    .into_iter()
    .chain(fields)
    {
        multipart_field(&mut body, &boundary, name, &value)?;
    }
    if partials > 0 {
        multipart_field(&mut body, &boundary, "stream", &json!(true))?;
        multipart_field(&mut body, &boundary, "partial_images", &json!(partials))?;
    }
    for (ordinal, image) in input.images.iter().enumerate() {
        multipart_image(&mut body, &boundary, "image[]", ordinal, image);
    }
    if let Some(mask) = &input.mask {
        multipart_image(&mut body, &boundary, "mask", 0, mask);
    }
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    Ok(PreparedImageRequest {
        endpoint: format!("{base}/images/edits"),
        content_type: format!("multipart/form-data; boundary={boundary}"),
        body,
        stream: partials > 0,
        requested_format: input.options.output_format.unwrap_or(Format::Png),
    })
}

fn multipart_field(
    body: &mut Vec<u8>,
    boundary: &str,
    name: &str,
    value: &Value,
) -> worker::Result<()> {
    let text = value
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| value.to_string());
    body.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{text}\r\n"
        )
        .as_bytes(),
    );
    Ok(())
}

fn multipart_image(body: &mut Vec<u8>, boundary: &str, name: &str, ordinal: usize, image: &Raster) {
    body.extend_from_slice(format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"; filename=\"image-{ordinal}.{}\"\r\nContent-Type: {}\r\n\r\n",image.format.name(),image.format.mime()).as_bytes());
    body.extend_from_slice(&image.bytes());
    body.extend_from_slice(b"\r\n");
}
