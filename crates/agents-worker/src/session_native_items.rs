//! Owner-bound handles for private native items; tokens never contain vendor secrets.
use super::*;
use sha2::{Digest, Sha256};

const PREFIX: &str = "mnri1.";
const MAX_HANDLE_BYTES: usize = 8192;

pub(crate) struct ItemContext {
    pub owner: String,
    pub agent: String,
    pub pin_hash: String,
    pub provider_binding: String,
}

impl ItemContext {
    pub(crate) fn new(
        owner: &str,
        agent: &str,
        pinned: &agents_core::PinnedContext,
        config: &ProviderConfig,
    ) -> Result<Self> {
        let value = json!({"owner":owner,"provider":pinned.model.provider,"protocol":config.protocol,
            "base_url":config.base_url,"model":config.model,"credential_id":pinned.model.credential_id});
        Ok(Self {
            owner: owner.into(),
            agent: agent.into(),
            pin_hash: pinned.prefix_hash(),
            provider_binding: digest(&value)?,
        })
    }
}

async fn lookup(
    env: &Env,
    context: &ItemContext,
    public_id: &str,
    token: Option<&str>,
) -> Result<(SessionRecord, Option<ItemRef>)> {
    let reference = token
        .map(|token| {
            ItemRef::open(
                &env.secret("DATA_ENCRYPTION_KEY")?.to_string(),
                &context.owner,
                public_id,
                token,
                &context.agent,
                &context.pin_hash,
                &context.provider_binding,
            )
        })
        .transpose()?;
    let source_id = match &reference {
        Some(reference) => reference.source_session_id.clone(),
        None => {
            crate::response_index::get(env, &context.owner, response_id(public_id)?)
                .await?
                .ok_or_else(|| Error::RustError("native_item_not_found".into()))?
                .session_id
        }
    };
    let source = Store::new(env)
        .get_session(&context.owner, &source_id)
        .await?
        .ok_or_else(|| Error::RustError("native_item_not_found".into()))?;
    if source.agent_id != context.agent || source.pinned.prefix_hash() != context.pin_hash {
        return Err(Error::RustError("native_item_context_mismatch".into()));
    }
    Ok((source, reference))
}

async fn resolve_one(
    env: &Env,
    context: &ItemContext,
    public_id: &str,
    token: Option<&str>,
    local: Option<&SessionInner>,
) -> Result<Value> {
    let (source, reference) = lookup(env, context, public_id, token).await?;
    if let Some(local) = local {
        let is_local = local
            .data
            .borrow()
            .as_ref()
            .is_some_and(|data| data.record.id == source.id);
        if is_local {
            return local
                .native_item_lookup(public_id, reference, &context.provider_binding)
                .await;
        }
    }
    let trace = crate::telemetry::TraceContext::from_traceparent(None);
    let route = crate::routes::RouteContext::new(env, &trace);
    let mut response = crate::routes::forward(
        &route,
        &source,
        Method::Post,
        "/native-item",
        Some(json!({"id":public_id,"binding":context.provider_binding,"reference":reference})),
    )
    .await?;
    match response.status_code() {
        200 => {}
        400 | 403 | 404 => return Err(Error::RustError("native_item_not_found".into())),
        _ => return Err(Error::RustError("native_item_lookup_unavailable".into())),
    }
    response.json().await
}

async fn resolve_items(
    env: &Env,
    context: &ItemContext,
    items: &[Value],
    local: Option<&SessionInner>,
) -> Result<Vec<Value>> {
    let mut resolved = Vec::with_capacity(items.len());
    for item in items {
        if !matches!(item["type"].as_str(), Some("item_reference" | "reasoning")) {
            resolved.push(item.clone());
            continue;
        }
        let id = item["id"].as_str().ok_or_else(invalid)?;
        let value =
            resolve_one(env, context, id, item["encrypted_content"].as_str(), local).await?;
        if item["type"] == "reasoning" && value["type"] != "reasoning" {
            return Err(invalid());
        }
        resolved.push(value);
    }
    Ok(resolved)
}

