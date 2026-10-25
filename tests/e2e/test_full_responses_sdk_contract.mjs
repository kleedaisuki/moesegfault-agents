import assert from 'node:assert/strict';
import test from 'node:test';
import fs from 'node:fs';
import path from 'node:path';
import { randomUUID, createHash } from 'node:crypto';
import { EventEmitter } from 'node:events';
import { WebSocketServer } from 'ws';
import OpenAI from 'openai';
import { ResponsesWS } from 'openai/resources/responses/ws';
import { assertBudget, assertCanonicalEvents, assertExternalEvidence, exerciseConversations, exerciseWebSocket, EXTERNAL_GROUPS, verifyExternalArtifacts, exerciseNoStoreParentRejection, traceHeaders, tracedFetch, createManagedConversation, countRootWithoutAdmission, compareRootCount, aggregateEvidence, CORE_GROUPS, RUNNER_CONTRACT, assertExecutionWorkspace, readDeploymentProvenance, requireLiveDeploymentProvenance, nativeAcceptanceModel, verifyWorkerIdentity, assertServingWorkerHealth, currentPythonWorkerArguments, currentPythonWorkerEnvironment, main } from './full-responses-sdk.mjs';

function response(id) { return { id, object: 'response', status: 'completed', output: [{ id: 'msg_' + id, type: 'message', role: 'assistant', status: 'completed', content: [{ type: 'output_text', text: 'ready', annotations: [] }] }], usage: null }; }
function events(id, lane) { return [
  { type: 'response.created', sequence_number: 0, response: { ...response(id), status: 'in_progress' }, ...(lane ? { stream_id: lane } : {}) },
  { type: 'response.output_text.delta', sequence_number: 1, delta: 'ready', ...(lane ? { stream_id: lane } : {}) },
  { type: 'response.completed', sequence_number: 2, response: response(id), ...(lane ? { stream_id: lane } : {}) },
]; }

test('source/paid gates reject before network or credential access', async () => {
  await assert.rejects(main([], {}), /exact_source_required_before_network/);
  await assert.rejects(main(['--source', 'a'.repeat(40)], {}), /explicit_paid_execution_gate_required/);
  assert.throws(() => assertBudget(8, 7));
  assert.throws(() => assertBudget(8, 33));
  assertBudget(8, 8);
  assert.throws(() => assertBudget(9, 8), /explicit_bounded_paid_submission_budget_required/);
  assertBudget(9, 9);
});

test('full evidence cannot turn absent groups or wrong source into a passed release', () => {
  const source = 'a'.repeat(40);
  const evidence = { source, groups: Object.fromEntries(EXTERNAL_GROUPS.map(name => [name, { status: 'passed', source, receipt: `receipt:${name}`, receiptSha256: 'c'.repeat(64), resourceIds: ['owned-fixture'] }])) };
  assertExternalEvidence(evidence, source);
  assert.throws(() => assertExternalEvidence(evidence, 'b'.repeat(40)));
  delete evidence.groups.hosted_interpreter;
  assert.throws(() => assertExternalEvidence(evidence, source), /required_full_group_not_passed/);
});

test('canonical event validator rejects forged terminal, lane, order and IDs', () => {
  assert.equal(assertCanonicalEvents(events('resp_one')).id, 'resp_one');
  assert.equal(assertCanonicalEvents(events('resp_one', 'left'), 'left').id, 'resp_one');
  assert.throws(() => assertCanonicalEvents(events('resp_one', 'left'), 'right'));
  const outOfOrder = events('resp_one'); outOfOrder[2].sequence_number = 0;
  assert.throws(() => assertCanonicalEvents(outOfOrder));
  const failed = events('resp_one'); failed[2].type = 'response.failed';
  assert.throws(() => assertCanonicalEvents(failed));
  const changed = events('resp_one'); changed[2].response.id = 'resp_other';
  assert.throws(() => assertCanonicalEvents(changed));
});

