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

async function migrate(db) {
  for (const name of (await readdir(path.join(root, 'migrations'))).filter(name => name.endsWith('.sql')).sort()) {
    const parsed = spawnSync(process.env.PYTHON ?? 'python', ['-c',
      'import sys,sqlite3,json\ns: str=""; out: list[str]=[]\nfor line in sys.stdin:\n s+=line\n if sqlite3.complete_statement(s): out.append(s); s=""\nassert not s.strip(), s\nprint(json.dumps(out))'],
    { input: await readFile(path.join(root, 'migrations', name), 'utf8'), encoding: 'utf8' });
    assert.equal(parsed.status, 0, `${name}: ${parsed.stderr}`);
    await db.batch(JSON.parse(parsed.stdout).map(statement => db.prepare(statement)));
  }
}

function message(id, text) {
  return { id, type: 'message', role: 'user', content: [{ type: 'input_text', text }] };
}

async function status(promise, expected) {
  await assert.rejects(promise, error => error.status === expected, `Expected SDK status ${expected}`);
}

test('compiled Rust Worker: model-independent Conversations SDK journey', { timeout: 60000 }, async t => {
  const encryptionKey = randomBytes(32);
  const identity = createIdentityFixture();
  const deployment = JSON.parse(await readFile(path.join(root, 'wrangler.jsonc'), 'utf8'));
  await mkdir(path.join(root, '.temp'), { recursive: true });
  const directory = await mkdtemp(path.join(root, '.temp/conversations-'));
  let externalCalls = 0;
  const mf = new Miniflare(convertV4MiniflareOptions({
    modulesRoot: build,
    modules: [
      { type: 'ESModule', path: path.join(build, 'worker/shim.mjs') },
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
      throw new Error(`Conversation-only journey must not call external service ${new URL(request.url).origin}`);
    },
  }));
  t.after(() => mf.dispose());
  const db = await mf.getD1Database('DB');
  await migrate(db);
  const principal = await createBrowserPrincipal({ mf, db, encryptionKey, owner: 'conversations-owner-a' });
  const stranger = await createBrowserPrincipal({ mf, db, encryptionKey, owner: 'conversations-owner-b' });
  function client(apiKey) {
    return new OpenAI({ apiKey, baseURL: 'https://agents.test/v1', maxRetries: 0, timeout: 8000,
      fetch: (input, init) => mf.dispatchFetch(input, init) });
  }
  const sdk = client(principal.apiKey);
  const other = client(stranger.apiKey);
  let conversation;

  await t.test('create needs no model, preserves metadata and normalizes initial messages', async () => {
    conversation = await sdk.conversations.create({ metadata: { name: '探索 雪' },
      items: [message('item_initial', 'Initial context without a model.')] }, { idempotencyKey: 'conversation-create-a' });
    assert.equal(conversation.object, 'conversation');
    assert.match(conversation.id, /^conv_/);
    assert.ok(Number.isSafeInteger(conversation.created_at));
    assert.deepEqual(conversation.metadata, { name: '探索 雪' });
    assert.deepEqual(await sdk.conversations.retrieve(conversation.id), conversation);
    const item = await sdk.conversations.items.retrieve('item_initial', { conversation_id: conversation.id });
    assert.equal(item.status, 'completed');
    assert.equal(item.content[0].text, 'Initial context without a model.');
    const replay = await sdk.conversations.create({ metadata: { name: '探索 雪' },
      items: [message('item_initial', 'Initial context without a model.')] }, { idempotencyKey: 'conversation-create-a' });
    assert.deepEqual(replay, conversation);
    await status(sdk.conversations.create({ metadata: { name: 'Different parameters' } },
      { idempotencyKey: 'conversation-create-a' }), 409);
    assert.equal((await sdk.conversations.items.list(conversation.id)).data.length, 1);
    assert.equal((await db.prepare('SELECT COUNT(*) AS n FROM sessions').first()).n, 0,
      'Conversation creation must not admit a model Session or container');
  });

  await t.test('item batches retain chronological identities, default desc and cursor pagination', async () => {
    const items = ['two', 'three', 'four'].map((text, index) => message(`item_${index + 2}`, text));
    const added = await sdk.conversations.items.create(conversation.id, { items });
    assert.equal(added.object, 'list');
    assert.deepEqual(added.data.map(item => item.id), ['item_2', 'item_3', 'item_4']);
    assert.equal(added.first_id, 'item_2');
    assert.equal(added.last_id, 'item_4');
    const latest = await sdk.conversations.items.list(conversation.id, { limit: 2 });
    assert.deepEqual(latest.data.map(item => item.id), ['item_4', 'item_3']);
    assert.equal(latest.has_more, true);
    const next = await latest.getNextPage();
    assert.deepEqual(next.data.map(item => item.id), ['item_2', 'item_initial']);
    assert.equal(next.has_more, false);
    const ascending = await sdk.conversations.items.list(conversation.id, { order: 'asc', after: 'item_initial', limit: 2 });
    assert.deepEqual(ascending.data.map(item => item.id), ['item_2', 'item_3']);
    assert.equal(ascending.has_more, true);
    const maximum = Array.from({ length: 20 }, (_, index) => message(`item_batch_${index}`, `batch ${index}`));
    assert.equal((await sdk.conversations.items.create(conversation.id, { items: maximum })).data.length, 20);
    await status(sdk.conversations.items.create(conversation.id, {
      items: Array.from({ length: 21 }, (_, index) => message(`item_over_${index}`, 'over limit')),
    }), 400);
    await status(sdk.conversations.items.create(conversation.id, { items: [message('item_2', 'Cannot rewrite prior data')] }), 409);
    assert.equal((await sdk.conversations.items.retrieve('item_2', { conversation_id: conversation.id })).content[0].text, 'two');
  });

  await t.test('include is opt-in and does not rewrite canonical encrypted data', async () => {
    await sdk.conversations.items.create(conversation.id, { items: [{
      id: 'reasoning_canonical', type: 'reasoning', summary: [], encrypted_content: 'fixture-opaque-reasoning',
    }] });
    const hidden = await sdk.conversations.items.retrieve('reasoning_canonical', { conversation_id: conversation.id });
    assert.equal(hidden.encrypted_content, undefined);
    const included = await sdk.conversations.items.retrieve('reasoning_canonical', {
      conversation_id: conversation.id, include: ['reasoning.encrypted_content'],
    });
    assert.equal(included.encrypted_content, 'fixture-opaque-reasoning');
    const listed = await sdk.conversations.items.list(conversation.id, { include: ['reasoning.encrypted_content'] });
    assert.equal(listed.data.find(item => item.id === 'reasoning_canonical').encrypted_content, 'fixture-opaque-reasoning');
    assert.equal((await sdk.conversations.items.retrieve('reasoning_canonical', { conversation_id: conversation.id })).encrypted_content, undefined);
  });

  await t.test('owner isolation, metadata constraints and invalid roles fail without mutation', async () => {
    await status(other.conversations.retrieve(conversation.id), 404);
    await status(other.conversations.items.list(conversation.id), 404);
    await status(other.conversations.items.retrieve('item_initial', { conversation_id: conversation.id }), 404);
    await status(other.conversations.items.create(conversation.id, { items: [message('stranger_item', 'Cannot append')] }), 404);
    await status(other.conversations.delete(conversation.id), 404);
    await status(sdk.conversations.update(conversation.id, { metadata: { tooLong: 'x'.repeat(513) } }), 400);
    await status(sdk.conversations.items.create(conversation.id, { items: [{ role: 'administrator', content: 'bad' }] }), 400);
    await status(sdk.conversations.items.create(conversation.id, { items: [{ type: 'not_a_real_item', id: 'item_bad' }] }), 400);
    const updated = await sdk.conversations.update(conversation.id, { metadata: { label: 'After update' } });
    assert.deepEqual(updated.metadata, { label: 'After update' });
    assert.equal(updated.id, conversation.id);
  });

  await t.test('item tombstones survive cold reconstruction and conversation deletion preserves remaining items', async () => {
    const deletion = await sdk.conversations.items.delete('item_3', { conversation_id: conversation.id });
    assert.equal(deletion.object, 'conversation', 'SDK item deletion returns the parent Conversation');
    await status(sdk.conversations.items.retrieve('item_3', { conversation_id: conversation.id }), 404);
    const next = await sdk.conversations.items.list(conversation.id, { order: 'asc', after: 'item_3', limit: 1 });
    assert.deepEqual(next.data.map(item => item.id), ['item_4'], 'A tombstoned item remains a valid stable cursor');
    await mf.unsafeEvictDurableObject('', 'ConversationState', { name: `${principal.owner}:${conversation.id}` });
    await status(sdk.conversations.items.retrieve('item_3', { conversation_id: conversation.id }), 404);
    assert.equal((await sdk.conversations.items.retrieve('item_initial', { conversation_id: conversation.id })).id, 'item_initial');
    assert.deepEqual(await sdk.conversations.delete(conversation.id), { id: conversation.id, object: 'conversation.deleted', deleted: true });
    await status(sdk.conversations.retrieve(conversation.id), 404);
    assert.equal((await sdk.conversations.items.retrieve('item_initial', { conversation_id: conversation.id })).id, 'item_initial',
      'Deleting a Conversation must not delete its items');
    await status(sdk.conversations.items.create(conversation.id, { items: [message('after_delete', 'No resurrection')] }), 404);
    const remaining = await sdk.conversations.items.list(conversation.id, { order: 'asc' });
    assert.ok(remaining.data.some(item => item.id === 'item_initial'));
    assert.ok(!remaining.data.some(item => item.id === 'item_3'));
  });
  assert.equal(externalCalls, 0, 'Conversation management must not invoke models or external services');
});
