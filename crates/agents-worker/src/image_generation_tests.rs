use super::*;
use proptest::prelude::*;

pub(super) fn raster() -> Raster {
    let mut bytes = Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
        2,
        3,
        image::Rgba([10, 20, 30, 128]),
    ))
    .write_to(&mut bytes, CodecFormat::Png)
    .unwrap();
    Raster::from_bytes(bytes.get_ref()).unwrap()
}

fn config(api: ImageApi) -> ImageConfig {
    let mut parameters = BTreeMap::new();
    for name in [
        "output_format",
        "size",
        "background",
        "input_fidelity",
        "moderation",
        "quality",
        "output_compression",
    ] {
        parameters.insert(name.into(), Parameter::Boolean);
    }
    ImageConfig {
        api,
        base_url: "https://images.example.com/v1".into(),
        model: "model-current".into(),
        public_model: None,
        api_key: "fixture-key-not-exported".into(),
        capabilities: Capabilities {
            streaming: true,
            omit_default_output_format: false,
            parameters,
            max_references: 16,
            mask: true,
        },
        asset_hosts: vec!["cdn.example.com".into()],
        traceparent: None,
        router_endpoint: None,
    }
}

fn input() -> ImageInput {
    ImageInput {
        prompt: "A red square.\nPreserve the requested style.".into(),
        images: vec![],
        mask: None,
        options: ImageOptions::default(),
    }
}

#[test]
fn modern_openai_generation_omits_retired_response_format_and_uses_resolved_model() {
    let config = config(ImageApi::Openai);
    let prepared = prepare(&config, &input()).unwrap();
    assert_eq!(
        prepared.endpoint,
        "https://images.example.com/v1/images/generations"
    );
    let body: Value = serde_json::from_slice(&prepared.body).unwrap();
    assert_eq!(body["model"], "model-current");
    assert_eq!(body["n"], 1);
    assert_eq!(body["output_format"], "png");
    assert!(body.get("response_format").is_none());
    assert!(body.get("api_key").is_none());
    assert!(
        !String::from_utf8(prepared.body)
            .unwrap()
            .contains("fixture-key")
    );
}

#[test]
fn declared_minimal_byok_sends_only_protocol_claimed_model_prompt_and_single_image() {
    for api in [ImageApi::Openai, ImageApi::Openrouter] {
        let mut config = config(api);
        config.model = "unknown-selfhost-model".into();
        config.base_url = "https://images.owner.example/v1".into();
        config.capabilities = Capabilities {
            omit_default_output_format: true,
            ..Default::default()
        };
        let input = input();
        let request = prepare(&config, &input).unwrap();
        assert!(
            request
                .endpoint
                .starts_with("https://images.owner.example/v1/images")
        );
        assert_eq!(
            serde_json::from_slice::<Value>(&request.body).unwrap(),
            json!({"model":"unknown-selfhost-model","prompt":input.prompt,"n":1})
        );
        assert!(
            !String::from_utf8(request.body)
                .unwrap()
                .contains("fixture-key")
        );
    }
}

#[test]
fn edit_uses_binary_multipart_with_generated_filenames_and_local_mask_bytes() {
    let config = config(ImageApi::Openai);
    let mut input = input();
    input.images.push(raster());
    input.mask = Some(raster());
    input.options.input_image_mask = Some(MaskReference {
        file_id: Some("file_mask".into()),
        image_url: None,
    });
    input.options.partial_images = Some(2);
    let prepared = prepare(&config, &input).unwrap();
    assert!(prepared.endpoint.ends_with("/images/edits"));
    assert!(prepared.stream);
    assert!(
        prepared
            .content_type
            .starts_with("multipart/form-data; boundary=magents-")
    );
    let body = String::from_utf8_lossy(&prepared.body);
    for expected in [
        "name=\"image[]\"; filename=\"image-0.png\"",
        "name=\"mask\"",
        "name=\"partial_images\"",
        "Content-Type: image/png",
    ] {
        assert!(body.contains(expected), "{expected}");
    }
    assert!(!body.contains("file_mask"));
    assert!(!body.contains("fixture-key"));
}

#[test]
fn router_uses_dedicated_image_api_and_pinned_provider_passthrough() {
    let mut config = config(ImageApi::Openrouter);
    config.model = "openai/model-current".into();
    config.public_model = Some("model-current".into());
    config.router_endpoint = Some(RouterEndpoint {
        provider_slug: "openai".into(),
        provider_tag: "openai".into(),
        passthrough_parameters: BTreeSet::from(["moderation".into()]),
    });
    let mut input = input();
    input.options.model = Some("model-current".into());
    input.options.moderation = Some("low".into());
    input.images.push(raster());
    let prepared = prepare(&config, &input).unwrap();
    assert!(prepared.endpoint.ends_with("/v1/images"));
    let body: Value = serde_json::from_slice(&prepared.body).unwrap();
    assert_eq!(body["provider"]["only"], json!(["openai"]));
    assert_eq!(body["provider"]["allow_fallbacks"], false);
    assert_eq!(body["provider"]["options"]["openai"]["moderation"], "low");
    assert!(body.get("moderation").is_none());
    assert!(
        body["input_references"][0]["image_url"]["url"]
            .as_str()
            .unwrap()
            .starts_with("data:image/png;base64,")
    );
}