test('real current SDK executes conversation CRUD pagination, isolation and non-cascade deletion', async () => {
  const items = new Map();
  const seen = [];
  let deleted = false;
  const conv = () => ({ id: 'conv_owned', object: 'conversation', created_at: 1, metadata: { acceptance: 'full-responses', ...(seen.some(value => value.method === 'POST' && value.path === '/v1/conversations/conv_owned') ? { revised: 'yes' } : {}) } });
  const fetch = async (url, init) => {
    const u = new URL(url), route = u.pathname, method = init.method;
    seen.push({ path: route, method });
    if (init.headers.get('Authorization') === 'Bearer other') return Response.json({ error: { message: 'Not found' } }, { status: 404 });
    if (route === '/v1/conversations') {
      items.set('msg_seed', { id: 'msg_seed', type: 'message', role: 'user', status: 'completed', content: [{ type: 'input_text', text: 'seed' }] });
      return Response.json(conv());
    }
    if (route === '/v1/conversations/conv_owned') {
      if (method === 'DELETE') { deleted = true; return Response.json({ id: 'conv_owned', object: 'conversation.deleted', deleted: true }); }
      return deleted ? Response.json({ error: { message: 'Not found' } }, { status: 404 }) : Response.json(conv());
    }
    if (route === '/v1/conversations/conv_owned/items' && method === 'POST') {
      const item = { id: 'msg_added', type: 'message', role: 'user', status: 'completed', content: [{ type: 'input_text', text: 'added' }] };
      items.set(item.id, item);
      return Response.json({ object: 'list', data: [item], first_id: item.id, last_id: item.id, has_more: false });
    }
    if (route === '/v1/conversations/conv_owned/items') {
      const id = u.searchParams.has('after') ? 'msg_added' : 'msg_seed';
      return Response.json({ object: 'list', data: [items.get(id)], first_id: id, last_id: id, has_more: id === 'msg_seed' });
    }
    const id = route.split('/').at(-1);
    if (!items.has(id)) return Response.json({ error: { message: 'Not found' } }, { status: 404 });
    if (method === 'DELETE') { items.delete(id); return Response.json(conv()); }
    return Response.json(items.get(id));
  };
  const client = new OpenAI({ apiKey: 'owned', baseURL: 'https://fixture.invalid/v1', fetch, maxRetries: 0 });
  const other = new OpenAI({ apiKey: 'other', baseURL: 'https://fixture.invalid/v1', fetch, maxRetries: 0 });
  const remembered = [];
  const result = await exerciseConversations(client, other, (kind, id) => remembered.push({ kind, id }));
  assert.equal(result.cursorItems, 2);
  assert.equal(result.deletionRetainsItems, true);
  assert.deepEqual(remembered, [{ kind: 'conversations', id: 'conv_owned' }]);
  assert.equal(items.size, 0);
  assert.equal(seen.filter(value => value.path === '/v1/conversations' && value.method === 'POST').length, 1, 'no create replay');
});

test('real current SDK WebSocket sends separate named parent forks and preserves native event lanes', async () => {
  const server = new WebSocketServer({ host: '127.0.0.1', port: 0 });
  await new Promise(resolve => server.once('listening', resolve));
  const address = server.address();
  const sent = [], remembered = [], budget = [];
  server.on('connection', socket => {
    socket.on('message', bytes => {
      const value = JSON.parse(bytes.toString()); sent.push(value);
      for (const event of events(`resp_${value.stream_id}`, value.stream_id)) socket.send(JSON.stringify(event));
    });
  });
  const client = new OpenAI({ apiKey: 'local-fixture-only', baseURL: `http://127.0.0.1:${address.port}/v1`, maxRetries: 0 });
  try {
    const result = await exerciseWebSocket(client, 'resp_parent', purpose => budget.push(purpose), (kind, id) => remembered.push({ kind, id }), value => new ResponsesWS(value, { reconnect: null, maxPayload: 1024 * 1024 }));
    assert.equal(result.lanes, 2);
    assert.deepEqual(sent.map(value => value.stream_id), ['fork-left', 'fork-right']);
    assert.ok(sent.every(value => value.type === 'response.create' && value.previous_response_id === 'resp_parent' && !('stream' in value)));
    assert.equal(budget.length, 2);
    assert.equal(remembered.length, 2);
  } finally {
    for (const client of server.clients) client.terminate();
    await new Promise(resolve => server.close(resolve));
  }
});

test('WS native failure never becomes fake completion and socket always closes', async () => {
  class FailedSocket extends EventEmitter {
    closed = false;
    constructor() { super(); this.socket = this; queueMicrotask(() => this.emit('open')); }
    send() { queueMicrotask(() => this.emit('error', Error('fixture failure'))); }
    close() { this.closed = true; }
  }
  const socket = new FailedSocket();
  await assert.rejects(exerciseWebSocket(null, 'resp_parent', () => {}, () => {}, () => socket), /websocket_native_error_no_paid_replay/);
  assert.equal(socket.closed, true);
});




test('external evidence verifies real files, hashes, source, group and observed resource handles', () => {
  const root = process.cwd();
  const owned = path.resolve('.temp/v0.3-full-acceptance');
  const directory = path.join(owned, 'node-gate-tests-' + randomUUID());
  assert.ok(directory.startsWith(owned + path.sep));
  fs.mkdirSync(directory, { recursive: true });
  const source = 'a'.repeat(40);
  const evidence = { source, groups: {} };
  const receipts = new Map();
  const save = name => {
    const target = path.join(directory, name + '.json');
    const bytes = Buffer.from(JSON.stringify(receipts.get(name)));
    fs.writeFileSync(target, bytes);
    evidence.groups[name] = { source, status: 'passed', receipt: path.relative(root, target), receiptSha256: createHash('sha256').update(bytes).digest('hex'), resourceIds: ['owned_' + name] };
  };
  for (const name of EXTERNAL_GROUPS) {
    receipts.set(name, { source, group: name, status: 'passed', realm: 'staging', resourceIds: ['owned_' + name], checks: [{ name: 'actual_owned_operation', result: 'passed', observed: { events: 3 } }] });
    save(name);
  }
  try {
    assert.equal(verifyExternalArtifacts(evidence, root).groups, EXTERNAL_GROUPS.length);
    const first = EXTERNAL_GROUPS[0], original = structuredClone(evidence.groups[first]);
    evidence.groups[first].receipt = path.join(directory, 'not-existent.json');
    assert.throws(() => verifyExternalArtifacts(evidence, root));
    evidence.groups[first] = structuredClone(original);
    evidence.groups[first].receiptSha256 = 'b'.repeat(64);
    assert.throws(() => verifyExternalArtifacts(evidence, root), /external_receipt_digest_changed/);
    receipts.get(first).source = 'b'.repeat(40); save(first);
    assert.throws(() => verifyExternalArtifacts(evidence, root), /external_receipt_source_mismatch/);
    receipts.get(first).source = source; receipts.get(first).group = 'fabricated_group'; save(first);
    assert.throws(() => verifyExternalArtifacts(evidence, root), /external_receipt_group_mismatch/);
    receipts.get(first).group = first; receipts.get(first).resourceIds = ['foreign']; save(first);
    assert.throws(() => verifyExternalArtifacts(evidence, root), /external_receipt_resource_ids_mismatch/);
    receipts.get(first).resourceIds = ['owned_' + first]; receipts.get(first).checks = []; save(first);
    assert.throws(() => verifyExternalArtifacts(evidence, root), /external_receipt_checks_missing/);
  } finally {
    assert.ok(directory.startsWith(owned + path.sep));
    fs.rmSync(directory, { recursive: true });
  }
});

