import assert from 'node:assert/strict';
import { createHash, randomBytes, randomUUID } from 'node:crypto';
import { deflateSync, crc32 } from 'node:zlib';
import { spawnSync } from 'node:child_process';
import { mkdir, mkdtemp, readFile, readdir, writeFile } from 'node:fs/promises';
import path from 'node:path';
import test from 'node:test';
import { fileURLToPath } from 'node:url';
import { Miniflare, convertV4MiniflareOptions } from 'miniflare';
import { createIdentityFixture } from './auth-journey.mjs';
import { createResponsesProviderFixture, createResponseJourneyFixture } from './responses-journey.mjs';

const root = fileURLToPath(new URL('../../', import.meta.url));
const build = path.resolve(root, process.env.WORKER_BUILD_DIR ?? 'crates/agents-worker/build');
const sha = bytes => createHash('sha256').update(bytes).digest('hex');
const origin = 'https://agents.test';

function chunk(type, bytes) {
  const tag = Buffer.from(type);
  const result = Buffer.alloc(bytes.length + 12);
  result.writeUInt32BE(bytes.length, 0);
  tag.copy(result, 4); bytes.copy(result, 8);
  result.writeUInt32BE(crc32(Buffer.concat([tag, bytes])), bytes.length + 8);
  return result;
}

// A valid small raster with a legal private ancillary chunk exercises raw-byte
// transport limits without conflating them with decompressed pixel allocation.
function png(bytes) {
  const header = Buffer.alloc(13);
  header.writeUInt32BE(2, 0); header.writeUInt32BE(2, 4); header[8] = 8; header[9] = 2;
  const ihdr = chunk('IHDR', header), idat = chunk('IDAT', deflateSync(Buffer.alloc(14))), iend = chunk('IEND', Buffer.alloc(0));
  const padding = bytes - 8 - ihdr.length - idat.length - iend.length - 12;
  assert.ok(padding >= 0);
  return Buffer.concat([Buffer.from([137, 80, 78, 71, 13, 10, 26, 10]), ihdr,
    chunk('npAD', Buffer.alloc(padding, bytes % 251)), idat, iend]);
}

async function migrate(db) {
  for (const name of (await readdir(path.join(root, 'migrations'))).filter(name => name.endsWith('.sql')).sort()) {
    const parsed = spawnSync(process.env.PYTHON ?? 'python', ['-c',
      'import sys,sqlite3,json\ns: str=""; out: list[str]=[]\nfor line in sys.stdin:\n s+=line\n if sqlite3.complete_statement(s): out.append(s); s=""\nassert not s.strip(), s\nprint(json.dumps(out))'],
    { input: await readFile(path.join(root, 'migrations', name), 'utf8'), encoding: 'utf8' });
    assert.equal(parsed.status, 0, `${name}: ${parsed.stderr}`);
    await db.batch(JSON.parse(parsed.stdout).map(statement => db.prepare(statement)));
  }
}

async function seedAsset(db, bucket, owner, response, item, partial, payload) {
  const digest = sha(payload), now = Date.now();
  const blob = { account_id: owner, object_key: `owner-blobs/${sha(owner)}/image/${digest}/${randomUUID()}`,
    sha256: digest, bytes: payload.length, content_type: 'image/png', kind: 'image' };
  await bucket.put(blob.object_key, payload, { sha256: digest, httpMetadata: { contentType: 'image/png' } });
  await db.prepare("INSERT INTO workspace_blobs(object_key,account_id,sha256,format_version,generation,archive_bytes,logical_bytes,entries,status,lease_until_ms,kind,content_type) VALUES(?,?,?,0,0,?,?,1,'ready',?,'image','image/png')")
    .bind(blob.object_key, owner, digest, payload.length, payload.length, now + 86400000).run();
  await db.prepare("INSERT INTO resource_storage_objects(object_key,account_id,session_id,size_bytes,retained_from_ms,billed_through_ms,status) VALUES(?,?,NULL,?,?,?,'active')")
    .bind(blob.object_key, owner, String(payload.length), now, now).run();
  const holder = partial === null ? `${item}:result` : `${item}:partial:${partial}`;
  await db.prepare("INSERT INTO workspace_blob_refs(account_id,kind,ref_id,object_key,response_id) VALUES(?,'response-artifact',?,?,?)")
    .bind(owner, `image:${sha(JSON.stringify([response, holder]))}`, blob.object_key, response).run();
  return { item_id: item, partial_index: partial, blob };
}

