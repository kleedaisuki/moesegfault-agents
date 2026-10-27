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
import { hostedImageProfile } from './hosted-image-journey.mjs';

const root = fileURLToPath(new URL('../../', import.meta.url));
const build = path.resolve(root, process.env.WORKER_BUILD_DIR ?? 'crates/agents-worker/build');
const origin = 'https://agents.test';
const digest = value => createHash('sha256').update(value).digest('hex');
const jpeg = '/9j/4AAQSkZJRgABAQAAAQABAAD/2wBDAAgGBgcGBQgHBwcJCQgKDBQNDAsLDBkSEw8UHRofHh0aHBwgJC4nICIsIxwcKDcpLDAxNDQ0Hyc5PTgyPC4zNDL/2wBDAQkJCQwLDBgNDRgyIRwhMjIyMjIyMjIyMjIyMjIyMjIyMjIyMjIyMjIyMjIyMjIyMjIyMjIyMjIyMjIyMjIyMjL/wAARCAABAAEDASIAAhEBAxEB/8QAHwAAAQUBAQEBAQEAAAAAAAAAAAECAwQFBgcICQoL/8QAtRAAAgEDAwIEAwUFBAQAAAF9AQIDAAQRBRIhMUEGE1FhByJxFDKBkaEII0KxwRVS0fAkM2JyggkKFhcYGRolJicoKSo0NTY3ODk6Q0RFRkdISUpTVFVWV1hZWmNkZWZnaGlqc3R1dnd4eXqDhIWGh4iJipKTlJWWl5iZmqKjpKWmp6ipqrKztLW2t7i5usLDxMXGx8jJytLT1NXW19jZ2uHi4+Tl5ufo6erx8vP09fb3+Pn6/8QAHwEAAwEBAQEBAQEBAQAAAAAAAAECAwQFBgcICQoL/8QAtREAAgECBAQDBAcFBAQAAQJ3AAECAxEEBSExBhJBUQdhcRMiMoEIFEKRobHBCSMzUvAVYnLRChYkNOEl8RcYGRomJygpKjU2Nzg5OkNERUZHSElKU1RVVldYWVpjZGVmZ2hpanN0dXZ3eHl6goOEhYaHiImKkpOUlZaXmJmaoqOkpaanqKmqsrO0tba3uLm6wsPExcbHyMnK0tPU1dbX2Nna4uPk5ebn6Onq8vP09fb3+Pn6/9oADAMBAAIRAxEAPwDyeiiivQOQ/9k=';
const cases = ['default_jpeg', 'receipt_ack', 'image_put', 'mime_mismatch', 'complete_error', 'transport_gap', 'diagnostic_gc'];
const sleep = ms => new Promise(resolve => setTimeout(resolve, ms));
async function until(probe, label, milliseconds = 30000) {
  const stop = Date.now() + milliseconds;
  while (Date.now() < stop) { const result = await probe(); if (result) return result; await sleep(20); }
  throw Error(`${label}: bounded observation deadline`);
}
async function migrate(db) {
  for (const name of (await readdir(path.join(root, 'migrations'))).filter(name => name.endsWith('.sql')).sort()) {
    const parsed = spawnSync(process.env.PYTHON ?? 'python', ['-c',
      'import sys,sqlite3,json\ns: str=""; out: list[str]=[]\nfor line in sys.stdin:\n s+=line\n if sqlite3.complete_statement(s): out.append(s); s=""\nassert not s.strip(), s\nprint(json.dumps(out))'],
    { input: await readFile(path.join(root, 'migrations', name), 'utf8'), encoding: 'utf8' });
    assert.equal(parsed.status, 0, `${name}: ${parsed.stderr}`);
    await db.batch(JSON.parse(parsed.stdout).map(statement => db.prepare(statement)));
  }
}