test('property probe preserves text and ordered identity across random event boundaries', () => {
  let state = 0x12345678;
  const random = () => { state ^= state << 13; state ^= state >>> 17; state ^= state << 5; return state >>> 0; };
  for (let round = 0; round < 200; round++) {
    const id = 'resp_property_' + round, lane = 'lane_' + random() % 32;
    const count = random() % 24 + 1;
    const chunks = Array.from({ length: count }, () => String.fromCodePoint(0x4e00 + random() % 1000));
    const values = [{ type: 'response.created', sequence_number: 0, stream_id: lane, response: { ...response(id), status: 'in_progress' } }];
    chunks.forEach((delta, index) => values.push({ type: 'response.output_text.delta', sequence_number: index + 1, stream_id: lane, delta }));
    const terminal = response(id); terminal.output[0].content[0].text = chunks.join('');
    values.push({ type: 'response.completed', sequence_number: count + 1, stream_id: lane, response: terminal });
    assert.equal(assertCanonicalEvents(values, lane).id, id);
    values[random() % count + 1].delta += 'corruption';
    assert.throws(() => assertCanonicalEvents(values, lane), /streamed_text_differs_from_canonical_terminal/);
  }
});

test('invalid no-store parent persists intent/key before its only POST and rejects without admission', async () => {
  const order = [], records = [], remembered = [];
  let posts = 0;
  const client = new OpenAI({ apiKey: 'fixture-only', baseURL: 'https://fixture.invalid/v1', maxRetries: 0, fetch: async (url, init) => {
    posts++; order.push('POST');
    assert.equal(init.headers.get('Idempotency-Key'), 'negative-operation-owned');
    const body = JSON.parse(init.body);
    assert.equal(body.stream, true);
    assert.equal(body.reasoning.effort, 'none');
    return Response.json({ error: { message: 'Not found', code: 'not_found' } }, { status: 404 });
  } });
  const result = await exerciseNoStoreParentRejection(client, { model: 'agent_owned', previous_response_id: 'resp_unstored', input: 'reject' }, {
    beforeDispatch: (purpose, details) => { order.push('durable-intent'); assert.equal(details.parentResponse, 'resp_unstored'); return 'negative-operation-owned'; },
    remember: (kind, id) => remembered.push({ kind, id }),
    record: value => records.push(value),
  });
  assert.deepEqual(order, ['durable-intent', 'POST']);
  assert.equal(posts, 1);
  assert.deepEqual(remembered, []);
  assert.equal(result.rejected, true);
  assert.equal(records[0].status, 'rejected');
});

test('unexpected negative admission preserves headers/canonical terminal and fails without retry', async () => {
  const order = [], records = [], remembered = [];
  let posts = 0;
  const canonical = events('resp_unexpected');
  const client = new OpenAI({ apiKey: 'fixture-only', baseURL: 'https://fixture.invalid/v1', maxRetries: 0, fetch: async () => {
    posts++; order.push('POST');
    const stream = canonical.map(event => `event: ${event.type}\ndata: ${JSON.stringify(event)}\n\n`).join('');
    return new Response(stream, { headers: { 'Content-Type': 'text/event-stream', 'x-agents-response': 'resp_unexpected', 'x-agents-session': 'session_unexpected' } });
  } });
  await assert.rejects(exerciseNoStoreParentRejection(client, { model: 'agent_owned', previous_response_id: 'resp_unstored' }, {
    beforeDispatch: () => { order.push('durable-intent'); return 'negative-operation-owned'; },
    remember: (kind, id) => { order.push('capture-' + kind); remembered.push({ kind, id }); },
    record: value => { order.push(value.status); records.push(value); },
  }), /store_false_parent_unexpectedly_admitted_no_replay/);
  assert.equal(posts, 1);
  assert.deepEqual(order.slice(0, 5), ['durable-intent', 'POST', 'capture-responses', 'capture-sessions', 'unexpected_accepted_observing_same_work']);
  assert.ok(remembered.some(value => value.kind === 'sessions' && value.id === 'session_unexpected'));
  assert.equal(records.at(-1).status, 'unexpected_terminal');
  assert.equal(records.at(-1).responseId, 'resp_unexpected');
  assert.equal(records.at(-1).terminal, 'response.completed');
});

