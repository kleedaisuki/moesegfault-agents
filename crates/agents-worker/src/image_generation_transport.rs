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
pub(crate) async fn generate<F, Fut>(
    config: &ImageConfig,
    input: &ImageInput,
    operation_key: &str,
    mut partial: F,
) -> worker::Result<ImageResult>
where
    F: FnMut(PartialImage) -> Fut,
    Fut: Future<Output = worker::Result<()>>,
{
    if operation_key.is_empty()
        || operation_key.len() > 256
        || !operation_key.bytes().all(|byte| byte.is_ascii_graphic())
    {
        return Err(invalid("invalid_image_generation_operation"));
    }
    let prepared = prepare(config, input)?;
    let trace = config.traceparent.as_deref().and_then(TraceContext::parse);
    let _span = NativeSpan::start("tool.image_generation", trace.as_ref());
    bounded(
        async {
            let headers = Headers::new();
            headers.set("Content-Type", &prepared.content_type)?;
            headers.set("Authorization", &format!("Bearer {}", config.api_key))?;
            headers.set("Idempotency-Key", operation_key)?;
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
            let mut response = Fetch::Request(request).send().await?;
            if !(200..300).contains(&response.status_code()) {
                return Err(invalid("image_generation_upstream_error"));
            }
            let request_id = response
                .headers()
                .get("x-request-id")?
                .filter(|value| valid_request_id(value));
            let mut result = if stream {
                consume_stream(
                    &mut response,
                    format,
                    request_id,
                    input.options.partial_images.unwrap_or(0),
                    &mut partial,
                )
                .await
            } else {
                parse_buffered(read_json(&mut response).await?, format, request_id)
            }?;
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
    .await
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

async fn read_json(response: &mut Response) -> worker::Result<Value> {
    serde_json::from_slice(&read_bytes(response, MAX_JSON_BYTES).await?)
        .map_err(|_| invalid("image_generation_protocol_error"))
}

pub(super) fn parse_buffered(
    mut value: Value,
    requested: Format,
    request_id: Option<String>,
) -> worker::Result<ImageResult> {
    let items = value
        .get_mut("data")
        .and_then(Value::as_array_mut)
        .filter(|items| items.len() == 1)
        .ok_or_else(|| invalid("image_generation_protocol_error"))?;
    let item = items.pop().unwrap();
    parse_result(value, item, requested, request_id)
}

fn parse_result(
    value: Value,
    mut item: Value,
    requested: Format,
    request_id: Option<String>,
) -> worker::Result<ImageResult> {
    if value.get("error").is_some_and(|value| !value.is_null()) {
        return Err(invalid("image_generation_upstream_error"));
    }
    let source = image_source(&mut item, requested)?;
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
        output_format: Some(requested),
        background,
        quality,
        requested_size: None,
        usage,
    })
}

fn image_source(item: &mut Value, requested: Format) -> worker::Result<ImageSource> {
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
        if image.format() != requested
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

async fn consume_stream<F, Fut>(
    response: &mut Response,
    format: Format,
    request_id: Option<String>,
    maximum_partials: u8,
    partial: &mut F,
) -> worker::Result<ImageResult>
where
    F: FnMut(PartialImage) -> Fut,
    Fut: Future<Output = worker::Result<()>>,
{
    if !response
        .headers()
        .get("content-type")?
        .is_some_and(|value| value.to_ascii_lowercase().starts_with("text/event-stream"))
    {
        return Err(invalid("image_generation_protocol_error"));
    }
    decode_stream(
        response.stream()?,
        format,
        request_id,
        maximum_partials,
        partial,
    )
    .await
}

async fn decode_stream<S, F, Fut>(
    source: S,
    format: Format,
    request_id: Option<String>,
    maximum_partials: u8,
    partial: &mut F,
) -> worker::Result<ImageResult>
where
    S: Stream<Item = worker::Result<Vec<u8>>> + Unpin,
    F: FnMut(PartialImage) -> Fut,
    Fut: Future<Output = worker::Result<()>>,
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
    while let Some(event) = events.next().await {
        let event = event.map_err(|_| invalid("image_generation_protocol_error"))?;
        if event.data == "[DONE]" {
            break;
        }
        if event.data.len() > MAX_JSON_BYTES {
            return Err(invalid("image_generation_response_too_large"));
        }
        let mut value: Value = serde_json::from_str(&event.data)
            .map_err(|_| invalid("image_generation_protocol_error"))?;
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
                    let ImageSource::Inline { image } = image_source(&mut value, format)? else {
                        return Err(invalid("image_generation_protocol_error"));
                    };
                    partial(PartialImage { index, image }).await?;
                }
            }
            Some("image_generation.completed") => {
                let fields = value
                    .as_object_mut()
                    .ok_or_else(|| invalid("image_generation_protocol_error"))?;
                let mut item = serde_json::Map::new();
                for name in ["b64_json", "media_type"] {
                    if let Some(value) = fields.remove(name) {
                        item.insert(name.into(), value);
                    }
                }
                // The authenticated completed event is the known outcome; a
                // later transport close must not turn it into another paid call.
                return parse_result(value, Value::Object(item), format, request_id);
            }
            _ => return Err(invalid("image_generation_protocol_error")),
        }
    }
    Err(invalid("image_generation_outcome_unknown"))
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
    let parsed = public_asset_url(url)?;
    if !config
        .asset_hosts
        .iter()
        .any(|host| host.eq_ignore_ascii_case(parsed.host_str().unwrap_or_default()))
    {
        return Err(invalid("image_generation_asset_origin_denied"));
    }
    bounded(
        async {
            let headers = Headers::new();
            headers.set("Accept", "image/png,image/jpeg,image/webp")?;
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
                .is_some_and(|bytes| bytes > MAX_IMAGE_BYTES as u64)
            {
                return Err(invalid("image_generation_image_too_large"));
            }
            let declared = response
                .headers()
                .get("content-type")?
                .and_then(|value| value.split(';').next().map(str::trim).map(str::to_owned));
            let image = Raster::from_bytes(&read_bytes(&mut response, MAX_IMAGE_BYTES).await?)?;
            if [media_type.as_deref(), declared.as_deref()]
                .into_iter()
                .flatten()
                .any(|mime| {
                    mime != "application/octet-stream"
                        && Format::from_mime(mime) != Some(image.format())
                })
            {
                return Err(invalid("image_generation_format_mismatch"));
            }
            validate_image(result, &image)?;
            Ok(image)
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
        let result = parse_buffered(value, Format::Png, None).unwrap();
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
        let result=parse_buffered(json!({"data":[{"b64_json":null,"url":"https://cdn.example.com/output.png?opaque=signature","media_type":"image/png"}],"usage":{"cost":"0.05"}}),Format::Png,Some("request_header".into())).unwrap();
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
                Format::Jpeg,
                None
            )
            .is_err()
        );
        assert!(parse_buffered(json!({"data":[]}), Format::Png, None).is_err());
        assert!(
            parse_buffered(
                json!({"data":[{"b64_json":raster.encoded()},{"b64_json":raster.encoded()}]}),
                Format::Png,
                None
            )
            .is_err()
        );
        assert!(
            parse_buffered(
                json!({"data":[{"b64_json":raster.encoded(),"media_type":"image/svg+xml"}]}),
                Format::Png,
                None
            )
            .is_err()
        );
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
            Format::Png,
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
                Format::Png,
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
                Format::Png,
                None,
                1,
                &mut sink
            ))
            .is_err()
        );
        let error = frame(json!({"type":"error","error":{"message":"private upstream body"}}));
        let error = block_on(decode_stream(
            stream::iter([Ok(error)]),
            Format::Png,
            None,
            1,
            &mut sink,
        ))
        .err()
        .unwrap();
        assert!(!error.to_string().contains("private upstream"));
    }
}
