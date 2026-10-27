//! Model-independent, owner-scoped conversation authority. The outer API authenticates
//! the caller; this object verifies the resulting owner binding on every operation.
use crate::{
    conversation_meter::ConversationOperation,
    conversation_quota::{self as quota, Body},
    conversation_routes::ConversationItemProvenance,
    session_storage::{read_chunks, read_many},
    util,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use worker::*;

#[path = "conversation_purge.rs"]
mod purge;

const HEAD: &str = "conversation.v1";
const IMAGE_DELETE: &str = "conversation.image-delete";
const MAX_ITEMS: usize = 20;
const MAX_BODY: usize = 16 * 1024 * 1024;
const PENDING_LEASE_MS: u64 = 120_000;

#[derive(Clone, Serialize, Deserialize)]
struct ConversationRecord {
    id: String,
    owner: String,
    created_at: u64,
    metadata: Value,
    revision: u64,
    deleted: bool,
    items: Vec<String>,
    tombstones: BTreeSet<String>,
    #[serde(default)]
    committed_responses: BTreeMap<String, u64>,
    #[serde(default)]
    admitted_responses: BTreeSet<String>,
    #[serde(default)]
    active_response_id: Option<String>,
    #[serde(default)]
    active_session_id: Option<String>,
    #[serde(default)]
    active_activated: bool,
    #[serde(default)]
    active_capacity_generation: Option<String>,
    #[serde(default)]
    pending_expires_at_ms: Option<u64>,
    #[serde(default)]
    execution_hint: Option<ExecutionHint>,
    #[serde(default)]
    quota_header_blocks: u64,
}

#[derive(Clone, Serialize, Deserialize)]
struct ExecutionHint {
    response_id: String,
    session_id: String,
    agent_id: String,
    pin_hash: String,
    adopted_revision: u64,
}

#[derive(Serialize, Deserialize)]
struct AdmissionReceipt {
    revision: u64,
    item_ids: Vec<String>,
    predecessor: Option<ExecutionHint>,
}

impl ConversationRecord {
    fn public(&self) -> Value {
        json!({"id":self.id,"object":"conversation","created_at":self.created_at,"metadata":self.metadata})
    }

    fn pending_expired(&self, now_ms: u64) -> bool {
        !self.active_activated
            && self
                .pending_expires_at_ms
                .is_some_and(|deadline| now_ms >= deadline)
    }

    fn clear_lease(&mut self) {
        self.active_response_id = None;
        self.active_session_id = None;
        self.active_activated = false;
        self.active_capacity_generation = None;
        self.pending_expires_at_ms = None;
    }

    fn reserve_lease(&mut self, response_id: &str, now_ms: u64) {
        if self.active_response_id.as_deref() == Some(response_id)
            && (self.active_activated || self.pending_expires_at_ms.is_none())
        {
            return;
        }
        self.active_response_id = Some(response_id.into());
        self.active_session_id = None;
        self.active_activated = false;
        self.pending_expires_at_ms = Some(now_ms.saturating_add(PENDING_LEASE_MS));
    }
}

#[durable_object]
pub struct ConversationState {
    state: State,
    env: Env,
    serial: futures::lock::Mutex<()>,
}

impl DurableObject for ConversationState {
    fn new(state: State, env: Env) -> Self {
        Self {
            state,
            env,
            serial: futures::lock::Mutex::new(()),
        }
    }

    async fn fetch(&self, req: Request) -> Result<Response> {
        let storage = self.state.storage();
        let env = self.env.clone();
        let owner = req.headers().get("X-Agents-Owner")?.unwrap_or_default();
        let headers = req.headers().clone();
        let operation = ConversationOperation::actor_scope(&headers)?;
        let started = util::now_ms();
        let conversation_id = headers.get("X-Agents-Conversation-ID")?;
        let request_id = headers.get("X-Agents-Conversation-Request")?;
        let path = req.path();
        operation.emit(
            "request.received",
            json!({"conversation_id":conversation_id,"request_id":request_id,"request_path":path}),
        );
        let _serial = self.serial.lock().await;
        operation.emit("request.locked", json!({"conversation_id":conversation_id,"request_id":request_id,"request_path":path,"lock_wait_ms":util::now_ms().saturating_sub(started)}));
        let result =
        crate::conversation_meter::actor(&env, &owner, &headers, async {
            let result = handle(req, &storage, &env, &operation).await;
            let result = match result {
                Ok(mut response) => {
                    if !matches!(quota::has_pending(&storage).await, Ok(false)) {
                        response.headers_mut().set("X-Agents-Conversation-Capacity-Status", "pending")?;
                    }
                    Ok(response)
                }
                error => error,
            };
            match result {
                Err(Error::RustError(code)) if code == "conversation_item_conflict" => {
                    util::json_error(
                        409,
                        "invalid_request",
                        "Item id already exists in this conversation",
                    )
                }
                Err(Error::RustError(code))
                    if code == "conversation_execution_context_invalid" =>
                {
                    util::json_error(
                        400,
                        "invalid_request",
                        "Invalid conversation execution context",
                    )
                }
                Err(Error::RustError(code)) if code == "conversation_capacity_exceeded" => {
                    let mut error = util::error_body("conversation_capacity_exceeded", "Conversation retained logical-byte capacity has been reached; this budget is independent of Space storage");
                    error["error"]["capacity_budget"] = json!("conversation_retained_logical_bytes");
                    error["error"]["capacity"] = serde_json::to_value(quota::usage(&env, &owner, &operation).await?)?;
                    Response::from_json(&error).map(|response| response.with_status(409))
                }
                Err(Error::RustError(code)) if code.starts_with("conversation_capacity_") && code!="conversation_capacity_exceeded" => util::json_error(409,&code,"Conversation capacity generation or input claim is no longer valid"),
                Err(Error::RustError(code)) if code=="conversation_item_reserved" => util::json_error(409,&code,"Item identity is reserved by an active response"),
                Err(Error::RustError(code)) if code=="unresolved_item_identity" => util::json_error(400,&code,"Server item identities require an owner-canonical reference"),
                result => result,
            }
        }).await;
        let mut fields = json!({"conversation_id":conversation_id,"request_id":request_id,"request_path":path,
            "duration_ms":util::now_ms().saturating_sub(started),"status":result.as_ref().ok().map(|response| response.status_code())});
        if let Err(error) = &result {
            fields.as_object_mut().unwrap().extend(
                crate::conversation_meter::error_metadata(error, started)
                    .as_object()
                    .unwrap()
                    .clone(),
            );
        }
        operation.emit(
            if result.is_ok() {
                "request.completed"
            } else {
                "request.failed"
            },
            fields,
        );
        result
    }
}

async fn load(storage: &Storage) -> Result<Option<ConversationRecord>> {
    read_chunks(storage, HEAD)
        .await?
        .map(|bytes| {
            serde_json::from_slice(&bytes)
                .map_err(|_| Error::RustError("conversation_state_corrupt".into()))
        })
        .transpose()
}

struct PublicationContext<'a> {
    storage: &'a Storage,
    env: &'a Env,
    operation: &'a ConversationOperation,
    legacy: Option<ConversationRecord>,
}

