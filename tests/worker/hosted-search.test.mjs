import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { createHash, randomBytes } from 'node:crypto';
import { mkdir, mkdtemp, readFile, readdir, writeFile } from 'node:fs/promises';
import path from 'node:path';
import test from 'node:test';
import { fileURLToPath } from 'node:url';
import OpenAI from 'openai';
import { Miniflare, convertV4MiniflareOptions } from 'miniflare';
import { createIdentityFixture } from './auth-journey.mjs';
import { createBrowserPrincipal } from './browser-principal.mjs';
import { createNativeSemanticsProvider } from './native-semantics-journey.mjs';

const root = fileURLToPath(new URL('../../', import.meta.url));
const build = path.resolve(root, process.env.WORKER_BUILD_DIR ?? 'crates/agents-worker/build');
const origin = 'https://agents.test';
const source = 'https://doc.rust-lang.org/book/ch04-00-understanding-ownership.html';
const answer = 'Ownership is checked by Rust. [Source](' + source + ')';
const cases = ['platform', 'byok', 'responses_byok', 'empty', 'http_error', 'unknown'];
const usage = { input_tokens: 7, output_tokens: 2, total_tokens: 9,
  input_tokens_details: { cached_tokens: 0 }, output_tokens_details: {} };
const final = { id: 'outer-final', type: 'message', role: 'assistant', status: 'completed',
  content: [{ type: 'output_text', text: answer, annotations: [], logprobs: [] }] };
const call = (id, action) => ({ id: 'outer-' + id, type: 'function_call', name: '__agents_web_search',
  call_id: id, arguments: JSON.stringify(action), status: 'completed' });

async function migrate(db) {
  for (const name of (await readdir(path.join(root, 'migrations'))).filter(name => name.endsWith('.sql')).sort()) {
    const parsed = spawnSync(process.env.PYTHON ?? 'python', ['-c',
      'import sys,sqlite3,json\ns: str=""; out: list[str]=[]\nfor line in sys.stdin:\n s+=line\n if sqlite3.complete_statement(s): out.append(s); s=""\nassert not s.strip(), s\nprint(json.dumps(out))'],
    { input: await readFile(path.join(root, 'migrations', name), 'utf8'), encoding: 'utf8' });
    assert.equal(parsed.status, 0, `${name}: ${parsed.stderr}`);
    await db.batch(JSON.parse(parsed.stdout).map(statement => db.prepare(statement)));
  }
}

function validateWire(response, events) {
  const checked = spawnSync(process.env.MAGENTS_CURRENT_SCHEMA_PYTHON,
    ['tests/e2e/current_responses_sdk.py', 'response-wire', '--input', '-'],
    { cwd: root, input: JSON.stringify({ response, events }), encoding: 'utf8', maxBuffer: 1024 * 1024 });
  assert.equal(checked.status, 0, checked.stderr || checked.stdout);
  return JSON.parse(checked.stdout);
}

