use std::{future::Future, time::Duration};

use eventsource_stream::Eventsource;
use futures::{Stream, StreamExt, future::Either};
use worker::{Fetch, Headers, Method, Request, RequestInit, RequestRedirect, Response};

use super::*;
use crate::telemetry::{NativeSpan, TraceContext};

const MAX_JSON_BYTES: usize = 24 * 1024 * 1024;
const MAX_STREAM_BYTES: usize = 4 * MAX_JSON_BYTES;

/// The caller must durably fence this operation before invoking it. The
/// Idempotency-Key assists capable upstreams; it is not an exactly-once claim.
pub(crate) async fn generate<F, Fut, C, Captured>(
    config: &ImageConfig,
    input: &ImageInput,
    operation_key: &str,
    mut partial: F,
    mut capture: C,
) -> Result<ImageExchange, ImageFailure>
where
    F: FnMut(PartialImage) -> Fut,
    Fut: Future<Output = worker::Result<()>>,
    C: FnMut(ProviderReply) -> Captured,
    Captured: Future<Output = worker::Result<ProviderReply>>,
{
    let started = crate::util::now_ms();
    let mut observed = ImageObservation::default();
    if operation_key.is_empty()
        || operation_key.len() > 256
        || !operation_key.bytes().all(|byte| byte.is_ascii_graphic())
    {
        return Err(ImageFailure::new(
            invalid("invalid_image_generation_operation"),
            observed,
        ));
    }
    let prepared =
        prepare(config, input).map_err(|error| ImageFailure::new(error, observed.clone()))?;
    observed.expected_format = prepared.requested_format;
    let trace = config.traceparent.as_deref().and_then(TraceContext::parse);
    let _span = NativeSpan::start("tool.image_generation", trace.as_ref());
    let generated = bounded(
        async {
            let headers = Headers::new();
            headers.set("Content-Type", &prepared.content_type)?;
            headers.set("Authorization", &format!("Bearer {}", config.api_key))?;
            headers.set("Idempotency-Key", operation_key)?;
            headers.set("X-Client-Request-Id", operation_key)?;
            if let Some(trace) = &trace {
                trace.inject(&headers)?;
            }
            let stream = prepared.stream;
            let format = prepared.requested_format;
            let mut init = RequestInit::new();
            init.with_method(Method::Post)
                .with_headers(headers)
                .with_redirect(RequestRedirect::Manual)
                .with_body(Some(
                    js_sys::Uint8Array::from(prepared.body.as_slice()).into(),
                ));
            let request = Request::new_with_init(&prepared.endpoint, &init)?;
            observed.phase = ImagePhase::Send;
            crate::util::emit("tool.image_generation.send", json!({"execution_id":operation_key,
                "model":config.model,"api":config.api,"endpoint":prepared.endpoint,
                "request_bytes":prepared.body.len(),"expected_format":format,"stream":stream}));
            let mut response = Fetch::Request(request).send().await?;
            observed.phase = ImagePhase::Headers;
            observed.upstream_status = Some(response.status_code());
            observed.content_type = response
                .headers()
                .get("content-type")?
                .filter(|v| v.len() <= 256 && !v.chars().any(char::is_control));
            observed.content_encoding = response
                .headers()
                .get("content-encoding")?
                .filter(|v| v.len() <= 64 && !v.chars().any(char::is_control));
            observed.declared_content_length = response
                .headers()
                .get("content-length")?
                .and_then(|v| v.parse::<u64>().ok());
            observed.cf_ray = response
                .headers()
                .get("cf-ray")?
                .filter(|v| observation::identifier(v));
            observed.request_id = response
                .headers()
                .get("x-request-id")?
                .filter(|v| observation::identifier(v));
            observed.generation_id = response
                .headers()
                .get("x-generation-id")?
                .filter(|v| observation::identifier(v));
            crate::util::emit("tool.image_generation.headers", json!({"execution_id":operation_key,
                "upstream_status":observed.upstream_status,"content_type":observed.content_type,
                "content_encoding":observed.content_encoding,"content_length":observed.declared_content_length,
                "provider_request_id":observed.request_id,"provider_generation_id":observed.generation_id,
                "cf_ray":observed.cf_ray}));
            let request_id = observed
                .generation_id
                .clone()
                .or_else(|| observed.request_id.clone());
            let openrouter = config.api == ImageApi::Openrouter
                && config.base_url.trim_end_matches('/') == "https://openrouter.ai/api/v1";
            let mut result = if stream {
                consume_stream_observed(
                    &mut response,
                    format,
                    request_id,
                    input.options.partial_images.unwrap_or(0),
                    &mut partial,
                    &mut capture,
                    &mut observed,
                    input,
                    started,
                    openrouter,
                )
                .await
            } else {
                observed.phase = ImagePhase::ReadBody;
                let body = read_observed_bytes(&mut response, &mut observed).await?;
                observed.duration_ms = crate::util::now_ms().saturating_sub(started);
                let reply = reply(0, body, &observed, input, format, false)?;
                observed.phase = ImagePhase::RetainBody;
                let retained = capture(reply).await?;
                observed.phase = ImagePhase::ParseJson;
                let value = parse_json(&retained.body, &mut observed)?;
                observed.observe_json(&value, openrouter);
                if observed
                    .upstream_status
                    .is_some_and(|status| !(200..300).contains(&status))
                {
                    return Err(invalid("image_generation_upstream_error"));
                }
                observed.phase = ImagePhase::DecodeImage;
                parse_buffered_observed(value, format, request_id, &mut observed)
            }?;
            observed.observe_result(&result);
            observed.phase = ImagePhase::ValidateImage;
            result.requested_size = input
                .options
                .size
                .as_ref()
                .filter(|size| size.as_str() != "auto")
                .cloned();
            if let ImageSource::Inline { image } = &result.source {
                validate_image(&result, image)?;
            }
            Ok(result)
        },
        300_000,
    )
    .await;
    observed.duration_ms = crate::util::now_ms().saturating_sub(started);
    match generated {
        Ok(result) => {
            observed.phase = ImagePhase::Complete;
            Ok(ImageExchange {
                result,
                observation: observed,
            })
        }
        Err(error) => Err(ImageFailure::new(error, observed)),
    }
}