impl PublicationContext<'_> {
    async fn save(&self, record: &mut ConversationRecord, bodies: Vec<Body>) -> Result<()> {
        self.save_scoped(record, bodies, None).await
    }

    async fn save_scoped(
        &self,
        record: &mut ConversationRecord,
        bodies: Vec<Body>,
        escrow: Option<&quota::escrow::Lease>,
    ) -> Result<()> {
        self.adopt_legacy().await?;
        loop {
            let encoded = serde_json::to_vec(record)?;
            let blocks = quota::header_blocks(
                &record.id,
                header_footprint(record, encoded.len())?,
                record.quota_header_blocks,
            )?;
            if blocks == record.quota_header_blocks {
                break;
            }
            record.quota_header_blocks = blocks;
        }
        let encoded = serde_json::to_vec(record)?;
        quota::publish(
            self.env,
            &record.owner,
            self.operation,
            self.storage,
            &record.id,
            HEAD,
            &encoded,
            record.quota_header_blocks,
            &bodies,
            escrow,
        )
        .await
    }

    async fn adopt_legacy(&self) -> Result<()> {
        let Some(record) = &self.legacy else {
            return Ok(());
        };
        let mut keys = record
            .items
            .iter()
            .map(|id| (format!("item:{id}"), true))
            .collect::<Vec<_>>();
        keys.extend(
            record
                .items
                .iter()
                .map(|id| (format!("provenance:{id}"), false)),
        );
        keys.extend(
            record
                .admitted_responses
                .iter()
                .map(|id| (format!("admission:{id}"), true)),
        );
        let names = keys.iter().map(|(key, _)| key.clone()).collect::<Vec<_>>();
        let encoded = read_many(self.storage, &names).await?;
        let mut payloads = vec![];
        for ((key, required), bytes) in keys.into_iter().zip(encoded) {
            if let Some(bytes) = bytes {
                payloads.push(quota::Payload::body(&record.id, &key, &bytes)?);
            } else if required {
                return Err(Error::RustError(
                    "conversation_capacity_legacy_payload_missing".into(),
                ));
            }
        }
        let bytes = read_chunks(self.storage, HEAD).await?.ok_or_else(|| {
            Error::RustError("conversation_capacity_legacy_header_missing".into())
        })?;
        let blocks = quota::header_blocks(&record.id, header_footprint(record, bytes.len())?, 0)?;
        payloads.extend((0..blocks).map(|index| quota::Payload::header_block(&record.id, index)));
        quota::adopt_existing_batch(self.env, &record.owner, self.operation, &payloads).await
    }
}

fn header_footprint(record: &ConversationRecord, encoded_bytes: usize) -> Result<usize> {
    // Reserve each item's bounded future tombstone slot at insertion, not at
    // deletion. Hiding retained data must remain possible at a full quota.
    let visibility = record
        .items
        .iter()
        .filter(|id| !record.tombstones.contains(id.as_str()))
        .try_fold(0usize, |total, id| total.checked_add(id.len() + 3))
        .ok_or_else(|| Error::RustError("conversation_capacity_size_overflow".into()))?;
    let revision = 20usize.saturating_sub(record.revision.to_string().len());
    encoded_bytes
        .checked_add(visibility)
        .and_then(|bytes| bytes.checked_add(revision))
        .and_then(|bytes| bytes.checked_add(usize::from(record.deleted)))
        .ok_or_else(|| Error::RustError("conversation_capacity_size_overflow".into()))
}

async fn save(context: &PublicationContext<'_>, record: &mut ConversationRecord) -> Result<()> {
    context.save(record, vec![]).await
}

async fn receipt(
    storage: &Storage,
    record: &ConversationRecord,
    response_id: &str,
) -> Result<Option<AdmissionReceipt>> {
    if !record.admitted_responses.contains(response_id) {
        return Ok(None);
    }
    let bytes = read_chunks(storage, &format!("admission:{response_id}"))
        .await?
        .ok_or_else(|| Error::RustError("conversation_admission_missing".into()))?;
    Ok(Some(serde_json::from_slice(&bytes)?))
}

async fn reconcile_image_deletes(
    context: &PublicationContext<'_>,
    record: Option<&ConversationRecord>,
) -> Result<()> {
    let ids = context
        .storage
        .get::<Vec<String>>(IMAGE_DELETE)
        .await?
        .unwrap_or_default();
    for id in ids {
        if let Some(record) = record.filter(|record| record.tombstones.contains(&id)) {
            context
                .operation
                .external_d1(
                    context.env,
                    &record.owner,
                    "conversation-image-delete",
                    2,
                    crate::workspace_blobs::release_conversation_image(
                        context.env,
                        &record.owner,
                        &record.id,
                        &id,
                    ),
                )
                .await?;
        }
    }
    context.storage.delete(IMAGE_DELETE).await?;
    Ok(())
}

fn reject_server_ids(
    items: &[Value],
    proof: Option<&BTreeMap<String, ConversationItemProvenance>>,
) -> Result<()> {
    if items.iter().any(|item| {
        item["id"].as_str().is_some_and(|id| {
            crate::response_limits::service_owned_id(id)
                && !proof.is_some_and(|proof| proof.contains_key(id))
        })
    }) {
        return Err(Error::RustError("unresolved_item_identity".into()));
    }
    Ok(())
}

fn busy(record: &ConversationRecord) -> Result<Response> {
    let mut body = util::error_body(
        "conversation_busy",
        "Another response is using this conversation",
    );
    body["error"]["active_response_id"] = json!(record.active_response_id);
    body["error"]["active_session_id"] = json!(record.active_session_id);
    body["error"]["activated"] = json!(record.active_activated);
    Ok(Response::from_json(&body)?.with_status(409))
}

async fn reclaim_pending(
    context: &PublicationContext<'_>,
    record: &mut ConversationRecord,
    rejected: Option<(&str, &str)>,
) -> Result<bool> {
    // A deadline exists only for the unactivated admission protocol. Legacy
    // active/unknown leases have no invented deadline and cannot be reclaimed.
    if record.active_activated || record.pending_expires_at_ms.is_none() {
        return Ok(false);
    }
    let known_rejection = rejected.is_some_and(|(response, generation)| {
        record.active_response_id.as_deref() == Some(response)
            && record
                .active_capacity_generation
                .as_deref()
                .is_none_or(|active| active == generation)
    });
    if !known_rejection && (rejected.is_some() || !record.pending_expired(util::now_ms())) {
        return Ok(false);
    }
    if let (Some(response), Some(generation)) = (
        &record.active_response_id,
        &record.active_capacity_generation,
    ) {
        quota::escrow::release(
            context.env,
            &record.owner,
            context.operation,
            &record.id,
            &quota::escrow::Lease {
                response_id: response.clone(),
                generation: generation.clone(),
            },
        )
        .await?;
    }
    if let Some((response, _)) = rejected.filter(|_| known_rejection) {
        // No Session input/run was accepted. Completed/cancelled Responses keep
        // their admission membership and retain their established retry path.
        record.admitted_responses.remove(response);
    }
    let abandoned = record.active_response_id.clone();
    record.clear_lease();
    save(context, record).await?;
    if !known_rejection && let Some(response) = abandoned {
        let started = util::now_ms();
        let closed = context
            .operation
            .settle_reclaimed_response(context.env, &record.owner, &response)
            .await;
        let mut fields =
            json!({"conversation_id":record.id,"response_id":response,"success":closed.is_ok()});
        if let Err(error) = &closed {
            fields.as_object_mut().unwrap().extend(
                crate::conversation_meter::error_metadata(error, started)
                    .as_object()
                    .unwrap()
                    .clone(),
            );
        }
        context.operation.emit("reclaim.resources", fields);
    }
    Ok(true)
}