#[test]
fn capability_and_local_validation_happen_before_a_paid_request_is_constructed() {
    let mut config = config(ImageApi::Openai);
    let mut value = input();
    value.options.background = Some("transparent".into());
    value.options.output_format = Some(Format::Jpeg);
    assert!(prepare(&config, &value).is_err());
    value.options = ImageOptions::default();
    value.options.partial_images = Some(4);
    assert!(prepare(&config, &value).is_err());
    value.options.partial_images = Some(1);
    config.capabilities.streaming = false;
    assert!(prepare(&config, &value).is_err());
    value.options = ImageOptions::default();
    config.capabilities.parameters.remove("output_format");
    assert!(prepare(&config, &value).is_err());
    for size in ["0x2", "999999999999999x2", "2x16385", "-1x4", "2x3\n"] {
        assert!(dimensions(size).is_err(), "{size}");
    }
}

#[test]
fn pending_rasters_keep_one_base64_copy_and_revalidate_on_restore() {
    let image = raster();
    assert_eq!(image.dimensions(), (2, 3));
    assert_eq!(image.format(), Format::Png);
    assert_eq!(image.len(), image.bytes().len());
    let serialized = serde_json::to_value(&image).unwrap();
    assert!(serialized.is_string());
    let restored: Raster = serde_json::from_value(serialized).unwrap();
    assert_eq!(restored.bytes(), image.bytes());
    assert_eq!(restored.sha256(), image.sha256());
    assert!(serde_json::from_value::<Raster>(json!("not a base64 image")).is_err());
    assert!(Raster::from_bytes(b"<svg onload='unsafe()'/>").is_err());
    assert!(Raster::from_bytes(b"%PDF-1.7").is_err());
}

#[test]
fn endpoint_catalog_capabilities_are_not_guessed_from_a_model_name() {
    let endpoint = json!({"provider_slug":"openai","provider_tag":"openai","allowed_passthrough_parameters":["moderation"],
        "supports_streaming":true,"supported_parameters":{"quality":{"type":"enum","values":["low","high"]},
        "input_references":{"type":"range","min":0,"max":16}}});
    let capabilities = Capabilities::from_openrouter_endpoint(&endpoint).unwrap();
    assert!(capabilities.streaming);
    assert_eq!(capabilities.max_references, 16);
    assert!(!capabilities.mask);
    assert!(!capabilities.parameters["quality"].accepts(&json!("max")));
    let route = RouterEndpoint::from_catalog(&endpoint).unwrap();
    assert_eq!(route.provider_tag, "openai");
    assert!(route.passthrough_parameters.contains("moderation"));
}

#[test]
fn seedream_default_accepts_native_auto_without_pretending_to_stream() {
    let endpoint = json!({"provider_slug":"seed","provider_tag":"seed","allowed_passthrough_parameters":[],
        "supports_streaming":false,"supported_parameters":{"resolution":{"type":"enum","values":["1K","2K"]},
        "input_references":{"type":"range","min":0,"max":14}}});
    let mut config = config(ImageApi::Openrouter);
    config.model = "bytedance-seed/seedream-5-0-flash".into();
    config.capabilities = Capabilities::from_openrouter_endpoint(&endpoint).unwrap();
    config.router_endpoint = Some(RouterEndpoint::from_catalog(&endpoint).unwrap());
    let mut input = input();
    input.options.quality = Some("auto".into());
    input.options.background = Some("auto".into());
    input.options.size = Some("auto".into());
    let body: Value = serde_json::from_slice(&prepare(&config, &input).unwrap().body).unwrap();
    assert_eq!(body["model"], "bytedance-seed/seedream-5-0-flash");
    assert_eq!(body["provider"]["only"], json!(["seed"]));
    for name in ["quality", "background", "size", "stream"] {
        assert!(body.get(name).is_none());
    }
    input.options.partial_images = Some(1);
    assert!(prepare(&config, &input).is_err());
    input.options.partial_images = None;
    input.options.quality = Some("high".into());
    assert!(prepare(&config, &input).is_err());
}