async function seedFile(db, owner, blob) {
  const now = Date.now(), space = `space_${sha(owner)}`, id = `file-${randomUUID().replaceAll('-', '')}`;
  await db.prepare("INSERT INTO spaces(id,account_id,name,created_at_ms,updated_at_ms) VALUES(?,?,'Image fixture',?,?) ON CONFLICT(account_id) DO NOTHING")
    .bind(space, owner, now, now).run();
  await db.prepare("INSERT INTO space_files(id,account_id,space_id,filename,purpose,content_type,object_key,blob_key,bytes,sha256,status,reservation_id,created_at_ms,updated_at_ms,recovery_at_ms,upload_outcome,upload_operations) VALUES(?,?,?,'checked.png','user_data','image/png',?,?,?,?,'processed','fixture-image-link',?,?,?,'stored',0)")
    .bind(id, owner, space, `space-links/${sha(owner)}/${id}`, blob.object_key, blob.bytes, blob.sha256, now, now, now).run();
  await db.prepare("INSERT INTO workspace_blob_refs(account_id,kind,ref_id,object_key) VALUES(?,'space-file',?,?)")
    .bind(owner, id, blob.object_key).run();
  return id;
}

test('compiled Rust Worker: retained image projections stay reference-only and replay exact bytes', { timeout: 120000 }, async t => {
  const encryptionKey = randomBytes(32), identity = createIdentityFixture(), providers = createResponsesProviderFixture();
  const deployment = JSON.parse(await readFile(path.join(root, 'wrangler.jsonc'), 'utf8'));
  await mkdir(path.join(root, '.temp'), { recursive: true });
  const directory = await mkdtemp(path.join(root, '.temp/image-projection-'));
  const modules = [];
  for (const [target, source] of [['shim.mjs', 'image-projection-shim.mjs'], ['workspace-blob-shim.mjs', 'workspace-blob-shim.mjs'], ['chat-recovery-shim.mjs', 'chat-recovery-shim.mjs']])
    modules.push({ type: 'ESModule', path: path.join(build, 'worker', target), contents: await readFile(new URL(source, import.meta.url), 'utf8') });
  modules.push({ type: 'ESModule', path: path.join(build, 'index.js') }, { type: 'CompiledWasm', path: path.join(build, 'index_bg.wasm') });
  const mf = new Miniflare(convertV4MiniflareOptions({ modulesRoot: build, modules,
    compatibilityDate: '2026-10-23', compatibilityFlags: ['enable_request_signal'],
    resourcePersistencePath: path.join(directory, 'storage'), d1Databases: ['DB'], r2Buckets: ['WORKSPACES', 'CONVERSATIONS', 'DIAGNOSTICS'],
    durableObjects: { SESSIONS: { className: 'Session', useSQLite: true }, AUTH_SESSIONS: { className: 'AuthSession', useSQLite: true },
      CONVERSATION_STATE: { className: 'ConversationState', useSQLite: true }, MODEL_OPERATIONS: { className: 'ModelOperation', useSQLite: true } },
    ratelimits: Object.fromEntries(['INGRESS', 'ACCOUNT', 'MUTATION'].map((name, index) => [`${name}_RATE_LIMITER`, { namespace_id: String(index + 1), simple: { limit: 1000, period: 60 } }])),
    bindings: { ENVIRONMENT: 'test', APP_ORIGIN: origin, ALLOWED_ORIGIN: origin, DATA_ENCRYPTION_KEY: encryptionKey.toString('base64url'),
      IDENTITY_ISSUER: 'https://identity.test', IDENTITY_WEB_CLIENT_ID: 'test-dashboard', IDENTITY_NATIVE_CLIENT_ID: 'test-native',
      IDENTITY_REDIRECT_URI: `${origin}/auth/callback`, IDENTITY_NATIVE_REDIRECT_URI: 'http://127.0.0.1/callback',
      IDENTITY_CLIENT_PRIVATE_JWK: JSON.stringify(identity.privateJwk), BILLING_PRODUCT_ID: 'agents', BILLING_MODE: 'test',
      MODEL_REGISTRY_JSON: '[]', MODEL_PRICING_JSON: '{}', RESOURCE_PRICING_JSON: deployment.env.staging.vars.RESOURCE_PRICING_JSON,
      RUN_RESOURCE_RESERVATION_MICROUSD: '20000', FREE_GRANT_BUDGET_MICROUSD_PER_DAY: '100000000' },
    async outboundService(request) {
      const result = await providers.handle(request);
      assert.notEqual(result, undefined, 'Projection fixtures must not call an external service');
      return result;
    },
  }));
  t.after(() => mf.dispose());
  const db = await mf.getD1Database('DB'); await migrate(db);
  async function request(method, endpoint, body, expected = 200, bearer) {
    const response = await mf.dispatchFetch(origin + endpoint, { method,
      headers: { Authorization: `Bearer ${bearer}`, 'Content-Type': 'application/json', 'CF-Connecting-IP': '192.0.2.73' },
      ...(body === undefined ? {} : { body: JSON.stringify(body) }) });
    const text = await response.text(); assert.equal(response.status, expected, `${method} ${endpoint}: unexpected HTTP status`);
    return JSON.parse(text);
  }
  const context = { mf, db, encryptionKey, providers, request, diagnostic: value => t.diagnostic(value) };
  for (const mode of ['large', 'partials']) {
    if (process.env.MAGENTS_IMAGE_CASE && process.env.MAGENTS_IMAGE_CASE !== mode) continue;
    await t.test(mode, { timeout: 60000 }, async () => {
      const scenario = await createResponseJourneyFixture(context, `image-${mode}`, [{ text: 'Checked image boundary.' }], { validate_response_schema: false });
      try {
        const created = await scenario.invoke({ input: 'Create a retained projection boundary.' });
        const responseId = created.value.id, sessionId = created.headers.get('x-agents-session');
        assert.ok(sessionId);
        const namespace = await mf.getDurableObjectNamespace('SESSIONS');
        const actor = namespace.get(namespace.idFromName(`${scenario.principal.owner}:${sessionId}`));
        const bucket = await mf.getR2Bucket('WORKSPACES'), item = `ig_${randomUUID().replaceAll('-', '')}`;
        const raw = png(mode === 'large' ? 16 * 1024 * 1024 : 4096);
        const assets = [await seedAsset(db, bucket, scenario.principal.owner, responseId, item, null, raw)];
        const previews = mode === 'partials' ? [png(1024), png(2048)] : [];
        for (let i = 0; i < previews.length; i++) assets.push(await seedAsset(db, bucket, scenario.principal.owner, responseId, item, i, previews[i]));
        const file = await seedFile(db, scenario.principal.owner, assets[0].blob);
        const seeded = await actor.fetch('https://session.internal/__test/image-projection-seed', { method: 'POST', body: JSON.stringify({ responseId, assets }) });
        assert.equal(seeded.status, 200);
        const canonical = await (await actor.fetch('https://session.internal/__test/image-projection-inspect', { method: 'POST', body: JSON.stringify({ responseId }) })).json();
        assert.ok(canonical.canonical_bytes < 65536, 'Canonical DO state must not contain the base64 body');
        assert.equal(canonical.inline_final, false); assert.equal(canonical.inline_partial, false);
        assert.equal(canonical.assets.length, assets.length);
        const check = value => {
          const image = value.output.find(output => output.id === item);
          assert.equal(image.result.length, Math.ceil(raw.length / 3) * 4);
          assert.equal(sha(Buffer.from(image.result, 'base64')), sha(raw));
          const encoded = JSON.stringify(value);
          for (const asset of assets) assert.ok(!encoded.includes(asset.blob.object_key), 'Private storage keys must never reach the wire');
        };
        check(await scenario.read(responseId));
        await request('DELETE', `/v1/files/${file}`, undefined, 200, scenario.principal.apiKey);
        await request('GET', `/v1/files/${file}`, undefined, 404, scenario.principal.apiKey);
        assert.equal(await db.prepare("SELECT COUNT(*) AS count FROM workspace_blob_refs WHERE account_id=? AND kind='space-file' AND ref_id=?")
          .bind(scenario.principal.owner, file).first('count'), 0);
        for (const asset of assets)
          await db.prepare('UPDATE workspace_blobs SET lease_until_ms=0 WHERE account_id=? AND object_key=?')
            .bind(scenario.principal.owner, asset.blob.object_key).run();
        const collected = await mf.dispatchFetch(`${origin}/__test/workspace-blob-gc`);
        assert.equal(collected.status, 200, 'Expired grace must not be the reason image bytes survive');
        await mf.unsafeEvictDurableObject('', 'Session', { name: `${scenario.principal.owner}:${sessionId}` });
        check(await scenario.read(responseId));
        const replay = await mf.dispatchFetch(`${origin}/v1/responses/${responseId}?stream=true`, { headers: { Authorization: `Bearer ${scenario.principal.apiKey}` } });
        assert.equal(replay.status, 200);
        const events = (await replay.text()).split('\n\n').map(block => block.split('\n').find(line => line.startsWith('data: '))).filter(Boolean).map(line => JSON.parse(line.slice(6)));
        check(events.findLast(event => event.type === 'response.completed').response);
        const done = events.find(event => event.type === 'response.output_item.done' && event.item.id === item);
        assert.equal(sha(Buffer.from(done.item.result, 'base64')), sha(raw));
        for (let i = 0; i < previews.length; i++) {
          const preview = events.find(event => event.type === 'response.image_generation_call.partial_image' && event.partial_image_index === i);
          assert.equal(sha(Buffer.from(preview.partial_image_b64, 'base64')), sha(previews[i]));
        }
        assert.equal(scenario.state.calls, 1, 'Observation and replay cannot regenerate the provider result');
        const refs = await db.prepare("SELECT COUNT(*) AS count FROM workspace_blob_refs WHERE account_id=? AND kind='response-artifact' AND response_id=?")
          .bind(scenario.principal.owner, responseId).first('count');
        assert.equal(refs, assets.length);
        const inspected = await namespace.get(namespace.idFromName(`${scenario.principal.owner}:${sessionId}`))
          .fetch('https://session.internal/__test/image-projection-inspect', { method: 'POST', body: JSON.stringify({ responseId }) });
        const after = await inspected.json();
        assert.equal(after.inline_final, false, 'Hydration must not persist base64 into nested terminal events');
        assert.equal(after.inline_partial, false);
        assert.ok(after.canonical_bytes < 65536);
        await writeFile(path.join(directory, `${mode}-receipt.json`), JSON.stringify({ raw_bytes: raw.length, base64_bytes: Math.ceil(raw.length / 3) * 4,
          canonical_bytes: canonical.canonical_bytes, image_slots: assets.length, get_after_files_delete: true, cold_sse_replay: true, sha256: sha(raw),
          memory_measurement: null, note: 'Actual compiled Worker path; this fixture seeds a checked retained blob and does not exercise generation or production PUT.' }, null, 2));
      } catch (error) { scenario.recordFailure(error); }
      finally { await scenario.finish(); }
    });
  }
});
