//! Bounded document conversion through the native platform service. Image/OCR
//! inference is not a fallback for an empty document text layer.

use js_sys::{Array, Function, Promise, Reflect, Uint8Array};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{borrow::Cow, io::Cursor};
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;
use worker::{Env, Error, Result};

use crate::{
    file_index::IngestJob,
    file_vector::{Allocation, VectorServiceProfile},
    resources::ResourceMeter,
    space::OwnedFile,
};

const RICH_LIMIT: usize = 4 * 1024 * 1024;
const TEXT_LIMIT: usize = 1024 * 1024;
const EXPANDED_LIMIT: u64 = 16 * 1024 * 1024;

fn fail(code: &str) -> Error {
    Error::RustError(code.into())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Pdf,
    Docx,
    Xlsx,
}

enum ConversionBilling<'a> {
    FreeNativeDocumentText,
    PlatformAllocation(&'a Allocation),
}

impl Kind {
    fn billing(self, allocation: Option<&Allocation>) -> ConversionBilling<'_> {
        // This capability is the native document-to-text parser, not image
        // description/OCR or an unknown converter with a missing price.
        // https://developers.cloudflare.com/workers-ai/features/markdown-conversion/#pricing
        // https://developers.cloudflare.com/workers-ai/features/markdown-conversion/supported-formats/
        match (self, allocation) {
            (_, Some(allocation)) => ConversionBilling::PlatformAllocation(allocation),
            (Self::Pdf | Self::Docx | Self::Xlsx, None) => {
                ConversionBilling::FreeNativeDocumentText
            }
        }
    }
    fn name(self) -> &'static str {
        match self {
            Self::Pdf => "source.pdf",
            Self::Docx => "source.docx",
            Self::Xlsx => "source.xlsx",
        }
    }
    fn mime(self) -> &'static str {
        match self {
            Self::Pdf => "application/pdf",
            Self::Docx => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
            Self::Xlsx => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        }
    }
}

fn u16_at(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([bytes[at], bytes[at + 1]])
}
fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

fn bounded_zip(bytes: &[u8]) -> Result<Kind> {
    // Cap the allocation advertised by the end record before constructing the
    // mature ZIP parser. ZIP64 is unnecessary for this four-MiB input profile.
    let end = bytes
        .len()
        .checked_sub(22)
        .ok_or_else(|| fail("file_search_document_invalid"))?;
    let start = bytes.len().saturating_sub(65557);
    let eocd = (start..=end)
        .rev()
        .find(|&at| {
            bytes[at..].starts_with(b"PK\x05\x06")
                && at + 22 + usize::from(u16_at(bytes, at + 20)) == bytes.len()
        })
        .ok_or_else(|| fail("file_search_document_invalid"))?;
    if u16_at(bytes, eocd + 4) != 0
        || u16_at(bytes, eocd + 6) != 0
        || u16_at(bytes, eocd + 8) != u16_at(bytes, eocd + 10)
        || u16_at(bytes, eocd + 10) > 2048
        || u32_at(bytes, eocd + 12) > 1024 * 1024
        || u64::from(u32_at(bytes, eocd + 16)) + u64::from(u32_at(bytes, eocd + 12)) > eocd as u64
        || (eocd >= 20 && bytes[eocd - 20..].starts_with(b"PK\x06\x07"))
    {
        return Err(fail("file_search_document_archive_limit"));
    }
    let mut zip = zip::ZipArchive::with_config(
        zip::read::Config {
            archive_offset: zip::read::ArchiveOffset::Known(0),
        },
        Cursor::new(bytes),
    )
    .map_err(|_| fail("file_search_document_invalid"))?;
    let mut expanded = 0u64;
    let mut word = false;
    let mut sheet = false;
    let mut types = false;
    for index in 0..zip.len() {
        let entry = zip
            .by_index_raw(index)
            .map_err(|_| fail("file_search_document_invalid"))?;
        expanded = expanded
            .checked_add(entry.size())
            .ok_or_else(|| fail("file_search_document_archive_limit"))?;
        let name = entry.name().to_ascii_lowercase();
        if expanded > EXPANDED_LIMIT
            || entry.encrypted()
            || entry.is_symlink()
            || entry.enclosed_name().is_none()
            || name.len() > 1024
            || name.contains(['\\', ':'])
            || name.ends_with("vbaproject.bin")
            || !matches!(
                entry.compression(),
                zip::CompressionMethod::STORE | zip::CompressionMethod::DEFLATE
            )
        {
            return Err(fail("file_search_document_archive_limit"));
        }
        word |= name == "word/document.xml";
        sheet |= name == "xl/workbook.xml";
        types |= name == "[content_types].xml";
    }
    match (types, word, sheet) {
        (true, true, false) => Ok(Kind::Docx),
        (true, false, true) => Ok(Kind::Xlsx),
        _ => Err(fail("unsupported_or_empty_document")),
    }
}

