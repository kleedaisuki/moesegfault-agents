import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { createHash, randomBytes } from 'node:crypto';
import { mkdir, mkdtemp, readFile, readdir, writeFile } from 'node:fs/promises';
import path from 'node:path';
import test from 'node:test';
import { fileURLToPath } from 'node:url';
import { Miniflare, convertV4MiniflareOptions } from 'miniflare';
import { createIdentityFixture } from './auth-journey.mjs';
import { createNativeSemanticsProvider } from './native-semantics-journey.mjs';
import { runNativeWindowJourney, runNativeWindowToolsJourney, runNativeWindowRejectionJourney, runContextCapacityJourney, runConversationWindowCountJourney, runConversationCapacityRejectionJourney, runNativeWindowCountRejectionJourney } from './native-window-journey.mjs';

const root = fileURLToPath(new URL('../../', import.meta.url));
const build = path.resolve(root, process.env.WORKER_BUILD_DIR ?? 'crates/agents-worker/build');

async function migrate(db) {
  for (const name of (await readdir(path.join(root, 'migrations'))).filter(name => name.endsWith('.sql')).sort()) {
    const sql = await readFile(path.join(root, 'migrations', name), 'utf8');
    const parsed = spawnSync(process.env.PYTHON ?? 'python', ['-c',
      'import sys,sqlite3,json\ns: str=""; out: list[str]=[]\nfor line in sys.stdin:\n s+=line\n if sqlite3.complete_statement(s): out.append(s); s=""\nassert not s.strip(), s\nprint(json.dumps(out))'],
    { input: sql, encoding: 'utf8' });
    assert.equal(parsed.status, 0, `${name}: ${parsed.stderr}`);
    await db.batch(JSON.parse(parsed.stdout).map(statement => db.prepare(statement)));
  }
}

test('compiled Rust Worker: native auto window preserves source and historical tool groups', { timeout: 75000 }, async t => {
  const encryptionKey = randomBytes(32);
  const identity = createIdentityFixture();
  const providers = createNativeSemanticsProvider();
  const deployment = JSON.parse(await readFile(path.join(root, 'wrangler.jsonc'), 'utf8'));
  await mkdir(path.join(root, '.temp'), { recursive: true });
  const directory = await mkdtemp(path.join(root, '.temp/native-semantics-'));
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
      MODEL_OPERATIONS: { className: 'ModelOperation', useSQLite: true },
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
      const response = await providers.handle(request);
      assert.notEqual(response, undefined, `Native fixtures must not access external services: ${new URL(request.url).origin}`);
      return response;
    },
  }));
  t.after(() => mf.dispose());
  const db = await mf.getD1Database('DB');
  await migrate(db);
  async function request(method, endpoint, body, expected = 200, bearer) {
    const response = await mf.dispatchFetch('https://agents.test' + endpoint, {
      method, headers: { Authorization: `Bearer ${bearer}`, 'Content-Type': 'application/json', 'CF-Connecting-IP': '192.0.2.73' },
      ...(body === undefined ? {} : { body: JSON.stringify(body) }),
    });
    const text = await response.text();
    assert.equal(response.status, expected, `${method} ${endpoint}: ${text}`);
    return JSON.parse(text);
  }
  const context = { mf, db, encryptionKey, request, providers };
  const receipts = {};
  if (process.env.MAGENTS_NATIVE_WINDOW_CASE === 'count-rejection') {
    await t.test('read-only auto count rejects an unfit latest group with HTTP400 and no generation', async () => {
      receipts.count_rejection = await runNativeWindowCountRejectionJourney(context);
    });
  }
  if (process.env.MAGENTS_NATIVE_WINDOW_CASE === 'capacity-rejection') {
    await t.test('known capacity rejection immediately reclaims its writer and permits explicit purge without model work', async () => {
      receipts.capacity_rejection = await runConversationCapacityRejectionJourney(context);
    });
  }
  if (process.env.MAGENTS_NATIVE_WINDOW_CASE === 'conversation-count') {
    await t.test('Conversation readonly count selects its immutable predecessor instead of a newer Session head', async () => {
      receipts.conversation_count = await runConversationWindowCountJourney(context);
    });
  }
  if (process.env.MAGENTS_NATIVE_WINDOW_CASE === 'context-capacity') {
    await t.test('request compaction prepays Conversation output and explicit purge preserves Response history', async () => {
      receipts.context_capacity = await runContextCapacityJourney(context);
    });
  }
  if (process.env.MAGENTS_NATIVE_WINDOW_CASE === 'rejection') {
    await t.test('oversized groups reject without model work', async () => { receipts.rejection = await runNativeWindowRejectionJourney(context); });
  }
  if (!process.env.MAGENTS_NATIVE_WINDOW_CASE || process.env.MAGENTS_NATIVE_WINDOW_CASE === 'history') {
    await t.test('history windows and default historical forks', async () => { receipts.history = await runNativeWindowJourney(context); });
  }
  if (!process.env.MAGENTS_NATIVE_WINDOW_CASE || process.env.MAGENTS_NATIVE_WINDOW_CASE === 'tools') {
    await t.test('closed tool pairs and pending historical forks', async () => { receipts.tools = await runNativeWindowToolsJourney(context); });
  }
  assert.equal(Object.keys(receipts).length, process.env.MAGENTS_NATIVE_WINDOW_CASE ? 1 : 2,
    'Do not write a passing receipt if an independent new window case failed');
  await mkdir(path.join(root, '.cache/v0.3-ci'), { recursive: true });
  await writeFile(path.join(root, '.cache/v0.3-ci/native-window.json'), JSON.stringify({
    fixture: 'actual-compiled-rust-worker',
    source: process.env.MAGENTS_TEST_EXPECTED_BUILD ?? process.env.GITHUB_SHA ?? null,
    wasm_sha256: createHash('sha256').update(await readFile(path.join(build, 'index_bg.wasm'))).digest('hex'),
    external_network_calls: 0, ...receipts,
  }, null, 2));
});
