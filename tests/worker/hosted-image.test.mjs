import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { createHash, randomBytes } from 'node:crypto';
import { mkdir, mkdtemp, readFile, readdir, writeFile } from 'node:fs/promises';
import path from 'node:path';
import test from 'node:test';
import { fileURLToPath } from 'node:url';
import { crc32 } from 'node:zlib';
import OpenAI from 'openai';
import { Miniflare, convertV4MiniflareOptions } from 'miniflare';
import { createIdentityFixture } from './auth-journey.mjs';
import { createBrowserPrincipal } from './browser-principal.mjs';
import { createNativeSemanticsProvider } from './native-semantics-journey.mjs';
import { createHostedByokImageFixture, createHostedImageProviderFixture, hostedImagePng, hostedImageProfile } from './hosted-image-journey.mjs';

const root = fileURLToPath(new URL('../../', import.meta.url));
const build = path.resolve(root, process.env.WORKER_BUILD_DIR ?? 'crates/agents-worker/build');
const origin = 'https://agents.test';
const sha = bytes => createHash('sha256').update(bytes).digest('hex');
const imageBytes = Buffer.from(hostedImagePng, 'base64');
function markedImage(mark) {
  const type = Buffer.from('tEXt'), data = Buffer.from(`fixture\0${mark}`), chunk = Buffer.alloc(data.length + 12);
  chunk.writeUInt32BE(data.length); type.copy(chunk, 4); data.copy(chunk, 8);
  chunk.writeUInt32BE(crc32(Buffer.concat([type, data])), data.length + 8);
  return Buffer.concat([imageBytes.subarray(0, -12), chunk, imageBytes.subarray(-12)]);
}
const partialBytes = [markedImage('preview-0'), markedImage('preview-1')];
const finalBytes = markedImage('final');
const partialOrigin = 'https://images-lifecycle.example.com/v1';
const partialProfile = { api: 'openai', base_url: partialOrigin, model: 'fixture-image-stream', public_model: null,
  capabilities: { streaming: true, parameters: { output_format: { type: 'enum', values: ['png', 'jpeg', 'webp'] }, size: { type: 'boolean' } },
    max_references: 16, mask: true }, router_endpoint: null, asset_hosts: [],
  price_revision: 'synthetic-owned-stream-v1', reservation_microusd: 0, platform_funded: false };
const caseNames = ['stored', 'no_store', 'lifecycle', 'byok', 'funded_delivery', 'conversation'];
const delay = milliseconds => new Promise(resolve => setTimeout(resolve, milliseconds));

function latch() {
  let resolve;
  const promise = new Promise(done => { resolve = done; });
  return { promise, resolve };
}

async function until(probe, label, milliseconds = 15000) {
  const deadline = Date.now() + milliseconds;
  while (Date.now() < deadline) {
    const result = await probe();
    if (result) return result;
    await delay(10);
  }
  throw Error(`${label}: bounded observation deadline`);
}