test('negative admission observer failure retains accepted IDs and never resubmits work', async () => {
  const remembered = [], records = [];
  let posts = 0;
  const client = { responses: { create: () => {
    posts++;
    return { withResponse: async () => ({ response: { headers: new Headers({ 'x-agents-response': 'resp_header', 'x-agents-session': 'session_owned' }) }, data: (async function* () { yield { type: 'response.created', response: { id: 'resp_canonical' } }; throw Error('observer_disconnected'); })() }) };
  } } };
  await assert.rejects(exerciseNoStoreParentRejection(client, { previous_response_id: 'resp_unstored' }, {
    beforeDispatch: () => 'negative-operation-owned',
    remember: (kind, id) => remembered.push({ kind, id }),
    record: value => records.push(value),
  }), /observer_disconnected/);
  assert.equal(posts, 1);
  assert.deepEqual(remembered.map(value => value.id), ['resp_header', 'session_owned', 'resp_canonical']);
  assert.equal(records.at(-1).status, 'unexpected_accepted_observing_same_work');
});

test('incremental prepaid root count preserves canonical source pin conversation and UserClock', async () => {
  const body = { model: 'agent_owned', conversation: 'conv_owned', input: 'actual-root', instructions: 'request-custom', reasoning: { effort: 'none' } };
  const canonical = { id: 'session_owned', status: 'idle', pinned: { model: { provider: 'deepseek', model: 'deepseek-flash' } }, messages: [], input_revision: 0, last_input_at_ms: 123, run_id: null };
  const seed = { object: 'list', data: [{ id: 'msg_seed' }], has_more: false };
  const observed = [];
  const client = new OpenAI({ apiKey: 'fixture-only', baseURL: 'https://fixture.invalid/v1', maxRetries: 0, fetch: async (url, init) => {
    observed.push({ method: init.method, path: new URL(url).pathname, body: JSON.parse(init.body) });
    assert.equal(init.headers.get('X-Agents-Session'), 'session_owned');
    return Response.json({ object: 'response.input_tokens', input_tokens: 23 });
  } });
  const request = async (method, route) => { observed.push({ method, route }); return structuredClone(route.includes('/conversations/') ? seed : canonical); };
  const result = await countRootWithoutAdmission(client, request, canonical.id, body);
  assert.deepEqual(observed.find(value => value.path)?.body, body);
  assert.deepEqual(observed.filter(value => value.path).map(value => value.path), ['/v1/responses/input_tokens']);
  assert.equal(result.evidence.noRunAdmitted, true);
  assert.equal(result.evidence.userClockUnchanged, true);
  assert.equal(compareRootCount(result.count, { usage: { input_tokens: 23 } }).status, 'equal_provider_reported_input_tokens');
  assert.throws(() => compareRootCount(result.count, { usage: { input_tokens: 24 } }), /same_rendered_root_count_differs/);
  assert.equal(compareRootCount(result.count, { usage: null }).status, 'not_compared_provider_standard_usage_unavailable');
  for (const [field, value, failure] of [['pinned', { changed: true }, 'count_changed_initial_pin'], ['messages', ['changed'], 'count_changed_initial_source'], ['input_revision', 1, 'count_changed_user_clock_revision'], ['last_input_at_ms', 124, 'count_changed_user_clock_time'], ['run_id', 'run_unexpected', 'count_admitted_a_run']]) {
    let reads = 0;
    const mutableRequest = async () => { const state = structuredClone(canonical); if (++reads === 2) state[field] = value; return state; };
    await assert.rejects(countRootWithoutAdmission(client, mutableRequest, canonical.id, { model: 'agent_owned', input: 'same' }), new RegExp(failure));
  }
});

test('incremental correlation reaches raw fetch official HTTP SDK and native WS handshake', async () => {
  const seed = '40190532245:1', expectedTrace = createHash('sha256').update(seed).digest('hex').slice(0, 32);
  const seen = [];
  const transport = tracedFetch(async (url, init) => { seen.push({ url, traceparent: init.headers.get('traceparent') }); return Response.json({ id: 'conv_trace', object: 'conversation', created_at: 1, metadata: {} }); }, seed);
  await transport('https://fixture.invalid/healthz');
  const client = new OpenAI({ apiKey: 'fixture-only', baseURL: 'https://fixture.invalid/v1', maxRetries: 0, fetch: transport });
  await client.conversations.retrieve('conv_trace');
  assert.equal(seen.length, 2);
  for (const request of seen) assert.match(request.traceparent, new RegExp(`^00-${expectedTrace}-[0-9a-f]{16}-00$`));
  assert.notEqual(seen[0].traceparent, seen[1].traceparent);
  assert.deepEqual(traceHeaders(undefined), {});
  assert.throws(() => traceHeaders('private-subject-not-a-run'), /invalid_ci_trace_seed/);
  const server = new WebSocketServer({ host: '127.0.0.1', port: 0 });
  await new Promise(resolve => server.once('listening', resolve));
  let handshake;
  server.on('connection', (socket, request) => {
    handshake = request.headers.traceparent;
    socket.on('message', bytes => { const incoming = JSON.parse(bytes.toString()); for (const event of events('resp_' + incoming.stream_id, incoming.stream_id)) socket.send(JSON.stringify(event)); });
  });
  try {
    const socketClient = new OpenAI({ apiKey: 'local-fixture-only', baseURL: `http://127.0.0.1:${server.address().port}/v1`, maxRetries: 0 });
    await exerciseWebSocket(socketClient, 'resp_parent', () => {}, () => {}, undefined, undefined, seed);
    assert.match(handshake, new RegExp(`^00-${expectedTrace}-[0-9a-f]{16}-00$`));
  } finally { for (const client of server.clients) client.terminate(); await new Promise(resolve => server.close(resolve)); }
});

