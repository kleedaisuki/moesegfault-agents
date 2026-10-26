//! One configured retrieval service, shared by HTTP, maintenance, and the harness.

use std::collections::BTreeMap;

use serde::Deserialize;
use serde_json::Value;
use worker::{Env, Error, Result};

use crate::{
    file_search::{FileSearchOptions, Hit},
    file_vector::VectorServiceProfile,
    resources::ResourceMeter,
};

pub(crate) fn profile(env: &Env) -> Result<VectorServiceProfile> {
    let encoded = env
        .var("FILE_SEARCH_PROFILE_JSON")
        .map_err(|_| Error::RustError("file_search_service_not_configured".into()))?
        .to_string();
    let profile: VectorServiceProfile = serde_json::from_str(&encoded)
        .map_err(|_| Error::RustError("invalid_file_search_service_profile".into()))?;
    profile.validate()?;
    profile.validate_bindings(env)?;
    Ok(profile)
}

pub(crate) fn preflight(profile: &VectorServiceProfile, meter: &ResourceMeter<'_>) -> Result<()> {
    meter.preflight(false)?;
    meter.require_tariff(&agents_auth::billing::Resource::R2Operation, "class_b")?;
    crate::file_index::storage::preflight(meter)?;
    for allocation in [
        &profile.embedding_allocation,
        &profile.query_allocation,
        &profile.upsert_allocation,
    ] {
        meter.require_tariff(&allocation.resource, &allocation.unit)?;
    }
    Ok(())
}

#[derive(Deserialize)]
struct StoreProfile {
    service_profile_json: Option<String>,
}

pub(crate) async fn store_profile(
    env: &Env,
    owner: &str,
    store: &str,
    meter: &ResourceMeter<'_>,
    event: &str,
    pin: bool,
) -> Result<VectorServiceProfile> {
    crate::file_index::storage::require_paid_store(env, owner, store).await?;
    let read = format!("{event}:profile");
    meter.require_d1(&read).await?;
    let result=env.d1("DB")?.prepare("SELECT service_profile_json FROM vector_stores WHERE id=?1 AND account_id=?2 AND status='completed' AND (expires_after_days IS NULL OR last_active_at+expires_after_days*86400000>unixepoch()*1000)")
        .bind(&[store.into(),owner.into()])?.all().await?;
    meter.record_d1(&read, &result).await?;
    let row = result
        .results::<StoreProfile>()?
        .into_iter()
        .next()
        .ok_or_else(|| Error::RustError("vector_store_not_found".into()))?;
    if let Some(encoded) = row.service_profile_json {
        let profile: VectorServiceProfile = serde_json::from_str(&encoded)?;
        profile.validate()?;
        profile.validate_bindings(env)?;
        preflight(&profile, meter)?;
        return Ok(profile);
    }
    let profile = profile(env)?;
    preflight(&profile, meter)?;
    if !pin {
        return Ok(profile);
    }
    let write = format!("{event}:pin");
    meter.require_d1(&write).await?;
    let result=env.d1("DB")?.prepare("UPDATE vector_stores SET service_profile_json=COALESCE(service_profile_json,?3) WHERE id=?1 AND account_id=?2 AND status='completed' RETURNING service_profile_json")
        .bind(&[store.into(),owner.into(),serde_json::to_string(&profile)?.into()])?.all().await?;
    meter.record_d1(&write, &result).await?;
    let encoded = result
        .results::<StoreProfile>()?
        .into_iter()
        .next()
        .and_then(|row| row.service_profile_json)
        .ok_or_else(|| Error::RustError("vector_store_not_found".into()))?;
    let profile: VectorServiceProfile = serde_json::from_str(&encoded)?;
    profile.validate()?;
    profile.validate_bindings(env)?;
    preflight(&profile, meter)?;
    Ok(profile)
}