async fn admit(
    context: &PublicationContext<'_>,
    storage: &Storage,
    record: &mut ConversationRecord,
    body: &Value,
) -> Result<Response> {
    let response_id = body["response_id"].as_str().unwrap_or_default();
    if !valid_id(response_id) {
        return util::json_error(400, "invalid_request", "Invalid response identity");
    }
    let accepted = receipt(storage, record, response_id).await?;
    if accepted.is_none() && record.deleted {
        return util::json_error(404, "not_found", "Conversation not found");
    }
    let now_ms = util::now_ms();
    reclaim_pending(context, record, None).await?;
    if !record.committed_responses.contains_key(response_id)
        && record
            .active_response_id
            .as_deref()
            .is_some_and(|active| active != response_id)
    {
        return busy(record);
    }
    let funding_closed = context
        .operation
        .response_closed(context.env, &record.owner, response_id)
        .await?;
    if !record.admitted_responses.contains(response_id) && funding_closed {
        return util::json_error(
            409,
            "conversation_request_closed",
            "This request was rejected before admission; use a new idempotency key",
        );
    }
    let mut bodies = vec![];
    let accepted = match accepted {
        Some(accepted) => accepted,
        None => {
            let accepted = AdmissionReceipt {
                revision: record.revision,
                item_ids: record
                    .items
                    .iter()
                    .filter(|id| !record.tombstones.contains(id.as_str()))
                    .cloned()
                    .collect(),
                predecessor: record
                    .execution_hint
                    .as_ref()
                    .filter(|hint| hint.adopted_revision == record.revision)
                    .cloned(),
            };
            // The header's receipt membership is the commit marker. An interrupted
            // write cannot make an orphan candidate bypass the writer gate.
            bodies.push(Body::json(format!("admission:{response_id}"), &accepted)?);
            record.admitted_responses.insert(response_id.into());
            accepted
        }
    };
    if !funding_closed && !record.deleted && !record.committed_responses.contains_key(response_id) {
        record.reserve_lease(response_id, now_ms);
        context.save(record, bodies).await?;
    }
    let ids = accepted
        .item_ids
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>();
    let items = items(storage, &ids).await?;
    let provenance = item_provenance(storage, &accepted.item_ids).await?;
    let previous = accepted.predecessor.as_ref();
    Response::from_json(
        &json!({"id":record.id,"revision":accepted.revision,"items":items,"item_provenance":provenance,
        "predecessor_response_id":previous.map(|hint| &hint.response_id),
        "predecessor_session_id":previous.map(|hint| &hint.session_id),
        "predecessor_agent_id":previous.map(|hint| &hint.agent_id),
        "predecessor_pin_hash":previous.map(|hint| &hint.pin_hash)}),
    )
}

fn capacity_failure(required: u64, available: u64) -> Result<Response> {
    let mut body = util::error_body(
        "conversation_capacity_exceeded",
        "Prepaid Conversation output capacity is unavailable; this logical budget is independent of Space storage",
    );
    body["error"]["capacity_budget"] = json!("conversation_retained_logical_bytes");
    body["error"]["required_bytes"] = json!(required);
    body["error"]["available_bytes"] = json!(available);
    Response::from_json(&body).map(|response| response.with_status(409))
}

async fn reject_capacity(
    context: &PublicationContext<'_>,
    response: &str,
    generation: &str,
    required: u64,
    available: u64,
) -> Result<Response> {
    // Reload canonical state after a definite failed header allocation, rather
    // than retaining the candidate's unpublished larger header high-water mark.
    let mut record = load(context.storage)
        .await?
        .ok_or_else(|| Error::RustError("conversation_state_corrupt".into()))?;
    let reclaimed = reclaim_pending(context, &mut record, Some((response, generation))).await;
    let released = matches!(reclaimed, Ok(true));
    if reclaimed.is_err() {
        util::emit(
            "conversation.pending_reclaim_deferred",
            json!({"conversation_id":record.id,"response_id":response}),
        );
    }
    let mut rejected = capacity_failure(required, available)?;
    rejected.headers_mut().set(
        "X-Agents-Conversation-Writer-Released",
        if released { "true" } else { "pending" },
    )?;
    Ok(rejected)
}

fn capacity_request(body: &Value) -> Result<quota::escrow::Request> {
    let mut request: quota::escrow::Request =
        serde_json::from_value(body.get("capacity").cloned().unwrap_or_else(|| json!({})))?;
    if request
        .input_items
        .iter()
        .any(|item| !item.get("id").and_then(Value::as_str).is_some_and(valid_id))
    {
        return Err(Error::RustError(
            "conversation_capacity_input_identity_missing".into(),
        ));
    }
    request.input_items = normalize_items(&json!(request.input_items), usize::MAX)
        .map_err(|_| Error::RustError("conversation_execution_context_invalid".into()))?;
    Ok(request)
}

fn reject_retained_ids(record: &ConversationRecord, items: &[Value]) -> Result<()> {
    if items.iter().any(|item| {
        item["id"]
            .as_str()
            .is_some_and(|id| record.items.iter().any(|existing| existing == id))
    }) {
        return Err(Error::RustError("conversation_item_conflict".into()));
    }
    Ok(())
}

