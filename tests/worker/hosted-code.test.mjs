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
const hash = bytes => createHash('sha256').update(bytes).digest('hex');
const contextId = `cntr_${'a'.repeat(32)}`;
const artifactId = `pyart_${'b'.repeat(64)}`;
const source = "retained_value = 42\nprint(retained_value)\n";
const artifact = Buffer.from('value\n42\n');

async function migrate(db) {
  for (const name of (await readdir(path.join(root, 'migrations'))).filter(name => name.endsWith('.sql')).sort()) {
    const parsed = spawnSync(process.env.PYTHON ?? 'python', ['-c',
      'import sys,sqlite3,json\ns: str=""; out: list[str]=[]\nfor line in sys.stdin:\n s+=line\n if sqlite3.complete_statement(s): out.append(s); s=""\nassert not s.strip(), s\nprint(json.dumps(out))'],
    { input: await readFile(path.join(root, 'migrations', name), 'utf8'), encoding: 'utf8' });
    assert.equal(parsed.status, 0, name);
    await db.batch(JSON.parse(parsed.stdout).map(statement => db.prepare(statement)));
  }
}

test('compiled Rust Worker: hosted Code Interpreter auto context publication and exact retry', { timeout: 90000 }, async t => {
  const encryptionKey = randomBytes(32), identity = createIdentityFixture(), providers = createNativeSemanticsProvider();
  const deployment = JSON.parse(await readFile(path.join(root, 'wrangler.jsonc'), 'utf8'));
  await mkdir(path.join(root, '.temp'), { recursive: true });
  const directory = await mkdtemp(path.join(root, '.temp/hosted-code-'));
  process.env.TMP = process.env.TEMP = process.env.TMPDIR = directory;
  const results = new Map();
  const order = [];
  let posts = 0, preparations = 0, acknowledgements = 0;
  const mf = new Miniflare(convertV4MiniflareOptions({ modulesRoot: build,
    modules: [{ type: 'ESModule', path: path.join(build, 'worker/shim.mjs'),
      contents: await readFile(new URL('./hosted-code-shim.mjs', import.meta.url), 'utf8') },
    { type: 'ESModule', path: path.join(build, 'index.js') }, { type: 'CompiledWasm', path: path.join(build, 'index_bg.wasm') }],
    compatibilityDate: '2026-10-23', compatibilityFlags: ['enable_request_signal'],
    resourcePersistencePath: path.join(directory, 'storage'), d1Databases: ['DB'], r2Buckets: ['WORKSPACES', 'CONVERSATIONS', 'DIAGNOSTICS'],
    durableObjects: { SESSIONS: { className: 'Session', useSQLite: true }, AUTH_SESSIONS: { className: 'AuthSession', useSQLite: true },
      CONVERSATION_STATE: { className: 'ConversationState', useSQLite: true }, MODEL_OPERATIONS: { className: 'ModelOperation', useSQLite: true } },
    ratelimits: Object.fromEntries(['INGRESS', 'ACCOUNT', 'MUTATION'].map((name, index) => [`${name}_RATE_LIMITER`, { namespace_id: String(index + 1), simple: { limit: 1000, period: 60 } }])),
    bindings: { ENVIRONMENT: 'test', APP_ORIGIN: 'https://agents.test', ALLOWED_ORIGIN: 'https://agents.test',
      DATA_ENCRYPTION_KEY: encryptionKey.toString('base64url'), IDENTITY_ISSUER: 'https://identity.test',
      IDENTITY_WEB_CLIENT_ID: 'test-dashboard', IDENTITY_NATIVE_CLIENT_ID: 'test-native', IDENTITY_REDIRECT_URI: 'https://agents.test/auth/callback',
      IDENTITY_NATIVE_REDIRECT_URI: 'http://127.0.0.1/callback', IDENTITY_CLIENT_PRIVATE_JWK: JSON.stringify(identity.privateJwk),
      BILLING_PRODUCT_ID: 'agents', BILLING_MODE: 'test', MODEL_REGISTRY_JSON: '[]', MODEL_PRICING_JSON: '{}',
      HOSTED_TOOL_RUNTIMES: 'code_interpreter', HOSTED_TOOL_CAPABILITIES_JSON: JSON.stringify({ version: 1,
        platform_images: false, platform_search: false, file_search: false, container_memory: null,
        available_container_profiles: ['1g'], code_network_disabled: true, code_network_allowlist: false }),
      RESOURCE_PRICING_JSON: deployment.env.staging.vars.RESOURCE_PRICING_JSON,
      RUN_RESOURCE_RESERVATION_MICROUSD: '20000', FREE_GRANT_BUDGET_MICROUSD_PER_DAY: '100000000' },
    async outboundService(request) {
      const url = new URL(request.url);
      if (url.hostname !== 'code-supervisor.fixture') {
        const response = await providers.handle(request);
        assert.notEqual(response, undefined, `Unexpected fixture origin ${url.origin}`);
        return response;
      }
      assert.ok(request.headers.get('authorization')?.startsWith('Bearer '));
      order.push(`${request.method} ${url.pathname}`);
      if (url.pathname === '/health') return Response.json({ boot_id: 'supervisor-known-boot' });
      if (url.pathname === '/v1/python/contexts') {
        const body = await request.json();
        assert.equal(body.memory_limit, '1g');
        assert.deepEqual(body.network_policy, { type: 'disabled' });
        assert.equal(body.container_id, null);
        preparations++;
        return Response.json({ container_id: contextId });
      }
      if (url.pathname === '/v1/python' && request.method === 'POST') {
        const body = await request.json();
        assert.equal(body.code, source);
        assert.equal(body.container_id, contextId);
        assert.equal(body.memory_limit, '1g');
        assert.ok(!results.has(body.id), 'No accepted cell may be POSTed twice');
        posts++;
        const result = { id: body.id, container_id: contextId, outputs: [{ type: 'logs', logs: '42\n' }],
          artifacts: [{ id: artifactId, filename: 'result.csv', path: 'result.csv', bytes: artifact.length,
            sha256: hash(artifact), mime_type: 'text/csv' }], artifact_error: null, error: null,
          python_error: false, timed_out: false, cancelled: false, truncated: false, duration_ms: 5 };
        results.set(body.id, result);
        return Response.json(result);
      }
      if (url.pathname === `/v1/python/artifacts/${artifactId}`) {
        if (request.method === 'DELETE') {
          acknowledgements++;
          return new Response(null, { status: 204 });
        }
        return new Response(artifact, { headers: { 'Content-Type': 'text/csv',
          'Content-Length': String(artifact.length), 'X-Content-Sha256': hash(artifact) } });
      }
      if (url.pathname.startsWith('/v1/python/')) {
        const result = results.get(url.pathname.split('/').at(-1));
        return result ? Response.json(result) : Response.json({ error: 'not found' }, { status: 404 });
      }
      throw Error(`Unexpected supervisor request ${request.method} ${url.pathname}`);
    } }));
  t.after(() => mf.dispose());
  const db = await mf.getD1Database('DB'); await migrate(db);
  const principal = await createBrowserPrincipal({ mf, db, encryptionKey, owner: 'hosted-code-auto' });
  const model = providers.register('hosted-code-auto', [
    { output: [{ id: 'native-code', type: 'function_call', name: '__agents_code_interpreter', call_id: 'code-call',
      arguments: JSON.stringify({ code: source }), status: 'completed' }] },
    { output: [{ id: 'native-answer', type: 'message', role: 'assistant', status: 'completed',
      content: [{ type: 'output_text', text: '42', annotations: [], logprobs: [] }] }] },
  ]);
  const key = await principal.browser('POST', '/v1/provider-keys', { protocol: 'openai', base_url: model.baseUrl, api_key: model.key }, 201);
  const client = new OpenAI({ apiKey: principal.apiKey, baseURL: 'https://agents.test/v1', maxRetries: 0,
    fetch: (input, init) => mf.dispatchFetch(input, init) });
  const agentResponse = await mf.dispatchFetch('https://agents.test/v1/agents', { method: 'POST',
    headers: { Authorization: `Bearer ${principal.apiKey}`, 'Content-Type': 'application/json' },
    body: JSON.stringify({ name: 'Code auto publication', workspace: false, tools: [],
      model: { provider: 'byok', credential_id: key.id, protocol: 'responses', model: model.model, context_window: 32768, max_output_tokens: 512 } }) });
  assert.equal(agentResponse.status, 201);
  const agent = await agentResponse.json();
  const input = { model: agent.id, input: 'Compute and publish CSV.', tools: [{ type: 'code_interpreter', container: { type: 'auto' } }] };
  const events = [];
  for await (const event of await client.responses.create({ ...input, stream: true }, { headers: { 'Idempotency-Key': 'known-code-auto' } })) events.push(event);
  const response = events.find(event => event.type === 'response.completed')?.response;
  assert.ok(response, 'Code must reach a genuine completed response');
  const item = response.output.find(item => item.type === 'code_interpreter_call');
  assert.ok(item, 'A completed model reply is not evidence that hosted Python executed');
  assert.equal(item.container_id, contextId);
  assert.equal(item.code, source);
  assert.deepEqual(item.outputs, [{ type: 'logs', logs: '42\n' }]);
  const phases = events.filter(event => event.item_id === item.id).map(event => event.type);
  assert.deepEqual(phases, ['response.code_interpreter_call.in_progress', 'response.code_interpreter_call_code.delta',
    'response.code_interpreter_call_code.done', 'response.code_interpreter_call.interpreting', 'response.code_interpreter_call.completed']);
  assert.equal(events.find(event => event.type === 'response.code_interpreter_call_code.done').code, source);
  assert.equal(model.calls, 2); assert.equal(posts, 1); assert.equal(preparations, 1); assert.equal(acknowledgements, 1);
  const files = await client.files.list();
  const file = files.data.find(file => file.filename === 'result.csv');
  assert.ok(file);
  assert.equal(hash(Buffer.from(await (await client.files.content(file.id)).arrayBuffer())), hash(artifact));
  const replayEvents = [];
  for await (const event of await client.responses.create({ ...input, stream: true }, { headers: { 'Idempotency-Key': 'known-code-auto' } })) replayEvents.push(event);
  assert.equal(replayEvents.find(event => event.type === 'response.completed')?.response.id, response.id);
  assert.deepEqual(replayEvents, events);
  assert.equal(posts, 1); assert.equal(preparations, 1); assert.equal(acknowledgements, 1); assert.equal(model.calls, 2);
  const getAt = order.indexOf(`GET /v1/python/artifacts/${artifactId}`), ackAt = order.indexOf(`DELETE /v1/python/artifacts/${artifactId}`);
  assert.ok(getAt >= 0 && ackAt > getAt);
  if (process.env.MAGENTS_CURRENT_SCHEMA_PYTHON) {
    const checked = spawnSync(process.env.MAGENTS_CURRENT_SCHEMA_PYTHON, ['tests/e2e/current_responses_sdk.py', 'response-wire', '--input', '-'],
      { cwd: root, input: JSON.stringify({ response, events }), encoding: 'utf8', maxBuffer: 1024 * 1024 });
    assert.equal(checked.status, 0, checked.stdout || checked.stderr);
  }
  await writeFile(path.join(directory, 'receipt.json'), JSON.stringify({ source: process.env.MAGENTS_TEST_EXPECTED_BUILD ?? null,
    wasm_sha256: hash(await readFile(path.join(build, 'index_bg.wasm'))), fixture: 'real-rust-worker-controlled-supervisor',
    real_python: false, external_network_calls: 0, model_calls: model.calls, code_posts: posts,
    preparations, acknowledgements, artifact_bytes: artifact.length, phases }, null, 2));
});
