//! Retained Conversation JSON has an independent, owner-wide logical-byte cap.
//! Publication and physical removal are proved by the Conversation authority;
//! an ambiguous RPC or a visibility tombstone never refunds reserved capacity.

use crate::{conversation_meter::ConversationOperation, space, util};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use worker::{Env, Error, Result};

#[path = "conversation_quota_escrow.rs"]
pub(crate) mod escrow;
#[path = "conversation_quota_publish.rs"]
mod publication;
pub(crate) use publication::{Body, has_pending, publish, recover};

const OBJECT_OVERHEAD: u64 = 256;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CapacityState {
    Reserved,
    Retained,
    Released,
}

#[derive(Clone, Debug, Deserialize)]
pub(crate) struct Reservation {
    pub conversation_id: String,
    pub object_id: String,
    pub digest: String,
    pub bytes: u64,
    pub state: CapacityState,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct Payload {
    conversation_id: String,
    object_id: String,
    digest: String,
    bytes: u64,
}

impl Payload {
    /// The ID names an immutable logical version, not a mutable storage pointer.
    pub(crate) fn json(conversation: &str, object: &str, value: &Value) -> Result<Self> {
        if conversation.is_empty()
            || object.is_empty()
            || conversation.len() > 256
            || object.len() > 512
        {
            return Err(fail("invalid_conversation_capacity_identity"));
        }
        let encoded = serde_json::to_vec(value)?;
        Self::encoded(
            conversation,
            object,
            &encoded,
            format!("{:x}", Sha256::digest(&encoded)),
        )
    }

    fn encoded(conversation: &str, object: &str, encoded: &[u8], digest: String) -> Result<Self> {
        let bytes = (encoded.len() as u64)
            .checked_add(conversation.len() as u64)
            .and_then(|bytes| bytes.checked_add(object.len() as u64))
            .and_then(|bytes| bytes.checked_add(OBJECT_OVERHEAD))
            .filter(|bytes| *bytes <= i64::MAX as u64)
            .ok_or_else(|| fail("conversation_capacity_size_overflow"))?;
        Ok(Self {
            conversation_id: conversation.into(),
            object_id: object.into(),
            digest,
            bytes,
        })
    }

    pub(crate) fn bytes(&self) -> u64 {
        self.bytes
    }

    pub(crate) fn body(conversation: &str, key: &str, encoded: &[u8]) -> Result<Self> {
        let digest = format!("{:x}", Sha256::digest(encoded));
        Self::encoded(
            conversation,
            &format!("body:{key}:{digest}"),
            encoded,
            digest,
        )
    }

