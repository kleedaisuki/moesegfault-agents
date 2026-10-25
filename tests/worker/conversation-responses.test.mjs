import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { randomBytes } from 'node:crypto';
import { mkdir, mkdtemp, readFile, readdir } from 'node:fs/promises';
import path from 'node:path';
import test from 'node:test';
import { fileURLToPath } from 'node:url';
import OpenAI from 'openai';
import { Miniflare, convertV4MiniflareOptions } from 'miniflare';
import { createIdentityFixture } from './auth-journey.mjs';
import { createResponseJourneyFixture, createResponsesProviderFixture } from './responses-journey.mjs';
import { assertCompletedResponse } from './responses-sdk.mjs';

const root = fileURLToPath(new URL('../../', import.meta.url));
const build = path.resolve(root, process.env.WORKER_BUILD_DIR ?? 'crates/agents-worker/build');
const fixed = 'Fixed agent prefix must remain in every model context.';
const targetPin = 'CONVERSATION-TARGET-PIN-月';
const markers = ['conversation-seed-雪', 'conversation-first-星', 'conversation-second-月',
  'conversation-metadata-turn-風', 'conversation-after-delete-火', 'conversation-future-item-水',
  'conversation-target-turn-土', 'conversation-call-turn-光', 'conversation-local-result-森'];
const localTool = {
  type: 'function', name: 'conversation_local_echo', strict: true,
  parameters: { type: 'object', properties: { value: { type: 'string' } },
    required: ['value'], additionalProperties: false },
};

function occurrences(value, marker) {
  return JSON.stringify(value).split(marker).length - 1;
}

function checkContext(included, excluded, pin = fixed) {
  return input => {
    for (const marker of included) assert.equal(occurrences(input.messages, marker), 1,
      `Conversation context must contain ${marker} exactly once`);
    for (const marker of excluded) assert.equal(occurrences(input.messages, marker), 0,
      `Conversation context must not contain ${marker}`);
    const instructions = input.messages.filter(message => ['system', 'developer'].includes(message.role));
    assert.equal(occurrences(instructions, pin), 1, 'One fixed pin, not a copied system prefix for every conversation turn');
    if (pin === targetPin) assert.equal(occurrences(instructions, fixed), 0,
      'Cross-agent adoption transfers user history, not the original agent permission/instruction prefix');
  };
}

async function waitUntil(predicate, timeout = 8000) {
  const deadline = Date.now() + timeout;
  while (true) {
    if (await predicate()) return;
    assert.ok(Date.now() < deadline, 'Conversation journey did not reach its expected boundary');
    await new Promise(resolve => setTimeout(resolve, 10));
  }
}

async function grantLocalGo(context, owner) {
  const now = Math.floor(Date.now() / 1000);
  const subscriptions = [{ id: `synthetic-conversation-go-${owner}`, product_id: 'agents', plan_id: 'agents-go', status: 'active',
    current_period_start: now - 10, current_period_end: now + 3600, entitlements: ['agents.access', 'agents.tier.go'] }];
  await context.db.prepare("INSERT INTO subscription_snapshots(account_id,product_id,subscription_json,fetched_at,valid_until) VALUES(?,'agents',?,?,?)")
    .bind(owner, JSON.stringify(subscriptions), now, now + 300).run();
}

function sdkFor(context, scenario) {
  return new OpenAI({ apiKey: scenario.principal.apiKey, baseURL: 'https://agents.test/v1',
    maxRetries: 0, timeout: 8000, fetch: (input, init) => context.mf.dispatchFetch(input, init) });
}

async function assertModelLedger(context, scenario, responseIds) {
  const rows = await context.db.prepare(`
    SELECT r.idempotency_key,r.state,CAST(l.quantity AS TEXT) AS quantity,l.unit,
      CAST(l.amount_microusd AS TEXT) AS amount,l.price_version
    FROM usage_ledger l JOIN usage_reservations r ON r.id=l.reservation_id
    WHERE r.account_id=? AND l.resource='model' ORDER BY r.idempotency_key
  `).bind(scenario.principal.owner).all();
  assert.deepEqual(rows.results, responseIds.map(id => ({ idempotency_key: `completion:${id}:round:0`,
    state: 'settled', quantity: '160', unit: 'tokens', amount: '0', price_version: 'byok',
  })).sort((left, right) => left.idempotency_key.localeCompare(right.idempotency_key)),
  'Every known BYOK inference is metered once; conversation admission and replay cannot add model charges');
}

