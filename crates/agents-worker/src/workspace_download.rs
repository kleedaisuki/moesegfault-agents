use agents_auth::billing::Resource;
use agents_core::WorkspaceSnapshot;
use worker::{Error, Response, Result};

use super::{Manifest, Workspace};
use crate::{
    exports::filename_id, metering, resources::ResourceMeter, resources::ResourcePricing, util,
};

const MAX_ARCHIVE_BYTES: u64 = 256 * 1024 * 1024;

pub(super) async fn checkpoint(
    workspace: &Workspace<'_>,
    expected: &WorkspaceSnapshot,
) -> Result<Response> {
    let Some(manifest) = workspace
        .state
        .storage()
        .get::<Manifest>("workspace:manifest")
        .await?
    else {
        return unavailable();
    };
    if !matches_snapshot(&manifest, expected, &workspace.prefix)
        || !workspace.valid_manifest_archive(&manifest).await?
    {
        return unavailable();
    }
    // API-key authentication does not refresh allowance windows. Downloads can
    // outlive their original run, so renew verified funding before reserving.
    crate::allowances::sync(workspace.env, &workspace.owner).await?;
    let pricing = ResourcePricing::parse(&workspace.env.var("RESOURCE_PRICING_JSON")?.to_string())?;
    let charge = pricing
        .tariff(&Resource::R2Operation, "class_b")?
        .charge(1)
        .map_err(|_| Error::RustError("invalid_resource_tariff".into()))?;
    let download_id = format!("download:{}", uuid::Uuid::new_v4());
    let snapshot_hash = format!("workspace:{}:{}", workspace.session, manifest.sha256);
    metering::reserve(
        &workspace.env.d1("DB")?,
        &workspace.owner,
        Some(&workspace.session),
        &download_id,
        &download_id,
        &snapshot_hash,
        charge.amount_microusd.get(),
        (util::now_ms() / 1000) as i64,
    )
    .await?;
    let meter = ResourceMeter::with_pricing(
        workspace.env,
        &workspace.owner,
        &workspace.session,
        &download_id,
        pricing,
    )?;
    let event = "workspace.download";
    meter
        .require_observation(event, Resource::R2Operation, "class_b")
        .await?;
    let object = workspace
        .env
        .bucket("WORKSPACES")?
        .get(&manifest.object_key)
        .execute()
        .await?;
    // A client disconnect cannot undo the already-observed R2 GET. Settle the
    // operation before handing its native body to the response stream.
    meter
        .record_observation(event, Resource::R2Operation, "class_b", 1)
        .await?;
    meter.settle_run().await?;
    let Some(object) = object else {
        return unavailable();
    };
    if object.size() != manifest.stats.archive_bytes {
        return unavailable();
    }
    let Some(body) = object.body() else {
        return unavailable();
    };
    let mut response = Response::from_body(body.response_body()?)?;
    let headers = response.headers_mut();
    headers.set("Content-Type", "application/zstd")?;
    headers.set("Content-Length", &manifest.stats.archive_bytes.to_string())?;
    headers.set("X-Snapshot-Sha256", &manifest.sha256)?;
    headers.set("X-Snapshot-Generation", &manifest.generation.to_string())?;
    headers.set(
        "X-Snapshot-Format-Version",
        &manifest.format_version.to_string(),
    )?;
    headers.set(
        "X-Snapshot-Logical-Bytes",
        &manifest.stats.logical_bytes.to_string(),
    )?;
    headers.set("X-Snapshot-Entries", &manifest.stats.entries.to_string())?;
    headers.set("X-Content-Type-Options", "nosniff")?;
    headers.set(
        "Content-Disposition",
        &format!(
            "attachment; filename=\"workspace-{}.tar.zst\"",
            filename_id(&workspace.session)
        ),
    )?;
    Ok(response)
}

fn unavailable() -> Result<Response> {
    util::json_error(
        409,
        "workspace_not_ready",
        "The committed workspace checkpoint is not available; retry after checkpoint recovery",
    )
}