test('incremental Conversation management create persists stable intent before one POST and retains ambiguity', async () => {
  const order = [], intents = [], remembered = [];
  let posts = 0;
  const hooks = { begin: (purpose, metadata) => { order.push('durable-intent'); intents.push({ purpose, ...metadata, key: 'stable-management-key', status: 'pending' }); return 'stable-management-key'; }, remember: (kind, id) => { order.push('capture-id'); remembered.push({ kind, id }); }, complete: (key, id) => { order.push('complete'); intents.at(-1).status = 'complete'; intents.at(-1).id = id; } };
  const client = new OpenAI({ apiKey: 'fixture-only', baseURL: 'https://fixture.invalid/v1', maxRetries: 2, fetch: async (url, init) => { posts++; order.push('POST'); assert.equal(init.headers.get('Idempotency-Key'), 'stable-management-key'); return Response.json({ id: 'conv_owned', object: 'conversation', created_at: 1, metadata: {} }); } });
  await createManagedConversation(client, { metadata: { fixture: 'bounded' } }, hooks);
  assert.deepEqual(order, ['durable-intent', 'POST', 'capture-id', 'complete']);
  assert.equal(intents[0].bodySha256.length, 64);
  assert.equal(posts, 1);
  const ambiguous = new OpenAI({ apiKey: 'fixture-only', baseURL: 'https://fixture.invalid/v1', maxRetries: 2, fetch: async () => { posts++; throw Error('unknown_transport_result'); } });
  await assert.rejects(createManagedConversation(ambiguous, { metadata: {} }, hooks));
  assert.equal(posts, 2, 'method-level zero retries overrides a retrying client');
  assert.equal(intents.at(-1).status, 'pending');
  assert.equal(remembered.length, 1);
});

test('incremental evidence-only aggregation reuses sealed source and dependencies with zero network', async () => {
  const root = process.cwd(), owned = path.resolve('.temp/v0.3-full-acceptance');
  const directory = path.join(owned, 'node-aggregate-tests-' + randomUUID());
  assert.ok(directory.startsWith(owned + path.sep)); fs.mkdirSync(directory, { recursive: true });
  const source = 'a'.repeat(40), external = { source, groups: {} };
  const corePath = path.join(directory, 'core.json'), externalPath = path.join(directory, 'external.json'), output = path.join(directory, 'aggregated.json');
  const core = { source, runnerContract: RUNNER_CONTRACT, dependencies: { nodeSdk: '7.43.0', currentPythonSdk: '3.37.0' }, managementIntents: [{ status: 'completed' }], status: 'core_passed_full_acceptance_not_claimed', pending: null, cleanup: { failures: [] }, checks: Object.fromEntries([...CORE_GROUPS, 'prepaid_count', 'current_python_wire', 'current_python_sdk'].map(name => [name, { status: 'passed' }])) };
  for (const group of EXTERNAL_GROUPS) {
    const receipt = { source, group, status: 'passed', realm: 'staging', resourceIds: ['owned_' + group], checks: [{ name: 'actual_observation_fixture', result: 'passed', observed: 1 }] };
    const file = path.join(directory, group + '.json'), bytes = Buffer.from(JSON.stringify(receipt)); fs.writeFileSync(file, bytes);
    external.groups[group] = { source, status: 'passed', receipt: file, receiptSha256: createHash('sha256').update(bytes).digest('hex'), resourceIds: receipt.resourceIds };
  }
  fs.writeFileSync(corePath, JSON.stringify(core)); fs.writeFileSync(externalPath, JSON.stringify(external));
  const originalFetch = globalThis.fetch; let requests = 0;
  globalThis.fetch = async () => { requests++; throw Error('aggregation_must_be_offline'); };
  try {
    await main(['--aggregate-only', '--source', source, '--core-receipt', corePath, '--external-evidence', externalPath, '--output', output], {});
    const result = JSON.parse(fs.readFileSync(output, 'utf8'));
    assert.equal(result.newModelRuns, 0); assert.equal(result.servingSourceRechecked, false); assert.equal(requests, 0);
    assert.equal(result.dependencyBaseline.status, 'sealed_core_not_retested');
    const config = { source, 'core-receipt': corePath, 'external-evidence': externalPath };
    core.source = 'b'.repeat(40); fs.writeFileSync(corePath, JSON.stringify(core));
    assert.throws(() => aggregateEvidence(config, root), /sealed_core_source_mismatch/);
    core.source = source; core.runnerContract = 'historical-unaccepted-contract'; fs.writeFileSync(corePath, JSON.stringify(core));
    assert.throws(() => aggregateEvidence(config, root), /sealed_core_contract_version_mismatch/);
  } finally { globalThis.fetch = originalFetch; assert.ok(directory.startsWith(owned + path.sep)); fs.rmSync(directory, { recursive: true }); }
});

