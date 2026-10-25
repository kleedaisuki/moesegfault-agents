import assert from 'node:assert/strict';
import { createCipheriv, createHash, randomBytes } from 'node:crypto';
import { mkdir, mkdtemp, readdir, readFile } from 'node:fs/promises';
import { spawnSync } from 'node:child_process';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import { Miniflare, convertV4MiniflareOptions } from 'miniflare';
import { createIdentityFixture, runAuthJourney, runClientAssertionJourney, runInvalidClientKeyJourney } from './auth-journey.mjs';
import { createBrowserPrincipal } from './browser-principal.mjs';
import { createGenericProviderFixture, runGenericByokJourney } from './generic-byok-journey.mjs';
import { runExportJourney } from './export-journey.mjs';
import { createCompactionProviderFixture, runCompactionJourney } from './compaction-journey.mjs';
import { createObserverProviderFixture, runProjectionFaultJourney } from './observer-journey.mjs';
import { createResponsesProviderFixture, runResponsesNodeSdkJourney, runResponsesLocalToolsJourney,
  runResponsesCompactionJourney, runResponsesBackgroundRaceJourney, runResponsesStorageAndFailureJourney,
  runResponsesCancellationJourney, runResponsesIdempotencyRaceJourney, runSessionViewsJourney,
  runResponsesInputAndOptionsJourney, runResponsesUnknownUsageJourney, runSeededAttachmentPinJourney } from './responses-journey.mjs';
import { runResponsesPolicyJourney } from './responses-policy-journey.mjs';
import { createCoalescingProviderFixture, runResponsesCoalescingJourney } from './responses-coalescing-journey.mjs';
import { runSpaceJourney } from './space-journey.mjs';
import { runWorkspaceBlobJourney } from './workspace-blob-journey.mjs';
import { runJsonObserverJourney } from './json-observer-journey.mjs';
import { createChatRecoveryControl, runChatRecoveryJourney } from './chat-recovery-journey.mjs';
import { createAuxiliaryProviderFixture, runAuxiliaryJourney } from './auxiliary-journey.mjs';
import { runDeepSeekTokenizerJourney } from './deepseek-tokenizer-journey.mjs';
import { createPromptProviderFixture, promptRegistry, runPromptTemplatesJourney } from './prompt-templates-journey.mjs';
import { runResponsesToolBudgetJourney } from './responses-tool-budget.mjs';

const root = fileURLToPath(new URL('../../', import.meta.url));
const build = path.resolve(root, process.env.WORKER_BUILD_DIR ?? 'crates/agents-worker/build');
const key = 'mag_integration_synthetic_key_not_a_real_credential';
const hash = createHash('sha256').update(key).digest('base64url');

function selected(context, name, ...arguments_) {
  const filter = process.env.MAGENTS_WORKER_CASE;
  if (filter && !name.includes(filter)) return Promise.resolve();
  return context.test(name, ...arguments_);
}

