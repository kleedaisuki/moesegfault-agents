import assert from 'node:assert/strict';
import test from 'node:test';
import fs from 'node:fs';
import path from 'node:path';
import { randomUUID, createHash } from 'node:crypto';
import { EventEmitter } from 'node:events';
import { WebSocketServer } from 'ws';
import OpenAI from 'openai';
import { ResponsesWS } from 'openai/resources/responses/ws';
import { assertBudget, assertCanonicalEvents, assertExternalEvidence, exerciseConversations, exerciseWebSocket, EXTERNAL_GROUPS, verifyExternalArtifacts, exerciseNoStoreParentRejection, main } from './full-responses-sdk.mjs';

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
