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


test('compiled Rust Worker: Conversation reclaims a never-activated admission and settles only its original observed ledger', { timeout: 85000 }, async t => {
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


  const owner = await createBrowserPrincipal({ mf, db, encryptionKey, owner: 'conversation-reclaimed-funding-owner' });
  const sdk = new OpenAI({ apiKey: owner.apiKey, baseURL: 'https://agents.test/v1', maxRetries: 0, timeout: 20000,
    fetch: (input, init) => mf.dispatchFetch(input, init) });
  const credential = await owner.browser('POST', '/v1/provider-keys', { protocol: 'openai',
    base_url: 'https://never-called.example.com/v1', api_key: 'synthetic-never-called-key' }, 201);
  const agent = await sdk.post('/agents', { body: { name: 'Reclaimed funding', instructions: 'Preserve the original intent.',
    model: { provider: 'byok', credential_id: credential.id, protocol: 'responses', model: 'never-called',
      context_window: 8192, max_output_tokens: 64 }, workspace: false, tools: [] } });
  const conversation = await sdk.conversations.create();
  const namespace = await mf.getDurableObjectNamespace('CONVERSATION_STATE');
  const actor = namespace.get(namespace.idFromName(owner.owner + ':' + conversation.id));
  await actor.fetch('https://fixture/__io/lose-reserve', { method: 'POST' });
  const failed = await mf.dispatchFetch('https://agents.test/v1/responses', { method: 'POST',
    headers: { Authorization: 'Bearer ' + owner.apiKey, 'Content-Type': 'application/json', 'Idempotency-Key': 'one-never-admitted-intent' },
    body: JSON.stringify({ model: agent.id, conversation: conversation.id, input: 'This must never reach a provider.' }) });
  assert.equal(failed.status, 503);
  assert.ok(['conversation_actor_transport_failed', 'conversation_actor_response_failed'].includes((await failed.json()).error.code));
  const response = failed.headers.get('x-agents-response');
  const session = failed.headers.get('x-agents-session');
  assert.ok(response?.startsWith('resp_') && session);
  assert.equal((await (await actor.fetch('https://fixture/__io/status')).json()).ackLost, true);
  const scope = 'conv-response:' + response;
  const before = await db.prepare("SELECT state,CAST(reserved_microusd AS TEXT) AS reserved FROM usage_reservations WHERE account_id=? AND id=?")
    .bind(owner.owner, scope).first();
  assert.equal(before.state, 'reserved');
  assert.ok(BigInt(before.reserved) > 0n);
  assert.equal((await db.prepare("SELECT COUNT(*) AS n FROM resource_observations WHERE run_id=? AND status='pending'").bind(scope).first()).n, 0);
  const prior = await db.prepare("SELECT COUNT(*) AS n,CAST(COALESCE(SUM(amount_microusd),0) AS TEXT) AS charged FROM usage_ledger WHERE reservation_id=?")
    .bind(scope).first();
  const held = await db.prepare("SELECT state,remaining_bytes FROM conversation_response_capacity WHERE account_id=? AND response_id=?")
    .bind(owner.owner, response).first();
  assert.equal(held.state, 'reserved'); assert.ok(held.remaining_bytes > 0);
  await actor.fetch('https://fixture/__io/expire-pending', { method: 'POST' });
  await sdk.conversations.delete(conversation.id);
  const purged = await mf.dispatchFetch('https://agents.test/v1/conversations/' + conversation.id + '/purge', {
    method: 'POST', headers: { Authorization: 'Bearer ' + owner.apiKey } });
  assert.equal(purged.status, 200, await purged.clone().text());
  const after = await db.prepare("SELECT state,CAST(settled_microusd AS TEXT) AS settled FROM usage_reservations WHERE account_id=? AND id=?")
    .bind(owner.owner, scope).first();
  assert.deepEqual(after, { state: 'settled', settled: prior.charged });
  assert.deepEqual(await db.prepare("SELECT COUNT(*) AS n,CAST(COALESCE(SUM(amount_microusd),0) AS TEXT) AS charged FROM usage_ledger WHERE reservation_id=?")
    .bind(scope).first(), prior, 'Recovery must not re-record time, reprice old usage or append a synthetic zero charge');
  assert.deepEqual(await db.prepare("SELECT state,remaining_bytes FROM conversation_response_capacity WHERE account_id=? AND response_id=?")
    .bind(owner.owner, response).first(), { state: 'released', remaining_bytes: 0 });
  assert.equal((await db.prepare("SELECT COUNT(*) AS n FROM usage_ledger l JOIN usage_reservations r ON r.id=l.reservation_id WHERE r.account_id=? AND l.resource='model'")
    .bind(owner.owner).first()).n, 0);
  assert.equal(externalCalls, 0);
});
