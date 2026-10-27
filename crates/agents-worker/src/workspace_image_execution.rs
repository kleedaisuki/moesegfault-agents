//! Image bytes belong to an executing run until its terminal outbox transfers
//! exact slots to durable public, Conversation or bounded delivery holders.
use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use worker::{D1Result, Env, Result};

use super::{OwnedBlobRef, conversation_ref_prefix, fail, image_ref_id, text};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct ImageExecutionAsset {
    pub artifact_id: String,
    pub item_id: String,
    pub partial_index: Option<u8>,
    pub blob: OwnedBlobRef,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct ImageDelivery {
    pub id: String,
    pub expires_at_ms: u64,
}
/// A deferred transfer still returns its known D1 work for exact accounting.
/// The caller persists/bills these observations before retrying a retained hold.
pub(crate) struct ImageTransfer {
    pub results: Vec<D1Result>,
    pub complete: bool,
}
#[derive(Deserialize)]
struct ClosedRun {
    generation: String,
}
#[derive(Deserialize)]
struct Source {
    ref_id: String,
    artifact_id: String,
    execution_generation: String,
    #[serde(flatten)]
    blob: OwnedBlobRef,
}

fn component(value: &str) -> Result<()> {
    if value.is_empty() || value.len() > 256 {
        return Err(fail("invalid_image_execution_scope"));
    }
    Ok(())
}
fn execution_ref_id(session: &str, run: &str, execution: &str, artifact: &str) -> Result<String> {
    for value in [session, run] {
        component(value)?;
    }
    for value in [execution, artifact] {
        if value.is_empty() || value.len() > 512 {
            return Err(fail("invalid_image_execution_scope"));
        }
    }
    let scope = Sha256::digest(serde_json::to_vec(&(session, run, execution))?);
    let slot = Sha256::digest(serde_json::to_vec(artifact)?);
    Ok(format!("image-execution:{scope:x}:{slot:x}"))
}

pub(crate) async fn bind_image_execution(
    env: &Env,
    owner: &str,
    session: &str,
    run: &str,
    execution: &str,
    artifact: &str,
    blob: &OwnedBlobRef,
) -> Result<ImageTransfer> {
    blob.validate(owner)?;
    let id = execution_ref_id(session, run, execution, artifact)?;
    let result=env.d1("DB")?.prepare("INSERT INTO workspace_blob_refs(account_id,kind,ref_id,object_key,session_id,response_id,execution_id,artifact_id,execution_generation) SELECT ?1,'image-execution',?2,?3,?4,?5,?6,?7,COALESCE(r.execution_generation,'legacy') FROM usage_reservations r JOIN workspace_blobs b ON b.object_key=?3 AND b.account_id=?1 AND b.kind='image' AND b.status='ready' AND b.sha256=?8 AND b.archive_bytes=CAST(?9 AS INTEGER) AND b.content_type=?10 WHERE r.id=?5 AND r.account_id=?1 AND r.session_id=?4 AND r.idempotency_key LIKE 'run:%' AND r.state='reserved' AND r.execution_finished_at IS NULL AND (r.execution_generation IS NULL OR EXISTS(SELECT 1 FROM execution_leases l WHERE l.reservation_id=r.id AND l.account_id=r.account_id AND l.generation=r.execution_generation AND l.state='active')) ON CONFLICT(account_id,kind,ref_id) DO UPDATE SET object_key=excluded.object_key")
        .bind(&[text(owner),text(&id),text(&blob.object_key),text(session),text(run),text(execution),text(artifact),text(&blob.sha256),text(&blob.bytes.to_string()),text(&blob.content_type)])?.run().await?;
    let changes = result
        .meta()?
        .and_then(|meta| meta.changes)
        .ok_or_else(|| fail("d1_usage_metadata_missing"))?;
    let complete = changes != 0;
    Ok(ImageTransfer {
        results: vec![result],
        complete,
    })
}

fn conversation_image_ref_id(conversation: &str, item: &str) -> Result<String> {
    let prefix = conversation_ref_prefix(conversation)?;
    component(item)?;
    let digest = Sha256::digest(serde_json::to_vec(item)?);
    Ok(format!("{prefix}image:{digest:x}"))
}

/// The Conversation actor calls this under its item gate before publication.
pub(crate) async fn bind_conversation_image(
    env: &Env,
    owner: &str,
    conversation: &str,
    item: &str,
    blob: &OwnedBlobRef,
) -> Result<Vec<D1Result>> {
    blob.validate(owner)?;
    let id = conversation_image_ref_id(conversation, item)?;
    let result=env.d1("DB")?.prepare("INSERT INTO workspace_blob_refs(account_id,kind,ref_id,object_key,artifact_id) SELECT ?1,'conversation-artifact',?2,?3,?4 WHERE EXISTS(SELECT 1 FROM workspace_blobs WHERE object_key=?3 AND account_id=?1 AND kind='image' AND status='ready' AND sha256=?5 AND archive_bytes=CAST(?6 AS INTEGER) AND content_type=?7) ON CONFLICT(account_id,kind,ref_id) DO UPDATE SET object_key=excluded.object_key")
        .bind(&[text(owner),text(&id),text(&blob.object_key),text(item),text(&blob.sha256),text(&blob.bytes.to_string()),text(&blob.content_type)])?.run().await?;
    if result.meta()?.and_then(|meta| meta.changes).unwrap_or(0) == 0 {
        return Err(fail("workspace_blob_identity_conflict"));
    }
    Ok(vec![result])
}
/// Only a proved never-published candidate may be discarded without a fence.
pub(crate) async fn discard_unpublished_conversation_image(
    env: &Env,
    owner: &str,
    conversation: &str,
    item: &str,
) -> Result<Vec<D1Result>> {
    let id = conversation_image_ref_id(conversation, item)?;
    let result=env.d1("DB")?.prepare("DELETE FROM workspace_blob_refs WHERE account_id=? AND kind='conversation-artifact' AND ref_id=? AND artifact_id=?")
        .bind(&[text(owner),text(&id),text(item)])?.run().await?;
    Ok(vec![result])
}
/// The actor must durably delete the canonical item before installing this fence.
pub(crate) async fn release_conversation_image(
    env: &Env,
    owner: &str,
    conversation: &str,
    item: &str,
) -> Result<Vec<D1Result>> {
    let id = conversation_image_ref_id(conversation, item)?;
    let db = env.d1("DB")?;
    let mark=db.prepare("INSERT INTO conversation_image_tombstones(account_id,conversation_id,item_id) VALUES(?,?,?) ON CONFLICT(account_id,conversation_id,item_id) DO NOTHING")
        .bind(&[text(owner),text(conversation),text(item)])?;
    let remove=db.prepare("DELETE FROM workspace_blob_refs WHERE account_id=? AND kind='conversation-artifact' AND ref_id=? AND artifact_id=?")
        .bind(&[text(owner),text(&id),text(item)])?;
    db.batch(vec![mark, remove]).await
}

fn validate_assets<'a>(
    owner: &str,
    generation: &str,
    assets: &'a [ImageExecutionAsset],
    sources: &'a [Source],
) -> Result<Vec<(&'a ImageExecutionAsset, &'a Source)>> {
    if assets.len() > 4 || sources.len() > 4 {
        return Err(fail("image_execution_asset_limit"));
    }
    let source: BTreeMap<&str, &Source> = sources
        .iter()
        .map(|row| (row.artifact_id.as_str(), row))
        .collect();
    let mut seen = BTreeSet::new();
    let mut mapped = Vec::with_capacity(assets.len());
    for asset in assets {
        component(&asset.item_id)?;
        asset.blob.validate(owner)?;
        let expected = match asset.partial_index {
            None => format!("{}:result", asset.item_id),
            Some(index) if index < 3 => format!("{}:partial:{index}", asset.item_id),
            _ => return Err(fail("image_execution_slot_conflict")),
        };
        if asset.artifact_id != expected {
            return Err(fail("image_execution_slot_conflict"));
        }
        let Some(row) = source.get(asset.artifact_id.as_str()) else {
            return Err(fail("image_execution_unknown_slot"));
        };
        if !seen.insert(&asset.artifact_id)
            || row.blob != asset.blob
            || row.execution_generation != generation
        {
            return Err(fail("image_execution_slot_conflict"));
        }
        mapped.push((asset, *row));
    }
    Ok(mapped)
}

