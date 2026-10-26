//! One owned retrieval effect. Publication retries reuse its retained evidence.

use super::*;
use crate::{
    file_ingest,
    file_search::Hit,
    hosted_bindings::{ResolvedBinding, ResolvedTool},
    resources::{ResourceMeter, ResourcePricing},
    session_storage::{read_chunks, write_chunks},
};
use sha2::{Digest, Sha256};

#[derive(Serialize, Deserialize)]
struct SearchOperation {
    digest: String,
    queries: Vec<String>,
    pricing: ResourcePricing,
    started: bool,
    hits: Option<Vec<Hit>>,
    error: Option<String>,
    finished: bool,
}

impl SessionInner {
    pub(super) async fn dispatch_hosted_file_search(
        &self,
        record: &SessionRecord,
        execution: &str,
        call: &provider::ToolCall,
        binding: &ResolvedBinding,
    ) -> Result<HostedDispatch> {
        let ResolvedTool::FileSearch { options } = &binding.tool else {
            return Err(Error::RustError("hosted_tool_binding_mismatch".into()));
        };
        let args = crate::hosted_tools::parse_file_search(&call.function.arguments)
            .map_err(|code| Error::RustError(code.into()))?;
        let digest = format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(&(
                &record.owner_id,
                &record.id,
                execution,
                binding,
                &args.queries
            ))?)
        );
        let key = format!("hosted-file:{}", hosted_item_id(execution));
        let storage = self.state.storage();
        let retained = read_chunks(&storage, &key).await?;
        let mut operation: SearchOperation = match retained {
            Some(bytes) => serde_json::from_slice(&bytes)?,
            None => SearchOperation {
                digest: digest.clone(),
                queries: args.queries,
                pricing: ResourcePricing::parse(
                    &self.env.var("RESOURCE_PRICING_JSON")?.to_string(),
                )?,
                started: false,
                hits: None,
                error: None,
                finished: false,
            },
        };
        if operation.digest != digest {
            return Err(Error::RustError("file_search_operation_conflict".into()));
        }
        if operation.finished {
            return Ok(HostedDispatch::Ready(search_result(&operation).into()));
        }
        let resource = format!("file-search:{}", hosted_item_id(execution));
        let meter = ResourceMeter::with_account_pricing(
            &self.env,
            &record.owner_id,
            &resource,
            operation.pricing.clone(),
        )?;
        if operation.started && operation.hits.is_none() && operation.error.is_none() {
            operation.error = Some("file_search_outcome_unknown".into());
            write_chunks(&storage, &key, &serde_json::to_vec(&operation)?).await?;
        }
        if !operation.started && self.file_search_cancelled() {
            release_empty_budget(&self.env, &record.owner_id, &resource).await?;
            return Ok(HostedDispatch::Ready(
                json!({"error":{"code":"file_search_cancelled"}})
                    .to_string()
                    .into(),
            ));
        }
        if !operation.started {
            let profile = file_ingest::profile(&self.env)?;
            file_ingest::preflight(&profile, &meter)?;
            crate::allowances::sync(&self.env, &record.owner_id).await?;
            crate::reconcile::maintenance_account_reservation(
                &self.env,
                &record.owner_id,
                &resource,
            )
            .await?;
            let id = hosted_item_id(execution);
            let item = json!({"id":id,"type":"file_search_call","status":"in_progress","queries":operation.queries,"results":null});
            self.publish_tool_item(&item, false, item.clone(), |index, _| {
                vec![
                    (
                        "response.file_search_call.in_progress",
                        json!({"item_id":id,"output_index":index}),
                    ),
                    (
                        "response.file_search_call.searching",
                        json!({"item_id":id,"output_index":index}),
                    ),
                ]
            })
            .await?;
            operation.started = true;
            write_chunks(&storage, &key, &serde_json::to_vec(&operation)?).await?;
            match file_ingest::search_queries(
                &self.env,
                &record.owner_id,
                &operation.queries,
                options,
                &meter,
                "query",
            )
            .await
            {
                Ok(hits) => operation.hits = Some(hits),
                Err(Error::RustError(code))
                    if code.len() <= 128
                        && code.bytes().all(|byte| {
                            byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_'
                        }) =>
                {
                    operation.error = Some(code)
                }
                Err(_) => operation.error = Some("file_search_failed".into()),
            }
            write_chunks(&storage, &key, &serde_json::to_vec(&operation)?).await?;
        }
        if operation.hits.is_some() {
            meter.settle_run().await?;
        } else {
            // Unknown provider/resource measurements keep only their own hold;
            // they must not keep the model execution alive or trigger a new query.
            let _ = meter.settle_run().await;
            release_empty_budget(&self.env, &record.owner_id, &resource).await?;
        }
        let item = json!({"id":hosted_item_id(execution),"type":"file_search_call","status":if operation.hits.is_some(){"completed"}else{"failed"},"queries":operation.queries,"results":operation.hits.as_ref().map(|hits|hits.iter().map(Hit::tool_result).collect::<Vec<_>>())});
        self.publish_hosted_item(&item, true).await?;
        operation.finished = true;
        write_chunks(&storage, &key, &serde_json::to_vec(&operation)?).await?;
        Ok(HostedDispatch::Ready(search_result(&operation).into()))
    }

    fn file_search_cancelled(&self) -> bool {
        self.data
            .borrow()
            .as_ref()
            .and_then(|data| data.run.as_ref())
            .and_then(|run| run.response.as_ref())
            .is_none_or(|response| response.cancel_requested)
    }
}

async fn release_empty_budget(env: &Env, owner: &str, resource: &str) -> Result<()> {
    // A query's first operation is a metered D1 ownership read. No observations
    // and no ledger therefore prove that this funded query never dispatched.
    env.d1("DB")?.prepare("UPDATE usage_reservations SET state='released',updated_at=CAST(?3 AS INTEGER) WHERE id=?1 AND account_id=?2 AND session_id IS NULL AND state='reserved' AND NOT EXISTS(SELECT 1 FROM resource_observations o WHERE o.run_id=?1 AND o.account_id=?2) AND NOT EXISTS(SELECT 1 FROM usage_ledger l WHERE l.reservation_id=?1)")
        .bind(&[resource.into(),owner.into(),util::now_ms().to_string().into()])?.run().await?;
    Ok(())
}

fn search_result(operation: &SearchOperation) -> String {
    let result = match &operation.hits {
        Some(hits) => {
            json!({"results":file_ingest::evidence(hits),"citation_instructions":"Append the exact citation_marker after each supported statement. Never invent a marker or cite a different file."})
        }
        None => {
            json!({"error":{"code":operation.error.as_deref().unwrap_or("file_search_failed")}})
        }
    };
    result.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_search_private_result_is_json_text_for_native_continuation() {
        let mut operation = SearchOperation {
            digest: "fixture".into(),
            queries: vec!["source".into()],
            pricing: ResourcePricing {
                version: "unused-fixture".into(),
                tariffs: vec![],
            },
            started: true,
            hits: Some(vec![]),
            error: None,
            finished: true,
        };
        let message =
            provider::Message::tool_result_content("call", search_result(&operation).into());
        let items = provider::responses::message_items(&[message]).unwrap();
        let text = items[0]["output"][0]["text"].as_str().unwrap();
        assert!(serde_json::from_str::<Value>(text).unwrap()["results"].is_array());
        operation.hits = None;
        operation.error = Some("file_search_outcome_unknown".into());
        let result = search_result(&operation);
        assert_eq!(
            serde_json::from_str::<Value>(&result).unwrap()["error"]["code"],
            "file_search_outcome_unknown"
        );
    }
}