test('incremental2 GHA workspace guard accepts only exact realpath source and repository', () => {
  const root = process.cwd(), source = 'a'.repeat(40);
  const environment = { GITHUB_ACTIONS: 'true', GITHUB_REPOSITORY: 'kleedaisuki/moesegfault-agents', GITHUB_SHA: source, GITHUB_WORKSPACE: root };
  assert.equal(assertExecutionWorkspace(root, source, environment).mode, 'trusted_actions_workspace');
  assert.throws(() => assertExecutionWorkspace(root, source, { ...environment, GITHUB_REPOSITORY: 'attacker/fork' }), /untrusted_actions_repository/);
  assert.throws(() => assertExecutionWorkspace(root, source, { ...environment, GITHUB_SHA: 'b'.repeat(40) }), /actions_source_mismatch/);
  assert.throws(() => assertExecutionWorkspace(root, source, { ...environment, GITHUB_WORKSPACE: path.dirname(root) }), /actions_cwd_workspace_mismatch/);
  assert.throws(() => assertExecutionWorkspace(root, source, { ...environment, GITHUB_WORKSPACE: '' }), /actions_workspace_missing/);
  assert.throws(() => assertExecutionWorkspace(path.dirname(root), source, {}), /explicit_release_worktree_required/);
  if (fs.realpathSync(root).replaceAll('\\', '/').toLowerCase() === 'd:/code/moesegfault-agents/.cache/v2-release-worktree') assert.equal(assertExecutionWorkspace(root, source, {}).mode, 'owned_local_release_worktree');
  else assert.throws(() => assertExecutionWorkspace(root, source, {}), /explicit_release_worktree_required/);
});

test('incremental2 known agents input usage retains parity despite unknown cache write fields', () => {
  const canonical = { usage: null, agents_usage: { input_tokens: 91, input_tokens_details: { cached_tokens: 4, cache_write_tokens: null }, output_tokens_details: { reasoning_tokens: null } } };
  const before = structuredClone(canonical);
  assert.deepEqual(compareRootCount({ input_tokens: 91 }, canonical), { status: 'equal_provider_reported_input_tokens', inputTokens: 91, usageSource: 'agents_usage' });
  assert.deepEqual(canonical, before, 'known input parity must not fabricate unknown usage detail');
  assert.throws(() => compareRootCount({ input_tokens: 90 }, canonical), /same_rendered_root_count_differs/);
  assert.equal(compareRootCount({ input_tokens: 91 }, { usage: null, agents_usage: { input_tokens: null } }).status, 'not_compared_provider_standard_usage_unavailable');
});

test('incremental2 cross-source aggregation seals old core separately from current five-component provenance', () => {
  const root = process.cwd(), owned = path.resolve('.temp/v0.3-full-acceptance');
  const directory = path.join(owned, 'cross-source-tests-' + randomUUID());
  assert.ok(directory.startsWith(owned + path.sep)); fs.mkdirSync(directory, { recursive: true });
  const baselineSource = 'a'.repeat(40), currentSource = 'b'.repeat(40);
  const components = ['worker', 'native', 'container', 'dashboard', 'tooling'];
  const old = Object.fromEntries(components.map((name, index) => [name, String(index + 1).repeat(64)]));
  const current = { ...old, worker: '9'.repeat(64) };
  const save = (file, value) => { const bytes = Buffer.from(JSON.stringify(value)); fs.writeFileSync(file, bytes); return createHash('sha256').update(bytes).digest('hex'); };
  const corePath = path.join(directory, 'core.json'), provenancePath = path.join(directory, 'deployment-provenance.json'), externalPath = path.join(directory, 'external.json');
  const core = { source: baselineSource, runnerContract: RUNNER_CONTRACT, component_fingerprints: old, dependencies: { nodeSdk: '7.43.0', currentPythonSdk: '3.37.0' }, managementIntents: [{ status: 'completed' }], status: 'core_passed_full_acceptance_not_claimed', pending: null, cleanup: { failures: [] }, checks: Object.fromEntries([...CORE_GROUPS, 'prepaid_count', 'current_python_wire', 'current_python_sdk'].map(name => [name, { status: 'passed' }])) };
  const coreHash = save(corePath, core);
  const provenance = { schema_version: 1, status: 'incremental_build_and_selected_checks_verified', source_sha: currentSource, source_run: '123', component_fingerprints: current, diagnostic_only_components: [], staging_acceptance: 'not_executed', production_authorized: false, baseline_evidence: { baseline: 'sealed-fixture' }, passed_current_checks: ['changed-worker-check'], artifacts: Object.fromEntries(['worker', 'dashboard', 'container'].map((name, index) => [name, { id: index + 1, run_id: '123', source_sha: currentSource, fingerprint: current[name], status: 'built_current' }])) };
  const external = { source: currentSource, core_baseline: { source: baselineSource, receipt: corePath, receiptSha256: coreHash, component_fingerprints: old }, current_deployment: { source: currentSource, receipt: provenancePath, receiptSha256: save(provenancePath, provenance) }, groups: {} };
  for (const group of EXTERNAL_GROUPS) {
    const receipt = { source: currentSource, group, status: 'passed', realm: 'staging', resourceIds: ['owned_' + group], checks: [{ name: 'observed-fixture', result: 'passed', observed: 1 }] };
    const file = path.join(directory, group + '.json'); external.groups[group] = { source: currentSource, status: 'passed', receipt: file, receiptSha256: save(file, receipt), resourceIds: receipt.resourceIds };
  }
  save(externalPath, external);
  const config = { source: currentSource, 'core-receipt': corePath, 'external-evidence': externalPath };
  try {
    const result = aggregateEvidence(config, root);
    assert.equal(result.status, 'baseline_not_reexecuted_final_judgment_required');
    assert.equal(result.source, currentSource); assert.equal(result.baselineSource, baselineSource);
    assert.deepEqual(result.changedComponents, ['worker']);
    assert.deepEqual(result.baselineComponentFingerprints, old); assert.deepEqual(result.currentComponentFingerprints, current);
    assert.equal(result.dependencyBaseline.currentPythonSdk, '3.37.0');
    assert.equal(result.newModelRuns, 0); assert.equal(result.servingSourceRechecked, false);
    const original = structuredClone(external);
    delete external.core_baseline; save(externalPath, external);
    assert.throws(() => aggregateEvidence(config, root), /sealed_core_source_mismatch/);
    Object.assign(external, structuredClone(original)); external.core_baseline.receiptSha256 = 'f'.repeat(64); save(externalPath, external);
    assert.throws(() => aggregateEvidence(config, root), /core_baseline_receipt_hash_mismatch/);
    Object.assign(external, structuredClone(original)); external.core_baseline.component_fingerprints.native = '0'.repeat(64); save(externalPath, external);
    assert.throws(() => aggregateEvidence(config, root), /core_baseline_fingerprints_mismatch/);
    Object.assign(external, structuredClone(original)); provenance.diagnostic_only_components = ['worker']; external.current_deployment.receiptSha256 = save(provenancePath, provenance); save(externalPath, external);
    assert.throws(() => aggregateEvidence(config, root), /diagnostic_scope_cannot_seal_core/);
    provenance.diagnostic_only_components = []; delete provenance.component_fingerprints.tooling; external.current_deployment.receiptSha256 = save(provenancePath, provenance); save(externalPath, external);
    assert.throws(() => aggregateEvidence(config, root), /all_five_component_fingerprints_required/);
    provenance.component_fingerprints.tooling = current.tooling; provenance.source_sha = baselineSource; external.current_deployment.receiptSha256 = save(provenancePath, provenance); save(externalPath, external);
    assert.throws(() => aggregateEvidence(config, root), /deployment_provenance_source_mismatch/);
  } finally { assert.ok(directory.startsWith(owned + path.sep)); fs.rmSync(directory, { recursive: true }); }
});