async fn reserve_capacity(
    context: &PublicationContext<'_>,
    storage: &Storage,
    record: &mut ConversationRecord,
    body: &Value,
) -> Result<Response> {
    let response_id = body["response_id"].as_str().unwrap_or_default();
    let session_id = body["session_id"].as_str().unwrap_or_default();
    if !valid_id(response_id) || !valid_id(session_id) {
        return util::json_error(
            400,
            "invalid_request",
            "Invalid response or Session identity",
        );
    }
    let accepted = receipt(storage, record, response_id).await?;
    if accepted.as_ref().map(|receipt| receipt.revision) != body["accepted_revision"].as_u64()
        || record.active_response_id.as_deref() != Some(response_id)
        || record.pending_expired(util::now_ms())
        || record.committed_responses.contains_key(response_id)
        || (record.active_activated && record.active_session_id.as_deref() != Some(session_id))
    {
        return busy(record);
    }
    let request = capacity_request(body)?;
    reject_retained_ids(record, &request.input_items)?;
    reject_server_ids(&request.input_items, Some(&request.input_provenance))?;
    let plan = quota::escrow::plan(&record.owner, &record.id, response_id, session_id, &request)?;
    let generation = record
        .active_capacity_generation
        .clone()
        .unwrap_or_else(|| uuid::Uuid::new_v4().simple().to_string());
    record.active_session_id = Some(session_id.into());
    record.active_capacity_generation = Some(generation.clone());
    if let Err(error) = save(context, record).await {
        if matches!(&error,Error::RustError(code) if code=="conversation_capacity_exceeded") {
            let usage = quota::usage(context.env, &record.owner, context.operation).await?;
            return reject_capacity(
                context,
                response_id,
                &generation,
                plan.required_bytes(),
                usage
                    .limit_bytes
                    .saturating_sub(usage.reserved_bytes.saturating_add(usage.retained_bytes)),
            )
            .await;
        }
        return Err(error);
    }
    match quota::escrow::prepare(
        context.env,
        &record.owner,
        context.operation,
        &record.id,
        response_id,
        session_id,
        &generation,
        &plan,
    )
    .await?
    {
        quota::escrow::Prepared::Held(lease) => Response::from_json(
            &json!({"id":record.id,"response_id":response_id,"capacity_generation":lease.generation,"reserved":true}),
        ),
        quota::escrow::Prepared::Insufficient {
            required_bytes,
            available_bytes,
        } => {
            reject_capacity(
                context,
                response_id,
                &generation,
                required_bytes,
                available_bytes,
            )
            .await
        }
    }
}

async fn reserve_additional_input(
    context: &PublicationContext<'_>,
    record: &ConversationRecord,
    body: &Value,
) -> Result<Response> {
    let response = body["response_id"].as_str().unwrap_or_default();
    let generation = body["capacity_generation"].as_str().unwrap_or_default();
    let session = body["session_id"].as_str().unwrap_or_default();
    let submission = body["submission_id"].as_str().unwrap_or_default();
    if !valid_id(submission)
        || record.active_response_id.as_deref() != Some(response)
        || !record.active_activated
        || record.active_session_id.as_deref() != Some(session)
        || record.active_capacity_generation.as_deref() != Some(generation)
        || record.committed_responses.contains_key(response)
    {
        return busy(record);
    }
    let request = capacity_request(body)?;
    reject_retained_ids(record, &request.input_items)?;
    reject_server_ids(&request.input_items, Some(&request.input_provenance))?;
    let lease = quota::escrow::Lease {
        response_id: response.into(),
        generation: generation.into(),
    };
    match quota::escrow::extend(
        context.env,
        &record.owner,
        context.operation,
        &record.id,
        &lease,
        submission,
        &request,
    )
    .await?
    {
        quota::escrow::Prepared::Held(_) => {
            Response::from_json(&json!({"id":record.id,"response_id":response,"reserved":true}))
        }
        quota::escrow::Prepared::Insufficient {
            required_bytes,
            available_bytes,
        } => capacity_failure(required_bytes, available_bytes),
    }
}

async fn activate(
    context: &PublicationContext<'_>,
    storage: &Storage,
    record: &mut ConversationRecord,
    body: &Value,
) -> Result<Response> {
    let response_id = body["response_id"].as_str().unwrap_or_default();
    let session_id = body["session_id"].as_str().unwrap_or_default();
    if !valid_id(response_id) || !valid_id(session_id) {
        return util::json_error(
            400,
            "invalid_request",
            "Invalid response or session identity",
        );
    }
    let accepted = receipt(storage, record, response_id).await?;
    if accepted.is_none()
        || accepted.as_ref().map(|receipt| receipt.revision) != body["accepted_revision"].as_u64()
    {
        return util::json_error(
            409,
            "conversation_revision_mismatch",
            "Response admission revision does not match",
        );
    }
    if record.active_response_id.as_deref() != Some(response_id)
        || record.pending_expired(util::now_ms())
        || (record.active_activated && record.active_session_id.as_deref() != Some(session_id))
    {
        return busy(record);
    }
    let generation = body["capacity_generation"]
        .as_str()
        .or(record.active_capacity_generation.as_deref());
    if let Some(generation) = generation {
        if record.active_capacity_generation.as_deref() != Some(generation) {
            return busy(record);
        }
        quota::escrow::validate_generation(
            context.env,
            &record.owner,
            context.operation,
            &record.id,
            &quota::escrow::Lease {
                response_id: response_id.into(),
                generation: generation.into(),
            },
            Some(session_id),
        )
        .await?;
    }
    record.active_session_id = Some(session_id.into());
    record.active_activated = true;
    record.pending_expires_at_ms = None;
    save(context, record).await?;
    Response::from_json(
        &json!({"id":record.id,"response_id":response_id,"session_id":session_id,"activated":true,"capacity_generation":record.active_capacity_generation}),
    )
}

async fn item_provenance(
    storage: &Storage,
    ids: &[String],
) -> Result<BTreeMap<String, ConversationItemProvenance>> {
    let keys = ids
        .iter()
        .map(|id| format!("provenance:{id}"))
        .collect::<Vec<_>>();
    let entries = read_many(storage, &keys).await?;
    let mut result = BTreeMap::new();
    for (id, bytes) in ids.iter().zip(entries) {
        if let Some(bytes) = bytes {
            result.insert(id.clone(), serde_json::from_slice(&bytes)?);
        }
    }
    Ok(result)
}

fn execution_hint(
    body: &Value,
    response_id: &str,
    revision: u64,
    accepted_revision: Option<u64>,
) -> Result<Option<ExecutionHint>> {
    let (Some(session), Some(agent), Some(pin)) = (
        body["session_id"].as_str(),
        body["agent_id"].as_str(),
        body["pin_hash"].as_str(),
    ) else {
        return Ok(None);
    };
    if !valid_id(session) || !valid_id(agent) || pin.is_empty() || pin.len() > 256 {
        return Err(Error::RustError(
            "conversation_execution_context_invalid".into(),
        ));
    }
    Ok(
        (accepted_revision == Some(revision)).then(|| ExecutionHint {
            response_id: response_id.into(),
            session_id: session.into(),
            agent_id: agent.into(),
            pin_hash: pin.into(),
            adopted_revision: revision + 1,
        }),
    )
}

fn metadata(value: &Value) -> bool {
    value.is_null()
        || value.as_object().is_some_and(|map| {
            map.len() <= 16
                && map.iter().all(|(key, value)| {
                    key.chars().count() <= 64
                        && value
                            .as_str()
                            .is_some_and(|value| value.chars().count() <= 512)
                })
        })
}