test('compiled Rust Worker: authenticated D1 and session DO user journey', { timeout: 180000 }, async (t) => {
  const encryptionKey = randomBytes(32);
  const identity = createIdentityFixture();
  const genericProviders = createGenericProviderFixture();
  const compactionProviders = createCompactionProviderFixture();
  const observerProvider = createObserverProviderFixture();
  const responsesProvider = createResponsesProviderFixture();
  const coalescingProviders = createCoalescingProviderFixture();
  const chatRecoveryControl = createChatRecoveryControl();
  const auxiliaryProvider = createAuxiliaryProviderFixture();
  const promptProvider = createPromptProviderFixture();
  const deployment = JSON.parse(await readFile(path.join(root, 'wrangler.jsonc'), 'utf8'));
  const resourcePricing = deployment.env.staging.vars.RESOURCE_PRICING_JSON;
  let providerCalls = 0;
  let redirectProvider = false;
  let redirectTargetCalls = 0;
  let slowProvider = false;
  let byokProviderKey = null;
  let billingAvailable = true;
  let billingMalformed = false;
  let billingFetches = 0;
  let billingSubscriptions = [];
  await mkdir(path.join(root, '.temp'), { recursive: true });
  const temp = await mkdtemp(path.join(root, '.temp/worker-integration-'));
  process.env.TMP = process.env.TEMP = process.env.TMPDIR = temp;
  const mfOptions = convertV4MiniflareOptions({
    modulesRoot: build,
    modules: [
      { type: 'ESModule', path: path.join(build, 'worker/shim.mjs'),
        contents: await readFile(new URL('./workspace-blob-shim.mjs', import.meta.url), 'utf8') },
      { type: 'ESModule', path: path.join(build, 'worker/chat-recovery-shim.mjs'),
        contents: await readFile(new URL('./chat-recovery-shim.mjs', import.meta.url), 'utf8') },
      { type: 'ESModule', path: path.join(build, 'index.js') },
      { type: 'CompiledWasm', path: path.join(build, 'index_bg.wasm') },
    ],
    compatibilityDate: '2026-10-23',
    compatibilityFlags: ['enable_request_signal'],
    resourcePersistencePath: path.join(temp, 'storage'),
    d1Databases: ['DB'],
    r2Buckets: ['WORKSPACES', 'CONVERSATIONS', 'DIAGNOSTICS'],
    durableObjects: {
      SESSIONS: { className: 'Session', useSQLite: true },
      AUTH_SESSIONS: { className: 'AuthSession', useSQLite: true },
      CONVERSATION_STATE: { className: 'ConversationState', useSQLite: true },
      MODEL_OPERATIONS: { className: 'ModelOperation', useSQLite: true },
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
      IDENTITY_CLIENT_PRIVATE_JWK: JSON.stringify(identity.privateJwk),
      BILLING_PRODUCT_ID: 'agents', BILLING_MODE: 'test',
      MODEL_REGISTRY_JSON: JSON.stringify([{ provider: 'openrouter', model: 'test-model', context_window: 10000, max_output_tokens: 1000 }, { provider: 'deepseek', model: 'unpriced-direct-model', context_window: 10000, max_output_tokens: 1000 }]),
      RESOURCE_PRICING_JSON: resourcePricing,
      OPENROUTER_API_KEY: 'synthetic-provider-key-never-sent-outside-fixture',
      RUN_RESOURCE_RESERVATION_MICROUSD: '20000',
      // Independent Responses/Space principals need trial grants; owner-level
      // exhaustion remains covered separately rather than sharing a global cap.
      FREE_GRANT_BUDGET_MICROUSD_PER_DAY: '100000000',
      MODEL_PRICING_JSON: JSON.stringify({ 'openrouter/test-model': { input_microusd_per_million: 300000, cached_input_microusd_per_million: 6000, output_microusd_per_million: 1200000, revision: 'integration' } }),
      PROMPT_REGISTRY_JSON: promptRegistry,
    },
    outboundService: async request => {
      const promptResponse = await promptProvider.handle(request);
      if (promptResponse !== undefined) return promptResponse;
      const auxiliaryResponse = await auxiliaryProvider.handle(request);
      if (auxiliaryResponse !== undefined) return auxiliaryResponse;
      const chatRecoveryResponse = await chatRecoveryControl.handle(request);
      if (chatRecoveryResponse !== undefined) return chatRecoveryResponse;
      const coalescingResponse = await coalescingProviders.handle(request);
      if (coalescingResponse !== undefined) return coalescingResponse;
      const responsesResponse = await responsesProvider.handle(request);
      if (responsesResponse !== undefined) return responsesResponse;
      const observerResponse = await observerProvider.handle(request);
      if (observerResponse !== undefined) return observerResponse;
      const compactionResponse = await compactionProviders.handle(request);
      if (compactionResponse !== undefined) return compactionResponse;
      if (request.url === 'https://billing.moesegfault.dev/v1/me') {
        billingFetches++;
        assert.equal(request.headers.get('authorization'), 'Bearer synthetic-return-access-token');
        return Response.json(billingAvailable ? billingMalformed ? {} : { subscriptions: billingSubscriptions } : { error: 'unavailable' }, { status: billingAvailable ? 200 : 503 });
      }
      const genericResponse = await genericProviders.handle(request);
      if (genericResponse !== undefined) return genericResponse;
      if (request.url.startsWith('https://redirect-target.test/')) {
        redirectTargetCalls++;
        throw new Error('Provider redirect must never be followed');
      }
      if (request.url === 'https://openrouter.ai/api/v1/chat/completions') {
        providerCalls++;
        if (byokProviderKey !== null) {
          assert.ok(request.headers.get('authorization') === `Bearer ${byokProviderKey}`, 'BYOK must use only the account-owned provider credential, never the platform key');
        }
        if (slowProvider) {
          assert.equal(providerCalls, 1, 'Heartbeat must not initiate extra provider calls');
          const input = await request.json();
          assert.equal(input.stream, true);
          await new Promise(resolve => setTimeout(resolve, 16000));
          return new Response(`data: ${JSON.stringify({ id: 'mock-heartbeat', model: 'test-model', choices: [{ index: 0, delta: { role: 'assistant', content: 'Delayed provider completed.' }, finish_reason: 'stop' }], usage: { prompt_tokens: 100, completion_tokens: 10, total_tokens: 110, cost: 0.00001 } })}\n\ndata: [DONE]\n\n`, { headers: { 'Content-Type': 'text/event-stream' } });
        }
        if (redirectProvider) return new Response(null, { status: 302, headers: { Location: 'https://redirect-target.test/steal' } });
        const input = await request.json();
        assert.equal(input.stream, true);
        assert.equal(input.model, 'test-model');
        assert.ok(input.tools.some(tool => tool.function.name === 'time'));
        if (providerCalls === 1 && byokProviderKey === null) {
          const before = await db.prepare('SELECT status,last_input_at,config_snapshot_json FROM sessions WHERE id=?1').bind(input.session_id).first();
          const rejected = await mf.dispatchFetch(`https://agents.test/v1/sessions/${input.session_id}/resume`, {
            method: 'POST', headers: { Authorization: `Bearer ${key}`, 'Content-Type': 'application/json' }, body: '{}',
          });
          assert.equal(rejected.status, 409, 'Resume must reject an executing run, not roll back its quota admission');
          const after = await db.prepare('SELECT status,last_input_at,config_snapshot_json FROM sessions WHERE id=?1').bind(input.session_id).first();
          assert.deepEqual(after, before);
        }
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
      if (request.url === 'https://identity.test/token' || request.url === 'https://identity.test/revoke') return identity.handle(request);
      if (request.url === 'https://identity.test/jwks') return Response.json(identity.jwks);
      assert.equal(request.url, 'https://identity.test/.well-known/openid-configuration');
      return Response.json({ issuer: 'https://identity.test', authorization_endpoint: 'https://identity.test/authorize',
        token_endpoint: 'https://identity.test/token', jwks_uri: 'https://identity.test/jwks',
        end_session_endpoint: 'https://identity.test/logout', revocation_endpoint: 'https://identity.test/revoke',
        code_challenge_methods_supported: ['S256'], token_endpoint_auth_methods_supported: ['private_key_jwt', 'none'] });
    },
  });
  const mf = new Miniflare(mfOptions);
  t.after(() => mf.dispose());
  let db = await mf.getD1Database('DB');
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
  await selected(t, 'unknown credentials fail without touching the key', async () => {
    await request('GET', '/v1/models', undefined, 401, 'mag_unknown');
    assert.equal((await db.prepare("SELECT last_used_at FROM api_keys WHERE id='key'").first()).last_used_at, null);
  });
  await selected(t, 'first API key use binds an integer across Wasm -> JavaScript -> D1', async () => {
    const models = await request('GET', '/v1/models');
    assert.deepEqual(models.data, []);
    assert.deepEqual(models.available_models.map(model => [model.provider, model.model]), [['openrouter', 'test-model'], ['deepseek', 'unpriced-direct-model']]);
    assert.deepEqual(models.available_platform_models.map(model => [model.provider, model.model]), [['openrouter', 'test-model']]);
    const row = await db.prepare("SELECT last_used_at,typeof(last_used_at) AS kind FROM api_keys WHERE id='key'").first();
    assert.equal(row.kind, 'integer');
    assert.ok(row.last_used_at > 1_700_000_000);
    await request('GET', '/v1/models');
    assert.equal((await db.prepare("SELECT last_used_at FROM api_keys WHERE id='key'").first()).last_used_at, row.last_used_at);
  });
  await selected(t, 'browser auth lifecycle uses real AuthSession DO and integer D1 values', () =>
    runAuthJourney({ mf, db, encryptionKey, origin: 'https://agents.test' }));
  await selected(t, 'callback and refresh ES256 assertions verify against the registered client key', () =>
    runClientAssertionJourney({ mf, db, origin: 'https://agents.test', identity }));
  await selected(t, 'invalid confidential-client scalar fails before exchange without authenticating', () =>
    runInvalidClientKeyJourney({ mf, db, origin: 'https://agents.test', identity, updatePrivateJwk: async jwk => {
      mfOptions.workers[0].config.env.IDENTITY_CLIENT_PRIVATE_JWK = { type: 'text', value: JSON.stringify(jwk) };
      await mf.setOptions(mfOptions);
      db = await mf.getD1Database('DB');
    } }));
  await selected(t, 'subscription return authenticates, bypasses a warm snapshot and preserves grants on refresh failure', async () => {
    const guest = await mf.dispatchFetch('https://agents.test/billing/return?plan=agents-pro&return_to=https://evil.test', { redirect: 'manual' });
    assert.equal(guest.status, 302);
    assert.equal(guest.headers.get('location'), 'https://agents.test/auth/login?return_to=%2Fbilling%2Freturn');
    assert.equal(billingFetches, 0);
    mfOptions.workers[0].config.env.BILLING_API_ORIGIN = { type: 'text', value: 'https://billing.moesegfault.dev' };
    await mf.setOptions(mfOptions);
    db = await mf.getD1Database('DB');
    try {
      const owner = 'billing-return-owner';
      const principal = await createBrowserPrincipal({ mf, db, encryptionKey, owner, oauth: { access_token: 'synthetic-return-access-token' } });
      const now = Math.floor(Date.now() / 1000);
      await db.prepare("INSERT INTO subscription_snapshots(account_id,product_id,subscription_json,fetched_at,valid_until) VALUES(?1,'agents','[]',?2,?3)").bind(owner, now, now + 300).run();
      assert.equal((await principal.browser('GET', '/v1/account')).plan, 'free');
      billingSubscriptions = [{ id: 'synthetic-agents-grant', product_id: 'agents', plan_id: 'agents-go', status: 'active', current_period_start: now - 10, current_period_end: now + 2592000, entitlements: ['agents.access', 'agents.tier.go'] }];
      const returned = await mf.dispatchFetch('https://agents.test/billing/return?plan=agents-pro', { headers: { Cookie: principal.cookie }, redirect: 'manual' });
      assert.equal(returned.status, 302);
      assert.equal(returned.headers.get('location'), 'https://agents.test/#usage');
      assert.equal(billingFetches, 1, 'The return must bypass even a just-fetched Free snapshot');
      assert.equal((await principal.browser('GET', '/v1/account')).plan, 'go', 'The query plan never grants Pro');
      billingSubscriptions[0].current_period_end += 2592000;
      const renewed = await mf.dispatchFetch('https://agents.test/billing/return', { headers: { Cookie: principal.cookie }, redirect: 'manual' });
      assert.equal(renewed.headers.get('location'), 'https://agents.test/#usage');
      assert.equal(billingFetches, 2, 'A second immediate return must fetch the extended period');
      const snapshot = await db.prepare("SELECT subscription_json FROM subscription_snapshots WHERE account_id=?1 AND product_id='agents'").bind(owner).first('subscription_json');
      assert.equal(JSON.parse(snapshot)[0].current_period_end, billingSubscriptions[0].current_period_end);
      billingAvailable = false;
      const failed = await mf.dispatchFetch('https://agents.test/billing/return', { headers: { Cookie: principal.cookie }, redirect: 'manual' });
      assert.equal(failed.headers.get('location'), 'https://agents.test/?billing_refresh=failed#usage');
      assert.equal((await principal.browser('GET', '/v1/account')).plan, 'go');
      assert.equal(await db.prepare("SELECT subscription_json FROM subscription_snapshots WHERE account_id=?1 AND product_id='agents'").bind(owner).first('subscription_json'), snapshot);
      assert.equal(await db.prepare('SELECT COUNT(*) AS count FROM browser_sessions WHERE account_id=?1').bind(owner).first('count'), 1);
      billingAvailable = true;
      billingMalformed = true;
      const malformed = await mf.dispatchFetch('https://agents.test/billing/return', { headers: { Cookie: principal.cookie }, redirect: 'manual' });
      assert.equal(malformed.headers.get('location'), 'https://agents.test/?billing_refresh=failed#usage');
      assert.equal(await db.prepare("SELECT subscription_json FROM subscription_snapshots WHERE account_id=?1 AND product_id='agents'").bind(owner).first('subscription_json'), snapshot, 'Malformed 200 JSON must not erase a valid paid grant');
    } finally {
      billingAvailable = true;
      billingMalformed = false;
      delete mfOptions.workers[0].config.env.BILLING_API_ORIGIN;
      await mf.setOptions(mfOptions);
      db = await mf.getD1Database('DB');
    }
  });
  await selected(t, 'agent CRUD, immutable session prefix, DO close and revocation', async (t) => {
    const agent = await request('POST', '/v1/agents', { name: 'Integration', instructions: 'Pinned original instructions', model: { provider: 'openrouter', model: 'test-model' }, workspace: false }, 201);
    assert.equal((await request('GET', '/v1/models')).data[0].id, agent.id);
    const session = await request('POST', `/v1/agents/${agent.id}/sessions`, {}, 201);
    assert.equal(session.status, 'idle');
    await request('PATCH', `/v1/agents/${agent.id}`, { instructions: 'Changed instructions' });
    const stored = await db.prepare('SELECT config_snapshot_json FROM sessions WHERE id=?1').bind(session.id).first();
    assert.ok(stored.config_snapshot_json.includes('Pinned original instructions'));
    assert.ok(!stored.config_snapshot_json.includes('Changed instructions'));
    await selected(t, 'read a live DO session through HTTP', () => request('GET', `/v1/sessions/${session.id}`));
    await selected(t, 'close a DO session through HTTP', () => request('DELETE', `/v1/sessions/${session.id}`));
    assert.equal((await db.prepare('SELECT status FROM sessions WHERE id=?1').bind(session.id).first()).status, 'closed');
    await selected(t, 'archived session resumes once and the same public retry is idempotent', async () => {
      const resumed = await request('POST', `/v1/sessions/${session.id}/resume`, {});
      assert.equal(resumed.status, 'idle');
      assert.deepEqual(resumed.pinned, session.pinned);
      assert.equal(resumed.input_revision, session.input_revision + 1);
      assert.ok(Number.isSafeInteger(resumed.last_input_at_ms));
      assert.ok(resumed.last_input_at_ms >= session.last_input_at_ms);
      const retried = await request('POST', `/v1/sessions/${session.id}/resume`, {});
      assert.equal(retried.status, 'idle');
      assert.deepEqual(retried.pinned, resumed.pinned);
      assert.equal(retried.input_revision, resumed.input_revision);
      assert.equal(retried.last_input_at_ms, resumed.last_input_at_ms);
      const listed = (await request('GET', '/v1/sessions')).data.find(row => row.id === session.id);
      assert.equal(listed.input_revision, resumed.input_revision);
      assert.equal(listed.last_input_at_ms, resumed.last_input_at_ms);
      assert.deepEqual(listed.pinned, session.pinned);
      const types = await db.prepare("SELECT json_type(config_snapshot_json,'$.last_input_at_ms') AS input_time,json_type(config_snapshot_json,'$.input_revision') AS revision FROM sessions WHERE id=?1").bind(session.id).first();
      assert.deepEqual(types, { input_time: 'integer', revision: 'integer' });
    });
    await request('DELETE', `/v1/sessions/${session.id}`);
    await request('DELETE', `/v1/agents/${agent.id}`);
    await request('GET', `/v1/agents/${agent.id}`, undefined, 404);
    await db.prepare("UPDATE api_keys SET revoked_at=1 WHERE id='key'").run();
    await request('GET', '/v1/models', undefined, 401);
  });
  await selected(t, 'non-streaming SDK call advances alarm, invokes time, and settles two model rounds', async () => {
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
      await runExportJourney({ mf, db, request, sessionId: session.id, key, createBrowserPrincipal, encryptionKey });
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
  await selected(t, 'slow provider receives SSE comment heartbeat before data and closes after DONE', { timeout: 32000 }, async () => {
    const heartbeatKey = 'mag_heartbeat_synthetic_fixture_only';
    await db.batch([
      db.prepare("INSERT INTO accounts(id,issuer,subject,created_at,updated_at) VALUES('heartbeat-owner','https://identity.test','heartbeat',0,0)"),
      db.prepare("INSERT INTO usage_accounts(account_id,updated_at) VALUES('heartbeat-owner',0)"),
      db.prepare("INSERT INTO api_keys(id,account_id,name,key_hash,key_prefix,created_at) VALUES('heartbeat-key','heartbeat-owner','heartbeat',?1,'mag_heartbeat',0)").bind(createHash('sha256').update(heartbeatKey).digest('base64url')),
    ]);
    const agent = await request('POST', '/v1/agents', { name: 'Heartbeat', tools: ['time'], workspace: false, model: { provider: 'openrouter', model: 'test-model' } }, 201, heartbeatKey);
    const session = await request('POST', `/v1/agents/${agent.id}/sessions`, {}, 201, heartbeatKey);
    slowProvider = true;
    providerCalls = 0;
    let timer;
    try {
      const started = Date.now();
      const response = await mf.dispatchFetch('https://agents.test/v1/chat/completions', {
        method: 'POST', headers: { Authorization: `Bearer ${heartbeatKey}`, 'Content-Type': 'application/json', 'X-Agents-Session': session.id },
        body: JSON.stringify({ model: agent.id, messages: [{ role: 'user', content: 'Wait for the provider response.' }], stream: true }),
      });
      assert.equal(response.status, 200);
      const initialInput = await db.prepare('SELECT last_input_at FROM sessions WHERE id=?1').bind(session.id).first();
      const reader = response.body.getReader();
      const decoder = new TextDecoder();
      let wire = '';
      let heartbeatAt;
      const receive = async () => {
        while (true) {
          const { done, value } = await reader.read();
          if (done) break;
          wire += decoder.decode(value, { stream: true });
          if (heartbeatAt === undefined && wire.includes(': keepalive\n\n')) heartbeatAt = Date.now() - started;
          if (wire.includes('Delayed provider completed.')) assert.notEqual(heartbeatAt, undefined, 'A comment heartbeat must arrive before delayed provider content');
        }
        wire += decoder.decode();
      };
      await Promise.race([receive(), new Promise((_, reject) => {
        timer = setTimeout(() => reject(new Error('Heartbeat stream did not reach EOF within 30 seconds')), 30000);
      })]);
      assert.ok(heartbeatAt >= 14000, `Unexpected heartbeat timing: ${heartbeatAt}`);
      assert.ok(wire.indexOf(': keepalive\n\n') < wire.indexOf('Delayed provider completed.'));
      assert.ok(wire.trimEnd().endsWith('data: [DONE]'), 'DONE must be followed by EOF, not more heartbeat comments');
      assert.equal(providerCalls, 1);
      const status = await request('GET', `/v1/sessions/${session.id}`, undefined, 200, heartbeatKey);
      assert.equal(status.input_revision, session.input_revision + 1);
      assert.equal(status.last_input_at_ms, initialInput.last_input_at);
      assert.deepEqual(status.messages.map(message => message.role), ['user', 'assistant']);
      const charges = await db.prepare("SELECT CAST(amount_microusd AS TEXT) AS amount FROM usage_ledger WHERE resource='model' AND reservation_id IN (SELECT id FROM usage_reservations WHERE session_id=?1)").bind(session.id).all();
      assert.deepEqual(charges.results, [{ amount: '10' }]);
    } finally {
      clearTimeout(timer);
      slowProvider = false;
    }
  });
  await selected(t, 'BYOK uses the owned key, isolates credentials, and charges resources but no model cost', { timeout: 12000 }, async () => {
    const owner = 'byok-owner';
    const cookie = randomBytes(32).toString('base64url');
    const csrf = randomBytes(32).toString('base64url');
    const cookieHash = createHash('sha256').update(cookie).digest('base64url');
    const now = Math.floor(Date.now() / 1000);
    const nonce = randomBytes(12);
    const cipher = createCipheriv('aes-256-gcm', encryptionKey, nonce);
    cipher.setAAD(Buffer.from(`session:${cookieHash}`));
    const encrypted = Buffer.concat([Buffer.from([1]), nonce,
      cipher.update(Buffer.from(JSON.stringify({ csrf_token: csrf, oauth: {}, access_expires_at: now + 3600 }))),
      cipher.final(), cipher.getAuthTag()]).toString('base64url');
    await db.batch([
      db.prepare('INSERT INTO accounts(id,issuer,subject,created_at,updated_at) VALUES(?,?,?,?,?)').bind(owner, 'https://identity.test', owner, now, now),
      db.prepare('INSERT INTO usage_accounts(account_id,updated_at) VALUES(?,?)').bind(owner, now),
      db.prepare('INSERT INTO browser_sessions(token_hash,account_id,tokens_encrypted,csrf_hash,expires_at,created_at) VALUES(?,?,?,?,?,?)')
        .bind(cookieHash, owner, encrypted, createHash('sha256').update(csrf).digest('base64url'), now + 3600, now),
    ]);
    async function browser(method, endpoint, body, expected = 200) {
      const response = await mf.dispatchFetch(`https://agents.test${endpoint}`, {
        method, headers: { Cookie: `__Host-magents_session=${cookie}`, Origin: 'https://agents.test', 'X-CSRF-Token': csrf, 'Content-Type': 'application/json' },
        ...(body === undefined ? {} : { body: JSON.stringify(body) }),
      });
      assert.equal(response.status, expected, `${method} ${endpoint}`);
      return response.json();
    }
    const apiKey = (await browser('POST', '/v1/api-keys', { name: 'BYOK journey' }, 201)).key;
    const userKey = 'synthetic-user-owned-openrouter-key-distinct-from-platform';
    const credential = await browser('PUT', '/v1/provider-keys/openrouter', { api_key: userKey });
    const model = { provider: 'byok', model: 'test-model', credential_id: credential.id };
    const rejected = await request('POST', '/v1/agents', { name: 'Foreign credential denied', model }, 400);
    assert.equal(rejected.error.code, 'invalid_request_error');
    const agent = await request('POST', '/v1/agents', { name: 'Owned BYOK', model, tools: ['time'], workspace: false }, 201, apiKey);
    const session = await request('POST', `/v1/agents/${agent.id}/sessions`, {}, 201, apiKey);
    redirectProvider = false;
    providerCalls = 0;
    byokProviderKey = userKey;
    let timer;
    try {
      const response = await Promise.race([
        mf.dispatchFetch('https://agents.test/v1/chat/completions', {
          method: 'POST', headers: { Authorization: `Bearer ${apiKey}`, 'Content-Type': 'application/json', 'X-Agents-Session': session.id },
          body: JSON.stringify({ model: agent.id, messages: [{ role: 'user', content: 'Call time and report the result.' }], stream: false }),
        }),
        new Promise((_, reject) => { timer = setTimeout(() => reject(new Error('BYOK inference stalled beyond 8 seconds')), 8000); }),
      ]);
      assert.equal(response.status, 200);
      assert.equal((await response.json()).choices[0].message.content, 'The clock tool completed.');
      assert.equal(providerCalls, 2);
      const status = await request('GET', `/v1/sessions/${session.id}`, undefined, 200, apiKey);
      assert.ok(status.messages.some(message => message.role === 'tool' && message.tool_call_id === 'call_time'));
      const modelCosts = await db.prepare("SELECT CAST(l.amount_microusd AS TEXT) AS amount,l.price_version FROM usage_ledger l JOIN usage_reservations r ON r.id=l.reservation_id WHERE r.account_id=?1 AND l.resource='model'").bind(owner).all();
      assert.deepEqual(modelCosts.results, [{ amount: '0', price_version: 'byok' }, { amount: '0', price_version: 'byok' }]);
      await request('DELETE', `/v1/sessions/${session.id}`, undefined, 200, apiKey);
      const wallet = await db.prepare('SELECT CAST(spent_microusd AS TEXT) AS spent,CAST(reserved_microusd AS TEXT) AS held FROM usage_accounts WHERE account_id=?1').bind(owner).first();
      assert.ok(BigInt(wallet.spent) > 0n, 'BYOK still pays observed platform resources');
      assert.equal(wallet.held, '0', 'Finished and archived BYOK work must release all holds');
      const held = await db.prepare("SELECT COUNT(*) AS count FROM usage_reservations WHERE account_id=?1 AND state='reserved'").bind(owner).first('count');
      assert.equal(held, 0);
    } finally {
      clearTimeout(timer);
      byokProviderKey = null;
    }
  });
  for (const protocol of ['openai', 'anthropic']) {
    await selected(t, `generic ${protocol} BYOK preserves route identity and completes tools without model charges`, { timeout: 32000 }, () =>
      runGenericByokJourney({ mf, db, encryptionKey, request, providers: genericProviders, protocol }));
  }
  for (const protocol of ['openai', 'anthropic']) {
    await selected(t, `generic ${protocol} persists JSON compaction then streams a complete continuation`, { timeout: 40000 }, () =>
      runCompactionJourney({ mf, db, encryptionKey, request, providers: compactionProviders, protocol }));
  }
  await selected(t, 'terminal delivery survives a failed final D1 projection after canonical settlement', { timeout: 24000 }, () =>
    runProjectionFaultJourney({ mf, db, encryptionKey, request, provider: observerProvider, diagnostic: value => t.diagnostic(value) }));
  const responseContext = () => ({ mf, db, encryptionKey, request, providers: responsesProvider, coalescingProviders, diagnostic: value => t.diagnostic(value) });
  await selected(t, 'response-wide server tool allowance survives disobedient calls and cold retrieval', { timeout: 40000 }, () =>
    runResponsesToolBudgetJourney(responseContext()));
  await selected(t, 'Responses auxiliary compact and count', { timeout: 60000 }, () =>
    runAuxiliaryJourney({ mf, db, encryptionKey, request, provider: auxiliaryProvider }));
  await selected(t, 'DeepSeek exact count-only tokenizer cold warm and profile replacement', { timeout: 60000 }, () =>
    runDeepSeekTokenizerJourney(responseContext()));
  await selected(t, 'Responses prompt templates preserve version owner variables and replay identity', { timeout: 40000 }, () =>
    runPromptTemplatesJourney({ ...responseContext(), provider: promptProvider,
      replaceRegistry: async value => {
        mfOptions.workers[0].config.env.PROMPT_REGISTRY_JSON.value = value;
        await mf.setOptions(mfOptions);
        db = await mf.getD1Database('DB');
      } }));
  await selected(t, 'Chat internal observation loss recovers original durable text and terminal without replay', { timeout: 40000 }, () =>
    runChatRecoveryJourney({ ...responseContext(), chatRecoveryControl }));
  await selected(t, 'JSON observers keep internal heartbeats private and preserve terminal success, errors and detached runs', { timeout: 40000 }, () =>
    runJsonObserverJourney(responseContext()));
  await selected(t, 'Space real R2 file bytes, metadata, owner isolation and failed-upload accounting', { timeout: 16000 }, () =>
    runSpaceJourney(responseContext()));
  await selected(t, 'Workspace shared owner archive historical fork and immutable download retain one physical copy', { timeout: 24000 }, () =>
    runWorkspaceBlobJourney(responseContext()));
  await selected(t, 'Responses actual Node SDK streams, recovers and returns local function results', { timeout: 16000 }, () =>
    runResponsesNodeSdkJourney(responseContext()));
  await selected(t, 'Responses mixed server/local tools reject partial results and resume an archived head', { timeout: 16000 }, () =>
    runResponsesLocalToolsJourney(responseContext()));
  await selected(t, 'Responses repeated compaction retains exact fixed and request-scoped instructions', { timeout: 16000 }, () =>
    runResponsesCompactionJourney(responseContext()));
  await selected(t, 'Responses concurrent children admit independent background executions', { timeout: 16000 }, () =>
    runResponsesBackgroundRaceJourney(responseContext()));
  await selected(t, 'Responses no-store, upstream failure and output-budget terminals remain distinct', { timeout: 16000 }, () =>
    runResponsesStorageAndFailureJourney(responseContext()));
  await selected(t, 'Responses durable cancellation cannot replay or revive an accepted execution', { timeout: 16000 }, () =>
    runResponsesCancellationJourney(responseContext()));
  await selected(t, 'Responses concurrent idempotency receipt creates one session and one run', { timeout: 16000 }, () =>
    runResponsesIdempotencyRaceJourney(responseContext()));
  await selected(t, 'history metadata validators and cursor pages reduce bytes without changing legacy history', { timeout: 16000 }, () =>
    runSessionViewsJourney(responseContext()));
  await selected(t, 'Responses typed context and generation controls are implemented rather than silently ignored', { timeout: 16000 }, () =>
    runResponsesInputAndOptionsJourney(responseContext()));
  await selected(t, 'Responses unknown reasoning preserves exact known usage without inventing a breakdown', { timeout: 16000 }, () =>
    runResponsesUnknownUsageJourney(responseContext()));
  await selected(t, 'Responses caller function selection and final structured output remain enforced across tools', { timeout: 24000 }, () =>
    runResponsesPolicyJourney(responseContext()));
  await selected(t, 'seeded D1 attachment pins survive observer cancellation until canonical DO completion', { timeout: 16000 }, () =>
    runSeededAttachmentPinJourney(responseContext()));
  await selected(t, 'Responses text coalescing bounds quiet latency and reduces actual SSE frames without changing Chat', { timeout: 24000 }, () =>
    runResponsesCoalescingJourney(responseContext()));
  await selected(t, 'native DO fetch rejection preserves known session identity and sanitized recovery metadata', { timeout: 12000 }, async () => {
    const principal = await createBrowserPrincipal({ mf, db, encryptionKey, owner: 'forward-recovery-owner' });
    const agent = await request('POST', '/v1/agents', { name: 'Forward failure', workspace: false, model: { provider: 'openrouter', model: 'test-model' } }, 201, principal.apiKey);
    const session = await request('POST', `/v1/agents/${agent.id}/sessions`, {}, 201, principal.apiKey);
    const before = await request('GET', `/v1/sessions/${session.id}`, undefined, 200, principal.apiKey);
    const walletBefore = await db.prepare('SELECT spent_microusd,reserved_microusd FROM usage_accounts WHERE account_id=?1').bind(principal.owner).first();
    const sentinel = 'synthetic-private-native-do-error-never-disclose';
    const throwingWorker = convertV4MiniflareOptions({
      name: 'throwing-session-fixture', modules: true, compatibilityDate: '2026-10-23',
      script: `export class ThrowingSession {
        constructor() { this.calls = 0; }
        fetch(request) {
          if (new URL(request.url).pathname === '/count') return Response.json({ calls: this.calls });
          this.calls++;
          throw new Error(${JSON.stringify(sentinel)});
        }
      }; export default { fetch() { return new Response(null, {status:404}); } };`,
      durableObjects: { SELF: { className: 'ThrowingSession', useSQLite: true } },
    }).workers[0];
    const failures = [];
    const primary = mfOptions.workers[0];
    const throwingOptions = {
      ...mfOptions,
      workers: [{ ...primary, config: { ...primary.config, env: { ...primary.config.env,
        SESSIONS: { type: 'durable-object', worker: 'throwing-session-fixture', exportName: 'ThrowingSession' },
      } } }, throwingWorker],
      // Suppress the intentionally throwing fixture's stack; inspect only structured application diagnostics.
      handleStructuredLogs(record) {
        try {
          const entry = JSON.parse(record.message);
          if (entry.event === 'request.failed') failures.push(entry);
        } catch { /* Non-JSON platform exception text is not application telemetry. */ }
      },
    };
    try {
      await mf.setOptions(throwingOptions);
      db = await mf.getD1Database('DB');
      const response = await mf.dispatchFetch('https://agents.test/v1/chat/completions', {
        method: 'POST', headers: { Authorization: `Bearer ${principal.apiKey}`, 'Content-Type': 'application/json', 'X-Agents-Session': session.id },
        body: JSON.stringify({ model: agent.id, messages: [{ role: 'user', content: 'Synthetic forward rejection.' }], stream: false }),
      });
      const body = await response.text();
      assert.equal(response.status, 500);
      assert.equal(response.headers.get('x-agents-session'), session.id, 'A failed observer must retain the known session recovery handle');
      assert.equal(JSON.parse(body).error.code, 'internal_error');
      assert.ok(!body.includes(sentinel));
      const namespace = await mf.getDurableObjectNamespace('SESSIONS');
      const counter = await namespace.get(namespace.idFromName(`${principal.owner}:${session.id}`)).fetch('https://fixture/count');
      assert.equal((await counter.json()).calls, 1, 'A rejected DO fetch must never be replayed');
      const deadline = Date.now() + 1000;
      while (failures.length === 0 && Date.now() < deadline) await new Promise(resolve => setTimeout(resolve, 10));
      assert.equal(failures.length, 1);
      const event = failures[0];
      assert.deepEqual(Object.keys(event.fields).sort(), ['phase', 'trace_id', 'session_id', 'agent_id', 'outcome', 'error_code', 'status'].sort());
      assert.equal(event.fields.phase, 'session_forward');
      assert.equal(event.fields.session_id, session.id);
      assert.equal(event.fields.agent_id, agent.id);
      assert.equal(event.fields.status, 500);
      assert.equal(event.fields.error_code, 'internal_error');
      assert.match(event.fields.trace_id, /^[0-9a-f]{32}$/);
      assert.ok(['javascript', 'binding', 'd1', 'serialization', 'runtime', 'platform'].includes(event.fields.outcome));
      assert.ok(!JSON.stringify(event).includes(sentinel));
      assert.deepEqual(await db.prepare('SELECT spent_microusd,reserved_microusd FROM usage_accounts WHERE account_id=?1').bind(principal.owner).first(), walletBefore);
    } finally {
      await mf.setOptions(mfOptions);
      db = await mf.getD1Database('DB');
    }
    const after = await request('GET', `/v1/sessions/${session.id}`, undefined, 200, principal.apiKey);
    assert.deepEqual(after.messages, before.messages);
    assert.equal(after.input_revision, before.input_revision);
    assert.equal(after.last_input_at_ms, before.last_input_at_ms);
  });
});
