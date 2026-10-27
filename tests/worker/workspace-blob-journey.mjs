import assert from 'node:assert/strict';
import { createHash, randomUUID } from 'node:crypto';
import { mkdir, writeFile } from 'node:fs/promises';
import { createResponseJourneyFixture } from './responses-journey.mjs';
import { createBrowserPrincipal } from './browser-principal.mjs';

const sha = bytes => createHash('sha256').update(bytes).digest('hex');
const origin = 'https://agents.test';

async function measuredState(db, bucket, owner) {
  const physical = await db.prepare("SELECT COUNT(*) AS objects,COALESCE(SUM(CAST(size_bytes AS INTEGER)),0) AS bytes FROM resource_storage_objects WHERE account_id=? AND object_key LIKE 'workspace-blobs/%' AND status='active'").bind(owner).first();
  const operations = (await db.prepare("SELECT l.unit,CAST(SUM(l.quantity) AS TEXT) AS quantity FROM usage_ledger l JOIN usage_reservations r ON r.id=l.reservation_id WHERE r.account_id=? AND l.resource='r2_operation' GROUP BY l.unit").bind(owner).all()).results;
  const objects = await bucket.list({ prefix: `workspace-blobs/${sha(owner)}/` });
  assert.equal(objects.truncated, false, 'The fixture corpus must fit one actual R2 listing');
  return { archive_objects: objects.objects.length,
    archive_bytes: objects.objects.reduce((sum, object) => sum + object.size, 0),
    ledger_objects: physical.objects, ledger_bytes: physical.bytes,
    class_a: Number(operations.find(row => row.unit === 'class_a')?.quantity ?? 0),
    class_b: Number(operations.find(row => row.unit === 'class_b')?.quantity ?? 0) };
}

async function seedArchive(db, bucket, archive, payload, { candidate = false } = {}) {
  await bucket.put(archive.object_key, payload, { sha256: archive.sha256 });
  await db.prepare("INSERT INTO workspace_blobs(object_key,account_id,sha256,format_version,generation,archive_bytes,logical_bytes,entries,status,lease_until_ms) VALUES(?,?,?,?,?,?,?,?,?,0)")
    .bind(archive.object_key, archive.account_id, archive.sha256, archive.format_version, archive.generation,
      archive.archive_bytes, archive.logical_bytes, archive.entries, candidate ? 'uploading' : 'ready').run();
  const now = Date.now();
  await db.prepare("INSERT INTO resource_storage_objects(object_key,account_id,session_id,size_bytes,retained_from_ms,billed_through_ms,removed_at_ms,status) VALUES(?,?,NULL,?,?,?,NULL,'active')")
    .bind(archive.object_key, archive.account_id, String(payload.length), now, now).run();
}

async function raw(context, apiKey, endpoint, expected = 200) {
  const response = await context.mf.dispatchFetch(origin + endpoint,
    { headers: { Authorization: `Bearer ${apiKey}`, 'CF-Connecting-IP': '192.0.2.42' } });
  assert.equal(response.status, expected, `${endpoint}: unexpected HTTP status`);
  return response;
}

