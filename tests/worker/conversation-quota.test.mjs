import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { randomBytes } from 'node:crypto';
import { mkdir, mkdtemp, readFile, readdir } from 'node:fs/promises';
import path from 'node:path';
import test from 'node:test';
import { fileURLToPath } from 'node:url';
import OpenAI from 'openai';
import { Miniflare, convertV4MiniflareOptions } from 'miniflare';
import { createIdentityFixture } from './auth-journey.mjs';
import { createBrowserPrincipal } from './browser-principal.mjs';

const root = fileURLToPath(new URL('../../', import.meta.url));
const build = path.resolve(root, process.env.WORKER_BUILD_DIR ?? 'crates/agents-worker/build');
const limit = 256 * 1024 * 1024;

async function migrate(db) {
  for (const name of (await readdir(path.join(root, 'migrations'))).filter(name => name.endsWith('.sql')).sort()) {
    const parsed = spawnSync(process.env.MAGENTS_SCHEMA_PYTHON ?? process.env.PYTHON ?? 'python', ['-c',
      'import sys,sqlite3,json\ns: str=""; out: list[str]=[]\nfor line in sys.stdin:\n s+=line\n if sqlite3.complete_statement(s): out.append(s); s=""\nassert not s.strip(), s\nprint(json.dumps(out))'],
    { input: await readFile(path.join(root, 'migrations', name), 'utf8'), encoding: 'utf8' });
    assert.equal(parsed.status, 0, `${name}: ${parsed.stderr}`);
    await db.batch(JSON.parse(parsed.stdout).map(statement => db.prepare(statement)));
  }
}

const message = (id, text) => ({ id, type: 'message', role: 'user', content: [{ type: 'input_text', text }] });