fn kind(bytes: &[u8]) -> Result<Option<Kind>> {
    if bytes.starts_with(b"%PDF-") {
        if bytes.len() > RICH_LIMIT {
            return Err(fail("file_extraction_byte_limit"));
        }
        return Ok(Some(Kind::Pdf));
    }
    if bytes.starts_with(b"PK\x03\x04") {
        if bytes.len() > RICH_LIMIT {
            return Err(fail("file_extraction_byte_limit"));
        }
        return bounded_zip(bytes).map(Some);
    }
    Ok(None)
}

struct PreparedConversion {
    ai: worker::Ai,
    method: Function,
    documents: Array,
    options: JsValue,
    kind: Kind,
}

fn prepare(
    env: &Env,
    profile: &VectorServiceProfile,
    kind: Kind,
    bytes: &[u8],
) -> Result<PreparedConversion> {
    prepare_binding(env, &profile.ai_binding, kind, bytes)
}

fn prepare_binding(
    env: &Env,
    binding: &str,
    kind: Kind,
    bytes: &[u8],
) -> Result<PreparedConversion> {
    let ai = env.ai(binding)?;
    let method = Reflect::get(ai.as_ref(), &JsValue::from_str("toMarkdown"))
        .map_err(|_| fail("file_search_document_service_unavailable"))?
        .dyn_into::<Function>()
        .map_err(|_| fail("file_search_document_service_unavailable"))?;
    let parts = Array::of1(&Uint8Array::from(bytes));
    let blob = web_sys::Blob::new_with_u8_array_sequence(&parts)
        .map_err(|_| fail("file_search_document_invalid"))?;
    let document = js_sys::Object::new();
    Reflect::set(&document, &"name".into(), &kind.name().into())
        .map_err(|_| fail("file_search_document_invalid"))?;
    Reflect::set(&document, &"blob".into(), blob.as_ref())
        .map_err(|_| fail("file_search_document_invalid"))?;
    let options =
        json!({"conversionOptions":{"output":{"format":"text"},"pdf":{"metadata":false}}})
            .serialize(&serde_wasm_bindgen::Serializer::json_compatible())?;
    Ok(PreparedConversion {
        ai,
        method,
        documents: Array::of1(&document),
        options,
        kind,
    })
}

