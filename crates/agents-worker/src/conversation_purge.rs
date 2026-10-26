//! Explicit irreversible removal is separate from API visibility deletion.

use super::*;

const MARKER: &str = "conversation.purge.v1";
const BODY_PREFIXES: &[&str] = &["item:", "provenance:", "admission:", "image:"];

pub(super) async fn resume(
    context: &PublicationContext<'_>,
    record: &mut ConversationRecord,
) -> Result<bool> {
    let Some(complete) = context.storage.get::<bool>(MARKER).await? else {
        return Ok(false);
    };
    if complete {
        return Ok(true);
    }
    // The marker fences every public and internal reader before any removal.
    // It fits the retained header's fixed logical-object metadata allowance.
    for id in &record.items {
        if read_chunks(context.storage, &format!("image:{id}"))
            .await?
            .is_none()
        {
            continue;
        }
        context
            .operation
            .external_d1(
                context.env,
                &record.owner,
                "conversation-purge-image",
                2,
                crate::workspace_blobs::release_conversation_image(
                    context.env,
                    &record.owner,
                    &record.id,
                    id,
                ),
            )
            .await?;
    }
    record.items.clear();
    record.tombstones.clear();
    record.admitted_responses.clear();
    record.committed_responses.clear();
    record.execution_hint = None;
    record.metadata = json!({});
    context.save(record, vec![]).await?;
    for prefix in BODY_PREFIXES {
        remove_prefix(context.storage, prefix).await?;
    }
    // No body or retained admission snapshot exists now. Never refund header
    // blocks: the deleted identity fence and physical header remain retained.
    context.operation.query(context.env, &record.owner, "capacity-purge-bodies",
        context.env.d1("DB")?.prepare("UPDATE conversation_capacity_objects SET state='released',release_proof='content_removed',updated_at_ms=CAST(?3 AS INTEGER) WHERE account_id=?1 AND conversation_id=?2 AND object_id LIKE 'body:%' AND state IN ('reserved','retained')")
            .bind(&[record.owner.as_str().into(),record.id.as_str().into(),util::now_ms().to_string().into()])?,
    ).await?;
    context.storage.put(MARKER, true).await?;
    Ok(true)
}

pub(super) async fn start(
    context: &PublicationContext<'_>,
    record: &mut ConversationRecord,
) -> Result<Response> {
    reclaim_pending(context, record, None).await?;
    if !record.deleted || record.active_response_id.is_some() {
        return util::json_error(
            409,
            "conversation_purge_conflict",
            "Delete the Conversation metadata and finish its active response before purging retained data",
        );
    }
    #[derive(Deserialize)]
    struct Active {
        count: u64,
    }
    let active = context.operation.query(context.env, &record.owner, "capacity-purge-check",
        context.env.d1("DB")?.prepare("SELECT COUNT(*) AS count FROM conversation_response_capacity WHERE account_id=?1 AND conversation_id=?2 AND state='reserved'")
            .bind(&[record.owner.as_str().into(),record.id.as_str().into()])?,
    ).await?.results::<Active>()?;
    if active.first().is_some_and(|row| row.count != 0) {
        return util::json_error(
            409,
            "conversation_purge_conflict",
            "Conversation capacity is still reserved",
        );
    }
    context.storage.put(MARKER, false).await?;
    resume(context, record).await?;
    completed(&record.id)
}

pub(super) fn completed(id: &str) -> Result<Response> {
    Response::from_json(&json!({"id":id,"object":"conversation.purged","purged":true}))
}

async fn remove_prefix(storage: &Storage, prefix: &str) -> Result<()> {
    loop {
        let entries = storage
            .list_with_options(ListOptions::new().prefix(prefix).limit(128))
            .await?;
        let keys = js_sys::Array::from(&entries.keys())
            .iter()
            .filter_map(|value| value.as_string())
            .collect::<Vec<_>>();
        if keys.is_empty() {
            return Ok(());
        }
        storage.delete_multiple(keys).await?;
    }
}