fn matches_snapshot(manifest: &Manifest, expected: &WorkspaceSnapshot, prefix: &str) -> bool {
    if manifest.format_version != 1
        || expected.format_version != 1
        || expected.manifest_key != format!("{prefix}/manifest.json")
        || manifest.generation != expected.generation
        || manifest.created_at_ms != expected.created_at_ms
        || manifest.sha256 != expected.sha256
        || manifest.stats.archive_bytes != expected.archive_bytes
        || manifest.stats.logical_bytes != expected.logical_bytes
        || manifest.stats.entries != expected.entries
        || manifest.stats.archive_bytes == 0
        || manifest.stats.archive_bytes > MAX_ARCHIVE_BYTES
        || super::decode_sha256(&manifest.sha256).is_err()
    {
        return false;
    }
    if super::archives::shared_archive_key(&manifest.object_key) {
        return matches_shared_object(manifest);
    }
    let object_prefix = format!("{prefix}/snapshots/{}/", manifest.sha256);
    let Some(id) = manifest
        .object_key
        .strip_prefix(&object_prefix)
        .and_then(|key| key.strip_suffix(".tar.zst"))
    else {
        return false;
    };
    uuid::Uuid::parse_str(id).is_ok_and(|value| value.to_string() == id)
}

fn matches_shared_object(manifest: &Manifest) -> bool {
    let parts = manifest.object_key.split('/').collect::<Vec<_>>();
    let (owner, sha, id) = match parts.as_slice() {
        ["workspace-blobs", owner, "1", sha, id] | ["space-versions", owner, sha, id] => {
            (owner, sha, id)
        }
        _ => return false,
    };
    owner.len() == 64
        && owner.bytes().all(|b| b.is_ascii_hexdigit())
        && *sha == manifest.sha256.as_str()
        && id
            .strip_suffix(".tar.zst")
            .is_some_and(|id| uuid::Uuid::parse_str(id).is_ok_and(|value| value.to_string() == id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::CheckpointStats;

    fn fixture() -> (Manifest, WorkspaceSnapshot) {
        let prefix = "workspaces/owner/session";
        let sha256 = "ab".repeat(32);
        let manifest = Manifest {
            format_version: 1,
            object_key: format!(
                "{prefix}/snapshots/{sha256}/9f231001-d773-4286-afba-80f1dd9b483b.tar.zst"
            ),
            sha256: sha256.clone(),
            stats: CheckpointStats {
                archive_bytes: 123,
                logical_bytes: 456,
                entries: 2,
                r2_write_operations: 2,
            },
            generation: 3,
            created_at_ms: 100,
        };
        let snapshot = WorkspaceSnapshot {
            generation: 3,
            manifest_key: format!("{prefix}/manifest.json"),
            sha256,
            format_version: 1,
            created_at_ms: 100,
            logical_bytes: 456,
            archive_bytes: 123,
            entries: 2,
        };
        (manifest, snapshot)
    }

    #[test]
    fn snapshot_requires_exact_owner_bound_generation_and_metadata() {
        let prefix = "workspaces/owner/session";
        let (mut manifest, mut snapshot) = fixture();
        assert!(matches_snapshot(&manifest, &snapshot, prefix));
        assert!(!matches_snapshot(
            &manifest,
            &snapshot,
            "workspaces/other/session"
        ));
        snapshot.generation += 1;
        assert!(!matches_snapshot(&manifest, &snapshot, prefix));
        snapshot.generation -= 1;
        manifest.stats.archive_bytes += 1;
        assert!(!matches_snapshot(&manifest, &snapshot, prefix));
    }

    #[test]
    fn snapshot_object_key_cannot_select_another_owner_or_arbitrary_object() {
        let prefix = "workspaces/owner/session";
        for key in [
            "workspaces/other/session/snapshots/archive.tar.zst".into(),
            format!("{prefix}/snapshots/{}/../private.tar.zst", "ab".repeat(32)),
            format!("{prefix}/snapshots/{}/nested/file.tar.zst", "ab".repeat(32)),
            format!("{prefix}/snapshots/{}/not-a-uuid.tar.zst", "ab".repeat(32)),
        ] {
            let (mut manifest, snapshot) = fixture();
            manifest.object_key = key;
            assert!(!matches_snapshot(&manifest, &snapshot, prefix));
        }
    }

    #[test]
    fn shared_archive_shapes_keep_session_public_identity() {
        let prefix = "workspaces/owner/session";
        for namespace in [
            format!("workspace-blobs/{}/1", "cd".repeat(32)),
            format!("space-versions/{}", "cd".repeat(32)),
        ] {
            let (mut manifest, snapshot) = fixture();
            manifest.object_key = format!(
                "{namespace}/{}/9f231001-d773-4286-afba-80f1dd9b483b.tar.zst",
                manifest.sha256
            );
            assert!(matches_snapshot(&manifest, &snapshot, prefix));
            assert!(!matches_snapshot(
                &manifest,
                &snapshot,
                "workspaces/other/session"
            ));
            manifest.object_key.push_str("/nested");
            assert!(!matches_snapshot(&manifest, &snapshot, prefix));
        }
    }
}