/// The caller durably maps its canonical output before calling this function.
/// Empty sources are a completed retry, never permission to recreate holders.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn transfer_image_execution(
    env: &Env,
    owner: &str,
    session: &str,
    run: &str,
    execution: &str,
    response: &str,
    assets: &[ImageExecutionAsset],
    store_response: bool,
    conversation: Option<&str>,
    delivery: Option<&ImageDelivery>,
) -> Result<ImageTransfer> {
    for value in [session, run, response] {
        component(value)?;
    }
    if execution.is_empty() || execution.len() > 512 {
        return Err(fail("invalid_image_execution_scope"));
    }
    let db = env.d1("DB")?;
    let run_result=db.prepare("SELECT COALESCE(execution_generation,'legacy') AS generation FROM usage_reservations WHERE id=? AND account_id=? AND session_id=? AND idempotency_key LIKE 'run:%' AND execution_finished_at IS NOT NULL")
        .bind(&[text(run),text(owner),text(session)])?.all().await?;
    let closed = run_result
        .results::<ClosedRun>()?
        .into_iter()
        .next()
        .ok_or_else(|| fail("image_execution_not_finished"))?;
    let source_result=db.prepare("SELECT f.ref_id,f.artifact_id,f.execution_generation,b.account_id,b.object_key,b.sha256,b.archive_bytes AS bytes,b.content_type,b.kind FROM workspace_blob_refs f JOIN workspace_blobs b ON b.object_key=f.object_key WHERE f.account_id=? AND f.session_id=? AND f.response_id=? AND f.execution_id=? AND f.kind='image-execution' AND b.kind='image' AND b.status='ready' LIMIT 5")
        .bind(&[text(owner),text(session),text(run),text(execution)])?.all().await?;
    let sources = source_result.results::<Source>()?;
    let mut receipts = vec![run_result, source_result];
    if sources.is_empty() {
        return Ok(ImageTransfer {
            results: receipts,
            complete: true,
        });
    }
    let mapped = validate_assets(owner, &closed.generation, assets, &sources)?;
    let now = crate::util::now_ms();
    let delivery = delivery.filter(|grant| grant.expires_at_ms > now);
    if let Some(grant) = delivery {
        component(&grant.id)?;
        if grant.expires_at_ms > now.saturating_add(3_600_000) {
            return Err(fail("invalid_image_delivery"));
        }
    }
    let conversation_prefix = conversation.map(conversation_ref_prefix).transpose()?;
    if let Some(conversation) = conversation {
        for (asset, _) in mapped
            .iter()
            .filter(|(asset, _)| asset.partial_index.is_none())
        {
            let id = conversation_image_ref_id(conversation, &asset.item_id)?;
            let check=db.prepare("SELECT EXISTS(SELECT 1 FROM conversation_image_tombstones WHERE account_id=?1 AND conversation_id=?2 AND item_id=?3) AS fenced,EXISTS(SELECT 1 FROM workspace_blob_refs WHERE account_id=?1 AND kind='conversation-artifact' AND ref_id=?4 AND object_key=?5 AND artifact_id=?3 AND response_id IS NULL) AS held")
                .bind(&[text(owner),text(conversation),text(&asset.item_id),text(&id),text(&asset.blob.object_key)])?.all().await?;
            #[derive(Deserialize)]
            struct Holder {
                fenced: u8,
                held: u8,
            }
            let holder = check
                .results::<Holder>()?
                .into_iter()
                .next()
                .ok_or_else(|| fail("image_conversation_not_ready"))?;
            receipts.push(check);
            if holder.fenced == 0 && holder.held == 0 {
                return Ok(ImageTransfer {
                    results: receipts,
                    complete: false,
                });
            }
        }
    }
    let mut statements = Vec::new();
    for (asset, source) in mapped {
        let public_id = image_ref_id(response, &asset.artifact_id)?;
        let mut targets: Vec<(&str, String, Option<u64>)> = Vec::new();
        if store_response {
            targets.push(("response-artifact", public_id.clone(), None));
        }
        if conversation_prefix
            .as_ref()
            .filter(|_| asset.partial_index.is_none())
            .is_some()
        {
            targets.push((
                "conversation-artifact",
                conversation_image_ref_id(conversation.unwrap_or(""), &asset.item_id)?,
                None,
            ));
        }
        if let Some(grant) = delivery {
            let digest = Sha256::digest(serde_json::to_vec(&(&grant.id, &asset.artifact_id))?);
            targets.push((
                "image-delivery",
                format!("delivery:{digest:x}"),
                Some(grant.expires_at_ms),
            ));
        }
        for (kind, id, expiry) in targets {
            let args = [
                text(owner),
                text(kind),
                text(&id),
                text(&asset.blob.object_key),
                if kind == "conversation-artifact" {
                    wasm_bindgen::JsValue::NULL
                } else {
                    text(response)
                },
                expiry
                    .map(|value| text(&value.to_string()))
                    .unwrap_or(wasm_bindgen::JsValue::NULL),
                text(&source.ref_id),
                text(session),
                text(run),
                text(execution),
                text(&asset.artifact_id),
                text(&closed.generation),
                text(conversation.unwrap_or("")),
                if kind == "conversation-artifact" {
                    text(&asset.item_id)
                } else {
                    wasm_bindgen::JsValue::NULL
                },
            ];
            statements.push(db.prepare("INSERT INTO workspace_blob_refs(account_id,kind,ref_id,object_key,response_id,lease_until_ms,artifact_id) SELECT ?1,?2,?3,?4,?5,CAST(?6 AS INTEGER),?14 WHERE EXISTS(SELECT 1 FROM workspace_blob_refs WHERE account_id=?1 AND kind='image-execution' AND ref_id=?7 AND object_key=?4 AND session_id=?8 AND response_id=?9 AND execution_id=?10 AND artifact_id=?11 AND execution_generation=?12) AND EXISTS(SELECT 1 FROM usage_reservations WHERE id=?9 AND account_id=?1 AND session_id=?8 AND idempotency_key LIKE 'run:%' AND execution_finished_at IS NOT NULL AND COALESCE(execution_generation,'legacy')=?12) AND (?2 NOT IN ('response-artifact','image-delivery') OR NOT EXISTS(SELECT 1 FROM response_blob_tombstones WHERE account_id=?1 AND response_id=?5)) AND (?2!='conversation-artifact' OR (NOT EXISTS(SELECT 1 FROM conversation_image_tombstones WHERE account_id=?1 AND conversation_id=?13 AND item_id=?14) AND EXISTS(SELECT 1 FROM workspace_blob_refs WHERE account_id=?1 AND kind='conversation-artifact' AND ref_id=?3 AND object_key=?4 AND artifact_id=?14 AND response_id IS NULL))) ON CONFLICT(account_id,kind,ref_id) DO UPDATE SET object_key=excluded.object_key,lease_until_ms=MIN(workspace_blob_refs.lease_until_ms,excluded.lease_until_ms)").bind(&args)?);
        }
    }
    statements.push(db.prepare("DELETE FROM workspace_blob_refs WHERE account_id=?1 AND kind='image-execution' AND session_id=?2 AND response_id=?3 AND execution_id=?4 AND execution_generation=?5 AND EXISTS(SELECT 1 FROM usage_reservations WHERE id=?3 AND account_id=?1 AND session_id=?2 AND idempotency_key LIKE 'run:%' AND execution_finished_at IS NOT NULL AND COALESCE(execution_generation,'legacy')=?5)")
        .bind(&[text(owner),text(session),text(run),text(execution),text(&closed.generation)])?);
    receipts.extend(db.batch(statements).await?);
    Ok(ImageTransfer {
        results: receipts,
        complete: true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace_blobs::BlobKind;

    fn blob() -> OwnedBlobRef {
        let owner = "owner";
        let hash = Sha256::digest(owner.as_bytes());
        let sha = "ab".repeat(32);
        OwnedBlobRef {
            account_id: owner.into(),
            object_key: format!(
                "owner-blobs/{hash:x}/image/{sha}/12345678-abcd-4abc-8abc-123456789abc"
            ),
            sha256: sha,
            bytes: 100,
            content_type: "image/png".into(),
            kind: BlobKind::ImageArtifact,
        }
    }
    #[test]
    fn mapped_assets_require_exact_generation_blob_and_unique_source_slot() {
        let blob = blob();
        let sources = vec![Source {
            ref_id: "source".into(),
            artifact_id: "item:result".into(),
            execution_generation: "legacy".into(),
            blob: blob.clone(),
        }];
        let asset = ImageExecutionAsset {
            artifact_id: "item:result".into(),
            item_id: "item".into(),
            partial_index: None,
            blob,
        };
        assert!(validate_assets("owner", "legacy", std::slice::from_ref(&asset), &sources).is_ok());
        assert!(
            validate_assets("owner", "different", std::slice::from_ref(&asset), &sources).is_err()
        );
        assert!(
            validate_assets("owner", "legacy", &[asset.clone(), asset.clone()], &sources).is_err()
        );
        let mut wrong = asset.clone();
        wrong.artifact_id = "unknown".into();
        assert!(validate_assets("owner", "legacy", &[wrong], &sources).is_err());
        wrong = asset;
        wrong.blob.bytes += 1;
        assert!(validate_assets("owner", "legacy", &[wrong], &sources).is_err());
    }
    #[test]
    fn conversation_final_image_identity_has_only_conversation_and_item_scope() {
        let first = conversation_image_ref_id("conv_a", "item").unwrap();
        assert_eq!(first, conversation_image_ref_id("conv_a", "item").unwrap());
        assert_ne!(first, conversation_image_ref_id("conv_b", "item").unwrap());
        assert_ne!(first, conversation_image_ref_id("conv_a", "other").unwrap());
        assert!(first.starts_with("conv_a:image:"));
        assert!(conversation_image_ref_id("conv:invalid", "item").is_err());
    }

    #[test]
    fn execution_slots_do_not_alias_scopes_or_artifact_ids() {
        assert_ne!(
            execution_ref_id("a:b", "c", "op", "slot").unwrap(),
            execution_ref_id("a", "b:c", "op", "slot").unwrap()
        );
        assert_ne!(
            execution_ref_id("a", "run", "first", "slot").unwrap(),
            execution_ref_id("a", "run", "second", "slot").unwrap()
        );
        assert_ne!(
            execution_ref_id("a", "run", "op", "first").unwrap(),
            execution_ref_id("a", "run", "op", "second").unwrap()
        );
    }
}