test('compiled Rust Worker: retained Conversation capacity and publication recovery', { timeout: 90000 }, async t => {
  const encryptionKey = randomBytes(32);
  const identity = createIdentityFixture();
  const deployment = JSON.parse(await readFile(path.join(root, 'wrangler.jsonc'), 'utf8'));
  await mkdir(path.join(root, '.temp'), { recursive: true });
  const directory = await mkdtemp(path.join(root, '.temp/conversation-quota-'));
  let externalCalls = 0;
  const mf = new Miniflare(convertV4MiniflareOptions({
    modulesRoot: build,
    modules: [
      { type: 'ESModule', path: path.join(build, 'worker/shim.mjs'),
        contents: await readFile(new URL('./conversation-quota-shim.mjs', import.meta.url), 'utf8') },
      { type: 'ESModule', path: path.join(build, 'index.js') },
      { type: 'CompiledWasm', path: path.join(build, 'index_bg.wasm') },
    ],
    compatibilityDate: '2026-10-23', compatibilityFlags: ['enable_request_signal'],
    resourcePersistencePath: path.join(directory, 'storage'),
    d1Databases: ['DB'], r2Buckets: ['WORKSPACES', 'CONVERSATIONS', 'DIAGNOSTICS'],
    durableObjects: {
      SESSIONS: { className: 'Session', useSQLite: true },
      AUTH_SESSIONS: { className: 'AuthSession', useSQLite: true },
      CONVERSATION_STATE: { className: 'ConversationState', useSQLite: true },
    },
    ratelimits: Object.fromEntries(['INGRESS', 'ACCOUNT', 'MUTATION'].map((name, index) => [
      `${name}_RATE_LIMITER`, { namespace_id: String(index + 1), simple: { limit: 1000, period: 60 } },
    ])),
    bindings: {
      ENVIRONMENT: 'test', APP_ORIGIN: 'https://agents.test', ALLOWED_ORIGIN: 'https://agents.test',
      DATA_ENCRYPTION_KEY: encryptionKey.toString('base64url'),
      IDENTITY_ISSUER: 'https://identity.test', IDENTITY_WEB_CLIENT_ID: 'test-dashboard',
      IDENTITY_NATIVE_CLIENT_ID: 'test-native', IDENTITY_REDIRECT_URI: 'https://agents.test/auth/callback',
      IDENTITY_NATIVE_REDIRECT_URI: 'http://127.0.0.1/callback',
      IDENTITY_CLIENT_PRIVATE_JWK: JSON.stringify(identity.privateJwk),
      BILLING_PRODUCT_ID: 'agents', BILLING_MODE: 'test',
      MODEL_REGISTRY_JSON: '[]', MODEL_PRICING_JSON: '{}',
      RESOURCE_PRICING_JSON: deployment.env.staging.vars.RESOURCE_PRICING_JSON,
      RUN_RESOURCE_RESERVATION_MICROUSD: '20000', FREE_GRANT_BUDGET_MICROUSD_PER_DAY: '100000000',
    },
    async outboundService(request) {
      externalCalls++;
      throw new Error(`Quota journey must not call external service ${new URL(request.url).origin}`);
    },
  }));
  t.after(() => mf.dispose());
  const db = await mf.getD1Database('DB');
  await migrate(db);
  const owner = await createBrowserPrincipal({ mf, db, encryptionKey, owner: 'quota-owner' });
  const other = await createBrowserPrincipal({ mf, db, encryptionKey, owner: 'quota-other' });
  const client = key => new OpenAI({ apiKey: key, baseURL: 'https://agents.test/v1', maxRetries: 0, timeout: 12000,
    fetch: (input, init) => mf.dispatchFetch(input, init) });
  const sdk = client(owner.apiKey);
  const stranger = client(other.apiKey);
  async function counters(account = owner.owner) {
    return await db.prepare('SELECT reserved_bytes,retained_bytes FROM conversation_capacity_accounts WHERE account_id=?').bind(account).first();
  }
  async function control(id, endpoint, body) {
    const namespace = await mf.getDurableObjectNamespace('CONVERSATION_STATE');
    return namespace.get(namespace.idFromName(`${owner.owner}:${id}`)).fetch(`https://quota.fixture${endpoint}`, {
      method: body ? 'POST' : 'GET', ...(body ? { body: JSON.stringify(body), headers: { 'Content-Type': 'application/json' } } : {}),
    });
  }

  await t.test('immutable bodies count once, metadata uses bounded high-water allocation', async () => {
    const body = { metadata: { label: 'quota 雪' }, items: [message('item_quota_seed', 'A retained seed.')] };
    const options = { headers: { 'Idempotency-Key': 'quota-create' } };
    const conversation = await sdk.conversations.create(body, options);
    const before = await counters();
    assert.equal(before.reserved_bytes, 0);
    assert.ok(before.retained_bytes > 4096, 'Both one header block and actual seed body are charged');
    assert.deepEqual(await sdk.conversations.create(body, options), conversation);
    assert.deepEqual(await counters(), before, 'Idempotent creation cannot charge the same immutable bodies twice');
    for (let n = 0; n < 6; n++) await sdk.conversations.update(conversation.id, { metadata: { label: `small-${n}` } });
    assert.deepEqual(await counters(), before, 'Small metadata overwrites reuse the retained header allocation');
    await sdk.conversations.items.delete('item_quota_seed', { conversation_id: conversation.id });
    await sdk.conversations.delete(conversation.id);
    assert.deepEqual(await counters(), before, 'Visibility deletion must not refund retained user payloads');
    const retained = await db.prepare("SELECT COUNT(*) AS n FROM conversation_capacity_objects WHERE account_id=? AND state='retained'").bind(owner.owner).first();
    assert.equal(retained.n, 2, 'One immutable body and one header block, not one header per mutation');
    const purge = key => mf.dispatchFetch('https://agents.test/v1/conversations/' + conversation.id + '/purge', {
      method: 'POST', headers: { Authorization: 'Bearer ' + key },
    });
    assert.equal((await purge(other.apiKey)).status, 404);
    assert.equal((await purge(owner.apiKey)).status, 200);
    const purged = await counters();
    assert.equal(purged.reserved_bytes, 0);
    assert.equal(purged.retained_bytes, 4096);
    await assert.rejects(sdk.conversations.items.retrieve('item_quota_seed', { conversation_id: conversation.id }), error => error.status === 404);
    await mf.unsafeEvictDurableObject('', 'ConversationState', { name: owner.owner + ':' + conversation.id });
    assert.equal((await purge(owner.apiKey)).status, 200);
    assert.deepEqual(await counters(), purged, 'Cold purge retry cannot refund the retained body twice');
  });

  await t.test('known header publication survives lost acknowledgments and quota commit recovers cold', async () => {
    const conversation = await sdk.conversations.create();
    await control(conversation.id, '/__quota/fault', { phase: 'header' });
    await sdk.conversations.items.create(conversation.id, { items: [message('item_header_ack', 'Header already committed.')] });
    assert.equal((await sdk.conversations.items.list(conversation.id)).data.length, 1);
    assert.equal((await counters()).reserved_bytes, 0);

    await control(conversation.id, '/__quota/fault', { phase: 'commit' });
    const pending = await sdk.conversations.items.create(conversation.id,
      { items: [message('item_commit_ack', 'Quota acknowledgment is unknown.')] }).withResponse();
    assert.equal(pending.response.status, 200, 'A definite mutation is not converted to a duplicate-prone HTTP500');
    assert.equal(pending.response.headers.get('X-Agents-Conversation-Capacity-Status'), 'pending');
    assert.ok((await counters()).reserved_bytes > 0, 'Unknown acknowledgment retains capacity until recovery');
    const state = await (await control(conversation.id, '/__quota/state')).json();
    assert.ok(state.intent);
    await mf.unsafeEvictDurableObject('', 'ConversationState', { name: `${owner.owner}:${conversation.id}` });
    const page = await sdk.conversations.items.list(conversation.id, { order: 'asc' });
    assert.deepEqual(page.data.map(item => item.id), ['item_header_ack', 'item_commit_ack']);
    assert.equal((await counters()).reserved_bytes, 0);
    assert.equal((await (await control(conversation.id, '/__quota/state')).json()).intent, null);
    assert.ok((await db.prepare("SELECT COUNT(*) AS n FROM resource_observations WHERE account_id=? AND status='pending'").bind(owner.owner).first()).n > 0,
      'The injected unknown D1 measurement is retained, never fabricated as zero');
  });

  await t.test('independent actors cannot overbook the owner cap; rejected writes and tombstones do not leak capacity', async () => {
    const first = await sdk.conversations.create();
    const second = await sdk.conversations.create();
    const before = await counters();
    assert.equal(before.reserved_bytes, 0);
    // Synthetic pre-existing retained history makes the real public boundary
    // inexpensive to exercise; this is not a physical storage measurement.
    const occupied = limit - before.retained_bytes - 8192;
    assert.ok(occupied > 0);
    await db.prepare("INSERT INTO conversation_capacity_objects(account_id,conversation_id,object_id,digest,bytes,state,updated_at_ms) VALUES(?,?,?, ?,?,'reserved',0)")
      .bind(owner.owner, 'fixture-existing-history', 'retained', 'a'.repeat(64), occupied).run();
    await db.prepare("UPDATE conversation_capacity_objects SET state='retained' WHERE account_id=? AND conversation_id='fixture-existing-history'").bind(owner.owner).run();
    const writes = [first, second].map((conversation, index) => sdk.conversations.items.create(conversation.id,
      { items: [message(`item_capacity_${index}`, 'x'.repeat(6500))] }));
    const results = await Promise.allSettled(writes);
    assert.equal(results.filter(result => result.status === 'fulfilled').length, 1);
    const rejected = results.find(result => result.status === 'rejected').reason;
    assert.equal(rejected.status, 409);
    assert.equal(rejected.error.code, 'conversation_capacity_exceeded');
    assert.equal(rejected.error.capacity_budget, 'conversation_retained_logical_bytes');
    const after = await counters();
    assert.equal(after.reserved_bytes, 0, 'Known pre-publication rejection releases only unwritten candidates');
    assert.ok(after.retained_bytes <= limit);
    const pages = await Promise.all([first, second].map(conversation => sdk.conversations.items.list(conversation.id)));
    assert.deepEqual(pages.map(page => page.data.length).sort(), [0, 1]);
    const winner = pages.findIndex(page => page.data.length === 1);
    const selected = [first, second][winner];
    await sdk.conversations.items.delete(pages[winner].data[0].id, { conversation_id: selected.id });
    await sdk.conversations.delete(selected.id);
    assert.deepEqual(await counters(), after, 'Deleting retained content visibility cannot create new quota');
    const isolated = await stranger.conversations.create({ items: [message('item_other_owner', 'Independent owner budget.')] });
    assert.equal((await stranger.conversations.items.list(isolated.id)).data.length, 1);
    assert.ok((await counters(other.owner)).retained_bytes < 8192);
  });
  assert.equal(externalCalls, 0, 'No model/provider/container work is part of logical capacity admission');
});
