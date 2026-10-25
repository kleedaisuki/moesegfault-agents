//! Connection-owned parent authority is bounded metadata, not public retention.

use super::*;
use crate::response_execution::{ConnectionLease, OwnedParent};
use sha2::{Digest, Sha256};

const INDEX_PREFIX: &str = "connection-parent:index:";
const DEADLINE_KEY: &str = "connection-parent:deadline";
const MAX_CONNECTIONS: usize = 64;
// Live and revoked IDs can coexist; both lists must fit one 128 KiB DO value.
const MAX_RESPONSES: usize = 256;
const MAX_LEASE_MS: u64 = 60 * 60 * 1000;
const CLEANUP_BATCH: usize = 64;
const RETRY_MS: u64 = 30_000;

#[derive(Clone, Serialize, Deserialize)]
struct ConnectionIndex {
    connection_id: String,
    expires_at_ms: u64,
    released: bool,
    responses: Vec<String>,
    #[serde(default)]
    revoked: Vec<String>,
}

#[derive(Serialize, Deserialize)]
struct ParentGrant {
    owner_id: String,
    response_id: String,
    connection_id: String,
    expires_at_ms: u64,
    token: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ParentRequest {
    owner_id: String,
    connection_id: String,
    expires_at_ms: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReleaseRequest {
    owner_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RevokeRequest {
    owner_id: String,
    connection_id: String,
    token: String,
}

fn valid_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
}

fn connection_key(connection: &str) -> String {
    format!("{:x}", Sha256::digest(connection.as_bytes()))
}

fn index_key(connection: &str) -> String {
    format!("{INDEX_PREFIX}{}", connection_key(connection))
}

fn grant_key(connection: &str, response: &str) -> String {
    format!(
        "connection-parent:grant:{}:{response}",
        connection_key(connection)
    )
}

fn valid_lease(lease: &ConnectionLease, now: u64) -> bool {
    valid_id(&lease.connection_id)
        && lease.expires_at_ms > now
        && lease.expires_at_ms <= now.saturating_add(MAX_LEASE_MS)
}

fn authority_matches(
    index: &ConnectionIndex,
    grant: &ParentGrant,
    owner: &str,
    response: &str,
    parent: &OwnedParent,
    now: u64,
) -> bool {
    !index.released
        && index.expires_at_ms > now
        && index.connection_id == parent.connection_id
        && grant.owner_id == owner
        && grant.response_id == response
        && parent.response_id == response
        && grant.connection_id == parent.connection_id
        && grant.expires_at_ms == index.expires_at_ms
        && grant.expires_at_ms > now
        && index.responses.iter().any(|id| id == response)
        && !index.revoked.iter().any(|id| id == response)
        && grant.token.as_deref() == Some(parent.token.as_str())
}

fn cleanup_deadline(indexes: &[ConnectionIndex], now: u64) -> Option<u64> {
    indexes
        .iter()
        .map(|index| {
            if index.released || !index.revoked.is_empty() || index.expires_at_ms <= now {
                now.saturating_add(1000)
            } else {
                index.expires_at_ms
            }
        })
        .min()
}

impl SessionInner {
    fn connection_owner(&self) -> Result<String> {
        self.data
            .borrow()
            .as_ref()
            .map(|data| data.record.owner_id.clone())
            .ok_or_else(|| Error::RustError("session_not_initialized".into()))
    }

    async fn connection_indexes(&self) -> Result<Vec<ConnectionIndex>> {
        let values = self
            .state
            .storage()
            .list_with_options(
                ListOptions::new()
                    .prefix(INDEX_PREFIX)
                    .limit(MAX_CONNECTIONS + 1),
            )
            .await?;
        if values.size() as usize > MAX_CONNECTIONS {
            return Err(Error::RustError("connection_parent_limit".into()));
        }
        let mut entries = vec![];
        values.for_each(&mut |value, _| entries.push(value));
        entries
            .into_iter()
            .map(|value| serde_wasm_bindgen::from_value(value).map_err(Error::from))
            .collect()
    }

    async fn arm_connection_deadline(&self, deadline: Option<u64>) -> Result<()> {
        match deadline {
            Some(deadline) => self.state.storage().put(DEADLINE_KEY, deadline).await?,
            None => {
                self.state.storage().delete(DEADLINE_KEY).await?;
            }
        }
        self.schedule_connection_cleanup(deadline).await
    }

    async fn schedule_connection_cleanup(&self, deadline: Option<u64>) -> Result<()> {
        self.data
            .borrow_mut()
            .as_mut()
            .unwrap()
            .connection_cleanup_at = deadline;
        let Some(deadline) = deadline else {
            return Ok(());
        };
        if self
            .state
            .storage()
            .get_alarm()
            .await?
            .is_none_or(|current| current as i128 > i128::from(deadline))
        {
            self.state
                .storage()
                .set_alarm(util::alarm_at(deadline))
                .await?;
        }
        Ok(())
    }

    pub(super) async fn restore_connection_cleanup(&self) -> Result<()> {
        let deadline = self.state.storage().get::<u64>(DEADLINE_KEY).await?;
        if let Some(deadline) = deadline {
            self.schedule_connection_cleanup(Some(deadline)).await?;
        }
        Ok(())
    }

    /// The caller owns the canonical persistence mutex and commits Session state
    /// after this metadata. An orphan grant cannot issue a parent without a run.
    pub(super) async fn record_connection_grant(
        &self,
        response_id: &str,
        lease: &ConnectionLease,
    ) -> Result<()> {
        let now = util::now_ms();
        if !valid_id(response_id)
            || !valid_id(&lease.connection_id)
            || lease.expires_at_ms > now.saturating_add(MAX_LEASE_MS)
        {
            return Err(Error::RustError("invalid_connection_parent_lease".into()));
        }
        // Accepted work can outlive its observer. Caching is optional authority
        // for future requests, never permission to execute this durable task.
        if lease.expires_at_ms <= now {
            return Ok(());
        }
        let owner = self.connection_owner()?;
        let storage = self.state.storage();
        let key = index_key(&lease.connection_id);
        let mut index = match storage.get::<ConnectionIndex>(&key).await? {
            Some(index) => index,
            None => {
                if self.connection_indexes().await?.len() >= MAX_CONNECTIONS {
                    return Ok(());
                }
                ConnectionIndex {
                    connection_id: lease.connection_id.clone(),
                    expires_at_ms: lease.expires_at_ms,
                    released: false,
                    responses: vec![],
                    revoked: vec![],
                }
            }
        };
        if index.released {
            return Ok(());
        }
        if index.connection_id != lease.connection_id || index.expires_at_ms != lease.expires_at_ms
        {
            return Err(Error::RustError("connection_parent_lease_conflict".into()));
        }
        let grant_key = grant_key(&lease.connection_id, response_id);
        let grant = storage.get::<ParentGrant>(&grant_key).await?;
        if let Some(grant) = &grant {
            if grant.owner_id != owner
                || grant.connection_id != lease.connection_id
                || grant.expires_at_ms != lease.expires_at_ms
                || grant.response_id != response_id
            {
                return Err(Error::RustError("connection_parent_lease_conflict".into()));
            }
        }
        if !index.responses.iter().any(|id| id == response_id) {
            if index.responses.len() >= MAX_RESPONSES {
                return Ok(());
            }
            index.responses.push(response_id.into());
        }
        let current = self.data.borrow().as_ref().unwrap().connection_cleanup_at;
        self.arm_connection_deadline(Some(
            current.map_or(lease.expires_at_ms, |old| old.min(lease.expires_at_ms)),
        ))
        .await?;
        // Publish cleanup membership first; an interrupted grant write therefore
        // leaves a collectible candidate, never an unindexed persistent token.
        storage.put(&key, &index).await?;
        if grant.is_none() {
            storage
                .put(
                    &grant_key,
                    ParentGrant {
                        owner_id: owner,
                        response_id: response_id.into(),
                        connection_id: lease.connection_id.clone(),
                        expires_at_ms: lease.expires_at_ms,
                        token: None,
                    },
                )
                .await?;
        }
        Ok(())
    }

    pub(super) async fn connection_grant_active(
        &self,
        response_id: &str,
        lease: &ConnectionLease,
    ) -> Result<bool> {
        if !valid_lease(lease, util::now_ms()) {
            return Ok(false);
        }
        let storage = self.state.storage();
        let Some(index) = storage
            .get::<ConnectionIndex>(&index_key(&lease.connection_id))
            .await?
        else {
            return Ok(false);
        };
        let Some(grant) = storage
            .get::<ParentGrant>(&grant_key(&lease.connection_id, response_id))
            .await?
        else {
            return Ok(false);
        };
        Ok(!index.released
            && index.connection_id == lease.connection_id
            && index.expires_at_ms == lease.expires_at_ms
            && grant.expires_at_ms == lease.expires_at_ms
            && grant.owner_id == self.connection_owner()?
            && grant.response_id == response_id
            && grant.connection_id == lease.connection_id
            && index.responses.iter().any(|id| id == response_id)
            && !index.revoked.iter().any(|id| id == response_id))
    }

    pub(super) async fn validate_parent_grant(
        &self,
        response_id: &str,
        parent: &OwnedParent,
    ) -> Result<bool> {
        if !valid_id(response_id) || !valid_id(&parent.connection_id) || parent.token.len() > 4096 {
            return Ok(false);
        }
        let storage = self.state.storage();
        let Some(index) = storage
            .get::<ConnectionIndex>(&index_key(&parent.connection_id))
            .await?
        else {
            return Ok(false);
        };
        let Some(grant) = storage
            .get::<ParentGrant>(&grant_key(&parent.connection_id, response_id))
            .await?
        else {
            return Ok(false);
        };
        Ok(authority_matches(
            &index,
            &grant,
            &self.connection_owner()?,
            response_id,
            parent,
            util::now_ms(),
        ))
    }

    pub(super) async fn connection_route(&self, mut req: Request) -> Result<Response> {
        let path = req.path();
        let parts = path.trim_matches('/').split('/').collect::<Vec<_>>();
        match (req.method(), parts.as_slice()) {
            (Method::Post, ["responses", response, "connection-parent"]) if valid_id(response) => {
                let input: ParentRequest = req.json().await?;
                self.lease_parent(response, input).await
            }
            (Method::Delete, ["responses", response, "connection-parent"])
                if valid_id(response) =>
            {
                let input: RevokeRequest = req.json().await?;
                self.revoke_parent(response, input).await
            }
            (Method::Delete, ["connection-leases", connection]) if valid_id(connection) => {
                let input: ReleaseRequest = req.json().await?;
                self.release_connection(connection, &input.owner_id).await
            }
            _ => util::json_error(404, "not_found", "Connection parent operation not found"),
        }
    }

    async fn lease_parent(&self, response_id: &str, input: ParentRequest) -> Result<Response> {
        let _guard = self.persistence.lock().await;
        if input.owner_id != self.connection_owner()? || !valid_id(&input.connection_id) {
            return util::json_error(404, "not_found", "Connection parent not found");
        }
        let storage = self.state.storage();
        let Some(index) = storage
            .get::<ConnectionIndex>(&index_key(&input.connection_id))
            .await?
        else {
            return util::json_error(404, "not_found", "Connection parent not found");
        };
        if index.released
            || index.revoked.iter().any(|id| id == response_id)
            || index.connection_id != input.connection_id
            || index.expires_at_ms <= util::now_ms()
            || index.expires_at_ms != input.expires_at_ms
        {
            return util::json_error(
                410,
                "connection_parent_expired",
                "Connection parent authority has expired",
            );
        }
        let key = grant_key(&input.connection_id, response_id);
        let Some(mut grant) = storage.get::<ParentGrant>(&key).await? else {
            return util::json_error(
                404,
                "not_found",
                "Response was not created by this connection",
            );
        };
        if grant.owner_id != input.owner_id
            || grant.connection_id != input.connection_id
            || grant.response_id != response_id
            || grant.expires_at_ms != input.expires_at_ms
            || !index.responses.iter().any(|id| id == response_id)
        {
            return util::json_error(404, "not_found", "Connection parent not found");
        }
        let Some(response) = self.read_response(response_id).await? else {
            return util::json_error(404, "not_found", "Response not found");
        };
        if response.deleted {
            return util::json_error(404, "not_found", "Response not found");
        }
        let ready = matches!(
            response.status.as_str(),
            "completed" | "failed" | "incomplete" | "cancelled"
        ) && response
            .branch_point
            .as_ref()
            .is_some_and(|point| !matches!(point.workspace, branch::WorkspaceSeed::Unsealed));
        if !ready {
            return util::json_error(
                409,
                "response_parent_not_ready",
                "Response has no sealed terminal parent boundary",
            );
        }
        let token = grant
            .token
            .get_or_insert_with(|| {
                format!(
                    "{}{}",
                    uuid::Uuid::new_v4().simple(),
                    uuid::Uuid::new_v4().simple()
                )
            })
            .clone();
        storage.put(&key, &grant).await?;
        Response::from_json(&json!({"response_id":response_id,"token":token}))
    }

    async fn revoke_parent(&self, response_id: &str, input: RevokeRequest) -> Result<Response> {
        if input.owner_id != self.connection_owner()?
            || !valid_id(&input.connection_id)
            || input.token.len() > 4096
        {
            return util::json_error(404, "not_found", "Connection parent not found");
        }
        let _guard = self.persistence.lock().await;
        let storage = self.state.storage();
        let key = index_key(&input.connection_id);
        let Some(mut index) = storage.get::<ConnectionIndex>(&key).await? else {
            return Response::from_json(
                &json!({"response_id":response_id,"released":true,"pending_cleanup":false}),
            );
        };
        let grant = storage
            .get::<ParentGrant>(&grant_key(&input.connection_id, response_id))
            .await?;
        let permitted = index.revoked.iter().any(|id| id == response_id)
            || grant.as_ref().is_some_and(|grant| {
                grant.owner_id == input.owner_id
                    && grant.response_id == response_id
                    && grant.connection_id == input.connection_id
                    && grant.token.as_deref() == Some(input.token.as_str())
            });
        if !permitted {
            return util::json_error(404, "not_found", "Connection parent not found");
        }
        if !index.revoked.iter().any(|id| id == response_id) {
            index.revoked.push(response_id.into());
        }
        storage.put(&key, &index).await?;
        storage
            .delete(&grant_key(&input.connection_id, response_id))
            .await?;
        self.arm_connection_deadline(Some(util::now_ms().saturating_add(1000)))
            .await?;
        let mut data = self.snapshot()?;
        save_state(&storage, &mut data).await?;
        Response::from_json(
            &json!({"response_id":response_id,"released":true,"pending_cleanup":true}),
        )
    }

    async fn release_connection(&self, connection: &str, owner: &str) -> Result<Response> {
        if owner != self.connection_owner()? {
            return util::json_error(404, "not_found", "Connection not found");
        }
        {
            let _guard = self.persistence.lock().await;
            if let Some(mut index) = self
                .state
                .storage()
                .get::<ConnectionIndex>(&index_key(connection))
                .await?
            {
                index.released = true;
                self.state
                    .storage()
                    .put(&index_key(connection), &index)
                    .await?;
                self.arm_connection_deadline(Some(util::now_ms().saturating_add(1000)))
                    .await?;
                let mut data = self.snapshot()?;
                save_state(&self.state.storage(), &mut data).await?;
            }
        }
        self.cleanup_connection_grants().await?;
        let pending = self
            .state
            .storage()
            .get::<ConnectionIndex>(&index_key(connection))
            .await?
            .is_some();
        Response::from_json(
            &json!({"connection_id":connection,"released":true,"pending_cleanup":pending}),
        )
    }

    pub(super) async fn cleanup_connection_grants(&self) -> Result<()> {
        let ready = self.data.borrow().as_ref().is_some_and(|data| {
            data.connection_cleanup_at
                .is_some_and(|at| at <= util::now_ms())
        });
        if !ready {
            return Ok(());
        }
        let _guard = self.persistence.lock().await;
        let owner = self.connection_owner()?;
        let now = util::now_ms();
        let mut remaining = CLEANUP_BATCH;
        let mut retry = false;
        for mut index in self.connection_indexes().await? {
            if remaining == 0 {
                break;
            }
            if index.expires_at_ms <= now {
                index.released = true;
            }
            if !index.released && index.revoked.is_empty() {
                continue;
            }
            self.state
                .storage()
                .put(&index_key(&index.connection_id), &index)
                .await?;
            while remaining > 0 {
                let candidate = if index.released {
                    index.responses.first()
                } else {
                    index.revoked.first()
                };
                let Some(response) = candidate.cloned() else {
                    break;
                };
                self.state
                    .storage()
                    .delete(&grant_key(&index.connection_id, &response))
                    .await?;
                if crate::workspace_blobs::release_connection(
                    &self.env,
                    &owner,
                    &index.connection_id,
                    &response,
                )
                .await
                .is_err()
                {
                    retry = true;
                    break;
                }
                index.responses.retain(|id| id != &response);
                index.revoked.retain(|id| id != &response);
                remaining -= 1;
            }
            if index.responses.is_empty() {
                self.state
                    .storage()
                    .delete(&index_key(&index.connection_id))
                    .await?;
            } else {
                self.state
                    .storage()
                    .put(&index_key(&index.connection_id), &index)
                    .await?;
            }
            if retry {
                break;
            }
        }
        let next = if retry {
            Some(now.saturating_add(RETRY_MS))
        } else {
            cleanup_deadline(&self.connection_indexes().await?, now)
        };
        self.arm_connection_deadline(next).await?;
        let mut data = self.snapshot()?;
        save_state(&self.state.storage(), &mut data).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn fixture() -> (ConnectionIndex, ParentGrant, OwnedParent) {
        (
            ConnectionIndex {
                connection_id: "conn".into(),
                expires_at_ms: 100,
                released: false,
                responses: vec!["resp_a".into()],
                revoked: vec![],
            },
            ParentGrant {
                owner_id: "owner".into(),
                response_id: "resp_a".into(),
                connection_id: "conn".into(),
                expires_at_ms: 100,
                token: Some("secret".into()),
            },
            OwnedParent {
                response_id: "resp_a".into(),
                connection_id: "conn".into(),
                token: "secret".into(),
            },
        )
    }

    #[test]
    fn authority_requires_owner_connection_response_token_and_live_index() {
        let (mut index, mut grant, mut parent) = fixture();
        assert!(authority_matches(
            &index, &grant, "owner", "resp_a", &parent, 99
        ));
        assert!(!authority_matches(
            &index, &grant, "owner", "resp_a", &parent, 100
        ));
        assert!(!authority_matches(
            &index, &grant, "other", "resp_a", &parent, 99
        ));
        parent.token = "guess".into();
        assert!(!authority_matches(
            &index, &grant, "owner", "resp_a", &parent, 99
        ));
        parent.token = "secret".into();
        grant.token = None;
        assert!(!authority_matches(
            &index, &grant, "owner", "resp_a", &parent, 99
        ));
        grant.token = Some("secret".into());
        grant.response_id = "resp_b".into();
        assert!(!authority_matches(
            &index, &grant, "owner", "resp_a", &parent, 99
        ));
        grant.response_id = "resp_a".into();
        index.released = true;
        assert!(!authority_matches(
            &index, &grant, "owner", "resp_a", &parent, 99
        ));
    }

    #[test]
    fn grants_cannot_extend_a_connection_beyond_one_hour() {
        assert!(valid_lease(
            &ConnectionLease {
                connection_id: "conn".into(),
                expires_at_ms: 10 + MAX_LEASE_MS
            },
            10
        ));
        assert!(!valid_lease(
            &ConnectionLease {
                connection_id: "conn".into(),
                expires_at_ms: 11 + MAX_LEASE_MS
            },
            10
        ));
        assert!(!valid_lease(
            &ConnectionLease {
                connection_id: "../conn".into(),
                expires_at_ms: 100
            },
            10
        ));
    }

    #[test]
    fn released_indexes_retry_without_interrupting_live_connection_ownership() {
        let (mut index, _, _) = fixture();
        assert_eq!(cleanup_deadline(&[index.clone()], 50), Some(100));
        index.released = true;
        assert_eq!(cleanup_deadline(&[index], 50), Some(1050));
        assert_eq!(cleanup_deadline(&[], 50), None);
    }

    #[test]
    fn revoking_one_parent_preserves_another_lane_parent() {
        let (mut index, mut grant, mut parent) = fixture();
        index.responses.push("resp_b".into());
        index.revoked.push("resp_a".into());
        assert!(!authority_matches(
            &index, &grant, "owner", "resp_a", &parent, 1
        ));
        grant.response_id = "resp_b".into();
        parent.response_id = "resp_b".into();
        assert!(authority_matches(
            &index, &grant, "owner", "resp_b", &parent, 1
        ));
    }

    #[test]
    fn metadata_and_issued_references_have_bounded_serialized_size() {
        let (mut index, _, _) = fixture();
        index.responses = (0..MAX_RESPONSES).map(|_| "r".repeat(128)).collect();
        index.revoked = index.responses.clone();
        assert!(serde_json::to_vec(&index).unwrap().len() < 128 * 1024);
        let reference = json!({"response_id":"r".repeat(128),"token":"a".repeat(64)});
        assert!(serde_json::to_vec(&reference).unwrap().len() < 4096);
    }

    proptest! {
        #[test]
        fn expiry_release_and_revocation_are_independent_authority_fences(
            now in any::<u64>(), expiry in any::<u64>(),
            released in any::<bool>(), revoked in any::<bool>(),
        ) {
            let (mut index, mut grant, parent) = fixture();
            index.expires_at_ms = expiry;
            grant.expires_at_ms = expiry;
            index.released = released;
            if revoked { index.revoked.push("resp_a".into()); }
            prop_assert_eq!(authority_matches(&index, &grant, "owner", "resp_a", &parent, now),
                !released && !revoked && now < expiry);
        }
    }
}