pub(crate) async fn search_queries(
    env: &Env,
    owner: &str,
    queries: &[String],
    options: &FileSearchOptions,
    meter: &ResourceMeter<'_>,
    event: &str,
) -> Result<Vec<Hit>> {
    if queries.is_empty()
        || queries.len() > 3
        || queries
            .iter()
            .any(|q| q.trim().is_empty() || q.len() > 8192)
    {
        return Err(Error::RustError("invalid_file_search_queries".into()));
    }
    options.validate()?;
    let mut selected: Option<(String, VectorServiceProfile)> = None;
    for (ordinal, store) in options.vector_store_ids.iter().enumerate() {
        let profile = store_profile(
            env,
            owner,
            store,
            meter,
            &format!("{event}:s{ordinal}"),
            false,
        )
        .await?;
        let encoded = serde_json::to_string(&profile)?;
        if selected.as_ref().is_some_and(|(old, _)| old != &encoded) {
            return Err(Error::RustError(
                "file_search_store_requires_reindex".into(),
            ));
        }
        selected = Some((encoded, profile));
    }
    let profile = selected
        .ok_or_else(|| Error::RustError("invalid_file_search_options".into()))?
        .1;
    let mut hits = BTreeMap::<String, Hit>::new();
    for (ordinal, query) in queries.iter().enumerate() {
        for hit in crate::file_vector::search(
            env,
            &profile,
            owner,
            query,
            options,
            meter,
            &format!("{event}:q{ordinal}"),
        )
        .await?
        {
            if hits
                .get(&hit.chunk_id)
                .is_none_or(|old| old.score < hit.score)
            {
                hits.insert(hit.chunk_id.clone(), hit);
            }
        }
    }
    let mut hits = hits.into_values().collect::<Vec<_>>();
    hits.sort_by(|left, right| {
        right
            .score
            .total_cmp(&left.score)
            .then(left.chunk_id.cmp(&right.chunk_id))
    });
    hits.truncate(options.max_num_results);
    // A later query can be slow: recheck every earlier query's result at publication.
    let ids = hits
        .iter()
        .map(|hit| hit.chunk_id.clone())
        .collect::<Vec<_>>();
    let mut live = crate::file_index::live_vector_candidates(
        env,
        owner,
        &ids,
        options,
        meter,
        &format!("{event}:final"),
    )
    .await?
    .into_iter()
    .map(|hit| (hit.chunk_id.clone(), hit))
    .collect::<BTreeMap<_, _>>();
    let activity = format!("{event}:activity");
    meter.require_d1(&activity).await?;
    let result=env.d1("DB")?.prepare("UPDATE vector_stores SET last_active_at=CAST(?3 AS INTEGER) WHERE account_id=?1 AND id IN (SELECT value FROM json_each(?2)) AND status='completed' AND (expires_after_days IS NULL OR last_active_at+expires_after_days*86400000>unixepoch()*1000)")
        .bind(&[owner.into(),serde_json::to_string(&options.vector_store_ids)?.into(),crate::util::now_ms().to_string().into()])?.run().await?;
    meter.record_d1(&activity, &result).await?;
    Ok(hits
        .into_iter()
        .filter_map(|hit| {
            live.remove(&hit.chunk_id).map(|mut current| {
                current.score = hit.score;
                current
            })
        })
        .collect())
}

/// Tool evidence contains exact owned IDs and filenames. Final answer annotation
/// offsets are anchored by the harness, never copied from retrieval snippets.
pub(crate) fn evidence(hits: &[Hit]) -> Vec<Value> {
    hits.iter()
        .map(|hit| {
            let mut result = hit.tool_result();
            result["citation_marker"] = serde_json::json!(format!("⟦file:{}⟧", hit.file_id));
            result
        })
        .collect()
}

/// Only evidence produced by this response's owned search can create citations.
/// Markers remain in the text, so already-emitted deltas never need rewriting.
pub(crate) fn annotate_part(part: &mut Value, outputs: &[Value]) -> Vec<Value> {
    if part["type"] != "output_text" {
        return vec![];
    }
    let Some(text) = part["text"].as_str() else {
        return vec![];
    };
    let mut sources = BTreeMap::<String, String>::new();
    for result in outputs
        .iter()
        .filter(|item| item["type"] == "file_search_call" && item["status"] == "completed")
        .flat_map(|item| item["results"].as_array().into_iter().flatten())
    {
        if let (Some(id), Some(name)) = (result["file_id"].as_str(), result["filename"].as_str()) {
            sources.insert(id.into(), name.into());
        }
    }
    let mut added = Vec::new();
    let existing = part["annotations"].as_array().cloned().unwrap_or_default();
    // Repeated model markers cannot amplify the retained annotation set.
    for (id, name) in sources {
        let marker = format!("⟦file:{id}⟧");
        if let Some(position) = text.find(&marker) {
            let index = text[..position].chars().count();
            if !existing.iter().any(|annotation| {
                annotation["type"] == "file_citation"
                    && annotation["file_id"] == id
                    && annotation["index"].as_u64() == Some(index as u64)
            }) {
                added.push(serde_json::json!({"type":"file_citation","file_id":id,"filename":name,"index":index}));
            }
        }
    }
    added.sort_by_key(|annotation| annotation["index"].as_u64().unwrap_or(0));
    if !added.is_empty() {
        let mut annotations = existing;
        annotations.extend(added.iter().cloned());
        part["annotations"] = Value::Array(annotations);
    }
    added
}

pub(crate) fn annotate_message(item: &mut Value, outputs: &[Value]) {
    if item["type"] != "message" {
        return;
    }
    if let Some(parts) = item["content"].as_array_mut() {
        for part in parts {
            annotate_part(part, outputs);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn file_citations_anchor_actual_unicode_text_and_owned_evidence_once() {
        let mut part = json!({"type":"output_text","text":"雪🦀 answer ⟦file:owned⟧. ⟦file:foreign⟧","annotations":[]});
        let evidence = vec![
            json!({"type":"file_search_call","status":"completed","results":[{"file_id":"owned","filename":"notes.txt"}]}),
        ];
        let before = part["text"].clone();
        let added = annotate_part(&mut part, &evidence);
        assert_eq!(added.len(), 1);
        assert_eq!(added[0]["index"], 10);
        assert_eq!(added[0]["file_id"], "owned");
        assert_eq!(part["text"], before);
        assert!(annotate_part(&mut part, &evidence).is_empty());
        assert_eq!(part["annotations"].as_array().unwrap().len(), 1);
    }
}