async fn convert(prepared: PreparedConversion) -> Result<String> {
    let promise = prepared
        .method
        .call2(prepared.ai.as_ref(), &prepared.documents, &prepared.options)
        .map_err(|_| fail("file_search_document_service_failed"))?
        .dyn_into::<Promise>()
        .map_err(|_| fail("file_search_document_service_failed"))?;
    let result = JsFuture::from(promise)
        .await
        .map_err(|_| fail("file_search_document_service_failed"))?;
    if !Array::is_array(&result) {
        return Err(fail("file_search_document_invalid"));
    }
    let result = Array::from(&result);
    if result.length() != 1 {
        return Err(fail("file_search_document_invalid"));
    }
    let result = result.get(0);
    let field = |name: &str| {
        Reflect::get(&result, &JsValue::from_str(name))
            .map_err(|_| fail("file_search_document_invalid"))
    };
    if field("format")?.as_string().as_deref() != Some("text")
        || field("mimetype")?.as_string().as_deref() != Some(prepared.kind.mime())
    {
        return Err(fail("file_search_document_invalid"));
    }
    let text = field("data")?;
    if !text.is_string() {
        return Err(fail("file_search_document_invalid"));
    }
    let js_text: js_sys::JsString = text.unchecked_into();
    if js_text.length() as usize > TEXT_LIMIT {
        return Err(fail("file_search_document_text_limit"));
    }
    let text = js_text
        .as_string()
        .ok_or_else(|| fail("file_search_document_invalid"))?;
    if text.len() > TEXT_LIMIT || text.contains('\0') || text.trim().is_empty() {
        return Err(fail("file_search_document_text_limit"));
    }
    Ok(text)
}

/// Responses attachments share the admitted native document-text parser. This
/// never invokes embedding, OCR, images, or an unpriced custom converter.
pub(crate) async fn attachment_text(env: &Env, bytes: &[u8]) -> Result<Option<String>> {
    let kind = match kind(bytes) {
        Ok(Some(kind)) => kind,
        Ok(None) => return Ok(None),
        Err(Error::RustError(code)) if code == "unsupported_or_empty_document" => return Ok(None),
        Err(error) => return Err(error),
    };
    convert(prepare_binding(env, "AI", kind, bytes)?)
        .await
        .map(Some)
}

#[derive(Deserialize)]
struct Extraction {
    status: String,
    text: Option<String>,
    error_code: Option<String>,
}