fn normalize_items(value: &Value, max: usize) -> std::result::Result<Vec<Value>, &'static str> {
    let items = value.as_array().ok_or("items must be an array")?;
    if items.len() > max {
        return Err("Too many items");
    }
    items
        .iter()
        .map(|item| {
            let mut item = item.clone();
            let object = item.as_object_mut().ok_or("Each item must be an object")?;
            if object.get("type").is_none() && object.get("role").is_some() {
                object.insert("type".into(), json!("message"));
            }
            if !object
                .get("type")
                .and_then(Value::as_str)
                .is_some_and(valid_item_type)
            {
                return Err("Invalid conversation item type");
            }
            match object.get("id") {
                Some(id) if !id.as_str().is_some_and(valid_id) => return Err("Invalid item id"),
                None => {
                    object.insert("id".into(), json!(util::id("item")));
                }
                _ => {}
            }
            if object.get("type").and_then(Value::as_str) == Some("message") {
                if !object
                    .get("role")
                    .and_then(Value::as_str)
                    .is_some_and(|role| {
                        matches!(
                            role,
                            "unknown"
                                | "user"
                                | "assistant"
                                | "system"
                                | "critic"
                                | "discriminator"
                                | "developer"
                                | "tool"
                        )
                    })
                {
                    return Err("Invalid message role");
                }
                object.entry("status").or_insert(json!("completed"));
                if !object
                    .get("status")
                    .and_then(Value::as_str)
                    .is_some_and(|status| {
                        matches!(status, "in_progress" | "completed" | "incomplete")
                    })
                {
                    return Err("Invalid message status");
                }
                if let Some(content) = object
                    .get("content")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                {
                    object.insert(
                        "content".into(),
                        json!([{"type":"input_text","text":content}]),
                    );
                }
                if !object.get("content").is_some_and(Value::is_array) {
                    return Err("Message content must be a string or array");
                }
            }
            Ok(item)
        })
        .collect()
}

fn valid_item_type(kind: &str) -> bool {
    matches!(
        kind,
        "message"
            | "function_call"
            | "function_call_output"
            | "file_search_call"
            | "web_search_call"
            | "image_generation_call"
            | "computer_call"
            | "computer_call_output"
            | "tool_search_call"
            | "tool_search_output"
            | "additional_tools"
            | "configuration_update"
            | "reasoning"
            | "program"
            | "program_output"
            | "compaction"
            | "code_interpreter_call"
            | "local_shell_call"
            | "local_shell_call_output"
            | "shell_call"
            | "shell_call_output"
            | "apply_patch_call"
            | "apply_patch_call_output"
            | "mcp_list_tools"
            | "mcp_approval_request"
            | "mcp_approval_response"
            | "mcp_call"
            | "custom_tool_call"
            | "custom_tool_call_output"
    )
}

fn valid_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
}

async fn item(storage: &Storage, id: &str) -> Result<Value> {
    let bytes = read_chunks(storage, &format!("item:{id}"))
        .await?
        .ok_or_else(|| Error::RustError("conversation_item_missing".into()))?;
    Ok(serde_json::from_slice(&bytes)?)
}

async fn items(storage: &Storage, ids: &[&str]) -> Result<Vec<Value>> {
    let keys = ids
        .iter()
        .map(|id| format!("item:{id}"))
        .collect::<Vec<_>>();
    read_many(storage, &keys)
        .await?
        .into_iter()
        .map(|bytes| {
            let bytes =
                bytes.ok_or_else(|| Error::RustError("conversation_item_missing".into()))?;
            Ok(serde_json::from_slice(&bytes)?)
        })
        .collect()
}

fn page(items: Vec<Value>, has_more: bool) -> Value {
    json!({"object":"list","first_id":items.first().and_then(|i| i.get("id")),
        "last_id":items.last().and_then(|i| i.get("id")),"has_more":has_more,"data":items})
}

fn includes(req: &Request) -> Result<BTreeSet<String>> {
    Ok(req
        .url()?
        .query_pairs()
        .filter(|(key, _)| matches!(key.as_ref(), "include" | "include[]"))
        .map(|(_, value)| value.into_owned())
        .collect())
}

fn project(mut item: Value, include: &BTreeSet<String>) -> Value {
    let (field, option) = match item["type"].as_str() {
        Some("reasoning") => ("encrypted_content", "reasoning.encrypted_content"),
        Some("file_search_call") => ("results", "file_search_call.results"),
        Some("code_interpreter_call") => ("outputs", "code_interpreter_call.outputs"),
        _ => ("", ""),
    };
    if !field.is_empty() && !include.contains(option) {
        if let Some(object) = item.as_object_mut() {
            object.remove(field);
        }
    }
    if item["type"] == "web_search_call" && !include.contains("web_search_call.action.sources") {
        if let Some(action) = item.get_mut("action").and_then(Value::as_object_mut) {
            action.remove("sources");
        }
    }
    if item["type"] == "computer_call_output"
        && !include.contains("computer_call_output.output.image_url")
    {
        if let Some(output) = item.get_mut("output").and_then(Value::as_object_mut) {
            output.remove("image_url");
        }
    }
    if item["type"] == "message" {
        if let Some(content) = item.get_mut("content").and_then(Value::as_array_mut) {
            for part in content {
                let (field, option) = match part["type"].as_str() {
                    Some("input_image") => ("image_url", "message.input_image.image_url"),
                    Some("output_text") => ("logprobs", "message.output_text.logprobs"),
                    _ => continue,
                };
                if !include.contains(option) {
                    if let Some(object) = part.as_object_mut() {
                        object.remove(field);
                    }
                }
            }
        }
    }
    item
}

#[derive(Default)]
struct AppendMetadata<'a> {
    provenance: Option<&'a BTreeMap<String, ConversationItemProvenance>>,
    output_source: Option<&'a ConversationItemProvenance>,
    output_ids: Option<&'a BTreeSet<String>>,
    images: Option<&'a BTreeMap<String, crate::workspace_blobs::OwnedBlobRef>>,
    escrow: Option<&'a quota::escrow::Lease>,
}

async fn append(
    context: &PublicationContext<'_>,
    record: &mut ConversationRecord,
    items: Vec<Value>,
    metadata: AppendMetadata<'_>,
) -> Result<Vec<Value>> {
    let mut seen = record.items.iter().cloned().collect::<BTreeSet<_>>();
    for item in &items {
        if !seen.insert(item["id"].as_str().unwrap_or_default().to_owned()) {
            return Err(Error::RustError("conversation_item_conflict".into()));
        }
    }
    if metadata.provenance.is_some_and(|provenance| {
        provenance
            .keys()
            .any(|id| !items.iter().any(|item| item["id"].as_str() == Some(id)))
    }) {
        return Err(Error::RustError(
            "conversation_execution_context_invalid".into(),
        ));
    }
    let mut bodies = vec![];
    for item in &items {
        let id = item["id"].as_str().unwrap_or_default();
        let origin = metadata
            .provenance
            .and_then(|provenance| provenance.get(id))
            .or_else(|| {
                metadata
                    .output_ids
                    .filter(|ids| ids.contains(id))
                    .and(metadata.output_source)
            });
        if let Some(origin) = origin {
            bodies.push(Body::json(format!("provenance:{id}"), origin)?);
        }
        if let Some(blob) = metadata.images.and_then(|images| images.get(id)) {
            bodies.push(Body::json(format!("image:{id}"), blob)?);
        }
        bodies.push(Body::json(format!("item:{id}"), item)?);
        record.items.push(id.to_owned());
    }
    record.revision += 1;
    context.save_scoped(record, bodies, metadata.escrow).await?;
    Ok(items)
}