pub(crate) async fn resolve_input(
    env: &Env,
    context: &ItemContext,
    items: &[Value],
) -> Result<Vec<Value>> {
    resolve_items(env, context, items, None).await
}

pub(crate) fn validation_input(items: &[Value]) -> Result<Vec<Value>> {
    items
        .iter()
        .map(|item| {
            if item["type"] != "reasoning" {
                return Ok(item.clone());
            }
            provider::responses::public_item(item)
                .map_err(Error::RustError)?
                .ok_or_else(invalid)
        })
        .collect()
}

async fn resolve_origin(
    env: &Env,
    context: &ItemContext,
    public_id: &str,
    origin: &crate::conversation_routes::ConversationItemProvenance,
    local: Option<&SessionInner>,
) -> Result<Option<Value>> {
    let source_id = origin.source_session_id.as_deref().ok_or_else(invalid)?;
    let response_id = origin.response_id.as_deref().ok_or_else(invalid)?;
    if origin.provider_binding.as_deref() != Some(context.provider_binding.as_str()) {
        return Ok(None);
    }
    let source = Store::new(env)
        .get_session(&context.owner, source_id)
        .await?
        .ok_or_else(|| Error::RustError("native_item_not_found".into()))?;
    if source.agent_id != context.agent || source.pinned.prefix_hash() != context.pin_hash {
        return Ok(None);
    }
    if let Some(local) = local {
        let is_local = local
            .data
            .borrow()
            .as_ref()
            .is_some_and(|data| data.record.id == source.id);
        if is_local {
            let reference = local.native_item_handle(public_id).await?;
            if reference.response_id != response_id
                || reference.provider_binding != context.provider_binding
            {
                return Err(invalid());
            }
            return local.native_item_read(&reference).await.map(Some);
        }
    }
    let trace = crate::telemetry::TraceContext::from_traceparent(None);
    let route = crate::routes::RouteContext::new(env, &trace);
    let mut result = crate::routes::forward(&route, &source, Method::Post, "/native-item",
        Some(json!({"id":public_id,"binding":context.provider_binding,"reference":null,"trusted_origin":true,"expected_response":response_id}))).await?;
    match result.status_code() {
        200 => result.json().await.map(Some),
        400 | 403 | 404 => Err(Error::RustError("native_item_not_found".into())),
        _ => Err(Error::RustError("native_item_lookup_unavailable".into())),
    }
}

async fn conversation_items(
    env: &Env,
    context: &ItemContext,
    items: &[Value],
    provenance: &std::collections::BTreeMap<
        String,
        crate::conversation_routes::ConversationItemProvenance,
    >,
    base: &str,
    local: Option<&SessionInner>,
) -> Result<Vec<Value>> {
    let mut adapted = Vec::with_capacity(items.len());
    for item in items {
        match item["type"].as_str() {
            Some("reasoning") => {
                if let Some(origin) = item["id"].as_str().and_then(|id| provenance.get(id)) {
                    if let Some(raw) =
                        resolve_origin(env, context, item["id"].as_str().unwrap(), origin, local)
                            .await?
                    {
                        adapted.push(raw);
                    } else if let Some(summary) = super::native::summary_projection(item) {
                        adapted.push(summary);
                    }
                } else {
                    adapted.extend(
                        resolve_items(env, context, std::slice::from_ref(item), local).await?,
                    );
                }
            }
            Some("item_reference") => adapted
                .extend(resolve_items(env, context, std::slice::from_ref(item), local).await?),
            Some("compaction") => {
                let expanded = crate::responses_auxiliary::expand_for_prefix(
                    &env.secret("DATA_ENCRYPTION_KEY")?.to_string(),
                    &context.owner,
                    &context.agent,
                    &context.pin_hash,
                    std::slice::from_ref(item),
                    base,
                )?;
                adapted.extend(expanded.items);
            }
            _ => adapted.extend(super::native::conversation_projection(
                std::slice::from_ref(item),
                provenance,
                &context.provider_binding,
            )?),
        }
    }
    Ok(adapted)
}

