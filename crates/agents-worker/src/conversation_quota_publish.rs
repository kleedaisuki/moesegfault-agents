//! Capacity reservation and authority publication have separate acknowledgments.

use super::*;
use crate::session_storage::{read_chunks, write_chunks};
use worker::{ListOptions, Storage};

const INTENT: &str = "conversation.quota-publication.v1";

pub(crate) struct Body {
    key: String,
    bytes: Vec<u8>,
}

impl Body {
    pub(crate) fn json<T: Serialize>(key: String, value: &T) -> Result<Self> {
        Ok(Self {
            key,
            bytes: serde_json::to_vec(value)?,
        })
    }
}

#[derive(Serialize, Deserialize)]
struct Publication {
    owner: String,
    head: String,
    #[serde(default)]
    conversation: String,
    previous_digest: Option<String>,
    target_digest: String,
    payloads: Vec<Payload>,
    candidates: Vec<String>,
    #[serde(default)]
    escrow: Option<escrow::Lease>,
}

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

pub(crate) async fn has_pending(storage: &Storage) -> Result<bool> {
    Ok(read_chunks(storage, INTENT).await?.is_some())
}

pub(crate) async fn publish(
    env: &Env,
    owner: &str,
    operation: &ConversationOperation,
    storage: &Storage,
    conversation: &str,
    head: &str,
    bytes: &[u8],
    blocks: u64,
    bodies: &[Body],
    escrow: Option<&escrow::Lease>,
) -> Result<()> {
    recover(env, owner, operation, storage).await?;
    // Completed intent indexes may have lost a chunk-cleanup acknowledgment.
    // They are intermediate metadata, never published Conversation payloads.
    remove_unpublished(storage, INTENT).await?;
    let mut payloads = bodies
        .iter()
        .map(|body| Payload::body(conversation, &body.key, &body.bytes))
        .collect::<Result<Vec<_>>>()?;
    payloads.extend((0..blocks).map(|index| Payload::header_block(conversation, index)));
    let publication = Publication {
        owner: owner.into(),
        head: head.into(),
        conversation: conversation.into(),
        previous_digest: read_chunks(storage, head).await?.as_deref().map(digest),
        target_digest: digest(bytes),
        payloads,
        candidates: bodies.iter().map(|body| body.key.clone()).collect(),
        escrow: escrow.cloned(),
    };
    // Record the recovery identities before the first D1 reservation, not after
    // it. A crash during admission must not leave undiscoverable capacity holds.
    write_chunks(storage, INTENT, &serde_json::to_vec(&publication)?).await?;
    if let Some(lease) = &publication.escrow {
        escrow::allocate(
            env,
            owner,
            operation,
            conversation,
            lease,
            &publication.payloads,
        )
        .await?;
    } else if !reserve_batch(env, owner, operation, &publication.payloads).await? {
        recover(env, owner, operation, storage).await?;
        return Err(fail("conversation_capacity_exceeded"));
    }
    let written = async {
        for body in bodies.iter().filter(|body| body.key.starts_with("image:")) {
            let item = body.key.strip_prefix("image:").unwrap();
            let blob: crate::workspace_blobs::OwnedBlobRef = serde_json::from_slice(&body.bytes)?;
            blob.validate(owner)?;
            operation
                .external_d1(
                    env,
                    owner,
                    "conversation-image-bind",
                    1,
                    crate::workspace_blobs::bind_conversation_image(
                        env,
                        owner,
                        conversation,
                        item,
                        &blob,
                    ),
                )
                .await?;
        }
        write_bodies(storage, bodies, head, bytes).await
    }
    .await;
    let current = if written.is_ok() {
        Some(publication.target_digest.clone())
    } else {
        read_chunks(storage, head).await?.as_deref().map(digest)
    };
    if current.as_ref() == Some(&publication.target_digest) {
        // The mutation is definite even if quota acknowledgment or obsolete
        // chunk cleanup failed. Never turn it into a retriable second append.
        if finalize(env, owner, operation, storage, &publication, true)
            .await
            .is_err()
        {
            util::emit(
                "conversation.capacity_commit_deferred",
                serde_json::json!({"conversation_id":conversation}),
            );
        }
        return Ok(());
    }
    if current == publication.previous_digest {
        finalize(env, owner, operation, storage, &publication, false).await?;
        return written.and(Err(fail("conversation_publication_not_committed")));
    }
    Err(fail("conversation_capacity_publication_unknown"))
}

async fn write_bodies(storage: &Storage, bodies: &[Body], head: &str, bytes: &[u8]) -> Result<()> {
    for body in bodies {
        write_chunks(storage, &body.key, &body.bytes).await?;
    }
    write_chunks(storage, head, bytes).await
}

pub(crate) async fn recover(
    env: &Env,
    owner: &str,
    operation: &ConversationOperation,
    storage: &Storage,
) -> Result<()> {
    let Some(bytes) = read_chunks(storage, INTENT).await? else {
        return Ok(());
    };
    let publication: Publication = serde_json::from_slice(&bytes)?;
    if publication.owner != owner {
        return Err(fail("conversation_capacity_owner_mismatch"));
    }
    let current = read_chunks(storage, &publication.head)
        .await?
        .as_deref()
        .map(digest);
    if current.as_ref() == Some(&publication.target_digest) {
        return finalize(env, owner, operation, storage, &publication, true).await;
    }
    if current == publication.previous_digest {
        return finalize(env, owner, operation, storage, &publication, false).await;
    }
    // No timeout guesses publication. A mixed-version writer or corrupt pointer
    // requires diagnosis rather than freeing bytes that may be user data.
    Err(fail("conversation_capacity_publication_unknown"))
}

async fn finalize(
    env: &Env,
    owner: &str,
    operation: &ConversationOperation,
    storage: &Storage,
    publication: &Publication,
    published: bool,
) -> Result<()> {
    if published {
        commit_batch(env, owner, operation, &publication.payloads).await?;
        if let Some(lease) = &publication.escrow {
            escrow::convert(env, owner, operation, &publication.conversation, lease).await?;
        }
    } else {
        for key in &publication.candidates {
            remove_unpublished(storage, key).await?;
            if let Some(item) = key.strip_prefix("image:") {
                operation
                    .external_d1(
                        env,
                        owner,
                        "conversation-image-discard",
                        1,
                        crate::workspace_blobs::discard_unpublished_conversation_image(
                            env,
                            owner,
                            &publication.conversation,
                            item,
                        ),
                    )
                    .await?;
            }
        }
        if let Some(lease) = &publication.escrow {
            escrow::rollback(
                env,
                owner,
                operation,
                &publication.conversation,
                lease,
                &publication.payloads,
            )
            .await?;
        } else {
            release_unpublished_batch(env, owner, operation, &publication.payloads).await?;
        }
    }
    // The financial outcome is now durable. Remove the discovery index before
    // its chunks so interrupted cleanup cannot leave an unreadable intent.
    remove_unpublished(storage, INTENT).await
}

async fn remove_unpublished(storage: &Storage, key: &str) -> Result<()> {
    storage.delete(key).await?;
    let prefix = format!("{key}:");
    loop {
        let chunks = storage
            .list_with_options(ListOptions::new().prefix(&prefix).limit(128))
            .await?;
        if chunks.size() == 0 {
            break;
        }
        let keys = js_sys::Array::from(&chunks.keys())
            .iter()
            .filter_map(|key| key.as_string())
            .collect::<Vec<_>>();
        if keys.is_empty() {
            return Err(fail("conversation_capacity_cleanup_invalid"));
        }
        storage.delete_multiple(keys).await?;
    }
    Ok(())
}
