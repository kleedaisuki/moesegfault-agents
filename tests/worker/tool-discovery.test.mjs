import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { createHash, randomBytes } from 'node:crypto';
import { mkdir, mkdtemp, readFile, readdir, writeFile } from 'node:fs/promises';
import path from 'node:path';
import test from 'node:test';
import { fileURLToPath } from 'node:url';
import { Miniflare, convertV4MiniflareOptions } from 'miniflare';
import { createIdentityFixture } from './auth-journey.mjs';
import { createDiscoveryProvider, runServerDiscovery, runInvalidDiscoveryArguments, runNativeDiscovery, runInvalidClientSearch, runCallerAuthority } from './tool-discovery-journey.mjs';

const root = fileURLToPath(new URL('../../', import.meta.url));
const build = path.resolve(root, process.env.WORKER_BUILD_DIR ?? 'crates/agents-worker/build');
const cases = [
  { id: 'server', name: 'server tool discovery preserves static prefix and namespace handoff through replay', run: runServerDiscovery },
  { id: 'strict_invalid', name: 'disobedient discovered strict arguments fail with known usage and zero completed handoff', run: runInvalidDiscoveryArguments },
  { id: 'native', name: 'native-capable transport preserves tool search outputs and resumes qualified calls', run: runNativeDiscovery },
  { id: 'client_invalid', name: 'client search arguments cannot publish completed handoff before validation', run: runInvalidClientSearch },
  { id: 'callers', name: 'ordinary function caller permissions reject direct and forged program handoffs', run: runCallerAuthority },
];

test('compiled Rust Worker: deferred tool discovery and qualified calls', { timeout: 60000 }, async t => {
  const selected = cases.filter(item => !process.env.MAGENTS_DISCOVERY_CASE || process.env.MAGENTS_DISCOVERY_CASE === item.id);
  assert.ok(selected.length > 0, 'Select an actual discovery case, never a zero-test pass');
  const encryptionKey = randomBytes(32);
  const identity = createIdentityFixture();
  const providers = createDiscoveryProvider();
  const deployment = JSON.parse(await readFile(path.join(root, 'wrangler.jsonc'), 'utf8'));
  await mkdir(path.join(root, '.temp'), { recursive: true });
  const directory = await mkdtemp(path.join(root, '.temp/tool-discovery-'));
  const mf = new Miniflare(convertV4MiniflareOptions({
    modulesRoot: build, modules: [
      { type: 'ESModule', path: path.join(build, 'worker/shim.mjs') },
      { type: 'ESModule', path: path.join(build, 'index.js') },
      { type: 'CompiledWasm', path: path.join(build, 'index_bg.wasm') },
    ],
    compatibilityDate: '2026-10-23', compatibilityFlags: ['enable_request_signal'],
    resourcePersistencePath: path.join(directory, 'storage'), d1Databases: ['DB'],
    r2Buckets: ['WORKSPACES', 'CONVERSATIONS', 'DIAGNOSTICS'],
    durableObjects: {
      SESSIONS: { className: 'Session', useSQLite: true }, SESSION_4G: { className: 'Session4G', useSQLite: true },
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
      IDENTITY_ISSUER: 'https://identity.test', IDENTITY_WEB_CLIENT_ID: 'test-dashboard', IDENTITY_NATIVE_CLIENT_ID: 'test-native',
      IDENTITY_REDIRECT_URI: 'https://agents.test/auth/callback', IDENTITY_NATIVE_REDIRECT_URI: 'http://127.0.0.1/callback',
      IDENTITY_CLIENT_PRIVATE_JWK: JSON.stringify(identity.privateJwk), BILLING_PRODUCT_ID: 'agents', BILLING_MODE: 'test',
      MODEL_REGISTRY_JSON: '[]', MODEL_PRICING_JSON: '{}',
      RESOURCE_PRICING_JSON: deployment.env.staging.vars.RESOURCE_PRICING_JSON,
      RUN_RESOURCE_RESERVATION_MICROUSD: '20000', FREE_GRANT_BUDGET_MICROUSD_PER_DAY: '100000000',
    },
    async outboundService(request) {
      const response = await providers.handle(request);
      assert.notEqual(response, undefined, `No unplanned external access: ${new URL(request.url).origin}`);
      return response;
    },
  }));
  t.after(() => mf.dispose());
  const db = await mf.getD1Database('DB');
  for (const name of (await readdir(path.join(root, 'migrations'))).filter(name => name.endsWith('.sql')).sort()) {
    const parsed = spawnSync(process.env.PYTHON ?? 'python', ['-c',
      'import sys,sqlite3,json\ns: str=""; out: list[str]=[]\nfor line in sys.stdin:\n s+=line\n if sqlite3.complete_statement(s): out.append(s); s=""\nassert not s.strip(), s\nprint(json.dumps(out))'],
    { input: await readFile(path.join(root, 'migrations', name), 'utf8'), encoding: 'utf8' });
    assert.equal(parsed.status, 0, `${name}: ${parsed.stderr}`);
    await db.batch(JSON.parse(parsed.stdout).map(statement => db.prepare(statement)));
  }
  async function request(method, endpoint, body, expected = 200, bearer) {
    const response = await mf.dispatchFetch('https://agents.test' + endpoint, {
      method, headers: { Authorization: `Bearer ${bearer}`, 'Content-Type': 'application/json', 'CF-Connecting-IP': '192.0.2.71' },
      ...(body === undefined ? {} : { body: JSON.stringify(body) }),
    });
    const text = await response.text();
    assert.equal(response.status, expected, `${method} ${endpoint}: ${text}`);
    return JSON.parse(text);
  }
  const results = {};
  const source = process.env.MAGENTS_TEST_EXPECTED_BUILD ?? process.env.GITHUB_SHA ?? null;
  const wasmSha = createHash('sha256').update(await readFile(path.join(build, 'index_bg.wasm'))).digest('hex');
  await mkdir(path.join(root, '.cache/v0.3-ci'), { recursive: true });
  for (const item of cases) {
    await t.test(item.name, { skip: !selected.includes(item) }, async () => {
      results[item.id] = await item.run({ mf, db, encryptionKey, providers, request });
      await writeFile(path.join(root, `.cache/v0.3-ci/tool-discovery-${item.id}.json`), JSON.stringify({
        source, wasm_sha256: wasmSha, fixture: 'actual-compiled-rust-worker', case_id: item.id,
        status: 'passed_current', external_network_calls: 0, ...results[item.id],
      }, null, 2));
    });
  }
  assert.deepEqual(Object.keys(results).sort(), selected.map(item => item.id).sort(), 'Failed cases never produce a successful receipt');
  await mkdir(path.join(root, '.cache/v0.3-ci'), { recursive: true });
  await writeFile(path.join(root, '.cache/v0.3-ci/tool-discovery.json'), JSON.stringify({
    source, wasm_sha256: wasmSha,
    fixture: 'actual-compiled-rust-worker', external_network_calls: 0,
    executed_case_ids: selected.map(item => item.id), results,
  }, null, 2));
});