pub(crate) async fn resolve_conversation(
    env: &Env,
    context: &ItemContext,
    items: &[Value],
    provenance: &std::collections::BTreeMap<
        String,
        crate::conversation_routes::ConversationItemProvenance,
    >,
    base: &str,
) -> Result<Vec<Value>> {
    conversation_items(env, context, items, provenance, base, None).await
}

pub(super) async fn resolve_local_conversation(
    session: &SessionInner,
    context: &ItemContext,
    items: &[Value],
    provenance: &std::collections::BTreeMap<
        String,
        crate::conversation_routes::ConversationItemProvenance,
    >,
    base: &str,
) -> Result<Vec<Value>> {
    conversation_items(
        &session.env,
        context,
        items,
        provenance,
        base,
        Some(session),
    )
    .await
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(super) struct ItemRef {
    version: u8,
    pub(super) source_session_id: String,
    pub(super) response_id: String,
    pub(super) public_id: String,
    pub(super) generation: String,
    pub(super) index: usize,
    pub(super) agent: String,
    pub(super) pin_hash: String,
    pub(super) provider_binding: String,
    pub(super) digest: String,
}

fn invalid() -> Error {
    Error::RustError("invalid_native_item_handle".into())
}

pub(super) fn invalid_lookup(error: &Error) -> bool {
    matches!(error, Error::RustError(code) if matches!(code.as_str(),
        "invalid_native_item_handle" | "native_item_context_mismatch" | "native_item_not_found" | "response_not_retained"))
}
fn name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}
fn digest(item: &Value) -> Result<String> {
    Ok(format!("{:x}", Sha256::digest(serde_json::to_vec(item)?)))
}

impl ItemRef {
    pub(super) fn new(
        record: &SessionRecord,
        history: &branch::NativeHistoryRef,
        response_id: &str,
        round: u64,
        ordinal: usize,
        item_count: usize,
        item: &Value,
    ) -> Result<Self> {
        let first = history.end.checked_sub(item_count).ok_or_else(invalid)?;
        if ordinal >= item_count {
            return Err(invalid());
        }
        let reference = Self {
            version: 1,
            source_session_id: record.id.clone(),
            response_id: response_id.into(),
            public_id: format!("item_{response_id}_{round}_{ordinal}"),
            generation: history.generation.clone(),
            index: first + ordinal,
            agent: record.agent_id.clone(),
            pin_hash: record.pinned.prefix_hash(),
            provider_binding: history.provider_binding.clone(),
            digest: digest(item)?,
        };
        reference.validate()?;
        Ok(reference)
    }
    fn validate(&self) -> Result<()> {
        if self.version != 1
            || !name(&self.source_session_id)
            || !name(&self.response_id)
            || !name(&self.public_id)
            || !name(&self.agent)
            || [
                &self.generation,
                &self.pin_hash,
                &self.provider_binding,
                &self.digest,
            ]
            .into_iter()
            .any(|value| value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()))
        {
            return Err(invalid());
        }
        Ok(())
    }
    pub(super) fn seal(&self, key: &str, owner: &str) -> Result<String> {
        self.validate()?;
        let aad = format!("responses:native-item:v1:{owner}:{}", self.public_id);
        let encrypted =
            agents_auth::seal(key, &aad, &serde_json::to_vec(self)?).map_err(|_| invalid())?;
        Ok(format!("{PREFIX}{encrypted}"))
    }
    pub(super) fn open(
        key: &str,
        owner: &str,
        public_id: &str,
        token: &str,
        agent: &str,
        pin_hash: &str,
        provider_binding: &str,
    ) -> Result<Self> {
        if !name(public_id) || token.len() > MAX_HANDLE_BYTES {
            return Err(invalid());
        }
        let ciphertext = token.strip_prefix(PREFIX).ok_or_else(invalid)?;
        let aad = format!("responses:native-item:v1:{owner}:{public_id}");
        let bytes = agents_auth::unseal(key, &aad, ciphertext).map_err(|_| invalid())?;
        let reference: Self = serde_json::from_slice(&bytes).map_err(|_| invalid())?;
        reference.validate()?;
        if reference.public_id != public_id
            || reference.agent != agent
            || reference.pin_hash != pin_hash
            || reference.provider_binding != provider_binding
        {
            return Err(Error::RustError("native_item_context_mismatch".into()));
        }
        Ok(reference)
    }
}