#[test]
fn seedream_dedicated_api_inline_contract_needs_no_cdn_origin_or_format_selector() {
    let endpoint = json!({
        "provider_slug":"seed","provider_tag":"seed",
        "allowed_passthrough_parameters":[],"supports_streaming":false,
        "supported_parameters":{
            "resolution":{"type":"enum","values":["1K","2K"]},
            "input_references":{"type":"range","min":0,"max":14}
        }
    });
    let mut config = config(ImageApi::Openrouter);
    config.base_url = "https://openrouter.ai/api/v1".into();
    config.model = "bytedance-seed/seedream-5-0-flash".into();
    config.capabilities = Capabilities::from_openrouter_endpoint(&endpoint).unwrap();
    config.router_endpoint = Some(RouterEndpoint::from_catalog(&endpoint).unwrap());
    config.asset_hosts.clear();
    let request = prepare(&config, &input()).unwrap();
    assert_eq!(request.endpoint, "https://openrouter.ai/api/v1/images");
    let body: Value = serde_json::from_slice(&request.body).unwrap();
    assert_eq!(body["provider"]["only"], json!(["seed"]));
    assert!(body.get("output_format").is_none());
    assert_eq!(request.requested_format, None);
    // The dedicated Image API documents inline bytes as its response contract,
    // not a response_format parameter inherited from legacy OpenAI DALL-E.
    assert!(body.get("response_format").is_none());
    let expected = raster();
    let known = transport::parse_buffered(
        json!({
            "created":123,"data":[{"b64_json":expected.encoded(),"media_type":"image/png"}],
            "usage":{"cost":0.018}
        }),
        None,
        None,
    )
    .unwrap();
    assert!(matches!(&known.source, ImageSource::Inline { .. }));
    let actual = futures::executor::block_on(materialize(&config, &known)).unwrap();
    assert_eq!(actual.sha256(), expected.sha256());
    assert_eq!(known.reported_cost().unwrap().to_string(), "0.018");
}

#[test]
fn service_default_jpeg_keeps_completed_identity_cost_and_real_format() {
    let endpoint = json!({"provider_slug":"seed","provider_tag":"seed","allowed_passthrough_parameters":[],"supports_streaming":false,
        "supported_parameters":{"resolution":{"type":"enum","values":["1K","2K"]},"input_references":{"type":"range","min":0,"max":14}}});
    let mut config = config(ImageApi::Openrouter);
    config.base_url = "https://openrouter.ai/api/v1".into();
    config.model = "bytedance-seed/seedream-5-0-flash".into();
    config.capabilities = Capabilities::from_openrouter_endpoint(&endpoint).unwrap();
    config.router_endpoint = Some(RouterEndpoint::from_catalog(&endpoint).unwrap());
    let request = prepare(&config, &input()).unwrap();
    assert!(
        serde_json::from_slice::<Value>(&request.body)
            .unwrap()
            .get("output_format")
            .is_none()
    );
    assert_eq!(request.requested_format, None);
    let mut bytes = Cursor::new(Vec::new());
    image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(2, 3, image::Rgb([7, 11, 13])))
        .write_to(&mut bytes, CodecFormat::Jpeg)
        .unwrap();
    let jpeg = Raster::from_bytes(bytes.get_ref()).unwrap();
    let wire = json!({"id":"img_seed_known","created":123,"data":[{"b64_json":jpeg.encoded(),"media_type":"image/jpeg"}],"usage":{"cost":0.018}});
    let known = transport::parse_buffered(wire.clone(), request.requested_format, None).unwrap();
    assert_eq!(known.output_format, Some(Format::Jpeg));
    assert_eq!(known.provider_request_id.as_deref(), Some("img_seed_known"));
    assert_eq!(known.reported_cost().unwrap().to_string(), "0.018");
    let actual = futures::executor::block_on(materialize(&config, &known)).unwrap();
    assert_eq!(actual.format(), Format::Jpeg);
    assert_eq!(actual.bytes(), jpeg.bytes());
    assert!(transport::parse_buffered(wire.clone(), Some(Format::Png), None).is_err());
    let mut wrong_mime = wire;
    wrong_mime["data"][0]["media_type"] = json!("image/png");
    assert!(transport::parse_buffered(wrong_mime, None, None).is_err());
    let mut explicit = input();
    explicit.options.output_format = Some(Format::Png);
    assert!(
        prepare(&config, &explicit).is_err(),
        "unsupported format must reject before payment"
    );
    let remote = transport::parse_buffered(json!({"data":[{"url":"https://cdn.example.com/result.jpg","media_type":"image/jpeg"}],"usage":{"cost":0.018}}), None, None).unwrap();
    assert_eq!(
        remote.output_format, None,
        "a service-default remote result has no fabricated PNG metadata"
    );
}

proptest! {
    #[test]
    fn untrusted_size_and_base64_inputs_do_not_panic(value in ".{0,256}") {
        let _=dimensions(&value);
        let _=Raster::from_base64(&value);
    }
}