export async function runWorkspaceBlobJourney(context) {
  const scenario = await createResponseJourneyFixture(context, 'workspace-blob', [
    { text: 'Immutable boundary A.' }, { text: 'Original session advanced to B.' },
    { text: 'Independent historical child C.' },
  ], { validate_response_schema: false });
  const { principal } = scenario;
  const { db, mf } = context;
  const bucket = await mf.getR2Bucket('WORKSPACES');
  let receipt;
  try {
    const first = await scenario.invoke({ input: 'Create historical boundary A.' });
    const parentSession = first.headers.get('x-agents-session');
    assert.ok(parentSession);
    await scenario.invoke({ previous_response_id: first.value.id, input: 'Advance the original head to B.' });

    // This is a checked-upload seed, not a measurement of the checkpoint uploader
    // or native tar/zstd restore. Public fork/download/GC execute real Rust code.
    const payload = Buffer.from(Array.from({ length: 32771 }, (_, i) => (i * 73 + 19) & 255));
    const archive = { account_id: principal.owner,
      object_key: `workspace-blobs/${sha(principal.owner)}/1/${sha(payload)}/${randomUUID()}.tar.zst`,
      sha256: sha(payload), format_version: 1, generation: 7,
      archive_bytes: payload.length, logical_bytes: 65539, entries: 3 };
    await seedArchive(db, bucket, archive, payload);
    const versionId = `wsv_fixture_${randomUUID()}`;
    await db.prepare("INSERT INTO workspace_versions(id,account_id,session_id,operation_id,name,object_key,sha256,format_version,generation,archive_bytes,logical_bytes,entries,created_at_ms) VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?)")
      .bind(versionId, principal.owner, parentSession, `fixture:${versionId}`, 'Immutable checked archive',
        archive.object_key, archive.sha256, 1, 7, archive.archive_bytes, archive.logical_bytes, archive.entries, Date.now()).run();
    for (const [kind, refId] of [['version', versionId], ['response', first.value.id]])
      await db.prepare('INSERT INTO workspace_blob_refs(account_id,kind,ref_id,object_key) VALUES(?,?,?,?)')
        .bind(principal.owner, kind, refId, archive.object_key).run();
    const namespace = await mf.getDurableObjectNamespace('SESSIONS');
    const parent = namespace.get(namespace.idFromName(`${principal.owner}:${parentSession}`));
    const seeded = await parent.fetch('https://session.internal/__test/workspace-blob-response', {
      method: 'POST', body: JSON.stringify({ responseId: first.value.id, archive }),
    });
    assert.equal(seeded.status, 200);
    assert.equal((await seeded.json()).source_session_id, parentSession);

    const before = await measuredState(db, bucket, principal.owner);
    assert.equal(before.archive_objects, 1);
    assert.equal(before.archive_bytes, payload.length);
    assert.equal(before.ledger_objects, 1);
    assert.equal(before.ledger_bytes, payload.length);
    const child = await scenario.invoke({ previous_response_id: first.value.id, input: 'Branch historical A, not current B.' });
    const childSession = child.headers.get('x-agents-session');
    assert.ok(childSession && childSession !== parentSession, 'Historical branching must use an independent Session DO');
    assert.equal(child.value.status, 'completed');
    const afterFork = await measuredState(db, bucket, principal.owner);
    assert.equal(afterFork.archive_objects, 1, 'Import binds the same owner archive, never copies its body');
    assert.equal(afterFork.archive_bytes, before.archive_bytes);
    assert.equal(afterFork.ledger_objects, 1, 'Additional strong references cannot create another physical storage clock');
    assert.equal(afterFork.ledger_bytes, before.ledger_bytes);
    assert.equal(afterFork.class_a, before.class_a, 'Pre-admission lazy import must not write an R2 archive or manifest');
    assert.equal(afterFork.class_b, before.class_b, 'Lazy import must not GET the archive body');
    const childRefs = (await db.prepare("SELECT object_key FROM workspace_blob_refs WHERE account_id=? AND kind='session-current' AND session_id=?")
      .bind(principal.owner, childSession).all()).results;
    assert.deepEqual(childRefs.map(row => row.object_key), [archive.object_key]);
    const pointer = await bucket.get(`workspaces/${sha(principal.owner)}/${sha(childSession)}/manifest.json`);
    assert.equal(pointer, null, 'An unused lazy workspace seed does not allocate an R2 manifest');
    const childStub = namespace.get(namespace.idFromName(`${principal.owner}:${childSession}`));
    const childSeed = await childStub.fetch('https://session.internal/__test/workspace-blob-seed');
    assert.equal(childSeed.status, 200);
    const seed = await childSeed.json();
    assert.equal(seed.manifest.object_key, archive.object_key, 'The child DO durably holds its recoverable seed before execution');
    assert.equal(seed.manifest.sha256, archive.sha256);
    assert.equal(seed.pending, true);

    const downloaded = await raw(context, principal.apiKey, `/v1/space/workspace-versions/${versionId}/content`);
    assert.equal(downloaded.headers.get('x-snapshot-sha256'), archive.sha256);
    assert.deepEqual(Buffer.from(await downloaded.arrayBuffer()), payload);
    const afterDownload = await measuredState(db, bucket, principal.owner);
    assert.equal(afterDownload.class_b - afterFork.class_b, 1, 'Version download charges its actual single R2 GET');
    assert.equal(afterDownload.class_a, afterFork.class_a);
    const other = await createBrowserPrincipal({ mf, db, encryptionKey: context.encryptionKey, owner: 'workspace-blob-foreign' });
    await (await raw(context, other.apiKey, `/v1/space/workspace-versions/${versionId}/content`, 404)).text();
    await assert.rejects(db.prepare("INSERT INTO workspace_blob_refs(account_id,kind,ref_id,object_key) VALUES(?,'response','foreign',?)")
      .bind(other.owner, archive.object_key).run(), 'Another account cannot acquire an archive reference');
    await assert.rejects(db.prepare('UPDATE workspace_versions SET object_key=? WHERE id=?')
      .bind('replacement', versionId).run(), 'Published user versions remain immutable');
    await assert.rejects(db.prepare("DELETE FROM workspace_blob_refs WHERE account_id=? AND kind='version' AND ref_id=?")
      .bind(principal.owner, versionId).run(), 'Published user data cannot lose its permanent strong reference');

    const orphan = { ...archive, object_key: `workspace-blobs/${sha(principal.owner)}/1/${sha(payload)}/${randomUUID()}.tar.zst` };
    await seedArchive(db, bucket, orphan, payload, { candidate: true });
    const swept = await mf.dispatchFetch(origin + '/__test/workspace-blob-gc', { method: 'POST' });
    assert.equal(swept.status, 200);
    await swept.text();
    assert.ok(await bucket.head(archive.object_key), 'Expired leases do not expire referenced user archives');
    assert.equal(await bucket.head(orphan.object_key), null, 'Negative control: expired unreferenced candidates are physically collected');
    assert.equal(await db.prepare('SELECT status FROM resource_storage_objects WHERE object_key=?').bind(orphan.object_key).first('status'), 'removed');
    const final = await measuredState(db, bucket, principal.owner);
    assert.equal(final.ledger_bytes, payload.length);
    assert.deepEqual(Buffer.from(await (await bucket.get(archive.object_key)).arrayBuffer()), payload);
    receipt = { scope: 'seeded-checked-upload-real-public-fork-download-and-gc',
      source_sha: process.env.GITHUB_SHA ?? null, archive_bytes: payload.length,
      upload_path_measured: false, native_archive_restore_measured: false,
      before, after_fork: afterFork, after_download: afterDownload, after_gc: final,
      historical_child_distinct_session: true, exact_download_sha256: sha(payload),
      expired_unreferenced_candidate_deleted: true,
      analytical_old_commit_plus_version_bytes: payload.length * 2 };
  } catch (error) { scenario.recordFailure(error); }
  finally { await scenario.finish(); }
  await mkdir('.cache/perf/v3', { recursive: true });
  await writeFile('.cache/perf/v3/workspace-blobs.json', JSON.stringify(receipt, null, 2) + '\n');
}