test('compiled Rust Worker: independently funded native web search and replay', { timeout: 120000 }, async t => {
  assert.ok(process.env.MAGENTS_CURRENT_SCHEMA_PYTHON, 'Use the current strict SDK interpreter');
  const selected = cases.filter(name => !process.env.MAGENTS_HOSTED_SEARCH_CASE || name === process.env.MAGENTS_HOSTED_SEARCH_CASE);
  assert.ok(selected.length, 'Unknown hosted search case');
  const encryptionKey = randomBytes(32), identity = createIdentityFixture(), providers = createNativeSemanticsProvider();
  const deployment = JSON.parse(await readFile(path.join(root, 'wrangler.jsonc'), 'utf8'));
  await mkdir(path.join(root, '.temp'), { recursive: true });
  const directory = await mkdtemp(path.join(root, '.temp/hosted-search-'));
  const requests = [];
  let active;
  const mf = new Miniflare(convertV4MiniflareOptions({ modulesRoot: build,
    modules: [{ type: 'ESModule', path: path.join(build, 'worker/shim.mjs') },
      { type: 'ESModule', path: path.join(build, 'index.js') }, { type: 'CompiledWasm', path: path.join(build, 'index_bg.wasm') }],
    compatibilityDate: '2026-10-23', compatibilityFlags: ['enable_request_signal'],
    resourcePersistencePath: path.join(directory, 'storage'), d1Databases: ['DB'], r2Buckets: ['WORKSPACES', 'CONVERSATIONS', 'DIAGNOSTICS'],
    durableObjects: { SESSIONS: { className: 'Session', useSQLite: true }, AUTH_SESSIONS: { className: 'AuthSession', useSQLite: true },
      CONVERSATION_STATE: { className: 'ConversationState', useSQLite: true }, MODEL_OPERATIONS: { className: 'ModelOperation', useSQLite: true } },
    ratelimits: Object.fromEntries(['INGRESS', 'ACCOUNT', 'MUTATION'].map((name, index) => [`${name}_RATE_LIMITER`, { namespace_id: String(index + 1), simple: { limit: 1000, period: 60 } }])),
    bindings: { ENVIRONMENT: 'test', APP_ORIGIN: origin, ALLOWED_ORIGIN: origin,
      DATA_ENCRYPTION_KEY: encryptionKey.toString('base64url'), IDENTITY_ISSUER: 'https://identity.test',
      IDENTITY_WEB_CLIENT_ID: 'test-dashboard', IDENTITY_NATIVE_CLIENT_ID: 'test-native', IDENTITY_REDIRECT_URI: `${origin}/auth/callback`,
      IDENTITY_NATIVE_REDIRECT_URI: 'http://127.0.0.1/callback', IDENTITY_CLIENT_PRIVATE_JWK: JSON.stringify(identity.privateJwk),
      BILLING_PRODUCT_ID: 'agents', BILLING_MODE: 'test', MODEL_REGISTRY_JSON: '[]', MODEL_PRICING_JSON: '{}',
      HOSTED_TOOL_RUNTIMES: 'web_search', DEEPSEEK_API_KEY: 'synthetic-platform-search-key',
      HOSTED_TOOL_CAPABILITIES_JSON: JSON.stringify({ version: 1, platform_images: false, platform_search: true,
        file_search: false, container_memory: null, available_container_profiles: [], code_network_disabled: false, code_network_allowlist: false }),
      RESOURCE_PRICING_JSON: deployment.env.staging.vars.RESOURCE_PRICING_JSON,
      RUN_RESOURCE_RESERVATION_MICROUSD: '20000', FREE_GRANT_BUDGET_MICROUSD_PER_DAY: '100000000' },
    async outboundService(request) {
      const url = new URL(request.url);
      if (url.origin === 'https://owned-search-service.example.com') {
        assert.equal(active.name, 'responses_byok');
        assert.equal(url.pathname, '/v1/responses');
        assert.equal(request.method, 'POST');
        assert.equal(request.headers.get('authorization'), 'Bearer synthetic-owner-responses-search-key');
        const body = await request.json();
        assert.equal(body.model, 'owned-search-model');
        assert.equal(body.tool_choice, 'required');
        assert.equal(body.max_tool_calls, 1);
        assert.equal(body.stream, false);
        assert.equal(body.store, false);
        assert.equal(body.tools[0].type, 'web_search');
        requests.push({ name: active.name, path: url.pathname, body });
        const db = await mf.getD1Database('DB');
        assert.equal((await db.prepare("SELECT COUNT(*) AS n FROM usage_reservations WHERE account_id=? AND idempotency_key LIKE 'tool:reported_cost:%' AND state='reserved'")
          .bind(active.owner).first()).n, 1, 'Responses BYOK also needs its independently bound hold before dispatch');
        return Response.json({ id: 'private-search-response', object: 'response', status: 'completed',
          output: [{ id: 'private-search-call', type: 'web_search_call', status: 'completed',
            action: { type: 'search', query: 'provider refined query', sources: [{ type: 'url', url: source }] } },
          { id: 'private-search-message', type: 'message', role: 'assistant', status: 'completed',
            content: [{ type: 'output_text', text: 'Different provider text.', annotations: [
              { type: 'url_citation', start_index: 0, end_index: 9, title: 'Ownership', url: source }] }] }],
          usage: { input_tokens: 91, output_tokens: 17, total_tokens: 108, cost: 1.234 } });
      }
      if (url.origin !== 'https://api.deepseek.com') {
        const response = await providers.handle(request);
        assert.notEqual(response, undefined, 'Controlled search never contacts the public Internet');
        return response;
      }
      assert.ok(active);
      assert.equal(request.method, 'POST');
      assert.equal(request.headers.get('authorization'), 'Bearer ' + (active.name === 'byok'
        ? 'synthetic-owner-search-key' : 'synthetic-platform-search-key'));
      assert.equal(request.headers.get('cookie'), null);
      assert.ok(request.headers.get('idempotency-key'));
      const body = await request.json();
      requests.push({ name: active.name, path: url.pathname, body });
      const db = await mf.getD1Database('DB');
      assert.equal((await db.prepare("SELECT COUNT(*) AS n FROM usage_reservations WHERE account_id=? AND idempotency_key LIKE 'tool:deepseek_request:%' AND state='reserved'")
        .bind(active.owner).first()).n, 1, 'Independent fee hold must precede each provider HTTP request');
      if (active.name === 'unknown') return new Response('{"results":', {
        headers: { 'Content-Type': 'application/json' },
      });
      if (active.name === 'http_error') return Response.json({ error: { message: 'Synthetic QPS limit', code: 'rate_limit_exceeded' } }, { status: 429 });
      if (url.pathname === '/v1/tools/fetch') {
        assert.deepEqual(body, { url: source });
        return Response.json({ title: 'Ownership', content: '# Ownership\nOwnership is checked by Rust.' });
      }
      assert.equal(url.pathname, '/v1/tools/search');
      assert.deepEqual(body, { query: 'Rust ownership', count: 10, need_content: false });
      return Response.json({ results: active.name === 'empty' ? [] : [{ rerank_score: 2.4,
        title: 'Ownership', url: source, snippet: 'Rust ownership', summary: 'Ownership is checked by Rust.',
        content: '', site_name: 'Rust', site_icon: null, date: null, authority: 3 }] });
    } }));
  t.after(() => mf.dispose());
  const db = await mf.getD1Database('DB'); await migrate(db);
  async function request(method, endpoint, body, expected = 200, bearer) {
    const response = await mf.dispatchFetch(origin + endpoint, { method,
      headers: { Authorization: `Bearer ${bearer}`, 'Content-Type': 'application/json', 'CF-Connecting-IP': '192.0.2.75' },
      ...(body === undefined ? {} : { body: JSON.stringify(body) }) });
    const text = await response.text();
    assert.equal(response.status, expected, `${method} ${endpoint}: ${text}`);
    return text ? JSON.parse(text) : null;
  }
  const receipts = {};
  for (const name of selected) await t.test(name, { timeout: 25000 }, async () => {
    const principal = await createBrowserPrincipal({ mf, db, encryptionKey, owner: 'hosted-search-' + name });
    active = { name, owner: principal.owner };
    const actions = [{ type: 'search', query: 'Rust ownership' }];
    if (name === 'platform') actions.push({ type: 'open_page', url: source }, { type: 'find_in_page', url: source, pattern: 'Ownership' });
    const model = providers.register('hosted-search-' + name, [...actions.map((action, index) => ({
      usage, output: [call('search-' + index, action)],
      ...(index ? { check(body) { assert.ok(JSON.stringify(body.input).includes(source), 'Real sources must reach the outer harness continuation'); } } : {}),
    })), { usage, output: [final], check(body) {
      const wire = JSON.stringify(body.input);
      assert.ok(wire.includes(name === 'unknown' ? 'search_provider_outcome_unknown' : name === 'http_error'
        ? 'search_provider_http_error' : name === 'empty' ? 'completed' : source));
      assert.ok(!wire.includes('synthetic-platform-search-key') && !wire.includes('synthetic-owner-search-key'));
    } }]);
    const credential = await principal.browser('POST', '/v1/provider-keys', { protocol: 'openai', base_url: model.baseUrl, api_key: model.key }, 201);
    const searchKey = ['byok', 'responses_byok'].includes(name) ? await principal.browser('POST', '/v1/provider-keys',
      { protocol: 'openai', base_url: name === 'byok' ? 'https://api.deepseek.com/v1' : 'https://owned-search-service.example.com/v1',
        api_key: name === 'byok' ? 'synthetic-owner-search-key' : 'synthetic-owner-responses-search-key' }, 201) : null;
    const service = name === 'responses_byok' ? { provider: 'openai_responses', credential_id: searchKey.id,
      model: 'owned-search-model', max_tool_calls: 1 } : searchKey ? { provider: 'deepseek_search', credential_id: searchKey.id }
      : { provider: 'platform_deepseek' };
    const agent = await request('POST', '/v1/agents', { name: 'Independent search ' + name, workspace: false, tools: [],
      model: { provider: 'byok', credential_id: credential.id, protocol: 'responses', model: model.model, context_window: 32768, max_output_tokens: 512 },
      tool_services: { web_search: service },
    }, 201, principal.apiKey);
    const client = new OpenAI({ apiKey: principal.apiKey, baseURL: `${origin}/v1`, maxRetries: 0,
      fetch: (input, init) => mf.dispatchFetch(input, init) });
    const input = { model: agent.id, input: 'Use the declared search service.', tools: [{ type: 'web_search' }],
      include: ['web_search_call.action.sources'] };
    const stream = await client.responses.create({ ...input, stream: true }, { headers: { 'Idempotency-Key': 'search-' + name } });
    const events = [];
    for await (const event of stream) events.push(event);
    if (model.failure) throw model.failure;
    const response = events.findLast(event => event.type === 'response.completed')?.response;
    assert.ok(response, JSON.stringify(events.at(-1)));
    const items = response.output.filter(item => item.type === 'web_search_call');
    assert.equal(items.length, actions.length);
    assert.equal(items[0].status, ['unknown', 'http_error'].includes(name) ? 'failed' : 'completed');
    assert.equal(items[0].action.query, name === 'responses_byok' ? 'provider refined query' : 'Rust ownership');
    if (['platform', 'byok', 'responses_byok'].includes(name)) assert.deepEqual(items[0].action.sources, [{ type: 'url', url: source }]);
    if (name === 'platform') {
      assert.deepEqual(items[1].action, { type: 'open_page', url: source });
      assert.deepEqual(items[2].action, { type: 'find_in_page', url: source, pattern: 'Ownership' });
    }
    assert.equal(events.filter(event => event.type === 'response.web_search_call.completed').length, actions.length);
    assert.equal(events.filter(event => event.type === 'response.web_search_call.in_progress').length, actions.length);
    assert.equal(events.filter(event => event.type === 'response.web_search_call.searching').length, actions.length);
    for (const item of items) {
      const phases = events.filter(event => event.item_id === item.id).map(event => event.type);
      assert.deepEqual(phases, ['response.web_search_call.in_progress', 'response.web_search_call.searching', 'response.web_search_call.completed']);
    }
    assert.ok(!JSON.stringify(response).includes('__agents_web_search'), 'Private harness aliases cannot escape canonical output');
    const schema = validateWire(response, events);
    assert.equal(response.usage, null, 'Missing provider breakdown is not invented for standard SDK usage');
    assert.equal(response.agents_usage.input_tokens, 7 * (actions.length + 1), 'Search units are not fabricated language-model tokens');
    assert.equal(response.agents_usage.output_tokens, 2 * (actions.length + 1));
    assert.equal(response.agents_usage.input_tokens_details.cache_write_tokens, null);
    assert.equal(response.agents_usage.output_tokens_details.reasoning_tokens, null);
    assert.equal(response.output.find(item => item.type === 'message').content[0].text, answer);
    assert.deepEqual(response.output.find(item => item.type === 'message').content[0].annotations, [], 'Dedicated search cannot copy invented answer-span offsets');
    const replay = await client.responses.create({ ...input, stream: true }, { headers: { 'Idempotency-Key': 'search-' + name } });
    const replayEvents = [];
    for await (const event of replay) replayEvents.push(event);
    assert.deepEqual(replayEvents, events, 'SSE replay uses canonical events, not another operation');
    const retrieved = await client.responses.retrieve(response.id, { include: input.include });
    assert.deepEqual(retrieved.output, response.output, 'JSON retrieval agrees with canonical SSE');
    assert.equal(requests.filter(row => row.name === name).length, actions.length);
    assert.equal(model.calls, actions.length + 1);
    const rows = (await db.prepare("SELECT CAST(l.quantity AS TEXT) AS quantity,CAST(l.amount_microusd AS TEXT) AS amount FROM usage_ledger l JOIN usage_reservations r ON r.id=l.reservation_id WHERE r.account_id=? AND l.unit=?")
      .bind(principal.owner, name === 'responses_byok' ? 'provider_request' : 'successful_search_request').all()).results;
    assert.equal(rows.length, name === 'unknown' ? 0 : actions.length);
    const empty = ['empty', 'http_error'].includes(name);
    const amount = empty || searchKey ? '0' : '750';
    for (const row of rows) assert.deepEqual(row, { quantity: empty ? '0' : '1', amount });
    const unresolved = await db.prepare("SELECT COUNT(*) AS n FROM usage_reservations WHERE account_id=? AND idempotency_key LIKE 'tool:deepseek_request:%' AND state='reserved'")
      .bind(principal.owner).first();
    assert.equal(unresolved.n, name === 'unknown' ? 1 : 0, 'Unknown dispatch preserves its hold; known receipts settle once');
    receipts[name] = { response_id: response.id, provider_posts: actions.length, model_calls: model.calls,
      search_units: name === 'unknown' ? null : empty ? 0 : actions.length,
      unresolved_service_holds: unresolved.n, strict_sdk: schema.sdk ?? null };
  });
  assert.deepEqual(Object.keys(receipts).sort(), selected.sort());
  await mkdir(path.join(root, '.cache/v0.3-ci'), { recursive: true });
  await writeFile(path.join(root, '.cache/v0.3-ci/hosted-search.json'), JSON.stringify({ source: process.env.MAGENTS_TEST_EXPECTED_BUILD ?? process.env.GITHUB_SHA ?? null,
    wasm_sha256: createHash('sha256').update(await readFile(path.join(build, 'index_bg.wasm'))).digest('hex'),
    fixture: 'actual-compiled-rust-worker', executed_case_ids: selected, external_network_calls: 0, ...receipts }, null, 2));
});
