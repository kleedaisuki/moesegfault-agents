//! Archive identity is owner-scoped, while generations belong to each session.
use worker::{Error, ResponseBody, Result};

use super::{CheckpointStats, Manifest, OwnedArchiveRef, UsageClock, Workspace, digest_key};
use crate::workspace_blobs::{self as blobs, RefKind};

impl Manifest {
    fn archive(&self, owner: &str) -> OwnedArchiveRef {
        OwnedArchiveRef {
            account_id: owner.into(),
            object_key: self.object_key.clone(),
            sha256: self.sha256.clone(),
            format_version: self.format_version,
            generation: self.generation,
            archive_bytes: self.stats.archive_bytes,
            logical_bytes: self.stats.logical_bytes,
            entries: self.stats.entries,
        }
    }
}

impl Workspace<'_> {
    pub(super) async fn reconcile_current_archive(&self) -> Result<()> {
        let Some(manifest) = self
            .state
            .storage()
            .get::<Manifest>("workspace:manifest")
            .await?
        else {
            return Ok(());
        };
        if !shared_archive_key(&manifest.object_key) {
            return Ok(());
        }
        let archive = manifest.archive(&self.owner);
        blobs::bind_session(self.env, &self.owner, &self.session, &archive).await?;
        blobs::reconcile_session(self.env, &self.owner, &self.session, &archive.object_key).await
    }

    pub(crate) async fn seal_response_archive(
        &self,
        response_id: &str,
    ) -> Result<Option<OwnedArchiveRef>> {
        if response_id.is_empty() {
            return Err(fail("invalid_response_workspace_anchor"));
        }
        let Some(archive) = self.seal_archive().await? else {
            return Ok(None);
        };
        blobs::bind(
            self.env,
            &self.owner,
            RefKind::Response,
            response_id,
            &archive,
        )
        .await?;
        Ok(Some(archive))
    }

    pub(crate) async fn seal_connection_archive(
        &self,
        response_id: &str,
        connection_id: &str,
        expires_at_ms: u64,
    ) -> Result<Option<OwnedArchiveRef>> {
        blobs::validate_connection_lease(
            connection_id,
            response_id,
            expires_at_ms,
            crate::util::now_ms(),
        )?;
        let Some(archive) = self.seal_archive().await? else {
            return Ok(None);
        };
        blobs::bind_connection(
            self.env,
            &self.owner,
            connection_id,
            response_id,
            expires_at_ms,
            &archive,
        )
        .await?;
        Ok(Some(archive))
    }

    // Callers select the public-response or private-connection holder only after
    // capture succeeds; store=false must never briefly acquire a permanent ref.
    async fn seal_archive(&self) -> Result<Option<OwnedArchiveRef>> {
        let running = self
            .state
            .container()
            .is_some_and(|container| container.running());
        let startup = self.startup_pending().await?;
        let clock = self
            .state
            .storage()
            .get::<UsageClock>(super::CONTAINER_CLOCK)
            .await?
            .unwrap_or_default();
        super::validate_checkpoint_source(running, &clock, startup)?;
        if running && !startup {
            self.checkpoint_active().await?;
        }
        let Some(manifest) = self
            .state
            .storage()
            .get::<Manifest>("workspace:manifest")
            .await?
        else {
            return Ok(None);
        };
        let mut archive = self.owned_archive(&manifest).await?;
        if archive.object_key != manifest.object_key {
            archive.generation = self.promote_manifest(&manifest, &archive).await?;
        }
        Ok(Some(archive))
    }

    /// Admission has not reserved a run yet. Persist only a trusted seed and its
    /// strong reference; the first actual workspace use publishes/restores it.
    pub(crate) async fn import_response_archive(
        &self,
        archive: &OwnedArchiveRef,
        child_response_id: &str,
    ) -> Result<()> {
        if child_response_id.is_empty() || !self.archive_is_trusted(archive).await? {
            return Err(fail("invalid_response_workspace_anchor"));
        }
        let storage = self.state.storage();
        if let Some(existing) = storage.get::<Manifest>("workspace:manifest").await? {
            if existing.object_key == archive.object_key && existing.sha256 == archive.sha256 {
                return Ok(());
            }
            return Err(fail("workspace_branch_already_initialized"));
        }
        blobs::bind_session(self.env, &self.owner, &self.session, archive).await?;
        let manifest = Manifest {
            format_version: archive.format_version,
            object_key: archive.object_key.clone(),
            sha256: archive.sha256.clone(),
            generation: archive.generation,
            created_at_ms: crate::util::now_ms(),
            stats: CheckpointStats {
                archive_bytes: archive.archive_bytes,
                logical_bytes: archive.logical_bytes,
                entries: archive.entries,
                r2_write_operations: 0,
            },
        };
        storage.put("workspace:seed-pending", true).await?;
        storage
            .put("workspace:generation", archive.generation)
            .await?;
        storage.put("workspace:manifest", manifest).await?;
        Ok(())
    }

    pub(super) async fn valid_manifest_archive(&self, manifest: &Manifest) -> Result<bool> {
        if manifest.format_version != 1 || super::decode_sha256(&manifest.sha256).is_err() {
            return Ok(false);
        }
        if manifest
            .object_key
            .starts_with(&format!("{}/snapshots/{}/", self.prefix, manifest.sha256))
        {
            return Ok(true);
        }
        self.archive_is_trusted(&manifest.archive(&self.owner))
            .await
    }

    async fn archive_is_trusted(&self, archive: &OwnedArchiveRef) -> Result<bool> {
        archive.validate(&self.owner)?;
        Ok(blobs::find_key(self.env, &self.owner, &archive.object_key)
            .await?
            .is_some_and(|stored| {
                stored.sha256 == archive.sha256
                    && stored.format_version == archive.format_version
                    && stored.archive_bytes == archive.archive_bytes
                    && stored.logical_bytes == archive.logical_bytes
                    && stored.entries == archive.entries
            }))
    }

    /// Legacy session keys cannot be adopted: a rolling old GC knows nothing
    /// about new references. Promote once into the protected owner namespace.
    pub(super) async fn owned_archive(&self, manifest: &Manifest) -> Result<OwnedArchiveRef> {
        if let Some(mut ready) = blobs::find_content(
            self.env,
            &self.owner,
            manifest.format_version,
            &manifest.sha256,
            manifest.stats.archive_bytes,
        )
        .await?
        {
            if ready.logical_bytes != manifest.stats.logical_bytes
                || ready.entries != manifest.stats.entries
            {
                return Err(fail("workspace_blob_identity_conflict"));
            }
            ready.generation = manifest.generation;
            return Ok(ready);
        }
        // Published legacy versions are already protected from rolling old GC.
        // Adopt their exact immutable key and its existing physical clock rather
        // than creating another archive solely to change the namespace.
        if let Some(version) = crate::workspace_versions::find_content(
            self.env,
            &self.owner,
            &manifest.sha256,
            manifest.stats.archive_bytes,
        )
        .await?
        {
            if version.format_version != manifest.format_version
                || version.logical_bytes != manifest.stats.logical_bytes
                || version.entries != manifest.stats.entries
            {
                return Err(fail("workspace_blob_identity_conflict"));
            }
            let existing = OwnedArchiveRef {
                account_id: self.owner.clone(),
                object_key: version.object_key,
                sha256: version.sha256,
                format_version: version.format_version,
                generation: manifest.generation,
                archive_bytes: version.archive_bytes,
                logical_bytes: version.logical_bytes,
                entries: version.entries,
            };
            blobs::begin(self.env, &existing).await?;
            let mut ready = blobs::complete(self.env, &existing).await?;
            ready.generation = manifest.generation;
            return Ok(ready);
        }
        let mut candidate = manifest.archive(&self.owner);
        candidate.object_key = self.archive_key(&manifest.sha256, manifest.format_version);
        blobs::begin(self.env, &candidate).await?;
        let bucket = self.env.bucket("WORKSPACES")?;
        let read = self.require_r2("class_b").await?;
        let source = bucket.get(&manifest.object_key).execute().await?;
        self.record_r2(&read, "class_b").await?;
        let source = source.ok_or_else(|| fail("workspace_archive_source_missing"))?;
        if source.size() != candidate.archive_bytes {
            return Err(fail("workspace_archive_size_mismatch"));
        }
        let ResponseBody::Stream(stream) = source
            .body()
            .ok_or_else(|| fail("workspace_archive_source_missing"))?
            .response_body()?
        else {
            return Err(fail("workspace_archive_not_streamable"));
        };
        let write = self.require_r2("class_a").await?;
        let uploaded = super::Deadline::new(crate::util::now_ms().saturating_add(120_000))?
            .wait(
                bucket
                    .put(&candidate.object_key, stream)
                    .sha256(super::decode_sha256(&candidate.sha256)?)
                    .execute(),
            )
            .await?
            .ok_or_else(|| fail("workspace_archive_copy_rejected"))?;
        self.record_r2(&write, "class_a").await?;
        crate::reconcile::register_account_storage_object(
            self.env,
            &self.owner,
            &candidate.object_key,
            uploaded.size(),
            uploaded.uploaded().as_millis(),
        )
        .await?;
        if uploaded.size() != candidate.archive_bytes {
            return Err(fail("workspace_archive_size_mismatch"));
        }
        let mut ready = blobs::complete(self.env, &candidate).await?;
        ready.generation = manifest.generation;
        Ok(ready)
    }

    pub(super) fn archive_key(&self, sha: &str, format: u8) -> String {
        format!(
            "workspace-blobs/{}/{format}/{sha}/{}.tar.zst",
            digest_key(&self.owner),
            uuid::Uuid::new_v4()
        )
    }

    pub(super) async fn promote_manifest(
        &self,
        manifest: &Manifest,
        archive: &OwnedArchiveRef,
    ) -> Result<u64> {
        let mut promoted = manifest.clone();
        promoted.object_key = archive.object_key.clone();
        promoted.generation = self
            .state
            .storage()
            .get::<u64>("workspace:generation")
            .await?
            .unwrap_or(0)
            .max(manifest.generation)
            .checked_add(1)
            .ok_or_else(|| fail("workspace_generation_exhausted"))?;
        promoted.created_at_ms = crate::util::now_ms();
        self.state
            .storage()
            .put("workspace:generation", promoted.generation)
            .await?;
        self.publish_manifest(&promoted).await?;
        Ok(promoted.generation)
    }
}

pub(super) fn shared_archive_key(key: &str) -> bool {
    key.starts_with("workspace-blobs/") || key.starts_with("space-versions/")
}

fn fail(code: &str) -> Error {
    Error::RustError(code.into())
}