async function deleteTarget(context, scenario, target) {
  if (!target) return;
  const sessions = await context.request('GET', `/v1/agents/${target.id}/sessions`, undefined, 200, scenario.principal.apiKey);
  for (const session of sessions.data) {
    await waitUntil(async () => !(await context.request('GET', `/v1/sessions/${session.id}`, undefined, 200, scenario.principal.apiKey)).run_id);
    await context.request('DELETE', `/v1/sessions/${session.id}`, undefined, 200, scenario.principal.apiKey);
  }
  await context.request('DELETE', `/v1/agents/${target.id}`, undefined, 200, scenario.principal.apiKey);
}

async function runConversationLifecycle(context) {
  const scenario = await createResponseJourneyFixture(context, 'conversation-lifecycle', [
    { text: 'Conversation answer one.', check: checkContext(markers.slice(0, 2), markers.slice(2)) },
    { text: 'Conversation answer two.', check: checkContext(markers.slice(0, 3), markers.slice(3)) },
    { text: 'Conversation answer three.', check: checkContext(markers.slice(0, 4), markers.slice(4)) },
    { text: 'Conversation answer after deletion.', check: checkContext(markers.slice(1, 5), [markers[0], ...markers.slice(5)]) },
    { text: 'Conversation target answer.', check: checkContext(markers.slice(1, 7), [markers[0], ...markers.slice(7)], targetPin) },
    { calls: [{ id: 'conversation-local-call', name: localTool.name, arguments: { value: 'from-conversation' } }],
      check: checkContext(markers.slice(1, 8), [markers[0], markers[8]], targetPin) },
    { text: 'Conversation local tool completed.', check(input) {
      checkContext(markers.slice(1), [markers[0]], targetPin)(input);
      const results = input.messages.filter(message => message.role === 'tool' && message.tool_call_id === 'conversation-local-call');
      assert.equal(results.length, 1);
      assert.equal(results[0].content, markers[8]);
    } },
  ]);
  const client = sdkFor(context, scenario);
  let conversation;
  let deleted = false;
  let target;
  const originals = [];
  async function invoke(body, options = {}) {
    const result = await client.responses.create({ model: scenario.agent.id, conversation: conversation.id, ...body }, options).withResponse();
    assertCompletedResponse(result.data);
    assert.equal(result.data.conversation?.id, conversation.id);
    assert.ok(result.response.headers.get('x-agents-session'));
    originals.push(result.data);
    await scenario.read(result.data.id);
    return { value: result.data, sessionId: result.response.headers.get('x-agents-session') };
  }
  async function assertOriginals() {
    assert.deepEqual(await Promise.all(originals.map(value => client.responses.retrieve(value.id))), originals,
      'Conversation metadata/item edits and later turns never rewrite accepted Response snapshots');
  }
  try {
    await grantLocalGo(context, scenario.principal.owner);
    conversation = await client.conversations.create({ metadata: { stage: 'initial' },
      items: [{ type: 'message', role: 'user', content: markers[0] }] });
    assert.equal(conversation.object, 'conversation');
    assert.deepEqual(await client.conversations.retrieve(conversation.id), conversation);
    const initialItems = await client.conversations.items.list(conversation.id, { order: 'asc', limit: 100 });
    assert.equal(initialItems.data.length, 1);
    const seedItem = initialItems.data[0];
    assert.equal(occurrences(seedItem, markers[0]), 1);
    const first = await invoke({ input: markers[1], instructions: 'CONVERSATION-RESPONSE-ONLY-ONE' });
    const pinned = (await context.request('GET', `/v1/sessions/${first.sessionId}`, undefined, 200, scenario.principal.apiKey)).pinned;
    const second = await invoke({ input: markers[2] });
    assert.equal(second.sessionId, first.sessionId, 'A compatible latest Conversation revision reuses its warm execution session');
    assert.equal(occurrences(scenario.state.requests[1].messages, 'CONVERSATION-RESPONSE-ONLY-ONE'), 0,
      'Response-scoped instructions are not conversation state');
    const metadata = await client.conversations.update(conversation.id, { metadata: { stage: 'updated' } });
    assert.deepEqual(metadata.metadata, { stage: 'updated' });
    const third = await invoke({ input: markers[3] });
    assert.equal(third.sessionId, first.sessionId, 'Metadata-only edits do not invalidate model-context reuse');
    assert.deepEqual((await context.request('GET', `/v1/sessions/${third.sessionId}`, undefined, 200, scenario.principal.apiKey)).pinned, pinned);
    const beforeDelete = await client.conversations.items.list(conversation.id, { order: 'asc', limit: 100 });
    assert.equal(beforeDelete.data.length, 7, 'Initial item plus three input/output pairs are each appended once');
    assert.equal(new Set(beforeDelete.data.map(item => item.id)).size, 7);
    for (const marker of markers.slice(0, 4)) assert.equal(occurrences(beforeDelete.data, marker), 1);
    const deletedItem = await client.conversations.items.delete(seedItem.id, { conversation_id: conversation.id });
    assert.equal(deletedItem.id, conversation.id);
    await assert.rejects(client.conversations.items.retrieve(seedItem.id, { conversation_id: conversation.id }),
      error => error instanceof OpenAI.APIError && error.status === 404);
    const fourthBody = { input: markers[4] };
    const fourth = await invoke(fourthBody, { headers: { 'Idempotency-Key': 'conversation-after-delete' } });
    assert.notEqual(fourth.sessionId, first.sessionId, 'Removing an input invalidates the old execution lane without changing its history');
    const oldSource = await context.request('GET', `/v1/sessions/${first.sessionId}`, undefined, 200, scenario.principal.apiKey);
    assert.equal(occurrences(oldSource.messages, markers[0]), 1, 'Deleting a Conversation item cannot erase the original source journal');
    const added = await client.conversations.items.create(conversation.id, {
      items: [{ type: 'message', role: 'user', content: markers[5] }],
    });
    assert.equal(added.data.length, 1);
    const beforeReplay = await client.conversations.items.list(conversation.id, { order: 'asc', limit: 100 });
    const replay = await client.responses.create({ model: scenario.agent.id, conversation: conversation.id, ...fourthBody },
      { headers: { 'Idempotency-Key': 'conversation-after-delete' } }).withResponse();
    assert.deepEqual(replay.data, fourth.value, 'An accepted idempotent request retains its original admission snapshot after future item edits');
    assert.equal(replay.response.headers.get('x-agents-session'), fourth.sessionId);
    assert.deepEqual((await client.conversations.items.list(conversation.id, { order: 'asc', limit: 100 })).data, beforeReplay.data,
      'Replaying an already committed Response cannot append its input/output twice');
    assert.equal(scenario.state.calls, 4);
    await assertOriginals();
    target = await context.request('POST', '/v1/agents', { name: 'Conversation target', instructions: targetPin,
      model: scenario.agent.model, tools: ['time'], workspace: false, compaction_percent: 80 }, 201, scenario.principal.apiKey);
    const fifth = await invoke({ model: target.id, input: markers[6] });
    assert.notEqual(fifth.sessionId, fourth.sessionId);
    const handoff = await invoke({ model: target.id, input: markers[7], tools: [localTool],
      tool_choice: { type: 'function', name: localTool.name } });
    assert.equal(handoff.sessionId, fifth.sessionId);
    const calls = handoff.value.output.filter(item => item.type === 'function_call');
    assert.equal(calls.length, 1);
    assert.equal(calls[0].call_id, 'conversation-local-call');
    const last = await invoke({ model: target.id, tools: [localTool],
      input: [{ type: 'function_call_output', call_id: calls[0].call_id, output: markers[8] }] });
    assert.equal(last.sessionId, fifth.sessionId, 'A Conversation-local function result resumes its bound execution lane');
    assert.equal(last.value.output_text, 'Conversation local tool completed.');
    const finalItems = await client.conversations.items.list(conversation.id, { order: 'asc', limit: 100 });
    assert.equal(finalItems.data.length, 15, 'Tombstoned seed, one manual item and seven input/output pairs have exact visibility');
    assert.equal(new Set(finalItems.data.map(item => item.id)).size, finalItems.data.length);
    assert.equal(occurrences(finalItems.data, markers[0]), 0);
    for (const marker of markers.slice(1)) assert.equal(occurrences(finalItems.data, marker), 1);
    await assertOriginals();
    assert.equal(scenario.state.calls, 7);
    await assertModelLedger(context, scenario, originals.map(value => value.id));
    const deletion = await client.conversations.delete(conversation.id);
    deleted = true;
    assert.deepEqual(deletion, { id: conversation.id, object: 'conversation.deleted', deleted: true });
    await assert.rejects(client.conversations.retrieve(conversation.id), error => error instanceof OpenAI.APIError && error.status === 404);
    assert.deepEqual((await client.conversations.items.list(conversation.id, { order: 'asc', limit: 100 })).data, finalItems.data,
      'Deleting a Conversation does not delete its items');
    await assertOriginals();
  } catch (error) { scenario.recordFailure(error); throw error; }
  finally {
    const errors = [];
    try { if (conversation && !deleted) await client.conversations.delete(conversation.id); } catch (error) { errors.push(error); }
    try { await deleteTarget(context, scenario, target); } catch (error) { errors.push(error); }
    try { await scenario.finish(); } catch (error) { errors.push(error); }
    if (errors.length === 1) throw errors[0];
    if (errors.length) throw new AggregateError(errors, 'Conversation lifecycle primary and cleanup failures retained');
  }
}

