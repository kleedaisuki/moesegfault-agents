//! One physical owner blob can back workspace checkpoints, immutable versions,
//! response images and file metadata. A deleting tombstone fences new holders.
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use wasm_bindgen::JsValue;
use worker::{Env, Error, Result};

const LEASE_MS: u64 = 86_400_000;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct OwnedArchiveRef {
    pub account_id: String,
    pub object_key: String,
    pub sha256: String,
    pub format_version: u8,
    pub generation: u64,
    pub archive_bytes: u64,
    pub logical_bytes: u64,
    pub entries: u64,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) enum BlobKind {
    #[serde(rename = "workspace")]
    WorkspaceArchive,
    #[serde(rename = "image")]
    ImageArtifact,
}
impl BlobKind {
    fn name(self) -> &'static str {
        match self {
            Self::WorkspaceArchive => "workspace",
            Self::ImageArtifact => "image",
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct OwnedBlobRef {
    pub account_id: String,
    pub object_key: String,
    pub sha256: String,
    pub bytes: u64,
    pub content_type: String,
    pub kind: BlobKind,
}
#[derive(Clone, Deserialize)]
struct BlobRow {
    #[serde(flatten)]
    archive: OwnedArchiveRef,
    kind: BlobKind,
    content_type: String,
}
impl BlobRow {
    fn workspace(archive: &OwnedArchiveRef) -> Self {
        Self {
            archive: archive.clone(),
            kind: BlobKind::WorkspaceArchive,
            content_type: "application/zstd".into(),
        }
    }
    fn image(blob: &OwnedBlobRef) -> Self {
        Self {
            archive: OwnedArchiveRef {
                account_id: blob.account_id.clone(),
                object_key: blob.object_key.clone(),
                sha256: blob.sha256.clone(),
                format_version: 0,
                generation: 0,
                archive_bytes: blob.bytes,
                logical_bytes: blob.bytes,
                entries: 1,
            },
            kind: blob.kind,
            content_type: blob.content_type.clone(),
        }
    }
    fn blob(&self) -> OwnedBlobRef {
        OwnedBlobRef {
            account_id: self.archive.account_id.clone(),
            object_key: self.archive.object_key.clone(),
            sha256: self.archive.sha256.clone(),
            bytes: self.archive.archive_bytes,
            content_type: self.content_type.clone(),
            kind: self.kind,
        }
    }
    fn same_object(&self, other: &Self) -> bool {
        self.kind == other.kind
            && self.content_type == other.content_type
            && self.archive.same_object(&other.archive)
    }
}
impl OwnedBlobRef {
    pub(crate) fn validate(&self, owner: &str) -> Result<()> {
        let hash = Sha256::digest(owner.as_bytes());
        let prefix = format!("owner-blobs/{hash:x}/image/{}/", self.sha256);
        let suffix = self.object_key.strip_prefix(&prefix).unwrap_or("");
        if self.kind != BlobKind::ImageArtifact
            || self.account_id != owner
            || self.bytes == 0
            || self.bytes > 16 * 1024 * 1024
            || self.sha256.len() != 64
            || !self
                .sha256
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            || !matches!(
                self.content_type.as_str(),
                "image/png" | "image/jpeg" | "image/webp"
            )
            || !uuid::Uuid::parse_str(suffix).is_ok_and(|id| id.hyphenated().to_string() == suffix)
        {
            return Err(fail("invalid_owned_image_blob"));
        }
        Ok(())
    }
}

#[derive(Clone, Copy)]
pub(crate) enum RefKind {
    Response,
    Version,
}
impl RefKind {
    fn name(self) -> &'static str {
        match self {
            Self::Response => "response",
            Self::Version => "version",
        }
    }
}

impl OwnedArchiveRef {
    fn same_object(&self, other: &Self) -> bool {
        self.account_id == other.account_id
            && self.object_key == other.object_key
            && self.sha256 == other.sha256
            && self.format_version == other.format_version
            && self.archive_bytes == other.archive_bytes
            && self.logical_bytes == other.logical_bytes
            && self.entries == other.entries
    }
    pub(crate) fn validate(&self, owner: &str) -> Result<()> {
        let digest = Sha256::digest(owner.as_bytes());
        let prefix = format!(
            "workspace-blobs/{digest:x}/{}/{}/",
            self.format_version, self.sha256
        );
        // Published legacy versions are already immutable and outside session GC.
        // Adoption keeps their original object and physical storage clock intact.
        let legacy = format!("space-versions/{digest:x}/{}/", self.sha256);
        let suffix = self
            .object_key
            .strip_prefix(&prefix)
            .or_else(|| self.object_key.strip_prefix(&legacy))
            .unwrap_or("");
        let uuid = suffix.strip_suffix(".tar.zst").unwrap_or("");
        let canonical_uuid =
            uuid::Uuid::parse_str(uuid).is_ok_and(|id| id.hyphenated().to_string() == uuid);
        if !canonical_uuid
            || self.account_id != owner
            || self.format_version != 1
            || self.sha256.len() != 64
            || !self
                .sha256
                .bytes()
                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
            || self.archive_bytes == 0
            || self.archive_bytes > 268_435_456
            || self.generation > i64::MAX as u64
            || self.logical_bytes > i64::MAX as u64
            || self.entries > i64::MAX as u64
        {
            return Err(fail("invalid_owned_workspace_archive"));
        }
        Ok(())
    }
}

/// Persist before PUT. Existing immutable keys may be adopted without charging
/// capacity again; new candidates reserve bytes atomically, including failures.
pub(crate) async fn begin(env: &Env, archive: &OwnedArchiveRef) -> Result<()> {
    archive.validate(&archive.account_id)?;
    begin_row(env, &BlobRow::workspace(archive), None).await
}
pub(crate) async fn begin_image(env: &Env, blob: &OwnedBlobRef) -> Result<()> {
    blob.validate(&blob.account_id)?;
    begin_row(env, &BlobRow::image(blob), None).await
}
pub(crate) async fn begin_image_reserved(
    env: &Env,
    blob: &OwnedBlobRef,
    operation: &str,
) -> Result<()> {
    blob.validate(&blob.account_id)?;
    if operation.is_empty() {
        return Err(fail("invalid_image_capacity_hold"));
    }
    begin_row(env, &BlobRow::image(blob), Some(operation)).await
}
/// Reserve before a paid provider invocation, not after its bytes are known.
pub(crate) async fn reserve_image_capacity(
    env: &Env,
    owner: &str,
    operation: &str,
    bytes: u64,
) -> Result<()> {
    if operation.is_empty() || bytes == 0 || bytes > 16 * 1024 * 1024 {
        return Err(fail("invalid_image_capacity_hold"));
    }
    let limits = crate::space::owner_limits(env, owner).await?;
    let expiry = crate::util::now_ms().saturating_add(LEASE_MS);
    env.d1("DB")?.prepare("INSERT INTO owner_blob_capacity_holds(account_id,operation_id,bytes,expires_at_ms) SELECT ?,?,CAST(? AS INTEGER),CAST(? AS INTEGER) WHERE EXISTS(SELECT 1 FROM owner_blob_capacity_holds WHERE account_id=? AND operation_id=? AND bytes=CAST(? AS INTEGER)) OR (SELECT COALESCE(SUM(bytes),0) FROM (SELECT MAX(bytes) AS bytes FROM space_storage_capacity WHERE account_id=? GROUP BY object_key))+CAST(? AS INTEGER)<=CAST(? AS INTEGER) ON CONFLICT(account_id,operation_id) DO UPDATE SET expires_at_ms=MAX(owner_blob_capacity_holds.expires_at_ms,excluded.expires_at_ms) WHERE owner_blob_capacity_holds.bytes=excluded.bytes")
        .bind(&[text(owner),text(operation),text(&bytes.to_string()),text(&expiry.to_string()),text(owner),text(operation),text(&bytes.to_string()),text(owner),text(&bytes.to_string()),text(&limits.bytes.to_string())])?.run().await?;
    #[derive(Deserialize)]
    struct Hold {
        bytes: u64,
    }
    let held = env
        .d1("DB")?
        .prepare(
            "SELECT bytes FROM owner_blob_capacity_holds WHERE account_id=? AND operation_id=?",
        )
        .bind(&[text(owner), text(operation)])?
        .first::<Hold>(None)
        .await?;
    if !held.is_some_and(|h| h.bytes == bytes) {
        return Err(fail("space_quota_exceeded"));
    }
    Ok(())
}
pub(crate) async fn release_image_capacity(env: &Env, owner: &str, operation: &str) -> Result<()> {
    env.d1("DB")?
        .prepare("DELETE FROM owner_blob_capacity_holds WHERE account_id=? AND operation_id=?")
        .bind(&[text(owner), text(operation)])?
        .run()
        .await?;
    Ok(())
}
async fn begin_row(env: &Env, row: &BlobRow, reservation: Option<&str>) -> Result<()> {
    let a = &row.archive;
    let limits = crate::space::owner_limits(env, &a.account_id).await?;
    let mut args = values(a, Some(crate::util::now_ms().saturating_add(LEASE_MS)));
    args.extend([
        text(row.kind.name()),
        text(&row.content_type),
        text(&a.account_id),
        text(&a.object_key),
        text(&a.archive_bytes.to_string()),
        text(&a.account_id),
        text(&a.archive_bytes.to_string()),
        text(&limits.bytes.to_string()),
    ]);
    let operation = reservation.unwrap_or("");
    args.extend([
        text(operation),
        text(&a.account_id),
        text(operation),
        text(&a.archive_bytes.to_string()),
        text(&crate::util::now_ms().to_string()),
        text(&a.account_id),
        text(&a.object_key),
    ]);
    // Arguments 12..17 remain the owner/key/size capacity tuple, followed by
    // the optional provider hold. Numbering keeps this single engine shared.
    let db = env.d1("DB")?;
    let insert=db.prepare("INSERT INTO workspace_blobs(object_key,account_id,sha256,format_version,generation,archive_bytes,logical_bytes,entries,status,lease_until_ms,kind,content_type) SELECT ?1,?2,?3,CAST(?4 AS INTEGER),CAST(?5 AS INTEGER),CAST(?6 AS INTEGER),CAST(?7 AS INTEGER),CAST(?8 AS INTEGER),'uploading',CAST(?9 AS INTEGER),?10,?11 WHERE (?18='' OR EXISTS(SELECT 1 FROM owner_blob_capacity_holds WHERE account_id=?19 AND operation_id=?20 AND bytes>=CAST(?21 AS INTEGER) AND expires_at_ms>CAST(?22 AS INTEGER)) OR EXISTS(SELECT 1 FROM workspace_blobs WHERE account_id=?23 AND object_key=?24)) AND (EXISTS(SELECT 1 FROM space_storage_capacity WHERE account_id=?12 AND object_key=?13 AND bytes>=CAST(?14 AS INTEGER)) OR (SELECT COALESCE(SUM(bytes),0) FROM (SELECT MAX(bytes) AS bytes FROM space_storage_capacity WHERE account_id=?15 AND object_key!=('capacity-hold:'||?18) GROUP BY object_key))+CAST(?16 AS INTEGER)<=CAST(?17 AS INTEGER)) ON CONFLICT(object_key) DO NOTHING")
        .bind(&args)?;
    let release=db.prepare("DELETE FROM owner_blob_capacity_holds WHERE account_id=? AND operation_id=? AND EXISTS(SELECT 1 FROM workspace_blobs WHERE account_id=? AND object_key=? AND sha256=? AND archive_bytes=CAST(? AS INTEGER) AND kind='image' AND content_type=? AND status!='deleting')")
        .bind(&[text(&a.account_id),text(operation),text(&a.account_id),text(&a.object_key),text(&a.sha256),text(&a.archive_bytes.to_string()),text(&row.content_type)])?;
    db.batch(vec![insert, release]).await?;
    let stored = any_row(env, &a.account_id, &a.object_key).await?;
    if !stored
        .as_ref()
        .is_some_and(|stored| stored.same_object(row))
    {
        return Err(fail(if stored.is_none() {
            "space_quota_exceeded"
        } else {
            "workspace_blob_identity_conflict"
        }));
    }
    Ok(())
}
async fn any_row(env: &Env, owner: &str, key: &str) -> Result<Option<BlobRow>> {
    env.d1("DB")?.prepare("SELECT * FROM workspace_blobs WHERE account_id=? AND object_key=? AND status!='deleting'")
        .bind(&[text(owner),text(key)])?.first(None).await
}
/// Only call after R2 checksum/size verification and physical clock registration.
pub(crate) async fn complete(env: &Env, archive: &OwnedArchiveRef) -> Result<OwnedArchiveRef> {
    archive.validate(&archive.account_id)?;
    Ok(complete_row(env, &BlobRow::workspace(archive))
        .await?
        .archive)
}
pub(crate) async fn complete_image(env: &Env, blob: &OwnedBlobRef) -> Result<OwnedBlobRef> {
    blob.validate(&blob.account_id)?;
    Ok(complete_row(env, &BlobRow::image(blob)).await?.blob())
}
async fn complete_row(env: &Env, row: &BlobRow) -> Result<BlobRow> {
    let a = &row.archive;
    let stored = any_row(env, &a.account_id, &a.object_key).await?;
    if !stored
        .as_ref()
        .is_some_and(|stored| stored.same_object(row))
    {
        return Err(fail("workspace_blob_identity_conflict"));
    }
    env.d1("DB")?.prepare("UPDATE workspace_blobs SET status='ready',lease_until_ms=CAST(? AS INTEGER) WHERE object_key=? AND account_id=? AND status='uploading' AND lease_until_ms>CAST(? AS INTEGER) AND NOT EXISTS(SELECT 1 FROM workspace_blobs WHERE account_id=? AND format_version=CAST(? AS INTEGER) AND sha256=? AND archive_bytes=CAST(? AS INTEGER) AND kind=? AND content_type=? AND status='ready')")
        .bind(&[text(&crate::util::now_ms().saturating_add(LEASE_MS).to_string()),text(&a.object_key),text(&a.account_id),text(&crate::util::now_ms().to_string()),text(&a.account_id),text(&a.format_version.to_string()),text(&a.sha256),text(&a.archive_bytes.to_string()),text(row.kind.name()),text(&row.content_type)])?.run().await?;
    env.d1("DB")?.prepare("SELECT * FROM workspace_blobs WHERE account_id=? AND format_version=CAST(? AS INTEGER) AND sha256=? AND archive_bytes=CAST(? AS INTEGER) AND kind=? AND content_type=? AND status='ready'")
        .bind(&[text(&a.account_id),text(&a.format_version.to_string()),text(&a.sha256),text(&a.archive_bytes.to_string()),text(row.kind.name()),text(&row.content_type)])?.first(None).await?.ok_or_else(||fail("workspace_blob_upload_expired"))
}
pub(crate) async fn find_image(
    env: &Env,
    owner: &str,
    sha: &str,
    bytes: u64,
    content_type: &str,
) -> Result<Option<OwnedBlobRef>> {
    Ok(env.d1("DB")?.prepare("SELECT * FROM workspace_blobs WHERE account_id=? AND kind='image' AND sha256=? AND archive_bytes=CAST(? AS INTEGER) AND content_type=? AND status='ready'")
        .bind(&[text(owner),text(sha),text(&bytes.to_string()),text(content_type)])?.first::<BlobRow>(None).await?.map(|r|r.blob()))
}
pub(crate) async fn find_image_key(
    env: &Env,
    owner: &str,
    key: &str,
) -> Result<Option<OwnedBlobRef>> {
    Ok(env.d1("DB")?.prepare("SELECT * FROM workspace_blobs WHERE account_id=? AND object_key=? AND kind='image' AND status='ready'")
        .bind(&[text(owner),text(key)])?.first::<BlobRow>(None).await?.map(|r|r.blob()))
}
pub(crate) async fn bind_image_response(
    env: &Env,
    owner: &str,
    response: &str,
    artifact: &str,
    blob: &OwnedBlobRef,
) -> Result<()> {
    blob.validate(owner)?;
    if response.is_empty() || artifact.is_empty() {
        return Err(fail("invalid_workspace_blob_ref"));
    }
    if find_image_key(env, owner, &blob.object_key).await?.as_ref() != Some(blob) {
        return Err(fail("workspace_blob_identity_conflict"));
    }
    env.d1("DB")?.prepare("INSERT INTO workspace_blob_refs(account_id,kind,ref_id,object_key,response_id) VALUES(?,'response-artifact',?,?,?) ON CONFLICT(account_id,kind,ref_id) DO UPDATE SET object_key=excluded.object_key")
        .bind(&[text(owner),text(artifact),text(&blob.object_key),text(response)])?.run().await?;
    Ok(())
}

pub(crate) async fn bind_image_file(
    env: &Env,
    owner: &str,
    file_id: &str,
    blob: &OwnedBlobRef,
) -> Result<()> {
    blob.validate(owner)?;
    if find_image_key(env, owner, &blob.object_key).await?.as_ref() != Some(blob) {
        return Err(fail("workspace_blob_identity_conflict"));
    }
    let result=env.d1("DB")?.prepare("INSERT INTO workspace_blob_refs(account_id,kind,ref_id,object_key) SELECT ?,'space-file',?,? WHERE EXISTS(SELECT 1 FROM space_files WHERE account_id=? AND id=? AND blob_key=? AND status IN ('uploading','processed','deleting')) ON CONFLICT(account_id,kind,ref_id) DO UPDATE SET object_key=excluded.object_key")
        .bind(&[text(owner),text(file_id),text(&blob.object_key),text(owner),text(file_id),text(&blob.object_key)])?.run().await?;
    if result.meta()?.and_then(|m| m.changes).unwrap_or(0) == 0 {
        return Err(fail("space_file_blob_owner_mismatch"));
    }
    Ok(())
}
pub(crate) async fn release_image_file(env: &Env, owner: &str, file_id: &str) -> Result<()> {
    env.d1("DB")?.prepare("DELETE FROM workspace_blob_refs WHERE account_id=? AND kind='space-file' AND ref_id=? AND NOT EXISTS(SELECT 1 FROM space_files WHERE account_id=? AND id=? AND status IN ('uploading','processed','deleting'))")
        .bind(&[text(owner),text(file_id),text(owner),text(file_id)])?.run().await?;
    Ok(())
}

pub(crate) async fn find_content(
    env: &Env,
    owner: &str,
    format: u8,
    sha: &str,
    bytes: u64,
) -> Result<Option<OwnedArchiveRef>> {
    env.d1("DB")?.prepare("SELECT * FROM workspace_blobs WHERE account_id=? AND format_version=CAST(? AS INTEGER) AND sha256=? AND archive_bytes=CAST(? AS INTEGER) AND kind='workspace' AND status='ready'")
        .bind(&[text(owner),text(&format.to_string()),text(sha),text(&bytes.to_string())])?.first(None).await
}
pub(crate) async fn find_key(env: &Env, owner: &str, key: &str) -> Result<Option<OwnedArchiveRef>> {
    env.d1("DB")?
        .prepare(
            "SELECT * FROM workspace_blobs WHERE account_id=? AND object_key=? AND kind='workspace' AND status='ready'",
        )
        .bind(&[text(owner), text(key)])?
        .first(None)
        .await
}

/// The insert/update and GC claim serialize in D1. A claim wins or this reference
/// wins; neither side relies on a stale read of a reference count.
pub(crate) async fn bind(
    env: &Env,
    owner: &str,
    kind: RefKind,
    id: &str,
    archive: &OwnedArchiveRef,
) -> Result<()> {
    archive.validate(owner)?;
    if id.is_empty() {
        return Err(fail("invalid_workspace_blob_ref"));
    }
    let stored = find_key(env, owner, &archive.object_key).await?;
    if !stored
        .as_ref()
        .is_some_and(|stored| stored.same_object(archive))
    {
        return Err(fail("workspace_blob_identity_conflict"));
    }
    env.d1("DB")?.prepare("INSERT INTO workspace_blob_refs(account_id,kind,ref_id,object_key) VALUES(?,?,?,?) ON CONFLICT(account_id,kind,ref_id) DO UPDATE SET object_key=excluded.object_key")
        .bind(&[text(owner),text(kind.name()),text(id),text(&archive.object_key)])?.run().await?;
    Ok(())
}
/// Acquire before exporting an archive anchor. It bridges parent deletion and
/// child admission without expiring any user-visible response or version ref.
pub(crate) async fn bind_transfer(
    env: &Env,
    owner: &str,
    child_response: &str,
    archive: &OwnedArchiveRef,
) -> Result<()> {
    archive.validate(owner)?;
    if child_response.is_empty() {
        return Err(fail("invalid_workspace_blob_ref"));
    }
    let stored = find_key(env, owner, &archive.object_key).await?;
    if !stored
        .as_ref()
        .is_some_and(|stored| stored.same_object(archive))
    {
        return Err(fail("workspace_blob_identity_conflict"));
    }
    env.d1("DB")?.prepare("INSERT INTO workspace_blob_refs(account_id,kind,ref_id,object_key,lease_until_ms) VALUES(?,'fork-transfer',?,?,CAST(? AS INTEGER)) ON CONFLICT(account_id,kind,ref_id) DO UPDATE SET object_key=excluded.object_key,lease_until_ms=MAX(workspace_blob_refs.lease_until_ms,excluded.lease_until_ms)")
        .bind(&[text(owner),text(child_response),text(&archive.object_key),text(&crate::util::now_ms().saturating_add(LEASE_MS).to_string())])?.run().await?;
    Ok(())
}
/// Child current ref must be committed before releasing the transfer lease.
pub(crate) async fn release_transfer(env: &Env, owner: &str, child_response: &str) -> Result<()> {
    env.d1("DB")?.prepare("DELETE FROM workspace_blob_refs WHERE account_id=? AND kind='fork-transfer' AND ref_id=?")
        .bind(&[text(owner),text(child_response)])?.run().await?;
    Ok(())
}

/// Keep both old and candidate archives alive until pointer publication commits.
pub(crate) async fn bind_session(
    env: &Env,
    owner: &str,
    session: &str,
    archive: &OwnedArchiveRef,
) -> Result<()> {
    archive.validate(owner)?;
    let stored = find_key(env, owner, &archive.object_key).await?;
    if !stored
        .as_ref()
        .is_some_and(|stored| stored.same_object(archive))
    {
        return Err(fail("workspace_blob_identity_conflict"));
    }
    let id = format!("{session}:{}", archive.object_key);
    env.d1("DB")?.prepare("INSERT INTO workspace_blob_refs(account_id,kind,ref_id,session_id,object_key) VALUES(?,'session-current',?,?,?) ON CONFLICT(account_id,kind,ref_id) DO UPDATE SET object_key=excluded.object_key")
        .bind(&[text(owner),text(&id),text(session),text(&archive.object_key)])?.run().await?;
    Ok(())
}
/// Only call after both R2 pointer and DO state commit to the retained key.
pub(crate) async fn reconcile_session(
    env: &Env,
    owner: &str,
    session: &str,
    keep_key: &str,
) -> Result<()> {
    if find_key(env, owner, keep_key).await?.is_none() {
        return Err(fail("workspace_blob_reference_unavailable"));
    }
    env.d1("DB")?.prepare("DELETE FROM workspace_blob_refs WHERE account_id=? AND kind='session-current' AND session_id=? AND object_key!=? AND EXISTS(SELECT 1 FROM workspace_blob_refs WHERE account_id=? AND kind='session-current' AND session_id=? AND object_key=?)")
        .bind(&[text(owner),text(session),text(keep_key),text(owner),text(session),text(keep_key)])?.run().await?;
    Ok(())
}
/// Call only after the owning DO has durably tombstoned the response. Other
/// branches retain their own anchors; published user versions are never released.
pub(crate) async fn release_response(env: &Env, owner: &str, response: &str) -> Result<()> {
    env.d1("DB")?
        .prepare(
            "DELETE FROM workspace_blob_refs WHERE account_id=? AND ((kind='response' AND ref_id=?) OR (kind='response-artifact' AND response_id=?))",
        )
        .bind(&[text(owner), text(response), text(response)])?
        .run()
        .await?;
    Ok(())
}
pub(crate) async fn referenced(env: &Env, key: &str) -> Result<bool> {
    #[derive(Deserialize)]
    struct Found {
        object_key: String,
    }
    Ok(env
        .d1("DB")?
        .prepare("SELECT object_key FROM workspace_blob_refs WHERE object_key=? LIMIT 1")
        .bind(&[text(key)])?
        .first::<Found>(None)
        .await?
        .is_some_and(|r| !r.object_key.is_empty()))
}

/// Metadata deletion can commit before a file-reference release. Reconcile a
/// bounded indexed batch; time never expires an active file or response holder.
async fn reconcile_file_refs(env: &Env, now: u64) -> Result<()> {
    #[derive(Deserialize)]
    struct FileRef {
        account_id: String,
        ref_id: String,
        object_key: String,
    }
    let rows=env.d1("DB")?.prepare("SELECT account_id,ref_id,object_key FROM workspace_blob_refs WHERE kind='space-file' AND maintenance_due_ms<=CAST(? AS INTEGER) ORDER BY maintenance_due_ms,rowid LIMIT 64")
        .bind(&[text(&now.to_string())])?.all().await?.results::<FileRef>()?;
    for row in rows {
        env.d1("DB")?.prepare("DELETE FROM workspace_blob_refs WHERE account_id=? AND kind='space-file' AND ref_id=? AND NOT EXISTS(SELECT 1 FROM space_files WHERE account_id=? AND id=? AND blob_key=? AND status IN ('uploading','processed','deleting'))")
            .bind(&[text(&row.account_id),text(&row.ref_id),text(&row.account_id),text(&row.ref_id),text(&row.object_key)])?.run().await?;
        env.d1("DB")?.prepare("UPDATE workspace_blob_refs SET maintenance_due_ms=CAST(? AS INTEGER) WHERE account_id=? AND kind='space-file' AND ref_id=?")
            .bind(&[text(&now.saturating_add(LEASE_MS).to_string()),text(&row.account_id),text(&row.ref_id)])?.run().await?;
    }
    Ok(())
}

/// Returns claimed tombstones, including prior claims whose external delete must
/// be retried. Only this returned set is safe to delete from R2.
pub(crate) async fn claim_expired(env: &Env, limit: usize) -> Result<Vec<OwnedArchiveRef>> {
    let now = crate::util::now_ms();
    reconcile_file_refs(env, now).await?;
    env.d1("DB")?.prepare("DELETE FROM workspace_blob_refs WHERE rowid IN (SELECT rowid FROM workspace_blob_refs WHERE kind='fork-transfer' AND lease_until_ms<=CAST(? AS INTEGER) ORDER BY lease_until_ms,rowid LIMIT 64)")
        .bind(&[text(&now.to_string())])?.run().await?;
    env.d1("DB")?.prepare("DELETE FROM owner_blob_capacity_holds WHERE rowid IN (SELECT rowid FROM owner_blob_capacity_holds WHERE expires_at_ms<=CAST(? AS INTEGER) ORDER BY expires_at_ms,rowid LIMIT 64)")
        .bind(&[text(&now.to_string())])?.run().await?;
    let rows=env.d1("DB")?.prepare("SELECT * FROM workspace_blobs WHERE lease_until_ms<=CAST(? AS INTEGER) ORDER BY lease_until_ms,object_key LIMIT ?")
        .bind(&[text(&now.to_string()),text(&limit.clamp(1,64).to_string())])?.all().await?.results::<OwnedArchiveRef>()?;
    let mut claimed = Vec::new();
    for row in rows {
        let result=env.d1("DB")?.prepare("UPDATE workspace_blobs SET status='deleting',lease_until_ms=CAST(? AS INTEGER) WHERE object_key=? AND lease_until_ms<=CAST(? AS INTEGER) AND NOT EXISTS(SELECT 1 FROM workspace_blob_refs WHERE object_key=workspace_blobs.object_key) AND NOT EXISTS(SELECT 1 FROM workspace_versions WHERE object_key=workspace_blobs.object_key) AND NOT EXISTS(SELECT 1 FROM space_files WHERE blob_key=workspace_blobs.object_key AND status IN ('uploading','processed','deleting'))")
            .bind(&[text(&now.saturating_add(60_000).to_string()),text(&row.object_key),text(&now.to_string())])?.run().await?;
        if result.meta()?.and_then(|m| m.changes).unwrap_or(0) > 0 {
            claimed.push(row);
        } else {
            env.d1("DB")?.prepare("UPDATE workspace_blobs SET lease_until_ms=CAST(? AS INTEGER) WHERE object_key=? AND status!='deleting' AND lease_until_ms<=CAST(? AS INTEGER) AND (EXISTS(SELECT 1 FROM workspace_blob_refs WHERE object_key=workspace_blobs.object_key) OR EXISTS(SELECT 1 FROM workspace_versions WHERE object_key=workspace_blobs.object_key) OR EXISTS(SELECT 1 FROM space_files WHERE blob_key=workspace_blobs.object_key AND status IN ('uploading','processed','deleting')))")
                .bind(&[text(&now.saturating_add(LEASE_MS).to_string()),text(&row.object_key),text(&now.to_string())])?.run().await?;
        }
    }
    Ok(claimed)
}
pub(crate) async fn finish_delete(env: &Env, key: &str) -> Result<()> {
    env.d1("DB")?.prepare("DELETE FROM workspace_blobs WHERE object_key=? AND status='deleting' AND NOT EXISTS(SELECT 1 FROM workspace_blob_refs WHERE object_key=workspace_blobs.object_key)")
        .bind(&[text(key)])?.run().await?;
    Ok(())
}
fn values(a: &OwnedArchiveRef, lease: Option<u64>) -> Vec<JsValue> {
    let mut values = vec![
        text(&a.object_key),
        text(&a.account_id),
        text(&a.sha256),
        text(&a.format_version.to_string()),
        text(&a.generation.to_string()),
        text(&a.archive_bytes.to_string()),
        text(&a.logical_bytes.to_string()),
        text(&a.entries.to_string()),
    ];
    if let Some(lease) = lease {
        values.push(text(&lease.to_string()));
    }
    values
}
fn text(value: &str) -> JsValue {
    JsValue::from_str(value)
}
fn fail(code: &str) -> Error {
    Error::RustError(code.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn archive(owner: &str, legacy: bool) -> OwnedArchiveRef {
        let digest = Sha256::digest(owner.as_bytes());
        let sha = "ab".repeat(32);
        let prefix = if legacy {
            format!("space-versions/{digest:x}/{sha}")
        } else {
            format!("workspace-blobs/{digest:x}/1/{sha}")
        };
        OwnedArchiveRef {
            account_id: owner.into(),
            object_key: format!("{prefix}/12345678-abcd-4abc-8abc-123456789abc.tar.zst"),
            sha256: sha,
            format_version: 1,
            generation: 4,
            archive_bytes: 123,
            logical_bytes: 456,
            entries: 7,
        }
    }

    #[test]
    fn image_refs_have_distinct_namespace_mime_and_raw_byte_limits() {
        let owner = "alice";
        let hash = Sha256::digest(owner.as_bytes());
        let sha = "ab".repeat(32);
        let value = OwnedBlobRef {
            account_id: owner.into(),
            object_key: format!(
                "owner-blobs/{hash:x}/image/{sha}/12345678-abcd-4abc-8abc-123456789abc"
            ),
            sha256: sha,
            bytes: 16 * 1024 * 1024,
            content_type: "image/png".into(),
            kind: BlobKind::ImageArtifact,
        };
        assert!(value.validate(owner).is_ok());
        assert!(value.validate("bob").is_err());
        for mime in ["image/png", "image/jpeg", "image/webp"] {
            let mut next = value.clone();
            next.content_type = mime.into();
            assert!(next.validate(owner).is_ok());
        }
        for bytes in [0, 16 * 1024 * 1024 + 1] {
            let mut next = value.clone();
            next.bytes = bytes;
            assert!(next.validate(owner).is_err());
        }
        let mut next = value.clone();
        next.content_type = "text/html".into();
        assert!(next.validate(owner).is_err());
        next = value.clone();
        next.object_key.push_str(".png");
        assert!(next.validate(owner).is_err());
        next = value;
        next.kind = BlobKind::WorkspaceArchive;
        assert!(next.validate(owner).is_err());
    }

    #[test]
    fn checked_immutable_namespaces_accept_only_the_named_owner_and_digest() {
        for legacy in [false, true] {
            for owner in ["alice", "bob", "owner/with/slashes", "用户"] {
                let value = archive(owner, legacy);
                assert!(value.validate(owner).is_ok());
                assert!(value.validate("other-owner").is_err());
                let mut wrong = value.clone();
                wrong.sha256 = "cd".repeat(32);
                assert!(wrong.validate(owner).is_err());
                wrong = value.clone();
                wrong.account_id = "other-owner".into();
                assert!(wrong.validate(owner).is_err());
                wrong = value.clone();
                wrong.format_version = 2;
                assert!(wrong.validate(owner).is_err());
            }
        }
    }

    #[test]
    fn immutable_archive_keys_reject_aliases_nested_paths_and_session_snapshots() {
        for legacy in [false, true] {
            let value = archive("alice", legacy);
            let prefix = value.object_key.rsplit_once('/').unwrap().0;
            for suffix in [
                "",
                "12345678-abcd-4abc-8abc-123456789abc",
                "12345678-ABCD-4ABC-8ABC-123456789ABC.tar.zst",
                "12345678abcd4abc8abc123456789abc.tar.zst",
                "../12345678-abcd-4abc-8abc-123456789abc.tar.zst",
                "12345678-abcd-4abc-8abc-123456789abc.tar.zst/extra",
                "12345678-abcd-4abc-8abc-123456789abc.tar.zst?query",
            ] {
                let mut wrong = value.clone();
                wrong.object_key = format!("{prefix}/{suffix}");
                assert!(wrong.validate("alice").is_err(), "accepted {suffix}");
            }
            let mut wrong = value;
            wrong.object_key = "workspaces/owner/session/snapshots/sha/archive.tar.zst".into();
            assert!(wrong.validate("alice").is_err());
        }
    }
}
