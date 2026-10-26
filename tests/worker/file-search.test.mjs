import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { createHash, randomBytes } from 'node:crypto';
import { mkdir, mkdtemp, readFile, readdir, writeFile } from 'node:fs/promises';
import path from 'node:path';
import test from 'node:test';
import { fileURLToPath } from 'node:url';
import OpenAI, { toFile } from 'openai';
import { Miniflare, convertV4MiniflareOptions } from 'miniflare';
import { createBrowserPrincipal } from './browser-principal.mjs';
import { createIdentityFixture } from './auth-journey.mjs';
import { dispatchSDKFetch } from './sdk-miniflare-fetch.mjs';
import { createNativeSemanticsProvider } from './native-semantics-journey.mjs';

const root = fileURLToPath(new URL('../../', import.meta.url));
const build = path.resolve(root, process.env.WORKER_BUILD_DIR ?? 'crates/agents-worker/build');
async function migrate(db) {
  for (const name of (await readdir(path.join(root, 'migrations'))).filter(name => name.endsWith('.sql')).sort()) {
    const sql = await readFile(path.join(root, 'migrations', name), 'utf8');
    const parsed = spawnSync(process.env.PYTHON ?? 'python', ['-c',
      'import sys,sqlite3,json\ns: str=""; out: list[str]=[]\nfor line in sys.stdin:\n s+=line\n if sqlite3.complete_statement(s): out.append(s); s=""\nassert not s.strip(), s\nprint(json.dumps(out))'], { input: sql, encoding: 'utf8' });
    assert.equal(parsed.status, 0, `${name}: ${parsed.stderr}`);
    await db.batch(JSON.parse(parsed.stdout).map(statement => db.prepare(statement)));
  }
}

function retrievalFixture() {
  const vectors = new Map();
  const state = { embedCalls: 0, upsertCalls: 0, queryCalls: 0, visible: false, deleted: 0 };
  const accepts = (value, filter) => Object.entries(filter).every(([key, condition]) =>
    ('$eq' in condition ? value[key] === condition.$eq : condition.$in.includes(value[key])));
  return { state, async handle(request) {
    if (new URL(request.url).origin !== 'https://retrieval.fixture.test') return undefined;
    const args = await request.json();
    switch (new URL(request.url).pathname) {
      case '/embed':
        assert.equal(args[0], '@cf/baai/bge-m3');
        assert.equal(args[1].truncate_inputs, false);
        state.embedCalls++;
        return Response.json({ shape: [args[1].text.length, 2], data: args[1].text.map(() => [1, 0]) });
      case '/upsert':
        state.upsertCalls++;
        for (const record of args[0]) vectors.set(record.id, record);
        return Response.json({ mutationId: `fixture-mutation-${state.upsertCalls}` });
      case '/query': {
        state.queryCalls++;
        assert.deepEqual(args[0], [1, 0]);
        assert.equal(args[1].namespace, 'retrieval-fixture');
        assert.ok(args[1].filter.owner_hash.$eq);
        const matches = state.visible ? [...vectors.values()].filter(record =>
          record.namespace === args[1].namespace && accepts(record.metadata, args[1].filter)) : [];
        return Response.json({ matches: matches.slice(0, args[1].topK).map(record => ({ id: record.id, score: 0.9 })) });
      }
      case '/getByIds': return Response.json(args[0].map(id => vectors.get(id)).filter(Boolean));
      case '/deleteByIds':
        for (const id of args[0]) { if (vectors.delete(id)) state.deleted++; }
        return Response.json({ mutationId: `fixture-delete-${state.deleted}` });
      case '/describe': return Response.json({ dimensions: 2, vectorCount: vectors.size });
      default: throw Error('unexpected_controlled_retrieval_method');
    }
  } };
}