function events(text) {
  return text.split('\n\n').flatMap(block => {
    const line = block.split('\n').find(value => value.startsWith('data: '));
    return line && line.slice(6) !== '[DONE]' ? [JSON.parse(line.slice(6))] : [];
  });
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

function validateWire(response, stream) {
  const jsonOnly = stream.length === 0;
  const arguments_ = jsonOnly ? ['-c', `
import json, sys, openai
from typing import Any
from openai.types.responses import Response
value: dict[str, Any] = json.load(sys.stdin)
validated: Response = Response.model_validate(value, strict=True)
assert validated.status == "completed"
print(json.dumps({"sdk": openai.__version__, "scope": "strict_current_sdk_json_only", "passed": True}))
`] : ['tests/e2e/current_responses_sdk.py', 'response-wire', '--input', '-'];
  const checked = spawnSync(process.env.MAGENTS_CURRENT_SCHEMA_PYTHON,
    arguments_,
    { cwd: root, input: JSON.stringify(jsonOnly ? response : { response, events: stream }), encoding: 'utf8', maxBuffer: 1024 * 1024 });
  assert.equal(checked.status, 0, checked.stderr || checked.stdout);
  return JSON.parse(checked.stdout);
}

async function imageRows(db, owner) {
  return (await db.prepare("SELECT r.kind,r.ref_id,r.session_id,r.object_key,r.response_id,r.lease_until_ms,b.sha256 FROM workspace_blob_refs r JOIN workspace_blobs b ON b.object_key=r.object_key WHERE r.account_id=? AND b.kind='image' ORDER BY r.kind,r.ref_id")
    .bind(owner).all()).results;
}

async function inspect(mf, owner, route) {
  const namespace = await mf.getDurableObjectNamespace('SESSIONS');
  const actor = namespace.get(namespace.idFromName(`${owner}:${route.session_id}`));
  const response = await actor.fetch('https://session.internal/__test/hosted-image-inspect',
    { method: 'POST', body: JSON.stringify({ responseId: route.id }) });
  assert.equal(response.status, 200);
  return response.json();
}

async function setClock(mf, owner, route, offset) {
  const body = JSON.stringify({ offset });
  const app = await (await mf.dispatchFetch(`${origin}/__test/hosted-image-clock`, { method: 'POST', body })).json();
  const namespace = await mf.getDurableObjectNamespace('SESSIONS');
  const actor = namespace.get(namespace.idFromName(`${owner}:${route.session_id}`));
  const actual = await (await actor.fetch('https://session.internal/__test/hosted-image-clock', { method: 'POST', body })).json();
  assert.ok(Math.abs(app.constructed_now - actual.constructed_now) < 5000, 'Application and actor test clock epochs agree without imposing a latency benchmark');
  return actual;
}

test('compiled Rust Worker: hosted image generation and private delivery lifecycles', { timeout: 120000 }, async t => {
  assert.ok(process.env.MAGENTS_CURRENT_SCHEMA_PYTHON, 'Use the current strict SDK interpreter, not a permissive old parser');
  // The BYOK no-registry application is intentionally a separately selected
  // deployment fixture; ordinary cases need the verified platform registry.
  const selected = caseNames.filter(name => process.env.MAGENTS_HOSTED_IMAGE_CASE
    ? name === process.env.MAGENTS_HOSTED_IMAGE_CASE : name !== 'byok');
  assert.ok(selected.length, `Unknown hosted image case ${JSON.stringify(process.env.MAGENTS_HOSTED_IMAGE_CASE)}`);
  const encryptionKey = randomBytes(32), identity = createIdentityFixture(), providers = createNativeSemanticsProvider();
  const images = createHostedImageProviderFixture();
  const ownedImages = createHostedByokImageFixture();
  const partialCalls = [];
  const deployment = JSON.parse(await readFile(path.join(root, 'wrangler.jsonc'), 'utf8'));
  await mkdir(path.join(root, '.temp'), { recursive: true });
  const directory = await mkdtemp(path.join(root, '.temp/hosted-image-'));
  let expectedOwner;
  let lockWalletOwner;
  const capacityReceipts = [];
  const mf = new Miniflare(convertV4MiniflareOptions({ modulesRoot: build,
    modules: [{ type: 'ESModule', path: path.join(build, 'worker/shim.mjs'), contents: await readFile(new URL('./hosted-image-lifecycle-shim.mjs', import.meta.url), 'utf8') },
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
      OPENROUTER_API_KEY: 'synthetic-platform-image-key',
      HOSTED_TOOL_RUNTIMES: 'image_generation',
      ...(selected.every(name => name === 'byok') ? {} : { IMAGE_GENERATION_PROFILE_JSON: JSON.stringify({ version: 1, profiles: [...hostedImageProfile.profiles, partialProfile] }) }),
      HOSTED_TOOL_CAPABILITIES_JSON: JSON.stringify({ version: 1, platform_images: true, platform_search: false,
        file_search: false, container_memory: null, available_container_profiles: [], code_network_disabled: false, code_network_allowlist: false }),
      RESOURCE_PRICING_JSON: deployment.env.staging.vars.RESOURCE_PRICING_JSON,
      RUN_RESOURCE_RESERVATION_MICROUSD: '20000', FREE_GRANT_BUDGET_MICROUSD_PER_DAY: '100000000' },
    async outboundService(request) {
      const url = new URL(request.url);
      if (request.url === `${ownedImages.baseUrl}/images/generations`) {
        const db = await mf.getD1Database('DB');
        const held = await db.prepare('SELECT COUNT(*) AS count FROM owner_blob_capacity_holds WHERE account_id=?')
          .bind(expectedOwner).first();
        assert.equal(held.count, 1);
        return ownedImages.handle(request);
      }
      if (url.origin === 'https://openrouter.ai' || request.url === `${partialOrigin}/images/generations`) {
        assert.ok(expectedOwner, 'The real caller must exist before image dispatch');
        const db = await mf.getD1Database('DB');
        const holds = (await db.prepare('SELECT operation_id,bytes FROM owner_blob_capacity_holds WHERE account_id=? ORDER BY operation_id')
          .bind(expectedOwner).all()).results;
        const expectedSlots = url.origin === 'https://openrouter.ai' ? 1 : 3;
        assert.equal(holds.length, expectedSlots, 'Reserve every final/preview slot before dispatch');
        for (const hold of holds) assert.equal(hold.bytes, 16 * 1024 * 1024);
        capacityReceipts.push({ owner: expectedOwner, slots: holds.length, bytes: holds[0].bytes });
        if (lockWalletOwner === expectedOwner) {
          assert.equal(expectedOwner, 'hosted-image-funded-delivery');
          await db.prepare("CREATE TRIGGER image_fixture_wallet_lock AFTER UPDATE ON usage_accounts WHEN NEW.account_id='hosted-image-funded-delivery' AND NEW.budget_microusd!=NEW.spent_microusd+NEW.reserved_microusd BEGIN UPDATE usage_accounts SET budget_microusd=NEW.spent_microusd+NEW.reserved_microusd WHERE account_id=NEW.account_id; END").run();
          await db.prepare('UPDATE usage_accounts SET budget_microusd=spent_microusd+reserved_microusd WHERE account_id=?').bind(expectedOwner).run();
          lockWalletOwner = undefined;
        }
        if (expectedSlots === 3) {
          assert.equal(request.method, 'POST');
          assert.equal(request.headers.get('authorization'), 'Bearer synthetic-owned-image-stream');
          const body = await request.json();
          assert.equal(body.model, partialProfile.model); assert.equal(body.partial_images, 2); assert.equal(body.stream, true);
          partialCalls.push(body);
          const event = value => `data: ${JSON.stringify(value)}\n\n`;
          return new Response(event({ type: 'image_generation.partial_image', partial_image_index: 0, b64_json: partialBytes[0].toString('base64') })
            + event({ type: 'image_generation.partial_image', partial_image_index: 1, b64_json: partialBytes[1].toString('base64') })
            + event({ type: 'image_generation.completed', id: 'owned-image-known', created_at: 123,
              b64_json: finalBytes.toString('base64'), output_format: 'png', usage: { cost: '0.04' } }),
          { headers: { 'Content-Type': 'text/event-stream', 'x-request-id': 'owned-image-request' } });
        }
        return images.handle(request);
      }
      const response = await providers.handle(request);
      assert.notEqual(response, undefined, 'Image lifecycle fixtures must never call the public Internet');
      return response;
    } }));
  t.after(() => mf.dispose());
  const db = await mf.getD1Database('DB'); await migrate(db);
  async function request(method, endpoint, body, expected = 200, bearer) {
    const response = await mf.dispatchFetch(origin + endpoint, { method,
      headers: { Authorization: `Bearer ${bearer}`, 'Content-Type': 'application/json', 'CF-Connecting-IP': '192.0.2.74' },
      ...(body === undefined ? {} : { body: JSON.stringify(body) }) });
    const text = await response.text();
    assert.equal(response.status, expected, `${method} ${endpoint}: ${text}`);
    return text ? JSON.parse(text) : null;
  }
  const receipts = {};
  for (const name of selected) await t.test(name, { timeout: 75000 }, async subtest => {
    if (name === 'conversation') {
      const principal = await createBrowserPrincipal({ mf, db, encryptionKey, owner: 'hosted-image-conversation' });
      expectedOwner = principal.owner;
      const beforePosts = images.state.calls.length;
      const model = providers.register('hosted-image-conversation', [
        { output: [{ id: 'conversation-image-call', type: 'function_call', name: '__agents_image_generation', call_id: 'conversation-image',
          arguments: JSON.stringify({ prompt: 'Generate one retained conversation fixture.' }), status: 'completed' }] },
        { output: [{ id: 'conversation-image-answer', type: 'message', role: 'assistant', status: 'completed',
          content: [{ type: 'output_text', text: 'The conversation retains its image.', annotations: [], logprobs: [] }] }] },
      ]);
      const credential = await principal.browser('POST', '/v1/provider-keys', { protocol: 'openai', base_url: model.baseUrl, api_key: model.key }, 201);
      const agent = await request('POST', '/v1/agents', { name: 'Conversation image retention', workspace: false, tools: [],
        model: { provider: 'byok', credential_id: credential.id, protocol: 'responses', model: model.model, context_window: 32768, max_output_tokens: 512 },
        tool_services: { image_generation: { provider: 'platform' } } }, 201, principal.apiKey);
      const client = new OpenAI({ apiKey: principal.apiKey, baseURL: `${origin}/v1`, maxRetries: 0,
        fetch: (input, init) => mf.dispatchFetch(input, init) });
      const conversation = await client.conversations.create({});
      const stream = await client.responses.create({ model: agent.id, input: 'Generate for this conversation.',
        conversation: conversation.id, store: false, stream: true, tools: [{ type: 'image_generation' }] });
      const frames = [];
      for await (const event of stream) frames.push(event);
      if (model.failure) throw model.failure;
      const response = frames.findLast(event => event.type === 'response.completed')?.response;
      assert.ok(response);
      const image = response.output.find(item => item.type === 'image_generation_call');
      assert.equal(image?.result, hostedImagePng);
      const schema = validateWire(response, frames);
      const holder = await until(async () => (await imageRows(db, principal.owner)).find(row => row.kind === 'conversation-artifact'), 'Conversation owns the image after terminal acknowledgement');
      const files = await request('GET', '/v1/files', undefined, 200, principal.apiKey);
      for (const file of files.data) await request('DELETE', '/v1/files/' + file.id, undefined, 200, principal.apiKey);
      await assert.rejects(client.responses.retrieve(response.id), error => error.status === 404);
      const retrieved = await client.conversations.items.retrieve(image.id, { conversation_id: conversation.id });
      assert.equal(retrieved.result, hostedImagePng, 'Conversation retrieval hydrates its own retained bytes after Files deletion');
      const listed = await client.conversations.items.list(conversation.id);
      assert.equal(listed.data.find(item => item.id === image.id).result, hostedImagePng, 'List and single-item JSON hydration agree');
      await client.conversations.delete(conversation.id);
      assert.equal((await client.conversations.items.retrieve(image.id, { conversation_id: conversation.id })).result, hostedImagePng,
        'Deleting Conversation metadata preserves its image items');
      await client.conversations.items.delete(image.id, { conversation_id: conversation.id });
      await assert.rejects(client.conversations.items.retrieve(image.id, { conversation_id: conversation.id }), error => error.status === 404);
      assert.equal((await imageRows(db, principal.owner)).filter(row => row.kind === 'conversation-artifact').length, 0);
      const late = await db.prepare("INSERT INTO workspace_blob_refs(account_id,kind,ref_id,object_key,artifact_id) VALUES(?,'conversation-artifact',?,?,?)")
        .bind(principal.owner, holder.ref_id, holder.object_key, image.id).run()
        .then(result => ({ result }), error => ({ error }));
      assert.ok(late.error || late.result?.success === false, 'An item tombstone fences a delayed Conversation image publisher');
      assert.equal(images.state.calls.length - beforePosts, 1);
      assert.equal(model.calls, 2);
      receipts.conversation = { response_id: response.id, conversation_id: conversation.id, image_item_id: image.id,
        generation_posts: 1, model_calls: 2, store_false_public_get: 404, files_independent: true,
        metadata_delete_preserves_images: true, item_delete_releases_holder: true, sdk: schema.sdk };
      return;
    }
    if (name === 'funded_delivery') {
      const principal = await createBrowserPrincipal({ mf, db, encryptionKey, owner: 'hosted-image-funded-delivery' });
      expectedOwner = principal.owner;
      const beforePosts = images.state.calls.length;
      const model = providers.register('hosted-image-funded-delivery', [
        { output: [{ id: 'funded-call', type: 'function_call', name: '__agents_image_generation', call_id: 'funded-image-call',
          arguments: JSON.stringify({ prompt: 'Generate one prepaid fixture image.' }), status: 'completed' }] },
        { output: [{ id: 'funded-answer', type: 'message', role: 'assistant', status: 'completed',
          content: [{ type: 'output_text', text: 'The prepaid image was delivered.', annotations: [], logprobs: [] }] }] },
      ]);
      const credential = await principal.browser('POST', '/v1/provider-keys', { protocol: 'openai', base_url: model.baseUrl, api_key: model.key }, 201);
      const agent = await request('POST', '/v1/agents', { name: 'Prepaid image delivery', tools: [], workspace: false,
        model: { provider: 'byok', credential_id: credential.id, protocol: 'responses', model: model.model, context_window: 32768, max_output_tokens: 512 },
        tool_services: { image_generation: { provider: 'platform' } } }, 201, principal.apiKey);
      lockWalletOwner = principal.owner;
      const client = new OpenAI({ apiKey: principal.apiKey, baseURL: `${origin}/v1`, maxRetries: 0,
        fetch: (input, init) => mf.dispatchFetch(input, init) });
      const delivered = await client.responses.create({ model: agent.id, input: 'Deliver using the accepted hold.',
        tools: [{ type: 'image_generation' }], store: true, stream: false });
      assert.equal(sha(Buffer.from(delivered.output.find(item => item.type === 'image_generation_call').result, 'base64')), sha(imageBytes));
      const wallet = await db.prepare('SELECT budget_microusd-spent_microusd-reserved_microusd AS available FROM usage_accounts WHERE account_id=?').bind(principal.owner).first('available');
      assert.equal(wallet, 0, 'The fixture must not accidentally gain new balance from run settlement');
      const reads = (await db.prepare("SELECT session_id,CAST(quantity AS TEXT) AS quantity FROM resource_observations WHERE account_id=? AND event_key LIKE 'image.original:%' AND resource='r2_operation' AND unit='class_b'").bind(principal.owner).all()).results;
      assert.ok(reads.length > 0, 'Original delivery must actually read through the prepaid image hold');
      assert.ok(reads.every(row => row.session_id === null && row.quantity === '1'));
      await request('GET', `/v1/responses/${delivered.id}`, undefined, 402, principal.apiKey);
      assert.equal(images.state.calls.length - beforePosts, 1);
      assert.equal(model.calls, 2, 'Neither failed later retrieval nor accepted delivery may regenerate');
      const schema = validateWire(delivered, []);
      receipts.funded_delivery = { response_id: delivered.id, original_json_prepaid: true, unreserved_balance: 0,
        later_fresh_get_status: 402, original_r2_reads: reads.length, generation_posts: 1, model_calls: model.calls, sdk: schema.sdk };
      return;
    }
    if (name === 'byok') {
      assert.equal(selected.length, 1, 'The explicit BYOK case must run without any operator image registry');
      const principal = await createBrowserPrincipal({ mf, db, encryptionKey, owner: 'hosted-image-owned-basic' });
      expectedOwner = principal.owner;
      const model = providers.register('hosted-image-owned-basic', [
        { output: [{ id: 'owned-native-call', type: 'function_call', name: '__agents_image_generation', call_id: 'owned-image-call',
          arguments: JSON.stringify({ prompt: 'Use my unknown self-hosted model.' }), status: 'completed' }] },
        { output: [{ id: 'owned-native-answer', type: 'message', role: 'assistant', status: 'completed',
          content: [{ type: 'output_text', text: 'Owned model completed.', annotations: [], logprobs: [] }] }] },
      ]);
      const outer = await principal.browser('POST', '/v1/provider-keys', { protocol: 'openai', base_url: model.baseUrl, api_key: model.key }, 201);
      const imageKey = await principal.browser('POST', '/v1/provider-keys', { protocol: 'openai', base_url: ownedImages.baseUrl, api_key: ownedImages.key }, 201);
      const agent = await request('POST', '/v1/agents', { name: 'Self-service image service', workspace: false, tools: [],
        model: { provider: 'byok', credential_id: outer.id, protocol: 'responses', model: model.model, context_window: 32768, max_output_tokens: 512 },
        tool_services: { image_generation: { provider: 'openai_images', credential_id: imageKey.id, model: ownedImages.model } } }, 201, principal.apiKey);
      const client = new OpenAI({ apiKey: principal.apiKey, baseURL: `${origin}/v1`, maxRetries: 0,
        fetch: (input, init) => mf.dispatchFetch(input, init) });
      const response = await client.responses.create({ model: agent.id, input: 'Generate via my service.', tools: [{ type: 'image_generation' }] });
      const image = response.output.find(item => item.type === 'image_generation_call');
      assert.ok(image); assert.equal(image.status, 'completed'); assert.equal(image.result, hostedImagePng);
      assert.equal(ownedImages.state.calls.length, 1); assert.equal(images.state.calls.length, 0);
      const raw = await request('GET', `/v1/responses/${response.id}`, undefined, 200, principal.apiKey);
      const replay = await mf.dispatchFetch(`${origin}/v1/responses/${response.id}?stream=true`,
        { headers: { Authorization: `Bearer ${principal.apiKey}` } });
      assert.equal(replay.status, 200);
      const schema = validateWire(raw, events(await replay.text()));
      const before = await db.prepare('SELECT COUNT(*) AS count FROM usage_reservations WHERE account_id=?').bind(principal.owner).first();
      for (const option of [{ partial_images: 1 }, { size: '1024x1024' }, { background: 'transparent' }]) {
        await request('POST', '/v1/responses', { model: agent.id, input: 'Undeclared advanced feature.',
          tools: [{ type: 'image_generation', ...option }] }, 400, principal.apiKey);
      }
      assert.equal(model.calls, 2, 'Unsupported advanced service controls fail before outer inference');
      assert.equal(ownedImages.state.calls.length, 1, 'No unsupported control reaches the owned service');
      const after = await db.prepare('SELECT COUNT(*) AS count FROM usage_reservations WHERE account_id=?').bind(principal.owner).first();
      assert.equal(after.count, before.count, 'Rejected capability declarations must not create inference or service holds');
      const charges = (await db.prepare("SELECT CAST(l.amount_microusd AS TEXT) AS amount FROM usage_ledger l JOIN usage_reservations r ON r.id=l.reservation_id WHERE r.account_id=? AND l.unit='generated_image'").bind(principal.owner).all()).results;
      assert.deepEqual(charges.map(row => row.amount), ['0'], 'User-owned API fees are not platform model debits');
      receipts.byok = { response_id: response.id, generation_posts: 1, model_calls: 2, operator_registry: false,
        sdk: schema.sdk, platform_image_posts: 0, undeclared_features_rejected_before_funds: 3, platform_image_microusd: '0' };
      return;
    }
    const gate = name === 'stored' ? null : latch();
    subtest.after(() => gate?.resolve());
    const principal = await createBrowserPrincipal({ mf, db, encryptionKey, owner: `hosted-image-${name}` });
    expectedOwner = principal.owner;
    const beforePosts = images.state.calls.length;
    const beforePartials = partialCalls.length;
    const model = providers.register(`hosted-image-${name}`, [
      { output: [{ id: 'native-image-call', type: 'function_call', name: '__agents_image_generation', call_id: 'image-call',
        arguments: JSON.stringify({ prompt: `Synthetic ${name} image.` }), status: 'completed' }] },
      { output: [{ id: 'native-image-answer', type: 'message', role: 'assistant', status: 'completed',
        content: [{ type: 'output_text', text: 'Image operation recorded.', annotations: [], logprobs: [] }] }], gate,
        check(body) {
          const result = body.input.findLast(item => item.type === 'function_call_output' && item.call_id === 'image-call');
          const output = result?.output;
          const text = typeof output === 'string' ? output
            : Array.isArray(output) && output.length === 1 && output[0]?.type === 'input_text' ? output[0].text : undefined;
          assert.equal(typeof text, 'string', 'Hosted image receipts cross the native function-result boundary as JSON text or one input_text part');
          assert.equal(JSON.parse(text).status, 'completed');
        } },
    ]);
    const credential = await principal.browser('POST', '/v1/provider-keys', { protocol: 'openai', base_url: model.baseUrl, api_key: model.key }, 201);
    const imageCredential = name === 'lifecycle' ? await principal.browser('POST', '/v1/provider-keys',
      { protocol: 'openai', base_url: partialOrigin, api_key: 'synthetic-owned-image-stream' }, 201) : null;
    const agent = await request('POST', '/v1/agents', { name: `Image ${name}`, workspace: false, tools: [],
      model: { provider: 'byok', credential_id: credential.id, protocol: 'responses', model: model.model, context_window: 32768, max_output_tokens: 512 },
      tool_services: { image_generation: imageCredential ? { provider: 'openai_images', credential_id: imageCredential.id, model: partialProfile.model } : { provider: 'platform' } } }, 201, principal.apiKey);
    let rawEvents, sdkStream;
    const client = new OpenAI({ apiKey: principal.apiKey, baseURL: `${origin}/v1`, maxRetries: 0, fetch: async (input, init) => {
      const response = await mf.dispatchFetch(input, init);
      if (name !== 'lifecycle' && response.headers.get('content-type')?.startsWith('text/event-stream')) rawEvents = response.clone().text().then(events);
      return response;
    } });
    const store = name === 'stored';
    const body = { model: agent.id, input: 'Generate exactly one fixture image.',
      tools: [{ type: 'image_generation', ...(imageCredential ? { partial_images: 2 } : {}) }], store, stream: true };
    const observed = [];
    const running = (async () => {
      sdkStream = await client.responses.create(body, { headers: { 'Idempotency-Key': `hosted-${name}` } });
      for await (const event of sdkStream) observed.push(event);
      return observed.findLast(event => event.type === 'response.completed')?.response;
    })();
    let runFailure;
    running.catch(error => { runFailure = error; });
    let route;
    let acceptedPrivate;
    let terminalClockFloor;
    if (gate) {
      await until(() => { if (runFailure) throw runFailure; return model.calls === 2; }, 'The image must finish before the final-model latch');
      route = await db.prepare('SELECT id,session_id FROM response_routes WHERE account_id=?').bind(principal.owner).first();
      assert.ok(route);
      const active = await inspect(mf, principal.owner, route);
      acceptedPrivate = active;
      assert.equal(body.execution, undefined, 'The ordinary HTTP caller supplies no trusted connection or execution authority');
      const held = await imageRows(db, principal.owner);
      const execution = held.filter(row => row.kind === 'image-execution');
      assert.equal(execution.length, name === 'lifecycle' ? 3 : 1, 'An accepted run retains each exact image until terminal acknowledgement');
      for (const row of execution) {
        assert.equal(row.lease_until_ms, null, 'An in-flight execution is not an expiring delivery cache');
        assert.equal(row.session_id, route.session_id);
        assert.equal(row.response_id, route.id);
      }
      assert.equal(held.filter(row => row.kind === 'response-artifact').length, 0, 'No-store must not acquire a permanent Response holder');
      assert.equal(active.inline_final, false); assert.equal(active.inline_partial, false);
      if (name === 'lifecycle') {
        const expectedHashes = [...partialBytes, finalBytes].map(sha).sort();
        assert.deepEqual(execution.map(row => row.sha256).sort(), expectedHashes);
        assert.deepEqual(active.images.map(asset => asset.blob.sha256).sort(), expectedHashes,
          'Canonical preview slots point to their own exact blob, not the final replacement');
        await until(() => observed.filter(event => event.type === 'response.image_generation_call.partial_image').length === 2
          && observed.some(event => event.type === 'response.output_item.done' && event.item?.type === 'image_generation_call'),
        'Published previews and final image reach the real SDK before disconnect');
        sdkStream.controller.abort();
        await running.catch(() => {});
        const visible = await request('GET', '/v1/files', undefined, 200, principal.apiKey);
        for (const file of visible.data) await request('DELETE', `/v1/files/${file.id}`, undefined, 200, principal.apiKey);
        assert.equal(held.filter(row => !['space-file', 'image-execution'].includes(row.kind)).length, 0,
          'An expiring connection cache must not substitute for the execution holder');
        await db.prepare("UPDATE workspace_blobs SET lease_until_ms=0 WHERE account_id=? AND kind='image'").bind(principal.owner).run();
        const advanced = await setClock(mf, principal.owner, route, 70 * 60 * 1000);
        assert.ok(Math.abs(advanced.now - advanced.constructed_now) < 100, 'Static and constructor clocks must agree for worker-rs');
        assert.ok(advanced.constructed_now > Date.now() + 69 * 60 * 1000);
        try {
          await mf.dispatchFetch(`${origin}/__test/hosted-image-sweep`);
          assert.equal((await imageRows(db, principal.owner)).filter(row => row.kind === 'image-execution').length, 3,
            'Expiry of an old connection/cache cannot collect an accepted run image');
        } finally {
          await setClock(mf, principal.owner, route, 0);
        }
        terminalClockFloor = (await setClock(mf, principal.owner, route, 5000)).constructed_now;
      }
      gate.resolve();
    }
    const response = name === 'lifecycle' ? null : await running;
    if (name !== 'lifecycle') assert.ok(response, JSON.stringify({
      error: 'The real Rust run must emit its terminal Response', model_calls: model.calls,
      image_posts: images.state.calls.length - beforePosts, terminal: observed.at(-1)?.type,
      error_code: observed.at(-1)?.response?.agents_error_code, fixture_error: model.failure?.message,
    }));
    const stream = name === 'lifecycle' ? observed : await rawEvents;
    const image = response?.output.find(item => item.type === 'image_generation_call')
      ?? observed.findLast(event => event.type === 'response.output_item.done' && event.item?.type === 'image_generation_call')?.item;
    assert.ok(image); assert.equal(image.status, 'completed');
    const expectedImage = name === 'lifecycle' ? finalBytes : imageBytes;
    assert.equal(sha(Buffer.from(image.result, 'base64')), sha(expectedImage));
    if (name === 'lifecycle') {
      for (const [index, bytes] of partialBytes.entries()) {
        const preview = stream.find(event => event.type === 'response.image_generation_call.partial_image' && event.partial_image_index === index);
        assert.ok(preview, 'Each provider preview must be retained as its own canonical event');
        assert.equal(sha(Buffer.from(preview.partial_image_b64, 'base64')), sha(bytes));
      }
    }
    const schema = response ? validateWire(stream.findLast(event => event.type === 'response.completed').response, stream) : null;
    route ??= await db.prepare('SELECT id,session_id FROM response_routes WHERE account_id=?').bind(principal.owner).first();
    const responseId = response?.id ?? route.id;
    if (name === 'lifecycle') await until(async () => {
      const rows = await imageRows(db, principal.owner);
      return rows.filter(row => row.kind === 'image-execution').length === 0
        && rows.filter(row => row.kind === 'image-delivery').length === 3;
    }, 'The disconnected run still finishes its canonical transfer');
    let terminalClockCeiling;
    if (name === 'lifecycle') {
      terminalClockCeiling = (await setClock(mf, principal.owner, route, 5000)).constructed_now;
      await setClock(mf, principal.owner, route, 0);
    }
    const observedPrivate = await inspect(mf, principal.owner, route);
    assert.ok(observedPrivate.found || !store, 'Stored Responses retain their canonical representation');
    const privateState = observedPrivate.found ? observedPrivate : acceptedPrivate;
    assert.ok(privateState.canonical_bytes < 65536, 'Canonical state contains references, not duplicate base64');
    assert.equal(privateState.inline_final, false); assert.equal(privateState.inline_partial, false);
    const files = await request('GET', '/v1/files', undefined, 200, principal.apiKey);
    const file = files.data.find(item => item.filename?.startsWith(image.id));
    if (name !== 'lifecycle') assert.ok(file);
    if (file) await request('DELETE', `/v1/files/${file.id}`, undefined, 200, principal.apiKey);
    if (store) {
      const before = await request('GET', `/v1/responses/${responseId}`, undefined, 200, principal.apiKey);
      await mf.unsafeEvictDurableObject('', 'Session', { name: `${principal.owner}:${route.session_id}` });
      const after = await request('GET', `/v1/responses/${responseId}`, undefined, 200, principal.apiKey);
      assert.equal(after.output.find(item => item.id === image.id).result, image.result);
      assert.deepEqual(after, before);
      const replay = await client.responses.create(body, { headers: { 'Idempotency-Key': `hosted-${name}` } });
      for await (const ignored of replay) void ignored;
      const retained = (await imageRows(db, principal.owner)).find(row => row.kind === 'response-artifact');
      assert.ok(retained);
      await request('DELETE', `/v1/responses/${responseId}`, undefined, 200, principal.apiKey);
      await request('GET', `/v1/responses/${responseId}`, undefined, 404, principal.apiKey);
      await until(async () => (await db.prepare('SELECT COUNT(*) AS count FROM response_blob_tombstones WHERE account_id=? AND response_id=?')
        .bind(principal.owner, responseId).first()).count === 1, 'Public DELETE records its publication fence');
      const late = await db.prepare("INSERT INTO workspace_blob_refs(account_id,kind,ref_id,object_key,response_id) VALUES(?,'response-artifact',?,?,?)")
        .bind(principal.owner, `late-${responseId}`, retained.object_key, responseId).run()
        .then(result => ({ result }), error => ({ error }));
      assert.ok(late.error || late.result?.success === false, 'A late publisher cannot resurrect a deleted Response holder');
      if (late.error) assert.match(String(late.error), /response_blob_deleted/);
    } else {
      await request('GET', `/v1/responses/${responseId}`, undefined, 404, principal.apiKey);
      await request('POST', '/v1/responses', { model: agent.id, previous_response_id: responseId, input: 'This must not grant parent authority.' }, 404, principal.apiKey);
      const refs = await imageRows(db, principal.owner);
      assert.equal(refs.filter(row => row.kind === 'image-execution').length, 0, 'Canonical terminal acknowledgement closes execution holders');
      assert.equal(refs.filter(row => row.kind === 'response-artifact').length, 0);
      const delivery = refs.filter(row => row.kind === 'image-delivery');
      assert.equal(delivery.length, name === 'lifecycle' ? 3 : 1, 'Each terminal slot has its own scoped private delivery holder');
      for (const row of delivery) {
        assert.ok(row.lease_until_ms > Date.now());
        assert.ok(row.lease_until_ms <= (terminalClockCeiling ?? Date.now()) + 60 * 60 * 1000,
          'Private retention is bounded from terminal acknowledgement, not initial admission');
        if (terminalClockFloor) assert.ok(row.lease_until_ms >= terminalClockFloor + 60 * 60 * 1000,
          'The delivery window starts at terminal ACK, not the earlier admission timestamp');
      }
      if (name === 'lifecycle') {
        // Advance beyond the preceding 70-minute scan as well as the terminal
        // lease. A backwards cron cursor would be a fixture, not product, defect.
        await setClock(mf, principal.owner, route, 75 * 60 * 1000);
        try {
          await mf.dispatchFetch(`${origin}/__test/hosted-image-sweep`);
          assert.equal((await imageRows(db, principal.owner)).filter(row => row.kind !== 'space-file').length, 0,
            'Terminal private delivery expires without granting public retrieval authority');
          await request('GET', `/v1/responses/${responseId}`, undefined, 404, principal.apiKey);
        } finally {
          await setClock(mf, principal.owner, route, 0);
        }
      }
    }
    assert.equal(images.state.calls.length - beforePosts + partialCalls.length - beforePartials, 1, 'GET/cold replay/expiry cannot regenerate an image');
    assert.equal(model.calls, 2);
    const charges = (await db.prepare("SELECT CAST(l.amount_microusd AS TEXT) AS amount FROM usage_ledger l JOIN usage_reservations r ON r.id=l.reservation_id WHERE r.account_id=? AND l.unit='generated_image'").bind(principal.owner).all()).results;
    assert.deepEqual(charges.map(row => row.amount), [name === 'lifecycle' ? '0' : '18000']);
    assert.equal((await db.prepare('SELECT COUNT(*) AS count FROM owner_blob_capacity_holds WHERE account_id=?').bind(principal.owner).first()).count, 0);
    if (name === 'lifecycle') {
      const lost = await createBrowserPrincipal({ mf, db, encryptionKey, owner: 'hosted-image-ambiguous' });
      expectedOwner = lost.owner;
      const plan = providers.register('hosted-image-ambiguous', [
        { output: [{ id: 'lost-native-image', type: 'function_call', name: '__agents_image_generation', call_id: 'lost-image-call',
          arguments: JSON.stringify({ prompt: 'One ambiguous generation attempt.' }), status: 'completed' }] },
        { output: [{ id: 'lost-native-answer', type: 'message', role: 'assistant', status: 'completed',
          content: [{ type: 'output_text', text: 'The image result is unknown.', annotations: [], logprobs: [] }] }] },
      ]);
      const key = await lost.browser('POST', '/v1/provider-keys', { protocol: 'openai', base_url: plan.baseUrl, api_key: plan.key }, 201);
      const lostAgent = await request('POST', '/v1/agents', { name: 'Unknown image receipt', workspace: false, tools: [],
        model: { provider: 'byok', credential_id: key.id, protocol: 'responses', model: plan.model, context_window: 32768, max_output_tokens: 512 },
        tool_services: { image_generation: { provider: 'platform' } } }, 201, lost.apiKey);
      const before = images.state.calls.length;
      images.state.mode = 'disconnect';
      let unknown;
      try {
        unknown = await request('POST', '/v1/responses', { model: lostAgent.id, input: 'Attempt one image.',
          tools: [{ type: 'image_generation' }], max_tool_calls: 1 }, 200, lost.apiKey);
      } finally { images.state.mode = 'inline'; }
      const failedImage = unknown.output.find(item => item.type === 'image_generation_call');
      assert.ok(failedImage); assert.equal(failedImage.status, 'failed'); assert.equal(failedImage.result, null);
      const lostRoute = await db.prepare('SELECT id,session_id FROM response_routes WHERE account_id=?').bind(lost.owner).first();
      await mf.unsafeEvictDurableObject('', 'Session', { name: `${lost.owner}:${lostRoute.session_id}` });
      await request('GET', `/v1/responses/${unknown.id}`, undefined, 200, lost.apiKey);
      assert.equal(images.state.calls.length - before, 1, 'Cold observation cannot repeat an ambiguous paid POST');
      const pending = (await db.prepare("SELECT state,CAST(reserved_microusd AS TEXT) AS amount FROM usage_reservations WHERE account_id=? AND idempotency_key LIKE 'tool:seedream5:%'")
        .bind(lost.owner).all()).results;
      assert.deepEqual(pending, [{ state: 'reserved', amount: '18000' }], 'Unknown billing must remain held, not invented as free or refunded');
      assert.equal((await db.prepare('SELECT COUNT(*) AS count FROM owner_blob_capacity_holds WHERE account_id=?').bind(lost.owner).first()).count, 1);
      assert.equal((await db.prepare("SELECT COUNT(*) AS count FROM usage_ledger l JOIN usage_reservations r ON r.id=l.reservation_id WHERE r.account_id=? AND l.unit='generated_image'")
        .bind(lost.owner).first()).count, 0);
    }
    const cleanupHolds = (await db.prepare("SELECT created_at FROM usage_reservations WHERE account_id=? AND idempotency_key LIKE 'image-cleanup:%'")
      .bind(principal.owner).all()).results;
    assert.ok(cleanupHolds.length);
    for (const hold of cleanupHolds) assert.ok(hold.created_at <= Math.floor(Date.now() / 1000),
      'Image cleanup reservations use the shared ledger Unix-seconds contract');
    receipts[name] = { response_id: responseId, generation_posts: 1, model_calls: model.calls, image_sha256: sha(expectedImage),
      sdk: schema?.sdk ?? null, observer_disconnected_before_terminal: name === 'lifecycle',
      private_canonical_bytes: privateState.canonical_bytes, memory_measurement: null,
      in_flight_non_ttl_holder: !store, store_false_public_authority: false };
  });
  assert.deepEqual(Object.keys(receipts).sort(), selected.sort(), 'Do not write a success receipt for a failed case');
  await mkdir(path.join(root, '.cache/v0.3-ci'), { recursive: true });
  await writeFile(path.join(root, '.cache/v0.3-ci/hosted-image.json'), JSON.stringify({ source: process.env.MAGENTS_TEST_EXPECTED_BUILD ?? process.env.MAGENTS_WORKER_SOURCE_SHA ?? process.env.GITHUB_SHA ?? null,
    wasm_sha256: sha(await readFile(path.join(build, 'index_bg.wasm'))), fixture: 'actual-compiled-rust-worker',
    executed_case_ids: selected, not_executed_case_ids: caseNames.filter(name => !selected.includes(name)),
    external_network_calls: 0, capacity_before_post: capacityReceipts, ...receipts }, null, 2));
});