test('incremental3 live execution rejects missing or mismatched deployment seal before network', () => {
  const root = process.cwd(), source = 'a'.repeat(40);
  assert.throws(() => requireLiveDeploymentProvenance({}, source, root), /live_deployment_provenance_required_before_network/);
  assert.throws(() => requireLiveDeploymentProvenance({ MAGENTS_TEST_DEPLOYMENT_PROVENANCE: '' }, source, root), /live_deployment_provenance_required_before_network/);
  const owned = path.resolve('.temp/v0.3-full-acceptance'), directory = path.join(owned, 'live-seal-tests-' + randomUUID());
  assert.ok(directory.startsWith(owned + path.sep)); fs.mkdirSync(directory, { recursive: true });
  const proof = path.join(directory, 'mismatched-provenance.json');
  fs.writeFileSync(proof, JSON.stringify({ schema_version: 1, status: 'incremental_build_and_selected_checks_verified', source_sha: 'b'.repeat(40) }));
  try {
    assert.throws(() => requireLiveDeploymentProvenance({ MAGENTS_TEST_DEPLOYMENT_PROVENANCE: proof }, source, root), /deployment_provenance_source_mismatch/);
  } finally { assert.ok(directory.startsWith(owned + path.sep)); fs.rmSync(directory, { recursive: true }); }
});

test('incremental4 native compact retains the real configured model budget instead of inventing a smaller summary limit', () => {
  const registered = { provider: 'deepseek', model: 'deepseek-flash', context_window: 1000000, max_output_tokens: 8192 };
  const selected = nativeAcceptanceModel(registered);
  assert.deepEqual(selected, { ...registered, protocol: 'responses' });
  assert.equal(selected.max_output_tokens, 8192);
  assert.equal(registered.protocol, undefined);
});