async fn handle(
    mut req: Request,
    storage: &Storage,
    env: &Env,
    operation: &ConversationOperation,
) -> Result<Response> {
    let owner = req.headers().get("X-Agents-Owner")?.unwrap_or_default();
    if owner.is_empty() {
        return util::json_error(403, "forbidden", "Owner binding required");
    }
    let path = req.path();
    let method = req.method();
    let mut record = load(storage).await?;
    if record.as_ref().is_some_and(|record| record.owner != owner) {
        return util::json_error(404, "not_found", "Conversation not found");
    }
    operation.emit("state.loaded", json!({"conversation_id":record.as_ref().map(|record| &record.id),
        "response_id":record.as_ref().and_then(|record|record.active_response_id.as_deref()),
        "session_id":record.as_ref().and_then(|record|record.active_session_id.as_deref()),
        "activated":record.as_ref().is_some_and(|record|record.active_activated),
        "pending":record.as_ref().is_some_and(|record|record.pending_expires_at_ms.is_some()),
        "pending_deadline_ms":record.as_ref().and_then(|record|record.pending_expires_at_ms),
        "capacity_generation":record.as_ref().and_then(|record|record.active_capacity_generation.as_deref())}));
    quota::recover(env, &owner, operation, storage).await?;
    let context = PublicationContext {
        storage,
        env,
        operation,
        legacy: record
            .as_ref()
            .filter(|record| record.quota_header_blocks == 0)
            .cloned(),
    };
    reconcile_image_deletes(&context, record.as_ref()).await?;
    if path == "/init" && method == Method::Post {
        if let Some(record) = record {
            if record.deleted {
                return util::json_error(404, "not_found", "Conversation not found");
            }
            return Response::from_json(&record.public());
        }
        let body: Value = req.json().await?;
        let id = body["id"].as_str().unwrap_or_default();
        let meta = body.get("metadata").cloned().unwrap_or(json!({}));
        if !valid_id(id) || !metadata(&meta) {
            return util::json_error(
                400,
                "invalid_request",
                "Invalid conversation id or metadata",
            );
        }
        let items = match normalize_items(body.get("items").unwrap_or(&json!([])), MAX_ITEMS) {
            Ok(items) => items,
            Err(message) => return util::json_error(400, "invalid_request", message),
        };
        reject_server_ids(&items, None)?;
        let mut record = ConversationRecord {
            id: id.into(),
            owner,
            created_at: util::now_ms() / 1000,
            metadata: meta,
            revision: 0,
            deleted: false,
            items: vec![],
            tombstones: BTreeSet::new(),
            committed_responses: BTreeMap::new(),
            admitted_responses: BTreeSet::new(),
            active_response_id: None,
            active_session_id: None,
            active_activated: false,
            active_capacity_generation: None,
            pending_expires_at_ms: None,
            execution_hint: None,
            quota_header_blocks: 0,
        };
        append(&context, &mut record, items, AppendMetadata::default()).await?;
        return Response::from_json(&record.public());
    }
    let Some(mut record) = record.take() else {
        return util::json_error(404, "not_found", "Conversation not found");
    };
    if purge::resume(&context, &mut record).await? {
        if method == Method::Post
            && matches!(path.as_str(), "/responses/release" | "/responses/noappend")
        {
            return purge::release_closed(
                &context,
                &record,
                &req.json().await?,
                path == "/responses/noappend",
            )
            .await;
        }
        return if path == "/purge" && method == Method::Post {
            purge::completed(&record.id)
        } else {
            util::json_error(404, "not_found", "Conversation retained data was purged")
        };
    }
    if path == "/purge" && method == Method::Post {
        return purge::start(&context, &mut record).await;
    }
    if record.deleted && !path.starts_with("/items") && !path.starts_with("/responses/") {
        return util::json_error(404, "not_found", "Conversation not found");
    }
    match (method, path.as_str()) {
        (Method::Post, "/responses/admit") => {
            let body: Value = req.json().await?;
            admit(&context, storage, &mut record, &body).await
        }
        (Method::Post, "/responses/reserve") => {
            let body: Value = req.json().await?;
            reserve_capacity(&context, storage, &mut record, &body).await
        }
        (Method::Post, "/responses/input") => {
            let body: Value = req.json().await?;
            reserve_additional_input(&context, &record, &body).await
        }
        (Method::Post, "/responses/activate") => {
            let body: Value = req.json().await?;
            activate(&context, storage, &mut record, &body).await
        }
        (Method::Post, "/responses/release" | "/responses/noappend") => {
            let body: Value = req.json().await?;
            let response_id = body["response_id"].as_str().unwrap_or_default();
            if !valid_id(response_id) {
                return util::json_error(400, "invalid_request", "Invalid response identity");
            }
            if path == "/responses/noappend" && record.committed_responses.contains_key(response_id)
            {
                return util::json_error(
                    409,
                    "conversation_capacity_generation_closed",
                    "The response was already appended",
                );
            }
            if record.active_response_id.as_deref() == Some(response_id) {
                if let Some(generation) = &record.active_capacity_generation {
                    if body
                        .get("capacity_generation")
                        .and_then(Value::as_str)
                        .is_some_and(|given| given != generation)
                    {
                        return busy(&record);
                    }
                    if !record.committed_responses.contains_key(response_id) {
                        quota::escrow::release(
                            env,
                            &owner,
                            operation,
                            &record.id,
                            &quota::escrow::Lease {
                                response_id: response_id.into(),
                                generation: generation.clone(),
                            },
                        )
                        .await?;
                    }
                }
                record.clear_lease();
                save(&context, &mut record).await?;
            }
            Response::from_json(&json!({"id":record.id,"response_id":response_id,"released":true}))
        }
        (Method::Get, "/metadata") => Response::from_json(&record.public()),
        (Method::Post, "/metadata") => {
            let body: Value = req.json().await?;
            if !body.get("metadata").is_some_and(metadata) {
                return util::json_error(400, "invalid_request", "Invalid metadata");
            }
            record.metadata = body["metadata"].clone();
            save(&context, &mut record).await?;
            Response::from_json(&record.public())
        }
        (Method::Delete, "/metadata") => {
            record.deleted = true;
            record.revision += 1;
            save(&context, &mut record).await?;
            Response::from_json(
                &json!({"id":record.id,"object":"conversation.deleted","deleted":true}),
            )
        }
        (Method::Get, "/snapshot") => {
            let ids = record
                .items
                .iter()
                .filter(|id| !record.tombstones.contains(id.as_str()))
                .map(String::as_str)
                .collect::<Vec<_>>();
            let items = items(storage, &ids).await?;
            let visible_ids = ids.iter().map(|id| (*id).to_owned()).collect::<Vec<_>>();
            let provenance = item_provenance(storage, &visible_ids).await?;
            let previous = record
                .execution_hint
                .as_ref()
                .filter(|hint| hint.adopted_revision == record.revision);
            Response::from_json(
                &json!({"id":record.id,"revision":record.revision,"items":items,"item_provenance":provenance,
                "predecessor_response_id":previous.map(|hint|&hint.response_id),
                "predecessor_session_id":previous.map(|hint|&hint.session_id),
                "predecessor_agent_id":previous.map(|hint|&hint.agent_id),
                "predecessor_pin_hash":previous.map(|hint|&hint.pin_hash)}),
            )
        }
        (Method::Post, "/responses/commit") => {
            let body: Value = req.json().await?;
            let response_id = body["response_id"].as_str().unwrap_or_default();
            if !valid_id(response_id) {
                return util::json_error(400, "invalid_request", "Invalid response append");
            }
            if let Some(revision) = record.committed_responses.get(response_id) {
                return Response::from_json(
                    &json!({"id":record.id,"revision":revision,"response_id":response_id}),
                );
            }
            let receipt = receipt(storage, &record, response_id).await?;
            if record.deleted && receipt.is_none() {
                return util::json_error(404, "not_found", "Conversation not found");
            }
            if record
                .active_response_id
                .as_deref()
                .is_some_and(|active| active != response_id)
            {
                return busy(&record);
            }
            if let Some(expected) = body.get("accepted_revision") {
                if expected.as_u64() != receipt.as_ref().map(|receipt| receipt.revision) {
                    return util::json_error(
                        409,
                        "conversation_revision_mismatch",
                        "Response admission revision does not match",
                    );
                }
            }
            let items = match normalize_items(&body["items"], usize::MAX) {
                Ok(items) => items,
                Err(message) => return util::json_error(400, "invalid_request", message),
            };
            if body["defer_release"] == true
                && (record.active_response_id.as_deref() != Some(response_id)
                    || !record.active_activated
                    || record.active_session_id.as_deref() != body["session_id"].as_str())
            {
                return busy(&record);
            }
            let provenance: BTreeMap<String, ConversationItemProvenance> = match body
                .get("item_provenance")
            {
                Some(value) => match serde_json::from_value(value.clone()) {
                    Ok(provenance) => provenance,
                    Err(_) => {
                        return util::json_error(400, "invalid_request", "Invalid item provenance");
                    }
                },
                None => BTreeMap::new(),
            };
            let output_source: Option<ConversationItemProvenance> = body
                .get("output_provenance")
                .filter(|value| !value.is_null())
                .map(|value| serde_json::from_value(value.clone()))
                .transpose()?;
            let images: BTreeMap<String, crate::workspace_blobs::OwnedBlobRef> = body
                .get("image_assets")
                .map(|value| serde_json::from_value(value.clone()))
                .transpose()?
                .unwrap_or_default();
            if images.len() > crate::response_image_projection::MAX_ASSETS {
                return Err(Error::RustError(
                    "conversation_execution_context_invalid".into(),
                ));
            }
            for (id, blob) in &images {
                blob.validate(&owner)?;
                if !items.iter().any(|item| {
                    item["id"].as_str() == Some(id.as_str())
                        && item["type"] == "image_generation_call"
                        && item.get("result") == Some(&Value::Null)
                }) {
                    return Err(Error::RustError(
                        "conversation_execution_context_invalid".into(),
                    ));
                }
            }
            let mut output_ids = items
                .iter()
                .filter_map(|item| {
                    item["id"]
                        .as_str()
                        .filter(|id| crate::response_limits::service_owned_id(id))
                        .map(str::to_owned)
                })
                .collect::<BTreeSet<_>>();
            let capacity =
                match quota::escrow::known_lease(env, &owner, operation, &record.id, response_id)
                    .await?
                {
                    Some(lease) => {
                        if body
                            .get("capacity_generation")
                            .and_then(Value::as_str)
                            .is_some_and(|generation| generation != lease.generation)
                            || record.active_capacity_generation.as_deref()
                                != Some(lease.generation.as_str())
                            || !record.active_activated
                        {
                            return busy(&record);
                        }
                        output_ids = quota::escrow::validate_commit(
                            env,
                            &owner,
                            operation,
                            &record.id,
                            &lease,
                            &items,
                            body["items"].as_array().ok_or_else(|| {
                                Error::RustError("conversation_execution_context_invalid".into())
                            })?,
                            &provenance,
                            output_source.as_ref(),
                        )
                        .await?;
                        Some(lease)
                    }
                    None => None,
                };
            if capacity.is_some() && images.keys().any(|id| !output_ids.contains(id)) {
                return Err(Error::RustError(
                    "conversation_execution_context_invalid".into(),
                ));
            }
            // Publish the receipt with the same header that exposes its items:
            // an interrupted transport can retry without duplicating model work.
            record
                .committed_responses
                .insert(response_id.into(), record.revision + 1);
            if body["defer_release"] != true {
                record.clear_lease();
            }
            record.execution_hint = execution_hint(
                &body,
                response_id,
                record.revision,
                receipt.as_ref().map(|receipt| receipt.revision),
            )?;
            append(
                &context,
                &mut record,
                items,
                AppendMetadata {
                    provenance: Some(&provenance),
                    output_source: output_source.as_ref(),
                    output_ids: Some(&output_ids),
                    images: Some(&images),
                    escrow: capacity.as_ref(),
                },
            )
            .await?;
            Response::from_json(
                &json!({"id":record.id,"revision":record.revision,"response_id":response_id}),
            )
        }
        (Method::Get, "/items") => list(req, storage, env, &record).await,
        (Method::Post, "/items") => {
            if record.deleted {
                return util::json_error(404, "not_found", "Conversation not found");
            }
            let include = includes(&req)?;
            let body: Value = req.json().await?;
            if serde_json::to_vec(&body)?.len() > MAX_BODY {
                return util::json_error(
                    413,
                    "request_too_large",
                    "Conversation items exceed request limit",
                );
            }
            let items = match normalize_items(&body["items"], MAX_ITEMS) {
                Ok(items) => items,
                Err(message) => return util::json_error(400, "invalid_request", message),
            };
            reject_server_ids(&items, None)?;
            if let Some(active) = record.active_response_id.as_deref() {
                quota::escrow::reject_reserved_inputs(
                    env, &owner, operation, &record.id, active, &items,
                )
                .await?;
            }
            let items = append(&context, &mut record, items, AppendMetadata::default()).await?;
            Response::from_json(&page(
                items
                    .into_iter()
                    .map(|item| project(item, &include))
                    .collect(),
                false,
            ))
        }
        (Method::Get | Method::Delete, _) if path.starts_with("/items/") => {
            let id = path.trim_start_matches("/items/");
            if !record.items.iter().any(|item| item == id) || record.tombstones.contains(id) {
                return util::json_error(404, "not_found", "Conversation item not found");
            }
            if req.method() == Method::Get {
                let value = project(item(storage, id).await?, &includes(&req)?);
                return Response::from_json(&public_value(storage, env, &owner, value).await?);
            }
            if read_chunks(storage, &format!("image:{id}"))
                .await?
                .is_some()
            {
                storage.put(IMAGE_DELETE, vec![id.to_owned()]).await?;
            }
            record.tombstones.insert(id.into());
            record.revision += 1;
            save(&context, &mut record).await?;
            if reconcile_image_deletes(&context, Some(&record))
                .await
                .is_err()
            {
                util::emit(
                    "conversation.image_delete_pending",
                    json!({"conversation_id":record.id,"item_id":id}),
                );
            }
            Response::from_json(&record.public())
        }
        _ => util::json_error(404, "not_found", "Unknown conversation operation"),
    }
}