test('compiled Rust Worker: owned file upload indexing query citation and deletion', { timeout: 60000 }, async t => {
  assert.ok(process.env.MAGENTS_CURRENT_SCHEMA_PYTHON, 'Use the installed current strict SDK for original wire data');
  const encryptionKey = randomBytes(32);
  const identity = createIdentityFixture();
  const providers = createNativeSemanticsProvider();
  const retrieval = retrievalFixture();
  const deployment = JSON.parse(await readFile(path.join(root, 'wrangler.jsonc'), 'utf8'));
  const pricing = JSON.parse(deployment.env.staging.vars.RESOURCE_PRICING_JSON);
  for (const [unit, denominator] of [['embedding_input_bytes', 64], ['queried_dimensions', 100], ['written_dimensions', 100], ['allocated_vector_dimension_ms', 1000]]) {
    pricing.tariffs.push({ resource: 'worker', unit, microusd_numerator: 1, quantity_denominator: denominator, price_version: pricing.version });
  }
  const profile = { embedding: { model: '@cf/baai/bge-m3', revision: 'controlled-bge-transport-v1', dimensions: 2, input_byte_limit: 32768 },
    ai_binding: 'AI', vector_binding: 'VECTORIZE', namespace: 'retrieval-fixture',
    embedding_allocation: { resource: 'worker', unit: 'embedding_input_bytes' },
    query_allocation: { resource: 'worker', unit: 'queried_dimensions' },
    upsert_allocation: { resource: 'worker', unit: 'written_dimensions' } };
  await mkdir(path.join(root, '.temp'), { recursive: true });
  const directory = await mkdtemp(path.join(root, '.temp/retrieval-worker-'));
  const mf = new Miniflare(convertV4MiniflareOptions({
    modulesRoot: build,
    modules: [
      { type: 'ESModule', path: path.join(build, 'worker/shim.mjs'), contents: await readFile(new URL('./file-search-shim.mjs', import.meta.url), 'utf8') },
      { type: 'ESModule', path: path.join(build, 'index.js') },
      { type: 'CompiledWasm', path: path.join(build, 'index_bg.wasm') },
    ],
    compatibilityDate: '2026-10-23', compatibilityFlags: ['enable_request_signal'],
    resourcePersistencePath: path.join(directory, 'storage'),
    d1Databases: ['DB'], r2Buckets: ['WORKSPACES', 'CONVERSATIONS', 'DIAGNOSTICS'],
    durableObjects: { SESSIONS: { className: 'Session', useSQLite: true }, AUTH_SESSIONS: { className: 'AuthSession', useSQLite: true },
      CONVERSATION_STATE: { className: 'ConversationState', useSQLite: true }, MODEL_OPERATIONS: { className: 'ModelOperation', useSQLite: true } },
    ratelimits: Object.fromEntries(['INGRESS', 'ACCOUNT', 'MUTATION'].map((name, index) => [`${name}_RATE_LIMITER`,
      { namespace_id: String(index + 1), simple: { limit: 1000, period: 60 } }])),
    bindings: { ENVIRONMENT: 'test', APP_ORIGIN: 'https://agents.test', ALLOWED_ORIGIN: 'https://agents.test',
      DATA_ENCRYPTION_KEY: encryptionKey.toString('base64url'), IDENTITY_ISSUER: 'https://identity.test', IDENTITY_WEB_CLIENT_ID: 'test-dashboard',
      IDENTITY_NATIVE_CLIENT_ID: 'test-native', IDENTITY_REDIRECT_URI: 'https://agents.test/auth/callback', IDENTITY_NATIVE_REDIRECT_URI: 'http://127.0.0.1/callback',
      IDENTITY_CLIENT_PRIVATE_JWK: JSON.stringify(identity.privateJwk), BILLING_PRODUCT_ID: 'agents', BILLING_MODE: 'test',
      MODEL_REGISTRY_JSON: '[]', MODEL_PRICING_JSON: '{}', RESOURCE_PRICING_JSON: JSON.stringify(pricing),
      FILE_SEARCH_PROFILE_JSON: JSON.stringify(profile), HOSTED_TOOL_RUNTIMES: 'file_search',
      HOSTED_TOOL_CAPABILITIES_JSON: JSON.stringify({ version: 1, platform_images: false, platform_search: false, file_search: true,
        container_memory: null, available_container_profiles: [], code_network_disabled: false, code_network_allowlist: false }),
      RUN_RESOURCE_RESERVATION_MICROUSD: '20000', FREE_GRANT_BUDGET_MICROUSD_PER_DAY: '100000000' },
    async outboundService(request) {
      const response = await retrieval.handle(request) ?? await providers.handle(request);
      assert.notEqual(response, undefined, `Controlled retrieval fixtures must not access ${new URL(request.url).origin}`);
      return response;
    },
  }));
  t.after(() => mf.dispose());
  const db = await mf.getD1Database('DB');
  await migrate(db);
  const principal = await createBrowserPrincipal({ mf, db, encryptionKey, owner: 'retrieval-owned' });
  const other = await createBrowserPrincipal({ mf, db, encryptionKey, owner: 'retrieval-other' });
  const parseEvents = text => text.split('\n').filter(line => line.startsWith('data:'))
    .map(line => line.slice(5).trim()).filter(data => data && data !== '[DONE]').map(data => JSON.parse(data));
  let originalEvents;
  const clientFor = key => new OpenAI({ apiKey: key, baseURL: 'https://agents.test/v1', maxRetries: 0,
    async fetch(input, init) {
      const response = await dispatchSDKFetch(mf, input, init);
      if (new URL(input).pathname === '/v1/responses' && init?.method === 'POST' && JSON.parse(init.body).stream) {
        originalEvents = response.clone().text().then(parseEvents);
      }
      return response;
    } });
  const sdk = clientFor(principal.apiKey);
  const source = await sdk.files.create({ purpose: 'assistants', file: await toFile(Buffer.from('检索示例 🦀\nThe recovery code is violet-orbit-742.\n'), 'owned-notes.txt', { type: 'text/plain' }) });
  const store = await sdk.vectorStores.create({});
  assert.equal(typeof store.name, 'string');
  assert.ok(store.name.length > 0);
  await sdk.vectorStores.files.create(store.id, { file_id: source.id, attributes: { kind: 'owned' }, chunking_strategy: { type: 'static', static: { max_chunk_size_tokens: 200, chunk_overlap_tokens: 50 } } });
  const until = Date.now() + 12000;
  while (retrieval.state.upsertCalls === 0 && Date.now() < until) await new Promise(resolve => setTimeout(resolve, 20));
  assert.equal(retrieval.state.upsertCalls, 1, 'A real background turn must perform the first upsert');
  await new Promise(resolve => setTimeout(resolve, 2100));
  assert.equal((await sdk.vectorStores.files.retrieve(source.id, { vector_store_id: store.id })).status, 'in_progress');
  await new Promise(resolve => setTimeout(resolve, 100));
  assert.equal(retrieval.state.embedCalls, 1, 'Visibility retry must not re-embed the same generation');
  assert.equal(retrieval.state.upsertCalls, 1, 'Known upsert receipt must survive a visibility retry');
  retrieval.state.visible = true;
  let membership;
  const published = Date.now() + 12000;
  do {
    await new Promise(resolve => setTimeout(resolve, 2100));
    membership = await sdk.vectorStores.files.retrieve(source.id, { vector_store_id: store.id });
  } while (membership.status === 'in_progress' && Date.now() < published);
  assert.equal(membership.status, 'completed', JSON.stringify({ services: retrieval.state,
    job: await db.prepare('SELECT status,lease_id,lease_until,retry_at,source_staged,chunk_count,last_error_json FROM vector_store_files WHERE vector_store_id=?').bind(store.id).all(),
    chunks: await db.prepare('SELECT vector_status,vector_mutation_id FROM file_search_chunks WHERE vector_store_id=?').bind(store.id).all(),
    observations: await db.prepare("SELECT status,unit,quantity FROM resource_observations WHERE account_id=? AND status='pending'").bind(principal.owner).all() }));
  const result = await sdk.vectorStores.search(store.id, { query: 'What is the recovery code?', filters: { type: 'eq', key: 'kind', value: 'owned' } });
  assert.ok(result.data.some(hit => hit.file_id === source.id && hit.content.some(part => part.text.includes('violet-orbit-742'))));
  await assert.rejects(clientFor(other.apiKey).vectorStores.retrieve(store.id), error => error.status === 404);
  await assert.rejects(clientFor(other.apiKey).vectorStores.files.create(store.id, { file_id: source.id }), error => error.status === 404);
  const state = providers.register('retrieval', [{ output: [{ type: 'function_call', id: 'hidden-call', call_id: 'search-call', name: '__agents_file_search', arguments: '{"queries":["recovery code"]}', status: 'completed' }] },
    { check(body) { assert.ok(JSON.stringify(body).includes('violet-orbit-742')); }, output: [{ id: 'answer', type: 'message', role: 'assistant', status: 'completed', content: [{ type: 'output_text', text: `雪🦀 answer ⟦file:${source.id}⟧`, annotations: [], logprobs: [] }] }] }]);
  const credential = await principal.browser('POST', '/v1/provider-keys', { protocol: 'openai', base_url: state.baseUrl, api_key: state.key }, 201);
  const agent = await principal.browser('POST', '/v1/agents', { name: 'Retrieval agent', instructions: 'Use owned evidence.',
    model: { provider: 'byok', protocol: 'responses', credential_id: credential.id, model: state.model, context_window: 32768, max_output_tokens: 512 }, workspace: false, tools: [], skills: [], compaction_percent: 80 }, 201);
  const events = [];
  for await (const event of await sdk.responses.create({ model: agent.id, input: 'Find the recovery code.', stream: true,
    tools: [{ type: 'file_search', vector_store_ids: [store.id] }], include: ['file_search_call.results'] }, { headers: { 'Idempotency-Key': 'retrieval-one-execution' } })) events.push(event);
  assert.equal(state.calls, 2);
  const final = events.at(-1).response;
  assert.equal(final.status, 'completed');
  assert.ok(final.output.some(item => item.type === 'file_search_call' && item.status === 'completed' && item.results.some(hit => hit.file_id === source.id)));
  const message = final.output.find(item => item.type === 'message');
  assert.ok(message.content[0].annotations.some(annotation => annotation.type === 'file_citation' && annotation.file_id === source.id && annotation.index === 10));
  assert.ok(events.some(event => event.type === 'response.output_text.annotation.added'));
  const rawEvents = await originalEvents;
  const rawResponse = await (await sdk.responses.retrieve(final.id, { include: ['file_search_call.results'] }).asResponse()).json();
  const checked = spawnSync(process.env.MAGENTS_CURRENT_SCHEMA_PYTHON, ['tests/e2e/current_responses_sdk.py', 'response-wire', '--input', '-'],
    { cwd: root, input: JSON.stringify({ response: rawResponse, events: rawEvents }), encoding: 'utf8', maxBuffer: 1024 * 1024 });
  assert.equal(checked.status, 0, checked.stdout || checked.stderr);
  const replay = parseEvents(await (await sdk.responses.retrieve(final.id, { stream: true, include: ['file_search_call.results'] }).asResponse()).text());
  assert.deepEqual(replay, rawEvents);
  const invalid = await sdk.files.create({ purpose: 'assistants', file: await toFile(Buffer.from([0xff, 0]), 'unsupported.bin', { type: 'application/octet-stream' }) });
  await sdk.vectorStores.files.create(store.id, { file_id: invalid.id });
  let failed;
  const failedDeadline = Date.now() + 6000;
  do {
    await new Promise(resolve => setTimeout(resolve, 50));
    failed = await sdk.vectorStores.files.retrieve(invalid.id, { vector_store_id: store.id });
  } while (failed.status === 'in_progress' && Date.now() < failedDeadline);
  assert.equal(failed.status, 'failed');
  assert.equal(failed.last_error.code, 'unsupported_file');
  await sdk.files.delete(invalid.id);
  await sdk.files.delete(source.id);
  assert.deepEqual((await sdk.vectorStores.search(store.id, { query: 'recovery code' })).data, []);
  await sdk.vectorStores.delete(store.id);
  // The live Session deliberately keeps its separately funded cleanup hold.
  // Only the completed model and retrieval operations must be fully settled.
  assert.equal((await db.prepare("SELECT COUNT(*) AS n FROM usage_reservations WHERE account_id=? AND state='reserved' AND (id=? OR id LIKE 'file-search:%' OR id LIKE 'file-ingest:%' OR id LIKE 'file-cleanup:%')").bind(principal.owner, final.id).first()).n, 0);
  await writeFile(path.join(directory, 'receipt.json'), JSON.stringify({ fixture: 'actual-rust-worker-controlled-ai-vectorize',
    source: process.env.MAGENTS_TEST_EXPECTED_BUILD ?? process.env.GITHUB_SHA ?? null,
    wasm_sha256: createHash('sha256').update(await readFile(path.join(build, 'index_bg.wasm'))).digest('hex'),
    real_cloudflare_embeddings: false, paid_llm_calls: 0, external_network_calls: 0, native_model_calls: state.calls,
    indexing_embeddings: 1, query_embeddings: retrieval.state.embedCalls - 1, upserts: retrieval.state.upsertCalls,
    checks: ['owned_upload_attach', 'background_index', 'visibility_without_reembedding', 'semantic_transport_and_lexical_fusion', 'cross_owner_rejected',
      'native_file_search_sse', 'actual_text_file_citation', 'strict_current_sdk_original_wire', 'bounded_format_failure', 'exact_replay', 'source_delete_invalidates', 'terminal_operation_holds_settled'] }, null, 2));
});