async function runConversationWriterFence(context) {
  let release;
  const pending = new Promise(resolve => { release = resolve; });
  const scenario = await createResponseJourneyFixture(context, 'conversation-writer-fence', [
    { text: 'First writer completed.', wait: pending, check(input) {
      assert.equal(occurrences(input.messages, 'active-writer-input'), 1);
      assert.equal(occurrences(input.messages, 'rejected-concurrent-writer'), 0);
    } },
    { text: 'Next writer completed.' },
  ]);
  const client = sdkFor(context, scenario);
  let conversation;
  try {
    await grantLocalGo(context, scenario.principal.owner);
    conversation = await client.conversations.create();
    const first = await client.responses.create({ model: scenario.agent.id, conversation: conversation.id,
      input: 'active-writer-input', background: true }).withResponse();
    const sessionId = first.response.headers.get('x-agents-session');
    assert.ok(sessionId);
    await waitUntil(() => scenario.state.calls === 1);
    await assert.rejects(client.responses.create({ model: scenario.agent.id, conversation: conversation.id,
      input: 'rejected-concurrent-writer', background: true }), error => {
      assert.ok(error instanceof OpenAI.APIError);
      assert.equal(error.status, 409);
      assert.equal(error.error?.code, 'conversation_busy');
      assert.equal(error.error?.active_response_id, first.data.id, 'Busy response identifies the canonical active writer');
      assert.equal(error.error?.active_session_id, sessionId, 'Busy response exposes the existing recovery handle, not another execution');
      assert.equal(error.error?.activated, true);
      return true;
    });
    assert.equal(scenario.state.calls, 1, 'Conversation contention must reject before another paid request');
    release();
    await waitUntil(async () => (await scenario.read(first.data.id)).status === 'completed');
    const firstCompleted = await client.responses.retrieve(first.data.id);
    assertCompletedResponse(firstCompleted, 'First writer completed.');
    const second = await client.responses.create({ model: scenario.agent.id, conversation: conversation.id,
      input: 'next-writer-after-release' }).withResponse();
    assertCompletedResponse(second.data, 'Next writer completed.');
    assert.equal(second.response.headers.get('x-agents-session'), sessionId, 'Terminal completion releases authority for the next compatible writer');
    assert.deepEqual(await client.responses.retrieve(first.data.id), firstCompleted);
    const items = await client.conversations.items.list(conversation.id, { order: 'asc', limit: 100 });
    assert.equal(items.data.length, 4);
    assert.equal(occurrences(items.data, 'rejected-concurrent-writer'), 0, 'A rejected writer cannot append user data into the Conversation');
    assert.equal(scenario.state.calls, 2);
    await assertModelLedger(context, scenario, [first.data.id, second.data.id]);
  } catch (error) { scenario.recordFailure(error); throw error; }
  finally {
    release();
    const errors = [];
    try {
      if (conversation) {
        await waitUntil(async () => {
          const sessions = await context.request('GET', `/v1/agents/${scenario.agent.id}/sessions`, undefined, 200, scenario.principal.apiKey);
          return (await Promise.all(sessions.data.map(session => context.request('GET', `/v1/sessions/${session.id}`, undefined, 200, scenario.principal.apiKey))))
            .every(session => !session.run_id);
        });
        await client.conversations.delete(conversation.id);
      }
    } catch (error) { errors.push(error); }
    try { await scenario.finish(); } catch (error) { errors.push(error); }
    if (errors.length === 1) throw errors[0];
    if (errors.length) throw new AggregateError(errors, 'Conversation writer primary and cleanup failures retained');
  }
}

