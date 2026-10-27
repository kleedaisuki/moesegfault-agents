//! Prepaid logical output capacity is distinct from money and physical storage.

use super::*;
use crate::{conversation_routes::ConversationItemProvenance, response_limits::OutputLimits};
use agents_core::RuntimeProfile;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct Lease {
    pub response_id: String,
    pub generation: String,
}

#[derive(Clone, Default, Serialize, Deserialize)]
pub(crate) struct Request {
    #[serde(default)]
    pub input_items: Vec<Value>,
    #[serde(default)]
    pub input_provenance: BTreeMap<String, ConversationItemProvenance>,
    pub agent_id: Option<String>,
    pub pin_hash: Option<String>,
    pub profile: Option<RuntimeProfile>,
    #[serde(default)]
    pub warmup: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
struct Input {
    id: String,
    body: Payload,
    provenance: Option<Payload>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Terms {
    initial: Vec<Input>,
    additional: BTreeMap<String, Vec<Input>>,
    request: Metadata,
    total_bytes: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Metadata {
    session_id: String,
    agent_id: Option<String>,
    pin_hash: Option<String>,
    profile: Option<RuntimeProfile>,
    #[serde(default)]
    warmup: bool,
}

#[derive(Clone, Debug, Deserialize)]
struct Row {
    generation: String,
    session_id: String,
    fingerprint: String,
    terms_json: String,
    state: String,
    remaining_bytes: u64,
}

pub(crate) struct Plan {
    terms: Terms,
    fingerprint: String,
}
impl Plan {
    pub(crate) fn required_bytes(&self) -> u64 {
        self.terms.total_bytes
    }
}

pub(crate) fn plan(
    owner: &str,
    conversation: &str,
    response: &str,
    session: &str,
    request: &Request,
) -> Result<Plan> {
    let mut terms = Terms {
        initial: inputs(
            conversation,
            &request.input_items,
            &request.input_provenance,
        )?,
        additional: BTreeMap::new(),
        request: Metadata {
            session_id: session.into(),
            agent_id: request.agent_id.clone(),
            pin_hash: request.pin_hash.clone(),
            profile: request.profile,
            warmup: request.warmup,
        },
        total_bytes: 0,
    };
    terms.total_bytes = quote(owner, conversation, response, &terms)?;
    let fingerprint = format!("{:x}", Sha256::digest(serde_json::to_vec(&terms)?));
    Ok(Plan { terms, fingerprint })
}

pub(crate) enum Prepared {
    Held(Lease),
    Insufficient {
        required_bytes: u64,
        available_bytes: u64,
    },
}

fn checked_add(left: u64, right: u64) -> Result<u64> {
    left.checked_add(right)
        .ok_or_else(|| fail("conversation_capacity_size_overflow"))
}
fn checked_mul(left: u64, right: u64) -> Result<u64> {
    left.checked_mul(right)
        .ok_or_else(|| fail("conversation_capacity_size_overflow"))
}
fn json_bytes(value: &impl Serialize) -> Result<u64> {
    Ok(serde_json::to_vec(value)?.len() as u64)
}

fn inputs(
    conversation: &str,
    items: &[Value],
    provenance: &BTreeMap<String, ConversationItemProvenance>,
) -> Result<Vec<Input>> {
    let mut ids = BTreeSet::new();
    let mut result = vec![];
    for item in items {
        let id = item["id"]
            .as_str()
            .ok_or_else(|| fail("conversation_capacity_input_identity_missing"))?;
        if !ids.insert(id) {
            return Err(fail("conversation_item_conflict"));
        }
        let encoded = serde_json::to_vec(item)?;
        result.push(Input {
            id: id.into(),
            body: Payload::body(conversation, &format!("item:{id}"), &encoded)?,
            provenance: provenance
                .get(id)
                .map(|source| {
                    serde_json::to_vec(source)
                        .map_err(Error::from)
                        .and_then(|bytes| {
                            Payload::body(conversation, &format!("provenance:{id}"), &bytes)
                        })
                })
                .transpose()?,
        });
    }
    if provenance.keys().any(|id| !ids.contains(id.as_str())) {
        return Err(fail("conversation_execution_context_invalid"));
    }
    Ok(result)
}

fn same_inputs(left: &[Input], right: &[Input]) -> bool {
    left == right
}

fn claims<'a>(terms: &'a Terms) -> impl Iterator<Item = &'a Input> {
    terms
        .initial
        .iter()
        .chain(terms.additional.values().flatten())
}

fn quote(owner: &str, conversation: &str, response: &str, terms: &Terms) -> Result<u64> {
    let limits = OutputLimits::for_response(response).map_err(fail)?;
    let count = if terms.request.warmup {
        0
    } else {
        limits.max_items as u64
    };
    let wire = if terms.request.warmup {
        2
    } else {
        limits.max_bytes as u64
    };
    let ids = checked_mul(count, limits.max_id_bytes as u64)?.min(wire);
    let mut known = 0;
    let mut known_ids = 0;
    for claim in claims(terms) {
        known = checked_add(known, claim.body.bytes)?;
        if let Some(provenance) = &claim.provenance {
            known = checked_add(known, provenance.bytes)?;
        }
        known_ids = checked_add(known_ids, claim.id.len() as u64 + 3)?;
    }
    let item_fixed = conversation.len() as u64 + "body:item::".len() as u64 + 64 + OBJECT_OVERHEAD;
    // Conversation normalization may add the fixed message status field or wrap
    // a string message in one text part. These are structural JSON bytes, not a
    // guessed token/character expansion ratio.
    let normalization = json_bytes(&serde_json::json!({"status":"completed"}))? - 1
        + json_bytes(&serde_json::json!([{"type":"input_text","text":""}]))?
        - 2;
    let output = checked_add(
        wire,
        checked_add(ids, checked_mul(count, item_fixed + normalization)?)?,
    )?;
    let provenance = ConversationItemProvenance {
        source_session_id: Some(terms.request.session_id.clone()),
        response_id: Some(response.into()),
        provider_binding: Some("f".repeat(64)),
    };
    let provenance_fixed = json_bytes(&provenance)?
        + conversation.len() as u64
        + "body:provenance::".len() as u64
        + 64
        + OBJECT_OVERHEAD;
    let provenance = checked_add(checked_mul(count, provenance_fixed)?, ids)?;
    let image_min = json_bytes(
        &serde_json::json!({"id":"x".repeat(limits.min_id_bytes),"type":"image_generation_call","result":null}),
    )?;
    let image_count = count
        .min(crate::response_image_projection::MAX_ASSETS as u64)
        .min(wire.saturating_sub(1) / (image_min + 1));
    let image_ref = crate::workspace_blobs::OwnedBlobRef {
        account_id: owner.into(),
        object_key: format!(
            "owner-blobs/{}/image/{}/{}",
            "f".repeat(64),
            "f".repeat(64),
            uuid::Uuid::nil()
        ),
        sha256: "f".repeat(64),
        bytes: 16 * 1024 * 1024,
        content_type: "image/jpeg".into(),
        kind: crate::workspace_blobs::BlobKind::ImageArtifact,
    };
    let image_fixed = json_bytes(&image_ref)?
        + conversation.len() as u64
        + "body:image::".len() as u64
        + 64
        + OBJECT_OVERHEAD;
    let image = checked_add(
        checked_mul(image_count, image_fixed)?,
        checked_mul(image_count, limits.max_id_bytes as u64)?.min(wire),
    )?;
    let agent = terms
        .request
        .agent_id
        .clone()
        .unwrap_or_else(|| "a".repeat(128));
    let pin = terms
        .request
        .pin_hash
        .clone()
        .unwrap_or_else(|| "p".repeat(256));
    let committed = BTreeMap::from([(response, u64::MAX)]);
    let metadata = serde_json::json!({"committed_responses":committed,
        "execution_hint":{"response_id":response,"session_id":terms.request.session_id,"agent_id":agent,"pin_hash":pin,"adopted_revision":u64::MAX},
        "quota_header_blocks":u64::MAX,"revision":u64::MAX});
    let slots = checked_mul(
        2,
        checked_add(known_ids, checked_add(ids, checked_mul(3, count)?)?)?,
    )?;
    // One block covers arbitrary alignment with header growth paid by other
    // legitimate mutations while this Response is executing.
    let header = checked_add(slots, json_bytes(&metadata)?)?
        .div_ceil(HEADER_BLOCK_BYTES)
        .checked_add(1)
        .and_then(|blocks| blocks.checked_mul(HEADER_BLOCK_BYTES))
        .ok_or_else(|| fail("conversation_capacity_size_overflow"))?;
    checked_add(
        known,
        checked_add(
            output,
            checked_add(image, checked_add(provenance, header)?)?,
        )?,
    )
}

async fn row(
    env: &Env,
    owner: &str,
    operation: &ConversationOperation,
    conversation: &str,
    response: &str,
) -> Result<Option<Row>> {
    Ok(operation.query(env,owner,"capacity-output-read",env.d1("DB")?
        .prepare("SELECT generation,session_id,fingerprint,terms_json,state,remaining_bytes FROM conversation_response_capacity WHERE account_id=? AND conversation_id=? AND response_id=?")
        .bind(&[owner.into(),conversation.into(),response.into()])?).await?.results::<Row>()?.into_iter().next())
}

pub(crate) async fn prepare(
    env: &Env,
    owner: &str,
    operation: &ConversationOperation,
    conversation: &str,
    response: &str,
    session: &str,
    generation: &str,
    plan: &Plan,
) -> Result<Prepared> {
    if let Some(existing) = row(env, owner, operation, conversation, response).await? {
        if existing.state != "reserved"
            || existing.session_id != session
            || existing.generation != generation
        {
            return Err(fail("conversation_capacity_generation_closed"));
        }
        let terms: Terms = serde_json::from_str(&existing.terms_json)?;
        validate_inputs(&terms, &plan.terms.initial)?;
        return Ok(Prepared::Held(Lease {
            response_id: response.into(),
            generation: existing.generation,
        }));
    }
    let terms = &plan.terms;
    let fingerprint = &plan.fingerprint;
    let limit = space::owner_limits(env, owner).await?.bytes;
    operation.query(env,owner,"capacity-output-account",env.d1("DB")?
        .prepare("INSERT INTO conversation_capacity_accounts(account_id) VALUES(?) ON CONFLICT(account_id) DO NOTHING")
        .bind(&[owner.into()])?).await?;
    let encoded = serde_json::to_string(&terms)?;
    operation.query(env,owner,"capacity-output-reserve",env.d1("DB")?
        .prepare("INSERT INTO conversation_response_capacity(account_id,conversation_id,response_id,generation,session_id,fingerprint,terms_json,state,remaining_bytes,updated_at_ms) SELECT ?1,?2,?3,?4,?5,?6,?7,'reserved',CAST(?8 AS INTEGER),CAST(?9 AS INTEGER) WHERE EXISTS(SELECT 1 FROM conversation_capacity_accounts WHERE account_id=?1 AND reserved_bytes+retained_bytes<=CAST(?10 AS INTEGER)-CAST(?8 AS INTEGER)) ON CONFLICT DO NOTHING")
        .bind(&[owner.into(),conversation.into(),response.into(),generation.into(),session.into(),fingerprint.as_str().into(),encoded.as_str().into(),terms.total_bytes.to_string().into(),util::now_ms().to_string().into(),limit.to_string().into()])?).await?;
    if let Some(existing) = row(env, owner, operation, conversation, response).await? {
        if existing.state != "reserved"
            || existing.fingerprint != *fingerprint
            || existing.session_id != session
        {
            return Err(fail("conversation_capacity_generation_closed"));
        }
        return Ok(Prepared::Held(Lease {
            response_id: response.into(),
            generation: existing.generation,
        }));
    }
    let usage = usage(env, owner, operation).await?;
    Ok(Prepared::Insufficient {
        required_bytes: terms.total_bytes,
        available_bytes: usage
            .limit_bytes
            .saturating_sub(usage.reserved_bytes.saturating_add(usage.retained_bytes)),
    })
}

fn validate_inputs(terms: &Terms, incoming: &[Input]) -> Result<()> {
    for required in &terms.initial {
        if !incoming.iter().any(|claim| {
            claim.id == required.id
                && same_inputs(std::slice::from_ref(claim), std::slice::from_ref(required))
        }) {
            return Err(fail("conversation_capacity_input_changed"));
        }
    }
    for claim in incoming {
        if !claims(terms).any(|allowed| {
            allowed.id == claim.id
                && same_inputs(std::slice::from_ref(allowed), std::slice::from_ref(claim))
        }) {
            return Err(fail("conversation_capacity_input_unreserved"));
        }
    }
    Ok(())
}

pub(crate) async fn known_lease(
    env: &Env,
    owner: &str,
    operation: &ConversationOperation,
    conversation: &str,
    response: &str,
) -> Result<Option<Lease>> {
    Ok(row(env, owner, operation, conversation, response)
        .await?
        .map(|row| Lease {
            response_id: response.into(),
            generation: row.generation,
        }))
}

pub(crate) async fn released(
    env: &Env,
    owner: &str,
    operation: &ConversationOperation,
    conversation: &str,
    response: &str,
    generation: &str,
) -> Result<bool> {
    Ok(row(env, owner, operation, conversation, response)
        .await?
        .is_some_and(|row| row.generation == generation && row.state == "released"))
}

pub(crate) async fn validate_generation(
    env: &Env,
    owner: &str,
    operation: &ConversationOperation,
    conversation: &str,
    lease: &Lease,
    session: Option<&str>,
) -> Result<()> {
    let existing = row(env, owner, operation, conversation, &lease.response_id)
        .await?
        .ok_or_else(|| fail("conversation_capacity_reservation_missing"))?;
    if existing.generation != lease.generation
        || existing.state != "reserved"
        || session.is_some_and(|session| session != existing.session_id)
    {
        return Err(fail("conversation_capacity_generation_closed"));
    }
    Ok(())
}

pub(crate) async fn allocate(
    env: &Env,
    owner: &str,
    operation: &ConversationOperation,
    conversation: &str,
    lease: &Lease,
    payloads: &[Payload],
) -> Result<()> {
    let encoded = serde_json::to_string(payloads)?;
    operation.query(env,owner,"capacity-output-exchange",env.d1("DB")?
        .prepare("WITH desired AS MATERIALIZED (SELECT json_extract(value,'$.object_id') AS object_id,json_extract(value,'$.digest') AS digest,CAST(json_extract(value,'$.bytes') AS INTEGER) AS bytes FROM json_each(?5)),needed AS MATERIALIZED (SELECT COALESCE(SUM(d.bytes),0) AS bytes FROM desired d LEFT JOIN conversation_capacity_objects o ON o.account_id=?1 AND o.conversation_id=?2 AND o.object_id=d.object_id WHERE o.state IS NULL OR o.state='released') UPDATE conversation_response_capacity SET remaining_bytes=remaining_bytes-(SELECT bytes FROM needed),action='allocate',plan_json=?5,updated_at_ms=CAST(?6 AS INTEGER) WHERE account_id=?1 AND conversation_id=?2 AND response_id=?3 AND generation=?4 AND state='reserved' AND remaining_bytes>=(SELECT bytes FROM needed) AND NOT EXISTS(SELECT 1 FROM desired d JOIN conversation_capacity_objects o ON o.account_id=?1 AND o.conversation_id=?2 AND o.object_id=d.object_id WHERE o.digest!=d.digest OR o.bytes!=d.bytes)")
        .bind(&[owner.into(),conversation.into(),lease.response_id.as_str().into(),lease.generation.as_str().into(),encoded.as_str().into(),util::now_ms().to_string().into()])?).await?;
    validate_generation(env, owner, operation, conversation, lease, None).await?;
    let rows=operation.query(env,owner,"capacity-output-materialized",env.d1("DB")?
        .prepare("SELECT o.conversation_id,o.object_id,o.digest,o.bytes,o.state FROM conversation_capacity_objects o JOIN json_each(?3) j ON o.object_id=json_extract(j.value,'$.object_id') WHERE o.account_id=?1 AND o.conversation_id=?2")
        .bind(&[owner.into(),conversation.into(),encoded.as_str().into()])?).await?.results::<Reservation>()?;
    for payload in payloads {
        let actual = rows
            .iter()
            .find(|row| row.object_id == payload.object_id)
            .ok_or_else(|| fail("conversation_capacity_envelope_exceeded"))?;
        verify(payload, actual)?;
        if actual.state == CapacityState::Released {
            return Err(fail("conversation_capacity_envelope_exceeded"));
        }
    }
    Ok(())
}

pub(crate) async fn rollback(
    env: &Env,
    owner: &str,
    operation: &ConversationOperation,
    conversation: &str,
    lease: &Lease,
    payloads: &[Payload],
) -> Result<()> {
    let encoded = serde_json::to_string(payloads)?;
    operation.query(env,owner,"capacity-output-rollback",env.d1("DB")?
        .prepare("UPDATE conversation_response_capacity SET remaining_bytes=remaining_bytes+(SELECT COALESCE(SUM(bytes),0) FROM conversation_capacity_objects o WHERE o.account_id=?1 AND o.conversation_id=?2 AND o.state='reserved' AND o.escrow_response_id=?3 AND o.escrow_generation=?4 AND EXISTS(SELECT 1 FROM json_each(?5) j WHERE o.object_id=json_extract(j.value,'$.object_id') AND o.digest=json_extract(j.value,'$.digest') AND o.bytes=CAST(json_extract(j.value,'$.bytes') AS INTEGER))),action='rollback',plan_json=?5,updated_at_ms=CAST(?6 AS INTEGER) WHERE account_id=?1 AND conversation_id=?2 AND response_id=?3 AND generation=?4 AND state='reserved'")
        .bind(&[owner.into(),conversation.into(),lease.response_id.as_str().into(),lease.generation.as_str().into(),encoded.as_str().into(),util::now_ms().to_string().into()])?).await?;
    Ok(())
}

pub(crate) async fn convert(
    env: &Env,
    owner: &str,
    operation: &ConversationOperation,
    conversation: &str,
    lease: &Lease,
) -> Result<()> {
    operation.query(env,owner,"capacity-output-refund",env.d1("DB")?
        .prepare("UPDATE conversation_response_capacity SET state='converted',remaining_bytes=0,action='none',plan_json='[]',updated_at_ms=CAST(?5 AS INTEGER) WHERE account_id=?1 AND conversation_id=?2 AND response_id=?3 AND generation=?4 AND state='reserved'")
        .bind(&[owner.into(),conversation.into(),lease.response_id.as_str().into(),lease.generation.as_str().into(),util::now_ms().to_string().into()])?).await?;
    Ok(())
}

/// The authority has proved no canonical append exists for this generation.
pub(crate) async fn release(
    env: &Env,
    owner: &str,
    operation: &ConversationOperation,
    conversation: &str,
    lease: &Lease,
) -> Result<()> {
    operation.query(env,owner,"capacity-output-close",env.d1("DB")?
        .prepare("UPDATE conversation_response_capacity SET state='released',remaining_bytes=0,action='release',plan_json='[]',updated_at_ms=CAST(?5 AS INTEGER) WHERE account_id=?1 AND conversation_id=?2 AND response_id=?3 AND generation=?4 AND state='reserved'")
        .bind(&[owner.into(),conversation.into(),lease.response_id.as_str().into(),lease.generation.as_str().into(),util::now_ms().to_string().into()])?).await?;
    Ok(())
}

pub(crate) async fn validate_commit(
    env: &Env,
    owner: &str,
    operation: &ConversationOperation,
    conversation: &str,
    lease: &Lease,
    items: &[Value],
    raw_items: &[Value],
    provenance: &BTreeMap<String, ConversationItemProvenance>,
    output_provenance: Option<&ConversationItemProvenance>,
) -> Result<BTreeSet<String>> {
    let row = row(env, owner, operation, conversation, &lease.response_id)
        .await?
        .ok_or_else(|| fail("conversation_capacity_reservation_missing"))?;
    if row.generation != lease.generation || row.state != "reserved" {
        return Err(fail("conversation_capacity_generation_closed"));
    }
    let terms: Terms = serde_json::from_str(&row.terms_json)?;
    let allowed = claims(&terms)
        .map(|claim| (claim.id.as_str(), claim))
        .collect::<BTreeMap<_, _>>();
    let mut new_inputs = vec![];
    for item in items {
        let id = item["id"]
            .as_str()
            .ok_or_else(|| fail("conversation_capacity_input_identity_missing"))?;
        if allowed.contains_key(id) {
            new_inputs.push(item.clone());
        }
    }
    let input_provenance = provenance
        .iter()
        .filter(|(id, _)| allowed.contains_key(id.as_str()))
        .map(|(id, value)| (id.clone(), value.clone()))
        .collect();
    validate_inputs(
        &terms,
        &inputs(conversation, &new_inputs, &input_provenance)?,
    )?;
    let outputs = raw_items
        .iter()
        .filter(|item| {
            !item["id"]
                .as_str()
                .is_some_and(|id| allowed.contains_key(id))
        })
        .collect::<Vec<_>>();
    let limits = OutputLimits::for_response(&lease.response_id).map_err(fail)?;
    if (terms.request.warmup && !outputs.is_empty())
        || outputs.len() > limits.max_items
        || serde_json::to_vec(&outputs)?.len() > limits.max_bytes
    {
        return Err(fail("response_output_limit"));
    }
    let output_ids = outputs
        .iter()
        .filter_map(|item| item["id"].as_str().map(str::to_owned))
        .collect();
    for item in outputs {
        limits.item_bytes(&lease.response_id, item).map_err(fail)?;
    }
    if let Some(source) = output_provenance {
        validate_source(source, &row.session_id, &lease.response_id)?;
    }
    for (id, source) in provenance {
        if !allowed.contains_key(id.as_str()) {
            validate_source(source, &row.session_id, &lease.response_id)?;
        }
    }
    Ok(output_ids)
}

fn validate_source(
    source: &ConversationItemProvenance,
    session: &str,
    response: &str,
) -> Result<()> {
    if source.source_session_id.as_deref() != Some(session)
        || source.response_id.as_deref() != Some(response)
        || source.provider_binding.as_ref().is_some_and(|binding| {
            binding.len() != 64 || !binding.bytes().all(|byte| byte.is_ascii_hexdigit())
        })
    {
        return Err(fail("conversation_execution_context_invalid"));
    }
    Ok(())
}

pub(crate) async fn reject_reserved_inputs(
    env: &Env,
    owner: &str,
    operation: &ConversationOperation,
    conversation: &str,
    response: &str,
    items: &[Value],
) -> Result<()> {
    let Some(row) = row(env, owner, operation, conversation, response).await? else {
        return Ok(());
    };
    if row.state != "reserved" {
        return Ok(());
    }
    let terms: Terms = serde_json::from_str(&row.terms_json)?;
    if items.iter().any(|item| {
        item["id"]
            .as_str()
            .is_some_and(|id| claims(&terms).any(|claim| claim.id == id))
    }) {
        return Err(fail("conversation_item_reserved"));
    }
    Ok(())
}

pub(crate) async fn extend(
    env: &Env,
    owner: &str,
    operation: &ConversationOperation,
    conversation: &str,
    lease: &Lease,
    submission: &str,
    request: &Request,
) -> Result<Prepared> {
    let row = row(env, owner, operation, conversation, &lease.response_id)
        .await?
        .ok_or_else(|| fail("conversation_capacity_reservation_missing"))?;
    if row.generation != lease.generation || row.state != "reserved" {
        return Err(fail("conversation_capacity_generation_closed"));
    }
    let mut terms: Terms = serde_json::from_str(&row.terms_json)?;
    let incoming = inputs(
        conversation,
        &request.input_items,
        &request.input_provenance,
    )?;
    if let Some(previous) = terms.additional.get(submission) {
        if !same_inputs(previous, &incoming) {
            return Err(fail("conversation_capacity_input_changed"));
        }
        return Ok(Prepared::Held(lease.clone()));
    }
    if incoming
        .iter()
        .any(|input| claims(&terms).any(|previous| previous.id == input.id))
    {
        return Err(fail("conversation_item_conflict"));
    }
    let old = terms.total_bytes;
    terms.additional.insert(submission.into(), incoming);
    terms.total_bytes = quote(owner, conversation, &lease.response_id, &terms)?;
    let delta = terms.total_bytes.saturating_sub(old);
    let encoded = serde_json::to_string(&terms)?;
    let limit = space::owner_limits(env, owner).await?.bytes;
    operation.query(env,owner,"capacity-input-extend",env.d1("DB")?
        .prepare("UPDATE conversation_response_capacity SET remaining_bytes=remaining_bytes+CAST(?5 AS INTEGER),terms_json=?6,action='none',updated_at_ms=CAST(?7 AS INTEGER) WHERE account_id=?1 AND conversation_id=?2 AND response_id=?3 AND generation=?4 AND state='reserved' AND terms_json=?8 AND EXISTS(SELECT 1 FROM conversation_capacity_accounts WHERE account_id=?1 AND reserved_bytes+retained_bytes<=CAST(?9 AS INTEGER)-CAST(?5 AS INTEGER))")
        .bind(&[owner.into(),conversation.into(),lease.response_id.as_str().into(),lease.generation.as_str().into(),delta.to_string().into(),encoded.as_str().into(),util::now_ms().to_string().into(),row.terms_json.as_str().into(),limit.to_string().into()])?).await?;
    let updated = self::row(env, owner, operation, conversation, &lease.response_id)
        .await?
        .ok_or_else(|| fail("conversation_capacity_reservation_missing"))?;
    if updated.terms_json == encoded {
        return Ok(Prepared::Held(lease.clone()));
    }
    let usage = usage(env, owner, operation).await?;
    Ok(Prepared::Insufficient {
        required_bytes: delta,
        available_bytes: usage
            .limit_bytes
            .saturating_sub(usage.reserved_bytes.saturating_add(usage.retained_bytes)),
    })
}
