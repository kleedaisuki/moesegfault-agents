//! Model-independent, owner-scoped conversation authority. The outer API authenticates
//! the caller; this object verifies the resulting owner binding on every operation.
use crate::{
    conversation_routes::ConversationItemProvenance,
    session_storage::{read_chunks, read_many, write_chunks},
    util,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use worker::*;

const HEAD: &str = "conversation.v1";
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
    pending_expires_at_ms: Option<u64>,
    #[serde(default)]
    execution_hint: Option<ExecutionHint>,
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
}

impl DurableObject for ConversationState {
    fn new(state: State, env: Env) -> Self {
        Self { state, env }
    }

    async fn fetch(&self, req: Request) -> Result<Response> {
        let storage = self.state.storage();
        let env = self.env.clone();
        let owner = req.headers().get("X-Agents-Owner")?.unwrap_or_default();
        let headers = req.headers().clone();
        self.state
            .block_concurrency_while(async move {
                crate::conversation_meter::actor(&env, &owner, &headers, async {
                    match handle(req, &storage).await {
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
                        result => result,
                    }
                })
                .await
            })
            .await
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

async fn save(storage: &Storage, record: &ConversationRecord) -> Result<()> {
    write_chunks(storage, HEAD, &serde_json::to_vec(record)?).await
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

async fn admit(
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
    if record.pending_expired(now_ms) {
        record.clear_lease();
    }
    if !record.committed_responses.contains_key(response_id)
        && record
            .active_response_id
            .as_deref()
            .is_some_and(|active| active != response_id)
    {
        return busy(record);
    }
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
            write_chunks(
                storage,
                &format!("admission:{response_id}"),
                &serde_json::to_vec(&accepted)?,
            )
            .await?;
            record.admitted_responses.insert(response_id.into());
            accepted
        }
    };
    if !record.deleted && !record.committed_responses.contains_key(response_id) {
        record.reserve_lease(response_id, now_ms);
        save(storage, record).await?;
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

async fn activate(
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
    record.active_session_id = Some(session_id.into());
    record.active_activated = true;
    record.pending_expires_at_ms = None;
    save(storage, record).await?;
    Response::from_json(
        &json!({"id":record.id,"response_id":response_id,"session_id":session_id,"activated":true}),
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

async fn append(
    storage: &Storage,
    record: &mut ConversationRecord,
    items: Vec<Value>,
    provenance: Option<&BTreeMap<String, ConversationItemProvenance>>,
) -> Result<Vec<Value>> {
    let mut seen = record.items.iter().cloned().collect::<BTreeSet<_>>();
    for item in &items {
        let id = item["id"].as_str().unwrap_or_default();
        if !seen.insert(id.to_owned()) {
            return Err(Error::RustError("conversation_item_conflict".into()));
        }
    }
    if let Some(provenance) = provenance {
        for (id, source) in provenance {
            if !items
                .iter()
                .any(|item| item["id"].as_str() == Some(id.as_str()))
            {
                return Err(Error::RustError(
                    "conversation_execution_context_invalid".into(),
                ));
            }
            write_chunks(
                storage,
                &format!("provenance:{id}"),
                &serde_json::to_vec(source)?,
            )
            .await?;
        }
    }
    // Bodies are immutable. Publishing the header last hides partial writes after
    // interruption and lets a retry replace only previously invisible candidates.
    for item in &items {
        let id = item["id"].as_str().unwrap_or_default();
        write_chunks(storage, &format!("item:{id}"), &serde_json::to_vec(item)?).await?;
        record.items.push(id.to_owned());
    }
    record.revision += 1;
    save(storage, record).await?;
    Ok(items)
}

async fn handle(mut req: Request, storage: &Storage) -> Result<Response> {
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
            pending_expires_at_ms: None,
            execution_hint: None,
        };
        append(storage, &mut record, items, None).await?;
        return Response::from_json(&record.public());
    }
    let Some(mut record) = record.take() else {
        return util::json_error(404, "not_found", "Conversation not found");
    };
    if record.deleted && !path.starts_with("/items") && !path.starts_with("/responses/") {
        return util::json_error(404, "not_found", "Conversation not found");
    }
    match (method, path.as_str()) {
        (Method::Post, "/responses/admit") => {
            let body: Value = req.json().await?;
            admit(storage, &mut record, &body).await
        }
        (Method::Post, "/responses/activate") => {
            let body: Value = req.json().await?;
            activate(storage, &mut record, &body).await
        }
        (Method::Post, "/responses/release") => {
            let body: Value = req.json().await?;
            let response_id = body["response_id"].as_str().unwrap_or_default();
            if !valid_id(response_id) {
                return util::json_error(400, "invalid_request", "Invalid response identity");
            }
            if record.active_response_id.as_deref() == Some(response_id) {
                record.clear_lease();
                save(storage, &record).await?;
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
            save(storage, &record).await?;
            Response::from_json(&record.public())
        }
        (Method::Delete, "/metadata") => {
            record.deleted = true;
            record.revision += 1;
            save(storage, &record).await?;
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
            Response::from_json(&json!({"id":record.id,"revision":record.revision,"items":items}))
        }
        (Method::Post, "/responses/commit") => {
            let body: Value = req.json().await?;
            let response_id = body["response_id"].as_str().unwrap_or_default();
            if !valid_id(response_id) || serde_json::to_vec(&body)?.len() > MAX_BODY {
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
            append(storage, &mut record, items, Some(&provenance)).await?;
            Response::from_json(
                &json!({"id":record.id,"revision":record.revision,"response_id":response_id}),
            )
        }
        (Method::Get, "/items") => list(req, storage, &record).await,
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
            let items = append(storage, &mut record, items, None).await?;
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
                return Response::from_json(&project(item(storage, id).await?, &includes(&req)?));
            }
            record.tombstones.insert(id.into());
            record.revision += 1;
            save(storage, &record).await?;
            Response::from_json(&record.public())
        }
        _ => util::json_error(404, "not_found", "Unknown conversation operation"),
    }
}

async fn list(req: Request, storage: &Storage, record: &ConversationRecord) -> Result<Response> {
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
    Response::from_json(&page(items, selected.next().is_some()))
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