    pub(crate) fn header_block(conversation: &str, index: u64) -> Self {
        let object_id = format!("header-block:{index}");
        Self {
            conversation_id: conversation.into(),
            digest: format!("{:x}", Sha256::digest(object_id.as_bytes())),
            object_id,
            bytes: HEADER_BLOCK_BYTES,
        }
    }
}

pub(crate) const HEADER_BLOCK_BYTES: u64 = 4096;

pub(crate) fn header_blocks(conversation: &str, bytes: usize, previous: u64) -> Result<u64> {
    let bytes = (bytes as u64)
        .checked_add(conversation.len() as u64)
        .and_then(|value| value.checked_add(OBJECT_OVERHEAD))
        .ok_or_else(|| fail("conversation_capacity_size_overflow"))?;
    Ok(previous.max(bytes.div_ceil(HEADER_BLOCK_BYTES)))
}

/// One conditional INSERT handles all newly required allocations. The triggers
/// update the same owner counter in the same SQLite statement transaction.
pub(crate) async fn reserve_batch(
    env: &Env,
    owner: &str,
    operation: &ConversationOperation,
    payloads: &[Payload],
) -> Result<bool> {
    let limit = space::owner_limits(env, owner).await?.bytes;
    reserve_batch_at_limit(env, owner, operation, payloads, limit).await
}

/// A one-time adoption of already-published pre-quota data, never a new write.
/// It may record an existing overage; subsequent new allocations still enforce
/// the current plan. Call only from a canonical authority read of its own keys.
pub(crate) async fn adopt_existing_batch(
    env: &Env,
    owner: &str,
    operation: &ConversationOperation,
    payloads: &[Payload],
) -> Result<()> {
    if !reserve_batch_at_limit(env, owner, operation, payloads, i64::MAX as u64).await? {
        return Err(fail("conversation_capacity_adoption_conflict"));
    }
    commit_batch(env, owner, operation, payloads).await
}

async fn reserve_batch_at_limit(
    env: &Env,
    owner: &str,
    operation: &ConversationOperation,
    payloads: &[Payload],
    limit: u64,
) -> Result<bool> {
    if payloads.is_empty() {
        return Ok(true);
    }
    let encoded = serde_json::to_string(payloads)?;
    operation.query(env, owner, "capacity-account", env.d1("DB")?
        .prepare("INSERT INTO conversation_capacity_accounts(account_id) VALUES(?) ON CONFLICT(account_id) DO NOTHING")
        .bind(&[owner.into()])?).await?;
    operation.query(env, owner, "capacity-reserve-batch", env.d1("DB")?
        .prepare("WITH desired AS MATERIALIZED (SELECT json_extract(value,'$.conversation_id') AS conversation_id,json_extract(value,'$.object_id') AS object_id,json_extract(value,'$.digest') AS digest,CAST(json_extract(value,'$.bytes') AS INTEGER) AS bytes FROM json_each(?2)),needed AS MATERIALIZED (SELECT d.* FROM desired d LEFT JOIN conversation_capacity_objects o ON o.account_id=?1 AND o.conversation_id=d.conversation_id AND o.object_id=d.object_id WHERE o.state IS NULL OR o.state='released'),admitted AS MATERIALIZED (SELECT 1 FROM conversation_capacity_accounts WHERE account_id=?1 AND reserved_bytes+retained_bytes<=CAST(?4 AS INTEGER)-(SELECT COALESCE(SUM(bytes),0) FROM needed) AND NOT EXISTS(SELECT 1 FROM desired d JOIN conversation_capacity_objects o ON o.account_id=?1 AND o.conversation_id=d.conversation_id AND o.object_id=d.object_id WHERE o.digest!=d.digest OR o.bytes!=d.bytes)) INSERT INTO conversation_capacity_objects(account_id,conversation_id,object_id,digest,bytes,state,updated_at_ms) SELECT ?1,d.conversation_id,d.object_id,d.digest,d.bytes,'reserved',CAST(?3 AS INTEGER) FROM desired d CROSS JOIN admitted WHERE 1 ON CONFLICT(account_id,conversation_id,object_id) DO UPDATE SET state='reserved',release_proof=NULL,updated_at_ms=excluded.updated_at_ms,escrow_response_id=NULL,escrow_generation=NULL WHERE conversation_capacity_objects.state='released'")
        .bind(&[owner.into(),encoded.as_str().into(),util::now_ms().to_string().into(),limit.to_string().into()])?).await?;
    let rows = operation.query(env, owner, "capacity-receipt-batch", env.d1("DB")?
        .prepare("SELECT o.conversation_id,o.object_id,o.digest,o.bytes,o.state FROM conversation_capacity_objects o JOIN json_each(?2) j ON o.conversation_id=json_extract(j.value,'$.conversation_id') AND o.object_id=json_extract(j.value,'$.object_id') WHERE o.account_id=?1")
        .bind(&[owner.into(),encoded.as_str().into()])?).await?.results::<Reservation>()?;
    let mut complete = rows.len() == payloads.len();
    for payload in payloads {
        if let Some(row) = rows.iter().find(|row| {
            row.conversation_id == payload.conversation_id && row.object_id == payload.object_id
        }) {
            verify(payload, row)?;
            complete &= row.state != CapacityState::Released;
        }
    }
    Ok(complete)
}

pub(crate) async fn commit_batch(
    env: &Env,
    owner: &str,
    operation: &ConversationOperation,
    payloads: &[Payload],
) -> Result<()> {
    if payloads.is_empty() {
        return Ok(());
    }
    let encoded = serde_json::to_string(payloads)?;
    operation.query(env, owner, "capacity-commit-batch", env.d1("DB")?
        .prepare("UPDATE conversation_capacity_objects SET state='retained',updated_at_ms=CAST(?3 AS INTEGER) WHERE account_id=?1 AND state='reserved' AND EXISTS(SELECT 1 FROM json_each(?2) j WHERE conversation_id=json_extract(j.value,'$.conversation_id') AND object_id=json_extract(j.value,'$.object_id') AND digest=json_extract(j.value,'$.digest') AND bytes=CAST(json_extract(j.value,'$.bytes') AS INTEGER))")
        .bind(&[owner.into(),encoded.as_str().into(),util::now_ms().to_string().into()])?).await?;
    Ok(())
}

/// All listed candidates have been proved unpublished by the authority. Already
/// retained objects are deliberately untouched, including reused header blocks.
pub(crate) async fn release_unpublished_batch(
    env: &Env,
    owner: &str,
    operation: &ConversationOperation,
    payloads: &[Payload],
) -> Result<()> {
    if payloads.is_empty() {
        return Ok(());
    }
    let encoded = serde_json::to_string(payloads)?;
    operation.query(env, owner, "capacity-release-batch", env.d1("DB")?
        .prepare("UPDATE conversation_capacity_objects SET state='released',release_proof='never_published',updated_at_ms=CAST(?3 AS INTEGER) WHERE account_id=?1 AND state='reserved' AND EXISTS(SELECT 1 FROM json_each(?2) j WHERE conversation_id=json_extract(j.value,'$.conversation_id') AND object_id=json_extract(j.value,'$.object_id') AND digest=json_extract(j.value,'$.digest') AND bytes=CAST(json_extract(j.value,'$.bytes') AS INTEGER))")
        .bind(&[owner.into(),encoded.as_str().into(),util::now_ms().to_string().into()])?).await?;
    Ok(())
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum RemovalProof {
    /// A canonical read proves no published header/receipt references the body.
    NeverPublished,
    /// The body is actually gone and no retained snapshot depends on its data.
    ContentRemoved,
}

impl RemovalProof {
    fn name(self) -> &'static str {
        match self {
            Self::NeverPublished => "never_published",
            Self::ContentRemoved => "content_removed",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct CapacityUsage {
    pub reserved_bytes: u64,
    pub retained_bytes: u64,
    pub limit_bytes: u64,
}

/// `None` is a definite capacity rejection. Existing identical reservations keep
/// working after a plan downgrade; a retry allocates no additional bytes.
pub(crate) async fn reserve(
    env: &Env,
    owner: &str,
    operation: &ConversationOperation,
    payload: &Payload,
) -> Result<Option<Reservation>> {
    let limit = space::owner_limits(env, owner).await?.bytes;
    operation.query(env, owner, "capacity-account", env.d1("DB")?
        .prepare("INSERT INTO conversation_capacity_accounts(account_id) VALUES(?) ON CONFLICT(account_id) DO NOTHING")
        .bind(&[owner.into()])?).await?;
    operation.query(env, owner, "capacity-reserve", env.d1("DB")?
        .prepare("INSERT INTO conversation_capacity_objects(account_id,conversation_id,object_id,digest,bytes,state,updated_at_ms) SELECT ?1,?2,?3,?4,CAST(?5 AS INTEGER),'reserved',CAST(?6 AS INTEGER) WHERE EXISTS(SELECT 1 FROM conversation_capacity_accounts WHERE account_id=?1 AND reserved_bytes+retained_bytes<=CAST(?7 AS INTEGER)-CAST(?5 AS INTEGER)) AND NOT EXISTS(SELECT 1 FROM conversation_capacity_objects WHERE account_id=?1 AND conversation_id=?2 AND object_id=?3) ON CONFLICT DO NOTHING")
        .bind(&[owner.into(),payload.conversation_id.as_str().into(),payload.object_id.as_str().into(),payload.digest.as_str().into(),payload.bytes.to_string().into(),util::now_ms().to_string().into(),limit.to_string().into()])?).await?;
    let Some(existing) = read(env, owner, operation, payload).await? else {
        return Ok(None);
    };
    verify(payload, &existing)?;
    if existing.state != CapacityState::Released {
        return Ok(Some(existing));
    }
    operation.query(env, owner, "capacity-retry", env.d1("DB")?
        .prepare("UPDATE conversation_capacity_objects SET state='reserved',release_proof=NULL,escrow_response_id=NULL,escrow_generation=NULL,updated_at_ms=CAST(?4 AS INTEGER) WHERE account_id=?1 AND conversation_id=?2 AND object_id=?3 AND state='released' AND EXISTS(SELECT 1 FROM conversation_capacity_accounts WHERE account_id=?1 AND reserved_bytes+retained_bytes<=CAST(?5 AS INTEGER)-bytes)")
        .bind(&[owner.into(),payload.conversation_id.as_str().into(),payload.object_id.as_str().into(),util::now_ms().to_string().into(),limit.to_string().into()])?).await?;
    Ok(read(env, owner, operation, payload)
        .await?
        .filter(|row| row.state != CapacityState::Released))
}

pub(crate) async fn commit(
    env: &Env,
    owner: &str,
    operation: &ConversationOperation,
    payload: &Payload,
) -> Result<()> {
    let existing = read(env, owner, operation, payload)
        .await?
        .ok_or_else(|| fail("conversation_capacity_reservation_missing"))?;
    verify(payload, &existing)?;
    operation.query(env, owner, "capacity-commit", env.d1("DB")?
        .prepare("UPDATE conversation_capacity_objects SET state='retained',updated_at_ms=CAST(?4 AS INTEGER) WHERE account_id=?1 AND conversation_id=?2 AND object_id=?3 AND state='reserved'")
        .bind(&[owner.into(),payload.conversation_id.as_str().into(),payload.object_id.as_str().into(),util::now_ms().to_string().into()])?).await?;
    let committed = read(env, owner, operation, payload)
        .await?
        .ok_or_else(|| fail("conversation_capacity_reservation_missing"))?;
    if committed.state != CapacityState::Retained {
        return Err(fail("conversation_capacity_publication_conflict"));
    }
    Ok(())
}

/// Call only after the authority's own storage/header checks establish the proof.
/// Never call from item/conversation DELETE merely because visibility changed.
pub(crate) async fn release(
    env: &Env,
    owner: &str,
    operation: &ConversationOperation,
    payload: &Payload,
    proof: RemovalProof,
) -> Result<()> {
    let existing = read(env, owner, operation, payload)
        .await?
        .ok_or_else(|| fail("conversation_capacity_reservation_missing"))?;
    verify(payload, &existing)?;
    operation.query(env, owner, "capacity-release", env.d1("DB")?
        .prepare("UPDATE conversation_capacity_objects SET state='released',release_proof=?4,updated_at_ms=CAST(?5 AS INTEGER) WHERE account_id=?1 AND conversation_id=?2 AND object_id=?3 AND (state='reserved' OR (state='retained' AND ?4='content_removed'))")
        .bind(&[owner.into(),payload.conversation_id.as_str().into(),payload.object_id.as_str().into(),proof.name().into(),util::now_ms().to_string().into()])?).await?;
    let released = read(env, owner, operation, payload)
        .await?
        .ok_or_else(|| fail("conversation_capacity_reservation_missing"))?;
    if released.state != CapacityState::Released {
        return Err(fail("conversation_capacity_removal_unproven"));
    }
    Ok(())
}

pub(crate) async fn usage(
    env: &Env,
    owner: &str,
    operation: &ConversationOperation,
) -> Result<CapacityUsage> {
    #[derive(Deserialize)]
    struct Counters {
        reserved_bytes: u64,
        retained_bytes: u64,
    }
    let rows = operation.query(env, owner, "capacity-usage", env.d1("DB")?
        .prepare("SELECT reserved_bytes,retained_bytes FROM conversation_capacity_accounts WHERE account_id=?")
        .bind(&[owner.into()])?).await?.results::<Counters>()?;
    let (reserved_bytes, retained_bytes) = rows
        .first()
        .map_or((0, 0), |row| (row.reserved_bytes, row.retained_bytes));
    Ok(CapacityUsage {
        reserved_bytes,
        retained_bytes,
        limit_bytes: space::owner_limits(env, owner).await?.bytes,
    })
}

async fn read(
    env: &Env,
    owner: &str,
    operation: &ConversationOperation,
    payload: &Payload,
) -> Result<Option<Reservation>> {
    Ok(operation.query(env, owner, "capacity-receipt", env.d1("DB")?
        .prepare("SELECT conversation_id,object_id,digest,bytes,state FROM conversation_capacity_objects WHERE account_id=? AND conversation_id=? AND object_id=?")
        .bind(&[owner.into(),payload.conversation_id.as_str().into(),payload.object_id.as_str().into()])?).await?.results::<Reservation>()?.into_iter().next())
}

fn verify(payload: &Payload, existing: &Reservation) -> Result<()> {
    if payload.conversation_id != existing.conversation_id
        || payload.object_id != existing.object_id
        || payload.digest != existing.digest
        || payload.bytes != existing.bytes
    {
        return Err(fail("conversation_capacity_identity_conflict"));
    }
    Ok(())
}

fn fail(code: &'static str) -> Error {
    Error::RustError(code.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_encoded_utf8_and_object_overhead_not_referenced_binary() {
        let value = serde_json::json!({"text":"雪","file_id":"file_ref","bytes":9_000_000});
        let payload = Payload::json("conv", "item", &value).unwrap();
        assert_eq!(
            payload.bytes(),
            serde_json::to_vec(&value).unwrap().len() as u64 + 8 + OBJECT_OVERHEAD
        );
        assert!(payload.bytes() < 1024);
    }

    #[test]
    fn immutable_identity_includes_payload_bytes() {
        let one = Payload::json("conv", "item", &serde_json::json!("one")).unwrap();
        let two = Payload::json("conv", "item", &serde_json::json!("two")).unwrap();
        assert_ne!(one.digest, two.digest);
    }
}