pub(super) fn response_id(public_id: &str) -> Result<&str> {
    if !name(public_id) {
        return Err(invalid());
    }
    if let Some((response, index)) = public_id.rsplit_once("_input_") {
        if response.starts_with("resp_") && index.parse::<usize>().is_ok() {
            return Ok(response);
        }
    }
    for prefix in ["msg_", "fc_"] {
        if let Some((response, index)) = public_id
            .strip_prefix(prefix)
            .and_then(|rest| rest.rsplit_once('_'))
        {
            if response.starts_with("resp_") && index.parse::<usize>().is_ok() {
                return Ok(response);
            }
        }
    }
    let rest = public_id.strip_prefix("item_").ok_or_else(invalid)?;
    let mut parts = rest.rsplitn(3, '_');
    parts
        .next()
        .filter(|part| part.parse::<usize>().is_ok())
        .ok_or_else(invalid)?;
    parts
        .next()
        .filter(|part| part.parse::<u64>().is_ok())
        .ok_or_else(invalid)?;
    parts
        .next()
        .filter(|part| part.starts_with("resp_"))
        .ok_or_else(invalid)
}

pub(super) async fn register(storage: &Storage, reference: &ItemRef) -> Result<()> {
    reference.validate()?;
    let key = format!("native:public:{}", reference.public_id);
    if let Some(existing) = storage.get::<ItemRef>(&key).await? {
        if existing != *reference {
            return Err(Error::RustError("native_item_identity_conflict".into()));
        }
        return Ok(());
    }
    storage.put(&key, reference).await
}

impl SessionInner {
    pub(super) async fn register_native_output(
        &self,
        completion: &Completion,
        calls: bool,
    ) -> Result<()> {
        let Some(turn) = &completion.native else {
            return Ok(());
        };
        let (record, history, response_id, round) = {
            let data = self.data.borrow();
            let data = data.as_ref().ok_or_else(invalid)?;
            let Some(run) = data.run.as_ref() else {
                return Ok(());
            };
            let Some(response) = run.response.as_ref() else {
                return Ok(());
            };
            let context = data.native_context.as_ref().ok_or_else(invalid)?;
            (
                data.record.clone(),
                context.history.clone(),
                response.id.clone(),
                run.round.saturating_sub(1),
            )
        };
        for (ordinal, raw) in turn.output.iter().enumerate() {
            let ordinary = matches!(raw["type"].as_str(), Some("message" | "reasoning"));
            if ordinary == calls {
                continue;
            }
            let reference = ItemRef::new(
                &record,
                &history,
                &response_id,
                round,
                ordinal,
                turn.output.len(),
                raw,
            )?;
            if calls {
                let data = self.data.borrow();
                let visible = data
                    .as_ref()
                    .and_then(|data| data.run.as_ref())
                    .and_then(|run| run.response.as_ref())
                    .is_some_and(|response| {
                        response
                            .output
                            .iter()
                            .any(|item| item["id"] == reference.public_id)
                    });
                if !visible {
                    continue;
                }
            }
            register(&self.state.storage(), &reference).await?;
        }
        Ok(())
    }

    async fn retained_response(&self, id: &str) -> Result<ResponseRun> {
        let response = self
            .read_response(id)
            .await?
            .ok_or_else(|| Error::RustError("response_not_retained".into()))?;
        if response.deleted || !response.request.store {
            return Err(Error::RustError("response_not_retained".into()));
        }
        Ok(response)
    }

