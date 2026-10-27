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
    const parsed = spawnSync(process.env.MAGENTS_SCHEMA_PYTHON ?? process.env.PYTHON ?? 'python', ['-c',
      'import sys,sqlite3,json\ns: str=""; out: list[str]=[]\nfor line in sys.stdin:\n s+=line\n if sqlite3.complete_statement(s): out.append(s); s=""\nassert not s.strip(), s\nprint(json.dumps(out))'],
    { input: await readFile(path.join(root, 'migrations', name), 'utf8'), encoding: 'utf8' });
    assert.equal(parsed.status, 0, `${name}: ${parsed.stderr}`);
    await db.batch(JSON.parse(parsed.stdout).map(statement => db.prepare(statement)));
  }
}


test('compiled Rust Worker: Conversation serialization survives slow D1 without a platform concurrency gate', { timeout: 85000 }, async t => {
  const encryptionKey = randomBytes(32);
  const identity = createIdentityFixture();
  const deployment = JSON.parse(await readFile(path.join(root, 'wrangler.jsonc'), 'utf8'));
  await mkdir(path.join(root, '.temp'), { recursive: true });
  const directory = await mkdtemp(path.join(root, '.temp/conversation-io-'));
  let externalCalls = 0;
  const mf = new Miniflare(convertV4MiniflareOptions({
    modulesRoot: build,
    modules: [
      { type: 'ESModule', path: path.join(build, 'worker/shim.mjs'),
        contents: await readFile(new URL('./conversation-io-shim.mjs', import.meta.url), 'utf8') },
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
      throw new Error(`Conversation I/O journey must not call external service ${new URL(request.url).origin}`);
    },
  }));
  t.after(() => mf.dispose());
  const db = await mf.getD1Database('DB');
  await migrate(db);

  const owner = await createBrowserPrincipal({ mf, db, encryptionKey, owner: 'conversation-slow-io-owner' });
  const sdk = new OpenAI({ apiKey: owner.apiKey, baseURL: 'https://agents.test/v1', maxRetries: 0, timeout: 70000,
    fetch: (input, init) => mf.dispatchFetch(input, init) });
  const conversation = await sdk.conversations.create();
  const namespace = await mf.getDurableObjectNamespace('CONVERSATION_STATE');
  const actor = namespace.get(namespace.idFromName(owner.owner + ':' + conversation.id));
  await actor.fetch('https://fixture/__io/arm', { method: 'POST' });
  const started = Date.now();
  const first = sdk.conversations.update(conversation.id, { metadata: { order: 'first' } });
  const deadline = Date.now() + 10000;
  while (!(await (await actor.fetch('https://fixture/__io/status')).json()).entered) {
    assert.ok(Date.now() < deadline, 'The controlled actor must enter D1 before the second writer');
    await new Promise(resolve => setTimeout(resolve, 10));
  }
  const second = sdk.conversations.update(conversation.id, { metadata: { order: 'second' } });
  const [one, two] = await Promise.all([first, second]);
  assert.deepEqual(one.metadata, { order: 'first' });
  assert.deepEqual(two.metadata, { order: 'second' });
  assert.ok(Date.now() - started >= 33000, 'Exercise real asynchronous time beyond the documented 30s platform gate');
  assert.deepEqual((await sdk.conversations.retrieve(conversation.id)).metadata, { order: 'second' },
    'The later request waits for the first publication instead of racing a stale header');
  const status = await (await actor.fetch('https://fixture/__io/status')).json();
  assert.equal(status.delays, 1);
  assert.equal(status.platformBlocks, 0, 'Regular funded requests must not enter blockConcurrencyWhile');
  const wall = (await db.prepare("SELECT CAST(quantity AS TEXT) AS quantity FROM resource_observations WHERE account_id=? AND resource='durable_object' AND status='observed'").bind(owner.owner).all()).results;
  assert.ok(wall.some(row => BigInt(row.quantity) >= 33000n), 'Record the actual long actor interval, not a fabricated short duration');
  assert.equal((await db.prepare("SELECT COUNT(*) AS n FROM resource_observations WHERE account_id=? AND status='pending'").bind(owner.owner).first()).n, 0);
  assert.equal((await db.prepare("SELECT COUNT(*) AS n FROM usage_ledger l JOIN usage_reservations r ON r.id=l.reservation_id WHERE r.account_id=? AND l.resource='model'").bind(owner.owner).first()).n, 0);
  assert.equal(externalCalls, 0);
});