pub(super) async fn text<'a>(
    env: &Env,
    profile: &VectorServiceProfile,
    job: &IngestJob,
    source: &OwnedFile,
    bytes: &'a [u8],
    meter: &ResourceMeter<'_>,
    event: &str,
) -> Result<Cow<'a, str>> {
    let Some(kind) = kind(bytes)? else {
        if source.content_type.starts_with("image/") {
            return Err(fail("unsupported_or_empty_document"));
        }
        let text = std::str::from_utf8(bytes).map_err(|_| fail("unsupported_or_empty_document"))?;
        if text.contains('\0') || text.trim().is_empty() {
            return Err(fail("unsupported_or_empty_document"));
        }
        return Ok(Cow::Borrowed(text));
    };
    let allocation = match kind.billing(profile.conversion_allocation.as_ref()) {
        ConversionBilling::FreeNativeDocumentText => None,
        ConversionBilling::PlatformAllocation(allocation) => Some(allocation),
    };
    meter.require_d1(&format!("{event}:read")).await?;
    let result=env.d1("DB")?.prepare("SELECT status,text,error_code FROM file_search_extractions WHERE account_id=?1 AND vector_store_id=?2 AND file_id=?3 AND generation=?4 AND source_sha256=?5")
        .bind(&[job.account_id.as_str().into(),job.vector_store_id.as_str().into(),job.file_id.as_str().into(),job.generation.as_str().into(),job.file_sha256.as_str().into()])?.all().await?;
    meter.record_d1(&format!("{event}:read"), &result).await?;
    if let Some(saved) = result.results::<Extraction>()?.into_iter().next() {
        return match (saved.status.as_str(), saved.text) {
            ("completed", Some(text)) => Ok(Cow::Owned(text)),
            _ => Err(fail(
                saved
                    .error_code
                    .as_deref()
                    .unwrap_or("file_search_document_outcome_unknown"),
            )),
        };
    }
    let prepared = prepare(env, profile, kind, bytes)?;
    let funded = if let Some(allocation) = allocation {
        Some(
            crate::file_index::funding::fund(
                env,
                &job.into(),
                "convert",
                Some((allocation, bytes.len() as u64, 1)),
                crate::file_index::funding::io_budget(env)?,
            )
            .await?,
        )
    } else {
        None
    };
    let paid = funded
        .as_ref()
        .map(|funding| funding.meter(env, &job.account_id))
        .transpose()?;
    let effect = "provider";
    meter.require_d1(&format!("{event}:intent")).await?;
    let result=env.d1("DB")?.prepare("INSERT INTO file_search_extractions(account_id,vector_store_id,file_id,generation,source_sha256,status,created_at_ms) SELECT ?1,?2,?3,?4,?5,'dispatched',CAST(?7 AS INTEGER) WHERE EXISTS(SELECT 1 FROM vector_store_files vf JOIN space_files sf ON sf.id=vf.file_id AND sf.account_id=vf.account_id WHERE vf.account_id=?1 AND vf.vector_store_id=?2 AND vf.file_id=?3 AND vf.generation=?4 AND vf.lease_id=?6 AND vf.status='in_progress' AND vf.lease_until>CAST(?7 AS INTEGER) AND sf.status='processed' AND sf.sha256=?5) ON CONFLICT(vector_store_id,file_id,generation) DO NOTHING RETURNING file_id")
        .bind(&[job.account_id.as_str().into(),job.vector_store_id.as_str().into(),job.file_id.as_str().into(),job.generation.as_str().into(),job.file_sha256.as_str().into(),job.lease_id.as_str().into(),crate::util::now_ms().to_string().into()])?.all().await?;
    meter.record_d1(&format!("{event}:intent"), &result).await?;
    if result.results::<Value>()?.is_empty() {
        return Err(fail("file_search_extraction_lease_lost"));
    }
    if let Some((allocation, paid)) = allocation.zip(paid.as_ref()) {
        paid.require_observation(effect, allocation.resource.clone(), &allocation.unit)
            .await?;
    }
    let outcome = convert(prepared).await;
    if matches!(&outcome,Err(Error::RustError(code)) if code=="file_search_document_service_failed")
    {
        return outcome.map(Cow::Owned);
    }
    if let Some((allocation, paid)) = allocation.zip(paid.as_ref()) {
        paid.record_observation(
            effect,
            allocation.resource.clone(),
            &allocation.unit,
            bytes.len() as u64,
        )
        .await?;
    }
    let (state, text, error) = match &outcome {
        Ok(text) => ("completed", Some(text.as_str()), None),
        Err(Error::RustError(code)) => ("failed", None, Some(code.as_str())),
        Err(_) => ("failed", None, Some("file_search_document_invalid")),
    };
    meter.require_d1(&format!("{event}:result")).await?;
    let result=env.d1("DB")?.prepare("UPDATE file_search_extractions SET status=?7,text=?8,error_code=?9 WHERE account_id=?1 AND vector_store_id=?2 AND file_id=?3 AND generation=?4 AND status='dispatched' AND EXISTS(SELECT 1 FROM vector_store_files vf WHERE vf.account_id=?1 AND vf.vector_store_id=?2 AND vf.file_id=?3 AND vf.generation=?4 AND vf.lease_id=?5 AND vf.status='in_progress' AND vf.lease_until>CAST(?6 AS INTEGER)) RETURNING file_id")
        .bind(&[job.account_id.as_str().into(),job.vector_store_id.as_str().into(),job.file_id.as_str().into(),job.generation.as_str().into(),job.lease_id.as_str().into(),crate::util::now_ms().to_string().into(),state.into(),text.map(JsValue::from_str).unwrap_or(JsValue::NULL),error.map(JsValue::from_str).unwrap_or(JsValue::NULL)])?.all().await?;
    meter.record_d1(&format!("{event}:result"), &result).await?;
    if result.results::<Value>()?.is_empty() {
        return Err(fail("file_search_extraction_lease_lost"));
    }
    if let Some(paid) = paid {
        paid.settle_run().await?;
    }
    outcome.map(Cow::Owned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn archive(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        for (name, data) in entries {
            writer
                .start_file(
                    *name,
                    zip::write::SimpleFileOptions::default()
                        .compression_method(zip::CompressionMethod::STORE),
                )
                .unwrap();
            writer.write_all(data).unwrap();
        }
        writer.finish().unwrap().into_inner()
    }

    #[test]
    fn native_free_text_capability_never_hides_configured_or_unknown_prices() {
        for kind in [Kind::Pdf, Kind::Docx, Kind::Xlsx] {
            assert!(matches!(
                kind.billing(None),
                ConversionBilling::FreeNativeDocumentText
            ));
            let explicit = Allocation {
                resource: agents_auth::billing::Resource::Worker,
                unit: "conversion_input_bytes".into(),
            };
            let ConversionBilling::PlatformAllocation(allocation) = kind.billing(Some(&explicit))
            else {
                panic!("A configured allocation cannot silently become free");
            };
            let prices=crate::resources::ResourcePricing::parse(r#"{"version":"fixture","tariffs":[{"resource":"worker","unit":"wall_ms","microusd_numerator":1,"quantity_denominator":1000,"price_version":"fixture"}]}"#).unwrap();
            assert!(
                prices
                    .tariff(&allocation.resource, &allocation.unit)
                    .is_err()
            );
        }
        assert!(kind(b"\x89PNG\r\n\x1a\n").unwrap().is_none());
        assert!(kind(b"PK\x03\x04unknown").is_err());
    }

    #[test]
    fn document_formats_require_actual_container_signatures() {
        assert_eq!(kind("plain UTF-8 snow 雪".as_bytes()).unwrap(), None);
        assert_eq!(kind(b"%PDF-1.7\ntext").unwrap(), Some(Kind::Pdf));
        let word = archive(&[
            ("[Content_Types].xml", b"types"),
            ("word/document.xml", b"text"),
        ]);
        assert_eq!(kind(&word).unwrap(), Some(Kind::Docx));
        let sheet = archive(&[
            ("[Content_Types].xml", b"types"),
            ("xl/workbook.xml", b"text"),
        ]);
        assert_eq!(kind(&sheet).unwrap(), Some(Kind::Xlsx));
        assert!(kind(&archive(&[("unrelated.txt", b"no")])).is_err());
    }

    #[test]
    fn document_archive_rejects_unsafe_paths_and_macros() {
        for name in [
            "../secret",
            "folder/../../../secret",
            "C:/secret",
            "word\\secret",
            "word/vbaProject.bin",
        ] {
            let bytes = archive(&[
                ("[Content_Types].xml", b"types"),
                ("word/document.xml", b"text"),
                (name, b"bad"),
            ]);
            assert!(kind(&bytes).is_err(), "{name}");
        }
    }

    #[test]
    fn document_archive_bounds_advertised_allocation_and_expansion() {
        let mut bytes = archive(&[
            ("[Content_Types].xml", b"types"),
            ("word/document.xml", b"text"),
        ]);
        let eocd = bytes.len() - 22;
        bytes[eocd + 10..eocd + 12].copy_from_slice(&2049u16.to_le_bytes());
        assert!(kind(&bytes).is_err());
        let mut expanded = archive(&[
            ("[Content_Types].xml", b"types"),
            ("word/document.xml", b"text"),
        ]);
        let central = expanded
            .windows(4)
            .rposition(|header| header == b"PK\x01\x02")
            .unwrap();
        expanded[central + 24..central + 28]
            .copy_from_slice(&(EXPANDED_LIMIT as u32 + 1).to_le_bytes());
        assert!(kind(&expanded).is_err());
        assert!(kind(&vec![b'P'; RICH_LIMIT + 1]).unwrap().is_none());
        let mut pdf = b"%PDF-1.7".to_vec();
        pdf.resize(RICH_LIMIT + 1, b' ');
        assert!(kind(&pdf).is_err());
    }
}