    pub(super) async fn response_retained_route(&self, mut request: Request) -> Result<Response> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Lookup {
            id: String,
        }
        let input: Lookup = request.json().await?;
        match self.retained_response(&input.id).await {
            Ok(_) => Response::from_json(&json!({"retained":true})),
            Err(_) => Response::error("response_not_retained", 404),
        }
    }
    pub(super) async fn native_item_handle(&self, public_id: &str) -> Result<ItemRef> {
        if !name(public_id) {
            return Err(invalid());
        }
        self.state
            .storage()
            .get(&format!("native:public:{public_id}"))
            .await?
            .ok_or_else(|| Error::RustError("native_item_not_found".into()))
    }

    pub(super) async fn native_item_token(&self, public_id: &str) -> Result<String> {
        let key = format!("native:sealed:{public_id}");
        let storage = self.state.storage();
        if let Some(token) = storage.get::<String>(&key).await? {
            return Ok(token);
        }
        let reference = self.native_item_handle(public_id).await?;
        let owner = self
            .data
            .borrow()
            .as_ref()
            .ok_or_else(invalid)?
            .record
            .owner_id
            .clone();
        let token = reference.seal(&self.env.secret("DATA_ENCRYPTION_KEY")?.to_string(), &owner)?;
        // AES-GCM nonces remain random; publication retries reuse the first
        // durable ciphertext rather than changing canonical response bytes.
        storage.put(&key, &token).await?;
        Ok(token)
    }

    pub(super) async fn native_item_read(&self, reference: &ItemRef) -> Result<Value> {
        reference.validate()?;
        {
            let data = self.data.borrow();
            let record = &data.as_ref().ok_or_else(invalid)?.record;
            if record.id != reference.source_session_id
                || record.agent_id != reference.agent
                || record.pinned.prefix_hash() != reference.pin_hash
            {
                return Err(invalid());
            }
        }
        let actual = self.native_item_handle(&reference.public_id).await?;
        if actual != *reference {
            return Err(invalid());
        }
        let bytes = read_chunks(
            &self.state.storage(),
            &format!("native:{}:entry:{}", reference.generation, reference.index),
        )
        .await?
        .ok_or_else(|| Error::RustError("native_item_not_found".into()))?;
        let item: Value = serde_json::from_slice(&bytes)?;
        if digest(&item)? != reference.digest {
            return Err(Error::RustError("native_item_integrity_error".into()));
        }
        Ok(item)
    }

    pub(super) async fn native_item_route(&self, request: Request) -> Result<Response> {
        match self.native_item_route_inner(request).await {
            Err(error) if invalid_lookup(&error) => Response::error("native_item_not_found", 404),
            result => result,
        }
    }

    async fn native_item_route_inner(&self, mut request: Request) -> Result<Response> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Lookup {
            id: String,
            binding: String,
            reference: Option<ItemRef>,
            #[serde(default)]
            trusted_origin: bool,
            expected_response: Option<String>,
        }
        let input: Lookup = request.json().await?;
        if input.trusted_origin {
            let reference = self.native_item_handle(&input.id).await?;
            if Some(reference.response_id.as_str()) != input.expected_response.as_deref()
                || reference.provider_binding != input.binding
            {
                return Err(invalid());
            }
            return Response::from_json(&self.native_item_read(&reference).await?);
        }
        Response::from_json(
            &self
                .native_item_lookup(&input.id, input.reference, &input.binding)
                .await?,
        )
    }

    async fn native_item_lookup(
        &self,
        public_id: &str,
        reference: Option<ItemRef>,
        expected_binding: &str,
    ) -> Result<Value> {
        let reference = match reference {
            Some(reference) => reference,
            None => {
                let response = self.retained_response(response_id(public_id)?).await?;
                match self
                    .state
                    .storage()
                    .get::<ItemRef>(&format!("native:public:{public_id}"))
                    .await?
                {
                    Some(reference) => reference,
                    None => {
                        let mut item = response
                            .input_items
                            .iter()
                            .chain(response.output.iter())
                            .find(|item| item["id"] == public_id)
                            .cloned()
                            .ok_or_else(|| Error::RustError("native_item_not_found".into()))?;
                        if matches!(
                            item["type"].as_str(),
                            Some("reasoning" | "compaction" | "item_reference")
                        ) {
                            return Err(invalid());
                        }
                        if !crate::client_tool_items::is_call(&item)
                            && !crate::client_tool_items::is_output(&item)
                        {
                            if let Some(fields) = item.as_object_mut() {
                                fields.remove("id");
                            }
                        }
                        return Ok(item);
                    }
                }
            }
        };
        if reference.public_id != public_id || reference.provider_binding != expected_binding {
            return Err(Error::RustError("native_item_context_mismatch".into()));
        }
        self.native_item_read(&reference).await
    }

    pub(super) async fn resolve_owned_native_input(
        &self,
        config: &ProviderConfig,
        items: &[Value],
    ) -> Result<Vec<Value>> {
        let record = self
            .data
            .borrow()
            .as_ref()
            .ok_or_else(invalid)?
            .record
            .clone();
        let context = ItemContext {
            owner: record.owner_id.clone(),
            agent: record.agent_id.clone(),
            pin_hash: record.pinned.prefix_hash(),
            provider_binding: super::native::provider_binding(&record, config)?,
        };
        resolve_items(&self.env, &context, items, Some(self)).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
    fn reference() -> ItemRef {
        ItemRef {
            version: 1,
            source_session_id: "session".into(),
            response_id: "resp_one".into(),
            public_id: "item_resp_one_0_2".into(),
            generation: "a".repeat(64),
            index: 2,
            agent: "agent".into(),
            pin_hash: "b".repeat(64),
            provider_binding: "c".repeat(64),
            digest: "d".repeat(64),
        }
    }
    #[test]
    fn handles_bind_owner_public_identity_and_effective_context() {
        let reference = reference();
        let key = URL_SAFE_NO_PAD.encode([7u8; 32]);
        let token = reference.seal(&key, "alice").unwrap();
        assert!(
            ItemRef::open(
                &key,
                "alice",
                &reference.public_id,
                &token,
                "agent",
                &reference.pin_hash,
                &reference.provider_binding
            )
            .unwrap()
                == reference
        );
        for (owner, id, agent, pin, binding) in [
            (
                "bob",
                reference.public_id.as_str(),
                "agent",
                reference.pin_hash.as_str(),
                reference.provider_binding.as_str(),
            ),
            (
                "alice",
                "item_resp_other_0_2",
                "agent",
                reference.pin_hash.as_str(),
                reference.provider_binding.as_str(),
            ),
            (
                "alice",
                reference.public_id.as_str(),
                "other",
                reference.pin_hash.as_str(),
                reference.provider_binding.as_str(),
            ),
            (
                "alice",
                reference.public_id.as_str(),
                "agent",
                "wrong",
                reference.provider_binding.as_str(),
            ),
            (
                "alice",
                reference.public_id.as_str(),
                "agent",
                reference.pin_hash.as_str(),
                "wrong",
            ),
        ] {
            assert!(ItemRef::open(&key, owner, id, &token, agent, pin, binding).is_err());
        }
        assert!(!token.contains("session"));
        assert!(
            agents_auth::unseal(
                &key,
                "responses:compaction:alice:item_resp_one_0_2",
                token.trim_start_matches(PREFIX)
            )
            .is_err()
        );
    }
    #[test]
    fn public_reference_routing_is_exact_and_bounded() {
        assert_eq!(
            response_id("item_resp_under_score_12_34").unwrap(),
            "resp_under_score"
        );
        for id in ["resp_one_input_2", "msg_resp_one_3", "fc_resp_one_4"] {
            assert_eq!(response_id(id).unwrap(), "resp_one");
        }
        for id in [
            "upstream",
            "item_foreign_0_1",
            "item_resp_one_bad_1",
            "item_resp_one_0_1/../other",
        ] {
            assert!(response_id(id).is_err());
        }
    }
}