fn reply(
    sequence: u16,
    body: Vec<u8>,
    observed: &ImageObservation,
    input: &ImageInput,
    expected_format: Option<Format>,
    event_stream: bool,
) -> worker::Result<ProviderReply> {
    Ok(ProviderReply {
        sequence,
        meta: crate::provider_response_receipt::ResponseMeta {
            status: observed
                .upstream_status
                .ok_or_else(|| invalid("image_response_status_missing"))?,
            content_type: observed.content_type.clone(),
            request_id: observed.request_id.clone(),
            generation_id: observed.generation_id.clone(),
            duration_ms: observed.duration_ms,
            stage: crate::provider_response_receipt::ResponseStage::BodyReceived,
        },
        body,
        expected_format,
        requested_size: input.options.size.clone(),
        event_stream,
        observation: observed.clone(),
    })
}

async fn read_observed_bytes(
    response: &mut Response,
    observed: &mut ImageObservation,
) -> worker::Result<Vec<u8>> {
    let mut source = response.stream()?;
    let mut bytes = Vec::new();
    while let Some(chunk) = source.next().await {
        let chunk = chunk?;
        observed.received_bytes = observed.received_bytes.saturating_add(chunk.len() as u64);
        if chunk.len() > MAX_JSON_BYTES.saturating_sub(bytes.len()) {
            return Err(invalid("image_generation_response_too_large"));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

fn parse_json(bytes: &[u8], observed: &mut ImageObservation) -> worker::Result<Value> {
    serde_json::from_slice(bytes).map_err(|error| {
        observed.parser_line = Some(error.line());
        observed.parser_column = Some(error.column());
        invalid("image_generation_protocol_error")
    })
}

/// Reprocess authenticated retained bytes, never another generation request.
pub(crate) fn recover_received(
    reply: &ProviderReply,
    openrouter: bool,
) -> Result<ImageExchange, ImageFailure> {
    let mut observed = ImageObservation {
        phase: ImagePhase::ParseJson,
        upstream_status: Some(reply.meta.status),
        request_id: reply.meta.request_id.clone(),
        generation_id: reply.meta.generation_id.clone(),
        content_type: reply.meta.content_type.clone(),
        duration_ms: reply.meta.duration_ms,
        expected_format: reply.expected_format,
        received_bytes: reply.body.len() as u64,
        ..reply.observation.clone()
    };
    let result = (|| {
        let value = parse_json(&reply.body, &mut observed)?;
        observed.observe_json(&value, openrouter);
        if !(200..300).contains(&reply.meta.status) || value["type"] == "error" {
            return Err(invalid("image_generation_upstream_error"));
        }
        observed.phase = ImagePhase::DecodeImage;
        let id = observed
            .generation_id
            .clone()
            .or_else(|| observed.request_id.clone());
        let mut result = if reply.event_stream {
            if value["type"] != "image_generation.completed" {
                return Err(invalid("image_generation_outcome_unknown"));
            }
            parse_completed_event(value, reply.expected_format, id, &mut observed)?
        } else {
            parse_buffered_observed(value, reply.expected_format, id, &mut observed)?
        };
        result.requested_size = reply.requested_size.clone().filter(|s| s != "auto");
        observed.observe_result(&result);
        observed.phase = ImagePhase::ValidateImage;
        if let ImageSource::Inline { image } = &result.source {
            validate_image(&result, image)?;
        }
        Ok(result)
    })();
    match result {
        Ok(result) => {
            observed.phase = ImagePhase::Complete;
            Ok(ImageExchange {
                result,
                observation: observed,
            })
        }
        Err(error) => Err(ImageFailure::new(error, observed)),
    }
}

fn parse_completed_event(
    mut value: Value,
    format: Option<Format>,
    request_id: Option<String>,
    observed: &mut ImageObservation,
) -> worker::Result<ImageResult> {
    let fields = value
        .as_object_mut()
        .ok_or_else(|| invalid("image_generation_protocol_error"))?;
    let mut item = serde_json::Map::new();
    for name in ["b64_json", "media_type"] {
        if let Some(value) = fields.remove(name) {
            item.insert(name.into(), value);
        }
    }
    parse_result(value, Value::Object(item), format, request_id, observed)
}

async fn bounded<T>(
    future: impl Future<Output = worker::Result<T>>,
    timeout_ms: u64,
) -> worker::Result<T> {
    match futures::future::select(
        Box::pin(future),
        Box::pin(worker::Delay::from(Duration::from_millis(timeout_ms))),
    )
    .await
    {
        Either::Left((result, _)) => result,
        Either::Right(_) => Err(invalid("image_generation_outcome_unknown")),
    }
}

async fn read_bytes(response: &mut Response, maximum: usize) -> worker::Result<Vec<u8>> {
    let mut stream = response.stream()?;
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        if chunk.len() > maximum.saturating_sub(bytes.len()) {
            return Err(invalid("image_generation_response_too_large"));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

#[cfg(test)]
pub(super) fn parse_buffered(
    value: Value,
    requested: Option<Format>,
    request_id: Option<String>,
) -> worker::Result<ImageResult> {
    parse_buffered_observed(
        value,
        requested,
        request_id,
        &mut ImageObservation::default(),
    )
}

fn parse_buffered_observed(
    mut value: Value,
    requested: Option<Format>,
    request_id: Option<String>,
    observed: &mut ImageObservation,
) -> worker::Result<ImageResult> {
    let items = value
        .get_mut("data")
        .and_then(Value::as_array_mut)
        .filter(|items| items.len() == 1)
        .ok_or_else(|| invalid("image_generation_protocol_error"))?;
    let item = items.pop().unwrap();
    parse_result(value, item, requested, request_id, observed)
}

fn parse_result(
    value: Value,
    mut item: Value,
    requested: Option<Format>,
    request_id: Option<String>,
    observed: &mut ImageObservation,
) -> worker::Result<ImageResult> {
    if value.get("error").is_some_and(|value| !value.is_null()) {
        return Err(invalid("image_generation_upstream_error"));
    }
    let source = image_source(&mut item, requested, observed)?;
    let output_format = match &source {
        ImageSource::Inline { image } => Some(image.format()),
        ImageSource::Remote { .. } => requested,
    };
    let provider_request_id = value
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| valid_request_id(id))
        .map(str::to_owned)
        .or(request_id);
    let created_at = value
        .get("created_at")
        .or_else(|| value.get("created"))
        .and_then(Value::as_u64);
    let revised_prompt = item
        .get("revised_prompt")
        .or_else(|| value.get("revised_prompt"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    let usage = value
        .get("usage")
        .filter(|usage| usage.is_object())
        .cloned();
    let background = value
        .get("background")
        .and_then(Value::as_str)
        .filter(|value| matches!(*value, "auto" | "opaque" | "transparent"))
        .map(str::to_owned);
    let quality = value
        .get("quality")
        .and_then(Value::as_str)
        .filter(|value| matches!(*value, "auto" | "low" | "medium" | "high" | "xhigh" | "max"))
        .map(str::to_owned);
    Ok(ImageResult {
        source,
        provider_request_id,
        created_at,
        revised_prompt,
        output_format,
        background,
        quality,
        requested_size: None,
        usage,
    })
}

fn image_source(
    item: &mut Value,
    requested: Option<Format>,
    observed: &mut ImageObservation,
) -> worker::Result<ImageSource> {
    let declared = item
        .get("media_type")
        .and_then(Value::as_str)
        .map(str::to_owned);
    if let Some(encoded) = item
        .as_object_mut()
        .and_then(|item| item.remove("b64_json"))
        .filter(|value| !value.is_null())
    {
        let Value::String(encoded) = encoded else {
            return Err(invalid("image_generation_protocol_error"));
        };
        let image = Raster::from_encoded(encoded)?;
        let (width, height) = image.dimensions();
        observed.actual_format = Some(image.format());
        observed.width = Some(width);
        observed.height = Some(height);
        observed.image_bytes = Some(image.len() as u64);
        observed.phase = ImagePhase::ValidateImage;
        if requested.is_some_and(|format| image.format() != format)
            || declared
                .as_deref()
                .is_some_and(|mime| Format::from_mime(mime) != Some(image.format()))
        {
            return Err(invalid("image_generation_format_mismatch"));
        }
        return Ok(ImageSource::Inline { image });
    }
    let url = item
        .get("url")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("image_generation_protocol_error"))?;
    public_asset_url(url)?;
    Ok(ImageSource::Remote {
        url: url.into(),
        media_type: declared,
    })
}

fn valid_request_id(value: &str) -> bool {
    !value.is_empty() && value.len() <= 256 && value.bytes().all(|byte| byte.is_ascii_graphic())
}

#[allow(clippy::too_many_arguments)]
async fn consume_stream_observed<F, Fut, C, Captured>(
    response: &mut Response,
    format: Option<Format>,
    request_id: Option<String>,
    maximum_partials: u8,
    partial: &mut F,
    capture: &mut C,
    observed: &mut ImageObservation,
    input: &ImageInput,
    started: u64,
    openrouter: bool,
) -> worker::Result<ImageResult>
where
    F: FnMut(PartialImage) -> Fut,
    Fut: Future<Output = worker::Result<()>>,
    C: FnMut(ProviderReply) -> Captured,
    Captured: Future<Output = worker::Result<ProviderReply>>,
{
    if !response
        .headers()
        .get("content-type")?
        .is_some_and(|value| value.to_ascii_lowercase().starts_with("text/event-stream"))
    {
        return Err(invalid("image_generation_protocol_error"));
    }
    decode_stream_observed(
        response.stream()?,
        format,
        request_id,
        maximum_partials,
        partial,
        capture,
        observed,
        input,
        started,
        openrouter,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn decode_stream_observed<S, F, Fut, C, Captured>(
    source: S,
    format: Option<Format>,
    request_id: Option<String>,
    maximum_partials: u8,
    partial: &mut F,
    capture: &mut C,
    observed: &mut ImageObservation,
    input: &ImageInput,
    started: u64,
    openrouter: bool,
) -> worker::Result<ImageResult>
where
    S: Stream<Item = worker::Result<Vec<u8>>> + Unpin,
    F: FnMut(PartialImage) -> Fut,
    Fut: Future<Output = worker::Result<()>>,
    C: FnMut(ProviderReply) -> Captured,
    Captured: Future<Output = worker::Result<ProviderReply>>,
{
    let mut total = 0usize;
    let source = source.map(move |chunk| {
        let chunk = chunk?;
        total = total.saturating_add(chunk.len());
        if total > MAX_STREAM_BYTES {
            return Err(invalid("image_generation_response_too_large"));
        }
        Ok(chunk)
    });
    let mut events = source.eventsource();
    let mut previews = 0u8;
    let mut sequence = 0u16;
    while let Some(event) = events.next().await {
        let event = event.map_err(|_| invalid("image_generation_protocol_error"))?;
        if event.data == "[DONE]" {
            break;
        }
        if event.data.len() > MAX_JSON_BYTES {
            return Err(invalid("image_generation_response_too_large"));
        }
        if sequence >= 16 {
            return Err(invalid("image_generation_event_limit"));
        }
        observed.received_bytes = observed
            .received_bytes
            .saturating_add(event.data.len() as u64);
        observed.duration_ms = crate::util::now_ms().saturating_sub(started);
        let reply = reply(
            sequence,
            event.data.into_bytes(),
            observed,
            input,
            format,
            true,
        )?;
        sequence += 1;
        observed.phase = ImagePhase::RetainBody;
        let retained = capture(reply).await?;
        observed.phase = ImagePhase::ParseJson;
        let mut value = parse_json(&retained.body, observed)?;
        observed.observe_json(&value, openrouter);
        observed.phase = ImagePhase::DecodeImage;
        match value["type"].as_str() {
            Some("image_generation.partial_image") => {
                let index = value["partial_image_index"]
                    .as_u64()
                    .filter(|index| *index < 256)
                    .ok_or_else(|| invalid("image_generation_protocol_error"))?
                    as u8;
                if index != previews {
                    return Err(invalid("image_generation_protocol_error"));
                }
                previews = previews
                    .checked_add(1)
                    .ok_or_else(|| invalid("image_generation_protocol_error"))?;
                if index < maximum_partials {
                    let ImageSource::Inline { image } = image_source(&mut value, format, observed)?
                    else {
                        return Err(invalid("image_generation_protocol_error"));
                    };
                    partial(PartialImage { index, image }).await?;
                }
            }
            Some("image_generation.completed") => {
                // The authenticated completed event is the known outcome; a
                // later transport close must not turn it into another paid call.
                return parse_completed_event(value, format, request_id, observed);
            }
            Some("error") => return Err(invalid("image_generation_upstream_error")),
            _ => return Err(invalid("image_generation_protocol_error")),
        }
    }
    Err(invalid("image_generation_outcome_unknown"))
}

#[cfg(test)]
async fn decode_stream<S, F, Fut>(
    source: S,
    format: Option<Format>,
    request_id: Option<String>,
    maximum_partials: u8,
    partial: &mut F,
) -> worker::Result<ImageResult>
where
    S: Stream<Item = worker::Result<Vec<u8>>> + Unpin,
    F: FnMut(PartialImage) -> Fut,
    Fut: Future<Output = worker::Result<()>>,
{
    let mut observed = ImageObservation {
        upstream_status: Some(200),
        ..Default::default()
    };
    let input = ImageInput {
        prompt: "synthetic".into(),
        images: vec![],
        mask: None,
        options: ImageOptions::default(),
    };
    decode_stream_observed(
        source,
        format,
        request_id,
        maximum_partials,
        partial,
        &mut |reply| futures::future::ready(Ok(reply)),
        &mut observed,
        &input,
        crate::util::now_ms(),
        false,
    )
    .await
}

/// A known generation result is persisted before this idempotent asset read.
/// Only explicitly pinned public origins are eligible; no API key is forwarded.
pub(crate) async fn materialize(
    config: &ImageConfig,
    result: &ImageResult,
) -> worker::Result<Raster> {
    let ImageSource::Remote { url, media_type } = &result.source else {
        let ImageSource::Inline { image } = &result.source else {
            unreachable!()
        };
        validate_image(result, image)?;
        return Ok(image.clone());
    };
    let image = fetch_image_asset(url, &config.asset_hosts, MAX_IMAGE_BYTES).await?;
    if media_type.as_deref().is_some_and(|mime| {
        mime != "application/octet-stream" && Format::from_mime(mime) != Some(image.format())
    }) {
        return Err(invalid("image_generation_format_mismatch"));
    }
    validate_image(result, &image)?;
    Ok(image)
}

/// Shared public-Internet reader, with no provider configuration or secrets.
/// An explicit origin pin remains mandatory even for caller-selected input.
pub(crate) async fn fetch_image_asset(
    url: &str,
    allowed_hosts: &[String],
    max_bytes: usize,
) -> worker::Result<Raster> {
    let fetched = fetch_public_image_bytes(
        url,
        allowed_hosts,
        max_bytes,
        "image/png,image/jpeg,image/webp",
    )
    .await?;
    let image = Raster::from_bytes(&fetched.bytes)?;
    if fetched.declared_mime.as_deref().is_some_and(|mime| {
        mime != "application/octet-stream" && Format::from_mime(mime) != Some(image.format())
    }) {
        return Err(invalid("image_generation_format_mismatch"));
    }
    Ok(image)
}

pub(crate) struct FetchedImageBytes {
    pub bytes: Vec<u8>,
    pub declared_mime: Option<String>,
}

/// One credential-free, origin-pinned, redirect-denying reader for all images.
pub(crate) async fn fetch_public_image_bytes(
    url: &str,
    allowed_hosts: &[String],
    max_bytes: usize,
    accept: &str,
) -> worker::Result<FetchedImageBytes> {
    if max_bytes == 0 || max_bytes > MAX_IMAGE_BYTES {
        return Err(invalid("invalid_image_generation_asset_limit"));
    }
    let parsed = public_asset_url(url)?;
    if !allowed_hosts
        .iter()
        .any(|host| host.eq_ignore_ascii_case(parsed.host_str().unwrap_or_default()))
    {
        return Err(invalid("image_generation_asset_origin_denied"));
    }
    bounded(
        async {
            let headers = Headers::new();
            headers.set("Accept", accept)?;
            let mut init = RequestInit::new();
            init.with_method(Method::Get)
                .with_headers(headers)
                .with_redirect(RequestRedirect::Manual);
            let request = Request::new_with_init(url, &init)?;
            let mut response = Fetch::Request(request).send().await?;
            if response.status_code() != 200 {
                return Err(invalid("image_generation_asset_unavailable"));
            }
            if response
                .headers()
                .get("content-length")?
                .and_then(|value| value.parse::<u64>().ok())
                .is_some_and(|bytes| bytes > max_bytes as u64)
            {
                return Err(invalid("image_generation_image_too_large"));
            }
            let declared = response
                .headers()
                .get("content-type")?
                .and_then(|value| value.split(';').next().map(str::trim).map(str::to_owned));
            Ok(FetchedImageBytes {
                bytes: read_bytes(&mut response, max_bytes).await?,
                declared_mime: declared,
            })
        },
        30_000,
    )
    .await
}

fn validate_image(result: &ImageResult, image: &Raster) -> worker::Result<()> {
    if result
        .output_format
        .is_some_and(|format| format != image.format())
        || result
            .requested_size
            .as_deref()
            .map(dimensions)
            .transpose()?
            .is_some_and(|size| size != image.dimensions())
    {
        return Err(invalid("image_generation_output_mismatch"));
    }
    Ok(())
}

fn public_asset_url(value: &str) -> worker::Result<url::Url> {
    if value.len() > 8192 || value.chars().any(char::is_control) {
        return Err(invalid("invalid_image_generation_asset"));
    }
    let mut url = url::Url::parse(value).map_err(|_| invalid("invalid_image_generation_asset"))?;
    if url.fragment().is_some() {
        return Err(invalid("invalid_image_generation_asset"));
    }
    let query = url.query().map(str::to_owned);
    url.set_query(None);
    crate::auth::provider_keys::normalize_base_url(
        agents_core::ProviderProtocol::Openai,
        url.as_str(),
    )?;
    url.set_query(query.as_deref());
    Ok(url)
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::{executor::block_on, future, stream};

    fn image() -> Raster {
        super::super::tests::raster()
    }

    fn frame(value: Value) -> Vec<u8> {
        format!("data: {value}\n\n").into_bytes()
    }

    #[test]
    fn buffered_result_preserves_exact_cost_and_known_request_identity() {
        let raster = image();
        let mut value:Value=serde_json::from_str(r#"{"id":"img_known","created":123,"usage":{"cost":0.000000000123,"input_tokens":12,"output_tokens":34,"total_tokens":46},"data":[]}"#).unwrap();
        value["data"] = json!([{"b64_json":raster.encoded(),"media_type":"IMAGE/PNG","revised_prompt":"Revised private prompt"}]);
        let result = parse_buffered(value, Some(Format::Png), None).unwrap();
        assert_eq!(result.provider_request_id.as_deref(), Some("img_known"));
        assert_eq!(
            result.reported_cost().unwrap().to_string(),
            "0.000000000123"
        );
        assert_eq!(result.created_at, Some(123));
        let ImageSource::Inline { image } = result.source else {
            panic!("expected inline")
        };
        assert_eq!(image.bytes(), raster.bytes());
    }

    #[test]
    fn url_outputs_are_known_but_never_fetched_by_response_parsing() {
        let result=parse_buffered(json!({"data":[{"b64_json":null,"url":"https://cdn.example.com/output.png?opaque=signature","media_type":"image/png"}],"usage":{"cost":"0.05"}}),Some(Format::Png),Some("request_header".into())).unwrap();
        assert_eq!(
            result.provider_request_id.as_deref(),
            Some("request_header")
        );
        assert!(matches!(result.source, ImageSource::Remote { .. }));
        for url in [
            "http://cdn.example.com/image.png",
            "https://127.0.0.1/image.png",
            "https://secret@cdn.example.com/image.png",
            "https://metadata.google.internal/file",
            "https://cdn.example.com/x#fragment",
        ] {
            assert!(public_asset_url(url).is_err(), "{url}");
        }
        assert_eq!(
            public_asset_url("https://cdn.example.com/image.png?signed=opaque")
                .unwrap()
                .query(),
            Some("signed=opaque")
        );
    }

    #[test]
    fn wrong_format_and_missing_or_multiple_images_do_not_fabricate_a_result() {
        let raster = image();
        assert!(
            parse_buffered(
                json!({"data":[{"b64_json":raster.encoded()}]}),
                Some(Format::Jpeg),
                None
            )
            .is_err()
        );
        assert!(parse_buffered(json!({"data":[]}), Some(Format::Png), None).is_err());
        assert!(
            parse_buffered(
                json!({"data":[{"b64_json":raster.encoded()},{"b64_json":raster.encoded()}]}),
                Some(Format::Png),
                None
            )
            .is_err()
        );
        assert!(
            parse_buffered(
                json!({"data":[{"b64_json":raster.encoded(),"media_type":"image/svg+xml"}]}),
                Some(Format::Png),
                None
            )
            .is_err()
        );
    }

    #[test]
    fn codec_rejection_preserves_actual_raster_and_original_paid_receipt_metadata() {
        let raster = image();
        let body = serde_json::to_vec(&json!({"id":"gen-img-original",
            "data":[{"b64_json":raster.encoded(),"media_type":"image/png"}],
            "usage":{"cost":"0.018"}}))
        .unwrap();
        let meta = crate::provider_response_receipt::ResponseMeta {
            status: 200,
            content_type: Some("application/json".into()),
            request_id: Some("http-request-original".into()),
            generation_id: None,
            duration_ms: 9300,
            stage: crate::provider_response_receipt::ResponseStage::BodyReceived,
        };
        let reply = ProviderReply {
            sequence: 0,
            meta,
            body,
            expected_format: Some(Format::Jpeg),
            requested_size: None,
            event_stream: false,
            observation: ImageObservation::default(),
        };
        let failure = recover_received(&reply, true).err().unwrap();
        assert_eq!(failure.code, "image_generation_format_mismatch");
        assert!(failure.observation.provider_completed);
        assert_eq!(
            failure.observation.generation_id.as_deref(),
            Some("gen-img-original")
        );
        assert_eq!(
            failure.observation.request_id.as_deref(),
            Some("http-request-original")
        );
        assert_eq!(failure.observation.actual_format, Some(Format::Png));
        assert_eq!(failure.observation.expected_format, Some(Format::Jpeg));
        assert_eq!(
            (failure.observation.width, failure.observation.height),
            (Some(2), Some(3))
        );
        assert_eq!(failure.observation.reported_cost_usd, Some(json!("0.018")));
        assert!(!failure.observation.confirmed_no_charge);
    }

    #[test]
    fn streamed_preview_and_final_are_real_images_and_terminal_wins_over_later_disconnect() {
        let raster = image();
        let bytes=[frame(json!({"type":"image_generation.partial_image","partial_image_index":0,"b64_json":raster.encoded()})),
            frame(json!({"type":"image_generation.completed","created_at":123,"b64_json":raster.encoded(),"output_format":"png","usage":{"input_tokens":2,"output_tokens":3,"total_tokens":5,"cost":"0.04"}}))].concat();
        let chunks = bytes
            .chunks(11)
            .map(|chunk| Ok(chunk.to_vec()))
            .chain([Err(invalid("native_connection_closed"))]);
        let mut previews = vec![];
        let mut sink = |preview: PartialImage| {
            previews.push((preview.index, preview.image.dimensions()));
            future::ready(Ok(()))
        };
        let result = block_on(decode_stream(
            stream::iter(chunks),
            Some(Format::Png),
            None,
            1,
            &mut sink,
        ))
        .unwrap();
        drop(sink);
        assert_eq!(previews, vec![(0, (2, 3))]);
        assert_eq!(result.reported_cost(), Some(&json!("0.04")));
        assert_eq!(result.created_at, Some(123));
    }

    #[test]
    fn partial_only_eof_and_bad_preview_order_remain_uncertain() {
        let raster = image();
        let mut sink = |_: PartialImage| future::ready(Ok(()));
        let preview = frame(
            json!({"type":"image_generation.partial_image","partial_image_index":0,"b64_json":raster.encoded()}),
        );
        assert!(
            block_on(decode_stream(
                stream::iter([Ok(preview)]),
                Some(Format::Png),
                None,
                1,
                &mut sink
            ))
            .is_err()
        );
        let bad = frame(
            json!({"type":"image_generation.partial_image","partial_image_index":1,"b64_json":raster.encoded()}),
        );
        assert!(
            block_on(decode_stream(
                stream::iter([Ok(bad)]),
                Some(Format::Png),
                None,
                1,
                &mut sink
            ))
            .is_err()
        );
        let error = frame(json!({"type":"error","error":{"message":"private upstream body"}}));
        let error = block_on(decode_stream(
            stream::iter([Ok(error)]),
            Some(Format::Png),
            None,
            1,
            &mut sink,
        ))
        .err()
        .unwrap();
        assert!(!error.to_string().contains("private upstream"));
    }
}
