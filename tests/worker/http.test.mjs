import assert from 'node:assert/strict';
import { createHash, randomBytes } from 'node:crypto';
import { mkdir, mkdtemp, readdir, readFile } from 'node:fs/promises';
import { spawnSync } from 'node:child_process';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import { Miniflare, convertV4MiniflareOptions } from 'miniflare';
import { runAuthJourney } from './auth-journey.mjs';

const root = fileURLToPath(new URL('../../', import.meta.url));
const build = path.resolve(root, process.env.WORKER_BUILD_DIR ?? 'crates/agents-worker/build');
const key = 'mag_integration_synthetic_key_not_a_real_credential';
const hash = createHash('sha256').update(key).digest('base64url');

test('compiled Rust Worker: authenticated D1 and session DO user journey', { timeout: 90000 }, async (t) => {
  const encryptionKey = randomBytes(32);
  await mkdir(path.join(root, '.temp'), { recursive: true });
  const temp = await mkdtemp(path.join(root, '.temp/worker-integration-'));
  process.env.TMP = process.env.TEMP = process.env.TMPDIR = temp;
  const mf = new Miniflare(convertV4MiniflareOptions({
    modulesRoot: build,
    modules: [
      { type: 'ESModule', path: path.join(build, 'worker/shim.mjs') },
      { type: 'ESModule', path: path.join(build, 'index.js') },
      { type: 'CompiledWasm', path: path.join(build, 'index_bg.wasm') },
    ],
    compatibilityDate: '2026-10-23',
    resourcePersistencePath: path.join(temp, 'storage'),
    d1Databases: ['DB'],
    r2Buckets: ['WORKSPACES', 'CONVERSATIONS', 'DIAGNOSTICS'],
    durableObjects: {
      SESSIONS: { className: 'Session', useSQLite: true },
      AUTH_SESSIONS: { className: 'AuthSession', useSQLite: true },
    },
    ratelimits: Object.fromEntries(['INGRESS', 'ACCOUNT', 'MUTATION'].map((name, i) => [
      `${name}_RATE_LIMITER`, { namespace_id: String(i + 1), simple: { limit: 1000, period: 60 } },
    ])),
    bindings: {
      ENVIRONMENT: 'test', APP_ORIGIN: 'https://agents.test', ALLOWED_ORIGIN: 'https://agents.test',
      DATA_ENCRYPTION_KEY: encryptionKey.toString('base64url'),
      IDENTITY_ISSUER: 'https://identity.test', IDENTITY_WEB_CLIENT_ID: 'test-dashboard',
      IDENTITY_NATIVE_CLIENT_ID: 'test-native', IDENTITY_REDIRECT_URI: 'https://agents.test/auth/callback',
      IDENTITY_NATIVE_REDIRECT_URI: 'http://127.0.0.1/callback',
      BILLING_PRODUCT_ID: 'agents', BILLING_MODE: 'test',
      MODEL_REGISTRY_JSON: JSON.stringify([{ provider: 'openrouter', model: 'test-model', context_window: 10000, max_output_tokens: 1000 }]),
      RESOURCE_PRICING_JSON: JSON.stringify({ version: 'test', tariffs: [] }),
    },
    outboundService: async request => {
      if (request.url === 'https://identity.test/jwks') return Response.json({ keys: [] });
      assert.equal(request.url, 'https://identity.test/.well-known/openid-configuration');
      return Response.json({ issuer: 'https://identity.test', authorization_endpoint: 'https://identity.test/authorize',
        token_endpoint: 'https://identity.test/token', jwks_uri: 'https://identity.test/jwks',
        end_session_endpoint: 'https://identity.test/logout', revocation_endpoint: 'https://identity.test/revoke',
        code_challenge_methods_supported: ['S256'], token_endpoint_auth_methods_supported: ['private_key_jwt', 'none'] });
    },
  }));
  t.after(() => mf.dispose());
  const db = await mf.getD1Database('DB');
  for (const name of (await readdir(path.join(root, 'migrations'))).filter(n => n.endsWith('.sql')).sort()) {
    const sql = await readFile(path.join(root, 'migrations', name), 'utf8');
    // Let SQLite recognize trigger bodies and SQL quoting, rather than splitting on semicolons.
    const parsed = spawnSync(process.env.PYTHON ?? 'python', ['-c', 'import sys,sqlite3,json\ns=""; out=[]\nfor line in sys.stdin:\n s+=line\n if sqlite3.complete_statement(s): out.append(s); s=""\nassert not s.strip(), s\nprint(json.dumps(out))'], { input: sql, encoding: 'utf8' });
    assert.equal(parsed.status, 0, `${name}: ${parsed.stderr}`);
    const statements = JSON.parse(parsed.stdout);
    await db.batch(statements.map(s => db.prepare(s)));
  }
  await db.batch([
    db.prepare("INSERT INTO accounts(id,issuer,subject,created_at,updated_at) VALUES('owner','https://identity.test','synthetic',0,0)"),
    db.prepare("INSERT INTO usage_accounts(account_id,updated_at) VALUES('owner',0)"),
    db.prepare("INSERT INTO api_keys(id,account_id,name,key_hash,key_prefix,created_at) VALUES('key','owner','integration',?1,'mag_test',0)").bind(hash),
  ]);
  async function request(method, endpoint, body, expected = 200, bearer = key) {
    const response = await mf.dispatchFetch(`https://agents.test${endpoint}`, {
      method, headers: { Authorization: `Bearer ${bearer}`, 'Content-Type': 'application/json', 'CF-Connecting-IP': '192.0.2.1' },
      ...(body === undefined ? {} : { body: JSON.stringify(body) }),
    });
    const text = await response.text();
    assert.equal(response.status, expected, `${method} ${endpoint}: ${text}`);
    return JSON.parse(text);
  }
  await t.test('unknown credentials fail without touching the key', async () => {
    await request('GET', '/v1/models', undefined, 401, 'mag_unknown');
    assert.equal((await db.prepare("SELECT last_used_at FROM api_keys WHERE id='key'").first()).last_used_at, null);
  });
  await t.test('first API key use binds an integer across Wasm -> JavaScript -> D1', async () => {
    const models = await request('GET', '/v1/models');
    assert.deepEqual(models.data, []);
    const row = await db.prepare("SELECT last_used_at,typeof(last_used_at) AS kind FROM api_keys WHERE id='key'").first();
    assert.equal(row.kind, 'integer');
    assert.ok(row.last_used_at > 1_700_000_000);
    await request('GET', '/v1/models');
    assert.equal((await db.prepare("SELECT last_used_at FROM api_keys WHERE id='key'").first()).last_used_at, row.last_used_at);
  });
  await t.test('browser auth lifecycle uses real AuthSession DO and integer D1 values', () =>
    runAuthJourney({ mf, db, encryptionKey, origin: 'https://agents.test' }));
  await t.test('agent CRUD, immutable session prefix, DO close and revocation', async (t) => {
    const agent = await request('POST', '/v1/agents', { name: 'Integration', instructions: 'Pinned original instructions', model: { provider: 'openrouter', model: 'test-model' }, workspace: false }, 201);
    assert.equal((await request('GET', '/v1/models')).data[0].id, agent.id);
    const session = await request('POST', `/v1/agents/${agent.id}/sessions`, {}, 201);
    assert.equal(session.status, 'idle');
    await request('PATCH', `/v1/agents/${agent.id}`, { instructions: 'Changed instructions' });
    const stored = await db.prepare('SELECT config_snapshot_json FROM sessions WHERE id=?1').bind(session.id).first();
    assert.ok(stored.config_snapshot_json.includes('Pinned original instructions'));
    assert.ok(!stored.config_snapshot_json.includes('Changed instructions'));
    await t.test('read a live DO session through HTTP', () => request('GET', `/v1/sessions/${session.id}`));
    await t.test('close a DO session through HTTP', () => request('DELETE', `/v1/sessions/${session.id}`));
    assert.equal((await db.prepare('SELECT status FROM sessions WHERE id=?1').bind(session.id).first()).status, 'closed');
    await request('DELETE', `/v1/agents/${agent.id}`);
    await request('GET', `/v1/agents/${agent.id}`, undefined, 404);
    await db.prepare("UPDATE api_keys SET revoked_at=1 WHERE id='key'").run();
    await request('GET', '/v1/models', undefined, 401);
  });
});