async fn public_value(
    storage: &Storage,
    env: &Env,
    owner: &str,
    mut value: Value,
) -> Result<Value> {
    let items = value
        .get("data")
        .and_then(Value::as_array)
        .map(|items| items.iter().collect::<Vec<_>>())
        .unwrap_or_else(|| vec![&value]);
    let mut images = crate::response_image_projection::ResponseImages::default();
    for item in items {
        if item["type"] != "image_generation_call"
            || item["status"] != "completed"
            || !item["result"].is_null()
        {
            continue;
        }
        let id = item["id"]
            .as_str()
            .ok_or_else(|| Error::RustError("invalid_image_projection".into()))?;
        if let Some(bytes) = read_chunks(storage, &format!("image:{id}")).await? {
            images.insert_final(owner, id, serde_json::from_slice(&bytes)?)?;
        }
    }
    // Validate the complete page envelope before the first separately funded
    // read, rather than independently allowing a maximum-size image per item.
    images.hydrate(env, owner, &mut value).await?;
    Ok(value)
}

async fn list(
    req: Request,
    storage: &Storage,
    env: &Env,
    record: &ConversationRecord,
) -> Result<Response> {
    let include = includes(&req)?;
    let url = req.url()?;
    let query = url.query_pairs().collect::<BTreeMap<_, _>>();
    let order = query
        .get("order")
        .map(|value| value.as_ref())
        .unwrap_or("desc");
    if !matches!(order, "asc" | "desc") {
        return util::json_error(400, "invalid_request", "Invalid item order");
    }
    let limit = match query.get("limit") {
        Some(value) => match value.parse::<usize>() {
            Ok(1..=100) => value.parse().unwrap_or(20),
            _ => return util::json_error(400, "invalid_request", "Item limit must be 1 to 100"),
        },
        None => 20,
    };
    let mut ids = record.items.iter().collect::<Vec<_>>();
    if order == "desc" {
        ids.reverse();
    }
    let start = match query.get("after") {
        Some(after) => match ids.iter().position(|id| id.as_str() == after.as_ref()) {
            Some(index) => index + 1,
            None => return util::json_error(400, "invalid_request", "Unknown item cursor"),
        },
        None => 0,
    };
    let mut selected = ids
        .into_iter()
        .skip(start)
        .filter(|id| !record.tombstones.contains(id.as_str()));
    let ids = selected
        .by_ref()
        .take(limit)
        .map(|id| id.as_str())
        .collect::<Vec<_>>();
    let items = items(storage, &ids)
        .await?
        .into_iter()
        .map(|item| project(item, &include))
        .collect();
    let value = page(items, selected.next().is_some());
    Response::from_json(&public_value(storage, env, &record.owner, value).await?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record() -> ConversationRecord {
        serde_json::from_value(json!({"id":"conv_a","owner":"owner_a","created_at":0,
            "metadata":{},"revision":1,"deleted":false,"items":[],"tombstones":[]}))
        .unwrap()
    }

    #[test]
    fn hiding_retained_items_never_grows_header_capacity() {
        let mut record = record();
        record.revision = 9;
        record.items = (0..100).map(|n| format!("item_{n}")).collect();
        let initial =
            header_footprint(&record, serde_json::to_vec(&record).unwrap().len()).unwrap();
        for id in record.items.clone() {
            record.tombstones.insert(id);
            record.revision += 1;
            let current =
                header_footprint(&record, serde_json::to_vec(&record).unwrap().len()).unwrap();
            assert!(current <= initial);
        }
        record.deleted = true;
        record.revision += 1;
        assert!(
            header_footprint(&record, serde_json::to_vec(&record).unwrap().len()).unwrap()
                <= initial
        );
    }

    #[test]
    fn pending_deadline_never_expires_an_activated_long_run() {
        let mut record = record();
        record.reserve_lease("resp_a", 1000);
        assert!(!record.pending_expired(120_999));
        assert!(record.pending_expired(121_000));
        record.active_activated = true;
        assert!(!record.pending_expired(u64::MAX));
    }

    #[test]
    fn renewal_extends_only_pending_lease_and_release_clears_all_fences() {
        let mut record = record();
        record.reserve_lease("resp_a", 1000);
        record.reserve_lease("resp_a", 2000);
        assert_eq!(record.pending_expires_at_ms, Some(122_000));
        record.active_activated = true;
        record.active_session_id = Some("session_a".into());
        record.pending_expires_at_ms = None;
        record.reserve_lease("resp_a", u64::MAX);
        assert_eq!(record.active_session_id.as_deref(), Some("session_a"));
        assert_eq!(record.pending_expires_at_ms, None);
        record.clear_lease();
        assert!(record.active_response_id.is_none());
        assert!(!record.active_activated);
    }

    #[test]
    fn legacy_active_lease_has_no_invented_timeout() {
        let mut record = record();
        record.active_response_id = Some("resp_legacy".into());
        record.reserve_lease("resp_legacy", 1000);
        assert!(record.pending_expires_at_ms.is_none());
        assert!(!record.pending_expired(u64::MAX));
    }
    #[test]
    fn metadata_counts_characters_and_checks_values() {
        assert!(metadata(&json!({"label":"中文"})));
        assert!(!metadata(&json!({"label":3})));
        assert!(!metadata(&json!({"x":"a".repeat(513)})));
    }
    #[test]
    fn message_normalization_preserves_explicit_ids() {
        let items =
            normalize_items(&json!([{"id":"item_a","role":"user","content":"hi"}]), 20).unwrap();
        assert_eq!(items[0]["id"], "item_a");
        assert_eq!(items[0]["type"], "message");
        assert_eq!(items[0]["content"][0]["text"], "hi");
    }
    #[test]
    fn mutation_batch_limit_is_twenty() {
        assert!(
            normalize_items(
                &json!(vec![json!({"type":"function_call","id":"x"}); 21]),
                20
            )
            .is_err()
        );
    }

    #[test]
    fn optional_projection_never_mutates_canonical_reasoning() {
        let canonical =
            json!({"id":"rs_a","type":"reasoning","summary":[],"encrypted_content":"opaque"});
        let hidden = project(canonical.clone(), &BTreeSet::new());
        assert!(hidden.get("encrypted_content").is_none());
        let include = BTreeSet::from(["reasoning.encrypted_content".to_owned()]);
        assert_eq!(project(canonical.clone(), &include), canonical);
    }
}
