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
  const deployment = JSON.parse(await readFile(path.join(root, 'wrangler.jsonc'), 'utf8'));
  const resourcePricing = deployment.env.staging.vars.RESOURCE_PRICING_JSON;
  let providerCalls = 0;
  let redirectProvider = false;
  let redirectTargetCalls = 0;
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
      RESOURCE_PRICING_JSON: resourcePricing,
      OPENROUTER_API_KEY: 'synthetic-provider-key-never-sent-outside-fixture',
      RUN_RESOURCE_RESERVATION_MICROUSD: '20000',
      FREE_GRANT_BUDGET_MICROUSD_PER_DAY: '10000000',
      MODEL_PRICING_JSON: JSON.stringify({ 'openrouter/test-model': { input_microusd_per_million: 300000, cached_input_microusd_per_million: 6000, output_microusd_per_million: 1200000, revision: 'integration' } }),
    },
    outboundService: async request => {
      if (request.url.startsWith('https://redirect-target.test/')) {
        redirectTargetCalls++;
        throw new Error('Provider redirect must never be followed');
      }
      if (request.url === 'https://openrouter.ai/api/v1/chat/completions') {
        providerCalls++;
        if (redirectProvider) return new Response(null, { status: 302, headers: { Location: 'https://redirect-target.test/steal' } });
        const input = await request.json();
        assert.equal(input.stream, true);
        assert.equal(input.model, 'test-model');
        assert.ok(input.tools.some(tool => tool.function.name === 'time'));
        assert.ok(providerCalls <= 2, 'A completed answer must not cause another inference');
        const first = providerCalls === 1;
        if (!first) {
          const result = input.messages.filter(message => message.role === 'tool' && message.tool_call_id === 'call_time').at(-1);
          assert.ok(result, 'Second inference must receive the actual Rust harness tool result');
          const clock = JSON.parse(result.content);
          assert.ok(Number.isSafeInteger(clock.unix_ms));
          assert.ok(Math.abs(clock.unix_ms - Date.now()) < 10000);
          assert.equal(typeof clock.utc, 'string');
        }
        const delta = first ? { role: 'assistant', tool_calls: [{ index: 0, id: 'call_time', type: 'function', function: { name: 'time', arguments: '{}' } }] }
          : { role: 'assistant', content: 'The clock tool completed.' };
        return new Response(`data: ${JSON.stringify({ id: `mock-${providerCalls}`, model: 'test-model', choices: [{ index: 0, delta, finish_reason: first ? 'tool_calls' : 'stop' }], usage: { prompt_tokens: 100, completion_tokens: 10, total_tokens: 110, cost: 0.00001 } })}\n\ndata: [DONE]\n\n`, { headers: { 'Content-Type': 'text/event-stream' } });
      }
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
  await t.test('non-streaming SDK call advances alarm, invokes time, and settles two model rounds', async () => {
    await db.prepare("UPDATE api_keys SET revoked_at=NULL WHERE id='key'").run();
    const agent = await request('POST', '/v1/agents', { name: 'Tool continuation', model: { provider: 'openrouter', model: 'test-model' }, tools: ['time'], workspace: false }, 201);
    const session = await request('POST', `/v1/agents/${agent.id}/sessions`, {}, 201);
    let timer;
    async function diagnostics() {
      const statusResponse = await mf.dispatchFetch(`https://agents.test/v1/sessions/${session.id}`, {
        headers: { Authorization: `Bearer ${key}` },
      });
      const status = statusResponse.status === 200 ? await statusResponse.json() : {};
      const reservations = await db.prepare('SELECT id,state,reserved_microusd,settled_microusd FROM usage_reservations WHERE session_id=?1').bind(session.id).all();
      const observations = await db.prepare('SELECT event_key,resource,status,quantity FROM resource_observations WHERE session_id=?1').bind(session.id).all();
      const ledger = await db.prepare('SELECT resource,unit,amount_microusd FROM usage_ledger WHERE reservation_id IN (SELECT id FROM usage_reservations WHERE session_id=?1)').bind(session.id).all();
      return { providerCalls, session: { status: status.status, run_id: status.run_id, last_error: status.last_error,
        checkpoint_pending: status.checkpoint_pending }, reservations: reservations.results, observations: observations.results, ledger: ledger.results };
    }
    const completion = mf.dispatchFetch('https://agents.test/v1/chat/completions', {
      method: 'POST', headers: { Authorization: `Bearer ${key}`, 'Content-Type': 'application/json', 'X-Agents-Session': session.id },
      body: JSON.stringify({ model: agent.id, messages: [{ role: 'user', content: 'Use time, then answer.' }], stream: false }),
    });
    try {
      const response = await Promise.race([completion, new Promise((_, reject) => {
        timer = setTimeout(async () => {
          const reservations = await db.prepare('SELECT COUNT(*) AS count FROM usage_reservations WHERE session_id=?1').bind(session.id).first('count');
          const ledger = await db.prepare('SELECT COUNT(*) AS count FROM usage_ledger').first('count');
          reject(new Error(`Run stalled after 8 seconds: providerCalls=${providerCalls}, reservations=${reservations}, ledger=${ledger}, session=${session.id}`));
        }, 8000);
      })]);
      if (response.status !== 200) t.diagnostic(JSON.stringify(await diagnostics()));
      assert.equal(response.status, 200, await response.clone().text());
      const answer = await response.json();
      assert.equal(answer.choices[0].message.content, 'The clock tool completed.');
      assert.equal(providerCalls, 2);
      assert.ok(await db.prepare("SELECT COUNT(*) AS count FROM usage_ledger WHERE resource='model'").first('count') >= 2);
      providerCalls = 0;
      const streamed = await mf.dispatchFetch('https://agents.test/v1/chat/completions', {
        method: 'POST', headers: { Authorization: `Bearer ${key}`, 'Content-Type': 'application/json', 'X-Agents-Session': session.id },
        body: JSON.stringify({ model: agent.id, messages: [{ role: 'user', content: 'Use time once more.' }], stream: true }),
      });
      assert.equal(streamed.status, 200);
      assert.equal(streamed.headers.get('x-agents-session'), session.id);
      assert.ok(streamed.headers.get('content-type').includes('text/event-stream'));
      assert.ok(streamed.headers.get('cache-control').includes('no-transform'));
      clearTimeout(timer);
      const events = await Promise.race([streamed.text(), new Promise((_, reject) => {
        timer = setTimeout(() => reject(new Error(`SSE continuation stalled after 8 seconds: providerCalls=${providerCalls}`)), 8000);
      })]);
      assert.ok(events.includes('The clock tool completed.'));
      assert.ok(events.includes('[DONE]'));
      assert.equal(providerCalls, 2);
      assert.ok(await db.prepare("SELECT COUNT(*) AS count FROM usage_ledger WHERE resource='model'").first('count') >= 4);
      const modelCharges = await db.prepare("SELECT CAST(amount_microusd AS TEXT) AS amount_microusd FROM usage_ledger WHERE resource='model'").all();
      assert.equal(modelCharges.results.length, 4);
      assert.ok(modelCharges.results.every(row => row.amount_microusd === '10'), 'Bill exactly the provider-reported API-equivalent cost');
      const beforeEviction = await request('GET', `/v1/sessions/${session.id}`);
      await mf.unsafeEvictDurableObject('', 'Session', { name: `owner:${session.id}` });
      const afterEviction = await request('GET', `/v1/sessions/${session.id}`);
      assert.deepEqual(afterEviction.messages, beforeEviction.messages, 'DO reconstruction must bulk-load the persisted message codec without data loss');
      assert.deepEqual(afterEviction.pending_messages, beforeEviction.pending_messages);
      assert.equal(afterEviction.last_error, beforeEviction.last_error);
      clearTimeout(timer);
      const redirectedSession = await request('POST', `/v1/agents/${agent.id}/sessions`, {}, 201);
      redirectProvider = true;
      providerCalls = 0;
      const rejectedRedirect = await Promise.race([
        mf.dispatchFetch('https://agents.test/v1/chat/completions', {
          method: 'POST', headers: { Authorization: `Bearer ${key}`, 'Content-Type': 'application/json', 'X-Agents-Session': redirectedSession.id },
          body: JSON.stringify({ model: agent.id, messages: [{ role: 'user', content: 'A provider redirect must fail closed.' }], stream: false }),
        }),
        new Promise((_, reject) => { timer = setTimeout(() => reject(new Error('Provider redirect rejection stalled')), 8000); }),
      ]);
      assert.equal(providerCalls, 1);
      assert.equal(redirectTargetCalls, 0, 'Never send provider credentials to a redirect target');
      assert.equal(rejectedRedirect.status, 502);
      assert.equal((await rejectedRedirect.json()).error.code, 'provider_error');
    } finally {
      clearTimeout(timer);
    }
  });
});