test('compiled Rust Worker: OpenAI SDK Conversations bind immutable Responses and execution lanes', { timeout: 90000 }, async t => {
  const encryptionKey = randomBytes(32);
  const identity = createIdentityFixture();
  const providers = createResponsesProviderFixture();
  const deployment = JSON.parse(await readFile(path.join(root, 'wrangler.jsonc'), 'utf8'));
  await mkdir(path.join(root, '.temp'), { recursive: true });
  const directory = await mkdtemp(path.join(root, '.temp/conversation-responses-'));
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
      BILLING_PRODUCT_ID: 'agents', BILLING_MODE: 'test', MODEL_REGISTRY_JSON: '[]', MODEL_PRICING_JSON: '{}',
      RESOURCE_PRICING_JSON: deployment.env.staging.vars.RESOURCE_PRICING_JSON,
      RUN_RESOURCE_RESERVATION_MICROUSD: '20000', FREE_GRANT_BUDGET_MICROUSD_PER_DAY: '100000000',
    },
    async outboundService(request) {
      const response = await providers.handle(request);
      assert.notEqual(response, undefined, 'Conversation fixtures cannot access external services');
      return response;
    },
  }));
  t.after(() => mf.dispose());
  const db = await mf.getD1Database('DB');
  for (const name of (await readdir(path.join(root, 'migrations'))).filter(name => name.endsWith('.sql')).sort()) {
    const sql = await readFile(path.join(root, 'migrations', name), 'utf8');
    const parsed = spawnSync(process.env.PYTHON ?? 'python', ['-c',
      'import sys,sqlite3,json\ns: str=""; out: list[str]=[]\nfor line in sys.stdin:\n s+=line\n if sqlite3.complete_statement(s): out.append(s); s=""\nassert not s.strip(), s\nprint(json.dumps(out))'],
    { input: sql, encoding: 'utf8' });
    assert.equal(parsed.status, 0, `${name}: ${parsed.stderr}`);
    await db.batch(JSON.parse(parsed.stdout).map(statement => db.prepare(statement)));
  }
  async function request(method, endpoint, body, expected = 200, bearer) {
    const response = await mf.dispatchFetch('https://agents.test' + endpoint, {
      method, headers: { Authorization: `Bearer ${bearer}`, 'Content-Type': 'application/json', 'CF-Connecting-IP': '192.0.2.70' },
      ...(body === undefined ? {} : { body: JSON.stringify(body) }),
    });
    const text = await response.text();
    assert.equal(response.status, expected, `${method} ${endpoint}: ${text}`);
    return JSON.parse(text);
  }
  const context = { mf, db, encryptionKey, request, providers, diagnostic: message => t.diagnostic(message) };
  await t.test('snapshot ownership, warm reuse, item edits, target pins and local function handoff', () => runConversationLifecycle(context));
  await t.test('an activated Conversation writer is discoverable and rejects conflicting execution', () => runConversationWriterFence(context));
});