test('incremental-mixed assembly provenance preserves reused Worker identity through health resolver and Python inspection', () => {
  const root = process.cwd(), owned = path.resolve('.temp/v0.3-full-acceptance');
  const directory = path.join(owned, 'mixed-assembly-tests-' + randomUUID());
  assert.ok(directory.startsWith(owned + path.sep)); fs.mkdirSync(directory, { recursive: true });
  const assembly = 'a'.repeat(40), worker = 'b'.repeat(40), container = 'c'.repeat(40);
  const fingerprints = Object.fromEntries(['worker', 'native', 'container', 'dashboard', 'tooling'].map((name, index) => [name, String(index + 1).repeat(64)]));
  const provenance = { schema_version: 1, status: 'incremental_build_and_selected_checks_verified', source_sha: assembly, source_run: '123', component_fingerprints: fingerprints, diagnostic_only_components: [], staging_acceptance: 'not_executed', production_authorized: false, baseline_evidence: [], passed_current_checks: [], artifacts: Object.fromEntries(['worker', 'dashboard', 'container'].map((name, index) => [name, { id: index + 1, run_id: '122', source_sha: name === 'worker' ? worker : name === 'container' ? container : assembly, fingerprint: fingerprints[name], status: name === 'dashboard' ? 'built_current' : 'reused_identical_artifact' }])) };
  const file = path.join(directory, 'deployment-provenance.json'); fs.writeFileSync(file, JSON.stringify(provenance));
  try {
    const accepted = requireLiveDeploymentProvenance({ MAGENTS_TEST_DEPLOYMENT_PROVENANCE: file }, assembly, root);
    assert.equal(accepted.source, assembly); assert.equal(accepted.assemblySource, assembly); assert.equal(accepted.workerSource, worker);
    assert.deepEqual(accepted.component_sources, { worker, dashboard: assembly, container });
    assert.equal(Object.hasOwn(accepted.component_sources, 'native'), false, 'verification sources are not deployed native binary identities');
    let verified;
    verifyWorkerIdentity(accepted, root, source => { verified = source; }); assert.equal(verified, worker);
    assertServingWorkerHealth({ status: 'ok', build_id: worker }, accepted);
    assert.throws(() => assertServingWorkerHealth({ status: 'ok', build_id: assembly }, accepted), /serving_worker_source_changed/);
    assert.throws(() => readDeploymentProvenance(file, worker, root), /deployment_provenance_source_mismatch/);
    assert.throws(() => verifyWorkerIdentity({ ...accepted, workerSource: assembly }, root, () => {}), /worker_source_map_mismatch/);
    const args = currentPythonWorkerArguments(root, accepted, { session: 'session_owned', response: 'resp_owned', agent: 'agent_owned' }, path.join(directory, 'python-safe.json'));
    assert.equal(args[args.indexOf('--source') + 1], worker);
    const env = currentPythonWorkerEnvironment({ MAGENTS_TEST_EXPECTED_BUILD: assembly, GITHUB_SHA: assembly }, accepted);
    assert.equal(env.MAGENTS_TEST_EXPECTED_BUILD, worker); assert.equal(env.MAGENTS_TEST_DEPLOYMENT_ASSEMBLY_SHA, assembly); assert.equal(env.GITHUB_SHA, assembly);
    assert.equal(assertExecutionWorkspace(root, assembly, { GITHUB_ACTIONS: 'true', GITHUB_WORKSPACE: root, GITHUB_REPOSITORY: 'kleedaisuki/moesegfault-agents', GITHUB_SHA: assembly }).source, assembly);
  } finally { assert.ok(directory.startsWith(owned + path.sep)); fs.rmSync(directory, { recursive: true }); }
});


test('core staging can exercise API and WebSocket without admitting unavailable compact', async () => {
  const { configuration } = await import('./full-responses-sdk.mjs');
  const args = ['--source', 'a'.repeat(40), '--scope', 'core', '--omit-compact', '--allow-model-calls', '--max-paid-submissions', '7', '--output', '.temp/core.json', '--current-python', '.cache/current-sdk-venv/Scripts/python.exe'];
  const env = { MAGENTS_TEST_API_ORIGIN: 'https://agents-staging.moesegfault.dev', MAGENTS_TEST_API_KEY: 'owned-fixture', MAGENTS_TEST_OTHER_API_KEY: 'other-owned-fixture' };
  const config = configuration(args, env);
  assert.equal(config['omit-compact'], true);
  assert.equal(config.limit, 7);
  const full = [...args]; full[full.indexOf('core')] = 'full';
  assert.throws(() => configuration(full, env), /compact_omission_cannot_claim_full_acceptance/);
});

test('core staging reader preserves a verified old daemon as compatible rather than source-identical', () => {
  const root = process.cwd(), owned = path.resolve('.temp/v0.3-full-acceptance');
  const directory = path.join(owned, 'core-compatible-' + randomUUID()); fs.mkdirSync(directory, { recursive: true });
  const source = 'a'.repeat(40), daemon = 'b1edf97746d3902373dbb05e224799f03d9a3a10';
  const fingerprints = Object.fromEntries(['worker', 'native', 'container', 'dashboard', 'tooling'].map((name, index) => [name, String(index + 1).repeat(64)]));
  const receipt = { schema_version: 1, status: 'incremental_build_and_selected_checks_verified', source_sha: source, source_run: '123', component_fingerprints: fingerprints, diagnostic_only_components: [], staging_acceptance: 'not_executed', production_authorized: false, baseline_evidence: [], passed_current_checks: ['core'], runtime_scope: 'core-compatible/disabled-runtime', artifacts: Object.fromEntries(['worker', 'dashboard', 'container'].map((name, index) => [name, { id: index + 1, run_id: name === 'container' ? '40191955285' : '123', source_sha: name === 'container' ? daemon : source, fingerprint: fingerprints[name], status: name === 'container' ? 'core_compatible_disabled_runtime' : 'built_current' }])) };
  const file = path.join(directory, 'deployment.json');
  try {
    fs.writeFileSync(file, JSON.stringify(receipt));
    assert.equal(readDeploymentProvenance(file, source, root).component_sources.container, daemon);
    delete receipt.runtime_scope; fs.writeFileSync(file, JSON.stringify(receipt));
    assert.throws(() => readDeploymentProvenance(file, source, root), /artifact_scope_not_accepted/);
  } finally { fs.rmSync(directory, { recursive: true }); }
});