test('compiled Rust Worker: paid image response recovery and precise failure boundaries', { timeout: 240000 }, async t => {
  const selected = process.env.MAGENTS_IMAGE_RECOVERY_CASE ? cases.filter(name => name === process.env.MAGENTS_IMAGE_RECOVERY_CASE) : cases.filter(name => name !== 'diagnostic_gc');
  assert.ok(selected.length, 'Select a known image recovery increment');
  const receipts = {};
  for (const name of selected) await t.test(name, { timeout: 45000 }, async () => {
    const completeError = ['complete_error', 'diagnostic_gc'].includes(name);
    const encryptionKey = randomBytes(32), identity = createIdentityFixture(), providers = createNativeSemanticsProvider();
    const deployment = JSON.parse(await readFile(path.join(root, 'wrangler.jsonc'), 'utf8'));
    const profile = structuredClone(hostedImageProfile);
    delete profile.profiles[0].capabilities.parameters.output_format;
    profile.profiles[0].capabilities.omit_default_output_format = true;
    await mkdir(path.join(root, '.temp'), { recursive: true });
    const directory = await mkdtemp(path.join(root, '.temp/image-recovery-'));
    let imagePosts = 0;
    const telemetry = [];
    let admission;
    let mf;
    mf = new Miniflare(convertV4MiniflareOptions({ modulesRoot: build,
      modules: [{ type: 'ESModule', path: path.join(build, 'worker/shim.mjs'), contents: await readFile(new URL('./image-recovery-shim.mjs', import.meta.url), 'utf8') },
        { type: 'ESModule', path: path.join(build, 'index.js') }, { type: 'CompiledWasm', path: path.join(build, 'index_bg.wasm') }],
      compatibilityDate: '2026-10-23', compatibilityFlags: ['enable_request_signal'],
      handleStructuredLogs({ level, message }) {
        try { const value = JSON.parse(message); if (value.event) telemetry.push(value); } catch {}
        if (level === 'error') console.error(message);
      },
      resourcePersistencePath: path.join(directory, 'storage'), d1Databases: ['DB'], r2Buckets: ['WORKSPACES', 'CONVERSATIONS', 'DIAGNOSTICS'],
      durableObjects: { SESSIONS: { className: 'Session', useSQLite: true }, AUTH_SESSIONS: { className: 'AuthSession', useSQLite: true },
        CONVERSATION_STATE: { className: 'ConversationState', useSQLite: true }, MODEL_OPERATIONS: { className: 'ModelOperation', useSQLite: true } },
      ratelimits: Object.fromEntries(['INGRESS', 'ACCOUNT', 'MUTATION'].map((key, index) => [`${key}_RATE_LIMITER`, { namespace_id: String(index + 1), simple: { limit: 1000, period: 60 } }])),
      bindings: { ENVIRONMENT: 'test', APP_ORIGIN: origin, ALLOWED_ORIGIN: origin,
        DATA_ENCRYPTION_KEY: encryptionKey.toString('base64url'), IDENTITY_ISSUER: 'https://identity.test', IDENTITY_WEB_CLIENT_ID: 'test-dashboard',
        IDENTITY_NATIVE_CLIENT_ID: 'test-native', IDENTITY_REDIRECT_URI: `${origin}/auth/callback`, IDENTITY_NATIVE_REDIRECT_URI: 'http://127.0.0.1/callback',
        IDENTITY_CLIENT_PRIVATE_JWK: JSON.stringify(identity.privateJwk), BILLING_PRODUCT_ID: 'agents', BILLING_MODE: 'test', MODEL_REGISTRY_JSON: '[]', MODEL_PRICING_JSON: '{}',
        OPENROUTER_API_KEY: 'synthetic-platform-image-key', HOSTED_TOOL_RUNTIMES: 'image_generation', IMAGE_GENERATION_PROFILE_JSON: JSON.stringify(profile),
        IMAGE_RECOVERY_FAULT: ['receipt_ack', 'image_put'].includes(name) ? name : '',
        HOSTED_TOOL_CAPABILITIES_JSON: JSON.stringify({ version: 1, platform_images: true, platform_search: false, file_search: false, container_memory: null,
          available_container_profiles: [], code_network_disabled: false, code_network_allowlist: false }),
        RESOURCE_PRICING_JSON: deployment.env.staging.vars.RESOURCE_PRICING_JSON, RUN_RESOURCE_RESERVATION_MICROUSD: '20000', FREE_GRANT_BUDGET_MICROUSD_PER_DAY: '100000000' },
      async outboundService(request) {
        if (new URL(request.url).origin === 'https://openrouter.ai') {
          imagePosts++;
          assert.equal(imagePosts, 1, 'A receive, codec or R2 failure cannot repeat the paid generation');
          assert.equal(request.headers.get('authorization'), 'Bearer synthetic-platform-image-key');
          const body = await request.json();
          assert.equal(body.output_format, undefined, 'Omitted output format must remain provider default');
          const db = await mf.getD1Database('DB');
          admission = await db.prepare('SELECT state,capacity_bytes,max_frames FROM provider_response_operations').first();
          assert.equal(admission.state, 'reserved'); assert.equal(admission.max_frames, 1); assert.ok(admission.capacity_bytes >= 24 * 1024 * 1024);
          if (name === 'transport_gap') throw Error('Controlled transport gap after provider dispatch');
          const headers = { 'x-request-id': 'request-controlled', 'x-generation-id': 'gen-img-controlled', 'cf-ray': 'controlled-ray' };
          if (completeError) return Response.json({ error: { code: 502, message: 'Controlled complete upstream failure' } }, { status: 502, headers });
          return Response.json({ id: 'gen-img-controlled', created: 123, data: [{ b64_json: jpeg, media_type: name === 'mime_mismatch' ? 'image/png' : 'image/jpeg' }], usage: { cost: 0.018 } }, { headers });
        }
        const response = await providers.handle(request);
        assert.notEqual(response, undefined, 'Controlled recovery fixtures must never contact the public network');
        return response;
      } }));
    try {
      const db = await mf.getD1Database('DB'); await migrate(db);
      const principal = await createBrowserPrincipal({ mf, db, encryptionKey, owner: `image-recovery-${name}` });
      const model = providers.register(`image-recovery-${name}`, [
        { output: [{ id: 'controlled-image-call', type: 'function_call', name: '__agents_image_generation', call_id: `controlled-${name}`,
          arguments: JSON.stringify({ prompt: 'Generate the controlled one-pixel fixture.' }), status: 'completed' }] },
        { output: [{ id: 'controlled-image-answer', type: 'message', role: 'assistant', status: 'completed', content: [{ type: 'output_text', text: 'The original image operation has a durable outcome.', annotations: [], logprobs: [] }] }] },
      ]);
      const credential = await principal.browser('POST', '/v1/provider-keys', { protocol: 'openai', base_url: model.baseUrl, api_key: model.key }, 201);
      const client = new OpenAI({ apiKey: principal.apiKey, baseURL: `${origin}/v1`, maxRetries: 0, fetch: (input, init) => mf.dispatchFetch(input, init) });
      const agentResponse = await mf.dispatchFetch(`${origin}/v1/agents`, { method: 'POST', headers: { Authorization: `Bearer ${principal.apiKey}`, 'Content-Type': 'application/json' },
        body: JSON.stringify({ name: 'Image recovery increment', workspace: false, tools: [], model: { provider: 'byok', credential_id: credential.id, protocol: 'responses', model: model.model, context_window: 32768, max_output_tokens: 512 }, tool_services: { image_generation: { provider: 'platform' } } }) });
      assert.equal(agentResponse.status, 201, await agentResponse.clone().text());
      const agent = await agentResponse.json();
      const requestBody = { model: agent.id, input: 'Run one controlled image operation.', background: true, tools: [{ type: 'image_generation' }], max_tool_calls: 1 };
      const initial = await (await client.responses.create(requestBody, { headers: { 'Idempotency-Key': `image-recovery-${name}` } }).asResponse()).json();
      const route = await until(() => db.prepare('SELECT id,session_id FROM response_routes WHERE account_id=?').bind(principal.owner).first(), 'Response route');
      async function inspect() {
        const ns = await mf.getDurableObjectNamespace('SESSIONS');
        const result = await ns.get(ns.idFromName(`${principal.owner}:${route.session_id}`)).fetch('https://fixture.internal/__test/image-recovery');
        assert.equal(result.status, 200); return result.json();
      }
      let faultReceipt;
      if (['receipt_ack', 'image_put'].includes(name)) {
        faultReceipt = await until(async () => { const state = await inspect(); return state.fault.injected && state.operations[0]?.value.lifecycle_failure ? state : null; }, 'Actual fault injection');
        assert.equal(faultReceipt.fault.encrypted, true, 'The R2 response body is encrypted before storage');
        assert.ok(faultReceipt.operations[0]?.value.provider_replies.length > 0, 'The raw receipt pointer precedes the failed acknowledgement or artifact write');
        await mf.unsafeEvictDurableObject('', 'Session', { name: `${principal.owner}:${route.session_id}` });
      }
      const response = await until(async () => {
        const value = await (await client.responses.retrieve(initial.id).asResponse()).json();
        return ['completed', 'failed', 'incomplete'].includes(value.status) ? value : null;
      }, 'Original public Response terminal', 35000);
      if (model.failure) throw model.failure;
      assert.equal(response.status, 'completed');
      assert.equal(imagePosts, 1); assert.equal(model.calls, 2);
      const image = response.output.find(item => item.type === 'image_generation_call');
      assert.ok(image);
      const success = ['default_jpeg', 'receipt_ack', 'image_put'].includes(name);
      assert.equal(image.status, success ? 'completed' : 'failed');
      if (success) {
        assert.equal(digest(Buffer.from(image.result, 'base64')), digest(Buffer.from(jpeg, 'base64')));
        assert.equal(image.output_format, 'jpeg');
        const files = await client.files.list(); assert.equal(files.data.length, 1);
        assert.equal(files.data[0].filename.endsWith('.jpg'), true);
        const downloaded = Buffer.from(await (await client.files.content(files.data[0].id)).arrayBuffer());
        assert.equal(digest(downloaded), digest(Buffer.from(jpeg, 'base64')));
        const operation = await until(async () => { const row = await db.prepare('SELECT state,capacity_bytes FROM provider_response_operations WHERE account_id=?').bind(principal.owner).first(); return row?.state === 'released' ? row : null; }, 'Published raw response cleanup');
        assert.equal(operation.capacity_bytes, 0);
        const live = await db.prepare('SELECT COUNT(*) AS count FROM provider_response_frames WHERE account_id=? AND deleted=0').bind(principal.owner).first();
        assert.equal(live.count, 0);
        const bucket = await mf.getR2Bucket('WORKSPACES');
        assert.equal((await bucket.list({ prefix: 'provider-response-receipts/' })).objects.length, 0);
      } else {
        assert.equal(image.result, null);
        const privateState = await inspect();
        const operation = privateState.operations[0].value;
        assert.equal(operation.observation?.upstream_status ?? null, name === 'transport_gap' ? null : completeError ? 502 : 200);
        if (name === 'mime_mismatch') {
          assert.equal(operation.observation.actual_format, 'jpeg');
          assert.equal(operation.observation.provider_completed, true);
          assert.equal(operation.observation.reported_cost_usd, 0.018);
          assert.equal(operation.observation.generation_id, 'gen-img-controlled');
          assert.equal(operation.failure, 'image_generation_format_mismatch');
        }
        const retained = await db.prepare('SELECT state,capacity_bytes,expires_at_ms FROM provider_response_operations WHERE account_id=?').bind(principal.owner).first();
        assert.equal(retained.state, completeError ? 'diagnostic' : 'recovery');
        assert.equal(retained.expires_at_ms !== null, completeError);
        if (name !== 'transport_gap') assert.ok(retained.capacity_bytes > 0);
      }
      const imageHolds = (await db.prepare("SELECT state,CAST(reserved_microusd AS TEXT) AS reserved,CAST(settled_microusd AS TEXT) AS settled FROM usage_reservations WHERE account_id=? AND idempotency_key LIKE 'tool:seedream5:%'").bind(principal.owner).all()).results;
      assert.equal(imageHolds.length, 1);
      assert.equal(imageHolds[0].state, name === 'transport_gap' ? 'reserved' : 'settled');
      assert.equal(imageHolds[0].settled, name === 'transport_gap' ? null : completeError ? '0' : '18000');
      await mf.unsafeEvictDurableObject('', 'Session', { name: `${principal.owner}:${route.session_id}` });
      const observed = await (await client.responses.retrieve(response.id).asResponse()).json();
      assert.deepEqual(observed, response, 'Cold retrieval preserves the original canonical outcome');
      assert.equal(imagePosts, 1, 'Cold public observation never reissues generation');
      let gcReceipt;
      if (name === 'diagnostic_gc') {
        async function sweep(offset, reserve_ack = false) {
          const result = await mf.dispatchFetch(origin + '/__test/image-recovery-sweep', { method: 'POST', body: JSON.stringify({ offset, reserve_ack }) });
          assert.equal(result.status, 200); return result.json();
        }
        const first = await sweep(86_401_000, true);
        assert.equal(first.fault.injected, true, 'The actual Rust GC reserved before the controlled lost acknowledgement');
        const abandoned = await db.prepare('SELECT state FROM usage_reservations WHERE id=?').bind(first.fault.reservation).first();
        assert.equal(abandoned.state, 'reserved');
        assert.equal((await db.prepare('SELECT COUNT(*) AS count FROM resource_observations WHERE run_id=?').bind(first.fault.reservation).first()).count, 0);
        const beforeSecond = await db.prepare('SELECT state FROM provider_response_operations WHERE account_id=?').bind(principal.owner).first();
        assert.equal(beforeSecond.state, 'deleting');
        await sweep(86_702_000);
        const finalized = await db.prepare('SELECT state,capacity_bytes,gc_attempt FROM provider_response_operations WHERE account_id=?').bind(principal.owner).first();
        assert.deepEqual(finalized, { state: 'released', capacity_bytes: 0, gc_attempt: 2 });
        const funding = (await db.prepare("SELECT id,state,CAST(settled_microusd AS TEXT) AS settled FROM usage_reservations WHERE account_id=? AND id LIKE 'receipt-gc:%' ORDER BY id").bind(principal.owner).all()).results;
        assert.equal(funding.length, 2);
        assert.equal(funding.find(row => row.id === first.fault.reservation).state, 'released', 'Retry closes the original empty scope, not a fabricated zero ledger');
        const measured = funding.find(row => row.id !== first.fault.reservation);
        assert.equal(measured.state, 'settled');
        const observations = (await db.prepare("SELECT status,CAST(quantity AS INTEGER) AS quantity FROM resource_observations WHERE run_id=? AND CAST(quantity AS INTEGER)>0").bind(measured.id).all()).results;
        assert.deepEqual(observations, [{ status: 'observed', quantity: 1 }], 'The one retained chunk incurs exactly one actual HEAD');
        assert.equal((await db.prepare("SELECT COUNT(*) AS count FROM usage_reservations WHERE account_id=? AND id LIKE 'receipt-gc:%' AND state='reserved'").bind(principal.owner).first()).count, 0);
        assert.equal((await (await mf.getR2Bucket('WORKSPACES')).list({ prefix: 'provider-response-receipts/' })).objects.length, 0);
        assert.equal(imagePosts, 1); assert.equal(model.calls, 2);
        gcReceipt = { attempts: 2, original_scope: first.fault.reservation, original_state: 'released', measured_state: measured.state, measured_microusd: measured.settled, actual_heads: 1, raw_objects: 0, pending_holds: 0, timestamp_unit_correction: 'pending_final_composed_source' };
      }
      const logs = await until(() => {
        const matching = telemetry.filter(value => value.event.startsWith('tool.image_generation.'));
        return matching.some(value => value.event === (success ? 'tool.image_generation.validated' : 'tool.image_generation.failed')) ? matching : null;
      }, 'Actual emitted Image telemetry');
      const terminal = logs.findLast(value => value.event === (success ? 'tool.image_generation.validated' : 'tool.image_generation.failed'));
      assert.ok(terminal.fields.observation, 'Real emit sanitizer must preserve structured observation, not only actor state');
      if (name !== 'transport_gap') {
        const headers = logs.find(value => value.event === 'tool.image_generation.headers');
        assert.equal(headers.fields.upstream_status, completeError ? 502 : 200);
        assert.equal(headers.fields.provider_request_id, 'request-controlled');
        assert.equal(headers.fields.provider_generation_id, 'gen-img-controlled');
      }
      if (name === 'mime_mismatch') {
        assert.equal(terminal.fields.observation.actual_format, 'jpeg');
        assert.equal(terminal.fields.observation.upstream_status, 200);
        assert.equal(terminal.fields.observation.generation_id, 'gen-img-controlled');
        assert.equal(Number(terminal.fields.observation.reported_cost_usd), 0.018);
        assert.equal(terminal.fields.observation.phase, 'decode_image');
        assert.equal(terminal.fields.error_code, 'image_generation_format_mismatch');
      }
      const serializedLogs = JSON.stringify(logs);
      assert.ok(!serializedLogs.includes(jpeg), 'Binary payload belongs to restricted receipt storage, not spans');
      assert.ok(!serializedLogs.includes('synthetic-platform-image-key'), 'Credentials are not diagnostic data');
      await writeFile(path.join(directory, 'telemetry.json'), JSON.stringify(logs, null, 2));
      receipts[name] = { gc: gcReceipt ?? null, telemetry_events: logs.map(value => value.event), response_id: response.id, generation_posts: imagePosts, model_calls: model.calls, cold_recovery: !!faultReceipt, encrypted_before_codec: faultReceipt?.fault.encrypted ?? null,
        image_status: image.status, reservation_state: imageHolds[0].state, image_microusd: imageHolds[0].settled, external_network_calls: 0 };
    } finally { await mf.dispose(); }
  });
  assert.deepEqual(Object.keys(receipts), selected);
  await mkdir(path.join(root, '.cache/v0.3-ci'), { recursive: true });
  await writeFile(path.join(root, '.cache/v0.3-ci/image-recovery.json'), JSON.stringify({ source: process.env.MAGENTS_WORKER_SOURCE_SHA ?? process.env.GITHUB_SHA ?? null,
    wasm_sha256: digest(await readFile(path.join(build, 'index_bg.wasm'))), fixture: 'actual-compiled-rust-worker', executed_case_ids: selected, ...receipts }, null, 2));
});
